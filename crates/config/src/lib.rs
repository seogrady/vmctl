use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use toml::Value;
use vmctl_domain::{BackendConfig, ImageConfig, ImageKind, ImageSource, Resource};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub defaults: BTreeMap<String, Value>,
    #[serde(default)]
    #[serde(rename = "const")]
    pub consts: BTreeMap<String, Value>,
    #[serde(default)]
    pub env: BTreeMap<String, Value>,
    #[serde(default)]
    pub images: BTreeMap<String, ImageConfig>,
    #[serde(default)]
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
        let resolved = Interpolator::new(value, process_env).resolve()?;
        let config: Config = resolved
            .try_into()
            .context("failed to deserialize resolved vmctl config")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for resource in &self.resources {
            if resource.name.trim().is_empty() {
                bail!("resource name cannot be empty");
            }
            if !matches!(resource.kind.as_str(), "vm" | "lxc") {
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
        self.validate_images()?;
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

struct Interpolator<'a> {
    root: Value,
    process_env: &'a BTreeMap<String, String>,
    consts: BTreeMap<String, Value>,
    env: BTreeMap<String, Value>,
}

impl<'a> Interpolator<'a> {
    fn new(root: Value, process_env: &'a BTreeMap<String, String>) -> Self {
        let consts = table_at(&root, "const");
        let env = table_at(&root, "env");
        Self {
            root,
            process_env,
            consts,
            env,
        }
    }

    fn resolve(mut self) -> Result<Value> {
        let mut stack = Vec::new();
        let mut root = self.root.clone();
        self.resolve_value(&mut root, &mut stack)?;
        self.root = root.clone();
        Ok(root)
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

        let in_const = self.consts.contains_key(reference);
        let in_env = self.env.contains_key(reference);
        match (in_const, in_env) {
            (true, false) => self.resolve_binding("const", reference, stack),
            (false, true) => self.resolve_env_or_process(reference, stack),
            (true, true) => {
                bail!("ambiguous interpolation `${{{reference}}}` exists in const and env")
            }
            (false, false) => self
                .process_env
                .get(reference)
                .cloned()
                .ok_or_else(|| anyhow!("unresolved interpolation `${{{reference}}}`")),
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
            Value::String(input)
                if namespace == "env" && input == &format!("${{{key}}}") =>
            {
                self.process_env
                    .get(key)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing environment variable `{key}`"))?
            }
            Value::String(input) if namespace == "env" && input.trim().is_empty() => self
                .process_env
                .get(key)
                .cloned()
                .unwrap_or_default(),
            Value::String(input) => self.resolve_string(input, stack)?,
            scalar if is_scalar(scalar) => scalar_to_string(scalar),
            _ => bail!("{namespace}.{key} must resolve to a scalar value"),
        };

        stack.pop();
        Ok(resolved)
    }

    fn resolve_full_path(&self, reference: &str, stack: &mut Vec<String>) -> Result<String> {
        let mut current = &self.root;
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
        || !key.chars().all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
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
            r#"
            [const]
            bridge = "vmbr0"

            [defaults]
            bridge = "${bridge}"
            "#,
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
            r#"
            [env]
            TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"

            [defaults.features.tailscale]
            auth_key = "${TAILSCALE_AUTH_KEY}"
            "#,
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
            r#"
            [env]
            WIREGUARD_PRIVATE_KEY = ""

            [defaults.features.vpn]
            wireguard_private_key = "${WIREGUARD_PRIVATE_KEY}"
            "#,
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

        let merged = process_env_with_shell_fallback_from_home(&BTreeMap::new(), Some(&root)).unwrap();
        assert_eq!(merged.get("WIREGUARD_PRIVATE_KEY").map(String::as_str), Some("wg-key"));
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

        let merged = process_env_with_shell_fallback_from_home(&BTreeMap::new(), Some(&root)).unwrap();
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
            r#"
            [const]
            value = "const-value"

            [env]
            value = "${VALUE}"

            [defaults]
            bridge = "${value}"
            "#,
            &env,
        )
        .unwrap_err();

        assert!(err.to_string().contains("ambiguous interpolation"));
    }

    #[test]
    fn rejects_cycles() {
        let err = Config::from_toml(
            r#"
            [const]
            a = "${const.b}"
            b = "${const.a}"

            [defaults]
            bridge = "${a}"
            "#,
            &BTreeMap::new(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("cyclic interpolation"));
    }

    #[test]
    fn parses_image_catalog_entries() {
        let cfg = Config::from_toml(
            r#"
            [images.debian_12_lxc]
            kind = "lxc"
            source = "pveam"
            storage = "local"
            content_type = "vztmpl"
            template = "debian-12-standard_12.7-1_amd64.tar.zst"
            "#,
            &BTreeMap::new(),
        )
        .unwrap();

        let image = cfg.images.get("debian_12_lxc").unwrap();
        assert_eq!(image.kind, ImageKind::Lxc);
        assert_eq!(image.source, ImageSource::Pveam);
    }

    #[test]
    fn rejects_invalid_pveam_vm_image() {
        let err = Config::from_toml(
            r#"
            [images.bad]
            kind = "vm"
            source = "pveam"
            storage = "local"
            content_type = "vztmpl"
            template = "debian-12-standard_12.7-1_amd64.tar.zst"
            "#,
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
}
