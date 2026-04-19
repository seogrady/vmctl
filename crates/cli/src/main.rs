use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use toml::Value;
use vmctl_backend::{EngineBackend, PlanMode, TargetSelector};
use vmctl_backend_terraform::TerraformBackend;
use vmctl_config::{resolve_config_path, Config};
use vmctl_dependencies::{backend_kind, CommandScope, DependencyPlan};
use vmctl_domain::{DesiredState, ImageKind, ImageSource, ResolvedImage, Workspace};
use vmctl_lockfile::Lockfile;
use vmctl_packs::PackRegistry;

#[derive(Debug, Parser)]
#[command(name = "vmctl", version, about = "Declarative Proxmox homelab manager")]
struct Cli {
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[arg(long, default_value = "packs")]
    packs: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Validate,
    Plan {
        target: Option<String>,
    },
    Apply {
        #[arg(long)]
        auto_approve: bool,
        #[arg(long)]
        verbose: bool,
        #[arg(long)]
        skip_provision: bool,
        #[arg(long)]
        no_image_ensure: bool,
        target: Option<String>,
    },
    Up {
        #[arg(long)]
        auto_approve: bool,
        #[arg(long)]
        verbose: bool,
        #[arg(long)]
        skip_provision: bool,
        #[arg(long)]
        no_image_ensure: bool,
        target: Option<String>,
    },
    Destroy {
        #[arg(long)]
        auto_approve: bool,
        target: String,
    },
    Import,
    Sync,
    Provision {
        target: Option<String>,
    },
    Backend {
        #[command(subcommand)]
        command: BackendCommand,
    },
    Images {
        #[command(subcommand)]
        command: ImagesCommand,
    },
    Passthrough {
        #[command(subcommand)]
        command: PassthroughCommand,
    },
}

#[derive(Debug, Subcommand)]
enum BackendCommand {
    Doctor,
    Plan {
        #[arg(long)]
        dry_run: bool,
        target: Option<String>,
    },
    Render,
    ShowState,
    Validate {
        #[arg(long)]
        live: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ImagesCommand {
    List,
    Plan,
    Ensure {
        #[arg(long)]
        dry_run: bool,
        image: Option<String>,
    },
    Doctor,
}

#[derive(Debug, Subcommand)]
enum PassthroughCommand {
    Doctor,
    Prepare {
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => init_workspace(cli.config.as_deref(), &cli.packs),
        Command::Validate => {
            let (_workspace, desired, _registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, None)?;
            check_dependencies(&desired, CommandScope::ValidateConfig)?;
            println!(
                "valid: {} resources, {} expanded roles",
                desired.resources.len(),
                desired.expansions.len()
            );
            Ok(())
        }
        Command::Plan { target } => {
            let (_workspace, desired, _registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, target.as_deref())?;
            check_dependencies(&desired, CommandScope::ValidateConfig)?;
            print!("{}", vmctl_render::render_plan(&desired));
            Ok(())
        }
        Command::Apply {
            auto_approve,
            verbose,
            skip_provision,
            no_image_ensure,
            target,
        } => apply_command(
            cli.config.as_deref(),
            &cli.packs,
            auto_approve,
            verbose,
            skip_provision,
            no_image_ensure,
            target.as_deref(),
            "apply",
        ),
        Command::Up {
            auto_approve,
            verbose,
            skip_provision,
            no_image_ensure,
            target,
        } => apply_command(
            cli.config.as_deref(),
            &cli.packs,
            auto_approve,
            verbose,
            skip_provision,
            no_image_ensure,
            target.as_deref(),
            "up",
        ),
        Command::Destroy {
            auto_approve,
            target,
        } => {
            require_auto_approve(auto_approve, "destroy")?;
            let (workspace, desired, _registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, None)?;
            check_dependencies(&desired, CommandScope::Destroy)?;
            let result = TerraformBackend.destroy(&workspace, &TargetSelector { name: target })?;
            println!("{}", result.summary);
            Ok(())
        }
        Command::Import => {
            let (workspace, desired, _registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, None)?;
            let lockfile_path = workspace.root.join("vmctl.lock");
            let lockfile = ensure_lockfile(&workspace, &desired)?;
            print!("{}", vmctl_import::summarize_lockfile(&lockfile_path)?);
            let state_path = workspace
                .root
                .join(&workspace.generated_dir)
                .join("terraform.tfstate");
            if state_path.exists() {
                print!(
                    "{}",
                    vmctl_import::summarize_terraform_state_with_lockfile(
                        &state_path,
                        Some(&lockfile)
                    )?
                );
            }
            Ok(())
        }
        Command::Sync => {
            let (workspace, desired, _registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, None)?;
            let lockfile = ensure_lockfile(&workspace, &desired)?;
            let summary = vmctl_import::compare_desired_to_lockfile(&desired, &lockfile);
            print!("{}", vmctl_import::render_sync_summary(&summary));
            Ok(())
        }
        Command::Provision { target } => {
            let (workspace, desired, registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, target.as_deref())?;
            check_dependencies(&desired, CommandScope::Provision)?;
            TerraformBackend.render(&workspace, &desired, &registry)?;
            let progress = ApplyProgress::new();
            let result = run_provision(&workspace, &desired, &progress)?;
            println!("{}", result.summary);
            Ok(())
        }
        Command::Backend { command } => match command {
            BackendCommand::Doctor => {
                let (workspace, desired, _registry) =
                    load_workspace(cli.config.as_deref(), &cli.packs, None)?;
                check_dependencies(&desired, CommandScope::Doctor)?;
                TerraformBackend.validate_backend(&workspace)
            }
            BackendCommand::Plan { dry_run, target } => {
                let (workspace, desired, registry) =
                    load_workspace(cli.config.as_deref(), &cli.packs, target.as_deref())?;
                let backend_workspace = if dry_run {
                    dry_run_workspace(&workspace)
                } else {
                    workspace.clone()
                };
                check_dependencies(&desired, CommandScope::Plan { dry_run })?;
                TerraformBackend.render_for_plan(
                    &backend_workspace,
                    &desired,
                    &registry,
                    if dry_run {
                        PlanMode::DryRun
                    } else {
                        PlanMode::Online
                    },
                )?;
                let result = TerraformBackend.plan(
                    &backend_workspace,
                    &desired,
                    if dry_run {
                        PlanMode::DryRun
                    } else {
                        PlanMode::Online
                    },
                )?;
                println!("{}", result.summary);
                Ok(())
            }
            BackendCommand::Render => {
                let (workspace, desired, registry) =
                    load_workspace(cli.config.as_deref(), &cli.packs, None)?;
                check_dependencies(&desired, CommandScope::Render)?;
                let result = TerraformBackend.render(&workspace, &desired, &registry)?;
                let lockfile = Lockfile::from_desired_with_artifacts(
                    &desired,
                    &workspace.root.join(&workspace.generated_dir),
                    &result.files,
                )?;
                lockfile.write_to_path(&workspace.root.join("vmctl.lock"))?;
                println!("{}; wrote vmctl.lock", result.summary);
                Ok(())
            }
            BackendCommand::ShowState => show_backend_state(&default_workspace()?),
            BackendCommand::Validate { live } => {
                let (workspace, desired, registry) =
                    load_workspace(cli.config.as_deref(), &cli.packs, None)?;
                let backend_workspace = if live {
                    workspace.clone()
                } else {
                    dry_run_workspace(&workspace)
                };
                check_dependencies(&desired, CommandScope::ValidateRendered { live })?;
                TerraformBackend.render_for_plan(
                    &backend_workspace,
                    &desired,
                    &registry,
                    if live {
                        PlanMode::Online
                    } else {
                        PlanMode::DryRun
                    },
                )?;
                let result = TerraformBackend.validate_rendered(&backend_workspace)?;
                println!("{}", result.summary);
                Ok(())
            }
        },
        Command::Images { command } => {
            let (_workspace, desired, _registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, None)?;
            match command {
                ImagesCommand::List => {
                    print!("{}", render_images_list(&desired));
                    Ok(())
                }
                ImagesCommand::Plan => {
                    print!("{}", render_images_plan(&desired));
                    Ok(())
                }
                ImagesCommand::Ensure { dry_run, image } => {
                    ensure_images(&desired, image.as_deref(), dry_run)
                }
                ImagesCommand::Doctor => {
                    print!("{}", render_images_plan(&desired));
                    Ok(())
                }
            }
        }
        Command::Passthrough { command } => {
            let (_workspace, desired, _registry) =
                load_workspace(cli.config.as_deref(), &cli.packs, None)?;
            match command {
                PassthroughCommand::Doctor => {
                    print!("{}", render_passthrough_doctor(&desired)?);
                    Ok(())
                }
                PassthroughCommand::Prepare { dry_run } => prepare_passthrough(&desired, dry_run),
            }
        }
    }
}

fn apply_command(
    config_path: Option<&Path>,
    packs_path: &Path,
    _auto_approve: bool,
    verbose: bool,
    skip_provision: bool,
    no_image_ensure: bool,
    target: Option<&str>,
    _command: &str,
) -> Result<()> {
    let (workspace, desired, registry) = load_workspace(config_path, packs_path, target)?;
    let progress = ApplyProgress::new();
    check_dependencies(&desired, CommandScope::Apply)?;
    if !skip_provision {
        check_dependencies(&desired, CommandScope::Provision)?;
    }

    println!("config: valid");
    if no_image_ensure {
        eprintln!("warning: skipping image ensure; missing images may fail during apply");
    } else {
        ensure_images(&desired, None, false)?;
    }
    prepare_passthrough_inner(&desired, false, false)?;
    ensure_passthrough_ready(&desired)?;

    let validation = progress.run("validating generated OpenTofu workspace", || {
        validate_live_backend(&workspace, &desired, &registry)
    })?;
    println!("{}", validation.summary);
    progress.run("checking interrupted-apply recovery", || {
        auto_recover_backend_state(&workspace, &desired)
    })?;

    let result = progress.run("applying OpenTofu plan", || {
        TerraformBackend.apply_with_output(&workspace, &desired, &registry, verbose)
    })?;
    progress.run("writing vmctl.lock", || {
        write_lockfile(&workspace, &desired)
    })?;
    println!("{}; wrote vmctl.lock", result.summary);
    if !skip_provision {
        let result = run_provision(&workspace, &desired, &progress)?;
        println!("{}", result.summary);
    }
    println!("vmctl apply complete");
    Ok(())
}

fn validate_live_backend(
    workspace: &Workspace,
    desired: &DesiredState,
    registry: &PackRegistry,
) -> Result<vmctl_backend::BackendValidation> {
    TerraformBackend.render_for_plan(workspace, desired, registry, PlanMode::Online)?;
    TerraformBackend.validate_rendered(workspace)
}

fn auto_recover_backend_state(workspace: &Workspace, desired: &DesiredState) -> Result<()> {
    let unmanaged = find_existing_unmanaged_resources(workspace, desired)?;
    if unmanaged.is_empty() {
        return Ok(());
    }

    for resource in unmanaged {
        println!(
            "state recovery: importing existing {} {} as {}",
            resource.kind, resource.vmid, resource.address
        );
        if let Err(error) = terraform_import(workspace, &resource.address, &resource.import_id) {
            eprintln!(
                "state recovery: import failed for `{}`:\n{}",
                resource.name, error
            );
            if confirm_destroy_existing_resource(&resource)? {
                destroy_existing_resource(&resource)?;
                println!(
                    "state recovery: destroyed existing {} {}; continuing with apply",
                    resource.kind, resource.vmid
                );
            } else {
                bail!("{}", manual_recovery_instructions(workspace, &resource));
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnmanagedBackendResource {
    name: String,
    kind: String,
    vmid: u32,
    address: String,
    import_id: String,
    destroy_command: String,
}

fn find_existing_unmanaged_resources(
    workspace: &Workspace,
    desired: &DesiredState,
) -> Result<Vec<UnmanagedBackendResource>> {
    let state_addresses = terraform_state_addresses(workspace)?;
    let default_node = default_proxmox_node(desired);
    let mut unmanaged = Vec::new();

    for resource in desired.normalized_resources.values() {
        let Some(vmid) = resource.vmid else {
            continue;
        };
        let Some(address) = backend_resource_address(&resource.name, &resource.kind) else {
            continue;
        };
        if state_addresses.contains(&address) || !proxmox_resource_exists(&resource.kind, vmid) {
            continue;
        }
        let node = resource
            .node
            .clone()
            .or_else(|| default_node.clone())
            .unwrap_or_else(|| "<node>".to_string());
        unmanaged.push(UnmanagedBackendResource {
            name: resource.name.clone(),
            kind: resource.kind.clone(),
            vmid,
            address,
            import_id: format!("{node}/{vmid}"),
            destroy_command: proxmox_destroy_command(&resource.kind, vmid),
        });
    }

    Ok(unmanaged)
}

fn backend_resource_address(name: &str, kind: &str) -> Option<String> {
    let module = sanitize_backend_module_name(name);
    match kind {
        "vm" => Some(format!(
            "module.{module}.proxmox_virtual_environment_vm.this[0]"
        )),
        "lxc" => Some(format!(
            "module.{module}.proxmox_virtual_environment_container.this[0]"
        )),
        _ => None,
    }
}

fn sanitize_backend_module_name(name: &str) -> String {
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

fn terraform_state_addresses(workspace: &Workspace) -> Result<BTreeSet<String>> {
    let generated = workspace.root.join(&workspace.generated_dir);
    let binary = terraform_binary_name();
    let output = std::process::Command::new(&binary)
        .args(["state", "list"])
        .current_dir(&generated)
        .output()
        .with_context(|| format!("failed to run `{binary} state list`"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        return Ok(stdout.lines().map(str::to_string).collect());
    }
    let combined = format!("{stdout}\n{stderr}");
    if combined.contains("No state file was found")
        || combined.contains("does not have a state")
        || combined.contains("No state file")
    {
        return Ok(BTreeSet::new());
    }
    bail!("`{binary} state list` failed:\n{combined}")
}

fn terraform_import(workspace: &Workspace, address: &str, import_id: &str) -> Result<()> {
    let generated = workspace.root.join(&workspace.generated_dir);
    let binary = terraform_binary_name();
    let output = std::process::Command::new(&binary)
        .args(["import", "-input=false", "-no-color", address, import_id])
        .current_dir(&generated)
        .output()
        .with_context(|| format!("failed to run `{binary} import {address} {import_id}`"))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("{stdout}\n{stderr}")
}

fn confirm_destroy_existing_resource(resource: &UnmanagedBackendResource) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }

    print!(
        "Destroy existing Proxmox {} {} for `{}` and recreate it? [y/N] ",
        resource.kind, resource.vmid, resource.name
    );
    std::io::stdout().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn destroy_existing_resource(resource: &UnmanagedBackendResource) -> Result<()> {
    let vmid = resource.vmid.to_string();
    match resource.kind.as_str() {
        "vm" => {
            let _ = std::process::Command::new("qm")
                .args(["stop", &vmid])
                .status();
            run_command_with_context(
                "qm",
                &["destroy", &vmid, "--purge"],
                "failed to destroy existing VM during state recovery",
            )
        }
        "lxc" => {
            let _ = std::process::Command::new("pct")
                .args(["stop", &vmid])
                .status();
            run_command_with_context(
                "pct",
                &["destroy", &vmid, "--purge"],
                "failed to destroy existing container during state recovery",
            )
        }
        _ => bail!("unsupported resource kind `{}`", resource.kind),
    }
}

fn manual_recovery_instructions(
    workspace: &Workspace,
    resource: &UnmanagedBackendResource,
) -> String {
    format!(
        "manual recovery required for `{}`.\n\nThe configured {} ID {} already exists in Proxmox, but it is not managed by OpenTofu state.\n\nOptions:\n- Adopt it into OpenTofu state:\n  cd {}\n  {} import '{}' '{}'\n- Or remove the existing Proxmox resource and rerun vmctl apply:\n  {}",
        resource.name,
        resource.kind,
        resource.vmid,
        workspace.root.join(&workspace.generated_dir).display(),
        terraform_binary_name(),
        resource.address,
        resource.import_id,
        resource.destroy_command,
    )
}

fn terraform_binary_name() -> String {
    if vmctl_util::command_exists("tofu") {
        "tofu".to_string()
    } else {
        "terraform".to_string()
    }
}

fn proxmox_resource_exists(kind: &str, vmid: u32) -> bool {
    let vmid = vmid.to_string();
    match kind {
        "vm" => command_succeeds("qm", &["status", &vmid]),
        "lxc" => command_succeeds("pct", &["status", &vmid]),
        _ => false,
    }
}

fn proxmox_destroy_command(kind: &str, vmid: u32) -> String {
    match kind {
        "vm" => format!("qm stop {vmid} || true; qm destroy {vmid} --purge"),
        "lxc" => format!("pct stop {vmid} || true; pct destroy {vmid} --purge"),
        _ => format!("remove Proxmox resource {vmid}"),
    }
}

fn render_images_list(desired: &DesiredState) -> String {
    let mut output = String::new();
    for image in desired.images.values() {
        output.push_str(&format!(
            "{}\tkind={:?}\tsource={:?}\tnode={}\tstorage={}\tcontent_type={}\tvolume_id={}\tstatus={}\n",
            image.name,
            image.kind,
            image.source,
            image.node,
            image.storage,
            image.content_type,
            image.volume_id,
            image_status_label(image)
        ));
    }
    if output.is_empty() {
        output.push_str("no images configured\n");
    }
    output
}

fn render_images_plan(desired: &DesiredState) -> String {
    let mut output = String::new();
    let required = required_image_names(desired);
    for image in desired.images.values() {
        let required_label = if required.contains(&image.name) {
            "required"
        } else {
            "unused"
        };
        let action = match (image.source, image.kind) {
            (ImageSource::Pveam, _) => format!(
                "ensure pveam template with `pveam download {} {}` if missing",
                image.storage, image.file_name
            ),
            (ImageSource::Url, _) => {
                "render provider download resource during backend render/apply".to_string()
            }
            (ImageSource::Existing, ImageKind::Vm) => image
                .vmid
                .map(|vmid| format!("validate existing VM/template with `qm status {vmid}`"))
                .unwrap_or_else(|| "validate existing VM/template before apply".to_string()),
            (ImageSource::Existing, ImageKind::Lxc) => {
                "validate existing Proxmox volume before apply".to_string()
            }
        };
        output.push_str(&format!(
            "{}\t{}\tstatus={}\taction={}\n",
            image.name,
            required_label,
            image_status_label(image),
            action
        ));
    }
    if output.is_empty() {
        output.push_str("no images configured\n");
    }
    output
}

fn ensure_images(desired: &DesiredState, selected: Option<&str>, dry_run: bool) -> Result<()> {
    let required = required_image_names(desired);
    for image in desired.images.values() {
        if let Some(selected) = selected {
            if image.name != selected {
                continue;
            }
        } else if !required.contains(&image.name) {
            continue;
        }
        ensure_image(image, dry_run)?;
    }
    if let Some(selected) = selected {
        if !desired.images.contains_key(selected) {
            bail!("image `{selected}` is not configured");
        }
    }
    Ok(())
}

fn ensure_image(image: &ResolvedImage, dry_run: bool) -> Result<()> {
    match image.source {
        ImageSource::Pveam => ensure_pveam_image(image, dry_run),
        ImageSource::Existing => ensure_existing_image(image, dry_run),
        ImageSource::Url => {
            println!(
                "image `{}` is provider-managed; backend apply will download {}",
                image.name, image.volume_id
            );
            Ok(())
        }
    }
}

fn ensure_pveam_image(image: &ResolvedImage, dry_run: bool) -> Result<()> {
    if image_is_present_with("pveam", &["list", &image.storage], &image.file_name) {
        println!("image `{}` present: {}", image.name, image.volume_id);
        return Ok(());
    }

    if dry_run {
        println!("pveam update");
        println!("pveam download {} {}", image.storage, image.file_name);
        return Ok(());
    }

    run_command("pveam", &["update"])?;
    run_command_with_context(
        "pveam",
        &["download", &image.storage, &image.file_name],
        &format!(
            "template `{}` is not available from pveam. Run `pveam available --section system | grep {}` on the Proxmox host and update vmctl.toml with the listed template name.",
            image.file_name,
            pveam_template_family(&image.file_name)
        ),
    )?;
    println!("image `{}` ensured: {}", image.name, image.volume_id);
    Ok(())
}

fn ensure_existing_image(image: &ResolvedImage, dry_run: bool) -> Result<()> {
    if image.kind == ImageKind::Vm {
        let vmid = image.vmid.with_context(|| {
            format!(
                "image `{}` is an existing VM image and requires vmid",
                image.name
            )
        })?;
        let vmid = vmid.to_string();
        if dry_run {
            println!("qm status {vmid}");
            return Ok(());
        }
        if command_succeeds("qm", &["status", &vmid]) {
            println!("image `{}` present: VMID {}", image.name, vmid);
            return Ok(());
        }
        bail!(
            "missing image `{}`: expected VM/template with VMID {}. Create the template or configure a different image.",
            image.name,
            vmid
        );
    }

    if dry_run {
        println!(
            "pvesm list {} --content {} | grep {}",
            image.storage, image.content_type, image.file_name
        );
        return Ok(());
    }

    if image_is_present_with(
        "pvesm",
        &["list", &image.storage, "--content", &image.content_type],
        &image.file_name,
    ) {
        println!("image `{}` present: {}", image.name, image.volume_id);
        Ok(())
    } else {
        bail!(
            "missing image `{}`: expected {}. Run `vmctl images ensure {}` or configure a different image.",
            image.name,
            image.volume_id,
            image.name
        );
    }
}

fn image_status_label(image: &ResolvedImage) -> &'static str {
    match image.source {
        ImageSource::Url => "provider-managed",
        ImageSource::Pveam => {
            if image_is_present_with("pveam", &["list", &image.storage], &image.file_name) {
                "present"
            } else {
                "missing"
            }
        }
        ImageSource::Existing => {
            if image.kind == ImageKind::Vm {
                let Some(vmid) = image.vmid else {
                    return "missing-vmid";
                };
                let vmid = vmid.to_string();
                return if command_succeeds("qm", &["status", &vmid]) {
                    "present"
                } else {
                    "missing"
                };
            }
            if image_is_present_with(
                "pvesm",
                &["list", &image.storage, "--content", &image.content_type],
                &image.file_name,
            ) {
                "present"
            } else {
                "missing"
            }
        }
    }
}

fn command_succeeds(command: &str, args: &[&str]) -> bool {
    std::process::Command::new(command)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn image_is_present_with(command: &str, args: &[&str], file_name: &str) -> bool {
    std::process::Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(file_name))
        .unwrap_or(false)
}

fn run_command(command: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(command)
        .args(args)
        .status()
        .with_context(|| format!("failed to run `{command} {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`{command} {}` failed", args.join(" "));
    }
    Ok(())
}

fn run_command_with_context(command: &str, args: &[&str], help: &str) -> Result<()> {
    let status = std::process::Command::new(command)
        .args(args)
        .status()
        .with_context(|| format!("failed to run `{command} {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`{command} {}` failed: {help}", args.join(" "));
    }
    Ok(())
}

fn pveam_template_family(file_name: &str) -> &str {
    file_name.split('_').next().unwrap_or(file_name)
}

fn render_passthrough_doctor(desired: &DesiredState) -> Result<String> {
    let requests = passthrough_requests(desired);
    if requests.is_empty() {
        return Ok("passthrough: no enabled PCI passthrough features\n".to_string());
    }

    let mut output = String::new();
    output.push_str("passthrough doctor\n");
    for request in &requests {
        output.push_str(&format!(
            "- {}: node={} mapping={} pci_device={}\n",
            request.resource,
            request.node.as_deref().unwrap_or("<default>"),
            request.mapping.as_deref().unwrap_or("none"),
            request.pci_device.as_deref().unwrap_or("none")
        ));
    }

    match check_passthrough_ready(&requests) {
        Ok(()) => output.push_str("status: ready\n"),
        Err(error) => output.push_str(&format!("status: not ready\n{error}\n")),
    }

    Ok(output)
}

fn ensure_passthrough_ready(desired: &DesiredState) -> Result<()> {
    let requests = passthrough_requests(desired);
    if requests.is_empty() {
        return Ok(());
    }
    check_passthrough_ready(&requests)?;
    println!("passthrough: ready");
    Ok(())
}

fn prepare_passthrough(desired: &DesiredState, dry_run: bool) -> Result<()> {
    prepare_passthrough_inner(desired, dry_run, true)
}

fn prepare_passthrough_inner(
    desired: &DesiredState,
    dry_run: bool,
    announce_empty: bool,
) -> Result<()> {
    let requests = passthrough_requests(desired);
    if requests.is_empty() {
        if announce_empty {
            println!("passthrough: no enabled PCI passthrough features");
        }
        return Ok(());
    }

    if !iommu_groups_present() {
        bail!(
            "IOMMU groups were not detected under /sys/kernel/iommu_groups. Enable VT-d/IOMMU in BIOS and ensure the Proxmox kernel has IOMMU enabled, then reboot before preparing PCI mappings."
        );
    }

    for request in requests {
        let Some(mapping) = &request.mapping else {
            bail!(
                "resource `{}` enables passthrough but has no `mapping`; set `mapping = \"intel-igpu\"` and `pci_device = \"00:02.0\"` first",
                request.resource
            );
        };
        let node = request
            .node
            .clone()
            .or_else(|| default_proxmox_node(desired))
            .with_context(|| {
                format!(
                    "resource `{}` passthrough mapping `{mapping}` requires a node; set backend.proxmox.node or resource node",
                    request.resource
                )
            })?;
        let Some(pci_device) = request.pci_device.as_deref() else {
            if pci_mapping_exists(mapping) {
                println!("passthrough mapping `{mapping}` already exists");
                continue;
            }
            bail!(
                "resource `{}` passthrough mapping `{mapping}` requires `pci_device` so vmctl can create the Proxmox PCI mapping",
                request.resource
            );
        };
        let pci_path = proxmox_pci_path(pci_device);
        let hardware_id = pci_hardware_id(pci_device).with_context(|| {
            format!("failed to resolve PCI vendor/device id for `{pci_device}` with lspci")
        })?;
        let subsystem_id = pci_subsystem_id(pci_device).with_context(|| {
            format!("failed to resolve PCI subsystem id for `{pci_device}` with lspci")
        })?;
        let iommu_group = pci_iommu_group(&pci_path).with_context(|| {
            format!("failed to resolve IOMMU group for PCI device `{pci_path}`")
        })?;
        let Some(iommu_group) = iommu_group else {
            bail!(
                "PCI device `{pci_path}` has no IOMMU group symlink at /sys/bus/pci/devices/{pci_path}/iommu_group. Enable IOMMU/VT-d in BIOS and reboot before preparing passthrough."
            );
        };
        let map = proxmox_pci_mapping_value(
            &node,
            &pci_path,
            &hardware_id,
            Some(&iommu_group),
            subsystem_id.as_deref(),
        );

        if dry_run {
            if pci_mapping_exists(mapping) {
                println!("pvesh set /cluster/mapping/pci/{mapping} --map {map}");
            } else {
                println!("pvesh create /cluster/mapping/pci --id {mapping} --map {map}");
            }
        } else if pci_mapping_exists(mapping) {
            run_command_with_context(
                "pvesh",
                &["set", &format!("/cluster/mapping/pci/{mapping}"), "--map", &map],
                "failed to update Proxmox PCI resource mapping. You need Mapping.Modify on /mapping/pci/<name>, and the device path/id/iommugroup/subsystem-id must match the host hardware.",
            )?;
            println!("updated passthrough mapping `{mapping}` for {pci_path} on {node}");
        } else {
            run_command_with_context(
                "pvesh",
                &["create", "/cluster/mapping/pci", "--id", mapping, "--map", &map],
                "failed to create Proxmox PCI resource mapping. You need Mapping.Modify on /mapping/pci/<name>, and the device path/id must match the host hardware.",
            )?;
            println!("created passthrough mapping `{mapping}` for {pci_path} on {node}");
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PassthroughRequest {
    resource: String,
    node: Option<String>,
    mapping: Option<String>,
    pci_device: Option<String>,
}

fn passthrough_requests(desired: &DesiredState) -> Vec<PassthroughRequest> {
    desired
        .normalized_resources
        .values()
        .filter_map(|resource| {
            let intel_igpu = resource
                .features
                .get("intel_igpu")
                .and_then(Value::as_table)?;
            let enabled = intel_igpu
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !enabled {
                return None;
            }
            Some(PassthroughRequest {
                resource: resource.name.clone(),
                node: resource.node.clone(),
                mapping: intel_igpu
                    .get("mapping")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                pci_device: intel_igpu
                    .get("pci_device")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

fn check_passthrough_ready(requests: &[PassthroughRequest]) -> Result<()> {
    let mut failures = Vec::new();

    if !iommu_groups_present() {
        failures.push(
            "IOMMU groups were not detected under /sys/kernel/iommu_groups. Enable VT-d/IOMMU in BIOS and ensure the Proxmox kernel has IOMMU enabled, then reboot."
                .to_string(),
        );
    }

    for request in requests {
        match (&request.mapping, &request.pci_device) {
            (Some(mapping), pci_device) => {
                if !pci_mapping_exists(mapping) {
                    failures.push(format!(
                        "resource `{}` requires PCI mapping `{mapping}`, but it was not found. Create it in Proxmox Datacenter -> Resource Mappings -> PCI Devices, or with pvesh, then grant the API token Mapping.Use on /mapping/pci/{mapping}.",
                        request.resource
                    ));
                } else if let Some(pci_device) = pci_device {
                    let pci_path = proxmox_pci_path(pci_device);
                    match pci_iommu_group(&pci_path) {
                        Ok(Some(iommu_group)) => {
                            if !pci_mapping_has_property(mapping, "iommugroup", &iommu_group) {
                                failures.push(format!(
                                    "resource `{}` requires PCI mapping `{mapping}`, but the mapping is missing expected iommugroup `{iommu_group}` for `{pci_path}`. Run `vmctl passthrough prepare` to update the mapping, then rerun `vmctl apply`.",
                                    request.resource
                                ));
                            }
                        }
                        Ok(None) => failures.push(format!(
                            "resource `{}` requires PCI device `{pci_path}`, but no IOMMU group symlink was found under /sys/bus/pci/devices/{pci_path}/iommu_group. Enable IOMMU/VT-d in BIOS and reboot.",
                            request.resource
                        )),
                        Err(error) => failures.push(format!(
                            "resource `{}` failed to inspect IOMMU group for `{pci_path}`: {error}",
                            request.resource
                        )),
                    }
                    match pci_subsystem_id(pci_device) {
                        Ok(Some(subsystem_id)) => {
                            if !pci_mapping_has_property(mapping, "subsystem-id", &subsystem_id) {
                                failures.push(format!(
                                    "resource `{}` requires PCI mapping `{mapping}`, but the mapping is missing expected subsystem-id `{subsystem_id}` for `{pci_path}`. Run `vmctl passthrough prepare` to update the mapping, then rerun `vmctl apply`.",
                                    request.resource
                                ));
                            }
                        }
                        Ok(None) => failures.push(format!(
                            "resource `{}` requires PCI device `{pci_path}`, but no subsystem id was found in `lspci -nnvs {pci_device}` output.",
                            request.resource
                        )),
                        Err(error) => failures.push(format!(
                            "resource `{}` failed to inspect subsystem id for `{pci_path}`: {error}",
                            request.resource
                        )),
                    }
                }
            }
            (None, Some(pci_device)) => failures.push(format!(
                "resource `{}` uses raw PCI device `{pci_device}`. Raw hostpci requires an interactive root@pam session and cannot be set by API token. Create a Proxmox PCI resource mapping and use `mapping = \"...\"` instead.",
                request.resource
            )),
            (None, None) => failures.push(format!(
                "resource `{}` enables intel_igpu but does not set `mapping`. Create a Proxmox PCI resource mapping and set `mapping = \"<name>\"`.",
                request.resource
            )),
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        bail!("passthrough preflight failed:\n- {}", failures.join("\n- "))
    }
}

fn iommu_groups_present() -> bool {
    std::fs::read_dir("/sys/kernel/iommu_groups")
        .ok()
        .and_then(|mut entries| entries.next())
        .is_some()
}

fn pci_mapping_exists(mapping: &str) -> bool {
    command_output_contains(
        "pvesh",
        &["get", &format!("/cluster/mapping/pci/{mapping}")],
        mapping,
    ) || command_output_contains("pvesh", &["get", "/cluster/mapping/pci"], mapping)
}

fn pci_mapping_has_property(mapping: &str, key: &str, expected: &str) -> bool {
    let Some(output) = command_output(
        "pvesh",
        &["get", &format!("/cluster/mapping/pci/{mapping}")],
    ) else {
        return false;
    };
    output.contains(key)
        && (output.contains(&format!("{key}={expected}"))
            || output.contains(&format!("{key}: {expected}"))
            || output.contains(&format!("\"{key}\":{expected}"))
            || output.contains(&format!("\"{key}\":\"{expected}\""))
            || output.contains(&format!("{key} {expected}")))
}

fn default_proxmox_node(desired: &DesiredState) -> Option<String> {
    desired
        .backend
        .settings
        .get("proxmox")
        .and_then(Value::as_table)
        .and_then(|proxmox| proxmox.get("node"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn proxmox_pci_path(pci_device: &str) -> String {
    if pci_device.matches(':').count() >= 2 {
        pci_device.to_string()
    } else {
        format!("0000:{pci_device}")
    }
}

fn proxmox_pci_mapping_value(
    node: &str,
    pci_path: &str,
    hardware_id: &str,
    iommu_group: Option<&str>,
    subsystem_id: Option<&str>,
) -> String {
    let mut fields = vec![
        format!("node={node}"),
        format!("path={pci_path}"),
        format!("id={hardware_id}"),
    ];
    if let Some(iommu_group) = iommu_group {
        fields.push(format!("iommugroup={iommu_group}"));
    }
    if let Some(subsystem_id) = subsystem_id {
        fields.push(format!("subsystem-id={subsystem_id}"));
    }
    fields.join(",")
}

fn pci_iommu_group(pci_path: &str) -> Result<Option<String>> {
    let path = Path::new("/sys/bus/pci/devices")
        .join(pci_path)
        .join("iommu_group");
    if !path.exists() {
        return Ok(None);
    }
    let target = std::fs::read_link(&path)
        .with_context(|| format!("failed to read IOMMU group symlink {}", path.display()))?;
    Ok(target
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string))
}

fn pci_hardware_id(pci_device: &str) -> Result<String> {
    let output = std::process::Command::new("lspci")
        .args(["-nns", pci_device])
        .output()
        .with_context(|| format!("failed to run `lspci -nns {pci_device}`"))?;
    if !output.status.success() {
        bail!("`lspci -nns {pci_device}` failed");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_lspci_hardware_id(&stdout)
        .map(str::to_string)
        .with_context(|| format!("could not parse vendor/device id from lspci output: {stdout}"))
}

fn pci_subsystem_id(pci_device: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("lspci")
        .args(["-nnvs", pci_device])
        .output()
        .with_context(|| format!("failed to run `lspci -nnvs {pci_device}`"))?;
    if !output.status.success() {
        bail!("`lspci -nnvs {pci_device}` failed");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_lspci_subsystem_id(&stdout).map(str::to_string))
}

fn parse_lspci_hardware_id(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .map(|part| part.trim_matches(&['[', ']'][..]))
        .find(|part| part.len() == 9 && part.as_bytes().get(4) == Some(&b':'))
}

fn parse_lspci_subsystem_id(output: &str) -> Option<&str> {
    output
        .lines()
        .find(|line| line.trim_start().starts_with("Subsystem:"))
        .and_then(parse_last_pci_id)
}

fn parse_last_pci_id(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .map(|part| part.trim_matches(&['[', ']'][..]))
        .filter(|part| part.len() == 9 && part.as_bytes().get(4) == Some(&b':'))
        .last()
}

fn command_output_contains(command: &str, args: &[&str], needle: &str) -> bool {
    command_output(command, args)
        .map(|output| output.contains(needle))
        .unwrap_or(false)
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

#[derive(Debug, Clone)]
struct ApplyProgress {
    enabled: bool,
}

impl ApplyProgress {
    fn new() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
        }
    }

    fn run<T>(&self, message: impl Into<String>, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let message = message.into();
        let spinner = self.start(message.clone());
        match action() {
            Ok(value) => {
                spinner.finish_ok(&message);
                Ok(value)
            }
            Err(error) => {
                spinner.finish_err(&message);
                Err(error)
            }
        }
    }

    fn start(&self, message: impl Into<String>) -> ProgressSpinner {
        ProgressSpinner::start(message.into(), self.enabled)
    }
}

struct ProgressSpinner {
    enabled: bool,
    message: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ProgressSpinner {
    fn start(message: String, enabled: bool) -> Self {
        if !enabled {
            eprintln!(".. {message}");
            return Self {
                enabled,
                message: Arc::new(Mutex::new(message)),
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }

        let message = Arc::new(Mutex::new(message));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_message = Arc::clone(&message);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let frames = ["|", "/", "-", "\\"];
            let mut index = 0usize;
            while !thread_stop.load(Ordering::SeqCst) {
                let current = thread_message
                    .lock()
                    .map(|message| message.clone())
                    .unwrap_or_else(|_| "working".to_string());
                eprint!("\r{} {}", frames[index % frames.len()], current);
                let _ = std::io::stderr().flush();
                index = index.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(120));
            }
        });

        Self {
            enabled,
            message,
            stop,
            handle: Some(handle),
        }
    }

    fn set_message(&self, message: impl Into<String>) {
        if let Ok(mut current) = self.message.lock() {
            *current = message.into();
        }
    }

    fn status(&self, prefix: &str, message: impl AsRef<str>) {
        if self.enabled {
            eprint!("\r\x1b[2K");
        }
        eprintln!("{prefix} {}", message.as_ref());
        if self.enabled {
            let _ = std::io::stderr().flush();
        }
    }

    fn finish_ok(mut self, message: &str) {
        self.finish("[ok]", message);
    }

    fn finish_err(mut self, message: &str) {
        self.finish("[failed]", message);
    }

    fn finish(&mut self, prefix: &str, message: &str) {
        if self.enabled {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            eprint!("\r\x1b[2K");
        }
        eprintln!("{prefix} {message}");
        self.enabled = false;
    }
}

impl Drop for ProgressSpinner {
    fn drop(&mut self) {
        if self.enabled {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

fn required_image_names(desired: &DesiredState) -> BTreeSet<String> {
    desired
        .resources
        .iter()
        .filter_map(|resource| resource.image.clone())
        .collect()
}

fn load_workspace(
    config_path: Option<&Path>,
    packs_path: &Path,
    target: Option<&str>,
) -> Result<(Workspace, DesiredState, PackRegistry)> {
    let workspace = default_workspace()?;
    let config_path = resolve_config_path(config_path)?.path;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let process_env = std::env::vars().collect();
    let config = Config::from_toml(&raw, &process_env)?;
    let registry = PackRegistry::load(packs_path)?;
    let desired = vmctl_planner::build_desired_state(config, &registry, target)?;
    Ok((workspace, desired, registry))
}

fn default_workspace() -> Result<Workspace> {
    Ok(Workspace {
        root: std::env::current_dir().context("failed to read current directory")?,
        generated_dir: PathBuf::from("backend/generated/workspace"),
    })
}

fn dry_run_workspace(workspace: &Workspace) -> Workspace {
    Workspace {
        root: workspace.root.clone(),
        generated_dir: PathBuf::from("backend/generated/dry-run-workspace"),
    }
}

fn init_workspace(config_path: Option<&Path>, packs_path: &Path) -> Result<()> {
    let config_path = config_path.unwrap_or_else(|| Path::new("vmctl.toml"));
    if !config_path.exists() {
        std::fs::write(config_path, include_str!("../../../vmctl.example.toml"))
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }

    std::fs::create_dir_all(packs_path.join("roles"))?;
    std::fs::create_dir_all(packs_path.join("services"))?;
    std::fs::create_dir_all(packs_path.join("templates"))?;
    std::fs::create_dir_all(packs_path.join("scripts"))?;
    println!("initialized vmctl workspace");
    Ok(())
}

fn check_dependencies(desired: &DesiredState, scope: CommandScope) -> Result<()> {
    DependencyPlan::for_command(backend_kind(&desired.backend.kind), scope).verify(None)
}

fn run_provision(
    workspace: &Workspace,
    desired: &DesiredState,
    progress: &ApplyProgress,
) -> Result<vmctl_provision::ProvisionResult> {
    let plan = vmctl_provision::build_provision_plan(workspace, desired)?;
    if plan.steps.is_empty() {
        return Ok(vmctl_provision::ProvisionResult {
            summary: "provisioned 0 scripts".to_string(),
        });
    }

    let spinner = progress.start("provisioning resources");
    let resource_totals = provision_resource_totals(&plan);
    let mut resource_completed = BTreeMap::<String, usize>::new();
    let result = vmctl_provision::run_provision_plan_with_progress(
        &plan,
        &vmctl_provision::SystemSshExecutor,
        |event| match event {
            vmctl_provision::ProvisionEvent::StepStarted { step, index, total } => {
                spinner.set_message(format!(
                    "provisioning {}/{}: {} via {}",
                    index,
                    total,
                    step.resource,
                    script_name(step)
                ));
            }
            vmctl_provision::ProvisionEvent::UploadStarted {
                step,
                attempt,
                total_attempts,
            } => {
                spinner.set_message(format!(
                    "uploading {} files for {} (attempt {}/{})",
                    script_name(step),
                    step.resource,
                    attempt,
                    total_attempts
                ));
            }
            vmctl_provision::ProvisionEvent::ExecuteStarted {
                step,
                attempt,
                total_attempts,
            } => {
                spinner.set_message(format!(
                    "running {} on {} (attempt {}/{})",
                    script_name(step),
                    step.resource,
                    attempt,
                    total_attempts
                ));
            }
            vmctl_provision::ProvisionEvent::StepRetry {
                step,
                attempt,
                total_attempts,
                error,
            } => {
                spinner.set_message(format!(
                    "retrying {} for {} after attempt {}/{} failed: {}",
                    script_name(step),
                    step.resource,
                    attempt,
                    total_attempts,
                    error
                ));
            }
            vmctl_provision::ProvisionEvent::StepFinished { step, index, total } => {
                let script = script_name(step);
                spinner.status("[ok]", format!("ran {script} on {}", step.resource));
                let completed = resource_completed
                    .entry(step.resource.clone())
                    .and_modify(|value| *value += 1)
                    .or_insert(1);
                if Some(*completed) == resource_totals.get(&step.resource).copied() {
                    spinner.status(
                        "[ok]",
                        format!("provisioned {} ({} scripts)", step.resource, completed),
                    );
                }
                spinner.set_message(format!(
                    "provisioned {}/{}: {} via {}",
                    index, total, step.resource, script
                ));
            }
        },
    );

    match result {
        Ok(result) => {
            spinner.finish_ok(&result.summary);
            Ok(result)
        }
        Err(error) => {
            spinner.finish_err("provisioning failed");
            Err(error)
        }
    }
}

fn provision_resource_totals(plan: &vmctl_provision::ProvisionPlan) -> BTreeMap<String, usize> {
    let mut totals = BTreeMap::new();
    for step in &plan.steps {
        *totals.entry(step.resource.clone()).or_insert(0) += 1;
    }
    totals
}

fn script_name(step: &vmctl_provision::ProvisionStep) -> String {
    step.local_script
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("script")
        .to_string()
}

fn ensure_lockfile(workspace: &Workspace, desired: &DesiredState) -> Result<Lockfile> {
    let path = workspace.root.join("vmctl.lock");
    match Lockfile::read_optional_from_path(&path)? {
        Some(lockfile) => Ok(lockfile),
        None => {
            let lockfile = write_lockfile(workspace, desired)?;
            eprintln!("vmctl.lock was missing; regenerated {}", path.display());
            Ok(lockfile)
        }
    }
}

fn write_lockfile(workspace: &Workspace, desired: &DesiredState) -> Result<Lockfile> {
    let generated = workspace.root.join(&workspace.generated_dir);
    let artifacts = if generated.exists() {
        list_absolute_files(&generated)?
    } else {
        Vec::new()
    };
    let mut lockfile = Lockfile::from_desired_with_artifacts(desired, &generated, &artifacts)?;
    let state_path = generated.join("terraform.tfstate");
    if state_path.exists() {
        let reconciliation = vmctl_import::reconcile_terraform_state(&state_path, &lockfile)?;
        let existing = reconciliation
            .matched
            .into_iter()
            .map(|matched| matched.name)
            .collect::<BTreeSet<_>>();
        for resource in &mut lockfile.resources {
            resource.exists = existing.contains(&resource.name);
        }
    }
    lockfile.write_to_path(&workspace.root.join("vmctl.lock"))?;
    Ok(lockfile)
}

fn require_auto_approve(auto_approve: bool, command: &str) -> Result<()> {
    if !auto_approve {
        anyhow::bail!("`vmctl {command}` requires --auto-approve");
    }
    Ok(())
}

fn show_backend_state(workspace: &Workspace) -> Result<()> {
    let generated = workspace.root.join(&workspace.generated_dir);
    if !generated.exists() {
        anyhow::bail!(
            "no generated backend state found at {}; run `vmctl backend render` first",
            generated.display()
        );
    }

    println!("backend generated directory: {}", generated.display());
    for entry in list_files(&generated)? {
        println!("- {}", entry.display());
    }
    Ok(())
}

fn list_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn list_absolute_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_absolute_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_absolute_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_absolute_files(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(root, &path, files)?;
        } else {
            files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use clap::CommandFactory;
    use vmctl_domain::{BackendConfig, Resource};

    #[test]
    fn backend_validate_accepts_live_flag() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "vmctl",
            "--config",
            "vmctl.example.toml",
            "backend",
            "validate",
            "--live",
        ])
        .unwrap();

        match cli.command {
            Command::Backend {
                command: BackendCommand::Validate { live },
            } => assert!(live),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn destroy_requires_auto_approve() {
        let err = require_auto_approve(false, "destroy").unwrap_err();

        assert!(err.to_string().contains("requires --auto-approve"));
        assert!(require_auto_approve(true, "destroy").is_ok());
    }

    #[test]
    fn apply_and_up_accept_default_approval_behavior() {
        Cli::command().debug_assert();
        let apply = Cli::try_parse_from(["vmctl", "apply"]).unwrap();
        let up = Cli::try_parse_from(["vmctl", "up"]).unwrap();

        assert!(matches!(apply.command, Command::Apply { .. }));
        assert!(matches!(up.command, Command::Up { .. }));
    }

    #[test]
    fn apply_accepts_verbose_flag() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["vmctl", "apply", "--verbose"]).unwrap();

        assert!(matches!(cli.command, Command::Apply { verbose: true, .. }));
    }

    #[test]
    fn backend_resource_addresses_match_generated_modules() {
        assert_eq!(
            backend_resource_address("media-stack", "vm").as_deref(),
            Some("module.media_stack.proxmox_virtual_environment_vm.this[0]")
        );
        assert_eq!(
            backend_resource_address("tailscale-gateway", "lxc").as_deref(),
            Some("module.tailscale_gateway.proxmox_virtual_environment_container.this[0]")
        );
    }

    #[test]
    fn manual_recovery_instructions_include_import_and_destroy() {
        let root = unique_temp_dir();
        let workspace = Workspace {
            root: root.clone(),
            generated_dir: PathBuf::from("generated"),
        };
        let instructions = manual_recovery_instructions(
            &workspace,
            &UnmanagedBackendResource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                vmid: 210,
                address: "module.media_stack.proxmox_virtual_environment_vm.this[0]".to_string(),
                import_id: "mini/210".to_string(),
                destroy_command: "qm stop 210 || true; qm destroy 210 --purge".to_string(),
            },
        );

        assert!(instructions.contains(" import "));
        assert!(instructions.contains("module.media_stack.proxmox_virtual_environment_vm.this[0]"));
        assert!(instructions.contains("mini/210"));
        assert!(instructions.contains("qm destroy 210 --purge"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn passthrough_doctor_command_parses() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["vmctl", "passthrough", "doctor"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Passthrough {
                command: PassthroughCommand::Doctor
            }
        ));
    }

    #[test]
    fn passthrough_prepare_command_parses() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["vmctl", "passthrough", "prepare", "--dry-run"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Passthrough {
                command: PassthroughCommand::Prepare { dry_run: true }
            }
        ));
    }

    #[test]
    fn passthrough_requests_find_enabled_igpu_resources() {
        let mut features = BTreeMap::new();
        features.insert(
            "intel_igpu".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([
                ("enabled".to_string(), toml::Value::Boolean(true)),
                (
                    "mapping".to_string(),
                    toml::Value::String("intel-igpu".to_string()),
                ),
            ])),
        );
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    node: Some("mini".to_string()),
                    features,
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
        };

        assert_eq!(
            passthrough_requests(&desired),
            vec![PassthroughRequest {
                resource: "media-stack".to_string(),
                node: Some("mini".to_string()),
                mapping: Some("intel-igpu".to_string()),
                pci_device: None,
            }]
        );
    }

    #[test]
    fn passthrough_preflight_rejects_raw_pci() {
        let err = check_passthrough_ready(&[PassthroughRequest {
            resource: "media-stack".to_string(),
            node: Some("mini".to_string()),
            mapping: None,
            pci_device: Some("00:02.0".to_string()),
        }])
        .unwrap_err();

        assert!(err.to_string().contains("Raw hostpci requires"));
    }

    #[test]
    fn parses_lspci_vendor_device_id() {
        assert_eq!(
            parse_lspci_hardware_id(
                "00:02.0 VGA compatible controller [0300]: Intel Corporation Alder Lake-P GT2 [Iris Xe Graphics] [8086:46a6] (rev 0c)"
            ),
            Some("8086:46a6")
        );
    }

    #[test]
    fn parses_lspci_subsystem_id() {
        assert_eq!(
            parse_lspci_subsystem_id(
                "00:02.0 VGA compatible controller [0300]: Intel Corporation Alder Lake-P GT2 [8086:46a6] (rev 0c)\n\tSubsystem: Intel Corporation Device [8086:7270]\n"
            ),
            Some("8086:7270")
        );
    }

    #[test]
    fn proxmox_pci_mapping_includes_iommu_group_and_subsystem_id_when_known() {
        assert_eq!(
            proxmox_pci_mapping_value(
                "mini",
                "0000:00:02.0",
                "8086:46a6",
                Some("0"),
                Some("8086:7270")
            ),
            "node=mini,path=0000:00:02.0,id=8086:46a6,iommugroup=0,subsystem-id=8086:7270"
        );
    }

    #[test]
    fn write_lockfile_marks_missing_state_resources_absent() {
        let root = unique_temp_dir();
        let generated_dir = PathBuf::from("generated");
        std::fs::create_dir_all(root.join(&generated_dir)).unwrap();
        std::fs::write(
            root.join(&generated_dir).join("terraform.tfstate"),
            r#"{"resources":[]}"#,
        )
        .unwrap();
        let workspace = Workspace {
            root: root.clone(),
            generated_dir,
        };
        let desired = DesiredState {
            backend: BackendConfig::default(),
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
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
        };

        let lockfile = write_lockfile(&workspace, &desired).unwrap();

        assert!(!lockfile.resources[0].exists);

        std::fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-cli-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }
}
