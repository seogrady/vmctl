use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use handlebars::handlebars_helper;
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use toml::Value;
use vmctl_domain::{Expansion, Resource};
use vmctl_hook_schema::HookSection;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceManifest {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub defaults: RoleDefaults,
    #[serde(default)]
    pub features: BTreeMap<String, Value>,
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub hooks: HookSection,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDefinition {
    pub name: String,
    #[serde(default)]
    pub container_type: String,
    #[serde(default)]
    pub image: BTreeMap<String, Value>,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub group_add: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, Value>,
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
pub struct ResourceRegistry {
    root: PathBuf,
    resources: Vec<Resource>,
    roles: BTreeMap<String, ResourceManifest>,
    services: BTreeMap<String, ServiceDefinition>,
    resource_roots: BTreeMap<String, PathBuf>,
    role_roots: BTreeMap<String, PathBuf>,
}

impl ResourceRegistry {
    pub fn load(root: &Path, services_root: &Path) -> Result<Self> {
        let roles = load_resource_roles(root, None, None)?;
        let resources = load_resource_manifests(root, None, None)?;
        let services = load_service_definitions(services_root, None, None)?;
        let resource_roots = resources
            .iter()
            .map(|resource| (resource.name.clone(), root.join(&resource.name)))
            .collect();
        let role_roots = roles
            .iter()
            .map(|(role, manifest)| (role.clone(), root.join(&manifest.name)))
            .collect();
        Self::from_loaded(
            root,
            services_root,
            resources,
            roles,
            services,
            resource_roots,
            role_roots,
        )
    }

    pub fn load_with_config(
        root: &Path,
        services_root: &Path,
        config_context: &Value,
        process_env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let roles = load_resource_roles(root, Some(config_context), Some(process_env))?;
        let resources = load_resource_manifests(root, Some(config_context), Some(process_env))?;
        let services =
            load_service_definitions(services_root, Some(config_context), Some(process_env))?;
        let resource_roots = resources
            .iter()
            .map(|resource| (resource.name.clone(), root.join(&resource.name)))
            .collect();
        let role_roots = roles
            .iter()
            .map(|(role, manifest)| (role.clone(), root.join(&manifest.name)))
            .collect();
        Self::from_loaded(
            root,
            services_root,
            resources,
            roles,
            services,
            resource_roots,
            role_roots,
        )
    }

    pub fn load_from_module_dirs(
        resource_module_dirs: &[PathBuf],
        service_module_dirs: &[PathBuf],
        config_context: &Value,
        process_env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut resources = Vec::new();
        let mut roles = BTreeMap::new();
        let mut resource_roots = BTreeMap::new();
        let mut role_roots = BTreeMap::new();

        for module_dir in resource_module_dirs {
            let path = module_dir.join("resource.toml");
            if !path.exists() {
                continue;
            }
            let value = load_toml_value(&path, Some(config_context), Some(process_env))?;
            let mut resource: Resource = value
                .clone()
                .try_into()
                .with_context(|| format!("failed to deserialize {}", path.display()))?;
            for owned_key in ["defaults", "render", "hooks"] {
                resource.settings.remove(owned_key);
            }
            let role: ResourceManifest = value
                .clone()
                .try_into()
                .with_context(|| format!("failed to deserialize {}", path.display()))?;
            let role_name = resource
                .role
                .clone()
                .unwrap_or_else(|| resource.name.clone());
            if roles.insert(role_name.clone(), role).is_some() {
                bail!("duplicate resource role in {}", path.display());
            }
            resource_roots.insert(resource.name.clone(), module_dir.clone());
            role_roots.insert(role_name, module_dir.clone());
            resources.push(resource);
        }
        resources.sort_by(|left, right| left.name.cmp(&right.name));
        let services = load_service_definitions_from_module_dirs(
            service_module_dirs,
            Some(config_context),
            Some(process_env),
        )?;

        Self::from_loaded(
            Path::new("resources"),
            Path::new("services"),
            resources,
            roles,
            services,
            resource_roots,
            role_roots,
        )
    }

    fn from_loaded(
        root: &Path,
        _services_root: &Path,
        resources: Vec<Resource>,
        roles: BTreeMap<String, ResourceManifest>,
        services: BTreeMap<String, ServiceDefinition>,
        resource_roots: BTreeMap<String, PathBuf>,
        role_roots: BTreeMap<String, PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            resources,
            roles,
            services,
            resource_roots,
            role_roots,
        })
    }

    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_for_role(&self, role: &str) -> Option<&ResourceManifest> {
        self.roles.get(role)
    }

    fn resource_owned_path(&self, resource: &Resource, kind: &str, file: &str) -> PathBuf {
        self.resource_module_root(resource).join(kind).join(file)
    }

    fn resource_module_root(&self, resource: &Resource) -> PathBuf {
        let role_name = resource
            .role
            .as_deref()
            .unwrap_or(resource.name.as_str())
            .to_string();
        self.role_roots
            .get(&role_name)
            .cloned()
            .or_else(|| self.resource_roots.get(&resource.name).cloned())
            .unwrap_or_else(|| self.root.join(&resource.name))
    }

    fn resolve_resource_hook_source(&self, resource: &Resource, hook: &str) -> PathBuf {
        let hook_path = PathBuf::from(hook);
        if hook_path.is_absolute() {
            hook_path
        } else {
            self.resource_module_root(resource).join(hook_path)
        }
    }

    pub fn expand_resource(&self, resource: &Resource) -> Result<Expansion> {
        let Some(role_name) = &resource.role else {
            return Ok(Expansion::default());
        };

        let role = self.roles.get(role_name).with_context(|| {
            format!(
                "resource `{}` references missing role resource `{role_name}`",
                resource.name
            )
        })?;

        if role.kind != resource.kind {
            bail!(
                "resource `{}` is kind `{}` but role resource `{}` requires `{}`",
                resource.name,
                resource.kind,
                role.name,
                role.kind
            );
        }

        let resource_module_root = self.resource_module_root(resource);
        let mut bootstrap_steps = role.hooks.bootstrap.resolve(&resource_module_root)?;
        bootstrap_steps.dedup();
        let validation_steps = role.hooks.validate.resolve(&resource_module_root)?;

        let mut expansion = Expansion {
            files: role.render.templates.clone(),
            bootstrap_steps,
            validation_steps,
            dependencies: role.defaults.requires.clone(),
            ..Expansion::default()
        };
        expansion
            .metadata
            .insert("role".to_string(), role.name.clone());

        for template in &role.render.templates {
            if !self
                .resource_owned_path(resource, "templates", template)
                .exists()
            {
                bail!(
                    "role resource `{}` references missing template `{template}`",
                    role.name
                );
            }
        }

        for script in expansion
            .bootstrap_steps
            .iter()
            .chain(expansion.validation_steps.iter())
        {
            if !self.resolve_resource_hook_source(resource, script).exists() {
                bail!(
                    "role resource `{}` references missing script `{script}`",
                    role.name
                );
            }
        }

        for service_name in service_names_for_resource(role, resource) {
            if !self.services.contains_key(&service_name) {
                bail!(
                    "role resource `{}` references missing service resource `{service_name}`",
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
                let source = self.resource_owned_path(resource, "templates", template);
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

            for script in expansion
                .bootstrap_steps
                .iter()
                .chain(expansion.validation_steps.iter())
            {
                let source = self.resolve_resource_hook_source(resource, script);
                if !source.exists() {
                    continue;
                }
                let output_path = resource_dir.join(script);
                if let Some(parent) = output_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                std::fs::copy(&source, &output_path).with_context(|| {
                    format!(
                        "failed to copy bootstrap script {} to {}",
                        source.display(),
                        output_path.display()
                    )
                })?;
                written.push(output_path);
            }

            copy_directory_files(
                &self.resource_module_root(resource).join("scripts"),
                &resource_dir.join("scripts"),
                &mut written,
            )?;
        }

        Ok(written)
    }
}

fn copy_directory_files(
    source_root: &Path,
    target_root: &Path,
    written: &mut Vec<PathBuf>,
) -> Result<()> {
    if !source_root.exists() {
        return Ok(());
    }

    std::fs::create_dir_all(target_root)
        .with_context(|| format!("failed to create {}", target_root.display()))?;

    for entry in std::fs::read_dir(source_root)
        .with_context(|| format!("failed to read {}", source_root.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target_root.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_directory_files(&source_path, &target_path, written)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        std::fs::copy(&source_path, &target_path).with_context(|| {
            format!(
                "failed to copy script file {} to {}",
                source_path.display(),
                target_path.display()
            )
        })?;
        written.push(target_path);
    }

    Ok(())
}

fn load_resource_manifests(
    root: &Path,
    config_context: Option<&Value>,
    process_env: Option<&BTreeMap<String, String>>,
) -> Result<Vec<Resource>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut resources = Vec::new();
    for entry in
        std::fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('_') {
            continue;
        }
        let path = entry.path().join("resource.toml");
        if !path.exists() {
            continue;
        }
        let value = load_toml_value(&path, config_context, process_env)?;
        let mut resource: Resource = value
            .try_into()
            .with_context(|| format!("failed to deserialize {}", path.display()))?;
        for owned_key in ["defaults", "render", "hooks"] {
            resource.settings.remove(owned_key);
        }
        let expected = entry.file_name().to_string_lossy().to_string();
        if resource.name != expected {
            bail!(
                "resource `{}` declares name `{}`; directory name must match",
                path.display(),
                resource.name
            );
        }
        resources.push(resource);
    }
    resources.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(resources)
}

fn load_resource_roles(
    root: &Path,
    config_context: Option<&Value>,
    process_env: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<String, ResourceManifest>> {
    let mut roles = BTreeMap::new();
    if !root.exists() {
        return Ok(roles);
    }
    for entry in
        std::fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('_') {
            continue;
        }
        let path = entry.path().join("resource.toml");
        if !path.exists() {
            continue;
        }
        let value = load_toml_value(&path, config_context, process_env)?;
        let resource: Resource = value
            .clone()
            .try_into()
            .with_context(|| format!("failed to deserialize {}", path.display()))?;
        let role: ResourceManifest = value
            .try_into()
            .with_context(|| format!("failed to deserialize {}", path.display()))?;
        let role_name = resource
            .role
            .clone()
            .unwrap_or_else(|| resource.name.clone());
        if roles.insert(role_name.clone(), role).is_some() {
            bail!("duplicate resource role in {}", path.display());
        }
    }

    Ok(roles)
}

fn load_service_definitions(
    services_root: &Path,
    config_context: Option<&Value>,
    process_env: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<String, ServiceDefinition>> {
    let mut services = BTreeMap::new();
    if !services_root.exists() {
        return Ok(services);
    }

    for entry in std::fs::read_dir(services_root)
        .with_context(|| format!("failed to read {}", services_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('_') {
            continue;
        }
        let path = entry.path().join("service.toml");
        if !path.exists() {
            continue;
        }
        let value = service_definition_value(load_toml_value(&path, config_context, process_env)?);
        let service: ServiceDefinition = value
            .try_into()
            .with_context(|| format!("failed to deserialize {}", path.display()))?;
        let expected = entry.file_name().to_string_lossy().to_string();
        if service.name != expected {
            bail!(
                "service `{}` declares name `{}`; directory name must match",
                path.display(),
                service.name
            );
        }
        if services.insert(service.name.clone(), service).is_some() {
            bail!("duplicate service `{expected}`");
        }
    }

    Ok(services)
}

fn load_service_definitions_from_module_dirs(
    service_module_dirs: &[PathBuf],
    config_context: Option<&Value>,
    process_env: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<String, ServiceDefinition>> {
    let mut services = BTreeMap::new();
    for module_dir in service_module_dirs {
        let path = module_dir.join("service.toml");
        if !path.exists() {
            continue;
        }
        let value = service_definition_value(load_toml_value(&path, config_context, process_env)?);
        let service: ServiceDefinition = value
            .try_into()
            .with_context(|| format!("failed to deserialize {}", path.display()))?;
        if services
            .insert(service.name.clone(), service.clone())
            .is_some()
        {
            bail!("duplicate service `{}`", service.name);
        }
    }
    Ok(services)
}

fn service_definition_value(mut value: Value) -> Value {
    let Some(table) = value.as_table_mut() else {
        return value;
    };
    for manifest_key in [
        "version",
        "scope",
        "targets",
        "inputs",
        "dependencies",
        "runtime",
        "hooks",
        "outputs",
    ] {
        table.remove(manifest_key);
    }
    if let Some(Value::Table(container)) = table.remove("container") {
        for (key, value) in container {
            table.insert(key, value);
        }
    }
    value
}

fn load_toml_value(
    path: &Path,
    config_context: Option<&Value>,
    process_env: Option<&BTreeMap<String, String>>,
) -> Result<Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value = raw
        .parse::<Value>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    match (config_context, process_env) {
        (Some(config_context), Some(process_env)) => {
            let merged_context = merge_context_env(config_context, &value)?;
            vmctl_config::resolve_toml_value_with_context_passthrough(
                value,
                &merged_context,
                process_env,
            )
            .with_context(|| format!("failed to interpolate {}", path.display()))
        }
        _ => Ok(value),
    }
}

fn merge_context_env(context: &Value, module_value: &Value) -> Result<Value> {
    let mut merged = context.clone();
    let Some(module_env) = module_value.get("env").and_then(Value::as_table) else {
        return Ok(merged);
    };
    let merged_table = merged
        .as_table_mut()
        .ok_or_else(|| anyhow!("vmctl config context must be a table"))?;
    let env = merged_table
        .entry("env".to_string())
        .or_insert_with(|| Value::Table(toml::map::Map::new()));
    let env_table = env
        .as_table_mut()
        .ok_or_else(|| anyhow!("vmctl config [env] must be a table"))?;
    for (key, value) in module_env {
        env_table.insert(key.clone(), value.clone());
    }
    Ok(merged)
}

fn render_template(source: &Path, context: &serde_json::Value) -> Result<String> {
    let template = std::fs::read_to_string(source)
        .with_context(|| format!("failed to read template {}", source.display()))?;
    let mut handlebars = Handlebars::new();
    handlebars_helper!(eq: |a: Json, b: Json| a == b);
    handlebars_helper!(has_items: |value: Json| {
        if let Some(items) = value.as_array() {
            !items.is_empty()
        } else if let Some(items) = value.as_object() {
            !items.is_empty()
        } else if let Some(text) = value.as_str() {
            !text.is_empty()
        } else if let Some(flag) = value.as_bool() {
            flag
        } else {
            !value.is_null()
        }
    });
    handlebars.register_helper("eq", Box::new(eq));
    handlebars.register_helper("has_items", Box::new(has_items));
    handlebars
        .render_template(&template, context)
        .with_context(|| format!("failed to render template {}", source.display()))
}

fn render_context(
    resource: &Resource,
    expansion: &Expansion,
    services: &BTreeMap<String, ServiceDefinition>,
) -> Result<serde_json::Value> {
    let service_definitions = expansion
        .service_defs
        .iter()
        .map(|name| {
            services
                .get(name)
                .with_context(|| format!("missing service resource `{name}`"))
        })
        .collect::<Result<Vec<_>>>()?;

    let ui_services = service_definitions
        .iter()
        .filter_map(|service| ui_service_context(service))
        .collect::<Vec<_>>();
    let service_settings = service_definitions
        .iter()
        .map(|service| {
            (
                service.name.clone(),
                serde_json::to_value(&service.settings).unwrap_or_else(|_| serde_json::json!({})),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();

    Ok(serde_json::json!({
        "resource": resource,
        "features": resource.features,
        "vpn": media_vpn_context(resource),
        "expansion": expansion,
        "services": expansion.service_defs,
        "service_definitions": service_definitions,
        "service_settings": service_settings,
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

fn ui_service_context(service: &ServiceDefinition) -> Option<serde_json::Value> {
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

fn service_names_for_resource(role: &ResourceManifest, resource: &Resource) -> Vec<String> {
    let resource_services = resource
        .features
        .values()
        .filter_map(Value::as_table)
        .filter_map(service_names_from_feature)
        .next();

    resource_services.unwrap_or_else(|| service_names_from_role(role))
}

fn service_names_from_role(role: &ResourceManifest) -> Vec<String> {
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
        let role: ResourceManifest = toml::from_str(
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
        let role: ResourceManifest = toml::from_str(
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
            enabled: true,
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
            enabled: true,
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
            enabled: true,
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
        resource.settings.insert(
            "hostname".to_string(),
            toml::Value::String("gateway".to_string()),
        );
        assert_eq!(
            tailscale_context(&resource).get("hostname").unwrap(),
            "tailscale-gateway"
        );
    }

    #[test]
    fn renders_templates_and_copies_scripts() {
        let root = unique_temp_dir();
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("guest/templates")).unwrap();
        std::fs::create_dir_all(resources_root.join("guest/scripts")).unwrap();
        std::fs::create_dir_all(services_root.join("demo")).unwrap();

        std::fs::write(
            resources_root.join("guest/resource.toml"),
            r#"
            name = "guest"
            kind = "vm"
            role = "example"

            [features.bundle]
            services = ["demo"]

            [render]
            templates = ["example.txt.hbs"]

            [hooks]
            bootstrap = ["scripts/bootstrap.sh"]
            "#,
        )
        .unwrap();
        std::fs::write(
            services_root.join("demo/service.toml"),
            r#"
            name = "demo"
            container_type = "docker"
            "#,
        )
        .unwrap();
        std::fs::write(
            resources_root.join("guest/templates/example.txt.hbs"),
            "{{resource.name}}:{{lookup services 0}}",
        )
        .unwrap();
        std::fs::write(
            resources_root.join("guest/scripts/bootstrap.sh"),
            "#!/usr/bin/env bash\n",
        )
        .unwrap();

        let registry = ResourceRegistry::load(&resources_root, &services_root).unwrap();
        let resource = Resource {
            name: "guest".to_string(),
            kind: "vm".to_string(),
            enabled: true,
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
    fn copies_non_hook_scripts_needed_by_hook_scripts() {
        let root = unique_temp_dir();
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("guest/scripts")).unwrap();

        std::fs::write(
            resources_root.join("guest/resource.toml"),
            r#"
            name = "guest"
            kind = "vm"
            role = "example"

            [hooks]
            bootstrap = ["scripts/bootstrap.sh"]
            "#,
        )
        .unwrap();
        std::fs::write(
            resources_root.join("guest/scripts/bootstrap.sh"),
            "#!/usr/bin/env bash\n. \"$(dirname \"$0\")/helper.sh\"\n",
        )
        .unwrap();
        std::fs::write(
            resources_root.join("guest/scripts/helper.sh"),
            "#!/usr/bin/env bash\necho helper\n",
        )
        .unwrap();

        let registry = ResourceRegistry::load(&resources_root, &services_root).unwrap();
        let resource = Resource {
            name: "guest".to_string(),
            kind: "vm".to_string(),
            enabled: true,
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

        assert!(output
            .join("resources/guest/scripts/bootstrap.sh")
            .is_file());
        assert!(output.join("resources/guest/scripts/helper.sh").is_file());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interpolates_service_toml_from_vmctl_config_context() {
        let root = unique_temp_dir();
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("guest/templates")).unwrap();
        std::fs::create_dir_all(services_root.join("demo")).unwrap();

        std::fs::write(
            resources_root.join("guest/resource.toml"),
            r#"
            name = "guest"
            kind = "vm"
            role = "example"

            [features.bundle]
            services = ["${const.service_name}"]

            [render]
            templates = ["example.env.hbs"]
            "#,
        )
        .unwrap();
        std::fs::write(
            services_root.join("demo/service.toml"),
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
            resources_root.join("guest/templates/example.env.hbs"),
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
        let registry = ResourceRegistry::load_with_config(
            &resources_root,
            &services_root,
            &config_context,
            &process_env,
        )
        .unwrap();
        let resource = Resource {
            name: "guest".to_string(),
            kind: "vm".to_string(),
            enabled: true,
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
    fn service_env_overrides_global_env_during_interpolation() {
        let root = unique_temp_dir();
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("guest/templates")).unwrap();
        std::fs::create_dir_all(services_root.join("demo")).unwrap();

        std::fs::write(
            resources_root.join("guest/resource.toml"),
            r#"
            name = "guest"
            kind = "vm"
            role = "example"

            [features.bundle]
            services = ["demo"]

            [render]
            templates = ["example.env.hbs"]
            "#,
        )
        .unwrap();
        std::fs::write(
            services_root.join("demo/service.toml"),
            r#"
            name = "demo"
            container_type = "docker"

            [env]
            DEMO_UI_NAME = "Service UI"

            [ui]
            enabled = true
            port = 8123
            name = "${env.DEMO_UI_NAME}"
            "#,
        )
        .unwrap();
        std::fs::write(
            resources_root.join("guest/templates/example.env.hbs"),
            "{{#each ui_services}}{{name}}={{port}}{{/each}}",
        )
        .unwrap();

        let config_context = r#"
            [env]
            DEMO_UI_NAME = "Global UI"
        "#
        .parse::<Value>()
        .unwrap();
        let registry = ResourceRegistry::load_with_config(
            &resources_root,
            &services_root,
            &config_context,
            &BTreeMap::new(),
        )
        .unwrap();
        let resource = Resource {
            name: "guest".to_string(),
            kind: "vm".to_string(),
            enabled: true,
            image: None,
            role: Some("example".to_string()),
            vmid: None,
            depends_on: Vec::new(),
            features: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        let expansion = registry.expand_resource(&resource).unwrap();
        let output = root.join("generated");
        registry
            .render_artifacts(
                &output,
                &[resource],
                &BTreeMap::from([("guest".to_string(), expansion)]),
            )
            .unwrap();

        let rendered = std::fs::read_to_string(output.join("resources/guest/example.env")).unwrap();
        assert_eq!(rendered, "Service UI=8123");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renders_templates_with_eq_helper() {
        let root = unique_temp_dir();
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("guest/templates")).unwrap();
        std::fs::create_dir_all(services_root.join("jellyfin")).unwrap();

        std::fs::write(
            resources_root.join("guest/resource.toml"),
            r#"
            name = "guest"
            kind = "vm"
            role = "example"

            [render]
            templates = ["routes.txt.hbs"]
            "#,
        )
        .unwrap();
        std::fs::write(
            resources_root.join("guest/templates/routes.txt.hbs"),
            r#"{{#each ui_services}}{{#if (eq this.name "Jellyfin")}}{{this.port}}{{/if}}{{/each}}"#,
        )
        .unwrap();
        std::fs::write(
            services_root.join("jellyfin/service.toml"),
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

        let registry = ResourceRegistry::load(&resources_root, &services_root).unwrap();
        let resource = Resource {
            name: "guest".to_string(),
            kind: "vm".to_string(),
            enabled: true,
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
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("media-stack/templates")).unwrap();
        std::fs::create_dir_all(services_root.join("jellyfin")).unwrap();
        std::fs::create_dir_all(services_root.join("seerr")).unwrap();
        std::fs::create_dir_all(services_root.join("gluetun")).unwrap();

        std::fs::write(
            resources_root.join("media-stack/resource.toml"),
            r#"
            name = "media-stack"
            kind = "vm"
            role = "media_stack"

            [features.media_services]
            enabled = true
            services = ["jellyfin", "seerr", "gluetun"]

            [render]
            templates = ["media-index.html.hbs"]
            "#,
        )
        .unwrap();
        std::fs::write(
            services_root.join("jellyfin/service.toml"),
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
            services_root.join("seerr/service.toml"),
            r#"
            name = "seerr"
            container_type = "docker"
            [ports]
            published = ["5055:5055"]
            [ui]
            enabled = true
            name = "Seerr"
            "#,
        )
        .unwrap();
        std::fs::write(
            services_root.join("gluetun/service.toml"),
            r#"
            name = "gluetun"
            container_type = "docker"
            "#,
        )
        .unwrap();
        std::fs::write(
            resources_root.join("media-stack/templates/media-index.html.hbs"),
            r#"{{#each ui_services}}{{name}}={{port}} {{/each}}"#,
        )
        .unwrap();

        let registry = ResourceRegistry::load(&resources_root, &services_root).unwrap();
        let resource = Resource {
            name: "media-stack".to_string(),
            kind: "vm".to_string(),
            enabled: true,
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
        assert!(rendered.contains("Seerr=5055"));
        assert!(!rendered.contains("gluetun"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn media_vpn_context_requires_wireguard_inputs() {
        let mut resource = Resource {
            name: "media-stack".to_string(),
            kind: "vm".to_string(),
            enabled: true,
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
