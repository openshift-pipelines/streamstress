pub mod mapping;
pub mod operator;
pub mod wait;

use crate::{config, k8s, progress};

const INTERNAL_REGISTRY: &str = "image-registry.openshift-image-registry.svc:5000";

/// Convert an external registry route to the internal cluster service URL.
/// e.g. "default-route-openshift-image-registry.apps.example.com/tekton-upstream"
///   -> "image-registry.openshift-image-registry.svc:5000/tekton-upstream"
fn to_internal_registry(registry: &str) -> String {
    // If it already uses the internal service, return as-is
    if registry.starts_with(INTERNAL_REGISTRY) {
        // Ensure namespace is present even if internal registry was passed bare
        if !registry.contains("/tekton-upstream") {
            return format!("{}/tekton-upstream", INTERNAL_REGISTRY);
        }
        return registry.to_string();
    }
    // Extract namespace path after the hostname (e.g. "/tekton-upstream")
    if let Some(slash_pos) = registry.find('/') {
        let namespace_path = &registry[slash_pos..];
        format!("{}{}", INTERNAL_REGISTRY, namespace_path)
    } else {
        // No namespace in the path — default to tekton-upstream
        format!("{}/tekton-upstream", INTERNAL_REGISTRY)
    }
}

/// Run the deploy flow: verify operator, map images, patch operator deployment.
pub fn run_deploy(
    component: &str,
    registry: &str,
    built_images: &[String],
    _verbose: bool,
) -> anyhow::Result<()> {
    // Step 1: Connect to cluster
    let pb = progress::stage_spinner("Connecting to cluster");
    let (rt, client) = k8s::create_kube_client()?;
    progress::finish_spinner(&pb, true);

    // Step 2: Verify operator is installed
    let pb = progress::stage_spinner("Verifying OpenShift Pipelines operator");
    operator::verify_operator(&rt, &client)?;
    progress::finish_spinner(&pb, true);

    // Step 3: Load component config
    let pb = progress::stage_spinner("Loading component config");
    let config_path = config::default_config_path();
    let config = config::load_config(&config_path)?;
    progress::finish_spinner(&pb, true);

    // Step 4: Build image mappings using internal registry URL
    // Pods pull from the internal service, not the external route
    let internal_registry = to_internal_registry(registry);
    let pb = progress::stage_spinner("Building image mappings");
    let mappings = mapping::build_image_mappings(&config, component, &internal_registry, built_images)?;
    progress::finish_spinner(&pb, true);

    // Step 5: Display mapping table
    mapping::display_mapping_table(&mappings);

    // Step 6: Find operator deployment
    let pb = progress::stage_spinner("Finding operator controller deployment");
    let (namespace, deployment_name) = operator::find_operator_deployment(&rt, &client)?;
    progress::finish_spinner(&pb, true);

    // Step 7: Patch Deployment directly (OLM does NOT revert deployment patches per issue #1853)
    let pb = progress::stage_spinner("Patching operator Deployment with IMAGE_ env vars");
    operator::patch_operator_deployment_env(&rt, &client, &namespace, &deployment_name, &mappings)?;
    progress::finish_spinner(&pb, true);
    eprintln!("  Patched {}/{} with {} IMAGE_ env vars", namespace, deployment_name, mappings.len());

    // Step 8: Restart operator pod so it picks up updated env vars
    let pb = progress::stage_spinner("Restarting operator pod");
    operator::restart_operator_pod(&rt, &client, &namespace, &deployment_name)?;
    progress::finish_spinner(&pb, true);

    // Step 8a: Verify env vars persisted AFTER restart (detect OLM revert)
    // Must happen after restart — OLM could revert between patch and restart
    let pb = progress::stage_spinner("Verifying Deployment env vars persisted");
    let env_ok = operator::verify_deployment_env(&rt, &client, &namespace, &deployment_name, &mappings)?;
    progress::finish_spinner(&pb, env_ok);
    if !env_ok {
        anyhow::bail!(
            "Deployment env vars were reverted (possibly by OLM). \
             Cannot proceed — operator would reconcile with old images."
        );
    }

    // Step 9: Ensure image-pull RBAC for upstream namespace
    let image_namespace = internal_registry
        .rsplit('/')
        .next()
        .unwrap_or("tekton-upstream");
    let pb = progress::stage_spinner("Ensuring image-pull RBAC");
    operator::ensure_image_pull_rbac(&rt, &client, image_namespace)?;
    progress::finish_spinner(&pb, true);

    // Step 10: Delete InstallerSets to force operator re-reconciliation with new images
    let pb = progress::stage_spinner("Deleting InstallerSets to trigger re-reconciliation");
    let prefix = config.components.get(component)
        .and_then(|c| c.installer_set_prefix.as_deref());
    let deleted = operator::delete_installer_sets(&rt, &client, component, prefix)?;
    progress::finish_spinner(&pb, true);
    eprintln!("  Deleted {} InstallerSets — operator will recreate with upstream images", deleted);

    // Step 11: Wait for reconciliation (failure is a warning, not fatal)
    eprintln!();
    match wait::wait_for_reconciliation(&rt, &client, &mappings, _verbose) {
        Ok(()) => {
            eprintln!(
                "Deploy complete. All Tekton components running with upstream images."
            );
        }
        Err(e) => {
            eprintln!("WARNING: Reconciliation wait failed:");
            eprintln!("  {}", e);
            eprintln!("  Continuing — deployment failure does not block the pipeline.");
        }
    }

    Ok(())
}
