use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use toml::Value;
use vmctl_domain::{Expansion, Resource};

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

#[derive(Debug, Clone, Default)]
pub struct PackRegistry {
    root: PathBuf,
    roles: BTreeMap<String, RolePack>,
    services: BTreeMap<String, ServicePack>,
    templates: Vec<String>,
    scripts: Vec<String>,
}

impl PackRegistry {
    pub fn load(root: &Path) -> Result<Self> {
        let roles = load_named_toml_dir::<RolePack>(&root.join("roles"), |role| &role.name)?;
        let services =
            load_named_toml_dir::<ServicePack>(&root.join("services"), |service| &service.name)?;
        let templates = load_files(&root.join("templates"))?;
        let scripts = load_files(&root.join("scripts"))?;

        Ok(Self {
            root: root.to_path_buf(),
            roles,
            services,
            templates,
            scripts,
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

        for script in &role.scripts.bootstrap {
            if !self.scripts.iter().any(|item| item == script) {
                bail!(
                    "role pack `{}` references missing script `{script}`",
                    role.name
                );
            }
        }

        for service_name in service_names_for_resource(role, resource) {
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

    pub fn render_artifacts(
        &self,
        generated_root: &Path,
        resources: &[Resource],
        expansions: &BTreeMap<String, Expansion>,
    ) -> Result<Vec<PathBuf>> {
        let mut written = Vec::new();
        for resource in resources {
            let Some(expansion) = expansions.get(&resource.name) else {
                continue;
            };

            let resource_dir = generated_root.join("resources").join(&resource.name);
            std::fs::create_dir_all(&resource_dir)
                .with_context(|| format!("failed to create {}", resource_dir.display()))?;

            for template in &expansion.files {
                let source = self.root.join("templates").join(template);
                let rendered = render_template(
                    &source,
                    &render_context(resource, expansion, &self.services)?,
                )?;
                let output_name = template.strip_suffix(".hbs").unwrap_or(template);
                let output_path = resource_dir.join(output_name);
                std::fs::write(&output_path, rendered)
                    .with_context(|| format!("failed to write {}", output_path.display()))?;
                written.push(output_path);
            }

            for script in &expansion.bootstrap_steps {
                let source = self.root.join("scripts").join(script);
                let output_dir = resource_dir.join("scripts");
                std::fs::create_dir_all(&output_dir)
                    .with_context(|| format!("failed to create {}", output_dir.display()))?;
                let output_path = output_dir.join(script);
                std::fs::copy(&source, &output_path).with_context(|| {
                    format!(
                        "failed to copy bootstrap script {} to {}",
                        source.display(),
                        output_path.display()
                    )
                })?;
                written.push(output_path);
            }
        }

        Ok(written)
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

fn load_files(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            files.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    files.sort();
    Ok(files)
}

fn render_template(source: &Path, context: &serde_json::Value) -> Result<String> {
    let template = std::fs::read_to_string(source)
        .with_context(|| format!("failed to read template {}", source.display()))?;
    let handlebars = Handlebars::new();
    handlebars
        .render_template(&template, context)
        .with_context(|| format!("failed to render template {}", source.display()))
}

fn render_context(
    resource: &Resource,
    expansion: &Expansion,
    services: &BTreeMap<String, ServicePack>,
) -> Result<serde_json::Value> {
    let service_packs = expansion
        .service_defs
        .iter()
        .map(|name| {
            services
                .get(name)
                .with_context(|| format!("missing service pack `{name}`"))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(serde_json::json!({
        "resource": resource,
        "features": resource.features,
        "expansion": expansion,
        "services": expansion.service_defs,
        "service_packs": service_packs,
        "auth_key": tailscale_auth_key(resource),
        "tailscale": tailscale_context(resource),
    }))
}

fn tailscale_auth_key(resource: &Resource) -> Option<String> {
    resource
        .features
        .get("tailscale")
        .and_then(Value::as_table)
        .and_then(|tailscale| tailscale.get("auth_key"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn tailscale_context(resource: &Resource) -> serde_json::Value {
    let tailscale = resource.features.get("tailscale").and_then(Value::as_table);
    let hostname = match resource.settings.get("hostname") {
        Some(Value::String(hostname)) => Some(hostname.clone()),
        Some(Value::Boolean(true)) => Some(resource.name.clone()),
        Some(Value::Boolean(false)) => None,
        _ if resource
            .settings
            .get("hostnames")
            .and_then(Value::as_bool)
            .unwrap_or(false) =>
        {
            Some(resource.name.clone())
        }
        _ => Some(resource.name.clone()),
    };

    serde_json::json!({
        "auth_key": tailscale_auth_key(resource),
        "enabled": tailscale
            .and_then(|tailscale| tailscale.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "hostname": hostname,
        "advertise_routes": tailscale
            .and_then(|tailscale| tailscale.get("advertise_routes"))
            .and_then(Value::as_array)
            .map(|routes| routes.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")),
        "advertise_exit_node": tailscale
            .and_then(|tailscale| tailscale.get("exit_node"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "accept_routes": tailscale
            .and_then(|tailscale| tailscale.get("accept_routes"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "tags": tailscale
            .and_then(|tailscale| tailscale.get("tags"))
            .and_then(Value::as_array)
            .map(|tags| tags.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")),
    })
}

fn service_names_for_resource(role: &RolePack, resource: &Resource) -> Vec<String> {
    let resource_services = resource
        .features
        .values()
        .filter_map(Value::as_table)
        .filter_map(service_names_from_feature)
        .next();

    resource_services.unwrap_or_else(|| service_names_from_role(role))
}

fn service_names_from_role(role: &RolePack) -> Vec<String> {
    role.features
        .values()
        .filter_map(Value::as_table)
        .filter_map(service_names_from_feature)
        .next()
        .unwrap_or_default()
}

fn service_names_from_feature(feature: &toml::map::Map<String, Value>) -> Option<Vec<String>> {
    feature
        .get("services")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .filter(|items: &Vec<String>| !items.is_empty())
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

    #[test]
    fn resource_service_names_override_role_defaults() {
        let role: RolePack = toml::from_str(
            r#"
            name = "media_stack"
            kind = "vm"

            [features.media_services]
            enabled = true
            services = ["jellyfin", "sonarr", "radarr"]
            "#,
        )
        .unwrap();
        let mut resource = Resource {
            name: "media-stack".to_string(),
            kind: "vm".to_string(),
            image: None,
            role: Some("media_stack".to_string()),
            vmid: None,
            depends_on: Vec::new(),
            features: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        resource.features.insert(
            "media_services".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "services".to_string(),
                toml::Value::Array(vec![
                    toml::Value::String("jellyfin".to_string()),
                    toml::Value::String("prowlarr".to_string()),
                ]),
            )])),
        );

        assert_eq!(
            service_names_for_resource(&role, &resource),
            vec!["jellyfin".to_string(), "prowlarr".to_string()]
        );
    }

    #[test]
    fn tailscale_hostname_defaults_to_resource_name() {
        let mut resource = Resource {
            name: "tailscale-gateway".to_string(),
            kind: "lxc".to_string(),
            image: None,
            role: Some("tailscale_gateway".to_string()),
            vmid: None,
            depends_on: Vec::new(),
            features: BTreeMap::from([(
                "tailscale".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "enabled".to_string(),
                    toml::Value::Boolean(true),
                )])),
            )]),
            settings: BTreeMap::new(),
        };

        assert_eq!(
            tailscale_context(&resource).get("hostname").unwrap(),
            "tailscale-gateway"
        );

        resource
            .settings
            .insert("hostname".to_string(), toml::Value::Boolean(false));
        assert!(tailscale_context(&resource)
            .get("hostname")
            .unwrap()
            .is_null());
    }

    #[test]
    fn renders_templates_and_copies_scripts() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("roles")).unwrap();
        std::fs::create_dir_all(root.join("services")).unwrap();
        std::fs::create_dir_all(root.join("templates")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();

        std::fs::write(
            root.join("roles/example.toml"),
            r#"
            name = "example"
            kind = "vm"

            [features.bundle]
            services = ["demo"]

            [render]
            templates = ["example.txt.hbs"]

            [scripts]
            bootstrap = ["bootstrap.sh"]
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("services/demo.toml"),
            r#"
            name = "demo"
            container_type = "docker"
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("templates/example.txt.hbs"),
            "{{resource.name}}:{{lookup services 0}}",
        )
        .unwrap();
        std::fs::write(root.join("scripts/bootstrap.sh"), "#!/usr/bin/env bash\n").unwrap();

        let registry = PackRegistry::load(&root).unwrap();
        let resource = Resource {
            name: "guest".to_string(),
            kind: "vm".to_string(),
            image: None,
            role: Some("example".to_string()),
            vmid: None,
            depends_on: Vec::new(),
            features: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        let expansion = registry.expand_resource(&resource).unwrap();
        let expansions = BTreeMap::from([("guest".to_string(), expansion)]);
        let output = root.join("generated");

        registry
            .render_artifacts(&output, &[resource], &expansions)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(output.join("resources/guest/example.txt")).unwrap(),
            "guest:demo"
        );
        assert!(output
            .join("resources/guest/scripts/bootstrap.sh")
            .is_file());

        std::fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }
}
