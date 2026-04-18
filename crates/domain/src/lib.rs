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
    pub ssh_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvisionConfig {
    pub host: Option<String>,
    pub user: Option<String>,
    pub private_key: Option<String>,
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
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub normalized_resources: BTreeMap<String, NormalizedResource>,
    pub expansions: BTreeMap<String, Expansion>,
}

fn default_backend_kind() -> String {
    "terraform".to_string()
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            kind: default_backend_kind(),
            settings: BTreeMap::new(),
        }
    }
}
