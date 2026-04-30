use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use toml::Value;
use vmctl_config::Config;
use vmctl_domain::{DesiredState, Resource, ServiceInstancePlan};
pub use vmctl_hook_schema::{HookRefs, HookSection};
use vmctl_resources::{ResourceManifest, ResourceRegistry};
use vmctl_services::{ServiceManifest, ServiceRegistry};
use vmctl_util::command_runner::{self, CommandOptions, LogPrefix};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookModuleKind {
    Resource,
    Service,
}

#[derive(Debug, Clone)]
pub struct HookRunRequest {
    pub command: String,
    pub targets: Vec<String>,
    pub groups: Vec<String>,
    pub dry_run: bool,
    pub parallel: bool,
    pub continue_on_error: bool,
}

#[derive(Debug, Clone)]
pub struct HookRunReport {
    pub command: String,
    pub order: Vec<String>,
    pub executed: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HookExecutionPlan {
    pub command: String,
    pub nodes: Vec<HookNode>,
}

#[derive(Debug, Clone)]
pub struct HookNode {
    pub key: String,
    pub kind: HookModuleKind,
    pub name: String,
    pub target: Option<String>,
    pub scripts_root: PathBuf,
    pub scripts: Vec<String>,
    pub dependencies: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl HookNode {
    fn label(&self) -> String {
        match self.kind {
            HookModuleKind::Resource => format!("resource {}", self.name),
            HookModuleKind::Service => match &self.target {
                Some(target) => format!("service {}@{}", self.name, target),
                None => format!("service {}", self.name),
            },
        }
    }
}

pub fn run_hooks(
    request: HookRunRequest,
    config: &Config,
    desired: &DesiredState,
    resource_registry: &ResourceRegistry,
    service_registry: &ServiceRegistry,
) -> Result<HookRunReport> {
    let plan = build_hook_plan(
        &request,
        config,
        desired,
        resource_registry,
        service_registry,
    )?;
    if request.dry_run {
        print_hook_plan(&plan);
        return Ok(HookRunReport {
            command: request.command,
            order: plan.nodes.iter().map(|node| node.key.clone()).collect(),
            executed: Vec::new(),
        });
    }

    execute_hook_plan(&plan, request.parallel, request.continue_on_error)
}

pub fn build_hook_plan(
    request: &HookRunRequest,
    config: &Config,
    desired: &DesiredState,
    resource_registry: &ResourceRegistry,
    service_registry: &ServiceRegistry,
) -> Result<HookExecutionPlan> {
    let selectors = expand_selectors(&request.targets, &request.groups, &config.groups)?;
    let resources = build_resource_nodes(
        &request.command,
        config,
        desired,
        resource_registry,
        selectors.as_ref(),
    )?;
    let services = build_service_nodes(
        &request.command,
        config,
        desired,
        service_registry,
        selectors.as_ref(),
    )?;

    let mut nodes = resources;
    nodes.extend(services);
    nodes = select_hook_nodes(nodes, selectors.as_ref());
    let ordered = topo_sort_nodes(nodes)?;
    Ok(HookExecutionPlan {
        command: request.command.clone(),
        nodes: ordered,
    })
}

fn build_resource_nodes(
    command: &str,
    config: &Config,
    desired: &DesiredState,
    registry: &ResourceRegistry,
    _selectors: Option<&BTreeSet<String>>,
) -> Result<Vec<HookNode>> {
    let mut nodes = Vec::new();
    for resource in &desired.resources {
        let Some(manifest) = resource
            .role
            .as_deref()
            .and_then(|role| registry.manifest_for_role(role))
        else {
            continue;
        };
        let Some(hooks) = manifest.hooks.hook_refs(command) else {
            continue;
        };
        let scripts = hooks.resolve(&registry.root().join(&resource.name))?;
        if scripts.is_empty() {
            continue;
        }
        let mut env = build_resource_env(resource, config, desired, command, registry, manifest);
        let dependencies = resource_dependencies(desired, resource);
        env.insert("VMCTL_MODULE_KIND".to_string(), "resource".to_string());
        env.insert("VMCTL_MODULE_NAME".to_string(), resource.name.clone());
        nodes.push(HookNode {
            key: format!("resource::{}", resource.name),
            kind: HookModuleKind::Resource,
            name: resource.name.clone(),
            target: Some(resource.name.clone()),
            scripts_root: registry.root().join(&resource.name),
            scripts,
            dependencies,
            env,
        });
    }
    Ok(nodes)
}

fn build_service_nodes(
    command: &str,
    config: &Config,
    desired: &DesiredState,
    registry: &ServiceRegistry,
    _selectors: Option<&BTreeSet<String>>,
) -> Result<Vec<HookNode>> {
    let mut nodes = Vec::new();
    for instance in &desired.service_plan.instances {
        let Some(manifest) = registry.manifest(&instance.service) else {
            continue;
        };
        let Some(hooks) = manifest.hooks.hook_refs(command) else {
            continue;
        };
        let service_root = registry.module_root(&instance.service);
        let scripts = hooks.resolve(&service_root)?;
        if scripts.is_empty() {
            continue;
        }
        let mut env = build_service_env(instance, config, desired, command, registry, manifest);
        let dependencies = service_dependencies(desired, instance, registry)?;
        env.insert("VMCTL_MODULE_KIND".to_string(), "service".to_string());
        env.insert("VMCTL_MODULE_NAME".to_string(), instance.service.clone());
        nodes.push(HookNode {
            key: instance.key.clone(),
            kind: HookModuleKind::Service,
            name: instance.service.clone(),
            target: instance.target.clone(),
            scripts_root: service_root,
            scripts,
            dependencies,
            env,
        });
    }
    Ok(nodes)
}

fn execute_hook_plan(
    plan: &HookExecutionPlan,
    parallel: bool,
    continue_on_error: bool,
) -> Result<HookRunReport> {
    let nodes_by_key = plan
        .nodes
        .iter()
        .map(|node| (node.key.clone(), node.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut in_degree = plan
        .nodes
        .iter()
        .map(|node| {
            let deps = node
                .dependencies
                .iter()
                .filter(|dependency| nodes_by_key.contains_key(*dependency))
                .count();
            (node.key.clone(), deps)
        })
        .collect::<BTreeMap<_, _>>();
    let mut ready = plan
        .nodes
        .iter()
        .filter(|node| in_degree.get(&node.key).copied().unwrap_or_default() == 0)
        .map(|node| node.key.clone())
        .collect::<BTreeSet<_>>();
    let mut executed = Vec::new();
    let mut order = Vec::new();

    while !ready.is_empty() {
        let batch = ready.iter().cloned().collect::<Vec<_>>();
        ready.clear();
        if parallel && batch.len() > 1 {
            let mut handles = Vec::new();
            let mut batch_keys = Vec::new();
            for key in batch.iter().cloned() {
                let node = nodes_by_key
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing hook node `{key}`"))?;
                order.push(key.clone());
                batch_keys.push(key.clone());
                handles.push(std::thread::spawn(move || execute_node(&node)));
            }
            let mut failed = Vec::new();
            for (key, handle) in batch_keys.into_iter().zip(handles.into_iter()) {
                match handle.join().map_err(|_| anyhow!("hook thread panicked"))? {
                    Ok(()) => executed.push(key.clone()),
                    Err(error) => {
                        failed.push((key.clone(), error));
                    }
                }
            }
            if !failed.is_empty() && !continue_on_error {
                let (key, error) = &failed[0];
                return Err(anyhow!("hook execution failed for `{key}`: {error}"));
            }
            for key in batch {
                if let Some(node) = nodes_by_key.get(&key) {
                    for dependency in dependents_for(node, &plan.nodes) {
                        if let Some(entry) = in_degree.get_mut(&dependency) {
                            *entry = entry.saturating_sub(1);
                            if *entry == 0 {
                                ready.insert(dependency);
                            }
                        }
                    }
                }
            }
        } else {
            for key in batch {
                let node = nodes_by_key
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing hook node `{key}`"))?;
                order.push(key.clone());
                match execute_node(&node) {
                    Ok(()) => executed.push(key.clone()),
                    Err(error) => {
                        if !continue_on_error {
                            return Err(anyhow!("hook execution failed for `{key}`: {error}"));
                        }
                    }
                }
                for dependency in dependents_for(&node, &plan.nodes) {
                    if let Some(entry) = in_degree.get_mut(&dependency) {
                        *entry = entry.saturating_sub(1);
                        if *entry == 0 {
                            ready.insert(dependency);
                        }
                    }
                }
            }
        }
    }

    if executed.len() != plan.nodes.len() && !continue_on_error {
        let missing = plan
            .nodes
            .iter()
            .filter(|node| !executed.contains(&node.key))
            .map(|node| node.key.clone())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(anyhow!("failed to execute hooks: {}", missing.join(", ")));
        }
    }

    Ok(HookRunReport {
        command: plan.command.clone(),
        order,
        executed,
    })
}

fn execute_node(node: &HookNode) -> Result<()> {
    for script in &node.scripts {
        let script_path = resolve_script_path(node.scripts_root.join(script))?;
        ensure_executable(&script_path)?;
        eprintln!("[vmctl] hook {} -> {}", node.label(), script);
        let output = command_runner::run(
            CommandOptions::new(
                script_path.to_string_lossy().to_string(),
                std::iter::empty::<&str>(),
            )
            .cwd(&node.scripts_root)
            .envs(
                node.env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            )
            .prefix(LogPrefix::Vmctl)
            .timeout(std::time::Duration::from_secs(3600))
            .stream(true),
        )
        .with_context(|| format!("failed to run hook `{script}` for {}", node.label()))?;
        if !output.stderr.trim().is_empty() {
            eprintln!("{}", output.stderr.trim());
        }
    }
    Ok(())
}

fn resolve_script_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    Ok(cwd.join(path))
}

fn ensure_executable(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to read hook {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("hook {} is not executable", path.display());
        }
    }
    Ok(())
}

fn print_hook_plan(plan: &HookExecutionPlan) {
    println!("vmctl run {}", plan.command);
    for node in &plan.nodes {
        println!("- {}: {}", node.label(), node.scripts.join(", "));
    }
}

fn topo_sort_nodes(nodes: Vec<HookNode>) -> Result<Vec<HookNode>> {
    let mut nodes_by_key = nodes
        .into_iter()
        .map(|node| (node.key.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let mut in_degree = BTreeMap::<String, usize>::new();
    let mut outgoing = BTreeMap::<String, BTreeSet<String>>::new();
    for (key, node) in &nodes_by_key {
        let mut count = 0;
        for dependency in &node.dependencies {
            if nodes_by_key.contains_key(dependency) {
                count += 1;
                outgoing
                    .entry(dependency.clone())
                    .or_default()
                    .insert(key.clone());
            }
        }
        in_degree.insert(key.clone(), count);
    }
    let mut ready = in_degree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(key, _)| key.clone())
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::new();
    while let Some(key) = ready.iter().next().cloned() {
        ready.remove(&key);
        ordered.push(
            nodes_by_key
                .remove(&key)
                .ok_or_else(|| anyhow!("missing hook node `{key}`"))?,
        );
        if let Some(dependents) = outgoing.get(&key) {
            for dependent in dependents {
                let entry = in_degree
                    .get_mut(dependent)
                    .ok_or_else(|| anyhow!("missing in-degree for `{dependent}`"))?;
                *entry = entry.saturating_sub(1);
                if *entry == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
    }
    if !nodes_by_key.is_empty() {
        let cycle = find_cycle(&nodes_by_key);
        if cycle.is_empty() {
            bail!("dependency cycle detected");
        }
        bail!("dependency cycle detected: {}", cycle.join(" -> "));
    }
    Ok(ordered)
}

fn find_cycle(nodes_by_key: &BTreeMap<String, HookNode>) -> Vec<String> {
    fn visit(
        key: &str,
        nodes_by_key: &BTreeMap<String, HookNode>,
        visiting: &mut Vec<String>,
        seen: &mut BTreeSet<String>,
    ) -> Option<Vec<String>> {
        if let Some(index) = visiting.iter().position(|item| item == key) {
            let mut cycle = visiting[index..].to_vec();
            cycle.push(key.to_string());
            return Some(cycle);
        }
        if !seen.insert(key.to_string()) {
            return None;
        }
        visiting.push(key.to_string());
        let node = nodes_by_key.get(key)?;
        for dependency in &node.dependencies {
            if !nodes_by_key.contains_key(dependency) {
                continue;
            }
            if let Some(cycle) = visit(dependency, nodes_by_key, visiting, seen) {
                return Some(cycle);
            }
        }
        visiting.pop();
        None
    }

    let mut visiting = Vec::new();
    let mut seen = BTreeSet::new();
    for key in nodes_by_key.keys() {
        if let Some(cycle) = visit(key, nodes_by_key, &mut visiting, &mut seen) {
            return cycle;
        }
    }
    Vec::new()
}

fn dependents_for(node: &HookNode, nodes: &[HookNode]) -> Vec<String> {
    nodes
        .iter()
        .filter(|candidate| {
            candidate
                .dependencies
                .iter()
                .any(|dependency| dependency == &node.key)
        })
        .map(|candidate| candidate.key.clone())
        .collect()
}

fn expand_selectors(
    targets: &[String],
    groups: &[String],
    configured_groups: &BTreeMap<String, Vec<String>>,
) -> Result<Option<BTreeSet<String>>> {
    let mut selectors = BTreeSet::new();
    for target in targets {
        selectors.insert(target.clone());
    }
    for group in groups {
        expand_group(
            group,
            configured_groups,
            &mut selectors,
            &mut BTreeSet::new(),
        )?;
    }
    if selectors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(selectors))
    }
}

fn expand_group(
    group: &str,
    configured_groups: &BTreeMap<String, Vec<String>>,
    selectors: &mut BTreeSet<String>,
    stack: &mut BTreeSet<String>,
) -> Result<()> {
    if !stack.insert(group.to_string()) {
        bail!("cyclic group reference detected at `{group}`");
    }
    let Some(members) = configured_groups.get(group) else {
        selectors.insert(group.to_string());
        stack.remove(group);
        return Ok(());
    };
    for member in members {
        if configured_groups.contains_key(member) {
            expand_group(member, configured_groups, selectors, stack)?;
        } else {
            selectors.insert(member.clone());
        }
    }
    stack.remove(group);
    Ok(())
}

fn select_hook_nodes(nodes: Vec<HookNode>, selectors: Option<&BTreeSet<String>>) -> Vec<HookNode> {
    let Some(selectors) = selectors else {
        return nodes;
    };
    let nodes_by_key = nodes
        .iter()
        .map(|node| (node.key.clone(), node.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeSet::new();
    let mut stack = Vec::new();
    for node in &nodes {
        if node_matches_selectors(node, selectors) {
            stack.push(node.key.clone());
        }
    }
    while let Some(key) = stack.pop() {
        if !selected.insert(key.clone()) {
            continue;
        }
        let Some(node) = nodes_by_key.get(&key) else {
            continue;
        };
        for dependency in &node.dependencies {
            if nodes_by_key.contains_key(dependency) {
                stack.push(dependency.clone());
            }
        }
    }
    nodes
        .into_iter()
        .filter(|node| selected.contains(&node.key))
        .collect()
}

fn node_matches_selectors(node: &HookNode, selectors: &BTreeSet<String>) -> bool {
    match node.kind {
        HookModuleKind::Resource => selectors.contains(&node.name),
        HookModuleKind::Service => {
            selectors.contains(&node.name)
                || selectors.contains(&node.key)
                || node
                    .target
                    .as_ref()
                    .is_some_and(|target| selectors.contains(target))
        }
    }
}

fn resource_dependencies(desired: &DesiredState, resource: &Resource) -> Vec<String> {
    let mut deps = resource.depends_on.clone();
    if let Some(expansion) = desired.expansions.get(&resource.name) {
        deps.extend(expansion.dependencies.clone());
    }
    deps.sort();
    deps.dedup();
    deps.into_iter()
        .map(|dependency| format!("resource::{dependency}"))
        .collect()
}

fn service_dependencies(
    desired: &DesiredState,
    instance: &ServiceInstancePlan,
    registry: &ServiceRegistry,
) -> Result<Vec<String>> {
    let mut deps = instance.required_dependencies.clone();
    deps.extend(instance.optional_dependencies.clone());
    deps.sort();
    deps.dedup();
    let lookup = service_instance_lookup(desired, registry)?;
    let mut keys = Vec::new();
    for dependency in deps {
        if let Some(key) = lookup.get(&(dependency.clone(), instance.target.clone())) {
            keys.push(key.clone());
        } else if let Some(key) = lookup.get(&(dependency, None)) {
            keys.push(key.clone());
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

fn service_instance_lookup(
    desired: &DesiredState,
    registry: &ServiceRegistry,
) -> Result<HashMap<(String, Option<String>), String>> {
    let mut lookup = HashMap::new();
    for instance in &desired.service_plan.instances {
        let Some(manifest) = registry.manifest(&instance.service) else {
            continue;
        };
        let target = if manifest.scope == "resource" {
            instance.target.clone()
        } else {
            None
        };
        lookup.insert((instance.service.clone(), target), instance.key.clone());
    }
    Ok(lookup)
}

fn build_resource_env(
    resource: &Resource,
    config: &Config,
    desired: &DesiredState,
    command: &str,
    registry: &ResourceRegistry,
    manifest: &ResourceManifest,
) -> BTreeMap<String, String> {
    let mut env = base_env(desired);
    let resource_env = resource_env_map(resource);
    env.extend(flatten_toml_map("", &config.consts));
    env.extend(flatten_toml_map("", &config.env));
    env.extend(flatten_toml_map("VMCTL_CONST", &config.consts));
    env.extend(flatten_toml_map("VMCTL_ENV", &config.env));
    env.extend(flatten_toml_map("", &resource_env));
    env.extend(flatten_toml_map("VMCTL_RESOURCE_ENV", &resource_env));
    env.insert("VMCTL_COMMAND".to_string(), command.to_string());
    env.insert("VMCTL_RESOURCE_NAME".to_string(), resource.name.clone());
    env.insert("VMCTL_RESOURCE_KIND".to_string(), resource.kind.clone());
    env.insert(
        "VMCTL_RESOURCE_ROLE".to_string(),
        resource.role.clone().unwrap_or_default(),
    );
    env.insert(
        "VMCTL_RESOURCE_VMID".to_string(),
        resource
            .vmid
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    env.insert("VMCTL_HOST_SHORT".to_string(), resource.name.clone());
    env.insert(
        "VMCTL_HTTP_BASE_URL_SHORT".to_string(),
        format!("http://{}", resource.name),
    );
    if let Some(target_service_urls) = service_urls_for_target(desired, &resource.name) {
        env.extend(target_service_urls);
    }
    env.extend(flatten_value_map(
        "VMCTL_RESOURCE",
        &serde_json::to_value(resource).unwrap_or_default(),
    ));
    env.extend(flatten_value_map(
        "VMCTL_RESOURCE_MANIFEST",
        &serde_json::to_value(manifest).unwrap_or_default(),
    ));
    env.insert(
        "VMCTL_RESOURCE_SCRIPT_ROOT".to_string(),
        registry
            .root()
            .join(&resource.name)
            .join("hooks")
            .to_string_lossy()
            .to_string(),
    );
    env
}

fn build_service_env(
    instance: &ServiceInstancePlan,
    config: &Config,
    desired: &DesiredState,
    command: &str,
    registry: &ServiceRegistry,
    manifest: &ServiceManifest,
) -> BTreeMap<String, String> {
    let mut env = base_env(desired);
    env.extend(flatten_toml_map("", &config.consts));
    env.extend(flatten_toml_map("", &config.env));
    env.extend(flatten_toml_map("VMCTL_CONST", &config.consts));
    env.extend(flatten_toml_map("VMCTL_ENV", &config.env));
    env.insert("VMCTL_COMMAND".to_string(), command.to_string());
    env.insert("VMCTL_SERVICE_NAME".to_string(), instance.service.clone());
    env.insert("VMCTL_SERVICE_KEY".to_string(), instance.key.clone());
    env.insert("VMCTL_SERVICE_SCOPE".to_string(), instance.scope.clone());
    env.insert(
        "VMCTL_RUNTIME_ENGINE".to_string(),
        instance.runtime_engine.clone(),
    );
    env.insert(
        "VMCTL_SERVICE_TARGET".to_string(),
        instance.target.clone().unwrap_or_default(),
    );
    env.insert("VMCTL_MODULE_NAME".to_string(), instance.service.clone());
    env.insert("VMCTL_MODULE_KIND".to_string(), "service".to_string());
    if let Some(target) = &instance.target {
        env.insert("VMCTL_HOST_SHORT".to_string(), target.clone());
        env.insert(
            "VMCTL_HTTP_BASE_URL_SHORT".to_string(),
            format!("http://{}", target),
        );
        if let Some(target_env) = service_urls_for_target(desired, target) {
            env.extend(target_env);
        }
    }
    env.extend(flatten_value_map(
        &format!("{}_OUTPUT", upper_name(&instance.service)),
        &serde_json::to_value(&instance.outputs).unwrap_or_default(),
    ));
    env.extend(flatten_value_map(
        &format!("{}_MANIFEST", upper_name(&instance.service)),
        &serde_json::to_value(manifest).unwrap_or_default(),
    ));
    env.insert(
        "VMCTL_SERVICE_SCRIPT_ROOT".to_string(),
        registry
            .root()
            .join(&instance.service)
            .to_string_lossy()
            .to_string(),
    );
    env.extend(flatten_toml_map("", &manifest.env));
    env.extend(flatten_toml_map("VMCTL_SERVICE_ENV", &manifest.env));
    if let Some(port) = instance
        .outputs
        .get("http_port")
        .and_then(Value::as_integer)
    {
        env.insert(
            format!("{}_HTTP_PORT", upper_name(&instance.service)),
            port.to_string(),
        );
        if let Some(target) = &instance.target {
            env.insert(
                format!("{}_URL", upper_name(&instance.service)),
                format!(
                    "http://{}:{}{}",
                    target,
                    port,
                    instance
                        .outputs
                        .get("base_url")
                        .and_then(Value::as_str)
                        .unwrap_or("/")
                ),
            );
        }
    }
    env
}

fn resource_env_map(resource: &Resource) -> BTreeMap<String, Value> {
    resource
        .settings
        .get("env")
        .and_then(Value::as_table)
        .map(|table| {
            table
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .or_else(|| {
            resource
                .settings
                .get("resource")
                .and_then(Value::as_table)
                .and_then(|resource_table| resource_table.get("env"))
                .and_then(Value::as_table)
                .map(|table| {
                    table
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<BTreeMap<_, _>>()
                })
        })
        .unwrap_or_default()
}

fn service_urls_for_target(
    desired: &DesiredState,
    target: &str,
) -> Option<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    let mut found = false;
    for instance in &desired.service_plan.instances {
        if instance.target.as_deref() != Some(target) {
            continue;
        }
        if let Some(url) = service_url(instance) {
            env.insert(format!("{}_URL", upper_name(&instance.service)), url);
            found = true;
        }
    }
    if found {
        Some(env)
    } else {
        None
    }
}

fn service_url(instance: &ServiceInstancePlan) -> Option<String> {
    let port = instance
        .outputs
        .get("http_port")
        .and_then(Value::as_integer)?;
    let base_url = instance
        .outputs
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("/");
    Some(format!(
        "http://{}:{}{}",
        instance.target.as_deref().unwrap_or(&instance.service),
        port,
        base_url
    ))
}

fn base_env(desired: &DesiredState) -> BTreeMap<String, String> {
    let mut env = std::env::vars().collect::<BTreeMap<_, _>>();
    env.insert(
        "VMCTL_RUNTIME_ENGINE".to_string(),
        desired.runtime.engine.clone(),
    );
    env
}

fn flatten_toml_map(prefix: &str, map: &BTreeMap<String, Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let next_prefix = if prefix.is_empty() {
            upper_name(key)
        } else {
            format!("{prefix}_{}", upper_name(key))
        };
        flatten_toml_value(&next_prefix, value, &mut out);
    }
    out
}

fn flatten_value_map(prefix: &str, value: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    flatten_json_value(prefix, value, &mut out);
    out
}

fn flatten_toml_value(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Table(table) => {
            for (key, nested) in table {
                let next_prefix = if prefix.is_empty() {
                    upper_name(key)
                } else {
                    format!("{prefix}_{}", upper_name(key))
                };
                flatten_toml_value(&next_prefix, nested, out);
            }
        }
        Value::Array(items) => {
            out.insert(
                prefix.to_string(),
                items
                    .iter()
                    .map(toml_value_text)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        _ => {
            out.insert(prefix.to_string(), toml_value_text(value));
        }
    }
}

fn flatten_json_value(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                let next_prefix = if prefix.is_empty() {
                    upper_name(key)
                } else {
                    format!("{prefix}_{}", upper_name(key))
                };
                flatten_json_value(&next_prefix, nested, out);
            }
        }
        serde_json::Value::Array(items) => {
            out.insert(
                prefix.to_string(),
                items
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        _ => {
            out.insert(
                prefix.to_string(),
                value
                    .as_str()
                    .map(|text| text.to_string())
                    .unwrap_or_else(|| value.to_string()),
            );
        }
    }
}

fn toml_value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Boolean(value) => value.to_string(),
        Value::Datetime(value) => value.to_string(),
        _ => value.to_string(),
    }
}

fn upper_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' => ch.to_ascii_uppercase(),
            'A'..='Z' | '0'..='9' => ch,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn build_hook_plan_expands_groups_and_orders_dependencies() {
        let root = temp_root();
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("a/scripts")).unwrap();
        std::fs::create_dir_all(resources_root.join("b/scripts")).unwrap();
        std::fs::create_dir_all(resources_root.join("c/scripts")).unwrap();

        write_resource(
            &resources_root.join("a/resource.toml"),
            r#"
            name = "a"
            kind = "vm"
            role = "a_role"

            [hooks]
            bootstrap = "scripts/bootstrap.sh"
            "#,
        );
        write_resource(
            &resources_root.join("b/resource.toml"),
            r#"
            name = "b"
            kind = "vm"
            role = "b_role"
            depends_on = ["a"]

            [hooks]
            bootstrap = "scripts/bootstrap.sh"
            "#,
        );
        write_resource(
            &resources_root.join("c/resource.toml"),
            r#"
            name = "c"
            kind = "vm"
            role = "c_role"
            depends_on = ["a"]

            [hooks]
            bootstrap = "scripts/bootstrap.sh"
            "#,
        );

        std::fs::write(
            resources_root.join("a/scripts/bootstrap.sh"),
            "#!/usr/bin/env bash\n",
        )
        .unwrap();
        std::fs::write(
            resources_root.join("b/scripts/bootstrap.sh"),
            "#!/usr/bin/env bash\n",
        )
        .unwrap();
        std::fs::write(
            resources_root.join("c/scripts/bootstrap.sh"),
            "#!/usr/bin/env bash\n",
        )
        .unwrap();
        make_executable(&resources_root.join("a/scripts/bootstrap.sh"));
        make_executable(&resources_root.join("b/scripts/bootstrap.sh"));
        make_executable(&resources_root.join("c/scripts/bootstrap.sh"));

        let resource_registry = ResourceRegistry::load(&resources_root, &services_root).unwrap();
        let desired = DesiredState {
            resources: resource_registry.resources().to_vec(),
            expansions: resource_registry
                .resources()
                .iter()
                .map(|resource| (resource.name.clone(), vmctl_domain::Expansion::default()))
                .collect(),
            ..DesiredState::default()
        };
        let config = Config {
            backend: vmctl_domain::BackendConfig::default(),
            runtime: vmctl_domain::RuntimeConfig::default(),
            services: BTreeMap::new(),
            defaults: BTreeMap::new(),
            consts: BTreeMap::new(),
            env: BTreeMap::new(),
            groups: BTreeMap::from([("pair".to_string(), vec!["b".to_string(), "c".to_string()])]),
            sources: Default::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
        };
        let service_registry = ServiceRegistry::load(&services_root).unwrap();

        let plan = build_hook_plan(
            &HookRunRequest {
                command: "bootstrap".to_string(),
                targets: Vec::new(),
                groups: vec!["pair".to_string()],
                dry_run: true,
                parallel: false,
                continue_on_error: false,
            },
            &config,
            &desired,
            &resource_registry,
            &service_registry,
        )
        .unwrap();

        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_hooks_executes_each_dependency_once_in_order() {
        let root = temp_root();
        let resources_root = root.join("resources");
        let services_root = root.join("services");
        std::fs::create_dir_all(resources_root.join("a/scripts")).unwrap();
        std::fs::create_dir_all(resources_root.join("b/scripts")).unwrap();
        std::fs::create_dir_all(resources_root.join("c/scripts")).unwrap();

        write_resource(
            &resources_root.join("a/resource.toml"),
            r#"
            name = "a"
            kind = "vm"
            role = "a_role"

            [hooks]
            bootstrap = "scripts/bootstrap.sh"
            "#,
        );
        write_resource(
            &resources_root.join("b/resource.toml"),
            r#"
            name = "b"
            kind = "vm"
            role = "b_role"
            depends_on = ["a"]

            [hooks]
            bootstrap = "scripts/bootstrap.sh"
            "#,
        );
        write_resource(
            &resources_root.join("c/resource.toml"),
            r#"
            name = "c"
            kind = "vm"
            role = "c_role"
            depends_on = ["a"]

            [hooks]
            bootstrap = "scripts/bootstrap.sh"
            "#,
        );

        let log_path = root.join("hooks.log");
        for name in ["a", "b", "c"] {
            let script = resources_root.join(name).join("scripts/bootstrap.sh");
            std::fs::write(
                &script,
                r#"#!/usr/bin/env bash
set -euo pipefail
echo "${VMCTL_RESOURCE_NAME}:${VMCTL_COMMAND}" >> "$HOOK_LOG_PATH"
"#,
            )
            .unwrap();
            make_executable(&script);
        }

        let resource_registry = ResourceRegistry::load(&resources_root, &services_root).unwrap();
        let desired = DesiredState {
            resources: resource_registry.resources().to_vec(),
            expansions: resource_registry
                .resources()
                .iter()
                .map(|resource| (resource.name.clone(), vmctl_domain::Expansion::default()))
                .collect(),
            ..DesiredState::default()
        };
        let config = Config {
            backend: vmctl_domain::BackendConfig::default(),
            runtime: vmctl_domain::RuntimeConfig::default(),
            services: BTreeMap::new(),
            defaults: BTreeMap::new(),
            consts: BTreeMap::new(),
            env: BTreeMap::from([(
                "HOOK_LOG_PATH".to_string(),
                Value::String(log_path.to_string_lossy().to_string()),
            )]),
            groups: BTreeMap::new(),
            sources: Default::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
        };
        let service_registry = ServiceRegistry::load(&services_root).unwrap();

        let report = run_hooks(
            HookRunRequest {
                command: "bootstrap".to_string(),
                targets: Vec::new(),
                groups: Vec::new(),
                dry_run: false,
                parallel: false,
                continue_on_error: false,
            },
            &config,
            &desired,
            &resource_registry,
            &service_registry,
        )
        .unwrap();

        assert_eq!(
            report.executed,
            vec!["resource::a", "resource::b", "resource::c"]
        );
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            vec!["a:bootstrap", "b:bootstrap", "c:bootstrap"]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    fn write_resource(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn temp_root() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-hooks-runner-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }
}
