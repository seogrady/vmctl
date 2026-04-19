use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use toml::Value;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub generated_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(default = "default_backend_kind")]
    pub kind: String,
    #[serde(flatten)]
    pub settings: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub vmid: Option<u32>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub features: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub settings: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NormalizedResource {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub role: Option<String>,
    pub vmid: Option<u32>,
    pub depends_on: Vec<String>,
    pub node: Option<String>,
    pub bridge: Option<String>,
    pub storage: Option<String>,
    pub template: Option<String>,
    pub template_storage: Option<String>,
    pub clone_vmid: Option<u32>,
    pub cores: Option<u32>,
    pub memory: Option<u32>,
    pub disk_gb: Option<u32>,
    pub rootfs_gb: Option<u32>,
    pub start_on_boot: Option<bool>,
    pub agent: Option<bool>,
    pub nameserver: Option<String>,
    pub searchdomain: Option<String>,
    pub hostname: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub os_type: Option<String>,
    pub network: Option<NetworkConfig>,
    pub cloud_init: Option<CloudInitConfig>,
    pub provision: Option<ProvisionConfig>,
    pub features: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub mode: Option<String>,
    pub mac: Option<String>,
    pub address: Option<String>,
    pub gateway: Option<String>,
    pub vlan_id: Option<u32>,
    pub mtu: Option<u32>,
    pub firewall: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudInitConfig {
    pub user: Option<String>,
    pub ssh_key_file: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvisionConfig {
    pub host: Option<String>,
    pub user: Option<String>,
    pub private_key_file: Option<String>,
    pub retries: Option<u32>,
    pub retry_delay_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expansion {
    pub files: Vec<String>,
    pub service_defs: Vec<String>,
    pub bootstrap_steps: Vec<String>,
    pub dependencies: Vec<String>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredState {
    pub backend: BackendConfig,
    #[serde(default)]
    pub images: BTreeMap<String, ResolvedImage>,
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub normalized_resources: BTreeMap<String, NormalizedResource>,
    pub expansions: BTreeMap<String, Expansion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageKind {
    Vm,
    Lxc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageSource {
    Pveam,
    Url,
    Existing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    pub kind: ImageKind,
    pub source: ImageSource,
    #[serde(default)]
    pub node: Option<String>,
    pub storage: String,
    pub content_type: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub vmid: Option<u32>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub checksum_algorithm: Option<String>,
    #[serde(default)]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedImage {
    pub name: String,
    pub kind: ImageKind,
    pub source: ImageSource,
    pub node: String,
    pub storage: String,
    pub content_type: String,
    pub file_name: String,
    pub volume_id: String,
    #[serde(default)]
    pub vmid: Option<u32>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub checksum_algorithm: Option<String>,
    #[serde(default)]
    pub checksum: Option<String>,
}

fn default_backend_kind() -> String {
    "tofu".to_string()
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            kind: default_backend_kind(),
            settings: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_defaults_to_tofu() {
        assert_eq!(BackendConfig::default().kind, "tofu");
    }
}
