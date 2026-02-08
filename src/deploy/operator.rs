use anyhow::{bail, Context};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ApiResource, DeleteParams, DynamicObject, ListParams};
use kube::Client;
use serde_json::json;
use tokio::runtime::Runtime;

/// Verify that the OpenShift Pipelines operator is installed by checking for the TektonConfig CR.
pub fn verify_operator(rt: &Runtime, client: &Client) -> anyhow::Result<DynamicObject> {
    let ar = ApiResource {
        group: "operator.tekton.dev".into(),
        version: "v1alpha1".into(),
        api_version: "operator.tekton.dev/v1alpha1".into(),
        kind: "TektonConfig".into(),
        plural: "tektonconfigs".into(),
    };

    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);

    let result = rt.block_on(api.get("config"));

    match result {
        Ok(tc) => Ok(tc),
        Err(kube::Error::Api(ref resp)) if resp.code == 404 => {
            bail!("OpenShift Pipelines operator not found. TektonConfig CR 'config' does not exist.\nIs the operator installed?")
        }
        Err(kube::Error::Api(ref resp)) if resp.code == 403 => {
            bail!("Insufficient permissions to read TektonConfig CR.\nEnsure you have cluster-admin or appropriate RBAC.")
        }
        Err(e) => Err(e).context("Failed to verify operator installation"),
    }
}

/// Known namespaces where the operator controller deployment may live.
const OPERATOR_NAMESPACES: &[&str] = &[
    "openshift-pipelines",
    "openshift-operators",
    "tekton-pipelines",
];

/// Known deployment names for the operator controller.
const OPERATOR_DEPLOYMENT_NAMES: &[&str] = &[
    "openshift-pipelines-operator",
    "tekton-operator",
];

/// Known label selectors for the operator controller deployment.
const OPERATOR_LABEL_SELECTORS: &[&str] = &[
    "app.kubernetes.io/name=openshift-pipelines-operator",
    "app=tekton-operator",
];

/// Find the operator controller deployment by checking known namespaces, names, and label selectors.
/// Returns (namespace, deployment_name).
///
/// This is the primary function used for deploying upstream images: the returned deployment
/// is patched directly via `patch_operator_deployment_env()` (direct Deployment patching,
/// not CSV patching -- OLM does NOT revert direct deployment modifications per issue #1853).
pub fn find_operator_deployment(
    rt: &Runtime,
    client: &Client,
) -> anyhow::Result<(String, String)> {
    // First try known deployment names directly (most reliable for OLM-managed operators)
    for ns in OPERATOR_NAMESPACES {
        let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
        for name in OPERATOR_DEPLOYMENT_NAMES {
            if let Ok(dep) = rt.block_on(api.get(name)) {
                if let Some(n) = &dep.metadata.name {
                    return Ok((ns.to_string(), n.clone()));
                }
            }
        }
    }

    // Fall back to label selectors
    for ns in OPERATOR_NAMESPACES {
        let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
        for selector in OPERATOR_LABEL_SELECTORS {
            let lp = kube::api::ListParams::default().labels(selector);
            let list = rt
                .block_on(api.list(&lp))
                .with_context(|| format!("Failed to list deployments in namespace {ns}"))?;
            if let Some(dep) = list.items.first() {
                if let Some(name) = &dep.metadata.name {
                    return Ok((ns.to_string(), name.clone()));
                }
            }
        }
    }

    bail!(
        "Operator controller deployment not found.\nChecked namespaces: {}\nChecked names: {}\nChecked labels: {}",
        OPERATOR_NAMESPACES.join(", "),
        OPERATOR_DEPLOYMENT_NAMES.join(", "),
        OPERATOR_LABEL_SELECTORS.join(", ")
    )
}

/// Patch the operator Deployment's container env vars directly.
///
/// This bypasses CSV patching entirely. OLM does NOT revert direct deployment
/// modifications per OLM issue #1853, so patching the Deployment directly is
/// both safe and more reliable than patching the CSV (which was broken with
/// OpenShift Pipelines v1.21.0 -- CSV env var changes were not propagated to
/// the Deployment).
///
/// The function finds the container named "openshift-pipelines-operator-lifecycle"
/// in the Deployment spec, merges the IMAGE_ env vars, and replaces the Deployment.
pub fn patch_operator_deployment_env(
    rt: &Runtime,
    client: &Client,
    namespace: &str,
    deployment_name: &str,
    mappings: &[(String, String)],
) -> anyhow::Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), namespace);

    // Get the current deployment
    let mut dep = rt
        .block_on(api.get(deployment_name))
        .with_context(|| format!("Failed to get Deployment {}/{}", namespace, deployment_name))?;

    // Find the container named "openshift-pipelines-operator-lifecycle"
    let containers = dep
        .spec
        .as_mut()
        .and_then(|s| s.template.spec.as_mut())
        .map(|s| &mut s.containers)
        .context("Deployment has no containers in spec.template.spec.containers")?;

    let container_index = containers
        .iter()
        .position(|c| c.name == "openshift-pipelines-operator-lifecycle")
        .context("Container 'openshift-pipelines-operator-lifecycle' not found in Deployment")?;

    let container = &mut containers[container_index];

    // Read existing env vars from the container
    let existing_envs = container.env.take().unwrap_or_default();

    // Build merged env list: update matching keys (set value, clear valueFrom), keep the rest
    let mut new_envs: Vec<k8s_openapi::api::core::v1::EnvVar> = existing_envs
        .into_iter()
        .map(|mut env| {
            if let Some((_, new_val)) = mappings.iter().find(|(k, _)| k == &env.name) {
                env.value = Some(new_val.clone());
                env.value_from = None;
            }
            env
        })
        .collect();

    // Add any new keys not already present
    let existing_names: Vec<String> = new_envs.iter().map(|e| e.name.clone()).collect();
    for (key, value) in mappings {
        if !existing_names.iter().any(|n| n == key) {
            new_envs.push(k8s_openapi::api::core::v1::EnvVar {
                name: key.clone(),
                value: Some(value.clone()),
                value_from: None,
            });
        }
    }

    container.env = Some(new_envs);

    // Replace the deployment with updated env vars
    // OLM does NOT revert direct deployment modifications (OLM issue #1853),
    // so this change persists even though OLM manages the deployment via CSV.
    let pp = kube::api::PostParams::default();
    rt.block_on(api.replace(deployment_name, &pp, &dep))
        .with_context(|| {
            format!(
                "Failed to update Deployment {}/{} with IMAGE_ env vars",
                namespace, deployment_name
            )
        })?;

    Ok(())
}

/// Delete TektonInstallerSets matching a component to force the operator to re-reconcile.
/// The operator uses IMAGE_ env vars when creating InstallerSets, so deleting them
/// causes recreation with the new (upstream) images.
pub fn delete_installer_sets(
    rt: &Runtime,
    client: &Client,
    component: &str,
    prefix_override: Option<&str>,
) -> anyhow::Result<u32> {
    let ar = ApiResource {
        group: "operator.tekton.dev".into(),
        version: "v1alpha1".into(),
        api_version: "operator.tekton.dev/v1alpha1".into(),
        kind: "TektonInstallerSet".into(),
        plural: "tektoninstallersets".into(),
    };

    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let lp = ListParams::default();
    let sets = rt
        .block_on(api.list(&lp))
        .context("Failed to list TektonInstallerSets")?;

    // Match installer sets by component prefix (e.g. "pipeline-main-deployment-*")
    // Use prefix_override if provided (e.g. "manualapprovalgate" for manual-approval-gate)
    let prefix = prefix_override.unwrap_or(component);
    let prefixes: Vec<String> = vec![
        format!("{}-main-deployment-", prefix),
        format!("{}-main-static-", prefix),
        format!("{}-post-", prefix),
        format!("{}-pre-", prefix),
    ];

    let mut deleted = 0u32;
    for set in &sets.items {
        if let Some(name) = &set.metadata.name {
            if prefixes.iter().any(|p| name.starts_with(p)) {
                let dp = kube::api::DeleteParams::default();
                match rt.block_on(api.delete(name, &dp)) {
                    Ok(_) => {
                        eprintln!("  Deleted InstallerSet: {}", name);
                        deleted += 1;
                    }
                    Err(e) => {
                        eprintln!("  WARNING: Failed to delete InstallerSet {}: {}", name, e);
                    }
                }
            }
        }
    }

    Ok(deleted)
}

/// Ensure all authenticated users can pull images from the upstream namespace.
/// Tekton uses entrypoint/nop/workingdirinit as init containers in arbitrary user
/// namespaces, so we need cluster-wide pull access — not just specific namespaces.
pub fn ensure_image_pull_rbac(
    rt: &Runtime,
    client: &Client,
    image_namespace: &str,
) -> anyhow::Result<()> {
    use k8s_openapi::api::rbac::v1::RoleBinding;

    let api: Api<RoleBinding> = Api::namespaced(client.clone(), image_namespace);

    let binding_name = "image-puller-all-authenticated";

    // Check if it already exists
    if rt.block_on(api.get(binding_name)).is_ok() {
        return Ok(());
    }

    let rb: RoleBinding = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {
            "name": binding_name,
            "namespace": image_namespace,
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "system:image-puller",
        },
        "subjects": [{
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "Group",
            "name": "system:authenticated",
        }]
    }))?;

    rt.block_on(api.create(&kube::api::PostParams::default(), &rb))
        .with_context(|| {
            format!(
                "Failed to create image-puller RoleBinding in {}",
                image_namespace
            )
        })?;
    eprintln!("  Granted image-puller to all authenticated users in {}", image_namespace);

    Ok(())
}

/// Check if a Pod is Running and has the Ready condition set to True.
fn is_pod_ready(pod: &Pod) -> bool {
    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("");
    if phase != "Running" {
        return false;
    }

    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
        .unwrap_or(false)
}

/// Verify that the Deployment's env vars match the expected mappings.
///
/// Reads back the Deployment and checks that the `openshift-pipelines-operator-lifecycle`
/// container has the expected IMAGE_ env vars with the correct values.
/// Returns `false` if OLM reverted the patch (env vars don't match).
pub fn verify_deployment_env(
    rt: &Runtime,
    client: &Client,
    namespace: &str,
    deployment_name: &str,
    mappings: &[(String, String)],
) -> anyhow::Result<bool> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let dep = rt
        .block_on(api.get(deployment_name))
        .with_context(|| format!("Failed to re-read Deployment {}/{}", namespace, deployment_name))?;

    let containers = dep
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .map(|s| &s.containers);

    let container = containers.and_then(|cs| {
        cs.iter()
            .find(|c| c.name == "openshift-pipelines-operator-lifecycle")
    });

    let container = match container {
        Some(c) => c,
        None => {
            eprintln!("  WARNING: lifecycle container not found during verify");
            return Ok(false);
        }
    };

    let envs = container.env.as_deref().unwrap_or(&[]);

    for (key, expected_val) in mappings {
        let found = envs.iter().find(|e| &e.name == key);
        match found {
            Some(env) if env.value.as_deref() == Some(expected_val) => {}
            Some(env) => {
                eprintln!(
                    "  Env var {} mismatch: expected {}, got {:?}",
                    key,
                    expected_val,
                    env.value
                );
                return Ok(false);
            }
            None => {
                eprintln!("  Env var {} not found in Deployment", key);
                return Ok(false);
            }
        }
    }

    Ok(true)
}

/// Restart the operator pod by deleting existing pods and waiting for a new Ready pod.
///
/// After patching the Deployment's env vars, the running pod may still use cached values.
/// This function deletes matching pods so the Deployment controller creates fresh ones
/// with the updated environment.
pub fn restart_operator_pod(
    rt: &Runtime,
    client: &Client,
    namespace: &str,
    deployment_name: &str,
) -> anyhow::Result<()> {
    let dep_api: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let dep = rt
        .block_on(dep_api.get(deployment_name))
        .with_context(|| format!("Failed to get Deployment {}/{}", namespace, deployment_name))?;

    // Build label selector from Deployment's spec.selector.matchLabels
    let match_labels = dep
        .spec
        .as_ref()
        .and_then(|s| s.selector.match_labels.as_ref())
        .context("Deployment has no spec.selector.matchLabels")?;

    let label_selector = match_labels
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join(",");

    // List and delete matching pods
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let lp = ListParams::default().labels(&label_selector);
    let pods = rt
        .block_on(pod_api.list(&lp))
        .with_context(|| "Failed to list operator pods")?;

    let old_pod_names: Vec<String> = pods
        .items
        .iter()
        .filter_map(|p| p.metadata.name.clone())
        .collect();

    if old_pod_names.is_empty() {
        eprintln!("  No operator pods found to restart");
        return Ok(());
    }

    for name in &old_pod_names {
        eprintln!("  Deleting operator pod: {}", name);
        let dp = DeleteParams::default();
        if let Err(e) = rt.block_on(pod_api.delete(name, &dp)) {
            eprintln!("  WARNING: Failed to delete pod {}: {}", name, e);
        }
    }

    // Poll for a new Ready pod (5s interval, 180s timeout)
    let poll_interval = std::time::Duration::from_secs(5);
    let timeout = std::time::Duration::from_secs(180);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            bail!(
                "Timed out waiting for new operator pod to become Ready ({}s)",
                timeout.as_secs()
            );
        }

        std::thread::sleep(poll_interval);

        let pods = match rt.block_on(pod_api.list(&lp)) {
            Ok(list) => list,
            Err(e) => {
                eprintln!("  WARNING: Failed to list pods (retrying): {}", e);
                continue;
            }
        };

        // Look for a new pod (not in old_pod_names) that is Ready
        for pod in &pods.items {
            if let Some(name) = &pod.metadata.name {
                if !old_pod_names.contains(name) && is_pod_ready(pod) {
                    eprintln!("  Operator pod restarted: {} is Running + Ready", name);
                    return Ok(());
                }
            }
        }
    }
}
