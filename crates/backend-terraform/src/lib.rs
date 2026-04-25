use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;
use vmctl_backend::{
    ApplyResult, BackendPlan, BackendValidation, EngineBackend, PlanMode, RenderResult,
    TargetSelector,
};
use vmctl_domain::{DesiredState, ImageSource, NormalizedResource, Resource, Workspace};
use vmctl_packs::PackRegistry;
use vmctl_util::command_runner::{self, CommandOptions, LogPrefix};

#[derive(Debug, Default)]
pub struct TerraformBackend;

impl TerraformBackend {
    pub fn render_for_plan(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &PackRegistry,
        mode: PlanMode,
    ) -> Result<RenderResult> {
        render_workspace(workspace, desired, registry, mode == PlanMode::Online)
    }

    pub fn apply_with_output(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &PackRegistry,
        verbose: bool,
    ) -> Result<ApplyResult> {
        self.apply_with_output_refresh(workspace, desired, registry, verbose, true)
    }

    pub fn apply_with_output_refresh(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &PackRegistry,
        verbose: bool,
        refresh: bool,
    ) -> Result<ApplyResult> {
        self.apply_with_output_refresh_target(workspace, desired, registry, verbose, refresh, None)
    }

    pub fn apply_with_output_refresh_target(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &PackRegistry,
        verbose: bool,
        refresh: bool,
        target: Option<&str>,
    ) -> Result<ApplyResult> {
        self.render(workspace, desired, registry)?;
        run_terraform(workspace, &["init", "-input=false"])?;
        let args = terraform_apply_args(refresh, target);
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = run_terraform_with_options(workspace, &arg_refs, verbose)?;
        Ok(ApplyResult {
            summary: output_summary("terraform apply", &output),
        })
    }
}

fn terraform_apply_args(refresh: bool, target: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "apply".to_string(),
        "-auto-approve".to_string(),
        "-input=false".to_string(),
        "-no-color".to_string(),
    ];
    if !refresh {
        args.push("-refresh=false".to_string());
    }
    if let Some(target) = target {
        args.push(format!("-target=module.{}", sanitize_module_name(target)));
    }
    args
}

impl EngineBackend for TerraformBackend {
    fn validate_backend(&self, _workspace: &Workspace) -> Result<()> {
        if vmctl_util::command_exists("tofu") || vmctl_util::command_exists("terraform") {
            println!("backend: terraform");
            println!("binary: found tofu/terraform");
        } else {
            println!("backend: terraform");
            println!("binary: missing tofu/terraform");
        }
        Ok(())
    }

    fn render(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &PackRegistry,
    ) -> Result<RenderResult> {
        render_workspace(workspace, desired, registry, true)
    }

    fn plan(
        &self,
        workspace: &Workspace,
        _desired: &DesiredState,
        mode: PlanMode,
    ) -> Result<BackendPlan> {
        run_terraform(workspace, &["init", "-input=false"])?;
        if mode == PlanMode::DryRun {
            run_terraform(workspace, &["validate", "-no-color"])?;
        }
        let output = match mode {
            PlanMode::Online => run_terraform(workspace, &["plan", "-input=false", "-no-color"])?,
            PlanMode::DryRun => run_terraform(
                workspace,
                &["plan", "-refresh=false", "-input=false", "-no-color"],
            )?,
        };
        Ok(BackendPlan {
            summary: plan_output(
                match mode {
                    PlanMode::Online => "tofu plan",
                    PlanMode::DryRun => "tofu dry-run plan",
                },
                &output,
            ),
        })
    }

    fn validate_rendered(&self, workspace: &Workspace) -> Result<BackendValidation> {
        run_terraform(workspace, &["init", "-input=false"])?;
        let output = run_terraform(workspace, &["validate", "-no-color"])?;
        Ok(BackendValidation {
            summary: output_summary("terraform validate", &output),
        })
    }

    fn apply(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &PackRegistry,
    ) -> Result<ApplyResult> {
        self.apply_with_output(workspace, desired, registry, false)
    }

    fn destroy(&self, workspace: &Workspace, target: &TargetSelector) -> Result<ApplyResult> {
        run_terraform(workspace, &["init", "-input=false"])?;
        let target_arg = format!("-target=module.{}", target.name.replace('-', "_"));
        let output = run_terraform(
            workspace,
            &[
                "destroy",
                "-auto-approve",
                "-input=false",
                "-no-color",
                &target_arg,
            ],
        )?;
        Ok(ApplyResult {
            summary: output_summary("terraform destroy", &output),
        })
    }
}

fn render_workspace(
    workspace: &Workspace,
    desired: &DesiredState,
    registry: &PackRegistry,
    include_proxmox_resources: bool,
) -> Result<RenderResult> {
    if include_proxmox_resources {
        validate_live_inputs(desired)?;
    }

    let generated = workspace.root.join(&workspace.generated_dir);
    prepare_generated_workspace(&generated)?;

    let mut files = Vec::new();
    write_json(
        &generated.join("desired-state.json"),
        &redacted_value(json!(desired)),
        &mut files,
    )?;
    write_json(
        &generated.join("terraform.tfvars.json"),
        &redacted_value(json!({
            "backend": desired.backend,
            "resources": desired.resources,
            "normalized_resources": desired.normalized_resources,
            "expansions": desired.expansions,
        })),
        &mut files,
    )?;
    write_json(
        &generated.join("variables.tf.json"),
        &variables_json(),
        &mut files,
    )?;
    if include_proxmox_resources {
        write_json(
            &generated.join("provider.tf.json"),
            &provider_json(desired),
            &mut files,
        )?;
    } else {
        let marker = generated.join("DRY_RUN_VALIDATION_ONLY.txt");
        std::fs::write(
            &marker,
            "This workspace was rendered for vmctl backend plan --dry-run.\nIt intentionally omits live Proxmox provider resources and must not be applied.\n",
        )?;
        files.push(marker);
    }
    write_json(
        &generated.join("main.tf.json"),
        &main_json(desired, include_proxmox_resources),
        &mut files,
    )?;
    write_json(
        &generated.join("outputs.tf.json"),
        &outputs_json(),
        &mut files,
    )?;
    files.extend(write_base_modules(&generated, include_proxmox_resources)?);

    files.extend(registry.render_artifacts(&generated, &desired.resources, &desired.expansions)?);

    Ok(RenderResult {
        summary: format!("rendered {} files to {}", files.len(), generated.display()),
        files,
    })
}

fn prepare_generated_workspace(generated: &Path) -> Result<()> {
    std::fs::create_dir_all(generated)?;
    for relative in [
        "desired-state.json",
        "terraform.tfvars.json",
        "variables.tf.json",
        "provider.tf.json",
        "main.tf.json",
        "outputs.tf.json",
        "DRY_RUN_VALIDATION_ONLY.txt",
        "modules",
        "resources",
    ] {
        let path = generated.join(relative);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        } else if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn validate_live_inputs(desired: &DesiredState) -> Result<()> {
    let proxmox = desired
        .backend
        .settings
        .get("proxmox")
        .and_then(toml::Value::as_table);
    let endpoint = proxmox
        .and_then(|settings| settings.get("endpoint"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let default_node = proxmox
        .and_then(|settings| settings.get("node"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();

    if endpoint.trim().is_empty() {
        bail!("live Terraform backend requires backend.proxmox.endpoint");
    }
    if default_node.trim().is_empty() {
        bail!("live Terraform backend requires backend.proxmox.node or per-resource node");
    }

    for resource in &desired.resources {
        let normalized = desired
            .normalized_resources
            .get(&resource.name)
            .cloned()
            .unwrap_or_else(|| normalize_fallback(resource));
        let node = normalized.node.as_deref().unwrap_or(default_node);
        if node.trim().is_empty() {
            bail!("resource `{}` requires a Proxmox node", resource.name);
        }
        if normalized.vmid.is_none() {
            bail!(
                "resource `{}` requires vmid for live operations",
                resource.name
            );
        }
        if normalized
            .storage
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            bail!(
                "resource `{}` requires storage for live operations",
                resource.name
            );
        }
        if normalized
            .bridge
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            bail!(
                "resource `{}` requires bridge for live operations",
                resource.name
            );
        }
        let template = normalized.template.as_deref().unwrap_or_default();
        if template.trim().is_empty() {
            bail!(
                "resource `{}` requires template for live operations",
                resource.name
            );
        }
        if normalized.kind == "vm" && normalized.clone_vmid.is_none() && resource.image.is_none() {
            bail!(
                "vm resource `{}` requires clone_vmid or image for live operations",
                resource.name
            );
        }
        validate_vm_preflight(&normalized)?;
    }

    Ok(())
}

fn validate_vm_preflight(resource: &NormalizedResource) -> Result<()> {
    if resource.kind != "vm" {
        return Ok(());
    }

    if resource.machine.as_deref() == Some("q35") && vmctl_util::command_exists("qm") {
        let q35_config = Path::new("/usr/share/qemu-server/pve-q35.cfg");
        if !q35_config.exists() {
            bail!(
                "vm resource `{}` requests machine=q35, but local Proxmox q35 config was not found at {}. Enable q35 support on the Proxmox host or use machine=\"i440fx\".",
                resource.name,
                q35_config.display()
            );
        }
    }

    let iothread = resource.iothread.unwrap_or(true);
    let raw_disk_interface = resource
        .disk_interface
        .as_deref()
        .unwrap_or(default_vm_disk_interface());
    let Some(disk_interface) = canonical_vm_disk_interface(raw_disk_interface) else {
        bail!(
            "vm resource `{}` sets disk_interface={raw_disk_interface}; expected slot syntax like virtio0/scsi0/sata0/ide0",
            resource.name
        );
    };
    if iothread && !iothread_compatible_disk(disk_interface) {
        bail!(
            "vm resource `{}` sets iothread=true with disk_interface={disk_interface}; use a virtio disk interface or set iothread=false",
            resource.name
        );
    }

    if let Some(cloud_init) = &resource.cloud_init {
        if cloud_init
            .ssh_key_file
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            bail!(
                "vm resource `{}` cloud_init requires ssh_key_file before apply",
                resource.name
            );
        }
    }

    Ok(())
}

fn iothread_compatible_disk(interface: &str) -> bool {
    interface.starts_with("virtio") || interface == "virtio-scsi-single"
}

fn default_vm_disk_interface() -> &'static str {
    "virtio0"
}

fn canonical_vm_disk_interface(value: &str) -> Option<&str> {
    let value = value.trim();
    let prefixes = ["virtio", "scsi", "sata", "ide"];
    let prefix = prefixes.iter().find(|prefix| value.starts_with(**prefix))?;
    let suffix = &value[prefix.len()..];
    if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(value)
}

fn write_json<T: serde::Serialize>(
    path: &PathBuf,
    value: &T,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value)?)?;
    files.push(path.clone());
    Ok(())
}

fn variables_json() -> serde_json::Value {
    json!({
        "variable": {
            "backend": {
                "type": "any",
                "description": "Resolved vmctl backend configuration."
            },
            "resources": {
                "type": "any",
                "description": "Normalized vmctl resources."
            },
            "expansions": {
                "type": "any",
                "description": "Expanded pack outputs keyed by resource name."
            },
            "normalized_resources": {
                "type": "any",
                "description": "Normalized vmctl resources keyed by resource name."
            },
            "proxmox_api_token": {
                "type": "string",
                "description": "Proxmox API token in USER@REALM!TOKENID=SECRET format. Prefer TF_VAR_proxmox_api_token.",
                "sensitive": true,
                "nullable": true,
                "default": null
            }
        }
    })
}

fn provider_json(desired: &DesiredState) -> serde_json::Value {
    let proxmox = desired
        .backend
        .settings
        .get("proxmox")
        .and_then(toml::Value::as_table);
    let endpoint = proxmox
        .and_then(|settings| settings.get("endpoint"))
        .and_then(toml::Value::as_str)
        .unwrap_or("");
    let tls_insecure = proxmox
        .and_then(|settings| settings.get("tls_insecure"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);

    json!({
        "terraform": {
            "required_providers": {
                "proxmox": {
                    "source": "bpg/proxmox",
                    "version": ">= 0.70.0, < 0.99.0"
                }
            }
        },
        "provider": {
            "proxmox": {
                "endpoint": endpoint,
                "api_token": "${var.proxmox_api_token}",
                "insecure": tls_insecure
            }
        }
    })
}

fn main_json(desired: &DesiredState, include_proxmox_resources: bool) -> serde_json::Value {
    let mut modules = Map::new();
    for resource in &desired.resources {
        modules.insert(module_name(resource), module_json(resource, desired));
    }

    let mut root = Map::new();
    if include_proxmox_resources {
        let download_resources = image_download_resources_json(desired);
        if !download_resources.is_empty() {
            root.insert(
                "proxmox_virtual_environment_download_file".to_string(),
                Value::Object(download_resources),
            );
        }
    }

    let mut document = json!({
        "terraform": {
            "required_version": ">= 1.6.0"
        },
        "locals": {
            "vmctl_resource_names": "${[for resource in var.resources : resource.name]}",
            "vmctl_resource_count": "${length(var.resources)}"
        },
        "module": modules
    });
    if !root.is_empty() {
        document["resource"] = Value::Object(root);
    }
    document
}

fn image_download_resources_json(desired: &DesiredState) -> Map<String, Value> {
    desired
        .images
        .values()
        .filter(|image| {
            image.source == ImageSource::Url
                && !image_cached_locally(image)
                && image_needed_for_new_resource(image, desired)
        })
        .map(|image| {
            let mut body = Map::new();
            body.insert("node_name".to_string(), Value::String(image.node.clone()));
            body.insert(
                "datastore_id".to_string(),
                Value::String(image.storage.clone()),
            );
            body.insert(
                "content_type".to_string(),
                Value::String(image.content_type.clone()),
            );
            body.insert(
                "file_name".to_string(),
                Value::String(image.file_name.clone()),
            );
            body.insert(
                "url".to_string(),
                Value::String(image.url.clone().unwrap_or_default()),
            );
            body.insert("overwrite".to_string(), Value::Bool(true));
            body.insert("overwrite_unmanaged".to_string(), Value::Bool(true));
            if let Some(algorithm) = &image.checksum_algorithm {
                body.insert(
                    "checksum_algorithm".to_string(),
                    Value::String(algorithm.clone()),
                );
            }
            if let Some(checksum) = &image.checksum {
                body.insert("checksum".to_string(), Value::String(checksum.clone()));
            }
            (image_resource_name(&image.name), Value::Object(body))
        })
        .collect()
}

fn outputs_json() -> serde_json::Value {
    json!({
        "output": {
            "vmctl_resource_names": {
                "description": "Resource names compiled by vmctl.",
                "value": "${local.vmctl_resource_names}"
            },
            "vmctl_resource_count": {
                "description": "Number of resources compiled by vmctl.",
                "value": "${local.vmctl_resource_count}"
            }
        }
    })
}

fn module_json(resource: &Resource, desired: &DesiredState) -> Value {
    let normalized = desired.normalized_resources.get(&resource.name);
    let mut module_resource = normalized
        .cloned()
        .unwrap_or_else(|| normalize_fallback(resource));
    if module_resource.kind == "vm" && module_resource.disk_interface.is_none() {
        if let Some(interface) = current_vm_disk_interface(module_resource.vmid) {
            module_resource.disk_interface = Some(interface);
        }
    }
    if module_resource.kind == "vm" {
        module_resource.disk_interface = module_resource
            .disk_interface
            .as_deref()
            .and_then(canonical_vm_disk_interface)
            .map(str::to_string)
            .or_else(|| Some(default_vm_disk_interface().to_string()));
    }
    if module_resource.kind == "vm" && module_resource.scsi_hardware.is_none() {
        if let Some(scsi_hardware) = current_vm_scsi_hardware(module_resource.vmid) {
            module_resource.scsi_hardware = Some(scsi_hardware);
        }
    }
    let mut module = Map::new();
    module.insert(
        "source".to_string(),
        Value::String(format!("./modules/{}", resource.kind)),
    );
    module.insert(
        "resource".to_string(),
        redacted_value(json!(module_resource)),
    );
    module.insert(
        "node_name".to_string(),
        normalized
            .and_then(|resource| resource.node.clone())
            .map(Value::String)
            .or_else(|| backend_proxmox_string(desired, "node"))
            .unwrap_or_else(|| Value::String(String::new())),
    );
    module.insert(
        "bridge".to_string(),
        normalized
            .and_then(|resource| resource.bridge.clone())
            .map(Value::String)
            .unwrap_or_else(|| Value::String(String::new())),
    );
    module.insert(
        "storage".to_string(),
        normalized
            .and_then(|resource| resource.storage.clone())
            .map(Value::String)
            .unwrap_or_else(|| Value::String(String::new())),
    );
    module.insert(
        "template".to_string(),
        module_template_value(resource, desired, normalized),
    );

    let mut depends_on = resource
        .depends_on
        .iter()
        .map(|dependency| format!("module.{}", sanitize_module_name(dependency)))
        .collect::<Vec<_>>();
    if let Some(image_dependency) = image_dependency(resource, desired) {
        depends_on.push(image_dependency);
    }
    if !depends_on.is_empty() {
        module.insert("depends_on".to_string(), json!(depends_on));
    }

    Value::Object(module)
}

fn module_template_value(
    resource: &Resource,
    desired: &DesiredState,
    normalized: Option<&NormalizedResource>,
) -> Value {
    if let Some(image_ref) = image_ref(resource, desired) {
        return image_ref;
    }
    normalized
        .and_then(|resource| resource.template.clone())
        .map(Value::String)
        .unwrap_or_else(|| Value::String(String::new()))
}

fn image_ref(resource: &Resource, desired: &DesiredState) -> Option<Value> {
    let image = resource
        .image
        .as_ref()
        .and_then(|name| desired.images.get(name))?;
    if image.source == ImageSource::Url {
        if resource_exists_locally(resource, desired) {
            return Some(Value::String(image.volume_id.clone()));
        }
        if image_cached_locally(image) {
            return Some(Value::String(image.volume_id.clone()));
        }
        Some(Value::String(format!(
            "${{proxmox_virtual_environment_download_file.{}.id}}",
            image_resource_name(&image.name)
        )))
    } else {
        Some(Value::String(image.volume_id.clone()))
    }
}

fn image_dependency(resource: &Resource, desired: &DesiredState) -> Option<String> {
    let image = resource
        .image
        .as_ref()
        .and_then(|name| desired.images.get(name))?;
    if image.source == ImageSource::Url
        && !resource_exists_locally(resource, desired)
        && !image_cached_locally(image)
    {
        Some(format!(
            "proxmox_virtual_environment_download_file.{}",
            image_resource_name(&image.name)
        ))
    } else {
        None
    }
}

fn image_needed_for_new_resource(
    image: &vmctl_domain::ResolvedImage,
    desired: &DesiredState,
) -> bool {
    desired
        .resources
        .iter()
        .filter(|resource| resource.image.as_deref() == Some(image.name.as_str()))
        .any(|resource| !resource_exists_locally(resource, desired))
}

fn resource_exists_locally(resource: &Resource, desired: &DesiredState) -> bool {
    if cfg!(test) && std::env::var_os("VMCTL_TEST_LIVE_PROXMOX").is_none() {
        return false;
    }
    let vmid = desired
        .normalized_resources
        .get(&resource.name)
        .and_then(|resource| resource.vmid)
        .or(resource.vmid);
    let Some(vmid) = vmid else {
        return false;
    };
    let vmid = vmid.to_string();
    let command = match resource.kind.as_str() {
        "vm" => "qm",
        "lxc" => "pct",
        _ => return false,
    };
    command_runner::run(
        CommandOptions::new(command, ["status", &vmid])
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .is_ok()
}

fn current_vm_disk_interface(vmid: Option<u32>) -> Option<String> {
    if cfg!(test) && std::env::var_os("VMCTL_TEST_LIVE_PROXMOX").is_none() {
        return None;
    }
    let vmid = vmid?.to_string();
    let output = command_runner::run(
        CommandOptions::new("qm", ["config", &vmid])
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .ok()?;
    output.combined.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key == "ide2" || !looks_like_disk_interface(key) {
            return None;
        }
        let value = value.trim();
        if value.contains(":vm-") || value.contains(":base-") || value.contains("size=") {
            Some(key.to_string())
        } else {
            None
        }
    })
}

fn current_vm_scsi_hardware(vmid: Option<u32>) -> Option<String> {
    if cfg!(test) && std::env::var_os("VMCTL_TEST_LIVE_PROXMOX").is_none() {
        return None;
    }
    let vmid = vmid?.to_string();
    let output = command_runner::run(
        CommandOptions::new("qm", ["config", &vmid])
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .ok()?;
    scsi_hardware_from_qm_config(&output.combined).map(str::to_string)
}

fn scsi_hardware_from_qm_config(config: &str) -> Option<&str> {
    config
        .lines()
        .find_map(|line| line.strip_prefix("scsihw:").map(str::trim))
}

fn looks_like_disk_interface(value: &str) -> bool {
    ["virtio", "scsi", "sata", "ide"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

fn image_cached_locally(image: &vmctl_domain::ResolvedImage) -> bool {
    if cfg!(test) && std::env::var_os("VMCTL_TEST_LIVE_PROXMOX").is_none() {
        return false;
    }
    if image.source != ImageSource::Url {
        return false;
    }
    let path_output = command_runner::run(
        CommandOptions::new("pvesm", ["path", &image.volume_id])
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    );
    if let Ok(output) = path_output {
        let path = output.stdout.trim();
        if !path.is_empty() && Path::new(path).is_file() {
            return true;
        }
    }

    command_runner::run(
        CommandOptions::new(
            "pvesm",
            ["list", &image.storage, "--content", &image.content_type],
        )
        .timeout(Duration::from_secs(20))
        .prefix(LogPrefix::Proxmox)
        .stream(false)
        .fail_on_proxmox_patterns(false),
    )
    .ok()
    .map(|output| output.stdout.contains(&image.file_name))
    .unwrap_or(false)
}

fn write_base_modules(generated: &Path, include_proxmox_resources: bool) -> Result<Vec<PathBuf>> {
    let modules_dir = generated.join("modules");
    let mut files = Vec::new();
    for kind in ["vm", "lxc"] {
        let module_dir = modules_dir.join(kind);
        std::fs::create_dir_all(&module_dir)?;
        write_json(
            &module_dir.join("variables.tf.json"),
            &base_module_variables_json(kind),
            &mut files,
        )?;
        write_json(
            &module_dir.join("main.tf.json"),
            &base_module_main_json(kind, include_proxmox_resources),
            &mut files,
        )?;
        write_json(
            &module_dir.join("outputs.tf.json"),
            &base_module_outputs_json(kind),
            &mut files,
        )?;
    }
    Ok(files)
}

fn base_module_variables_json(kind: &str) -> Value {
    json!({
        "variable": {
            "resource": {
                "type": "any",
                "description": format!("Normalized vmctl {kind} resource.")
            },
            "node_name": {
                "type": "string",
                "description": "Resolved Proxmox node name for this resource."
            },
            "bridge": {
                "type": "string",
                "description": "Resolved Proxmox bridge for this resource."
            },
            "storage": {
                "type": "string",
                "description": "Resolved Proxmox storage for this resource."
            },
            "template": {
                "type": "string",
                "description": "Resolved template or image identifier for this resource."
            },
            "proxmox_resource_enabled": {
                "type": "bool",
                "description": "Whether this generated module should create Proxmox provider resources.",
                "default": true
            }
        }
    })
}

fn base_module_main_json(kind: &str, include_proxmox_resources: bool) -> Value {
    let proxmox_resource = if include_proxmox_resources {
        match kind {
            "vm" => Some(vm_resource_json()),
            "lxc" => Some(lxc_resource_json()),
            _ => None,
        }
    } else {
        None
    };

    let mut resources = Map::new();
    if let Some((resource_type, resource_body)) = proxmox_resource {
        resources.insert(resource_type, resource_body);
    }
    resources.insert(
        "terraform_data".to_string(),
        json!({
            "this": {
                "input": {
                    "kind": kind,
                    "name": "${var.resource.name}",
                    "vmid": "${try(var.resource.vmid, null)}",
                    "node_name": "${var.node_name}",
                    "bridge": "${var.bridge}",
                    "storage": "${var.storage}",
                    "template": "${var.template}",
                    "resource": "${var.resource}"
                }
            }
        }),
    );

    json!({
        "terraform": {
            "required_providers": {
                "proxmox": {
                    "source": "bpg/proxmox",
                    "version": ">= 0.70.0, < 0.99.0"
                }
            }
        },
        "resource": resources
    })
}

fn vm_resource_json() -> (String, Value) {
    (
        "proxmox_virtual_environment_vm".to_string(),
        json!({
            "this": {
                "count": "${var.proxmox_resource_enabled ? 1 : 0}",
                "description": "${try(var.resource.description, \"managed by vmctl\")}",
                "name": "${var.resource.name}",
                "node_name": "${var.node_name}",
                "vm_id": "${try(var.resource.vmid, null)}",
                "lifecycle": {
                    "ignore_changes": ["disk", "initialization"]
                },
                "machine": "${try(var.resource.machine, try(var.resource.features.intel_igpu.enabled, false) ? \"q35\" : null)}",
                "scsi_hardware": "${try(var.resource.scsi_hardware, null)}",
                "on_boot": "${try(var.resource.start_on_boot, true)}",
                "started": "${coalesce(try(var.resource.started, null), try(var.resource.start_on_boot, true))}",
                "tags": "${try(var.resource.tags, [])}",
                "agent": [{
                    "enabled": "${try(var.resource.agent, true)}",
                    "timeout": "${try(var.resource.agent_timeout, \"15s\")}"
                }],
                "cpu": [{
                    "cores": "${try(var.resource.cores, 1)}",
                    "type": "host"
                }],
                "memory": [{
                    "dedicated": "${try(var.resource.memory, 1024)}",
                    "floating": "${try(var.resource.memory, 1024)}"
                }],
                "disk": [{
                    "datastore_id": "${var.storage}",
                    "import_from": "${try(var.resource.clone_vmid, null) == null && var.template != \"\" ? var.template : null}",
                    "interface": "${coalesce(try(var.resource.disk_interface, null), \"virtio0\")}",
                    "iothread": "${coalesce(try(var.resource.iothread, null), true)}",
                    "discard": "on",
                    "size": "${try(var.resource.disk_gb, 8)}"
                }],
                "network_device": [{
                    "bridge": "${var.bridge}",
                    "disconnected": false,
                    "enabled": true,
                    "firewall": "${try(var.resource.network.firewall, false)}",
                    "mac_address": "${try(var.resource.network.mac, null)}",
                    "model": "virtio",
                    "mtu": "${try(var.resource.network.mtu, 0)}",
                    "queues": 0,
                    "rate_limit": 0,
                    "trunks": "",
                    "vlan_id": "${try(var.resource.network.vlan_id, null)}"
                }],
                "dynamic": {
                    "clone": {
                        "for_each": "${try(var.resource.clone_vmid, null) == null ? [] : [var.resource.clone_vmid]}",
                        "content": {
                            "vm_id": "${clone.value}",
                            "datastore_id": "${var.storage}",
                            "full": true
                        }
                    },
                    "hostpci": {
                        "for_each": "${try(var.resource.features.intel_igpu.enabled, false) && (try(var.resource.features.intel_igpu.pci_device, null) != null || try(var.resource.features.intel_igpu.mapping, null) != null) ? [var.resource.features.intel_igpu] : []}",
                        "content": {
                            "device": "hostpci0",
                            "id": "${try(hostpci.value.mapping, null) == null ? try(hostpci.value.pci_device, null) : null}",
                            "mapping": "${try(hostpci.value.mapping, null)}",
                            "pcie": "${try(hostpci.value.pcie, true)}",
                            "rombar": "${try(hostpci.value.rombar, true)}",
                            "xvga": "${try(hostpci.value.xvga, false)}"
                        }
                    },
                    "initialization": {
                        "for_each": "${try(var.resource.cloud_init, null) == null && try(var.resource.network, null) == null && try(var.resource.nameserver, null) == null && try(var.resource.searchdomain, null) == null ? [] : [1]}",
                        "content": {
                            "dns": [{
                                "domain": "${try(var.resource.searchdomain, null)}",
                                "servers": "${compact(split(\",\", replace(try(var.resource.nameserver, \"\"), \" \", \"\")))}"
                            }],
                            "user_account": [{
                                "username": "${try(var.resource.cloud_init.user, null)}",
                                "keys": "${try(var.resource.cloud_init.ssh_key_file, null) == null ? [] : [file(var.resource.cloud_init.ssh_key_file)]}"
                            }],
                            "ip_config": [{
                                "ipv4": [{
                                    "address": "${try(var.resource.network.mode, \"dhcp\") == \"dhcp\" ? \"dhcp\" : try(var.resource.network.address, \"dhcp\")}",
                                    "gateway": "${try(var.resource.network.gateway, null)}"
                                }]
                            }]
                        }
                    }
                }
            }
        }),
    )
}

fn lxc_resource_json() -> (String, Value) {
    (
        "proxmox_virtual_environment_container".to_string(),
        json!({
            "this": {
                "count": "${var.proxmox_resource_enabled ? 1 : 0}",
                "description": "${try(var.resource.description, \"managed by vmctl\")}",
                "node_name": "${var.node_name}",
                "vm_id": "${try(var.resource.vmid, null)}",
                "lifecycle": {
                    "ignore_changes": [
                        "console",
                        "device_passthrough",
                        "disk",
                        "features",
                        "network_interface",
                        "operating_system",
                        "tags"
                    ]
                },
                "start_on_boot": "${try(var.resource.start_on_boot, true)}",
                "started": "${try(var.resource.start_on_boot, true)}",
                "tags": "${try(var.resource.tags, [])}",
                "unprivileged": true,
                "features": [{
                    "nesting": "${try(var.resource.features.lxc.nesting, false)}"
                }],
                "memory": [{
                    "dedicated": "${try(var.resource.memory, 1024)}"
                }],
                "cpu": [{
                    "cores": "${try(var.resource.cores, 1)}"
                }],
                "initialization": [{
                    "hostname": "${coalesce(try(var.resource.hostname, null), var.resource.name)}",
                    "dns": [{
                        "domain": "${try(var.resource.searchdomain, null)}",
                        "servers": "${compact(split(\",\", replace(try(var.resource.nameserver, \"\"), \" \", \"\")))}"
                    }],
                    "ip_config": [{
                        "ipv4": [{
                            "address": "${try(var.resource.network.mode, \"dhcp\") == \"dhcp\" ? \"dhcp\" : try(var.resource.network.address, \"dhcp\")}",
                            "gateway": "${try(var.resource.network.gateway, null)}"
                        }]
                    }]
                }],
                "network_interface": [{
                    "name": "eth0",
                    "bridge": "${var.bridge}",
                    "enabled": true,
                    "firewall": "${try(var.resource.network.firewall, false)}",
                    "mac_address": "${try(var.resource.network.mac, null)}",
                    "mtu": "${try(var.resource.network.mtu, 0)}",
                    "vlan_id": "${try(var.resource.network.vlan_id, null)}"
                }],
                "disk": [{
                    "datastore_id": "${var.storage}",
                    "size": "${try(var.resource.rootfs_gb, 8)}"
                }],
                "operating_system": [{
                    "template_file_id": "${strcontains(var.template, \":\") ? var.template : format(\"%s:vztmpl/%s\", try(var.resource.template_storage, var.storage), var.template)}",
                    "type": "${coalesce(try(var.resource.os_type, null), \"debian\")}"
                }]
            }
        }),
    )
}

fn base_module_outputs_json(kind: &str) -> Value {
    json!({
        "output": {
            "name": {
                "description": format!("Rendered vmctl {kind} resource name."),
                "value": "${var.resource.name}"
            },
            "vmid": {
                "description": format!("Rendered vmctl {kind} resource VMID."),
                "value": "${try(var.resource.vmid, null)}"
            }
        }
    })
}

fn module_name(resource: &Resource) -> String {
    sanitize_module_name(&resource.name)
}

fn image_resource_name(name: &str) -> String {
    format!("image_{}", sanitize_module_name(name))
}

fn backend_proxmox_string(desired: &DesiredState, key: &str) -> Option<Value> {
    desired
        .backend
        .settings
        .get("proxmox")
        .and_then(toml::Value::as_table)
        .and_then(|settings| settings.get(key))
        .and_then(toml::Value::as_str)
        .map(|value| Value::String(value.to_string()))
}

fn normalize_fallback(resource: &Resource) -> NormalizedResource {
    NormalizedResource {
        name: resource.name.clone(),
        kind: resource.kind.clone(),
        image: resource.image.clone(),
        role: resource.role.clone(),
        vmid: resource.vmid,
        depends_on: resource.depends_on.clone(),
        features: resource.features.clone(),
        ..NormalizedResource::default()
    }
}

fn redacted_value(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    if is_secret_key(&key) {
                        (key, Value::String("<redacted>".to_string()))
                    } else {
                        (key, redacted_value(value))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redacted_value).collect()),
        other => other,
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("secret")
        || key.contains("token")
        || key.contains("auth_key")
        || key.contains("private_key")
}

fn sanitize_module_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[allow(dead_code)]
fn module_names_by_resource(resources: &[Resource]) -> BTreeMap<String, String> {
    resources
        .iter()
        .map(|resource| (resource.name.clone(), module_name(resource)))
        .collect()
}

fn terraform_binary() -> Result<&'static str> {
    if vmctl_util::command_exists("tofu") {
        Ok("tofu")
    } else if vmctl_util::command_exists("terraform") {
        Ok("terraform")
    } else {
        bail!("missing Terraform backend binary; install `tofu` or `terraform`")
    }
}

fn run_terraform(workspace: &Workspace, args: &[&str]) -> Result<String> {
    run_terraform_with_options(workspace, args, true)
}

fn run_terraform_with_options(
    workspace: &Workspace,
    args: &[&str],
    include_full_error_output: bool,
) -> Result<String> {
    let binary = terraform_binary()?;
    let generated = workspace.root.join(&workspace.generated_dir);
    let timeout = if args.first() == Some(&"apply") || args.first() == Some(&"destroy") {
        Duration::from_secs(1800)
    } else {
        Duration::from_secs(600)
    };
    match command_runner::run(
        CommandOptions::new(binary, args.iter().copied())
            .cwd(generated)
            .timeout(timeout)
            .prefix(LogPrefix::Terraform),
    ) {
        Ok(output) => Ok(output.combined),
        Err(error) => {
            if include_full_error_output {
                Err(error.into())
            } else {
                let concise = concise_terraform_error(&error.to_string());
                bail!("{concise}")
            }
        }
    }
}

fn concise_terraform_error(output: &str) -> String {
    if let Some(index) = output.rfind("\nError:") {
        return output[index..].trim().to_string();
    }
    if let Some(index) = output.find("Error:") {
        return output[index..].trim().to_string();
    }
    output
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_summary(prefix: &str, output: &str) -> String {
    let tail = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("completed");
    format!("{prefix}: {tail}")
}

fn plan_output(prefix: &str, output: &str) -> String {
    let output = output.trim();
    if output.is_empty() {
        format!("{prefix}: completed")
    } else {
        format!("{prefix}:\n{output}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use vmctl_domain::{BackendConfig, DesiredState};

    #[test]
    fn plan_output_includes_full_plan_body() {
        let output = plan_output(
            "tofu dry-run plan",
            "\nTerraform will perform the following actions:\n  # module.media_stack will be created\nPlan: 1 to add, 0 to change, 0 to destroy.\n",
        );

        assert!(output.starts_with("tofu dry-run plan:\n"));
        assert!(output.contains("module.media_stack will be created"));
        assert!(output.contains("Plan: 1 to add, 0 to change, 0 to destroy."));
    }

    #[test]
    fn render_preserves_terraform_state_files() {
        let root = unique_temp_dir();
        let generated_dir = root.join("generated");
        std::fs::create_dir_all(generated_dir.join(".terraform")).unwrap();
        std::fs::write(generated_dir.join("terraform.tfstate"), "{}").unwrap();
        std::fs::write(generated_dir.join(".terraform.lock.hcl"), "# lock").unwrap();
        std::fs::write(generated_dir.join("main.tf.json"), "{}").unwrap();
        std::fs::create_dir_all(generated_dir.join("modules/stale")).unwrap();

        prepare_generated_workspace(&generated_dir).unwrap();

        assert!(generated_dir.join("terraform.tfstate").is_file());
        assert!(generated_dir.join(".terraform.lock.hcl").is_file());
        assert!(generated_dir.join(".terraform").is_dir());
        assert!(!generated_dir.join("main.tf.json").exists());
        assert!(!generated_dir.join("modules").exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concise_terraform_error_hides_plan_body() {
        let output = "OpenTofu will perform the following actions:\n  # module.vm will be created\nPlan: 1 to add, 0 to change, 0 to destroy.\n\nError: failed late\n\n  with module.vm";

        let concise = concise_terraform_error(output);

        assert!(!concise.contains("OpenTofu will perform"));
        assert!(!concise.contains("Plan: 1 to add"));
        assert!(concise.contains("Error: failed late"));
    }

    #[test]
    fn renders_module_blocks_for_resources_and_dependencies() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: vec![
                Resource {
                    name: "gateway".to_string(),
                    kind: "lxc".to_string(),
                    image: None,
                    role: None,
                    vmid: Some(101),
                    depends_on: Vec::new(),
                    features: BTreeMap::new(),
                    settings: BTreeMap::new(),
                },
                Resource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    image: None,
                    role: None,
                    vmid: Some(210),
                    depends_on: vec!["gateway".to_string()],
                    features: BTreeMap::new(),
                    settings: BTreeMap::new(),
                },
            ],
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
        };

        let rendered = main_json(&desired, true);
        let modules = rendered.get("module").and_then(Value::as_object).unwrap();

        assert_eq!(modules["gateway"]["source"], "./modules/lxc");
        assert_eq!(modules["media_stack"]["source"], "./modules/vm");
        assert_eq!(modules["media_stack"]["depends_on"][0], "module.gateway");
    }

    #[test]
    fn renders_provider_and_module_inputs() {
        let desired = DesiredState {
            backend: BackendConfig {
                kind: "terraform".to_string(),
                settings: BTreeMap::from([(
                    "proxmox".to_string(),
                    toml::Value::Table(toml::map::Map::from_iter([
                        (
                            "endpoint".to_string(),
                            toml::Value::String("https://mini:8006/api2/json".to_string()),
                        ),
                        ("node".to_string(), toml::Value::String("mini".to_string())),
                        (
                            "token_id".to_string(),
                            toml::Value::String("root@pam!vmctl".to_string()),
                        ),
                        (
                            "token_secret".to_string(),
                            toml::Value::String("secret".to_string()),
                        ),
                        ("tls_insecure".to_string(), toml::Value::Boolean(true)),
                    ])),
                )]),
            },
            images: BTreeMap::new(),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                image: None,
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::from([
                    (
                        "bridge".to_string(),
                        toml::Value::String("vmbr0".to_string()),
                    ),
                    (
                        "storage".to_string(),
                        toml::Value::String("local-lvm".to_string()),
                    ),
                    (
                        "template".to_string(),
                        toml::Value::String("ubuntu-template".to_string()),
                    ),
                ]),
            }],
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    node: Some("mini".to_string()),
                    bridge: Some("vmbr0".to_string()),
                    storage: Some("local-lvm".to_string()),
                    template: Some("ubuntu-template".to_string()),
                    ..NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
        };

        let provider = provider_json(&desired);
        let rendered = main_json(&desired, true);
        let module = &rendered["module"]["media_stack"];

        assert_eq!(
            provider["provider"]["proxmox"]["endpoint"],
            "https://mini:8006/api2/json"
        );
        assert_eq!(
            provider["provider"]["proxmox"]["api_token"],
            "${var.proxmox_api_token}"
        );
        assert_eq!(module["node_name"], "mini");
        assert_eq!(module["bridge"], "vmbr0");
        assert_eq!(module["storage"], "local-lvm");
        assert_eq!(module["template"], "ubuntu-template");
    }

    #[test]
    fn renders_url_image_download_resource_and_dependency() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::from([(
                "debian_12_lxc_url".to_string(),
                vmctl_domain::ResolvedImage {
                    name: "debian_12_lxc_url".to_string(),
                    kind: vmctl_domain::ImageKind::Lxc,
                    source: ImageSource::Url,
                    node: "mini".to_string(),
                    storage: "local".to_string(),
                    content_type: "vztmpl".to_string(),
                    file_name: "debian-12-rootfs.tar.zst".to_string(),
                    volume_id: "local:vztmpl/debian-12-rootfs.tar.zst".to_string(),
                    vmid: None,
                    url: Some("https://example.invalid/debian-12-rootfs.tar.zst".to_string()),
                    checksum_algorithm: Some("sha256".to_string()),
                    checksum: Some("abc123".to_string()),
                },
            )]),
            resources: vec![Resource {
                name: "gateway".to_string(),
                kind: "lxc".to_string(),
                image: Some("debian_12_lxc_url".to_string()),
                role: None,
                vmid: Some(101),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::from([(
                "gateway".to_string(),
                NormalizedResource {
                    name: "gateway".to_string(),
                    kind: "lxc".to_string(),
                    image: Some("debian_12_lxc_url".to_string()),
                    template: Some("local:vztmpl/debian-12-rootfs.tar.zst".to_string()),
                    ..NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
        };

        let rendered = main_json(&desired, true);
        let download = &rendered["resource"]["proxmox_virtual_environment_download_file"]
            ["image_debian_12_lxc_url"];
        let module = &rendered["module"]["gateway"];

        assert_eq!(
            download["url"],
            "https://example.invalid/debian-12-rootfs.tar.zst"
        );
        assert_eq!(
            module["template"],
            "${proxmox_virtual_environment_download_file.image_debian_12_lxc_url.id}"
        );
        assert_eq!(
            module["depends_on"][0],
            "proxmox_virtual_environment_download_file.image_debian_12_lxc_url"
        );
    }

    #[test]
    fn renders_url_vm_image_download_resource_and_dependency() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::from([(
                "ubuntu_24_cloud_image".to_string(),
                vmctl_domain::ResolvedImage {
                    name: "ubuntu_24_cloud_image".to_string(),
                    kind: vmctl_domain::ImageKind::Vm,
                    source: ImageSource::Url,
                    node: "mini".to_string(),
                    storage: "local-lvm".to_string(),
                    content_type: "import".to_string(),
                    file_name: "noble-server-cloudimg-amd64.qcow2".to_string(),
                    volume_id: "local-lvm:import/noble-server-cloudimg-amd64.qcow2".to_string(),
                    vmid: None,
                    url: Some(
                        "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
                            .to_string(),
                    ),
                    checksum_algorithm: None,
                    checksum: None,
                },
            )]),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                image: Some("ubuntu_24_cloud_image".to_string()),
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    image: Some("ubuntu_24_cloud_image".to_string()),
                    template: Some("local-lvm:import/noble-server-cloudimg-amd64.qcow2".to_string()),
                    ..NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
        };

        let rendered = main_json(&desired, true);
        let download = &rendered["resource"]["proxmox_virtual_environment_download_file"]
            ["image_ubuntu_24_cloud_image"];
        let module = &rendered["module"]["media_stack"];
        let vm_module = base_module_main_json("vm", true);
        let vm = &vm_module["resource"]["proxmox_virtual_environment_vm"]["this"];

        assert_eq!(
            download["url"],
            "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
        );
        assert_eq!(
            module["template"],
            "${proxmox_virtual_environment_download_file.image_ubuntu_24_cloud_image.id}"
        );
        assert_eq!(
            vm["disk"][0]["import_from"],
            "${try(var.resource.clone_vmid, null) == null && var.template != \"\" ? var.template : null}"
        );
    }

    #[test]
    fn canonical_vm_disk_interface_requires_slot_suffix() {
        assert_eq!(canonical_vm_disk_interface("virtio0"), Some("virtio0"));
        assert_eq!(canonical_vm_disk_interface("scsi12"), Some("scsi12"));
        assert_eq!(canonical_vm_disk_interface("sata"), None);
        assert_eq!(canonical_vm_disk_interface("virtio"), None);
    }

    #[test]
    fn parses_scsi_hardware_from_qm_config() {
        assert_eq!(
            scsi_hardware_from_qm_config(
                "memory: 8192\nscsi0: local-lvm:vm-210-disk-0,iothread=1,size=64G\nscsihw: virtio-scsi-single\n"
            ),
            Some("virtio-scsi-single")
        );
    }

    #[test]
    fn vm_module_preserves_scsi_hardware_setting() {
        let vm_module = base_module_main_json("vm", true);
        let vm = &vm_module["resource"]["proxmox_virtual_environment_vm"]["this"];

        assert_eq!(
            vm["scsi_hardware"],
            "${try(var.resource.scsi_hardware, null)}"
        );
    }

    #[test]
    fn terraform_apply_args_can_disable_refresh_for_safe_apply() {
        assert_eq!(
            terraform_apply_args(false, None),
            vec![
                "apply",
                "-auto-approve",
                "-input=false",
                "-no-color",
                "-refresh=false"
            ]
        );
        assert!(!terraform_apply_args(true, None).contains(&"-refresh=false".to_string()));
        assert!(terraform_apply_args(true, Some("kodi-htpc"))
            .contains(&"-target=module.kodi_htpc".to_string()));
    }

    #[test]
    fn render_writes_redacted_snapshot_artifacts() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            root: root.clone(),
            generated_dir: PathBuf::from("generated"),
        };
        let desired = DesiredState {
            backend: BackendConfig {
                kind: "terraform".to_string(),
                settings: BTreeMap::from([(
                    "proxmox".to_string(),
                    toml::Value::Table(toml::map::Map::from_iter([
                        (
                            "endpoint".to_string(),
                            toml::Value::String("https://mini:8006/api2/json".to_string()),
                        ),
                        ("node".to_string(), toml::Value::String("mini".to_string())),
                        (
                            "token_secret".to_string(),
                            toml::Value::String("super-secret".to_string()),
                        ),
                    ])),
                )]),
            },
            images: BTreeMap::new(),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                image: None,
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::from([(
                    "tailscale".to_string(),
                    toml::Value::Table(toml::map::Map::from_iter([(
                        "auth_key".to_string(),
                        toml::Value::String("tskey-secret".to_string()),
                    )])),
                )]),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    node: Some("mini".to_string()),
                    bridge: Some("vmbr0".to_string()),
                    storage: Some("local-lvm".to_string()),
                    template: Some("9000".to_string()),
                    clone_vmid: Some(9000),
                    ..NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
        };

        TerraformBackend
            .render(&workspace, &desired, &PackRegistry::default())
            .unwrap();

        let main = std::fs::read_to_string(root.join("generated/modules/vm/main.tf.json")).unwrap();
        let provider = std::fs::read_to_string(root.join("generated/provider.tf.json")).unwrap();
        let state = std::fs::read_to_string(root.join("generated/desired-state.json")).unwrap();

        assert!(main.contains("proxmox_virtual_environment_vm"));
        assert!(provider.contains("${var.proxmox_api_token}"));
        assert!(!provider.contains("super-secret"));
        assert!(!state.contains("tskey-secret"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_render_rejects_vm_without_clone_vmid() {
        let desired = DesiredState {
            backend: BackendConfig {
                kind: "terraform".to_string(),
                settings: BTreeMap::from([(
                    "proxmox".to_string(),
                    toml::Value::Table(toml::map::Map::from_iter([
                        (
                            "endpoint".to_string(),
                            toml::Value::String("https://mini:8006/api2/json".to_string()),
                        ),
                        ("node".to_string(), toml::Value::String("mini".to_string())),
                    ])),
                )]),
            },
            images: BTreeMap::new(),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                image: None,
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    bridge: Some("vmbr0".to_string()),
                    storage: Some("local-lvm".to_string()),
                    template: Some("ubuntu-template".to_string()),
                    ..NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
        };

        let err = validate_live_inputs(&desired).unwrap_err();

        assert!(err.to_string().contains("requires clone_vmid"));
    }

    #[test]
    fn live_render_rejects_unindexed_vm_disk_interface() {
        let desired = DesiredState {
            backend: BackendConfig {
                kind: "terraform".to_string(),
                settings: BTreeMap::from([(
                    "proxmox".to_string(),
                    toml::Value::Table(toml::map::Map::from_iter([
                        (
                            "endpoint".to_string(),
                            toml::Value::String("https://mini:8006/api2/json".to_string()),
                        ),
                        ("node".to_string(), toml::Value::String("mini".to_string())),
                    ])),
                )]),
            },
            images: BTreeMap::new(),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                image: None,
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    bridge: Some("vmbr0".to_string()),
                    storage: Some("local-lvm".to_string()),
                    template: Some("ubuntu-template".to_string()),
                    clone_vmid: Some(9000),
                    disk_interface: Some("scsi".to_string()),
                    iothread: Some(false),
                    ..NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
        };

        let err = validate_live_inputs(&desired).unwrap_err();

        assert!(err.to_string().contains("disk_interface=scsi"));
        assert!(err.to_string().contains("virtio0/scsi0/sata0/ide0"));
    }

    #[test]
    fn vm_module_matches_fixture() {
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/vm-module-main.tf.json")).unwrap();
        assert_eq!(base_module_main_json("vm", true), expected);
    }

    #[test]
    fn lxc_module_matches_fixture() {
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/lxc-module-main.tf.json"))
                .unwrap();
        assert_eq!(base_module_main_json("lxc", true), expected);
    }

    #[test]
    fn lxc_module_omits_device_passthrough_from_provider_create_path() {
        let module = base_module_main_json("lxc", true);
        let lxc = &module["resource"]["proxmox_virtual_environment_container"]["this"];
        assert!(lxc["dynamic"]["device_passthrough"].is_null());
    }

    #[test]
    fn lxc_module_defaults_os_type_to_debian_when_unset() {
        let module = base_module_main_json("lxc", true);
        let lxc = &module["resource"]["proxmox_virtual_environment_container"]["this"];
        assert_eq!(
            lxc["operating_system"][0]["type"],
            "${coalesce(try(var.resource.os_type, null), \"debian\")}"
        );
    }

    #[test]
    fn kodi_bootstrap_tolerates_jellyfin_unauthorized_auth() {
        let script =
            include_str!("../tests/fixtures/example-workspace/resources/kodi-htpc/scripts/bootstrap-kodi-jellyfin.sh");
        assert!(script.contains("if exc.code in (401, 403):"));
        assert!(script.contains("skipping Kodi Jellyfin token bootstrap"));
    }

    #[test]
    fn media_jellyfin_bootstrap_adds_virtual_folders_via_query_params() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-jellyfin.sh"
        );
        assert!(script.contains("ensure_library(name, path, collection_type, token)"));
        assert!(script.contains("urllib.parse.urlencode"));
        assert!(script.contains("\"name\": name"));
        assert!(script.contains("\"collectionType\": collection_type"));
        assert!(script.contains("\"PathInfos\": [{\"Path\": path}]"));
        assert!(script.contains("call(\"POST\", \"/Library/Refresh\", token=token"));
    }

    #[test]
    fn media_ui_routing_bootstrap_configures_tailscale_https_serve() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-ui-routing.sh"
        );
        assert!(script.contains("TAILSCALE_HTTPS_ENABLED"));
        assert!(script.contains("TAILSCALE_FUNNEL_ENABLED"));
        assert!(script.contains("tailscale funnel --yes --bg"));
        assert!(script.contains("tailscale serve --yes --bg"));
        assert!(script.contains("tailscale serve reset"));
        assert!(script.contains("tailscale funnel reset"));
    }

    #[test]
    fn kodi_env_uses_port_8080_for_kodi_upstream() {
        let env = include_str!("../tests/fixtures/example-workspace/resources/kodi-htpc/kodi.env");
        assert!(env.contains("KODI_WEB_SKIN=webinterface.default"));
        assert!(env.contains("KODI_WEB_PORT=8080"));
        assert!(env.contains("KODI_TAILSCALE_HTTPS_TARGET=http://127.0.0.1:8080"));
    }

    #[test]
    fn kodi_bootstrap_installs_chorus2_and_fronts_it_on_port_80() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/kodi-htpc/scripts/bootstrap-kodi.sh"
        );
        assert!(script.contains("KODI_WEB_PORT=\"${KODI_WEB_PORT:-8080}\""));
        assert!(script.contains("nfs-common"));
        assert!(script.contains("KODI_CHORUS2_REF"));
        assert!(script.contains("name=\"Kodi web interface - Chorus2\""));
        assert!(script.contains("https://github.com/xbmc/chorus2/archive/refs/tags"));
        assert!(script.contains("KODI_MEDIA_EXPORT_PATH"));
        assert!(script.contains("umount -fl /media"));
        assert!(script.contains("reverse_proxy 127.0.0.1:${KODI_WEB_PORT}"));
        assert!(script.contains("Chorus 2 - Kodi web interface"));
        assert!(!script.contains("repair_kodi_web_assets"));
    }

    #[test]
    fn media_bootstrap_exports_media_for_kodi_playback() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh"
        );
        assert!(script.contains("nfs-kernel-server"));
        assert!(script.contains("/etc/exports.d/vmctl-media.exports"));
        assert!(script.contains("192.168.86.0/24(ro,sync,no_subtree_check,insecure)"));
        assert!(script.contains("exportfs -ra"));
    }

    #[test]
    fn media_seerr_bootstrap_initializes_and_wires_integrations() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-seerr.sh"
        );
        assert!(script.contains("settings[\"public\"][\"initialized\"] = True"));
        assert!(script.contains("settings[\"public\"][\"mediaServerLogin\"] = True"));
        assert!(script.contains("settings[\"main\"][\"mediaServerLogin\"] = True"));
        assert!(script.contains("settings[\"jellyfin\"]"));
        assert!(script.contains("settings[\"sonarr\"]"));
        assert!(script.contains("settings[\"radarr\"]"));
        assert!(script.contains("JELLYFIN_INTERNAL_URL"));
        assert!(script.contains("SONARR_INTERNAL_URL"));
        assert!(script.contains("RADARR_INTERNAL_URL"));
        assert!(script.contains("SONARR_EXTERNAL_URL"));
        assert!(script.contains("RADARR_EXTERNAL_URL"));
        assert!(script.contains("pick_profile("));
        assert!(script.contains("SONARR_DEFAULT_QUALITY_PROFILE"));
        assert!(script.contains("RADARR_DEFAULT_QUALITY_PROFILE"));
        assert!(script.contains("build_external_url("));
        assert!(script.contains("\"externalHostname\""));
        assert!(script.contains("seerr failed to finish initialization bootstrap"));
    }

    #[test]
    fn media_arr_bootstrap_sets_qbittorrent_credentials() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh"
        );
        assert!(script.contains("QBIT_USERNAME = os.environ.get(\"QBITTORRENT_USERNAME\""));
        assert!(script.contains("QBIT_PASSWORD = os.environ.get(\"QBITTORRENT_PASSWORD\""));
        assert!(script.contains("\"AuthenticationMethod\": \"External\""));
        assert!(script.contains("\"AuthenticationRequired\": \"DisabledForLocalAddresses\""));
        assert!(script.contains("category_field = \"tvCategory\""));
        assert!(script.contains("category_field = \"movieCategory\""));
        assert!(script.contains("qBittorrent download client did not converge"));
        assert!(script.contains("request(\"GET\", f\"{url}/api/v3/downloadclient\", api_key, allow=())"));
        assert!(script.contains("request(\"PUT\", f\"{url}/api/v3/downloadclient/{item['id']}\""));
        assert!(script.contains("PROWLARR_INTERNAL_URL"));
        assert!(script.contains("ensure_default_indexers"));
        assert!(script.contains("ensure_sabnzbd_download_client"));
        assert!(script.contains("protocol\": \"usenet\""));
        assert!(script.contains(
            "existing_names = {item.get(\"name\") for item in existing if item.get(\"name\")}"
        ));
        assert!(script.contains("PROWLARR_BOOTSTRAP_INDEXERS"));
        assert!(script.contains("QBITTORRENT_CATEGORY_TV"));
        assert!(script.contains("QBITTORRENT_CATEGORY_MOVIES"));
        assert!(script.contains("SONARR_PROWLARR_CATEGORIES"));
        assert!(script.contains("RADARR_PROWLARR_CATEGORIES"));
        assert!(script.contains("SABNZBD_INTERNAL_URL"));
        assert!(script.contains("SABNZBD_API_KEY"));
    }

    #[test]
    fn media_bootstrap_preserves_env_and_repairs_jellystat_db_password() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh"
        );
        assert!(script.contains("sync_env_from_template()"));
        assert!(script.contains("\"MEILI_MASTER_KEY\""));
        assert!(script.contains("\"JELLYFIN_STREMIO_PASSWORD\""));
        assert!(script.contains("\"JELLYFIN_STREMIO_AUTH_TOKEN\""));
        assert!(script.contains("\"JELLIO_STREMIO_MANIFEST_URL_TAILSCALE\""));
        assert!(script.contains("\"CLOUDFLARED_TOKEN\""));
        assert!(script.contains("\"SABNZBD_API_KEY\""));
        assert!(script.contains("configure_sabnzbd()"));
        assert!(script.contains("SABNZBD_SERVER_HOST"));
        assert!(script.contains("SABNZBD_SERVER_ENABLE"));
        assert!(script.contains("ensure_env_value \"$STACK_DIR/.env\" \"SEERR_API_KEY\""));
        assert!(script.contains("html.unescape(value)"));
        assert!(script.contains("ipv4 = [part for part in parts if \":\" not in part]"));
        assert!(script.contains("MEDIA_SERVICES_CSV="));
        assert!(script.contains("service_enabled()"));
        assert!(script.contains("sync_template_env_defaults()"));
        assert!(script.contains("sync_template_env_defaults \"$RESOURCE_DIR/media.env\""));
        assert!(!script.contains("MEDIA_PUBLIC_BASE_URL_LAN"));
        assert!(!script.contains("VMCTL_HOST_FQDN"));
        assert!(!script.contains("/etc/hosts"));
        assert!(!script.contains("chown -R 1000:1000 \"$STACK_DIR/config\""));
        assert!(script.contains("chown -R 70:70 \"$STACK_DIR/config/jellystat-db\""));
        assert!(script.contains("recover_jellystat_db()"));
        assert!(script.contains("credential drift detected; recreating database volume"));
        assert!(script.contains("refusing to start qBittorrent without VPN"));
        assert!(script.contains("configure_bazarr()"));
        assert!(script.contains("SABNZBD_API_KEY"));
        assert!(script.contains("\"enabled_integrations\": [\"sonarr\", \"radarr\"]"));
        assert!(script.contains("\"use_sonarr\": True"));
        assert!(script.contains("\"use_radarr\": True"));
        assert!(script.contains("p7zip-full"));
    }

    #[test]
    fn media_download_unpack_bootstrap_extracts_archives_and_triggers_arr_scans() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-download-unpack.sh"
        );
        assert!(script.contains("vmctl-media-unpack.service"));
        assert!(script.contains("vmctl-media-unpack.timer"));
        assert!(script.contains("DownloadedMoviesScan"));
        assert!(script.contains("DownloadedEpisodesScan"));
        assert!(script.contains("\"7z\", \"x\", \"-y\""));
        assert!(script.contains("jellyfin_refresh"));
    }

    #[test]
    fn media_jellystat_bootstrap_configures_jellyfin_and_disables_login() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-jellystat.sh"
        );
        assert!(script.contains(
            "if ! service_enabled \"jellystat\" || ! service_enabled \"jellystat-db\"; then"
        ));
        assert!(script.contains("/auth/createuser"));
        assert!(script.contains("/auth/configSetup"));
        assert!(script.contains("JELLYFIN_INTERNAL_URL"));
        assert!(script.contains("sha3_512"));
        assert!(script.contains("UPDATE app_config SET \"REQUIRE_LOGIN\" = false"));
    }

    #[test]
    fn media_caddy_fixture_uses_service_port_mode_with_stremio_jellyfin_proxy() {
        let caddy = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/caddyfile.media"
        );
        assert!(caddy.contains("auto_https off"));
        assert!(caddy.contains(":80 {"));
        assert!(caddy.contains("handle_path /healthz"));
        assert!(caddy.contains("header -Strict-Transport-Security"));
        assert!(caddy.contains("log {"));
        assert!(caddy.contains("handle {"));
        assert!(caddy.contains("@tizen_jellio"));
        assert!(caddy.contains("path /jellio/*"));
        assert!(caddy.contains("handle /jellio/*"));
        assert!(!caddy.contains("/jellio-lan/"));
        assert!(!caddy.contains("/jellio-lan-ip/"));
        assert!(!caddy.contains("/jellio-lan-short/"));
        assert!(caddy.contains("handle_path /jf/*"));
        assert!(caddy.contains("handle /Items/*"));
        assert!(caddy.contains("handle /items/*"));
        assert!(caddy.contains("@tizen_stream"));
        assert!(caddy.contains("@tizen_jf_stream"));
        assert!(caddy.contains("path_regexp tizen_stream ^/[Vv]ideos/([^/]+)/stream$"));
        assert!(caddy.contains("path_regexp tizen_jf_stream ^/jf/[Vv]ideos/([^/]+)/stream$"));
        assert!(caddy.contains("rewrite * /Videos/{re.tizen_stream.1}/master.m3u8"));
        assert!(caddy.contains("rewrite * /Videos/{re.tizen_jf_stream.1}/master.m3u8"));
        assert!(caddy.contains("header_up Accept-Encoding identity"));
        assert!(caddy.contains("handle /Videos/*"));
        assert!(caddy.contains("handle /videos/*"));
        assert!(caddy.contains("header_up X-MediaBrowser-Token {$JELLYFIN_STREMIO_AUTH_TOKEN}"));
        assert!(caddy.contains("reverse_proxy seerr:5055"));
        assert!(!caddy.contains("header_up X-API-Key {$SEERR_API_KEY}"));
        assert!(!caddy.contains("handle /sonarr*"));
        assert!(!caddy.contains("handle /radarr*"));
        assert!(!caddy.contains("handle /prowlarr*"));
        assert!(!caddy.contains("handle /qbittorrent*"));
        assert!(!caddy.contains("reverse_proxy sonarr:8989"));
    }

    #[test]
    fn media_jellio_bootstrap_uses_jf_public_base_for_streams_and_artwork() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-jellio.sh"
        );
        assert!(script.contains("host_server_name = (os.environ.get(\"VMCTL_RESOURCE_NAME\")"));
        assert!(script.contains("jellyfin_public_base = f\"{addon_base.rstrip('/')}/jf\""));
        assert!(script.contains("\"PublicBaseUrl\": jellyfin_public_base"));
        assert!(!script.contains("JELLIO_STREMIO_MANIFEST_URL_LAN"));
        assert!(!script.contains("jellio-manifest.lan"));
        assert!(
            script.contains("return f\"{addon_base.rstrip('/')}/jellio/{encoded}/manifest.json\"")
        );
        assert!(script
            .contains("set_env_value(env_file, \"JELLYFIN_STREMIO_AUTH_TOKEN\", stremio_token)"));
    }

    #[test]
    fn media_env_fixture_derives_public_hosts_from_resource_identity() {
        let env =
            include_str!("../tests/fixtures/example-workspace/resources/media-stack/media.env");
        assert!(env.contains("VMCTL_RESOURCE_NAME=media-stack"));
        assert!(env.contains("VMCTL_HOST_SHORT=media-stack"));
        assert!(env.contains("VMCTL_HTTP_BASE_URL_SHORT=http://media-stack"));
        assert!(!env.contains("VMCTL_SEARCHDOMAIN="));
        assert!(!env.contains("VMCTL_HOST_FQDN="));
        assert!(!env.contains("VMCTL_HTTP_BASE_URL_FQDN="));
        assert!(!env.contains("JELLIO_STREMIO_MANIFEST_URL_LAN"));
    }

    #[test]
    fn streaming_validation_fixture_checks_tizen_catalogs() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-validate-streaming-stack.sh"
        );
        assert!(script.contains("TIZEN_STREMIO_USER_AGENT"));
        assert!(script.contains("settings/public\" \"seerr proxied public settings"));
        assert!(script.contains("settings_path = config_root / \"seerr\" / \"settings.json\""));
        assert!(script.contains("settings payload is missing applicationTitle"));
        assert!(script.contains("settings payload has mediaServerLogin disabled"));
        assert!(script.contains("configured_jellyfin = bool"));
        assert!(script.contains("if not configured_jellyfin:"));
        assert!(script.contains("\"http://127.0.0.1:5055/api/v1/auth/jellyfin\""));
        assert!(script.contains("validation failed: Jellyfin login returned HTTP"));
        assert!(script.contains("missing qBittorrent download client"));
        assert!(script.contains("qBittorrent category mismatch"));
        assert!(script.contains("SABnzbd download client"));
        assert!(script.contains("host_whitelist missing {required}"));
        assert!(script.contains("local_ranges ="));
        assert!(script.contains("SABnzbd server subsection missing"));
        assert!(script.contains("still redirects to wizard"));
        assert!(script.contains("100.64.0.0/10"));
        assert!(script.contains("172.18.0.0/16"));
        assert!(script.contains("192.168.0.0/16"));
        assert!(script.contains("quality_profiles:"));
        assert!(script.contains("delete_old_custom_formats: true"));
        assert!(script.contains("trash_id: 72dae194fc92bf828f32cde7744e51a1"));
        assert!(script.contains("trash_id: d1d67249d3890e49bc12e275d989a7e9"));
        assert!(script.contains("Accept-Encoding\": \"identity"));
        assert!(script.contains("Tizen-like Jellio catalog requests returned empty metas"));
        assert!(script
            .contains("playback validation skipped because no movie catalog item is available"));
        assert!(script.contains("#EXTM3U"));
    }

    #[test]
    fn media_index_fixture_links_to_service_ports_and_manifest_files() {
        let index = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/media-index.html"
        );
        assert!(index.contains("Stremio Manifest (Tailscale)"));
        assert!(index.contains("data-service-port=\"8097\""));
        assert!(index.contains("data-service-port=\"5056\""));
        assert!(index.contains("data-service-port=\"8989\""));
        assert!(index.contains("data-service-port=\"7878\""));
        assert!(index.contains("data-service-port=\"9696\""));
        assert!(index.contains("data-service-port=\"8080\""));
        assert!(index.contains("data-service-port=\"8085\""));
        assert!(index.contains("data-service-path=\"/\""));
        assert!(index.contains("link.href = \"http://\" + host + \":\" + port + path;"));
        assert!(!index.contains("jellio-manifest.lan.url"));
        assert!(!index.contains("jellio-manifest.lan-ip.url"));
        assert!(!index.contains("jellio-manifest.lan-short.url"));
        assert!(index
            .contains("wire(\"jellio-manifest-tailscale-link\", \"/jellio-manifest.tailscale.url\");"));
        assert!(index.contains(
            "wire(\"jellio-manifest-cloudflare-link\", \"/jellio-manifest.cloudflare.url\");"
        ));
        assert!(!index.contains("Jellyfin (Auto Auth)"));
        assert!(!index.contains("Seerr (Auto Auth)"));
    }

    #[test]
    fn media_compose_fixture_pins_seerr_and_uses_init() {
        let compose = include_str!(
            "../tests/fixtures/example-workspace/resources/media-stack/docker-compose.media"
        );
        assert!(compose.contains("image: \"ghcr.io/seerr-team/seerr:v3.2.0\""));
        assert!(compose.contains("image: \"lscr.io/linuxserver/sabnzbd:latest\""));
        assert!(compose.contains("image: \"ghcr.io/recyclarr/recyclarr:latest\""));
        assert!(compose.contains("init: true"));
        assert!(!compose.contains("fallenbagel/"));
    }

    #[test]
    fn kodi_bootstrap_configures_tailscale_https_serve() {
        let script = include_str!(
            "../tests/fixtures/example-workspace/resources/kodi-htpc/scripts/bootstrap-kodi.sh"
        );
        assert!(script.contains("KODI_TAILSCALE_HTTPS_ENABLED"));
        assert!(script.contains("tailscale serve --yes --bg"));
        assert!(script.contains("tailscale serve reset"));
        assert!(script.contains("libcap2-bin"));
        assert!(script.contains("cap_net_bind_service=+ep"));
    }

    #[test]
    fn example_workspace_matches_fixtures() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let raw = include_str!("../../../vmctl.example.toml");
        let env = BTreeMap::from([
            ("PROXMOX_TOKEN_ID".to_string(), "root@pam!vmctl".to_string()),
            (
                "PROXMOX_TOKEN_SECRET".to_string(),
                "dummy-secret".to_string(),
            ),
            (
                "TAILSCALE_AUTH_KEY".to_string(),
                "tskey-fixture".to_string(),
            ),
            (
                "DEFAULT_SSH_KEY_FILE".to_string(),
                "/home/me/.ssh/id_ed25519.pub".to_string(),
            ),
            (
                "DEFAULT_SSH_PRIVATE_KEY_FILE".to_string(),
                "/home/me/.ssh/id_ed25519".to_string(),
            ),
            (
                "JELLYFIN_ADMIN_PASSWORD".to_string(),
                "dummy-jellyfin-password".to_string(),
            ),
        ]);
        let config_value = vmctl_config::resolve_toml_value(raw.parse().unwrap(), &env).unwrap();
        let config = vmctl_config::Config::from_value(config_value.clone()).unwrap();
        let registry =
            PackRegistry::load_with_config(&workspace_root.join("packs"), &config_value, &env)
                .unwrap();
        let desired = vmctl_planner::build_desired_state(config, &registry, None).unwrap();
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            root: root.clone(),
            generated_dir: PathBuf::from("generated"),
        };

        TerraformBackend
            .render(&workspace, &desired, &registry)
            .unwrap();

        let main: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("generated/main.tf.json")).unwrap(),
        )
        .unwrap();
        assert!(
            main["module"]["media_stack"]["resource"]["features"]["media_services"]["ui_routes"]
                .is_null()
        );
        assert!(
            main["module"]["media_stack"]["resource"]["features"]["media_services"]["upstreams"]
                .is_null()
        );

        let provider: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("generated/provider.tf.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            provider["provider"]["proxmox"]["api_token"],
            "${var.proxmox_api_token}"
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/docker-compose.media"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/docker-compose.media"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/caddyfile.media"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/caddyfile.media"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/media-index.html"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/media-index.html"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/media.env"),
            include_str!("../tests/fixtures/example-workspace/resources/media-stack/media.env"),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-node.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-node.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-media.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-jellyfin.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-jellyfin.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-jellystat.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-jellystat.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-qbittorrent.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-qbittorrent.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-arr.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-download-unpack.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-download-unpack.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-seerr.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-seerr.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-sabnzbd.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-sabnzbd.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-recyclarr.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-recyclarr.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-ui-routing.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-ui-routing.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/scripts/bootstrap-tailscale.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-tailscale.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/media-stack/tailscale-setup.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/media-stack/tailscale-setup.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/tailscale-gateway/scripts/bootstrap-node.sh"),
            include_str!("../tests/fixtures/example-workspace/resources/tailscale-gateway/scripts/bootstrap-node.sh"),
        );
        assert_file_fixture(
            &root.join("generated/resources/tailscale-gateway/scripts/bootstrap-tailscale.sh"),
            include_str!("../tests/fixtures/example-workspace/resources/tailscale-gateway/scripts/bootstrap-tailscale.sh"),
        );
        assert_file_fixture(
            &root.join("generated/resources/tailscale-gateway/tailscale-setup.sh"),
            include_str!("../tests/fixtures/example-workspace/resources/tailscale-gateway/tailscale-setup.sh"),
        );
        assert_file_fixture(
            &root.join("generated/resources/kodi-htpc/kodi.env"),
            include_str!("../tests/fixtures/example-workspace/resources/kodi-htpc/kodi.env"),
        );
        assert_file_fixture(
            &root.join("generated/resources/kodi-htpc/scripts/bootstrap-node.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/kodi-htpc/scripts/bootstrap-node.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/kodi-htpc/scripts/bootstrap-tailscale.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/kodi-htpc/scripts/bootstrap-tailscale.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/kodi-htpc/scripts/bootstrap-kodi.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/kodi-htpc/scripts/bootstrap-kodi.sh"
            ),
        );
        assert_file_fixture(
            &root.join("generated/resources/kodi-htpc/scripts/bootstrap-kodi-jellyfin.sh"),
            include_str!(
                "../tests/fixtures/example-workspace/resources/kodi-htpc/scripts/bootstrap-kodi-jellyfin.sh"
            ),
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    fn assert_file_fixture(path: &Path, expected: &str) {
        assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
    }

    fn unique_temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-backend-terraform-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }
}
