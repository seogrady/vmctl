use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use glob::glob;
use serde::{Deserialize, Serialize};
use toml::Value;
use vmctl_domain::{
    Resource, RuntimeConfig, ServiceExecutionPlan, ServiceInstancePlan, ServiceSelection,
    ServiceTemplatePlan,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceManifest {
    pub name: String,
    pub version: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub inputs: InputSection,
    #[serde(default)]
    pub dependencies: DependencySection,
    #[serde(default)]
    pub runtime: RuntimeSection,
    #[serde(default)]
    pub scripts: ScriptSection,
    #[serde(default)]
    pub outputs: OutputSection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputSection {
    #[serde(default)]
    pub schema: Vec<InputSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSpec {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: InputKind,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub allowed: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Bool,
    String,
    U16,
    U32,
    I64,
    Array,
    Table,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencySection {
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub optional: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeSection {
    #[serde(default)]
    pub requirements: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub templates: Vec<TemplateSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSpec {
    pub src: String,
    pub dst: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScriptSection {
    #[serde(default)]
    pub provision: ScriptRefs,
    #[serde(default)]
    pub validate: ScriptRefs,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScriptRefs {
    One(String),
    Many(Vec<String>),
    #[default]
    None,
}

impl ScriptRefs {
    fn resolve(&self, root: &Path) -> Result<Vec<String>> {
        let patterns = match self {
            ScriptRefs::One(pattern) => vec![pattern.as_str()],
            ScriptRefs::Many(patterns) => patterns.iter().map(String::as_str).collect(),
            ScriptRefs::None => Vec::new(),
        };
        let mut resolved = Vec::new();
        for pattern in patterns {
            let full_pattern = root.join(pattern);
            let pattern_text = full_pattern.to_string_lossy().to_string();
            let mut matches = glob(&pattern_text)
                .with_context(|| format!("invalid script glob `{pattern}`"))?
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("failed to resolve script glob `{pattern}`"))?;
            matches.sort();
            if matches.is_empty() && !has_glob_meta(pattern) {
                matches.push(root.join(pattern));
            }
            if matches.is_empty() {
                bail!("script glob `{pattern}` matched no files");
            }
            for path in matches {
                let relative = path.strip_prefix(root).with_context(|| {
                    format!("script {} is outside {}", path.display(), root.display())
                })?;
                resolved.push(relative.to_string_lossy().to_string());
            }
        }
        resolved.dedup();
        Ok(resolved)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputSection {
    #[serde(default)]
    pub publish: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct ServiceRegistry {
    root: PathBuf,
    manifests: BTreeMap<String, ServiceManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCommand {
    pub program: String,
    pub args: Vec<String>,
}

pub trait ContainerRuntime {
    fn engine(&self) -> &'static str;
    fn compose_up(&self, project_dir: &Path) -> RuntimeCommand;
    fn compose_down(&self, project_dir: &Path) -> RuntimeCommand;
    fn logs(&self, service: &str) -> RuntimeCommand;
    fn exec(&self, service: &str, args: &[&str]) -> RuntimeCommand;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DockerRuntime;

#[derive(Debug, Clone, Copy, Default)]
pub struct PodmanRuntime;

pub fn runtime_for(config: &RuntimeConfig) -> Result<Box<dyn ContainerRuntime>> {
    match config.engine.as_str() {
        "docker" => Ok(Box::new(DockerRuntime)),
        "podman" => Ok(Box::new(PodmanRuntime)),
        other => bail!("unsupported container runtime `{other}`; expected docker or podman"),
    }
}

impl ContainerRuntime for DockerRuntime {
    fn engine(&self) -> &'static str {
        "docker"
    }

    fn compose_up(&self, project_dir: &Path) -> RuntimeCommand {
        compose_command("docker", project_dir, ["compose"], ["up", "-d"])
    }

    fn compose_down(&self, project_dir: &Path) -> RuntimeCommand {
        compose_command("docker", project_dir, ["compose"], ["down"])
    }

    fn logs(&self, service: &str) -> RuntimeCommand {
        RuntimeCommand {
            program: "docker".to_string(),
            args: vec!["logs".to_string(), service.to_string()],
        }
    }

    fn exec(&self, service: &str, args: &[&str]) -> RuntimeCommand {
        exec_command("docker", service, args)
    }
}

impl ContainerRuntime for PodmanRuntime {
    fn engine(&self) -> &'static str {
        "podman"
    }

    fn compose_up(&self, project_dir: &Path) -> RuntimeCommand {
        compose_command("podman", project_dir, ["compose"], ["up", "-d"])
    }

    fn compose_down(&self, project_dir: &Path) -> RuntimeCommand {
        compose_command("podman", project_dir, ["compose"], ["down"])
    }

    fn logs(&self, service: &str) -> RuntimeCommand {
        RuntimeCommand {
            program: "podman".to_string(),
            args: vec!["logs".to_string(), service.to_string()],
        }
    }

    fn exec(&self, service: &str, args: &[&str]) -> RuntimeCommand {
        exec_command("podman", service, args)
    }
}

impl ServiceRegistry {
    pub fn load(root: &Path) -> Result<Self> {
        let mut manifests = BTreeMap::new();
        if !root.exists() {
            return Ok(Self {
                root: root.to_path_buf(),
                manifests,
            });
        }

        for entry in std::fs::read_dir(root)
            .with_context(|| format!("failed to read services directory {}", root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path().join("service.toml");
            if !path.exists() {
                continue;
            }
            let manifest = load_manifest(&path)?;
            let expected = entry.file_name().to_string_lossy().to_string();
            if manifest.name != expected {
                bail!(
                    "service `{}` declares name `{}`; directory name must match",
                    path.display(),
                    manifest.name
                );
            }
            if manifests.insert(manifest.name.clone(), manifest).is_some() {
                bail!("duplicate service `{expected}`");
            }
        }

        Ok(Self {
            root: root.to_path_buf(),
            manifests,
        })
    }

    pub fn from_manifests(manifests: Vec<ServiceManifest>) -> Result<Self> {
        let mut by_name = BTreeMap::new();
        for manifest in manifests {
            if by_name.insert(manifest.name.clone(), manifest).is_some() {
                bail!("duplicate service manifest");
            }
        }
        Ok(Self {
            root: PathBuf::from("services"),
            manifests: by_name,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.manifests.is_empty()
    }

    pub fn manifests(&self) -> &BTreeMap<String, ServiceManifest> {
        &self.manifests
    }

    pub fn build_plan(
        &self,
        selections: &BTreeMap<String, ServiceSelection>,
        resources: &[Resource],
        target: Option<&str>,
    ) -> Result<ServiceExecutionPlan> {
        let mut requested = BTreeSet::<(String, Option<String>)>::new();
        for (name, selection) in selections {
            if selection.enabled() {
                let target = self
                    .manifests
                    .get(name)
                    .filter(|manifest| manifest.scope == "resource")
                    .map(|_| target.unwrap_or("*").to_string());
                requested.insert((name.clone(), target));
            }
        }
        for resource in resources {
            if target.is_some_and(|target| target != resource.name) {
                continue;
            }
            if let Some(role) = &resource.role {
                let normalized = role.replace('_', "-");
                if self.manifests.contains_key(&normalized) {
                    requested.insert((normalized, Some(resource.name.clone())));
                }
            }
            for service in resource_services(resource) {
                if self.manifests.contains_key(&service) {
                    requested.insert((service, Some(resource.name.clone())));
                }
            }
        }

        let mut visiting = BTreeSet::<(String, Option<String>)>::new();
        let mut visited = BTreeSet::<(String, Option<String>)>::new();
        let mut ordered = Vec::<(String, Option<String>)>::new();
        for (name, target) in requested {
            self.visit(
                &name,
                target.as_deref(),
                selections,
                &mut visiting,
                &mut visited,
                &mut ordered,
            )?;
        }

        let mut instances = Vec::new();
        for (name, instance_target) in ordered {
            let manifest = self
                .manifests
                .get(&name)
                .ok_or_else(|| anyhow!("missing service `{name}`"))?;
            let config = resolve_inputs(
                manifest,
                selections
                    .get(&name)
                    .map(ServiceSelection::overrides)
                    .unwrap_or_default(),
            )?;
            let plan_target = if manifest.scope == "resource" {
                instance_target
            } else {
                None
            };
            instances.push(ServiceInstancePlan {
                key: instance_key(manifest, plan_target.as_deref()),
                service: manifest.name.clone(),
                version: manifest.version.clone(),
                scope: manifest.scope.clone(),
                target: plan_target,
                required_dependencies: manifest.dependencies.requires.clone(),
                optional_dependencies: enabled_optional_dependencies(manifest, selections),
                services: manifest.runtime.services.clone(),
                templates: manifest
                    .runtime
                    .templates
                    .iter()
                    .map(|template| ServiceTemplatePlan {
                        src: template.src.clone(),
                        dst: template.dst.clone(),
                    })
                    .collect(),
                provision_scripts: manifest
                    .scripts
                    .provision
                    .resolve(&self.root.join(&manifest.name))?,
                validation_scripts: manifest
                    .scripts
                    .validate
                    .resolve(&self.root.join(&manifest.name))?,
                runtime_requirements: manifest.runtime.requirements.clone(),
                outputs: config,
            });
        }

        Ok(ServiceExecutionPlan { instances })
    }

    fn visit(
        &self,
        name: &str,
        target: Option<&str>,
        selections: &BTreeMap<String, ServiceSelection>,
        visiting: &mut BTreeSet<(String, Option<String>)>,
        visited: &mut BTreeSet<(String, Option<String>)>,
        ordered: &mut Vec<(String, Option<String>)>,
    ) -> Result<()> {
        let manifest = self
            .manifests
            .get(name)
            .ok_or_else(|| anyhow!("service `{name}` was requested but no manifest exists"))?;
        let instance_target = if manifest.scope == "resource" {
            target.map(str::to_string)
        } else {
            None
        };
        let key = (name.to_string(), instance_target.clone());
        if visited.contains(&key) {
            return Ok(());
        }
        if !visiting.insert(key.clone()) {
            bail!("service dependency cycle detected at `{name}`");
        }
        for dependency in &manifest.dependencies.requires {
            self.visit(
                dependency,
                instance_target.as_deref(),
                selections,
                visiting,
                visited,
                ordered,
            )?;
        }
        for dependency in enabled_optional_dependencies(manifest, selections) {
            self.visit(
                &dependency,
                instance_target.as_deref(),
                selections,
                visiting,
                visited,
                ordered,
            )?;
        }
        visiting.remove(&key);
        visited.insert(key.clone());
        ordered.push(key);
        Ok(())
    }

    pub fn render_artifacts(
        &self,
        generated_root: &Path,
        plan: &ServiceExecutionPlan,
    ) -> Result<Vec<PathBuf>> {
        let mut written = Vec::new();
        for instance in &plan.instances {
            let Some(manifest) = self.manifests.get(&instance.service) else {
                continue;
            };
            let module_dir = self.root.join(&manifest.name);
            let output_dir = generated_root.join("service-artifacts").join(&instance.key);
            std::fs::create_dir_all(&output_dir)
                .with_context(|| format!("failed to create {}", output_dir.display()))?;
            for template in &manifest.runtime.templates {
                let src = module_dir.join(&template.src);
                let dst = output_dir.join(&template.dst);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                if src.exists() {
                    std::fs::copy(&src, &dst).with_context(|| {
                        format!("failed to copy {} to {}", src.display(), dst.display())
                    })?;
                    written.push(dst);
                }
            }
            let mut scripts = instance.provision_scripts.clone();
            scripts.extend(instance.validation_scripts.clone());
            for script in scripts {
                let src = module_dir.join(&script);
                let dst = output_dir.join(&script);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                if src.exists() {
                    std::fs::copy(&src, &dst).with_context(|| {
                        format!("failed to copy {} to {}", src.display(), dst.display())
                    })?;
                    written.push(dst);
                }
            }
        }
        Ok(written)
    }

    pub fn render_resource_artifacts(
        &self,
        generated_root: &Path,
        plan: &ServiceExecutionPlan,
    ) -> Result<Vec<PathBuf>> {
        let mut written = Vec::new();
        for instance in &plan.instances {
            let Some(target) = &instance.target else {
                continue;
            };
            let Some(manifest) = self.manifests.get(&instance.service) else {
                continue;
            };
            let service_dir = self.root.join(&manifest.name);
            let resource_dir = generated_root.join("resources").join(target);

            for template in &manifest.runtime.templates {
                let src = service_dir.join(&template.src);
                let dst = resource_dir.join(&template.dst);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                if src.exists() {
                    std::fs::copy(&src, &dst).with_context(|| {
                        format!("failed to copy {} to {}", src.display(), dst.display())
                    })?;
                    written.push(dst);
                }
            }

            let mut scripts = instance.provision_scripts.clone();
            scripts.extend(instance.validation_scripts.clone());
            for script in scripts {
                let src = service_dir.join(&script);
                let dst = resource_dir
                    .join("scripts")
                    .join(&instance.service)
                    .join(&script);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                if src.exists() {
                    std::fs::copy(&src, &dst).with_context(|| {
                        format!("failed to copy {} to {}", src.display(), dst.display())
                    })?;
                    written.push(dst);
                }
            }
        }
        Ok(written)
    }
}

fn load_manifest(path: &Path) -> Result<ServiceManifest> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: ServiceManifest =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
    validate_manifest(&manifest).with_context(|| format!("invalid service {}", path.display()))?;
    Ok(manifest)
}

fn validate_manifest(manifest: &ServiceManifest) -> Result<()> {
    if manifest.name.trim().is_empty() {
        bail!("service name cannot be empty");
    }
    if manifest.version.trim().is_empty() {
        bail!("service `{}` requires version", manifest.name);
    }
    if !matches!(manifest.scope.as_str(), "workspace" | "resource" | "host") {
        bail!(
            "service `{}` has invalid scope `{}`",
            manifest.name,
            manifest.scope
        );
    }
    let mut inputs = BTreeSet::new();
    for input in &manifest.inputs.schema {
        if !inputs.insert(input.key.as_str()) {
            bail!(
                "service `{}` defines duplicate input `{}`",
                manifest.name,
                input.key
            );
        }
    }
    Ok(())
}

fn resolve_inputs(
    manifest: &ServiceManifest,
    overrides: BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>> {
    let mut resolved = BTreeMap::new();
    for input in &manifest.inputs.schema {
        let value = overrides
            .get(&input.key)
            .cloned()
            .or_else(|| input.default.clone());
        let Some(value) = value else {
            if input.required {
                bail!("service `{}` requires input `{}`", manifest.name, input.key);
            }
            continue;
        };
        validate_input_value(manifest, input, &value)?;
        resolved.insert(input.key.clone(), value);
    }
    for key in overrides.keys() {
        if !manifest.inputs.schema.iter().any(|input| input.key == *key) {
            bail!("service `{}` does not define input `{key}`", manifest.name);
        }
    }
    Ok(resolved)
}

fn validate_input_value(
    manifest: &ServiceManifest,
    input: &InputSpec,
    value: &Value,
) -> Result<()> {
    let valid = match input.kind {
        InputKind::Bool => value.is_bool(),
        InputKind::String => value.is_str(),
        InputKind::U16 => value
            .as_integer()
            .is_some_and(|value| u16::try_from(value).is_ok()),
        InputKind::U32 => value
            .as_integer()
            .is_some_and(|value| u32::try_from(value).is_ok()),
        InputKind::I64 => value.is_integer(),
        InputKind::Array => value.is_array(),
        InputKind::Table => value.is_table(),
    };
    if !valid {
        bail!(
            "service `{}` input `{}` expected {:?}",
            manifest.name,
            input.key,
            input.kind
        );
    }
    if !input.allowed.is_empty() && !input.allowed.iter().any(|allowed| allowed == value) {
        bail!(
            "service `{}` input `{}` has unsupported value `{}`",
            manifest.name,
            input.key,
            value
        );
    }
    Ok(())
}

fn enabled_optional_dependencies(
    manifest: &ServiceManifest,
    selections: &BTreeMap<String, ServiceSelection>,
) -> Vec<String> {
    manifest
        .dependencies
        .optional
        .iter()
        .filter(|name| selections.get(*name).is_some_and(ServiceSelection::enabled))
        .cloned()
        .collect()
}

fn instance_key(manifest: &ServiceManifest, target: Option<&str>) -> String {
    match target {
        Some(target) => format!("{}__{}__{target}", manifest.scope, manifest.name),
        None => format!("{}__{}", manifest.scope, manifest.name),
    }
}

fn resource_services(resource: &Resource) -> Vec<String> {
    resource
        .features
        .values()
        .filter_map(Value::as_table)
        .filter_map(|feature| feature.get("services"))
        .filter_map(Value::as_array)
        .flat_map(|items| items.iter().filter_map(Value::as_str).map(str::to_string))
        .collect()
}

fn has_glob_meta(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

fn compose_command<'a>(
    program: &str,
    project_dir: &Path,
    prelude: impl IntoIterator<Item = &'a str>,
    args: impl IntoIterator<Item = &'a str>,
) -> RuntimeCommand {
    let mut command_args = prelude.into_iter().map(str::to_string).collect::<Vec<_>>();
    command_args.extend([
        "--project-directory".to_string(),
        project_dir.display().to_string(),
    ]);
    command_args.extend(args.into_iter().map(str::to_string));
    RuntimeCommand {
        program: program.to_string(),
        args: command_args,
    }
}

fn exec_command(program: &str, service: &str, args: &[&str]) -> RuntimeCommand {
    let mut command_args = vec!["exec".to_string(), service.to_string()];
    command_args.extend(args.iter().map(|arg| (*arg).to_string()));
    RuntimeCommand {
        program: program.to_string(),
        args: command_args,
    }
}

fn default_scope() -> String {
    "workspace".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_resolution_is_topologically_sorted_and_deduped() {
        let registry = ServiceRegistry::from_manifests(vec![
            manifest("app", &["runtime"], &[]),
            manifest("metrics", &["runtime"], &[]),
            manifest("runtime", &[], &[]),
        ])
        .unwrap();
        let selections = BTreeMap::from([
            ("app".to_string(), ServiceSelection::Enabled(true)),
            ("metrics".to_string(), ServiceSelection::Enabled(true)),
        ]);

        let plan = registry.build_plan(&selections, &[], None).unwrap();

        assert_eq!(
            plan.instances
                .iter()
                .map(|instance| instance.service.as_str())
                .collect::<Vec<_>>(),
            vec!["runtime", "app", "metrics"]
        );
    }

    #[test]
    fn cycles_fail_clearly() {
        let registry = ServiceRegistry::from_manifests(vec![
            manifest("a", &["b"], &[]),
            manifest("b", &["a"], &[]),
        ])
        .unwrap();
        let selections = BTreeMap::from([("a".to_string(), ServiceSelection::Enabled(true))]);

        let error = registry.build_plan(&selections, &[], None).unwrap_err();

        assert!(error.to_string().contains("cycle"));
    }

    #[test]
    fn optional_dependencies_are_included_only_when_enabled() {
        let registry = ServiceRegistry::from_manifests(vec![
            manifest("app", &[], &["cache"]),
            manifest("cache", &[], &[]),
        ])
        .unwrap();

        let disabled = registry
            .build_plan(
                &BTreeMap::from([("app".to_string(), ServiceSelection::Enabled(true))]),
                &[],
                None,
            )
            .unwrap();
        assert_eq!(disabled.instances.len(), 1);

        let enabled = registry
            .build_plan(
                &BTreeMap::from([
                    ("app".to_string(), ServiceSelection::Enabled(true)),
                    ("cache".to_string(), ServiceSelection::Enabled(true)),
                ]),
                &[],
                None,
            )
            .unwrap();
        assert_eq!(
            enabled
                .instances
                .iter()
                .map(|instance| instance.service.as_str())
                .collect::<Vec<_>>(),
            vec!["cache", "app"]
        );
    }

    #[test]
    fn resource_scoped_services_are_deduped_per_target_resource() {
        let mut service = manifest("jellyfin", &["container-runtime"], &[]);
        service.scope = "resource".to_string();
        let mut runtime = manifest("container-runtime", &[], &[]);
        runtime.scope = "resource".to_string();
        let registry = ServiceRegistry::from_manifests(vec![service, runtime]).unwrap();
        let resources = vec![
            resource_with_services("media-a", &["jellyfin"]),
            resource_with_services("media-b", &["jellyfin"]),
        ];

        let plan = registry
            .build_plan(&BTreeMap::new(), &resources, None)
            .unwrap();

        assert_eq!(
            plan.instances
                .iter()
                .map(|instance| instance.key.as_str())
                .collect::<Vec<_>>(),
            vec![
                "resource__container-runtime__media-a",
                "resource__jellyfin__media-a",
                "resource__container-runtime__media-b",
                "resource__jellyfin__media-b",
            ]
        );
    }

    #[test]
    fn service_inputs_resolve_defaults_and_validate_overrides() {
        let mut app = manifest("app", &[], &[]);
        app.inputs.schema = vec![InputSpec {
            key: "http_port".to_string(),
            kind: InputKind::U16,
            default: Some(Value::Integer(8096)),
            required: false,
            allowed: Vec::new(),
        }];
        let registry = ServiceRegistry::from_manifests(vec![app]).unwrap();

        let plan = registry
            .build_plan(
                &BTreeMap::from([(
                    "app".to_string(),
                    ServiceSelection::Config(BTreeMap::from([(
                        "http_port".to_string(),
                        Value::Integer(9096),
                    )])),
                )]),
                &[],
                None,
            )
            .unwrap();

        assert_eq!(
            plan.instances[0].outputs.get("http_port"),
            Some(&Value::Integer(9096))
        );
    }

    #[test]
    fn runtime_adapters_generate_engine_specific_commands() {
        let docker = runtime_for(&RuntimeConfig {
            engine: "docker".to_string(),
        })
        .unwrap();
        let podman = runtime_for(&RuntimeConfig {
            engine: "podman".to_string(),
        })
        .unwrap();

        assert_eq!(docker.compose_up(Path::new("/tmp/app")).program, "docker");
        assert_eq!(
            podman.compose_up(Path::new("/tmp/app")).args,
            vec![
                "compose".to_string(),
                "--project-directory".to_string(),
                "/tmp/app".to_string(),
                "up".to_string(),
                "-d".to_string()
            ]
        );
    }

    #[test]
    fn service_scripts_accept_single_paths_arrays_and_globs() {
        let root = unique_temp_dir();
        let app = root.join("app");
        std::fs::create_dir_all(app.join("scripts")).unwrap();
        std::fs::write(
            app.join("service.toml"),
            r#"
            name = "app"
            version = "1.0.0"
            scope = "resource"

            [scripts]
            provision = ["scripts/01.sh", "scripts/provision-*.sh"]
            validate = "scripts/validate.sh"
        "#,
        )
        .unwrap();
        std::fs::write(app.join("scripts/01.sh"), "").unwrap();
        std::fs::write(app.join("scripts/provision-02.sh"), "").unwrap();
        std::fs::write(app.join("scripts/validate.sh"), "").unwrap();

        let registry = ServiceRegistry::load(&root).unwrap();
        let plan = registry
            .build_plan(
                &BTreeMap::new(),
                &[resource_with_services("media-stack", &["app"])],
                None,
            )
            .unwrap();

        assert_eq!(
            plan.instances[0].provision_scripts,
            vec![
                "scripts/01.sh".to_string(),
                "scripts/provision-02.sh".to_string()
            ]
        );
        assert_eq!(
            plan.instances[0].validation_scripts,
            vec!["scripts/validate.sh".to_string()]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    fn manifest(name: &str, requires: &[&str], optional: &[&str]) -> ServiceManifest {
        ServiceManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            scope: "workspace".to_string(),
            targets: Vec::new(),
            inputs: InputSection::default(),
            dependencies: DependencySection {
                requires: requires.iter().map(|value| (*value).to_string()).collect(),
                optional: optional.iter().map(|value| (*value).to_string()).collect(),
            },
            runtime: RuntimeSection::default(),
            scripts: ScriptSection::default(),
            outputs: OutputSection::default(),
        }
    }

    fn resource_with_services(name: &str, services: &[&str]) -> Resource {
        Resource {
            name: name.to_string(),
            kind: "vm".to_string(),
            enabled: true,
            image: None,
            role: None,
            vmid: None,
            depends_on: Vec::new(),
            features: BTreeMap::from([(
                "media_services".to_string(),
                Value::Table(toml::map::Map::from_iter([(
                    "services".to_string(),
                    Value::Array(
                        services
                            .iter()
                            .map(|service| Value::String((*service).to_string()))
                            .collect(),
                    ),
                )])),
            )]),
            settings: BTreeMap::new(),
        }
    }

    fn unique_temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-services-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }
}
