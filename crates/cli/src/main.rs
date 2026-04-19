use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

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
            let result = run_provision(&workspace, &desired)?;
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
    ensure_passthrough_ready(&desired)?;

    let validation = validate_live_backend(&workspace, &desired, &registry)?;
    println!("{}", validation.summary);
    auto_recover_backend_state(&workspace, &desired)?;

    let result = TerraformBackend.apply_with_output(&workspace, &desired, &registry, verbose)?;
    write_lockfile(&workspace, &desired)?;
    println!("{}; wrote vmctl.lock", result.summary);
    if !skip_provision {
        let result = run_provision(&workspace, &desired)?;
        println!("{}", result.summary);
    }
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
    let requests = passthrough_requests(desired);
    if requests.is_empty() {
        println!("passthrough: no enabled PCI passthrough features");
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
        if pci_mapping_exists(mapping) {
            println!("passthrough mapping `{mapping}` already exists");
            continue;
        }

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
        let pci_device = request.pci_device.as_deref().with_context(|| {
            format!(
                "resource `{}` passthrough mapping `{mapping}` requires `pci_device` so vmctl can create the Proxmox PCI mapping",
                request.resource
            )
        })?;
        let pci_path = proxmox_pci_path(pci_device);
        let hardware_id = pci_hardware_id(pci_device).with_context(|| {
            format!("failed to resolve PCI vendor/device id for `{pci_device}` with lspci")
        })?;
        let map = format!("node={node},path={pci_path},id={hardware_id}");

        if dry_run {
            println!("pvesh create /cluster/mapping/pci --id {mapping} --map {map}");
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
            (Some(mapping), _) => {
                if !pci_mapping_exists(mapping) {
                    failures.push(format!(
                        "resource `{}` requires PCI mapping `{mapping}`, but it was not found. Create it in Proxmox Datacenter -> Resource Mappings -> PCI Devices, or with pvesh, then grant the API token Mapping.Use on /mapping/pci/{mapping}.",
                        request.resource
                    ));
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

fn parse_lspci_hardware_id(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .map(|part| part.trim_matches(&['[', ']'][..]))
        .find(|part| part.len() == 9 && part.as_bytes().get(4) == Some(&b':'))
}

fn command_output_contains(command: &str, args: &[&str], needle: &str) -> bool {
    std::process::Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout).contains(needle)
                || String::from_utf8_lossy(&output.stderr).contains(needle)
        })
        .unwrap_or(false)
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
) -> Result<vmctl_provision::ProvisionResult> {
    let plan = vmctl_provision::build_provision_plan(workspace, desired)?;
    vmctl_provision::run_provision_plan(&plan, &vmctl_provision::SystemSshExecutor)
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
