use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vmctl_domain::{DesiredState, Resource};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    pub backend: String,
    pub generated_at: String,
    pub resources: Vec<LockedResource>,
    #[serde(default)]
    pub artifacts: Vec<LockedArtifact>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedArtifact {
    pub path: String,
    pub digest: String,
}

impl Lockfile {
    pub fn from_desired(desired: &DesiredState) -> Result<Self> {
        Self::from_desired_with_artifacts(desired, Path::new("."), &[])
    }

    pub fn from_desired_with_artifacts(
        desired: &DesiredState,
        artifact_root: &Path,
        artifact_paths: &[PathBuf],
    ) -> Result<Self> {
        let resources = desired
            .resources
            .iter()
            .map(locked_resource)
            .collect::<Result<Vec<_>>>()?;
        let artifacts = artifact_paths
            .iter()
            .filter(|path| path.is_file())
            .map(|path| locked_artifact(artifact_root, path))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            version: 1,
            backend: desired.backend.kind.clone(),
            generated_at: generated_at(),
            resources,
            artifacts,
        })
    }

    pub fn write_to_path(&self, path: &Path) -> Result<()> {
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn read_from_path(path: &Path) -> Result<Self> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn read_optional_from_path(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        Self::read_from_path(path).map(Some)
    }
}

fn locked_resource(resource: &Resource) -> Result<LockedResource> {
    let serialized = serde_json::to_vec(&redacted_value(serde_json::to_value(resource)?))?;
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

fn locked_artifact(root: &Path, path: &Path) -> Result<LockedArtifact> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(bytes);
    let artifact_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    Ok(LockedArtifact {
        path: artifact_path,
        digest: format!("sha256:{digest:x}"),
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
    "unix:0".to_string()
}

fn redacted_value(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_secret_key(&key) {
                        None
                    } else {
                        Some((key, redacted_value(value)))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redacted_value).collect()),
        other => other,
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("secret") || key.contains("token") || key.contains("auth_key")
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
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
        };

        let lockfile = Lockfile::from_desired(&desired).unwrap();

        assert_eq!(
            lockfile.resources[0].backend_address,
            "module.media_stack.proxmox_virtual_environment_vm.this"
        );
        assert!(lockfile.resources[0].digest.starts_with("sha256:"));
    }

    #[test]
    fn resource_digest_ignores_secret_values() {
        let mut first = Resource {
            name: "media-stack".to_string(),
            kind: "vm".to_string(),
            role: None,
            vmid: Some(210),
            depends_on: Vec::new(),
            features: BTreeMap::from([(
                "tailscale".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "auth_key".to_string(),
                    toml::Value::String("secret-one".to_string()),
                )])),
            )]),
            settings: BTreeMap::new(),
        };
        let mut second = first.clone();
        second.features = BTreeMap::from([(
            "tailscale".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "auth_key".to_string(),
                toml::Value::String("secret-two".to_string()),
            )])),
        )]);

        assert_eq!(
            locked_resource(&first).unwrap().digest,
            locked_resource(&second).unwrap().digest
        );

        first.vmid = Some(211);
        assert_ne!(
            locked_resource(&first).unwrap().digest,
            locked_resource(&second).unwrap().digest
        );
    }

    #[test]
    fn missing_lockfile_is_optional() {
        let path = std::env::temp_dir().join(format!(
            "vmctl-lockfile-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        assert!(Lockfile::read_optional_from_path(&path).unwrap().is_none());
    }

    #[test]
    fn generated_at_is_deterministic() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            resources: vec![],
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
        };

        assert_eq!(
            Lockfile::from_desired(&desired).unwrap().generated_at,
            "unix:0"
        );
    }
}
