use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use vmctl_domain::DesiredState;
use vmctl_lockfile::Lockfile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSummary {
    pub desired_only: Vec<String>,
    pub lockfile_only: Vec<String>,
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerraformStateReconciliation {
    pub matched: Vec<TerraformStateMatch>,
    pub state_only: Vec<String>,
    pub lockfile_only: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerraformStateMatch {
    pub address: String,
    pub name: String,
    pub kind: String,
    pub vmid: Option<u32>,
}

pub fn summarize_lockfile(path: &Path) -> Result<String> {
    let lockfile = Lockfile::read_from_path(path)?;
    let mut output = format!(
        "lockfile: backend={}, resources={}\n",
        lockfile.backend,
        lockfile.resources.len()
    );
    for resource in lockfile.resources {
        output.push_str(&format!(
            "- {} {} vmid={:?} exists={}\n",
            resource.kind, resource.name, resource.vmid, resource.exists
        ));
    }
    Ok(output)
}

pub fn summarize_terraform_state(path: &Path) -> Result<String> {
    summarize_terraform_state_with_lockfile(path, None)
}

pub fn summarize_terraform_state_with_lockfile(
    path: &Path,
    lockfile: Option<&Lockfile>,
) -> Result<String> {
    let state_resources = read_terraform_state_resources(path)?;
    let mut output = format!("terraform state: resources={}\n", state_resources.len());
    if let Some(lockfile) = lockfile {
        let reconciliation = reconcile_terraform_state_resources(&state_resources, lockfile);
        for matched in &reconciliation.matched {
            output.push_str(&format!(
                "- {address} -> {} {} vmid={:?}\n",
                matched.kind,
                matched.name,
                matched.vmid,
                address = matched.address,
            ));
        }
        for address in &reconciliation.state_only {
            output.push_str(&format!("- {address} -> unmapped\n"));
        }
        output.push_str("terraform state reconciliation\n");
        output.push_str(&format!(
            "- matched: {}\n",
            render_names(
                &reconciliation
                    .matched
                    .iter()
                    .map(|matched| matched.name.clone())
                    .collect::<Vec<_>>()
            )
        ));
        output.push_str(&format!(
            "- state only: {}\n",
            render_names(&reconciliation.state_only)
        ));
        output.push_str(&format!(
            "- lockfile only: {}\n",
            render_names(&reconciliation.lockfile_only)
        ));
    } else {
        for resource in state_resources {
            output.push_str(&format!("- {}\n", resource.backend_address()));
        }
    }
    Ok(output)
}

pub fn reconcile_terraform_state(
    state_path: &Path,
    lockfile: &Lockfile,
) -> Result<TerraformStateReconciliation> {
    let state_resources = read_terraform_state_resources(state_path)?;
    Ok(reconcile_terraform_state_resources(
        &state_resources,
        lockfile,
    ))
}

#[derive(Debug, Deserialize)]
struct TerraformStateResource {
    module: Option<String>,
    #[serde(rename = "type")]
    resource_type: Option<String>,
    name: Option<String>,
}

impl TerraformStateResource {
    fn backend_address(&self) -> String {
        let resource_type = self.resource_type.as_deref().unwrap_or("unknown");
        let name = self.name.as_deref().unwrap_or("unknown");
        match self.module.as_deref() {
            Some(module) => format!("{module}.{resource_type}.{name}"),
            None => format!("root.{resource_type}.{name}"),
        }
    }
}

fn resources_by_backend_address(
    lockfile: &Lockfile,
) -> BTreeMap<String, &vmctl_lockfile::LockedResource> {
    lockfile
        .resources
        .iter()
        .map(|resource| (resource.backend_address.clone(), resource))
        .collect()
}

fn read_terraform_state_resources(path: &Path) -> Result<Vec<TerraformStateResource>> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    state
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

fn reconcile_terraform_state_resources(
    state_resources: &[TerraformStateResource],
    lockfile: &Lockfile,
) -> TerraformStateReconciliation {
    let locked_by_address = resources_by_backend_address(lockfile);
    let state_addresses = state_resources
        .iter()
        .map(TerraformStateResource::backend_address)
        .collect::<BTreeSet<_>>();
    let mut matched = Vec::new();
    let mut state_only = Vec::new();

    for address in &state_addresses {
        if let Some(locked) = locked_by_address.get(address) {
            matched.push(TerraformStateMatch {
                address: address.clone(),
                name: locked.name.clone(),
                kind: locked.kind.clone(),
                vmid: locked.vmid,
            });
        } else {
            state_only.push(address.clone());
        }
    }

    let lockfile_only = lockfile
        .resources
        .iter()
        .filter(|resource| !state_addresses.contains(&resource.backend_address))
        .map(|resource| resource.name.clone())
        .collect();

    TerraformStateReconciliation {
        matched,
        state_only,
        lockfile_only,
    }
}

pub fn compare_desired_to_lockfile(desired: &DesiredState, lockfile: &Lockfile) -> SyncSummary {
    let desired_names = desired
        .resources
        .iter()
        .map(|resource| resource.name.clone())
        .collect::<BTreeSet<_>>();
    let lockfile_names = lockfile
        .resources
        .iter()
        .map(|resource| resource.name.clone())
        .collect::<BTreeSet<_>>();
    let changed = lockfile
        .resources
        .iter()
        .filter(|locked| {
            desired
                .resources
                .iter()
                .find(|resource| resource.name == locked.name)
                .and_then(|resource| serde_json::to_vec(resource).ok())
                .map(|bytes| digest_bytes(&bytes) != locked.digest)
                .unwrap_or(false)
        })
        .map(|resource| resource.name.clone())
        .collect();

    SyncSummary {
        desired_only: desired_names.difference(&lockfile_names).cloned().collect(),
        lockfile_only: lockfile_names.difference(&desired_names).cloned().collect(),
        changed,
    }
}

pub fn render_sync_summary(summary: &SyncSummary) -> String {
    format!(
        "sync summary\n- desired only: {}\n- lockfile only: {}\n- changed: {}\n",
        render_names(&summary.desired_only),
        render_names(&summary.lockfile_only),
        render_names(&summary.changed)
    )
}

fn render_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use vmctl_domain::{BackendConfig, Resource};
    use vmctl_lockfile::{LockedResource, Lockfile};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn compares_desired_resources_to_lockfile() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                enabled: true,
                image: None,
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };
        let lockfile = Lockfile {
            version: 1,
            backend: "terraform".to_string(),
            generated_at: "test".to_string(),
            artifacts: Vec::new(),
            resources: vec![LockedResource {
                name: "old".to_string(),
                kind: "vm".to_string(),
                vmid: Some(100),
                backend_address: "module.old.x".to_string(),
                digest: "sha256:old".to_string(),
                exists: true,
            }],
        };

        let summary = compare_desired_to_lockfile(&desired, &lockfile);

        assert_eq!(summary.desired_only, vec!["media-stack".to_string()]);
        assert_eq!(summary.lockfile_only, vec!["old".to_string()]);
    }

    #[test]
    fn maps_terraform_state_resources_to_lockfile_records() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        let state_path = root.join("terraform.tfstate");
        std::fs::write(
            &state_path,
            r#"{
              "resources": [
                {
                  "module": "module.media_stack",
                  "mode": "managed",
                  "type": "proxmox_virtual_environment_vm",
                  "name": "this"
                },
                {
                  "module": "module.unmanaged",
                  "mode": "managed",
                  "type": "proxmox_virtual_environment_container",
                  "name": "this"
                }
              ]
            }"#,
        )
        .unwrap();
        let lockfile = Lockfile {
            version: 1,
            backend: "terraform".to_string(),
            generated_at: "test".to_string(),
            artifacts: Vec::new(),
            resources: vec![LockedResource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                vmid: Some(210),
                backend_address: "module.media_stack.proxmox_virtual_environment_vm.this"
                    .to_string(),
                digest: "sha256:media".to_string(),
                exists: true,
            }],
        };

        let summary =
            summarize_terraform_state_with_lockfile(&state_path, Some(&lockfile)).unwrap();

        assert!(summary.contains(
            "module.media_stack.proxmox_virtual_environment_vm.this -> vm media-stack vmid=Some(210)"
        ));
        assert!(summary
            .contains("module.unmanaged.proxmox_virtual_environment_container.this -> unmapped"));
        assert!(summary.contains("terraform state reconciliation"));
        assert!(summary.contains("- matched: media-stack"));
        assert!(summary
            .contains("- state only: module.unmanaged.proxmox_virtual_environment_container.this"));
        assert!(summary.contains("- lockfile only: none"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconciles_lockfile_only_resources() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        let state_path = root.join("terraform.tfstate");
        std::fs::write(&state_path, r#"{"resources":[]}"#).unwrap();
        let lockfile = Lockfile {
            version: 1,
            backend: "terraform".to_string(),
            generated_at: "test".to_string(),
            artifacts: Vec::new(),
            resources: vec![LockedResource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                vmid: Some(210),
                backend_address: "module.media_stack.proxmox_virtual_environment_vm.this"
                    .to_string(),
                digest: "sha256:media".to_string(),
                exists: true,
            }],
        };

        let reconciliation = reconcile_terraform_state(&state_path, &lockfile).unwrap();

        assert!(reconciliation.matched.is_empty());
        assert!(reconciliation.state_only.is_empty());
        assert_eq!(
            reconciliation.lockfile_only,
            vec!["media-stack".to_string()]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-import-test-{}-{}-{}",
            std::process::id(),
            TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }
}
