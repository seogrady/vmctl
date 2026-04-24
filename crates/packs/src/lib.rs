use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use handlebars::handlebars_helper;
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
    pub environment: BTreeMap<String, Value>,
    #[serde(default)]
    pub ports: BTreeMap<String, Value>,
    #[serde(default)]
    pub volumes: BTreeMap<String, Value>,
    #[serde(default)]
    pub ui: ServiceUiConfig,
    #[serde(flatten)]
    pub settings: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceUiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
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
        let roles =
            load_named_toml_dir::<RolePack>(&root.join("roles"), |role| &role.name, None, None)?;
        let services = load_named_toml_dir::<ServicePack>(
            &root.join("services"),
            |service| &service.name,
            None,
            None,
        )?;
        Self::from_loaded(root, roles, services)
    }

    pub fn load_with_config(
        root: &Path,
        config_context: &Value,
        process_env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let roles = load_named_toml_dir::<RolePack>(
            &root.join("roles"),
            |role| &role.name,
            Some(config_context),
            Some(process_env),
        )?;
        let services = load_named_toml_dir::<ServicePack>(
            &root.join("services"),
            |service| &service.name,
            Some(config_context),
            Some(process_env),
        )?;
        Self::from_loaded(root, roles, services)
    }

    fn from_loaded(
        root: &Path,
        roles: BTreeMap<String, RolePack>,
        services: BTreeMap<String, ServicePack>,
    ) -> Result<Self> {
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

fn load_named_toml_dir<T>(
    dir: &Path,
    name: impl Fn(&T) -> &str,
    config_context: Option<&Value>,
    process_env: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<String, T>>
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
        let value = raw
            .parse::<Value>()
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let value = match (config_context, process_env) {
            (Some(config_context), Some(process_env)) => {
                vmctl_config::resolve_toml_value_with_context_passthrough(
                    value,
                    config_context,
                    process_env,
                )
                .with_context(|| format!("failed to interpolate {}", path.display()))?
            }
            _ => value,
        };
        let item: T = value
            .try_into()
            .with_context(|| format!("failed to deserialize {}", path.display()))?;
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
    let mut handlebars = Handlebars::new();
    handlebars_helper!(eq: |a: Json, b: Json| a == b);
    handlebars.register_helper("eq", Box::new(eq));
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

    let ui_services = service_packs
        .iter()
        .filter_map(|service| ui_service_context(service))
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "resource": resource,
        "features": resource.features,
        "vpn": media_vpn_context(resource),
        "expansion": expansion,
        "services": expansion.service_defs,
        "service_packs": service_packs,
        "ui_services": ui_services,
        "auth_key": tailscale_auth_key(resource),
        "tailscale": tailscale_context(resource),
    }))
}

fn media_vpn_context(resource: &Resource) -> serde_json::Value {
    let vpn = resource.features.get("vpn").and_then(Value::as_table);
    let configured = vpn
        .and_then(|table| table.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let provider = vpn
        .and_then(|table| table.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let vpn_type = vpn
        .and_then(|table| table.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let wireguard_private_key = vpn
        .and_then(|table| table.get("wireguard_private_key"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let wireguard_addresses = vpn
        .and_then(|table| table.get("wireguard_addresses"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let enabled = if !configured {
        false
    } else if vpn_type.eq_ignore_ascii_case("wireguard") {
        !provider.is_empty() && !wireguard_private_key.is_empty() && !wireguard_addresses.is_empty()
    } else {
        true
    };
    serde_json::json!({
        "enabled": enabled,
        "configured": configured,
    })
}

fn ui_service_context(service: &ServicePack) -> Option<serde_json::Value> {
    if !service.ui.enabled {
        return None;
    }
    let port = service
        .ui
        .port
        .or_else(|| first_published_host_port(&service.ports))?;
    let path = service.ui.path.clone().unwrap_or_else(|| "/".to_string());
    let name = service
        .ui
        .name
        .clone()
        .unwrap_or_else(|| service.name.clone());
    Some(serde_json::json!({
        "service": service.name,
        "name": name,
        "port": port,
        "path": path,
        "description": service.ui.description.clone().unwrap_or_default(),
    }))
}

fn first_published_host_port(ports: &BTreeMap<String, Value>) -> Option<u16> {
    let published = ports.get("published")?.as_array()?;
    for entry in published {
        let raw = entry.as_str()?.trim();
        if raw.ends_with("/udp") {
            continue;
        }
        let normalized = raw.strip_suffix("/tcp").unwrap_or(raw);
        let head = normalized.split(':').next()?;
        if let Ok(port) = head.parse::<u16>() {
            return Some(port);
        }
    }
    None
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

    serde_json::json!({
        "auth_key": tailscale_auth_key(resource),
        "enabled": tailscale
            .and_then(|tailscale| tailscale.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "hostname": resource.name.clone(),
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
        let resource = Resource {
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
    }

    #[test]
    fn tailscale_hostname_ignores_legacy_override_setting() {
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
        resource
            .settings
            .insert("hostname".to_string(), toml::Value::String("gateway".to_string()));
        assert_eq!(
            tailscale_context(&resource).get("hostname").unwrap(),
            "tailscale-gateway"
        );
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

    #[test]
    fn interpolates_pack_toml_from_vmctl_config_context() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("roles")).unwrap();
        std::fs::create_dir_all(root.join("services")).unwrap();
        std::fs::create_dir_all(root.join("templates")).unwrap();

        std::fs::write(
            root.join("roles/example.toml"),
            r#"
            name = "example"
            kind = "vm"

            [features.bundle]
            services = ["${const.service_name}"]

            [render]
            templates = ["example.env.hbs"]
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("services/demo.toml"),
            r#"
            name = "${const.service_name}"
            container_type = "docker"

            [ui]
            enabled = true
            port = 8123
            name = "${env.DEMO_UI_NAME}"
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("templates/example.env.hbs"),
            "{{#each ui_services}}{{name}}={{port}}{{/each}}",
        )
        .unwrap();

        let config_context = r#"
            [const]
            service_name = "demo"

            [env]
            DEMO_UI_NAME = "Demo UI"
        "#
        .parse::<Value>()
        .unwrap();
        let process_env = BTreeMap::new();
        let registry =
            PackRegistry::load_with_config(&root, &config_context, &process_env).unwrap();
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

        assert_eq!(expansion.service_defs, vec!["demo".to_string()]);

        let expansions = BTreeMap::from([("guest".to_string(), expansion)]);
        let output = root.join("generated");
        registry
            .render_artifacts(&output, &[resource], &expansions)
            .unwrap();

        let rendered = std::fs::read_to_string(output.join("resources/guest/example.env")).unwrap();
        assert_eq!(rendered, "Demo UI=8123");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renders_templates_with_eq_helper() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("roles")).unwrap();
        std::fs::create_dir_all(root.join("services")).unwrap();
        std::fs::create_dir_all(root.join("templates")).unwrap();

        std::fs::write(
            root.join("roles/example.toml"),
            r#"
            name = "example"
            kind = "vm"

            [render]
            templates = ["routes.txt.hbs"]
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("templates/routes.txt.hbs"),
            r#"{{#each ui_services}}{{#if (eq this.name "Jellyfin")}}{{this.port}}{{/if}}{{/each}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("services/jellyfin.toml"),
            r#"
            name = "jellyfin"
            container_type = "docker"
            [ui]
            enabled = true
            port = 8096
            name = "Jellyfin"
            "#,
        )
        .unwrap();

        let registry = PackRegistry::load(&root).unwrap();
        let resource = Resource {
            name: "guest".to_string(),
            kind: "vm".to_string(),
            image: None,
            role: Some("example".to_string()),
            vmid: None,
            depends_on: Vec::new(),
            features: BTreeMap::from([(
                "media_services".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "services".to_string(),
                    toml::Value::Array(vec![toml::Value::String("jellyfin".to_string())]),
                )])),
            )]),
            settings: BTreeMap::new(),
        };
        let expansion = registry.expand_resource(&resource).unwrap();
        let expansions = BTreeMap::from([("guest".to_string(), expansion)]);
        let output = root.join("generated");
        registry
            .render_artifacts(&output, &[resource], &expansions)
            .unwrap();

        let rendered = std::fs::read_to_string(output.join("resources/guest/routes.txt")).unwrap();
        assert_eq!(rendered.trim(), "8096");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renders_media_index_from_ui_enabled_services() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("roles")).unwrap();
        std::fs::create_dir_all(root.join("services")).unwrap();
        std::fs::create_dir_all(root.join("templates")).unwrap();

        std::fs::write(
            root.join("roles/media_stack.toml"),
            r#"
            name = "media_stack"
            kind = "vm"

            [features.media_services]
            enabled = true
            services = ["jellyfin", "jellyseerr", "gluetun"]

            [render]
            templates = ["media-index.html.hbs"]
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("services/jellyfin.toml"),
            r#"
            name = "jellyfin"
            container_type = "docker"
            [ui]
            enabled = true
            port = 8096
            name = "Jellyfin"
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("services/jellyseerr.toml"),
            r#"
            name = "jellyseerr"
            container_type = "docker"
            [ports]
            published = ["5055:5055"]
            [ui]
            enabled = true
            name = "Jellyseerr"
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("services/gluetun.toml"),
            r#"
            name = "gluetun"
            container_type = "docker"
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("templates/media-index.html.hbs"),
            r#"{{#each ui_services}}{{name}}={{port}} {{/each}}"#,
        )
        .unwrap();

        let registry = PackRegistry::load(&root).unwrap();
        let resource = Resource {
            name: "media-stack".to_string(),
            kind: "vm".to_string(),
            image: None,
            role: Some("media_stack".to_string()),
            vmid: None,
            depends_on: Vec::new(),
            features: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        let expansion = registry.expand_resource(&resource).unwrap();
        let expansions = BTreeMap::from([("media-stack".to_string(), expansion)]);
        let output = root.join("generated");
        registry
            .render_artifacts(&output, &[resource], &expansions)
            .unwrap();

        let rendered =
            std::fs::read_to_string(output.join("resources/media-stack/media-index.html")).unwrap();
        assert!(rendered.contains("Jellyfin=8096"));
        assert!(rendered.contains("Jellyseerr=5055"));
        assert!(!rendered.contains("gluetun"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn media_vpn_context_requires_wireguard_inputs() {
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
            "vpn".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([
                ("enabled".to_string(), toml::Value::Boolean(true)),
                (
                    "provider".to_string(),
                    toml::Value::String("mullvad".to_string()),
                ),
                (
                    "type".to_string(),
                    toml::Value::String("wireguard".to_string()),
                ),
                (
                    "wireguard_private_key".to_string(),
                    toml::Value::String("".to_string()),
                ),
                (
                    "wireguard_addresses".to_string(),
                    toml::Value::String("".to_string()),
                ),
            ])),
        );
        let context = media_vpn_context(&resource);
        assert_eq!(context.get("configured").unwrap(), true);
        assert_eq!(context.get("enabled").unwrap(), false);

        resource.features.insert(
            "vpn".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([
                ("enabled".to_string(), toml::Value::Boolean(true)),
                (
                    "provider".to_string(),
                    toml::Value::String("mullvad".to_string()),
                ),
                (
                    "type".to_string(),
                    toml::Value::String("wireguard".to_string()),
                ),
                (
                    "wireguard_private_key".to_string(),
                    toml::Value::String("key".to_string()),
                ),
                (
                    "wireguard_addresses".to_string(),
                    toml::Value::String("10.0.0.2/32".to_string()),
                ),
            ])),
        );
        let context = media_vpn_context(&resource);
        assert_eq!(context.get("enabled").unwrap(), true);
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
