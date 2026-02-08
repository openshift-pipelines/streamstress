mod batch;
mod build;
mod bundle;
mod callback;
mod check;
mod cli;
mod component;
mod config;
mod deploy;
mod dryrun;
mod exec;
mod github;
mod incluster;
mod k8s;
mod konflux;
mod perf;
mod profile;
mod progress;
mod publish;
mod registry;
mod results;
mod setup;
mod snapshot;
mod test;
mod types;

use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Check { fix } => {
            match check::run_check(cli.verbose) {
                Ok(true) => {
                    if fix {
                        eprintln!("\nAll checks passed, nothing to fix.");
                    }
                    std::process::exit(0);
                }
                Ok(false) => {
                    if fix {
                        eprintln!("\nRunning auto-setup to fix issues...");
                        let result = tokio::task::spawn_blocking(|| {
                            setup::run_auto_setup()
                        }).await.expect("spawn_blocking panicked");
                        if let Err(e) = result {
                            eprintln!("Auto-setup error: {e:#}");
                            std::process::exit(2);
                        }
                        std::process::exit(0);
                    }
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        Commands::Build { component, registry, as_of: _ } => {
            if !cli.no_auto_setup {
                let result = tokio::task::spawn_blocking(|| {
                    setup::run_auto_setup()
                }).await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("WARNING: Auto-setup had errors: {e:#}"),
                    Err(e) => eprintln!("WARNING: Auto-setup panicked: {e}"),
                }
            }
            match run_build(&component, registry.as_deref()) {
                Ok(_) => std::process::exit(0),
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        Commands::Deploy {
            component,
            registry,
        } => {
            if !cli.no_auto_setup {
                let result = tokio::task::spawn_blocking(|| {
                    setup::run_auto_setup()
                }).await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("WARNING: Auto-setup had errors: {e:#}"),
                    Err(e) => eprintln!("WARNING: Auto-setup panicked: {e}"),
                }
            }
            // Placeholder: in production, built_images comes from the build phase output.
            // For now, derive image names from the TOML config for the given component.
            let built_images = match load_image_names_from_config(&component) {
                Ok(names) => names,
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(2);
                }
            };
            eprintln!("Note: using image names from config (placeholder until build phase integration)");
            let verbose = cli.verbose;
            let result = tokio::task::spawn_blocking(move || {
                deploy::run_deploy(&component, &registry, &built_images, verbose)
            }).await;
            match result {
                Ok(Ok(_)) => std::process::exit(0),
                Ok(Err(e)) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(2);
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(2);
                }
            }
        }
        Commands::Test {
            tags,
            release_tests_ref,
            output_dir,
            profile,
        } => {
            match test::run_tests(&tags, &release_tests_ref, std::path::Path::new(&output_dir), cli.verbose, profile).await {
                Ok(true) => std::process::exit(0),
                Ok(false) => std::process::exit(1),
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        Commands::Run {
            components,
            as_of,
            date_range,
            dry_run,
            json,
            tags,
            release_tests_ref,
            output_dir,
            registry,
            skip_build,
            profile,
            image,
            perf,
            perf_scenario,
            perf_ref,
        } => {
            // Handle --date-range for batch historical runs
            if let Some(ref range) = date_range {
                let exit_code = run_batch_historical(
                    range,
                    &components,
                    &release_tests_ref,
                    &output_dir,
                    skip_build,
                    registry.as_deref(),
                    cli.verbose,
                    profile,
                    cli.no_auto_setup,
                    dry_run,
                );
                std::process::exit(exit_code);
            }

            let mut specs = match components {
                Some(ref s) => match component::parse_component_specs(s) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(2);
                    }
                },
                None => component::default_specs(),
            };

            // Apply --as-of date to components without explicit refs
            if let Some(ref date) = as_of {
                component::apply_as_of_date(&mut specs, date);
            }

            if !cli.no_auto_setup && !skip_build && !incluster::is_incluster() {
                let result = tokio::task::spawn_blocking(|| {
                    setup::run_auto_setup()
                }).await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("WARNING: Auto-setup had errors: {e:#}"),
                    Err(e) => eprintln!("WARNING: Auto-setup panicked: {e}"),
                }
            }

            if skip_build {
                // In-cluster mode: skip clone/build, go straight to deploy+test
                let mut exit_code = run_deploy_and_test(&specs, &tags, &release_tests_ref, &output_dir, registry.as_deref(), cli.verbose, profile, cli.no_auto_setup, as_of.as_deref()).await;

                // Run performance tests if --perf is set
                if perf {
                    let perf_exit = run_perf_tests_standalone(&output_dir, &perf_scenario, perf_ref.as_deref(), cli.verbose, profile).await;
                    exit_code = combine_exit_codes(exit_code, perf_exit);
                }

                // Publish results directly to gh-pages if configured
                callback::maybe_publish_results();
                std::process::exit(exit_code);
            }

            if incluster::is_incluster() {
                // Already in-cluster: run deploy+test directly (don't re-wrap)
                let mut exit_code = run_deploy_and_test(&specs, &tags, &release_tests_ref, &output_dir, registry.as_deref(), cli.verbose, profile, cli.no_auto_setup, as_of.as_deref()).await;

                // Run performance tests if --perf is set
                if perf {
                    let perf_exit = run_perf_tests_standalone(&output_dir, &perf_scenario, perf_ref.as_deref(), cli.verbose, profile).await;
                    exit_code = combine_exit_codes(exit_code, perf_exit);
                }

                // Publish results directly to gh-pages if configured
                callback::maybe_publish_results();
                std::process::exit(exit_code);
            }

            // Normal mode: build locally, then create in-cluster Job for deploy+test
            // Note: perf flags are NOT passed to in-cluster Job yet (would need incluster module changes)
            // For now, perf tests run only in skip_build or is_incluster paths
            let exit_code = run_multi(specs, dry_run, json, &tags, &release_tests_ref, &output_dir, registry.as_deref(), cli.verbose, as_of.as_deref(), image.as_deref()).await;
            std::process::exit(exit_code);
        }
        Commands::Results { output_dir } => {
            let output_path = std::path::Path::new(&output_dir);
            let results_dir = output_path.join("results");
            if let Err(e) = std::fs::create_dir_all(&results_dir) {
                eprintln!("Error creating results directory: {e:#}");
                std::process::exit(2);
            }

            // Try JUnit XML first, then fall back to Gauge stdout
            let junit_path = results_dir.join("junit.xml");
            let stdout_path = output_path.join("logs/test-stdout.log");

            let parse_result = if junit_path.exists() {
                results::parse_junit_xml(&junit_path)
            } else if stdout_path.exists() {
                results::parse_gauge_stdout(&stdout_path)
            } else {
                eprintln!("No test results found in {}", output_dir);
                eprintln!("Expected: {}/results/junit.xml or {}/logs/test-stdout.log", output_dir, output_dir);
                std::process::exit(2);
            };

            match parse_result {
                Ok(result) => {
                    let categorized = results::categorize_results(&result);
                    results::print_categorized_results(&categorized);

                    let json_path = results_dir.join("results.json");
                    if let Err(e) = results::write_categorized_json(&categorized, &json_path) {
                        eprintln!("Error writing JSON: {e:#}");
                        std::process::exit(2);
                    }
                    println!("Results written to {}", json_path.display());
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Error parsing test results: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        Commands::Status => {
            let client = match kube::Client::try_default().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error connecting to cluster: {e:#}");
                    std::process::exit(2);
                }
            };
            let namespace = "openshift-pipelines";
            if let Err(e) = incluster::show_status(&client, namespace).await {
                eprintln!("Error: {e:#}");
                std::process::exit(2);
            }
        }
        Commands::Konflux {
            registry,
            operator_branch,
            output_dir,
            components,
            refs,
            as_of,
            trigger,
            pipeline_namespace,
            timeout,
        } => {
            let output_path = std::path::Path::new(&output_dir);
            std::fs::create_dir_all(output_path).expect("Failed to create output directory");

            let snapshot_path = output_path.join("snapshot.json");
            let operator_dir_path = output_path.join("operator");

            // Check if we already have a snapshot (skip build phase)
            let need_build = !snapshot_path.exists();

            if need_build {
                eprintln!("\n=== Building Konflux SNAPSHOT ===\n");

                // Auto-setup cluster if needed
                if !cli.no_auto_setup {
                    let result = tokio::task::spawn_blocking(|| {
                        setup::run_auto_setup()
                    }).await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => eprintln!("WARNING: Auto-setup had errors: {e:#}"),
                        Err(e) => eprintln!("WARNING: Auto-setup panicked: {e}"),
                    }
                }

                // Parse component specs (refs can be embedded like "pipeline:v0.60.0,triggers")
                let mut specs = match component::parse_component_specs(&components) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Error parsing components: {e}");
                        std::process::exit(2);
                    }
                };

                // Apply refs if provided separately (overrides embedded refs)
                if let Some(ref refs_str) = refs {
                    for ref_part in refs_str.split(',') {
                        if let Some((name, git_ref)) = ref_part.trim().split_once(':') {
                            for spec in &mut specs {
                                if spec.name == name.trim() {
                                    spec.git_ref = Some(git_ref.trim().to_string());
                                }
                            }
                        }
                    }
                }

                // Apply --as-of date to components without explicit refs
                if let Some(ref date) = as_of {
                    component::apply_as_of_date(&mut specs, date);
                }

                // Step 1: Build upstream images and push to external registry
                eprintln!("Step 1: Building upstream images...");
                let mut all_image_refs: std::collections::HashMap<String, String> = std::collections::HashMap::new();

                for spec in &specs {
                    eprintln!("\n  Building {}...", spec.name);
                    match build::run_build_with_refs(&spec.name, Some(&registry), &spec.git_ref) {
                        Ok(refs) => {
                            for (name, pullspec) in refs {
                                all_image_refs.insert(name, pullspec);
                            }
                        }
                        Err(e) => {
                            eprintln!("Error building {}: {e:#}", spec.name);
                            std::process::exit(2);
                        }
                    }
                }

                eprintln!("\n  Built {} images", all_image_refs.len());

                // Step 2: Clone operator repo
                eprintln!("\nStep 2: Cloning operator repo (branch: {})...", operator_branch);
                let temp_operator_dir = match bundle::clone_operator_repo(&operator_branch) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("Error cloning operator: {e:#}");
                        std::process::exit(2);
                    }
                };

                // Copy operator dir to output for pipeline trigger
                if operator_dir_path.exists() {
                    let _ = std::fs::remove_dir_all(&operator_dir_path);
                }
                let copy_result = std::process::Command::new("cp")
                    .args(["-r", temp_operator_dir.to_str().unwrap(), operator_dir_path.to_str().unwrap()])
                    .status();
                if copy_result.is_err() || !copy_result.unwrap().success() {
                    eprintln!("WARNING: Failed to copy operator dir to output");
                }

                // Step 3: Patch CSV with upstream images
                eprintln!("\nStep 3: Patching CSV with upstream images...");
                if let Err(e) = bundle::patch_csv(&temp_operator_dir, &all_image_refs) {
                    eprintln!("Error patching CSV: {e:#}");
                    std::process::exit(2);
                }

                // Generate timestamp tag
                let tag = format!("upstream-{}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs());

                // Step 4: Build bundle image
                eprintln!("\nStep 4: Building operator bundle image...");
                let bundle_pullspec = match bundle::build_bundle_image(&temp_operator_dir, &registry, &tag) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Error building bundle: {e:#}");
                        std::process::exit(2);
                    }
                };

                // Step 5: Build FBC index image
                eprintln!("\nStep 5: Building FBC index image...");
                let index_pullspec = match bundle::build_index_image(&bundle_pullspec, &registry, &tag) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Error building index: {e:#}");
                        std::process::exit(2);
                    }
                };

                // Step 6: Generate SNAPSHOT
                eprintln!("\nStep 6: Generating SNAPSHOT...");
                if let Err(e) = snapshot::generate_snapshot(&index_pullspec, &snapshot_path) {
                    eprintln!("Error generating snapshot: {e:#}");
                    std::process::exit(2);
                }

                eprintln!("\n=== SNAPSHOT generated successfully ===");
                eprintln!("  Output: {}", snapshot_path.display());
                eprintln!("  Index: {}", index_pullspec);

                // Cleanup temp dir
                let _ = std::fs::remove_dir_all(&temp_operator_dir);
            } else {
                eprintln!("Using existing snapshot at {}", snapshot_path.display());
            }

            // Trigger pipeline if requested
            if trigger {
                if !operator_dir_path.exists() {
                    eprintln!("Error: operator directory not found at {}", operator_dir_path.display());
                    eprintln!("Run without --trigger first to generate the SNAPSHOT and operator clone.");
                    std::process::exit(2);
                }

                eprintln!("\n=== Triggering standalone release-test-pipeline ===");
                let pr_name = match konflux::trigger_pipeline(
                    &snapshot_path,
                    &operator_dir_path,
                    &pipeline_namespace,
                ) {
                    Ok(name) => name,
                    Err(e) => {
                        eprintln!("Error triggering pipeline: {e:#}");
                        std::process::exit(2);
                    }
                };

                eprintln!("PipelineRun: {}", pr_name);
                let result = match konflux::wait_for_pipeline(&pr_name, &pipeline_namespace, timeout) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Error waiting for pipeline: {e:#}");
                        std::process::exit(2);
                    }
                };

                let duration_min = result.duration.as_secs() / 60;
                println!("\nPipelineRun: {}", result.name);
                println!("Status: {:?}", result.status);
                println!("Reason: {}", result.reason);
                println!("Duration: {}m {}s", duration_min, result.duration.as_secs() % 60);

                // Collect results from pipeline task logs (regardless of pass/fail)
                if result.status != konflux::PipelineRunStatus::Timeout {
                    eprintln!("\n=== Collecting pipeline results ===");
                    match konflux::collect_results(&pr_name, &pipeline_namespace, output_path) {
                        Ok(task_results) => {
                            if !task_results.is_empty() {
                                konflux::print_pipeline_summary(&task_results);

                                if let Err(e) = konflux::save_konflux_results(
                                    &task_results, &snapshot_path, output_path,
                                ) {
                                    eprintln!("WARNING: Failed to save results: {e:#}");
                                } else {
                                    eprintln!(
                                        "\nResults saved to {}/results/results.json",
                                        output_dir
                                    );
                                    eprintln!(
                                        "Run `streamstress publish --output-dir {}` to update dashboard.",
                                        output_dir
                                    );
                                }
                            } else {
                                eprintln!("No test results collected from pipeline tasks.");
                            }
                        }
                        Err(e) => {
                            eprintln!("WARNING: Failed to collect pipeline results: {e:#}");
                        }
                    }
                }

                match result.status {
                    konflux::PipelineRunStatus::Succeeded => std::process::exit(0),
                    konflux::PipelineRunStatus::Failed => std::process::exit(1),
                    konflux::PipelineRunStatus::Timeout => std::process::exit(2),
                }
            } else {
                eprintln!("\nTo trigger the pipeline, run:");
                eprintln!("  streamstress konflux --registry {} --trigger --output-dir {}", registry, output_dir);
                std::process::exit(0);
            }
        }
        Commands::Publish { output_dir, remote, label } => {
            match publish::publish(&output_dir, remote.as_deref(), label.as_deref()) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        Commands::Logs { job } => {
            let client = match kube::Client::try_default().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error connecting to cluster: {e:#}");
                    std::process::exit(2);
                }
            };
            let namespace = "openshift-pipelines";
            if let Err(e) = incluster::stream_job_logs(&client, namespace, job.as_deref()).await {
                eprintln!("Error: {e:#}");
                std::process::exit(2);
            }
        }
    }
}

/// Deploy and test only (used in-cluster where builds already happened locally).
async fn run_deploy_and_test(
    specs: &[component::ComponentSpec],
    tags: &str,
    release_tests_ref: &str,
    output_dir: &str,
    registry_override: Option<&str>,
    verbose: bool,
    profile: bool,
    no_auto_setup: bool,
    as_of: Option<&str>,
) -> i32 {
    if !no_auto_setup {
        let result = tokio::task::spawn_blocking(|| {
            setup::run_auto_setup()
        }).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("WARNING: Auto-setup had errors: {e:#}"),
            Err(e) => eprintln!("WARNING: Auto-setup panicked: {e}"),
        }
    }

    let _cfg = match config::load_config(&config::default_config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e:#}");
            return 2;
        }
    };

    let registry_route = match registry_override {
        Some(r) => r.to_string(),
        None => match registry::get_registry_route() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error: {e:#}");
                return 2;
            }
        },
    };

    // Deploy phase
    eprintln!("\n=== Deploying (in-cluster) ===");
    for spec in specs {
        let image_names = match load_image_names_from_config(&spec.name) {
            Ok(names) => names,
            Err(e) => {
                eprintln!("WARNING: Could not load images for {}: {e:#}", spec.name);
                continue;
            }
        };
        let comp_name = spec.name.clone();
        let registry_route = registry_route.clone();
        let result = tokio::task::spawn_blocking(move || {
            deploy::run_deploy(&comp_name, &registry_route, &image_names, verbose)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("WARNING: Deploy failed for {}: {e:#}", spec.name),
            Err(e) => eprintln!("WARNING: Deploy panicked for {}: {e}", spec.name),
        }
    }

    // Test phase
    eprintln!("\n=== Running tests (in-cluster) ===");
    let test_result = test::run_tests(tags, release_tests_ref, std::path::Path::new(output_dir), verbose, profile).await;

    // Write as-of metadata for dashboard tracking if --as-of was used
    if let Some(date) = as_of {
        write_as_of_metadata(output_dir, date, specs);
    }

    match test_result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("Error running tests: {e:#}");
            1
        }
    }
}

/// Write as-of metadata file for dashboard tracking.
///
/// Creates `results/metadata.json` with as_of_date and resolved component refs.
/// This is read by the publish command to include in run data.
fn write_as_of_metadata(output_dir: &str, as_of: &str, specs: &[component::ComponentSpec]) {
    let output_path = std::path::Path::new(output_dir);
    let results_dir = output_path.join("results");
    if std::fs::create_dir_all(&results_dir).is_err() {
        eprintln!("WARNING: Could not create results directory for metadata");
        return;
    }

    let meta_path = results_dir.join("metadata.json");
    let meta = serde_json::json!({
        "as_of_date": as_of,
        "resolved_components": specs.iter().map(|s| {
            serde_json::json!({
                "name": s.name,
                "git_ref": s.git_ref.as_deref().unwrap_or("HEAD"),
                "as_of_date": s.as_of_date
            })
        }).collect::<Vec<_>>()
    });

    match serde_json::to_string_pretty(&meta) {
        Ok(json_str) => {
            if let Err(e) = std::fs::write(&meta_path, json_str) {
                eprintln!("WARNING: Could not write metadata.json: {e}");
            } else {
                eprintln!("Wrote as-of metadata to {}", meta_path.display());
            }
        }
        Err(e) => {
            eprintln!("WARNING: Could not serialize metadata: {e}");
        }
    }
}

/// Load image names from config for a component (placeholder for build phase output).
fn load_image_names_from_config(component: &str) -> anyhow::Result<Vec<String>> {
    let cfg = config::load_config(&config::default_config_path())?;
    let comp = cfg
        .components
        .get(component)
        .ok_or_else(|| anyhow::anyhow!("Component '{component}' not in config"))?;
    Ok(comp.images.keys().cloned().collect())
}

fn run_build(component: &str, external_registry: Option<&str>) -> anyhow::Result<()> {
    // Stage 1: Registry setup
    let pb = progress::stage_spinner("Registry setup");
    let route = registry::get_registry_route()?;
    registry::ensure_namespace(registry::DEFAULT_NAMESPACE)?;
    registry::registry_login(&route)?;
    let registry_target = format!("{}/{}", route, registry::DEFAULT_NAMESPACE);
    progress::finish_spinner(&pb, true);

    // Stage 2: Clone upstream source
    let pb = progress::stage_spinner("Clone upstream source");
    let temp_dir = tempfile::tempdir()?;
    let repo_url = format!("https://github.com/tektoncd/{}.git", component);
    build::clone_repo(&repo_url, temp_dir.path())?;
    build::sanitize_ko_config(temp_dir.path())?;
    progress::finish_spinner(&pb, true);

    // Stage 3: Build images with ko
    let cfg = config::load_config(&config::default_config_path())?;
    let comp_cfg = cfg
        .components
        .get(component)
        .ok_or_else(|| anyhow::anyhow!("Component '{}' not in config", component))?;

    let pb = progress::stage_spinner("Build images with ko");
    let image_names = build::ko_build_with_external(
        temp_dir.path(),
        &registry_target,
        &comp_cfg.import_paths,
        external_registry,
    )?;
    progress::finish_spinner(&pb, true);

    if external_registry.is_some() {
        println!("\nBuilt and pushed {} images for {} to external registry:", image_names.len(), component);
    } else {
        println!("\nBuilt {} images for {}:", image_names.len(), component);
    }
    for name in &image_names {
        println!("  - {}", name);
    }

    Ok(())
}

/// Multi-component orchestration: build all in parallel, then create in-cluster Job for deploy+test.
/// Returns exit code: 0=success, 2=error.
async fn run_multi(
    specs: Vec<component::ComponentSpec>,
    dry_run: bool,
    json_output: bool,
    tags: &str,
    release_tests_ref: &str,
    output_dir: &str,
    registry_override: Option<&str>,
    _verbose: bool,
    as_of: Option<&str>,
    image_override: Option<&str>,
) -> i32 {
    let cfg = match config::load_config(&config::default_config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e:#}");
            return 2;
        }
    };

    // Dry-run: just print the plan
    if dry_run {
        return print_dry_run_plan(&specs, &cfg, json_output, as_of);
    }

    // When --image is provided, skip build phase entirely and use pre-built image
    if let Some(ref img) = image_override {
        eprintln!("\n=== Using pre-built image: {} ===", img);
        eprintln!("Skipping registry setup and component builds.");

        // Build CLI args for the in-cluster Job
        let spec_str = specs.iter().map(|s| {
            match &s.git_ref {
                Some(r) => format!("{}:{}", s.name, r),
                None => s.name.clone(),
            }
        }).collect::<Vec<_>>().join(",");
        let mut cli_args = vec![
            "run".to_string(),
            "--components".to_string(), spec_str,
            "--tags".to_string(), tags.to_string(),
            "--release-tests-ref".to_string(), release_tests_ref.to_string(),
            "--output-dir".to_string(), output_dir.to_string(),
        ];
        if let Some(reg) = registry_override {
            cli_args.push("--registry".to_string());
            cli_args.push(reg.to_string());
        }
        if let Some(date) = as_of {
            cli_args.push("--as-of".to_string());
            cli_args.push(date.to_string());
        }

        let img_clone = img.to_string();
        // Registry route not needed when using pre-built image, pass empty string
        let result = tokio::task::spawn_blocking(move || {
            incluster::run_incluster("", "openshift-pipelines", &cli_args, Some(&img_clone))
        }).await;
        return match result {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => { eprintln!("Error creating in-cluster Job: {e:#}"); 2 }
            Err(e) => { eprintln!("Error: in-cluster task panicked: {e}"); 2 }
        };
    }

    // Registry setup
    let registry_route = match registry_override {
        Some(r) => r.to_string(),
        None => match registry::get_registry_route() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error: {e:#}");
                return 2;
            }
        },
    };

    if let Err(e) = registry::ensure_namespace(registry::DEFAULT_NAMESPACE) {
        eprintln!("Error ensuring namespace: {e:#}");
        return 2;
    }
    if let Err(e) = registry::registry_login(&registry_route) {
        eprintln!("Error logging into registry: {e:#}");
        return 2;
    }

    let registry_target = format!("{}/{}", registry_route, registry::DEFAULT_NAMESPACE);

    // Build phase: build all components in parallel
    eprintln!("\n=== Building components in parallel ===");
    let results = build::build_components_parallel(&specs, &cfg.components, &registry_target).await;

    let mut all_images: Vec<(String, Vec<String>)> = Vec::new();
    let mut build_failed = false;

    for (name, result) in results {
        match result {
            Ok(images) => {
                eprintln!("  {} built {} images", name, images.len());
                all_images.push((name, images));
            }
            Err(e) => {
                eprintln!("  {} FAILED: {e:#}", name);
                build_failed = true;
            }
        }
    }

    if build_failed {
        return 2;
    }

    // Deploy+test phase: create in-cluster Job instead of running locally
    eprintln!("\n=== Creating in-cluster Job for deploy+test ===");
    let spec_str = specs.iter().map(|s| {
        match &s.git_ref {
            Some(r) => format!("{}:{}", s.name, r),
            None => s.name.clone(),
        }
    }).collect::<Vec<_>>().join(",");
    let mut cli_args = vec![
        "run".to_string(),
        "--components".to_string(), spec_str,
        "--tags".to_string(), tags.to_string(),
        "--release-tests-ref".to_string(), release_tests_ref.to_string(),
        "--output-dir".to_string(), output_dir.to_string(),
    ];
    if let Some(reg) = registry_override {
        cli_args.push("--registry".to_string());
        cli_args.push(reg.to_string());
    }
    if let Some(date) = as_of {
        cli_args.push("--as-of".to_string());
        cli_args.push(date.to_string());
    }

    let registry_route_clone = registry_route.clone();
    let result = tokio::task::spawn_blocking(move || {
        incluster::run_incluster(&registry_route_clone, "openshift-pipelines", &cli_args, None)
    }).await;
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => { eprintln!("Error creating in-cluster Job: {e:#}"); 2 }
        Err(e) => { eprintln!("Error: in-cluster task panicked: {e}"); 2 }
    }
}

/// Print the dry-run execution plan using the dryrun module.
fn print_dry_run_plan(
    specs: &[component::ComponentSpec],
    cfg: &config::Config,
    json_output: bool,
    as_of: Option<&str>,
) -> i32 {
    let resolved = dryrun::resolve_components_with_date(specs, &cfg.components, as_of);
    if json_output {
        dryrun::print_json(&resolved);
    } else {
        dryrun::print_table(&resolved);
    }
    0
}

/// Run performance tests standalone (after functional tests).
///
/// Clones the openshift-pipelines/performance repo, runs the specified scenario,
/// and writes results to output_dir/perf/.
async fn run_perf_tests_standalone(
    output_dir: &str,
    perf_scenario: &str,
    perf_ref: Option<&str>,
    verbose: bool,
    profile: bool,
) -> i32 {
    eprintln!("\n========================================");
    eprintln!("PERFORMANCE TESTS");
    eprintln!("========================================\n");

    // Parse scenario
    let scenario: perf::PerfScenario = match perf_scenario.parse() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Invalid perf scenario: {}", e);
            return 2;
        }
    };

    // Clone performance repo to temp dir
    let temp_dir = std::env::temp_dir().join("streamstress-perf");
    let perf_repo_dir = match perf::clone_perf_repo(&temp_dir, perf_ref) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to clone performance repo: {}", e);
            return 2;
        }
    };

    // Create perf output directory
    let perf_output_dir = std::path::Path::new(output_dir).join("perf");
    if let Err(e) = std::fs::create_dir_all(&perf_output_dir) {
        eprintln!("Failed to create perf output directory: {}", e);
        return 2;
    }

    // Start resource profiling if requested
    let profiler = if profile {
        match start_perf_profiling().await {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("WARNING: Failed to start profiling: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Run performance tests
    let perf_result = perf::run_perf_tests(&perf_repo_dir, &scenario, &perf_output_dir, verbose);

    // Stop profiling and collect resource data
    if let Some(p) = profiler {
        if let Ok(resource_data) = stop_perf_profiling(p).await {
            let resource_path = perf_output_dir.join("resource-profile.json");
            if let Ok(json) = serde_json::to_string_pretty(&resource_data) {
                if let Err(e) = std::fs::write(&resource_path, json) {
                    eprintln!("WARNING: Failed to write resource profile: {}", e);
                }
            }
        }
    }

    match perf_result {
        Ok(result) => {
            if let Err(e) = perf::write_perf_results(&result, &perf_output_dir) {
                eprintln!("WARNING: Failed to write perf results: {}", e);
            }

            eprintln!("\nPerformance Test Results:");
            eprintln!("  Scenario: {}", result.scenario);
            eprintln!("  Passed: {}", result.passed);
            eprintln!("  Duration: {:.1}s", result.duration_seconds);
            if let Some(throughput) = result.metrics.throughput_per_minute {
                eprintln!("  Throughput: {:.1} runs/min", throughput);
            }
            if let Some(p50) = result.metrics.p50_latency_seconds {
                eprintln!("  P50 Latency: {:.2}s", p50);
            }
            if let Some(p95) = result.metrics.p95_latency_seconds {
                eprintln!("  P95 Latency: {:.2}s", p95);
            }

            if result.passed { 0 } else { 1 }
        }
        Err(e) => {
            eprintln!("Performance test error: {}", e);
            2
        }
    }
}

/// Start resource profiling for performance tests.
async fn start_perf_profiling() -> anyhow::Result<profile::MetricsCollector> {
    let client = kube::Client::try_default().await?;

    // Check if metrics API is available
    if !profile::check_metrics_available(&client).await? {
        anyhow::bail!("Metrics API not available");
    }

    Ok(profile::MetricsCollector::start(client))
}

/// Stop profiling and return collected spec profiles.
async fn stop_perf_profiling(collector: profile::MetricsCollector) -> anyhow::Result<Vec<profile::SpecProfile>> {
    collector.stop().await
}

/// Combine exit codes from functional and performance tests.
///
/// Returns:
/// - 0 if both passed
/// - 1 if any test failed
/// - 2 if any error occurred
fn combine_exit_codes(func_exit: i32, perf_exit: i32) -> i32 {
    match (func_exit, perf_exit) {
        (0, 0) => 0,           // Both passed
        (2, _) | (_, 2) => 2,  // Any error
        _ => 1,                 // Any failure
    }
}

/// Run batch historical tests for a date range.
///
/// Iterates through each date in the range, running build-deploy-test for each.
/// Results are stored in output-dir/DATE/ subdirectories.
/// Note: Full implementation in plan 14-03.
fn run_batch_historical(
    range: &batch::DateRange,
    components: &Option<String>,
    release_tests_ref: &str,
    output_dir: &str,
    skip_build: bool,
    registry: Option<&str>,
    verbose: bool,
    profile: bool,
    no_auto_setup: bool,
    dry_run: bool,
) -> i32 {
    let dates = batch::generate_dates(range);
    let mut progress = batch::BatchProgress::new(dates.len());

    if dry_run {
        eprintln!("\n=== BATCH HISTORICAL RUN (DRY-RUN) ===");
        eprintln!("Date range: {} to {}", range.start, range.end);
        eprintln!("Total dates: {}", dates.len());
        eprintln!("Components: {:?}", components);
        eprintln!("\nWould process dates:");
        for date in &dates {
            eprintln!("  {}", date.format("%Y-%m-%d"));
        }
        return 0;
    }

    eprintln!("\n=== BATCH HISTORICAL RUN ===");
    eprintln!("Date range: {} to {}", range.start, range.end);
    eprintln!("Total dates: {}", dates.len());

    for date in dates {
        let date_str = date.format("%Y-%m-%d").to_string();
        progress.advance(&date_str);
        progress.print_progress();

        // Create date-specific output directory
        let date_output_dir = format!("{}/{}", output_dir, date_str);
        if let Err(e) = std::fs::create_dir_all(&date_output_dir) {
            eprintln!("ERROR: Failed to create output directory: {}", e);
            progress.record_result(2);
            continue;
        }

        // Build command args for this date
        let mut args = vec![
            "run".to_string(),
            "--as-of".to_string(),
            date_str.clone(),
            "--release-tests-ref".to_string(),
            release_tests_ref.to_string(),
            "--output-dir".to_string(),
            date_output_dir,
        ];

        if let Some(c) = components {
            args.push("--components".to_string());
            args.push(c.clone());
        }
        if skip_build {
            args.push("--skip-build".to_string());
        }
        if let Some(reg) = registry {
            args.push("--registry".to_string());
            args.push(reg.to_string());
        }
        if no_auto_setup {
            args.push("--no-auto-setup".to_string());
        }

        // Execute via subprocess (self-invocation)
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(&args);
        if verbose {
            cmd.arg("--verbose");
        }
        let status = cmd.status();

        let exit_code = match status {
            Ok(s) => s.code().unwrap_or(2),
            Err(e) => {
                eprintln!("ERROR: Failed to execute for date {}: {}", date_str, e);
                2
            }
        };

        progress.record_result(exit_code);
        let _ = profile; // Silence unused warning - profile passed to subprocess
    }

    progress.print_summary();

    // Return overall exit code
    if progress.errors > 0 {
        2
    } else if progress.failed > 0 {
        1
    } else {
        0
    }
}
