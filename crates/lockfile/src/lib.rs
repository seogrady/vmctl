use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vmctl_domain::{DesiredState, Resource};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    pub backend: String,
    pub generated_at: String,
    pub resources: Vec<LockedResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedResource {
    pub name: String,
    pub kind: String,
    pub vmid: Option<u32>,
    pub backend_address: String,
    pub digest: String,
    pub exists: bool,
}

impl Lockfile {
    pub fn from_desired(desired: &DesiredState) -> Result<Self> {
        let resources = desired
            .resources
            .iter()
            .map(locked_resource)
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            version: 1,
            backend: desired.backend.kind.clone(),
            generated_at: generated_at(),
            resources,
        })
    }

    pub fn write_to_path(&self, path: &Path) -> Result<()> {
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

fn locked_resource(resource: &Resource) -> Result<LockedResource> {
    let serialized = serde_json::to_vec(resource)?;
    let digest = Sha256::digest(serialized);
    Ok(LockedResource {
        name: resource.name.clone(),
        kind: resource.kind.clone(),
        vmid: resource.vmid,
        backend_address: backend_address(resource),
        digest: format!("sha256:{digest:x}"),
        exists: true,
    })
}

fn backend_address(resource: &Resource) -> String {
    let module_name = resource.name.replace('-', "_");
    match resource.kind.as_str() {
        "vm" => format!("module.{module_name}.proxmox_virtual_environment_vm.this"),
        "lxc" => {
            format!("module.{module_name}.proxmox_virtual_environment_container.this")
        }
        other => format!("module.{module_name}.vmctl_{other}.this"),
    }
}

fn generated_at() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!("unix:{seconds}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use vmctl_domain::{BackendConfig, DesiredState};

    #[test]
    fn creates_stable_resource_digest_and_backend_address() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            expansions: BTreeMap::new(),
        };

        let lockfile = Lockfile::from_desired(&desired).unwrap();

        assert_eq!(
            lockfile.resources[0].backend_address,
            "module.media_stack.proxmox_virtual_environment_vm.this"
        );
        assert!(lockfile.resources[0].digest.starts_with("sha256:"));
    }
}
