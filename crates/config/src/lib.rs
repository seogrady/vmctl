use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use toml::Value;
use vmctl_domain::{
    BackendConfig, ImageConfig, ImageKind, ImageSource, Resource, RuntimeConfig, ServiceSelection,
};

const SUPPORTED_CONFIG_MAJOR: u64 = 2;
const GENERIC_CONFIG_VERSION_ERROR: &str = "unsupported config format";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceCatalogConfig {
    #[serde(
        default = "default_local_sources",
        deserialize_with = "deserialize_source_paths"
    )]
    pub local: Vec<PathBuf>,
    #[serde(default)]
    pub git: Vec<String>,
}

fn default_local_sources() -> Vec<PathBuf> {
    vec![PathBuf::from("./resources"), PathBuf::from("./services")]
}

fn deserialize_source_paths<'de, D>(deserializer: D) -> std::result::Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = Option::<Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(default_local_sources());
    };

    let mut out = Vec::new();
    match value {
        Value::String(path) => out.push(PathBuf::from(path)),
        Value::Array(items) => {
            for item in items {
                let path = item
                    .as_str()
                    .ok_or_else(|| D::Error::custom("sources.local items must be strings"))?;
                out.push(PathBuf::from(path));
            }
        }
        _ => {
            return Err(D::Error::custom(
                "sources.local must be a string or array of strings",
            ));
        }
    }
    Ok(out)
}

fn deserialize_resources<'de, D>(deserializer: D) -> std::result::Result<Vec<Resource>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = Option::<Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    match value {
        Value::Table(entries) => {
            let mut resources = Vec::new();
            let mut names = entries.keys().cloned().collect::<Vec<_>>();
            names.sort();
            for name in names {
                let entry = entries
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| D::Error::custom("failed to read resource entry"))?;
                let entry_table = entry.as_table().cloned().ok_or_else(|| {
                    D::Error::custom(format!("resource `{name}` must be a table"))
                })?;
                let resource_value = normalize_resource_entry(&name, entry_table)
                    .map_err(|error| D::Error::custom(error.to_string()))?;
                let resource: Resource = resource_value
                    .try_into()
                    .map_err(|error| D::Error::custom(format!("resource `{name}`: {error}")))?;
                resources.push(resource);
            }
            Ok(resources)
        }
        Value::Array(_) => Err(D::Error::custom(
            "legacy `[[resources]]` format is not supported; migrate to `[resources.<name>]`",
        )),
        _ => Err(D::Error::custom(
            "`resources` must be a table keyed by resource name",
        )),
    }
}

fn normalize_resource_entry(name: &str, mut entry: toml::map::Map<String, Value>) -> Result<Value> {
    let mut normalized = toml::map::Map::<String, Value>::new();
    normalized.insert("name".to_string(), Value::String(name.to_string()));

    if let Some(config) = entry.remove("config") {
        let config_table = config
            .as_table()
            .cloned()
            .ok_or_else(|| anyhow!("resource `{name}` config must be a table"))?;
        for (key, value) in config_table {
            normalized.insert(key, value);
        }
    }

    entry.remove("source");
    entry.remove("services");

    for (key, value) in entry {
        normalized.insert(key, value);
    }

    Ok(Value::Table(normalized))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceSelection>,
    #[serde(default)]
    pub defaults: BTreeMap<String, Value>,
    #[serde(default)]
    #[serde(rename = "const")]
    pub consts: BTreeMap<String, Value>,
    #[serde(default)]
    pub env: BTreeMap<String, Value>,
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub sources: SourceCatalogConfig,
    #[serde(default)]
    pub images: BTreeMap<String, ImageConfig>,
    #[serde(default, deserialize_with = "deserialize_resources")]
    pub resources: Vec<Resource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfigPath {
    pub path: PathBuf,
    pub source: ConfigPathSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPathSource {
    Explicit,
    Default,
    ExampleFallback,
}

pub fn resolve_config_path(explicit: Option<&Path>) -> Result<ResolvedConfigPath> {
    resolve_config_path_in(Path::new("."), explicit)
}

pub fn resolve_config_path_in(root: &Path, explicit: Option<&Path>) -> Result<ResolvedConfigPath> {
    if let Some(path) = explicit {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        if candidate.exists() {
            return Ok(ResolvedConfigPath {
                path: path.to_path_buf(),
                source: ConfigPathSource::Explicit,
            });
        }
        bail!("config file not found: {}", path.display());
    }

    let default = root.join("vmctl.toml");
    if default.exists() {
        return Ok(ResolvedConfigPath {
            path: PathBuf::from("vmctl.toml"),
            source: ConfigPathSource::Default,
        });
    }

    let example = root.join("vmctl.example.toml");
    if example.exists() {
        return Ok(ResolvedConfigPath {
            path: PathBuf::from("vmctl.example.toml"),
            source: ConfigPathSource::ExampleFallback,
        });
    }

    bail!("config file not found: create vmctl.toml or pass --config <path>");
}

impl Config {
    pub fn from_toml(raw: &str, process_env: &BTreeMap<String, String>) -> Result<Self> {
        let value = raw.parse::<Value>().context("failed to parse vmctl TOML")?;
        validate_config_version(&value)?;
        let config = Self::from_value(resolve_toml_value(value, process_env)?)?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_value(value: Value) -> Result<Self> {
        validate_config_version(&value)?;
        let mut config: Config = value
            .try_into()
            .context("failed to deserialize resolved vmctl config")?;
        apply_runtime_defaults(&mut config)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for resource in &self.resources {
            if resource.name.trim().is_empty() {
                bail!("resource name cannot be empty");
            }
            if !matches!(resource.kind.as_str(), "vm" | "lxc" | "host") {
                bail!(
                    "resource `{}` has unsupported kind `{}`",
                    resource.name,
                    resource.kind
                );
            }
            if !names.insert(resource.name.clone()) {
                bail!("duplicate resource name `{}`", resource.name);
            }
        }
        for (group, members) in &self.groups {
            if group.trim().is_empty() {
                bail!("group name cannot be empty");
            }
            if members.iter().any(|member| member.trim().is_empty()) {
                bail!("group `{group}` contains an empty member");
            }
        }
        self.validate_images()?;
        self.validate_runtime()?;
        Ok(())
    }

    fn validate_runtime(&self) -> Result<()> {
        if !matches!(self.runtime.engine.as_str(), "docker" | "podman") {
            bail!(
                "runtime.engine must be `docker` or `podman`, got `{}`",
                self.runtime.engine
            );
        }
        Ok(())
    }

    fn validate_images(&self) -> Result<()> {
        for (name, image) in &self.images {
            if name.trim().is_empty() {
                bail!("image name cannot be empty");
            }
            if image.storage.trim().is_empty() {
                bail!("image `{name}` requires storage");
            }
            if image.content_type.trim().is_empty() {
                bail!("image `{name}` requires content_type");
            }
            if image.checksum_algorithm.is_some() != image.checksum.is_some() {
                bail!("image `{name}` requires checksum and checksum_algorithm together");
            }
            match image.source {
                ImageSource::Pveam => {
                    if image.kind != ImageKind::Lxc {
                        bail!("image `{name}` source pveam requires kind = \"lxc\"");
                    }
                    if image.content_type != "vztmpl" {
                        bail!("image `{name}` source pveam requires content_type = \"vztmpl\"");
                    }
                    if image
                        .template
                        .as_deref()
                        .unwrap_or_default()
                        .trim()
                        .is_empty()
                    {
                        bail!("image `{name}` source pveam requires template");
                    }
                }
                ImageSource::Url => {
                    if image.node.as_deref().unwrap_or_default().trim().is_empty() {
                        bail!("image `{name}` source url requires node");
                    }
                    if image
                        .file_name
                        .as_deref()
                        .unwrap_or_default()
                        .trim()
                        .is_empty()
                    {
                        bail!("image `{name}` source url requires file_name");
                    }
                    if image.url.as_deref().unwrap_or_default().trim().is_empty() {
                        bail!("image `{name}` source url requires url");
                    }
                }
                ImageSource::Existing => {
                    if image.kind == ImageKind::Vm && image.vmid.is_some() {
                        continue;
                    }
                    if image
                        .file_name
                        .as_deref()
                        .unwrap_or_default()
                        .trim()
                        .is_empty()
                    {
                        bail!("image `{name}` source existing requires file_name");
                    }
                }
            }
        }
        Ok(())
    }
}

fn validate_config_version(value: &Value) -> Result<()> {
    let Some(version) = value.get("version").and_then(Value::as_str) else {
        bail!(GENERIC_CONFIG_VERSION_ERROR);
    };
    let Some((major, _minor, _patch)) = parse_semver(version) else {
        bail!(GENERIC_CONFIG_VERSION_ERROR);
    };
    if major != SUPPORTED_CONFIG_MAJOR {
        bail!(GENERIC_CONFIG_VERSION_ERROR);
    }
    Ok(())
}

fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let core = raw.split_once('-').map(|(left, _)| left).unwrap_or(raw);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn apply_runtime_defaults(config: &mut Config) -> Result<()> {
    let Some(runtime_defaults) = config.defaults.get("runtime") else {
        return Ok(());
    };
    let runtime_defaults = runtime_defaults
        .as_table()
        .ok_or_else(|| anyhow!("defaults.runtime must be a table"))?;
    if let Some(engine) = runtime_defaults.get("engine").and_then(Value::as_str) {
        config.runtime.engine = engine.to_string();
    }
    Ok(())
}

pub fn resolve_toml_value(value: Value, process_env: &BTreeMap<String, String>) -> Result<Value> {
    Interpolator::new(value, process_env).resolve()
}

pub fn resolve_toml_value_with_context(
    value: Value,
    context: &Value,
    process_env: &BTreeMap<String, String>,
) -> Result<Value> {
    Interpolator::new_with_context(value, context.clone(), process_env).resolve()
}

pub fn resolve_toml_value_with_context_passthrough(
    value: Value,
    context: &Value,
    process_env: &BTreeMap<String, String>,
) -> Result<Value> {
    Interpolator::new_with_context(value, context.clone(), process_env)
        .with_unresolved_passthrough(true)
        .with_unqualified_passthrough(true)
        .resolve()
}

struct Interpolator<'a> {
    value: Value,
    context: Value,
    process_env: &'a BTreeMap<String, String>,
    consts: BTreeMap<String, Value>,
    env: BTreeMap<String, Value>,
    unresolved_passthrough: bool,
    unqualified_passthrough: bool,
}

impl<'a> Interpolator<'a> {
    fn new(value: Value, process_env: &'a BTreeMap<String, String>) -> Self {
        Self::new_with_context(value.clone(), value, process_env)
    }

    fn new_with_context(
        value: Value,
        context: Value,
        process_env: &'a BTreeMap<String, String>,
    ) -> Self {
        let consts = table_at(&context, "const");
        let env = table_at(&context, "env");
        Self {
            value,
            context,
            process_env,
            consts,
            env,
            unresolved_passthrough: false,
            unqualified_passthrough: false,
        }
    }

    fn with_unresolved_passthrough(mut self, enabled: bool) -> Self {
        self.unresolved_passthrough = enabled;
        self
    }

    fn with_unqualified_passthrough(mut self, enabled: bool) -> Self {
        self.unqualified_passthrough = enabled;
        self
    }

    fn resolve(mut self) -> Result<Value> {
        let mut stack = Vec::new();
        let mut value = self.value.clone();
        self.resolve_value(&mut value, &mut stack)?;
        self.value = value.clone();
        Ok(value)
    }

    fn resolve_value(&self, value: &mut Value, stack: &mut Vec<String>) -> Result<()> {
        match value {
            Value::String(input) => *input = self.resolve_string(input, stack)?,
            Value::Array(items) => {
                for item in items {
                    self.resolve_value(item, stack)?;
                }
            }
            Value::Table(table) => {
                for (_key, item) in table.iter_mut() {
                    self.resolve_value(item, stack)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn resolve_string(&self, input: &str, stack: &mut Vec<String>) -> Result<String> {
        let mut out = String::new();
        let mut rest = input;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after_start = &rest[start + 2..];
            let end = after_start
                .find('}')
                .ok_or_else(|| anyhow!("unclosed interpolation in `{input}`"))?;
            let name = &after_start[..end];
            out.push_str(&self.resolve_ref(name, stack)?);
            rest = &after_start[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn resolve_ref(&self, reference: &str, stack: &mut Vec<String>) -> Result<String> {
        if let Some(name) = reference.strip_prefix("const.") {
            return self.resolve_binding("const", name, stack);
        }
        if let Some(name) = reference.strip_prefix("env.") {
            return self.resolve_env_or_process(name, stack);
        }
        if reference.contains('.') {
            return self.resolve_full_path(reference, stack);
        }
        if self.unqualified_passthrough {
            return Ok(format!("${{{reference}}}"));
        }

        let in_const = self.consts.contains_key(reference);
        let in_env = self.env.contains_key(reference);
        match (in_const, in_env) {
            (true, false) => self.resolve_binding("const", reference, stack),
            (false, true) => self.resolve_env_or_process(reference, stack),
            (true, true) => {
                bail!("ambiguous interpolation `${{{reference}}}` exists in const and env")
            }
            (false, false) => self.process_env.get(reference).cloned().map_or_else(
                || {
                    if self.unresolved_passthrough {
                        Ok(format!("${{{reference}}}"))
                    } else {
                        Err(anyhow!("unresolved interpolation `${{{reference}}}`"))
                    }
                },
                Ok,
            ),
        }
    }

    fn resolve_env_or_process(&self, name: &str, stack: &mut Vec<String>) -> Result<String> {
        if self.env.contains_key(name) {
            self.resolve_binding("env", name, stack)
        } else {
            self.process_env
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("missing environment variable `{name}`"))
        }
    }

    fn resolve_binding(
        &self,
        namespace: &str,
        key: &str,
        stack: &mut Vec<String>,
    ) -> Result<String> {
        let stack_key = format!("{namespace}.{key}");
        if stack.iter().any(|item| item == &stack_key) {
            bail!(
                "cyclic interpolation detected: {} -> {stack_key}",
                stack.join(" -> ")
            );
        }
        stack.push(stack_key);

        let value = match namespace {
            "const" => self.consts.get(key),
            "env" => self.env.get(key),
            _ => None,
        }
        .ok_or_else(|| anyhow!("missing {namespace} binding `{key}`"))?;

        let resolved = match value {
            Value::String(input) if namespace == "env" && input == &format!("${{{key}}}") => self
                .process_env
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("missing environment variable `{key}`"))?,
            Value::String(input) if namespace == "env" && input.trim().is_empty() => {
                self.process_env.get(key).cloned().unwrap_or_default()
            }
            Value::String(input) => self.resolve_string(input, stack)?,
            scalar if is_scalar(scalar) => scalar_to_string(scalar),
            _ => bail!("{namespace}.{key} must resolve to a scalar value"),
        };

        stack.pop();
        Ok(resolved)
    }

    fn resolve_full_path(&self, reference: &str, stack: &mut Vec<String>) -> Result<String> {
        let mut current = &self.context;
        for segment in reference.split('.') {
            current = current
                .as_table()
                .and_then(|table| table.get(segment))
                .ok_or_else(|| {
                    anyhow!("full-path interpolation `${{{reference}}}` was not found")
                })?;
        }

        match current {
            Value::String(input) => self.resolve_string(input, stack),
            scalar if is_scalar(scalar) => Ok(scalar_to_string(scalar)),
            _ => bail!("full-path interpolation `${{{reference}}}` must reference a scalar"),
        }
    }
}

pub fn process_env_with_shell_fallback(
    process_env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    process_env_with_shell_fallback_from_home(process_env, home.as_deref())
}

fn process_env_with_shell_fallback_from_home(
    process_env: &BTreeMap<String, String>,
    home: Option<&Path>,
) -> Result<BTreeMap<String, String>> {
    let mut merged = process_env.clone();
    if let Some(home) = home {
        for shell_file in [".bashrc", ".profile"] {
            merge_shell_exports(home.join(shell_file).as_path(), &mut merged)?;
        }
    }
    Ok(merged)
}

fn merge_shell_exports(path: &Path, env: &mut BTreeMap<String, String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read shell env file {}", path.display()))?;
    for line in raw.lines() {
        if let Some((key, value)) = parse_shell_assignment(line) {
            env.entry(key).or_insert(value);
        }
    }
    Ok(())
}

fn parse_shell_assignment(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let binding = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    if binding.starts_with("declare -x ") {
        return parse_shell_assignment(binding.trim_start_matches("declare -x ").trim());
    }

    let (key, value) = binding.split_once('=')?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        || !key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        return None;
    }

    let value = strip_inline_comment(value.trim());
    let parsed = if let Some(stripped) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        stripped.replace("\\\"", "\"").replace("\\\\", "\\")
    } else if let Some(stripped) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        stripped.to_string()
    } else {
        value.to_string()
    };
    Some((key.to_string(), parsed))
}

fn strip_inline_comment(value: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_escape = false;
    for (idx, ch) in value.char_indices() {
        if ch == '\\' && !prev_escape {
            prev_escape = true;
            continue;
        }
        if ch == '\'' && !in_double && !prev_escape {
            in_single = !in_single;
        } else if ch == '"' && !in_single && !prev_escape {
            in_double = !in_double;
        } else if ch == '#' && !in_single && !in_double && !prev_escape {
            let trimmed = value[..idx].trim_end();
            return trimmed;
        }
        prev_escape = false;
    }
    value
}

fn table_at(root: &Value, key: &str) -> BTreeMap<String, Value> {
    root.get(key)
        .and_then(Value::as_table)
        .map(|table| {
            table
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::String(_)
            | Value::Integer(_)
            | Value::Float(_)
            | Value::Boolean(_)
            | Value::Datetime(_)
    )
}

fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Boolean(value) => value.to_string(),
        Value::Datetime(value) => value.to_string(),
        _ => unreachable!("scalar_to_string called with non-scalar value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_direct_const_reference() {
        let cfg = Config::from_toml(
            &with_version(
                r#"
            [const]
            bridge = "vmbr0"

            [defaults]
            bridge = "${bridge}"
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(
            cfg.defaults.get("bridge").and_then(Value::as_str),
            Some("vmbr0")
        );
    }

    #[test]
    fn resolves_direct_env_reference() {
        let env = BTreeMap::from([("TAILSCALE_AUTH_KEY".into(), "tskey-123".into())]);
        let cfg = Config::from_toml(
            &with_version(
                r#"
            [env]
            TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"

            [defaults.features.tailscale]
            auth_key = "${TAILSCALE_AUTH_KEY}"
            "#,
            ),
            &env,
        )
        .unwrap();

        assert_eq!(
            cfg.defaults
                .get("features")
                .and_then(Value::as_table)
                .and_then(|features| features.get("tailscale"))
                .and_then(Value::as_table)
                .and_then(|tailscale| tailscale.get("auth_key"))
                .and_then(Value::as_str),
            Some("tskey-123")
        );
    }

    #[test]
    fn falls_back_to_process_env_for_empty_env_bindings() {
        let env = BTreeMap::from([("WIREGUARD_PRIVATE_KEY".into(), "wg-key".into())]);
        let cfg = Config::from_toml(
            &with_version(
                r#"
            [env]
            WIREGUARD_PRIVATE_KEY = ""

            [defaults.features.vpn]
            wireguard_private_key = "${WIREGUARD_PRIVATE_KEY}"
            "#,
            ),
            &env,
        )
        .unwrap();

        assert_eq!(
            cfg.defaults
                .get("features")
                .and_then(Value::as_table)
                .and_then(|features| features.get("vpn"))
                .and_then(Value::as_table)
                .and_then(|vpn| vpn.get("wireguard_private_key"))
                .and_then(Value::as_str),
            Some("wg-key")
        );
    }

    #[test]
    fn context_interpolation_can_leave_unknown_runtime_placeholders() {
        let context = r#"
            [const]
            service = "demo"

            [env]
            RUNTIME_SECRET = ""
        "#
        .parse::<Value>()
        .unwrap();
        let value = r#"
            name = "${const.service}"
            mount = "${STORAGE_PATH}:/media"
            secret = "${RUNTIME_SECRET}"
        "#
        .parse::<Value>()
        .unwrap();

        let resolved =
            resolve_toml_value_with_context_passthrough(value, &context, &BTreeMap::new()).unwrap();

        assert_eq!(resolved["name"].as_str(), Some("demo"));
        assert_eq!(resolved["mount"].as_str(), Some("${STORAGE_PATH}:/media"));
        assert_eq!(resolved["secret"].as_str(), Some("${RUNTIME_SECRET}"));
    }

    #[test]
    fn merges_shell_env_exports_from_bashrc() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(".bashrc"),
            r#"
            export WIREGUARD_PRIVATE_KEY=wg-key
            export WIREGUARD_ADDRESSES="10.67.87.73/32,fc00:bbbb:bbbb:bb01::4:5748/128"
            "#,
        )
        .unwrap();

        let merged =
            process_env_with_shell_fallback_from_home(&BTreeMap::new(), Some(&root)).unwrap();
        assert_eq!(
            merged.get("WIREGUARD_PRIVATE_KEY").map(String::as_str),
            Some("wg-key")
        );
        assert_eq!(
            merged.get("WIREGUARD_ADDRESSES").map(String::as_str),
            Some("10.67.87.73/32,fc00:bbbb:bbbb:bb01::4:5748/128")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn strips_inline_comments_from_shell_exports() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(".bashrc"),
            "export VPN_SERVER_CITIES='Melbourne'   # or your Mullvad city label\n",
        )
        .unwrap();

        let merged =
            process_env_with_shell_fallback_from_home(&BTreeMap::new(), Some(&root)).unwrap();
        assert_eq!(
            merged.get("VPN_SERVER_CITIES").map(String::as_str),
            Some("Melbourne")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_ambiguous_direct_reference() {
        let env = BTreeMap::from([("VALUE".into(), "env-value".into())]);
        let err = Config::from_toml(
            &with_version(
                r#"
            [const]
            value = "const-value"

            [env]
            value = "${VALUE}"

            [defaults]
            bridge = "${value}"
            "#,
            ),
            &env,
        )
        .unwrap_err();

        assert!(err.to_string().contains("ambiguous interpolation"));
    }

    #[test]
    fn rejects_cycles() {
        let err = Config::from_toml(
            &with_version(
                r#"
            [const]
            a = "${const.b}"
            b = "${const.a}"

            [defaults]
            bridge = "${a}"
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("cyclic interpolation"));
    }

    #[test]
    fn parses_image_catalog_entries() {
        let cfg = Config::from_toml(
            &with_version(
                r#"
            [images.debian_12_lxc]
            kind = "lxc"
            source = "pveam"
            storage = "local"
            content_type = "vztmpl"
            template = "debian-12-standard_12.7-1_amd64.tar.zst"
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap();

        let image = cfg.images.get("debian_12_lxc").unwrap();
        assert_eq!(image.kind, ImageKind::Lxc);
        assert_eq!(image.source, ImageSource::Pveam);
    }

    #[test]
    fn parses_resources_table_format() {
        let cfg = Config::from_toml(
            &with_version(
                r#"
            [resources.media-stack]
            kind = "vm"
            role = "media_stack"
            vmid = 210

            [resources.media-stack.config]
            memory = 8192

            [resources.media-stack.config.features.media_services]
            services = ["jellyfin", "radarr"]
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(cfg.resources.len(), 1);
        let resource = &cfg.resources[0];
        assert_eq!(resource.name, "media-stack");
        assert_eq!(resource.kind, "vm");
        assert_eq!(resource.vmid, Some(210));
        assert_eq!(
            resource.settings.get("memory").and_then(Value::as_integer),
            Some(8192)
        );
    }

    #[test]
    fn rejects_missing_config_version() {
        let err = Config::from_toml(
            r#"
            [env]
            TOKEN = "b"
            "#,
            &BTreeMap::new(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("unsupported config format"));
    }

    #[test]
    fn sources_local_accepts_single_string() {
        let cfg = Config::from_toml(
            &with_version(
                r#"
            [sources]
            local = "./modules"
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(cfg.sources.local, vec![PathBuf::from("./modules")]);
    }

    #[test]
    fn rejects_unsupported_config_major() {
        let err = Config::from_toml("version = \"3.0.0\"", &BTreeMap::new()).unwrap_err();

        assert!(err.to_string().contains("unsupported config format"));
    }

    #[test]
    fn runtime_reads_from_defaults_runtime() {
        let cfg = Config::from_toml(
            &with_version(
                r#"
            [defaults.runtime]
            engine = "podman"
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(cfg.runtime.engine, "podman");
    }

    #[test]
    fn rejects_legacy_resources_array_format() {
        let err = Config::from_toml(
            &with_version(
                r#"
            [[resources]]
            name = "media-stack"
            kind = "vm"
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("legacy `[[resources]]` format")
                || message.contains("resources")
                || message.contains("deserialize"),
            "{message}"
        );
    }

    #[test]
    fn rejects_invalid_pveam_vm_image() {
        let err = Config::from_toml(
            &with_version(
                r#"
            [images.bad]
            kind = "vm"
            source = "pveam"
            storage = "local"
            content_type = "vztmpl"
            template = "debian-12-standard_12.7-1_amd64.tar.zst"
            "#,
            ),
            &BTreeMap::new(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("source pveam requires kind"));
    }

    #[test]
    fn resolves_default_config_before_example() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("vmctl.toml"), "").unwrap();
        std::fs::write(root.join("vmctl.example.toml"), "").unwrap();

        let resolved = resolve_config_path_in(&root, None).unwrap();

        assert_eq!(resolved.path, PathBuf::from("vmctl.toml"));
        assert_eq!(resolved.source, ConfigPathSource::Default);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn falls_back_to_example_config() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("vmctl.example.toml"), "").unwrap();

        let resolved = resolve_config_path_in(&root, None).unwrap();

        assert_eq!(resolved.path, PathBuf::from("vmctl.example.toml"));
        assert_eq!(resolved.source, ConfigPathSource::ExampleFallback);

        std::fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    fn with_version(body: &str) -> String {
        format!("version = \"2.0.0\"\n{body}")
    }
}
