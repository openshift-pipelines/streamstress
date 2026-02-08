use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use tokio::task::JoinSet;

use crate::component::{self, ComponentSpec};
use crate::config::{self, ComponentConfig};
use crate::exec;
use crate::progress;
use crate::registry;

/// Safe base image to replace restricted ghcr.io/tektoncd/ images in .ko.yaml.
/// Chainguard static is a minimal, publicly available base image suitable for Go binaries.
const KO_SAFE_BASE_IMAGE: &str = "cgr.dev/chainguard/static:latest";

/// Sanitize `.ko.yaml` in the given source directory by replacing restricted base images.
///
/// Upstream Tekton repos reference `ghcr.io/tektoncd/` base images in `.ko.yaml`
/// which return DENIED when pulled outside the Tekton CI. This function replaces
/// those references with a publicly available alternative so `ko build` succeeds.
///
/// No-op if `.ko.yaml` doesn't exist (not all components have one).
pub fn sanitize_ko_config(source_dir: &Path) -> Result<()> {
    let ko_yaml_path = source_dir.join(".ko.yaml");
    if !ko_yaml_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&ko_yaml_path)
        .with_context(|| format!("Failed to read {}", ko_yaml_path.display()))?;

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", ko_yaml_path.display()))?;

    let mut replaced = 0u32;

    // Replace defaultBaseImage if it references ghcr.io/tektoncd/
    if let Some(default_base) = doc.get_mut("defaultBaseImage") {
        if let Some(s) = default_base.as_str() {
            if s.starts_with("ghcr.io/tektoncd/") {
                *default_base = serde_yaml::Value::String(KO_SAFE_BASE_IMAGE.to_string());
                replaced += 1;
            }
        }
    }

    // Replace baseImageOverrides values that reference ghcr.io/tektoncd/
    if let Some(overrides) = doc.get_mut("baseImageOverrides") {
        if let Some(mapping) = overrides.as_mapping_mut() {
            for (_key, value) in mapping.iter_mut() {
                if let Some(s) = value.as_str() {
                    if s.starts_with("ghcr.io/tektoncd/") {
                        *value = serde_yaml::Value::String(KO_SAFE_BASE_IMAGE.to_string());
                        replaced += 1;
                    }
                }
            }
        }
    }

    if replaced > 0 {
        let patched = serde_yaml::to_string(&doc)
            .with_context(|| "Failed to serialize patched .ko.yaml")?;
        std::fs::write(&ko_yaml_path, patched)
            .with_context(|| format!("Failed to write {}", ko_yaml_path.display()))?;
        eprintln!("  Sanitized .ko.yaml: replaced {} restricted base image(s)", replaced);
    }

    Ok(())
}

/// Build a component and return a HashMap of IMAGE_ env var -> SHA-pinned pullspec.
///
/// This function:
/// 1. Loads the component config
/// 2. Clones the repo with the specified git ref
/// 3. Builds with ko to internal registry (capturing SHA refs)
/// 4. Pushes to external registry using skopeo
/// 5. Returns HashMap where key is IMAGE_ env var name, value is SHA pullspec
pub fn run_build_with_refs(
    component: &str,
    external_registry: Option<&str>,
    git_ref: &Option<String>,
) -> Result<HashMap<String, String>> {
    let config_path = config::default_config_path();
    let config = config::load_config(&config_path)
        .with_context(|| format!("Failed to load config from {}", config_path.display()))?;

    let comp_cfg = config
        .components
        .get(component)
        .ok_or_else(|| anyhow::anyhow!("Component '{}' not found in config", component))?;

    // Create temp directory for clone
    let temp_dir = tempfile::tempdir()
        .with_context(|| "Failed to create temp directory")?;

    // Clone with git ref
    eprintln!("  Cloning {} (ref: {})...", comp_cfg.repo, git_ref.as_deref().unwrap_or("HEAD"));
    component::clone_with_ref(&comp_cfg.repo, temp_dir.path(), git_ref.as_deref())?;

    // Sanitize .ko.yaml to replace restricted base images
    sanitize_ko_config(temp_dir.path())?;

    // Get internal registry for building (ko pushes here)
    let internal_registry = registry::get_registry_route()
        .with_context(|| "Failed to get internal registry route")?;
    let internal_registry = format!("{}/tekton-upstream", internal_registry);

    // Login to internal registry
    let registry_host = internal_registry.split('/').next().unwrap_or(&internal_registry);
    registry::registry_login(registry_host)?;

    let image_refs: Vec<(String, String)> = match comp_cfg.build_system.as_deref() {
        Some("docker") => {
            // Docker build to internal registry
            let built = docker_build(temp_dir.path(), &internal_registry, &comp_cfg.images)?;
            // Get digests
            let mut refs = Vec::new();
            for image_name in built {
                let tag = format!("{}/{}", internal_registry, image_name);
                let inspect = exec::run_cmd(
                    "skopeo",
                    &["inspect", "--format", "{{.Digest}}", &format!("docker://{}", tag), "--tls-verify=false"],
                );
                let pullspec = match inspect {
                    Ok(r) if r.exit_code == 0 => {
                        format!("{}@{}", tag.split(':').next().unwrap_or(&tag), r.stdout.trim())
                    }
                    _ => tag,
                };
                refs.push((image_name, pullspec));
            }
            refs
        }
        _ => {
            // ko build with --image-refs to internal registry
            let image_refs_file = temp_dir.path().join(".ko-image-refs");
            let image_refs_path_str = image_refs_file.to_string_lossy().to_string();

            let mut args: Vec<&str> = vec!["build", "--base-import-paths", "--sbom=none", "--image-refs", &image_refs_path_str];
            for p in &comp_cfg.import_paths {
                args.push(p.as_str());
            }

            eprintln!("  Building to internal registry: {}", internal_registry);
            let status = Command::new("ko")
                .args(&args)
                .env("KO_DOCKER_REPO", &internal_registry)
                .env("GOFLAGS", "-mod=vendor")
                .current_dir(temp_dir.path())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .with_context(|| "failed to execute ko")?;

            if !status.success() {
                anyhow::bail!("ko build failed for {}", component);
            }

            // Collect refs from ko output
            if image_refs_file.exists() {
                registry::collect_image_refs(&image_refs_file)?
            } else {
                anyhow::bail!("ko did not produce image refs file");
            }
        }
    };

    // Push to external registry if specified
    let final_refs: Vec<(String, String)> = if let Some(ext_reg) = external_registry {
        eprintln!("  Pushing {} images to external registry: {}", image_refs.len(), ext_reg);
        let mut pushed = Vec::new();
        for (short_name, sha_ref) in image_refs {
            let pinned = registry::push_to_external(&sha_ref, ext_reg)?;
            pushed.push((short_name, pinned));
        }
        pushed
    } else {
        image_refs
    };

    // Map short names to IMAGE_ env var names
    let mut result: HashMap<String, String> = HashMap::new();
    for (short_name, pullspec) in final_refs {
        // Find the IMAGE_ env var for this short name
        if let Some(env_var) = comp_cfg.images.get(&short_name) {
            result.insert(env_var.clone(), pullspec);
        } else {
            eprintln!("  WARNING: No IMAGE_ mapping for {}", short_name);
        }
    }

    Ok(result)
}

/// Clone a git repository (shallow, depth 1) into the given destination directory.
pub fn clone_repo(repo_url: &str, dest: &Path) -> Result<()> {
    let dest_str = dest.to_str().unwrap_or_default();
    exec::run_cmd("git", &["clone", "--depth", "1", repo_url, dest_str])?;
    Ok(())
}

/// Build images using ko with streaming output.
///
/// Sets `KO_DOCKER_REPO` and `GOFLAGS=-mod=vendor` env vars.
/// Uses `--base-import-paths` so image names match the last path segment.
/// Runs ko from `source_dir` with `current_dir`.
/// Returns the list of expected image names derived from import paths.
pub fn ko_build(source_dir: &Path, registry: &str, import_paths: &[String]) -> Result<Vec<String>> {
    ko_build_with_external(source_dir, registry, import_paths, None)
}

/// Build images using ko, optionally pushing to an external registry.
///
/// When `external_registry` is Some, images are copied from the build registry
/// to the external registry using skopeo, and SHA-pinned external pullspecs are returned.
/// When None, behaves like the original ko_build (returns image names only).
pub fn ko_build_with_external(
    source_dir: &Path,
    registry: &str,
    import_paths: &[String],
    external_registry: Option<&str>,
) -> Result<Vec<String>> {
    // Create a temp file for --image-refs output
    let image_refs_file = source_dir.join(".ko-image-refs");

    let image_refs_path_str = image_refs_file.to_string_lossy().to_string();
    let mut args: Vec<&str> = vec!["build", "--base-import-paths", "--sbom=none", "--image-refs", &image_refs_path_str];
    for p in import_paths {
        args.push(p.as_str());
    }

    let docker_config = std::env::var("DOCKER_CONFIG")
        .unwrap_or_else(|_| String::new());
    let mut envs: Vec<(&str, &str)> = vec![
        ("KO_DOCKER_REPO", registry),
        ("GOFLAGS", "-mod=vendor"),
    ];
    if !docker_config.is_empty() {
        envs.push(("DOCKER_CONFIG", &docker_config));
    }

    let status = Command::new("ko")
        .args(&args)
        .envs(envs.iter().cloned())
        .current_dir(source_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| "failed to execute ko")?;

    let code = status.code().unwrap_or(-1);
    if code != 0 {
        anyhow::bail!("ko build failed with exit code {}", code);
    }

    // Collect SHA-pinned image refs from ko output
    let image_refs = if image_refs_file.exists() {
        registry::collect_image_refs(&image_refs_file)?
    } else {
        // Fallback: derive names from import paths (no SHA info)
        import_paths
            .iter()
            .filter_map(|p| p.rsplit('/').next())
            .map(|s| (s.to_string(), format!("{}/{}", registry, s)))
            .collect()
    };

    // If external registry is specified, push each image there
    if let Some(ext_registry) = external_registry {
        eprintln!("Pushing {} images to external registry: {}", image_refs.len(), ext_registry);
        let mut external_pullspecs = Vec::new();
        for (_short_name, sha_ref) in &image_refs {
            let pinned = registry::push_to_external(sha_ref, ext_registry)?;
            external_pullspecs.push(pinned);
        }
        return Ok(external_pullspecs);
    }

    // Return image names (original behavior)
    let image_names: Vec<String> = image_refs
        .iter()
        .map(|(short_name, _)| short_name.clone())
        .collect();

    Ok(image_names)
}

/// Build images using docker/podman for non-ko components (e.g. console-plugin).
///
/// Tries podman first, falls back to docker.
/// Builds and pushes each image defined in the config images map.
pub fn docker_build(source_dir: &Path, registry: &str, images: &HashMap<String, String>) -> Result<Vec<String>> {
    let mut built = Vec::new();
    for image_name in images.keys() {
        let tag = format!("{}/{}", registry, image_name);
        // Try podman first, fall back to docker
        let builder = if Command::new("podman").arg("--version").output().is_ok() {
            "podman"
        } else {
            "docker"
        };
        let status = Command::new(builder)
            .args(["build", "-t", &tag, "."])
            .current_dir(source_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to execute {builder} build"))?;
        if !status.success() {
            anyhow::bail!("{builder} build failed for {image_name}");
        }
        // Push
        let push_status = Command::new(builder)
            .args(["push", &tag, "--tls-verify=false"])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to push {image_name}"))?;
        if !push_status.success() {
            anyhow::bail!("{builder} push failed for {image_name}");
        }
        built.push(image_name.clone());
    }
    Ok(built)
}

/// Build multiple components in parallel using tokio JoinSet.
///
/// Each component gets its own spinner via MultiProgress.
/// Failed builds do not block other builds.
/// Returns a Vec of (component_name, Result<image_names>).
pub async fn build_components_parallel(
    specs: &[ComponentSpec],
    configs: &HashMap<String, ComponentConfig>,
    registry: &str,
) -> Vec<(String, Result<Vec<String>>)> {
    let mp = progress::multi_progress();
    let mut set = JoinSet::new();

    for spec in specs {
        let comp_name = spec.name.clone();
        let git_ref = spec.git_ref.clone();
        let pb = progress::component_spinner(&mp, &comp_name);

        let comp_cfg = match configs.get(&comp_name) {
            Some(c) => c,
            None => {
                pb.finish_with_message(format!("{comp_name}: FAILED - not in config"));
                // We can't spawn a task, just push the error result later
                // Use a trivial task that returns the error
                set.spawn(async move {
                    (comp_name, Err(anyhow::anyhow!("component not in config")))
                });
                continue;
            }
        };

        let repo_url = comp_cfg.repo.clone();
        let import_paths = comp_cfg.import_paths.clone();
        let build_system = comp_cfg.build_system.clone();
        let images = comp_cfg.images.clone();
        let registry = registry.to_string();

        set.spawn(async move {
            // Clone
            pb.set_message(format!("{comp_name}: cloning..."));
            let temp_dir = match tempfile::tempdir() {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("{comp_name}: FAILED - {e}");
                    pb.finish_with_message(msg);
                    return (comp_name, Err(anyhow::anyhow!("temp dir: {e}")));
                }
            };

            let clone_dest = temp_dir.path().to_path_buf();
            let clone_repo = repo_url.clone();
            let clone_ref = git_ref.clone();
            let clone_result = tokio::task::spawn_blocking(move || {
                component::clone_with_ref(&clone_repo, &clone_dest, clone_ref.as_deref())
            })
            .await;

            match clone_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    pb.finish_with_message(format!("{comp_name}: FAILED - {e}"));
                    return (comp_name, Err(e));
                }
                Err(e) => {
                    pb.finish_with_message(format!("{comp_name}: FAILED - join error"));
                    return (comp_name, Err(anyhow::anyhow!("join error: {e}")));
                }
            }

            // Sanitize .ko.yaml to replace restricted base images
            if let Err(e) = sanitize_ko_config(temp_dir.path()) {
                pb.finish_with_message(format!("{comp_name}: FAILED - {e}"));
                return (comp_name, Err(e));
            }

            // Build
            pb.set_message(format!("{comp_name}: building..."));
            let build_dir = temp_dir.path().to_path_buf();
            let build_registry = registry.clone();
            let build_paths = import_paths.clone();
            let build_images = images.clone();
            let build_sys = build_system.clone();
            let build_result = tokio::task::spawn_blocking(move || {
                match build_sys.as_deref() {
                    Some("docker") => docker_build(&build_dir, &build_registry, &build_images),
                    _ => ko_build(&build_dir, &build_registry, &build_paths),
                }
            })
            .await;

            match build_result {
                Ok(Ok(images)) => {
                    pb.finish_with_message(format!("{comp_name}: done ({} images)", images.len()));
                    (comp_name, Ok(images))
                }
                Ok(Err(e)) => {
                    pb.finish_with_message(format!("{comp_name}: FAILED - {e}"));
                    (comp_name, Err(e))
                }
                Err(e) => {
                    pb.finish_with_message(format!("{comp_name}: FAILED - join error"));
                    (comp_name, Err(anyhow::anyhow!("join error: {e}")))
                }
            }
        });
    }

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(pair) => results.push(pair),
            Err(e) => results.push(("unknown".to_string(), Err(anyhow::anyhow!("task panic: {e}")))),
        }
    }

    results
}
