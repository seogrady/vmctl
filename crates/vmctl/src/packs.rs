use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use toml::Value;

use crate::config::Resource;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolePack {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub defaults: RoleDefaults,
    #[serde(default)]
    pub features: BTreeMap<String, Value>,
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub scripts: ScriptConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleDefaults {
    #[serde(default)]
    pub requires: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RenderConfig {
    #[serde(default)]
    pub templates: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScriptConfig {
    #[serde(default)]
    pub bootstrap: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServicePack {
    pub name: String,
    #[serde(default)]
    pub container_type: String,
    #[serde(default)]
    pub image: BTreeMap<String, Value>,
    #[serde(default)]
    pub ports: BTreeMap<String, Value>,
    #[serde(default)]
    pub volumes: BTreeMap<String, Value>,
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

#[derive(Debug, Clone, Default)]
pub struct PackRegistry {
    roles: BTreeMap<String, RolePack>,
    services: BTreeMap<String, ServicePack>,
    templates: Vec<String>,
}

impl PackRegistry {
    pub fn load(root: &Path) -> Result<Self> {
        let roles = load_named_toml_dir::<RolePack>(&root.join("roles"), |role| &role.name)?;
        let services =
            load_named_toml_dir::<ServicePack>(&root.join("services"), |service| &service.name)?;
        let templates = load_templates(&root.join("templates"))?;

        Ok(Self {
            roles,
            services,
            templates,
        })
    }

    pub fn expand_resource(&self, resource: &Resource) -> Result<Expansion> {
        let Some(role_name) = &resource.role else {
            return Ok(Expansion::default());
        };

        let role = self.roles.get(role_name).with_context(|| {
            format!(
                "resource `{}` references missing role pack `{role_name}`",
                resource.name
            )
        })?;

        if role.kind != resource.kind {
            bail!(
                "resource `{}` is kind `{}` but role pack `{}` requires `{}`",
                resource.name,
                resource.kind,
                role.name,
                role.kind
            );
        }

        let mut expansion = Expansion {
            files: role.render.templates.clone(),
            bootstrap_steps: role.scripts.bootstrap.clone(),
            dependencies: role.defaults.requires.clone(),
            ..Expansion::default()
        };
        expansion
            .metadata
            .insert("role".to_string(), role.name.clone());

        for template in &role.render.templates {
            if !self.templates.iter().any(|item| item == template) {
                bail!(
                    "role pack `{}` references missing template `{template}`",
                    role.name
                );
            }
        }

        for service_name in service_names_from_role(role) {
            if !self.services.contains_key(&service_name) {
                bail!(
                    "role pack `{}` references missing service pack `{service_name}`",
                    role.name
                );
            }
            expansion.service_defs.push(service_name);
        }

        Ok(expansion)
    }
}

fn load_named_toml_dir<T>(dir: &Path, name: impl Fn(&T) -> &str) -> Result<BTreeMap<String, T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut items = BTreeMap::new();
    if !dir.exists() {
        return Ok(items);
    }

    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let item: T =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        let item_name = name(&item).to_string();
        let expected = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        if item_name != expected {
            bail!(
                "pack `{}` declares name `{}`; file name must match",
                path.display(),
                item_name
            );
        }
        if items.insert(item_name.clone(), item).is_some() {
            bail!("duplicate pack `{item_name}`");
        }
    }

    Ok(items)
}

fn load_templates(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut templates = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            templates.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    Ok(templates)
}

fn service_names_from_role(role: &RolePack) -> Vec<String> {
    role.features
        .values()
        .filter_map(Value::as_table)
        .filter_map(|feature| feature.get("services"))
        .filter_map(Value::as_array)
        .flat_map(|items| items.iter().filter_map(Value::as_str).map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_names_are_data_driven() {
        let role: RolePack = toml::from_str(
            r#"
            name = "media_stack"
            kind = "vm"

            [features.media_services]
            enabled = true
            services = ["jellyfin", "sonarr"]
            "#,
        )
        .unwrap();

        assert_eq!(
            service_names_from_role(&role),
            vec!["jellyfin".to_string(), "sonarr".to_string()]
        );
    }
}
