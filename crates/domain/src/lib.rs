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
