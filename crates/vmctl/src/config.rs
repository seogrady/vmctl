use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use toml::Value;

use crate::packs::{Expansion, PackRegistry};

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub generated_dir: PathBuf,
}

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
    pub resources: Vec<Resource>,
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
        Ok(())
    }
}

impl DesiredState {
    pub fn from_config(
        config: Config,
        registry: &PackRegistry,
        target: Option<&str>,
    ) -> Result<Self> {
        let resources: Vec<_> = config
            .resources
            .into_iter()
            .filter(|resource| target.map_or(true, |name| resource.name == name))
            .collect();

        if let Some(target) = target {
            if resources.is_empty() {
                bail!("target resource `{target}` was not found");
            }
        }

        let expansions = resources
            .iter()
            .map(|resource| {
                registry
                    .expand_resource(resource)
                    .map(|expansion| (resource.name.clone(), expansion))
            })
            .collect::<Result<_>>()?;

        Ok(Self {
            backend: config.backend,
            resources,
            expansions,
        })
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
            Value::String(input) => {
                *input = self.resolve_string(input, stack)?;
            }
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
            Value::String(input) if namespace == "env" && input == &format!("${{{key}}}") => self
                .process_env
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("missing environment variable `{key}`"))?,
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
}
