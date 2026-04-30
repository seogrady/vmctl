use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use toml::Value;
use vmctl_backend::{EngineBackend, PlanMode, TargetSelector};
use vmctl_backend_terraform::TerraformBackend;
use vmctl_config::{resolve_config_path, Config};
use vmctl_dependencies::{backend_kind, CommandScope, DependencyPlan};
use vmctl_domain::{DesiredState, ImageKind, ImageSource, ResolvedImage, Workspace};
use vmctl_hooks::{run_hooks, HookRunRequest};
use vmctl_lockfile::{LockedGitSource, LockedInlineSource, LockedSources, Lockfile};
use vmctl_modules::{
    DefaultSourceResolver, FsModuleIndexer, GitRepoManager, ModuleIndexer, ModuleLayer,
    ModuleOrigin, ModuleRegistry as ResolvedModuleRegistry, ModuleRegistryBuilder, RepoManager,
    SourceResolver, SourceSpec,
};
use vmctl_resources::ResourceRegistry;
use vmctl_services::ServiceRegistry;
use vmctl_util::command_runner::{self, CommandOptions, LogPrefix};

const GLOBAL_APPLY_TIMEOUT: Duration = Duration::from_secs(3600);
const MIN_HOST_MEMORY_RESERVE_MIB: u64 = 2048;

#[derive(Debug, Parser)]
#[command(name = "vmctl", version, about = "Declarative Proxmox homelab manager")]
struct Cli {
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[arg(long, default_value = "resources")]
    resources: PathBuf,

    #[arg(long, default_value = "services")]
    services: PathBuf,

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
        dry_run: bool,
        #[arg(long)]
        verbose: bool,
        #[arg(long)]
        skip_provision: bool,
        #[arg(long)]
        no_image_ensure: bool,
        #[arg(long)]
        no_start: bool,
        #[arg(long)]
        ignore_lock: bool,
        target: Option<String>,
    },
    Inspect {
        target: Option<String>,
    },
    Fetch {
        #[arg(long)]
        offline: bool,
    },
    Update {
        #[arg(long)]
        source: Option<String>,
    },
    Sources {
        #[arg(long)]
        module: Option<String>,
    },
    Debug {
        name: String,
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
        #[arg(long)]
        ignore_lock: bool,
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
    Run {
        command: String,
        #[arg(long, value_delimiter = ',')]
        target: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        group: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        parallel: bool,
        #[arg(long)]
        continue_on_error: bool,
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
    Grant {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        token: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => init_workspace(cli.config.as_deref(), &cli.resources, &cli.services),
        Command::Validate => {
            let (_workspace, desired, _registry, _service_registry) =
                load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
            check_dependencies(&desired, CommandScope::ValidateConfig)?;
            println!(
                "valid: {} resources, {} expanded resource compositions",
                desired.resources.len(),
                desired.expansions.len()
            );
            Ok(())
        }
        Command::Plan { target } => {
            let (_workspace, desired, _registry, _service_registry) = load_workspace(
                cli.config.as_deref(),
                &cli.resources,
                &cli.services,
                target.as_deref(),
            )?;
            check_dependencies(&desired, CommandScope::ValidateConfig)?;
            print!("{}", vmctl_render::render_plan(&desired));
            Ok(())
        }
        Command::Apply {
            auto_approve,
            dry_run,
            verbose,
            skip_provision,
            no_image_ensure,
            no_start,
            ignore_lock,
            target,
        } => apply_command(
            cli.config.as_deref(),
            &cli.resources,
            &cli.services,
            auto_approve,
            dry_run,
            verbose,
            skip_provision,
            no_image_ensure,
            no_start,
            ignore_lock,
            target.as_deref(),
            "apply",
        ),
        Command::Inspect { target } => inspect_command(
            cli.config.as_deref(),
            &cli.resources,
            &cli.services,
            target.as_deref(),
            InspectMode::Inspect,
        ),
        Command::Fetch { offline } => {
            fetch_sources_command(
                cli.config.as_deref(),
                &cli.resources,
                &cli.services,
                offline,
                None,
            )?;
            Ok(())
        }
        Command::Update { source } => {
            fetch_sources_command(
                cli.config.as_deref(),
                &cli.resources,
                &cli.services,
                false,
                source.as_deref(),
            )?;
            Ok(())
        }
        Command::Sources { module } => {
            sources_command(
                cli.config.as_deref(),
                &cli.resources,
                &cli.services,
                module.as_deref(),
            )?;
            Ok(())
        }
        Command::Debug { name } => inspect_command(
            cli.config.as_deref(),
            &cli.resources,
            &cli.services,
            Some(&name),
            InspectMode::Debug,
        ),
        Command::Up {
            auto_approve,
            verbose,
            skip_provision,
            no_image_ensure,
            ignore_lock,
            target,
        } => apply_command(
            cli.config.as_deref(),
            &cli.resources,
            &cli.services,
            auto_approve,
            false,
            verbose,
            skip_provision,
            no_image_ensure,
            false,
            ignore_lock,
            target.as_deref(),
            "up",
        ),
        Command::Destroy {
            auto_approve,
            target,
        } => {
            require_auto_approve(auto_approve, "destroy")?;
            let (workspace, desired, _registry, _service_registry) =
                load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
            check_dependencies(&desired, CommandScope::Destroy)?;
            let result = TerraformBackend.destroy(&workspace, &TargetSelector { name: target })?;
            println!("{}", result.summary);
            Ok(())
        }
        Command::Import => {
            let (workspace, desired, _registry, _service_registry) =
                load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
            let lockfile_path = workspace.root.join("vmctl.lock");
            let lockfile = ensure_lockfile(
                &workspace,
                &desired,
                cli.config.as_deref(),
                &cli.resources,
                &cli.services,
            )?;
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
            let (workspace, desired, _registry, _service_registry) =
                load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
            let lockfile = ensure_lockfile(
                &workspace,
                &desired,
                cli.config.as_deref(),
                &cli.resources,
                &cli.services,
            )?;
            let summary = vmctl_import::compare_desired_to_lockfile(&desired, &lockfile);
            print!("{}", vmctl_import::render_sync_summary(&summary));
            Ok(())
        }
        Command::Provision { target } => {
            let (workspace, desired, registry, _service_registry, source_fingerprint) =
                load_workspace_with_fingerprint(
                    cli.config.as_deref(),
                    &cli.resources,
                    &cli.services,
                    target.as_deref(),
                )?;
            check_dependencies(&desired, CommandScope::Provision)?;
            warn_workspace_sources_changed(
                cli.config.as_deref(),
                &cli.resources,
                &cli.services,
                &source_fingerprint,
            )?;
            let progress = ApplyProgress::new();
            let result = run_provision(
                &workspace,
                &desired,
                &registry,
                &_service_registry,
                &vmctl_provision::SystemSshExecutor,
                &progress,
            )?;
            println!("{}", result.summary);
            Ok(())
        }
        Command::Run {
            command,
            target,
            group,
            dry_run,
            parallel,
            continue_on_error,
        } => {
            let (config, desired, resource_registry, service_registry) =
                load_hook_workspace(cli.config.as_deref(), &cli.resources, &cli.services)?;
            let report = run_hooks(
                HookRunRequest {
                    command,
                    targets: target,
                    groups: group,
                    dry_run,
                    parallel,
                    continue_on_error,
                },
                &config,
                &desired,
                &resource_registry,
                &service_registry,
            )?;
            if dry_run {
                println!(
                    "planned {} hook nodes for `{}`",
                    report.order.len(),
                    report.command
                );
            } else {
                println!(
                    "ran {} hook nodes for `{}`",
                    report.executed.len(),
                    report.command
                );
            }
            Ok(())
        }
        Command::Backend { command } => match command {
            BackendCommand::Doctor => {
                let (workspace, desired, _registry, _service_registry) =
                    load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
                check_dependencies(&desired, CommandScope::Doctor)?;
                TerraformBackend.validate_backend(&workspace)
            }
            BackendCommand::Plan { dry_run, target } => {
                let (workspace, desired, registry, service_registry) = load_workspace(
                    cli.config.as_deref(),
                    &cli.resources,
                    &cli.services,
                    target.as_deref(),
                )?;
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
                    &service_registry,
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
                let (workspace, desired, registry, service_registry) =
                    load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
                check_dependencies(&desired, CommandScope::Render)?;
                let result =
                    TerraformBackend.render(&workspace, &desired, &registry, &service_registry)?;
                write_lockfile(
                    &workspace,
                    &desired,
                    cli.config.as_deref(),
                    &cli.resources,
                    &cli.services,
                )?;
                println!("{}; wrote vmctl.lock", result.summary);
                Ok(())
            }
            BackendCommand::ShowState => show_backend_state(&default_workspace()?),
            BackendCommand::Validate { live } => {
                let (workspace, desired, registry, service_registry) =
                    load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
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
                    &service_registry,
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
            let (_workspace, desired, _registry, _service_registry) =
                load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
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
            let (_workspace, desired, _registry, _service_registry) =
                load_workspace(cli.config.as_deref(), &cli.resources, &cli.services, None)?;
            match command {
                PassthroughCommand::Doctor => {
                    print!("{}", render_passthrough_doctor(&desired)?);
                    Ok(())
                }
                PassthroughCommand::Prepare { dry_run } => prepare_passthrough(&desired, dry_run),
                PassthroughCommand::Grant { dry_run, token } => {
                    grant_passthrough_permissions(&desired, dry_run, token.as_deref())
                }
            }
        }
    }
}

fn apply_command(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
    _auto_approve: bool,
    dry_run: bool,
    verbose: bool,
    skip_provision: bool,
    no_image_ensure: bool,
    no_start: bool,
    ignore_lock: bool,
    target: Option<&str>,
    _command: &str,
) -> Result<()> {
    let (workspace, mut desired, registry, service_registry, source_fingerprint) =
        load_workspace_with_fingerprint(config_path, resources_path, services_path, None)?;
    let mut provision_desired = if target.is_some() {
        let (
            _target_workspace,
            target_desired,
            _target_registry,
            _target_service_registry,
            _target_fingerprint,
        ) = load_workspace_with_fingerprint(config_path, resources_path, services_path, target)?;
        target_desired
    } else {
        desired.clone()
    };
    if no_start {
        disable_vm_start(&mut desired);
        disable_vm_start(&mut provision_desired);
    }
    if dry_run {
        return inspect_workspace(&workspace, &desired, InspectMode::Inspect);
    }
    let skip_provision = skip_provision || no_start;
    let progress = ApplyProgress::new();
    let guard = ApplyGuard::new(GLOBAL_APPLY_TIMEOUT);
    check_dependencies(&desired, CommandScope::Apply)?;
    if !skip_provision {
        check_dependencies(&desired, CommandScope::Provision)?;
    }

    println!("config: valid");
    validate_apply_preflight(&desired)?;
    if no_image_ensure {
        eprintln!("warning: skipping image ensure; missing images may fail during apply");
    } else {
        guard.checkpoint("image ensure")?;
        ensure_images(&desired, None, false)?;
    }
    guard.checkpoint("passthrough prepare")?;
    prepare_passthrough_inner(&desired, false, false)?;
    ensure_passthrough_ready(&desired)?;
    guard.checkpoint("runtime repair before apply")?;
    repair_existing_runtime_settings(&desired)?;

    guard.checkpoint("backend validation")?;
    let validation = progress.run("validating generated OpenTofu workspace", || {
        validate_live_backend(&workspace, &desired, &registry, &service_registry)
    })?;
    println!("{}", validation.summary);
    guard.checkpoint("state recovery")?;
    progress.run("checking interrupted-apply recovery", || {
        auto_recover_backend_state(&workspace, &desired)
    })?;

    guard.checkpoint("terraform apply")?;
    let result = if no_start {
        progress.run("rendering OpenTofu workspace in no-start mode", || {
            TerraformBackend.render(&workspace, &desired, &registry, &service_registry)?;
            Ok(vmctl_backend::ApplyResult {
                summary: "terraform apply skipped: no-start mode avoids Proxmox VM runtime changes"
                    .to_string(),
            })
        })?
    } else {
        progress.run("applying OpenTofu plan", || {
            TerraformBackend.apply_with_output_refresh_target(
                &workspace,
                &desired,
                &registry,
                &service_registry,
                verbose,
                true,
                target,
            )
        })?
    };
    warn_workspace_sources_changed(
        config_path,
        resources_path,
        services_path,
        &source_fingerprint,
    )?;
    guard.checkpoint("runtime repair after apply")?;
    repair_existing_runtime_settings(&desired)?;
    guard.checkpoint("lockfile write")?;
    progress.run("writing vmctl.lock", || {
        write_lockfile(
            &workspace,
            &desired,
            config_path,
            resources_path,
            services_path,
        )
    })?;
    if ignore_lock {
        eprintln!("warning: --ignore-lock is enabled; vmctl.lock was ignored as an input cache");
    }
    println!("{}; wrote vmctl.lock", result.summary);
    if !skip_provision {
        let result = run_provision(
            &workspace,
            &provision_desired,
            &registry,
            &service_registry,
            &vmctl_provision::SystemSshExecutor,
            &progress,
        )?;
        println!("{}", result.summary);
    }
    println!("vmctl apply complete");
    Ok(())
}

fn disable_vm_start(desired: &mut DesiredState) {
    disable_vm_start_with_status(desired, |vmid| current_vm_started(vmid));
}

fn disable_vm_start_with_status<F>(desired: &mut DesiredState, mut current_status: F)
where
    F: FnMut(u32) -> Option<bool>,
{
    for resource in desired.normalized_resources.values_mut() {
        if resource.kind == "vm" {
            resource.started = resource.vmid.and_then(&mut current_status).or(Some(false));
        }
    }
}

fn current_vm_started(vmid: u32) -> Option<bool> {
    let vmid = vmid.to_string();
    let output = safe_command_output("qm", &["status", &vmid], Duration::from_secs(10))?;
    parse_qm_started(&output)
}

fn parse_qm_started(output: &str) -> Option<bool> {
    let status = output
        .lines()
        .find_map(|line| line.strip_prefix("status:").map(str::trim))?;
    match status {
        "running" => Some(true),
        "stopped" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectMode {
    Inspect,
    Debug,
}

fn inspect_command(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
    target: Option<&str>,
    mode: InspectMode,
) -> Result<()> {
    let (workspace, desired, _registry, _service_registry) =
        load_workspace(config_path, resources_path, services_path, target)?;
    inspect_workspace(&workspace, &desired, mode)
}

fn inspect_workspace(
    _workspace: &Workspace,
    desired: &DesiredState,
    mode: InspectMode,
) -> Result<()> {
    println!("[vmctl] inspect mode: no qm create/start or Terraform apply will be executed");
    validate_apply_preflight(desired)?;
    println!("[vmctl] desired resources: {}", desired.resources.len());
    print!("{}", render_inspect_summary(desired));

    if let Some(output) = safe_command_output("qm", &["list"], Duration::from_secs(20)) {
        println!("[proxmox] qm list\n{}", output.trim());
    } else {
        println!("[proxmox] qm list unavailable; run inspect on a Proxmox host for live VM state");
    }

    for resource in desired.normalized_resources.values() {
        let Some(vmid) = resource.vmid else {
            continue;
        };
        let vmid = vmid.to_string();
        println!("[vmctl] resource `{}` vmid={vmid}", resource.name);
        match resource.kind.as_str() {
            "vm" => inspect_vm(resource, &vmid, mode),
            "lxc" => inspect_lxc(resource, &vmid),
            _ => {}
        }
    }

    Ok(())
}

fn inspect_vm(resource: &vmctl_domain::NormalizedResource, vmid: &str, mode: InspectMode) {
    let config = safe_command_output("qm", &["config", vmid], Duration::from_secs(20));
    let status = safe_command_output("qm", &["status", vmid], Duration::from_secs(10));
    if let Some(status) = status {
        println!("[proxmox] qm status {vmid}\n{}", status.trim());
    }
    if let Some(config) = config {
        println!("[proxmox] qm config {vmid}\n{}", config.trim());
        print_vm_diagnostics(resource, &config);
        if mode == InspectMode::Debug {
            print_recent_failure_hints(&config);
        }
    } else {
        println!("[proxmox] qm config {vmid} unavailable");
    }
}

fn inspect_lxc(resource: &vmctl_domain::NormalizedResource, vmid: &str) {
    let status = safe_command_output("pct", &["status", vmid], Duration::from_secs(10));
    if let Some(status) = status {
        println!("[proxmox] pct status {vmid}\n{}", status.trim());
    }
    if let Some(config) = safe_command_output("pct", &["config", vmid], Duration::from_secs(20)) {
        println!("[proxmox] pct config {vmid}\n{}", config.trim());
    } else {
        println!(
            "[proxmox] pct config {vmid} unavailable for `{}`",
            resource.name
        );
    }
}

fn render_inspect_summary(desired: &DesiredState) -> String {
    let mut output = String::new();
    for resource in desired.normalized_resources.values() {
        output.push_str(&format!(
            "[vmctl] desired {} `{}` vmid={} machine={} disk_interface={} iothread={} cloud_init={}\n",
            resource.kind,
            resource.name,
            resource
                .vmid
                .map(|vmid| vmid.to_string())
                .unwrap_or_else(|| "none".to_string()),
            resource.machine.as_deref().unwrap_or("default"),
            resource
                .disk_interface
                .as_deref()
                .unwrap_or("virtio"),
            resource.iothread.unwrap_or(true),
            if resource.cloud_init.is_some() { "enabled" } else { "none" }
        ));
    }
    output
}

fn print_vm_diagnostics(resource: &vmctl_domain::NormalizedResource, config: &str) {
    let machine = config_value(config, "machine").unwrap_or("default");
    let desired_machine = resource.machine.as_deref().unwrap_or("default");
    if machine != desired_machine && desired_machine != "default" {
        println!("[vmctl] mismatch: desired machine={desired_machine}, current machine={machine}");
    }

    let desired_disk = resource.disk_interface.as_deref().unwrap_or("virtio");
    if !config.lines().any(|line| line.starts_with(desired_disk)) {
        println!(
            "[vmctl] hint: desired disk interface `{desired_disk}` was not found in qm config"
        );
    }
    if resource.iothread.unwrap_or(true) && !desired_disk.starts_with("virtio") {
        println!("[vmctl] error: iothread=true requires a virtio disk interface");
    }
    if resource.cloud_init.is_some() && !config.lines().any(|line| line.starts_with("ide2:")) {
        println!("[vmctl] hint: cloud-init is desired but no ide2 cloud-init drive was found");
    }
}

fn print_recent_failure_hints(config: &str) {
    for line in config.lines() {
        if line.contains("q35") {
            println!("[vmctl] q35 hint: if start fails with `q35 machine model is not enabled`, verify q35 support on the Proxmox node");
        }
        if line.contains("iothread=1") && !line.starts_with("virtio") {
            println!("[vmctl] disk hint: Proxmox only accepts iothread on virtio disks or virtio-scsi-single");
        }
    }
}

fn config_value<'a>(config: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}:");
    config
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::trim))
}

fn safe_command_output(command: &str, args: &[&str], timeout: Duration) -> Option<String> {
    command_runner::run(
        CommandOptions::new(command, args.iter().copied())
            .timeout(timeout)
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .ok()
    .map(|output| output.combined)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VmRuntimeRepair {
    Memory { desired_mib: u32 },
    ScsiHardware { value: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LxcRuntimeRepair {
    TunDevice { slot: String },
    RootAuthorizedKey { local_key_file: String },
}

fn repair_existing_runtime_settings(desired: &DesiredState) -> Result<()> {
    for resource in desired.normalized_resources.values() {
        let Some(vmid) = resource.vmid else {
            continue;
        };
        let vmid_string = vmid.to_string();
        match resource.kind.as_str() {
            "vm" => {
                let Some(config) =
                    safe_command_output("qm", &["config", &vmid_string], Duration::from_secs(20))
                else {
                    continue;
                };
                for repair in vm_runtime_repairs(resource, &config) {
                    match repair {
                        VmRuntimeRepair::Memory { desired_mib } => {
                            let desired_mib_string = desired_mib.to_string();
                            run_command_with_context(
                                "qm",
                                &["set", &vmid_string, "--memory", &desired_mib_string],
                                "failed to update existing VM memory before Terraform apply",
                            )?;
                            println!(
                                "[proxmox] repaired `{}` memory to {} MiB before backend apply",
                                resource.name, desired_mib
                            );
                        }
                        VmRuntimeRepair::ScsiHardware { value } => {
                            run_command_with_context(
                                "qm",
                                &["set", &vmid_string, "--scsihw", value],
                                "failed to update SCSI controller for iothread compatibility",
                            )?;
                            println!(
                                "[proxmox] repaired `{}` SCSI controller to {value} for iothread compatibility",
                                resource.name
                            );
                        }
                    }
                }
            }
            "lxc" => {
                let Some(config) =
                    safe_command_output("pct", &["config", &vmid_string], Duration::from_secs(20))
                else {
                    continue;
                };
                for repair in lxc_runtime_repairs(resource, &config) {
                    match repair {
                        LxcRuntimeRepair::TunDevice { slot } => {
                            let slot_flag = format!("-{slot}");
                            run_command_with_context(
                                "pct",
                                &["set", &vmid_string, &slot_flag, "/dev/net/tun,mode=0666"],
                                "failed to configure /dev/net/tun passthrough for LXC",
                            )?;
                            println!(
                                "[proxmox] repaired `{}` LXC {} passthrough for /dev/net/tun",
                                resource.name, slot
                            );
                        }
                        LxcRuntimeRepair::RootAuthorizedKey { local_key_file } => {
                            let remote_key_path = "/tmp/vmctl-root-authorized_keys";
                            run_command_with_context(
                                "pct",
                                &["push", &vmid_string, &local_key_file, remote_key_path],
                                "failed to upload root SSH authorized key into LXC",
                            )?;
                            run_command_with_context(
                                "pct",
                                &[
                                    "exec",
                                    &vmid_string,
                                    "--",
                                    "sh",
                                    "-lc",
                                    "mkdir -p /root/.ssh && install -m 600 /tmp/vmctl-root-authorized_keys /root/.ssh/authorized_keys",
                                ],
                                "failed to install root SSH authorized key in LXC",
                            )?;
                            println!(
                                "[proxmox] repaired `{}` root SSH authorized_keys from {}",
                                resource.name, local_key_file
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn vm_runtime_repairs(
    resource: &vmctl_domain::NormalizedResource,
    config: &str,
) -> Vec<VmRuntimeRepair> {
    let mut repairs = Vec::new();
    if let Some(desired_mib) = resource.memory {
        if qm_config_memory_mib(config).is_some_and(|current_mib| current_mib != desired_mib) {
            repairs.push(VmRuntimeRepair::Memory { desired_mib });
        }
    }
    if qm_config_has_scsi_iothread(config) && qm_config_scsihw(config) != Some("virtio-scsi-single")
    {
        repairs.push(VmRuntimeRepair::ScsiHardware {
            value: "virtio-scsi-single",
        });
    }
    repairs
}

fn lxc_runtime_repairs(
    resource: &vmctl_domain::NormalizedResource,
    config: &str,
) -> Vec<LxcRuntimeRepair> {
    let mut repairs = Vec::new();
    if resource_requires_lxc_tun_passthrough(resource) && !lxc_has_tun_passthrough(config) {
        if let Some(slot) = lxc_next_device_slot(config) {
            repairs.push(LxcRuntimeRepair::TunDevice { slot });
        }
    }
    if let Some(local_key_file) = resource
        .cloud_init
        .as_ref()
        .and_then(|cloud_init| cloud_init.ssh_key_file.as_deref())
        .filter(|value| !value.trim().is_empty())
        .filter(|path| Path::new(path).is_file())
        .map(str::to_string)
    {
        repairs.push(LxcRuntimeRepair::RootAuthorizedKey { local_key_file });
    }
    repairs
}

fn qm_config_memory_mib(config: &str) -> Option<u32> {
    config_value(config, "memory")?.parse().ok()
}

fn qm_config_scsihw(config: &str) -> Option<&str> {
    config_value(config, "scsihw")
}

fn qm_config_has_scsi_iothread(config: &str) -> bool {
    config.lines().any(|line| {
        line.split_once(':')
            .map(|(key, value)| key.starts_with("scsi") && value.contains("iothread=1"))
            .unwrap_or(false)
    })
}

fn lxc_has_tun_passthrough(config: &str) -> bool {
    config
        .lines()
        .any(|line| line.contains("/dev/net/tun") && line.starts_with("dev"))
}

fn lxc_next_device_slot(config: &str) -> Option<String> {
    let used = config
        .lines()
        .filter_map(|line| {
            let (key, _value) = line.split_once(':')?;
            key.strip_prefix("dev")?.parse::<u8>().ok()
        })
        .collect::<BTreeSet<_>>();
    (0u8..16)
        .find(|slot| !used.contains(slot))
        .map(|slot| format!("dev{slot}"))
}

#[cfg(test)]
fn discover_with_retry<F>(attempts: u32, retry_delay: Duration, mut discover: F) -> Option<String>
where
    F: FnMut() -> Option<String>,
{
    if attempts == 0 {
        return None;
    }
    for attempt in 1..=attempts {
        if let Some(ip) = discover() {
            return Some(ip);
        }
        if attempt < attempts && !retry_delay.is_zero() {
            std::thread::sleep(retry_delay);
        }
    }
    None
}

#[cfg(test)]
fn primary_config_mac(config: &str) -> Option<&str> {
    let net0 = config_value(config, "net0")?;
    if let Some(value) = net0.split(',').find_map(|field| {
        field
            .strip_prefix("hwaddr=")
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }) {
        return Some(value);
    }
    let (model, _rest) = net0.split_once('=')?;
    if !["virtio", "e1000", "rtl8139", "vmxnet3"].contains(&model) {
        return None;
    }
    net0.split_once('=')?
        .1
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
fn primary_config_bridge(config: &str) -> Option<&str> {
    let net0 = config_value(config, "net0")?;
    net0.split(',').find_map(|field| {
        let value = field.strip_prefix("bridge=")?;
        Some(value.trim()).filter(|value| !value.is_empty())
    })
}

#[cfg(test)]
fn ip_for_mac_from_arp_scan(output: &str, mac: &str) -> Option<String> {
    let expected = mac.to_ascii_lowercase();
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let ip = fields.next()?;
        let found_mac = fields.next()?.to_ascii_lowercase();
        if found_mac == expected {
            Some(ip.to_string())
        } else {
            None
        }
    })
}

fn validate_apply_preflight(desired: &DesiredState) -> Result<()> {
    let host_memory_mib = if is_local_proxmox_host() {
        host_total_memory_mib()
    } else {
        None
    };
    validate_apply_preflight_with_host_memory(desired, host_memory_mib)
}

fn validate_apply_preflight_with_host_memory(
    desired: &DesiredState,
    host_memory_mib: Option<u64>,
) -> Result<()> {
    let mut failures = Vec::new();
    for (name, resource) in &desired.normalized_resources {
        let applies = desired
            .resources
            .iter()
            .find(|candidate| candidate.name == *name)
            .map(|resource| resource.applies())
            .unwrap_or(true);
        if !applies {
            continue;
        }
        if resource.kind == "vm" {
            if resource.machine.as_deref() == Some("q35")
                && vmctl_util::command_exists("qm")
                && !Path::new("/usr/share/qemu-server/pve-q35.cfg").exists()
            {
                failures.push(format!(
                    "resource `{}` requests machine=q35 but local Proxmox q35 config is missing",
                    resource.name
                ));
            }
            let raw_disk_interface = resource.disk_interface.as_deref().unwrap_or("virtio0");
            let Some(disk_interface) = canonical_vm_disk_interface(raw_disk_interface) else {
                failures.push(format!(
                    "resource `{}` sets disk_interface={raw_disk_interface}; expected slot syntax like virtio0/scsi0/sata0/ide0",
                    resource.name
                ));
                continue;
            };
            if resource.iothread.unwrap_or(true) && !disk_interface.starts_with("virtio") {
                failures.push(format!(
                    "resource `{}` sets iothread=true with disk_interface={disk_interface}; use virtio or set iothread=false",
                    resource.name
                ));
            }
            if let (Some(memory_mib), Some(host_memory_mib)) = (resource.memory, host_memory_mib) {
                let memory_mib = u64::from(memory_mib);
                if memory_mib + MIN_HOST_MEMORY_RESERVE_MIB > host_memory_mib {
                    failures.push(format!(
                        "resource `{}` memory={} MiB leaves less than {} MiB for the Proxmox host (host MemTotal={} MiB); lower VM memory or use a larger host before starting it",
                        resource.name,
                        memory_mib,
                        MIN_HOST_MEMORY_RESERVE_MIB,
                        host_memory_mib
                    ));
                }
            }
            if let Some(cloud_init) = &resource.cloud_init {
                if cloud_init
                    .ssh_key_file
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    failures.push(format!(
                        "resource `{}` cloud_init requires ssh_key_file",
                        resource.name
                    ));
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("apply preflight failed:\n- {}", failures.join("\n- "))
    }
}

fn is_local_proxmox_host() -> bool {
    vmctl_util::command_exists("qm") || Path::new("/etc/pve").exists()
}

fn host_total_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_memtotal_mib(&meminfo)
}

fn parse_memtotal_mib(meminfo: &str) -> Option<u64> {
    let kb = meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })?;
    Some(kb / 1024)
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

fn resource_requires_lxc_tun_passthrough(resource: &vmctl_domain::NormalizedResource) -> bool {
    resource.kind == "lxc"
        && resource
            .features
            .get("tailscale")
            .and_then(toml::Value::as_table)
            .and_then(|feature| feature.get("enabled"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
}

struct ApplyGuard {
    started: Instant,
    timeout: Duration,
}

impl ApplyGuard {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    fn checkpoint(&self, phase: &str) -> Result<()> {
        if self.started.elapsed() > self.timeout {
            bail!(
                "apply exceeded global timeout of {}s before {phase}",
                self.timeout.as_secs()
            );
        }
        Ok(())
    }
}

fn validate_live_backend(
    workspace: &Workspace,
    desired: &DesiredState,
    registry: &ResourceRegistry,
    service_registry: &ServiceRegistry,
) -> Result<vmctl_backend::BackendValidation> {
    TerraformBackend.render_for_plan(
        workspace,
        desired,
        registry,
        service_registry,
        PlanMode::Online,
    )?;
    TerraformBackend.validate_rendered(workspace)
}

fn auto_recover_backend_state(workspace: &Workspace, desired: &DesiredState) -> Result<()> {
    prune_missing_state_resources(workspace, desired)?;
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

fn prune_missing_state_resources(workspace: &Workspace, desired: &DesiredState) -> Result<()> {
    let state_addresses = terraform_state_addresses(workspace)?;
    let stale = missing_backend_state_resources(desired, &state_addresses, |kind, vmid| {
        proxmox_resource_exists(kind, vmid)
    });
    for stale_resource in stale {
        println!(
            "state recovery: removing stale state for missing {} {} ({})",
            stale_resource.kind, stale_resource.vmid, stale_resource.address
        );
        terraform_state_rm(workspace, &stale_resource.address)?;
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
    let result = command_runner::run(
        CommandOptions::new(&binary, ["state", "list"])
            .cwd(generated)
            .timeout(Duration::from_secs(120))
            .prefix(LogPrefix::Terraform),
    );
    let error = match result {
        Ok(output) => return Ok(output.stdout.lines().map(str::to_string).collect()),
        Err(error) => error.to_string(),
    };
    if error.contains("No state file was found")
        || error.contains("does not have a state")
        || error.contains("No state file")
    {
        return Ok(BTreeSet::new());
    }
    bail!("`{binary} state list` failed:\n{error}")
}

fn terraform_import(workspace: &Workspace, address: &str, import_id: &str) -> Result<()> {
    let generated = workspace.root.join(&workspace.generated_dir);
    let binary = terraform_binary_name();
    command_runner::run(
        CommandOptions::new(
            &binary,
            ["import", "-input=false", "-no-color", address, import_id],
        )
        .cwd(generated)
        .timeout(Duration::from_secs(600))
        .prefix(LogPrefix::Terraform),
    )
    .with_context(|| format!("failed to run `{binary} import {address} {import_id}`"))?;
    Ok(())
}

fn terraform_state_rm(workspace: &Workspace, address: &str) -> Result<()> {
    let generated = workspace.root.join(&workspace.generated_dir);
    let binary = terraform_binary_name();
    command_runner::run(
        CommandOptions::new(&binary, ["state", "rm", address])
            .cwd(generated)
            .timeout(Duration::from_secs(120))
            .prefix(LogPrefix::Terraform),
    )
    .with_context(|| format!("failed to run `{binary} state rm {address}`"))?;
    Ok(())
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
            let _ = command_runner::run(
                CommandOptions::new("qm", ["stop", &vmid])
                    .timeout(Duration::from_secs(60))
                    .prefix(LogPrefix::Proxmox),
            );
            run_command_with_context(
                "qm",
                &["destroy", &vmid, "--purge"],
                "failed to destroy existing VM during state recovery",
            )
        }
        "lxc" => {
            let _ = command_runner::run(
                CommandOptions::new("pct", ["stop", &vmid])
                    .timeout(Duration::from_secs(60))
                    .prefix(LogPrefix::Proxmox),
            );
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

fn missing_backend_state_resources<F>(
    desired: &DesiredState,
    state_addresses: &BTreeSet<String>,
    mut exists: F,
) -> Vec<UnmanagedBackendResource>
where
    F: FnMut(&str, u32) -> bool,
{
    let mut stale = Vec::new();
    for resource in desired.normalized_resources.values() {
        let Some(vmid) = resource.vmid else {
            continue;
        };
        let Some(address) = backend_resource_address(&resource.name, &resource.kind) else {
            continue;
        };
        if !state_addresses.contains(&address) {
            continue;
        }
        if exists(&resource.kind, vmid) {
            continue;
        }
        stale.push(UnmanagedBackendResource {
            name: resource.name.clone(),
            kind: resource.kind.clone(),
            vmid,
            address,
            import_id: String::new(),
            destroy_command: String::new(),
        });
    }
    stale
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
                .map(|vmid| {
                    format!(
                        "validate existing VM/template with `qm status {vmid}`; if missing, restore or recreate the template and update `images.{}` if the VMID changes",
                        image.name
                    )
                })
                .unwrap_or_else(|| {
                    "validate existing VM/template before apply; if missing, restore or recreate it and assign a VMID in vmctl.toml".to_string()
                }),
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
        ImageSource::Url => ensure_url_image(image, dry_run),
    }
}

fn ensure_url_image(image: &ResolvedImage, dry_run: bool) -> Result<()> {
    if url_image_present(image) {
        println!("image `{}` present: {}", image.name, image.volume_id);
        return Ok(());
    }
    let url = image
        .url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .with_context(|| format!("image `{}` source=url requires url", image.name))?;
    let path = url_image_path(image).with_context(|| {
        format!(
            "failed to resolve local path for image `{}` ({})",
            image.name, image.volume_id
        )
    })?;
    if dry_run {
        println!("curl -L --fail --output {} {}", path.display(), url);
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create image cache directory {}",
                parent.display()
            )
        })?;
    }
    let output_path = path.display().to_string();
    run_command_with_context(
        "curl",
        &[
            "-L",
            "--fail",
            "--show-error",
            "--output",
            &output_path,
            url,
        ],
        "failed to download URL image for local cache",
    )?;
    println!("image `{}` downloaded: {}", image.name, image.volume_id);
    Ok(())
}

fn url_image_present(image: &ResolvedImage) -> bool {
    url_image_path(image).is_some_and(|path| path.is_file())
        || image_is_present_with(
            "pvesm",
            &["list", &image.storage, "--content", &image.content_type],
            &image.file_name,
        )
}

fn url_image_path(image: &ResolvedImage) -> Option<PathBuf> {
    let output = command_runner::run(
        CommandOptions::new("pvesm", ["path", &image.volume_id])
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .ok()?;
    let path = output.stdout.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
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
        bail!("{}", missing_existing_vm_image_error(image, &vmid));
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

fn missing_existing_vm_image_error(image: &ResolvedImage, vmid: &str) -> String {
    format!(
        "missing image `{}`: expected VM/template with VMID {}. `source = \"existing\"` only validates that the VM/template already exists. Use `source = \"url\"` for a provider-downloaded image, or restore/recreate the template and keep the VMID.",
        image.name, vmid
    )
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
    command_runner::run(
        CommandOptions::new(command, args.iter().copied())
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .is_ok()
}

fn image_is_present_with(command: &str, args: &[&str], file_name: &str) -> bool {
    command_runner::run(
        CommandOptions::new(command, args.iter().copied())
            .timeout(Duration::from_secs(30))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .ok()
    .map(|output| output.stdout.contains(file_name))
    .unwrap_or(false)
}

fn run_command(command: &str, args: &[&str]) -> Result<()> {
    command_runner::run(
        CommandOptions::new(command, args.iter().copied())
            .timeout(Duration::from_secs(600))
            .prefix(LogPrefix::Proxmox),
    )
    .with_context(|| format!("failed to run `{command} {}`", args.join(" ")))?;
    Ok(())
}

fn run_command_with_context(command: &str, args: &[&str], help: &str) -> Result<()> {
    command_runner::run(
        CommandOptions::new(command, args.iter().copied())
            .timeout(Duration::from_secs(600))
            .prefix(LogPrefix::Proxmox),
    )
    .with_context(|| format!("`{command} {}` failed: {help}", args.join(" ")))?;
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

fn grant_passthrough_permissions(
    desired: &DesiredState,
    dry_run: bool,
    token: Option<&str>,
) -> Result<()> {
    let requests = passthrough_requests(desired);
    if requests.is_empty() {
        println!("passthrough: no enabled PCI passthrough features");
        return Ok(());
    }

    let token = token
        .map(str::to_string)
        .or_else(default_proxmox_token_principal)
        .with_context(|| {
            "pass `--token USER@REALM!TOKENID` or set TF_VAR_proxmox_api_token so vmctl can grant mapping permissions".to_string()
        })?;

    let mut granted = BTreeSet::new();
    for request in requests {
        let Some(mapping) = request.mapping.as_deref() else {
            continue;
        };
        if !granted.insert(mapping.to_string()) {
            continue;
        }

        let path = format!("/mapping/pci/{mapping}");
        if dry_run {
            println!("pveum acl modify {path} --tokens {token} --roles PVEMappingUser");
            continue;
        }

        run_command_with_context(
            "pveum",
            &[
                "acl",
                "modify",
                &path,
                "--tokens",
                &token,
                "--roles",
                "PVEMappingUser",
            ],
            "failed to grant Proxmox mapping permission. You need permission to modify ACLs, and the token path must be valid.",
        )?;
        println!("granted mapping permission on `{path}` to `{token}`");
    }

    Ok(())
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
    let output = command_runner::run(
        CommandOptions::new("lspci", ["-nns", pci_device])
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Vmctl)
            .fail_on_proxmox_patterns(false),
    )
    .with_context(|| format!("failed to run `lspci -nns {pci_device}`"))?;
    parse_lspci_hardware_id(&output.stdout)
        .map(str::to_string)
        .with_context(|| {
            format!(
                "could not parse vendor/device id from lspci output: {}",
                output.stdout
            )
        })
}

fn pci_subsystem_id(pci_device: &str) -> Result<Option<String>> {
    let output = command_runner::run(
        CommandOptions::new("lspci", ["-nnvs", pci_device])
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Vmctl)
            .fail_on_proxmox_patterns(false),
    )
    .with_context(|| format!("failed to run `lspci -nnvs {pci_device}`"))?;
    Ok(parse_lspci_subsystem_id(&output.stdout).map(str::to_string))
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
    command_runner::run(
        CommandOptions::new(command, args.iter().copied())
            .timeout(Duration::from_secs(20))
            .prefix(LogPrefix::Proxmox)
            .stream(false)
            .fail_on_proxmox_patterns(false),
    )
    .ok()
    .map(|output| output.combined)
}

fn default_proxmox_token_principal() -> Option<String> {
    env::var("TF_VAR_proxmox_api_token")
        .ok()
        .and_then(|value| token_principal_from_api_token(&value))
}

fn token_principal_from_api_token(value: &str) -> Option<String> {
    let principal = value.split_once('=')?.0.trim();
    if principal.is_empty() {
        None
    } else {
        Some(principal.to_string())
    }
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
    resources_path: &Path,
    services_path: &Path,
    target: Option<&str>,
) -> Result<(Workspace, DesiredState, ResourceRegistry, ServiceRegistry)> {
    let (workspace, desired, registry, service_registry, _) =
        load_workspace_with_fingerprint(config_path, resources_path, services_path, target)?;
    Ok((workspace, desired, registry, service_registry))
}

fn load_hook_workspace(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
) -> Result<(Config, DesiredState, ResourceRegistry, ServiceRegistry)> {
    let config_path = resolve_config_path(config_path)?.path;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let process_env = vmctl_config::process_env_with_shell_fallback(&std::env::vars().collect())?;
    let config_value = vmctl_config::resolve_toml_value(
        raw.parse().context("failed to parse vmctl TOML")?,
        &process_env,
    )?;
    let config = Config::from_value(config_value.clone())?;
    let (registry, service_registry, _) = load_module_registries(
        &config,
        &config_value,
        &process_env,
        resources_path,
        services_path,
    )?;
    let desired = vmctl_planner::build_desired_state_with_services(
        config.clone(),
        &registry,
        &service_registry,
        None,
    )?;
    Ok((config, desired, registry, service_registry))
}

fn load_workspace_with_fingerprint(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
    target: Option<&str>,
) -> Result<(
    Workspace,
    DesiredState,
    ResourceRegistry,
    ServiceRegistry,
    String,
)> {
    let workspace = default_workspace()?;
    let config_path = resolve_config_path(config_path)?.path;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let process_env = vmctl_config::process_env_with_shell_fallback(&std::env::vars().collect())?;
    let config_value = vmctl_config::resolve_toml_value(
        raw.parse().context("failed to parse vmctl TOML")?,
        &process_env,
    )?;
    let config = Config::from_value(config_value.clone())?;
    let (registry, service_registry, module_roots) = load_module_registries(
        &config,
        &config_value,
        &process_env,
        resources_path,
        services_path,
    )?;
    let desired = vmctl_planner::build_desired_state_with_services(
        config,
        &registry,
        &service_registry,
        target,
    )?;
    let source_fingerprint = workspace_source_fingerprint(&config_path, &module_roots)?;
    Ok((
        workspace,
        desired,
        registry,
        service_registry,
        source_fingerprint,
    ))
}

fn load_module_registries(
    config: &Config,
    config_value: &Value,
    process_env: &BTreeMap<String, String>,
    resources_path: &Path,
    services_path: &Path,
) -> Result<(ResourceRegistry, ServiceRegistry, Vec<PathBuf>)> {
    let workspace_root = std::env::current_dir().context("failed to read current directory")?;
    let resolver = DefaultSourceResolver;
    let indexer = FsModuleIndexer;
    let repo_manager = GitRepoManager::new(workspace_root.join("backend/cache/git"));
    let mut scanned_roots = Vec::<PathBuf>::new();

    let module_registry = build_module_registry(
        config,
        resources_path,
        services_path,
        &workspace_root,
        &resolver,
        &indexer,
        &repo_manager,
        false,
        &mut scanned_roots,
    )?;

    let resource_dirs = unique_parent_dirs(module_registry.resources.values().map(|location| {
        location
            .manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| location.manifest_path.clone())
    }));
    let service_dirs = unique_parent_dirs(module_registry.services.values().map(|location| {
        location
            .manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| location.manifest_path.clone())
    }));

    scanned_roots.extend(resource_dirs.clone());
    scanned_roots.extend(service_dirs.clone());
    scanned_roots.sort();
    scanned_roots.dedup();

    let resource_registry = ResourceRegistry::load_from_module_dirs(
        &resource_dirs,
        &service_dirs,
        config_value,
        process_env,
    )?;
    let service_registry = ServiceRegistry::load_from_module_dirs(&service_dirs)?;
    Ok((resource_registry, service_registry, scanned_roots))
}

fn build_module_registry(
    config: &Config,
    resources_path: &Path,
    services_path: &Path,
    workspace_root: &Path,
    resolver: &DefaultSourceResolver,
    indexer: &FsModuleIndexer,
    repo_manager: &GitRepoManager,
    offline: bool,
    scanned_roots: &mut Vec<PathBuf>,
) -> Result<ResolvedModuleRegistry> {
    let mut registry_builder = ModuleRegistryBuilder::default();

    for root in &config.sources.local {
        let SourceSpec::LocalPath { path: source_path } =
            resolver.parse(&root.to_string_lossy())?
        else {
            bail!("sources.local entries must resolve to local paths");
        };
        let path = resolve_workspace_path(&workspace_root, &source_path);
        scanned_roots.push(path.clone());
        registry_builder.add_indexed(
            indexer.index_collection(
                &path,
                &ModuleOrigin::Local {
                    collection_root: path.clone(),
                    module_dir: PathBuf::new(),
                },
            )?,
            ModuleLayer::Local,
        )?;
    }

    // Keep CLI overrides as explicit high-priority local collections.
    for cli_root in [resources_path, services_path] {
        let path = resolve_workspace_path(&workspace_root, cli_root);
        if !scanned_roots.iter().any(|root| root == &path) {
            scanned_roots.push(path.clone());
            registry_builder.add_indexed(
                indexer.index_collection(
                    &path,
                    &ModuleOrigin::Local {
                        collection_root: path.clone(),
                        module_dir: PathBuf::new(),
                    },
                )?,
                ModuleLayer::Local,
            )?;
        }
    }

    for git_source in &config.sources.git {
        let SourceSpec::Git {
            repo_url,
            ref_,
            subdir,
        } = resolver.parse(git_source)?
        else {
            bail!("sources.git entry must be a git URL, got `{git_source}`");
        };
        let resolved = repo_manager.ensure_repo(
            &vmctl_modules::RepoRef {
                repo_url: repo_url.clone(),
                ref_: ref_.clone(),
            },
            offline,
        )?;
        let collection_root = match subdir {
            Some(subdir) => resolved.checkout_root.join(subdir),
            None => resolved.checkout_root.clone(),
        };
        scanned_roots.push(collection_root.clone());
        registry_builder.add_indexed(
            indexer.index_collection(
                &collection_root,
                &ModuleOrigin::Git {
                    repo_url,
                    ref_,
                    commit: resolved.commit,
                    checkout_root: resolved.checkout_root,
                    module_dir: PathBuf::new(),
                },
            )?,
            ModuleLayer::Remote,
        )?;
    }

    Ok(registry_builder.build())
}

fn unique_parent_dirs<I>(dirs: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    let mut dirs = dirs.into_iter().collect::<Vec<_>>();
    dirs.sort();
    dirs.dedup();
    dirs
}

fn resolve_workspace_path(workspace_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    }
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

fn warn_workspace_sources_changed(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
    expected_fingerprint: &str,
) -> Result<()> {
    let config_path = resolve_config_path(config_path)?.path;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let process_env = vmctl_config::process_env_with_shell_fallback(&std::env::vars().collect())?;
    let config_value = vmctl_config::resolve_toml_value(
        raw.parse().context("failed to parse vmctl TOML")?,
        &process_env,
    )?;
    let config = Config::from_value(config_value.clone())?;
    let (_registry, _service_registry, module_roots) = load_module_registries(
        &config,
        &config_value,
        &process_env,
        resources_path,
        services_path,
    )?;
    let current_fingerprint = workspace_source_fingerprint(&config_path, &module_roots)?;
    if current_fingerprint != expected_fingerprint {
        eprintln!(
            "warning: workspace sources changed during apply; regenerating rendered artifacts before provisioning"
        );
    }
    Ok(())
}

fn workspace_source_fingerprint(config_path: &Path, module_roots: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    let config_bytes = std::fs::read(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    hasher.update(b"config\0");
    hasher.update(config_path.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(&config_bytes);
    hasher.update(b"\0");

    let mut files = Vec::new();
    for root in module_roots {
        files.extend(list_absolute_files(root)?);
    }
    files.sort();
    for path in files {
        let rel = module_roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok())
            .unwrap_or(path.as_path());
        let bytes =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(&bytes);
        hasher.update(b"\0");
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn fetch_sources_command(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
    offline: bool,
    source_filter: Option<&str>,
) -> Result<()> {
    let resolved_config_path = resolve_config_path(config_path)?.path;
    let raw = std::fs::read_to_string(&resolved_config_path)
        .with_context(|| format!("failed to read {}", resolved_config_path.display()))?;
    let process_env = vmctl_config::process_env_with_shell_fallback(&std::env::vars().collect())?;
    let config_value = vmctl_config::resolve_toml_value(
        raw.parse().context("failed to parse vmctl TOML")?,
        &process_env,
    )?;
    let config = Config::from_value(config_value)?;

    let workspace_root = std::env::current_dir().context("failed to read current directory")?;
    let resolver = DefaultSourceResolver;
    let repo_manager = GitRepoManager::new(workspace_root.join("backend/cache/git"));
    let refs = resolve_git_refs(&config, source_filter, &resolver)?;
    if refs.is_empty() {
        println!("no git sources found");
        return Ok(());
    }

    for repo in refs {
        let resolved = repo_manager.ensure_repo(&repo, offline)?;
        println!(
            "{}@{} => {} ({})",
            resolved.repo.repo_url,
            resolved.repo.ref_,
            resolved.commit,
            resolved.checkout_root.display()
        );
    }
    let (workspace, desired, _registry, _service_registry) = load_workspace(
        Some(resolved_config_path.as_path()),
        resources_path,
        services_path,
        None,
    )?;
    write_lockfile(
        &workspace,
        &desired,
        Some(resolved_config_path.as_path()),
        resources_path,
        services_path,
    )?;
    println!("updated vmctl.lock source pins");
    Ok(())
}

fn sources_command(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
    module_filter: Option<&str>,
) -> Result<()> {
    let config_path = resolve_config_path(config_path)?.path;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let process_env = vmctl_config::process_env_with_shell_fallback(&std::env::vars().collect())?;
    let config_value = vmctl_config::resolve_toml_value(
        raw.parse().context("failed to parse vmctl TOML")?,
        &process_env,
    )?;
    let config = Config::from_value(config_value)?;

    let workspace_root = std::env::current_dir().context("failed to read current directory")?;
    let resolver = DefaultSourceResolver;
    let repo_manager = GitRepoManager::new(workspace_root.join("backend/cache/git"));
    let indexer = FsModuleIndexer;
    let mut scanned_roots = Vec::new();
    let module_registry = build_module_registry(
        &config,
        resources_path,
        services_path,
        &workspace_root,
        &resolver,
        &indexer,
        &repo_manager,
        true,
        &mut scanned_roots,
    )?;
    let tracked_refs = resolve_git_refs(&config, None, &resolver)?;
    let cached = repo_manager.list_repos()?;
    if cached.is_empty() {
        println!("no cached git sources");
        return Ok(());
    }

    for repo in cached {
        println!(
            "- {}@{} commit {}",
            repo.repo.repo_url, repo.repo.ref_, repo.commit
        );
        let tracked = tracked_refs
            .iter()
            .any(|entry| entry.repo_url == repo.repo.repo_url && entry.ref_ == repo.repo.ref_);
        println!("  tracked: {}", if tracked { "yes" } else { "no" });
        println!("  checkout: {}", repo.checkout_root.display());

        let modules = indexer.index_collection(
            &repo.checkout_root,
            &ModuleOrigin::Git {
                repo_url: repo.repo.repo_url.clone(),
                ref_: repo.repo.ref_.clone(),
                commit: repo.commit.clone(),
                checkout_root: repo.checkout_root.clone(),
                module_dir: PathBuf::new(),
            },
        )?;
        if modules.is_empty() {
            println!("  modules: none");
            continue;
        }
        for module in modules {
            let kind = match module.kind {
                vmctl_modules::ModuleKind::Resource => "resource",
                vmctl_modules::ModuleKind::Service => "service",
            };
            println!(
                "  module: {kind}/{} ({})",
                module.name,
                module.module_dir.display()
            );
        }
    }

    if let Some(module_name) = module_filter {
        if let Some(location) = module_registry.resources.get(module_name) {
            println!("resolved resource `{module_name}` => {}", location.origin);
        } else if let Some(location) = module_registry.services.get(module_name) {
            println!("resolved service `{module_name}` => {}", location.origin);
        } else {
            println!("module `{module_name}` was not resolved");
        }
    }

    Ok(())
}

fn resolve_git_refs(
    config: &Config,
    source_filter: Option<&str>,
    resolver: &impl SourceResolver,
) -> Result<Vec<vmctl_modules::RepoRef>> {
    let mut refs = config
        .sources
        .git
        .iter()
        .map(|source| {
            let SourceSpec::Git {
                repo_url,
                ref_,
                subdir: _,
            } = resolver.parse(source)?
            else {
                bail!("sources.git entry must be a git URL, got `{source}`");
            };
            Ok(vmctl_modules::RepoRef { repo_url, ref_ })
        })
        .collect::<Result<Vec<_>>>()?;

    if let Some(filter) = source_filter {
        refs.retain(|repo| repo.repo_url.contains(filter) || repo.ref_ == filter);
    }
    refs.sort_by(|a, b| {
        a.repo_url
            .cmp(&b.repo_url)
            .then_with(|| a.ref_.cmp(&b.ref_))
    });
    refs.dedup_by(|left, right| left.repo_url == right.repo_url && left.ref_ == right.ref_);
    Ok(refs)
}

fn collect_locked_sources(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
) -> Result<LockedSources> {
    let config_path = resolve_config_path(config_path)?.path;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let process_env = vmctl_config::process_env_with_shell_fallback(&std::env::vars().collect())?;
    let config_value = vmctl_config::resolve_toml_value(
        raw.parse().context("failed to parse vmctl TOML")?,
        &process_env,
    )?;
    let config = Config::from_value(config_value.clone())?;

    let workspace_root = std::env::current_dir().context("failed to read current directory")?;
    let resolver = DefaultSourceResolver;
    let repo_manager = GitRepoManager::new(workspace_root.join("backend/cache/git"));
    let refs = resolve_git_refs(&config, None, &resolver)?;
    let mut git = Vec::new();
    for repo in refs {
        let resolved = repo_manager.ensure_repo(&repo, false).with_context(|| {
            format!(
                "failed to resolve git source {}@{} while writing lockfile",
                repo.repo_url, repo.ref_
            )
        })?;
        git.push(LockedGitSource {
            repo_url: resolved.repo.repo_url,
            r#ref: resolved.repo.ref_,
            commit: resolved.commit,
        });
    }
    git.sort_by(|a, b| {
        a.repo_url
            .cmp(&b.repo_url)
            .then_with(|| a.r#ref.cmp(&b.r#ref))
            .then_with(|| a.commit.cmp(&b.commit))
    });
    git.dedup_by(|left, right| {
        left.repo_url == right.repo_url && left.r#ref == right.r#ref && left.commit == right.commit
    });

    let mut inline = collect_inline_sources(&config_value);
    inline.sort_by(|a, b| a.config_path.cmp(&b.config_path));
    inline.dedup_by(|left, right| {
        left.config_path == right.config_path && left.digest == right.digest
    });

    let _ = (resources_path, services_path); // retained for CLI signature symmetry.
    Ok(LockedSources { git, inline })
}

fn collect_inline_sources(root: &Value) -> Vec<LockedInlineSource> {
    let mut out = Vec::new();
    collect_inline_sources_in_value(root, String::new(), &mut out);
    out
}

fn collect_inline_sources_in_value(value: &Value, path: String, out: &mut Vec<LockedInlineSource>) {
    match value {
        Value::Table(table) => {
            if table
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|source| source == "inline")
            {
                let digest = digest_toml_value(value);
                out.push(LockedInlineSource {
                    config_path: if path.is_empty() {
                        "root".to_string()
                    } else {
                        path.clone()
                    },
                    digest,
                });
            }
            for (key, nested) in table {
                let nested_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                collect_inline_sources_in_value(nested, nested_path, out);
            }
        }
        Value::Array(items) => {
            for (idx, nested) in items.iter().enumerate() {
                let nested_path = if path.is_empty() {
                    idx.to_string()
                } else {
                    format!("{path}.{idx}")
                };
                collect_inline_sources_in_value(nested, nested_path, out);
            }
        }
        _ => {}
    }
}

fn digest_toml_value(value: &Value) -> String {
    let rendered = toml::to_string(value).unwrap_or_else(|_| value.to_string());
    let digest = Sha256::digest(rendered.as_bytes());
    format!("sha256:{digest:x}")
}

fn init_workspace(
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
) -> Result<()> {
    let config_path = config_path.unwrap_or_else(|| Path::new("vmctl.toml"));
    if !config_path.exists() {
        std::fs::write(config_path, include_str!("../../../vmctl.example.toml"))
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }

    std::fs::create_dir_all(resources_path)?;
    std::fs::create_dir_all(services_path)?;
    println!("initialized vmctl workspace");
    Ok(())
}

fn check_dependencies(desired: &DesiredState, scope: CommandScope) -> Result<()> {
    DependencyPlan::for_command(backend_kind(&desired.backend.kind), scope).verify(None)
}

fn run_provision(
    workspace: &Workspace,
    desired: &DesiredState,
    registry: &ResourceRegistry,
    service_registry: &ServiceRegistry,
    executor: &dyn vmctl_provision::SshExecutor,
    progress: &ApplyProgress,
) -> Result<vmctl_provision::ProvisionResult> {
    refresh_rendered_workspace(workspace, desired, registry, service_registry)?;
    let plan = vmctl_provision::build_provision_plan(workspace, desired)?;
    if plan.steps.is_empty() {
        return Ok(vmctl_provision::ProvisionResult {
            summary: "provisioned 0 scripts".to_string(),
        });
    }

    let spinner = progress.start("provisioning resources");
    let resource_totals = provision_resource_totals(&plan);
    let mut resource_completed = BTreeMap::<String, usize>::new();
    let result =
        vmctl_provision::run_provision_plan_with_progress(&plan, executor, |event| match event {
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
        });

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

fn refresh_rendered_workspace(
    workspace: &Workspace,
    desired: &DesiredState,
    registry: &ResourceRegistry,
    service_registry: &ServiceRegistry,
) -> Result<()> {
    TerraformBackend.render(workspace, desired, registry, service_registry)?;
    Ok(())
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

fn ensure_lockfile(
    workspace: &Workspace,
    desired: &DesiredState,
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
) -> Result<Lockfile> {
    let path = workspace.root.join("vmctl.lock");
    match Lockfile::read_optional_from_path(&path)? {
        Some(lockfile) => Ok(lockfile),
        None => {
            let lockfile = write_lockfile(
                workspace,
                desired,
                config_path,
                resources_path,
                services_path,
            )?;
            eprintln!("vmctl.lock was missing; regenerated {}", path.display());
            Ok(lockfile)
        }
    }
}

fn write_lockfile(
    workspace: &Workspace,
    desired: &DesiredState,
    config_path: Option<&Path>,
    resources_path: &Path,
    services_path: &Path,
) -> Result<Lockfile> {
    let generated = workspace.root.join(&workspace.generated_dir);
    let artifacts = if generated.exists() {
        list_absolute_files(&generated)?
    } else {
        Vec::new()
    };
    let sources = collect_locked_sources(config_path, resources_path, services_path)?;
    let mut lockfile = Lockfile::from_desired_with_artifacts(desired, &generated, &artifacts)?
        .with_sources(sources);
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
    if !root.exists() {
        return Ok(files);
    }
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
    use std::collections::{BTreeMap, BTreeSet};

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
    fn apply_accepts_no_start_flag() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["vmctl", "apply", "--no-start"]).unwrap();

        assert!(matches!(cli.command, Command::Apply { no_start: true, .. }));
    }

    #[test]
    fn apply_accepts_ignore_lock_flag() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["vmctl", "apply", "--ignore-lock"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Apply {
                ignore_lock: true,
                ..
            }
        ));
    }

    #[test]
    fn apply_accepts_dry_run_flag() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["vmctl", "apply", "--dry-run", "media-stack"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Apply {
                dry_run: true,
                target: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn inspect_and_debug_commands_parse() {
        Cli::command().debug_assert();
        let inspect = Cli::try_parse_from(["vmctl", "inspect", "media-stack"]).unwrap();
        let debug = Cli::try_parse_from(["vmctl", "debug", "media-stack"]).unwrap();

        assert!(matches!(
            inspect.command,
            Command::Inspect { target: Some(_) }
        ));
        assert!(matches!(debug.command, Command::Debug { name } if name == "media-stack"));
    }

    #[test]
    fn inspect_workspace_is_safe_without_proxmox_commands() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    disk_interface: Some("virtio0".to_string()),
                    iothread: Some(true),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };
        let workspace = Workspace {
            root: PathBuf::from("/tmp/nonexistent-vmctl-test"),
            generated_dir: PathBuf::from("generated"),
        };

        inspect_workspace(&workspace, &desired, InspectMode::Inspect).unwrap();
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
    fn passthrough_grant_command_parses() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "vmctl",
            "passthrough",
            "grant",
            "--dry-run",
            "--token",
            "vmctl@pve!automation",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Command::Passthrough {
                command: PassthroughCommand::Grant {
                    dry_run: true,
                    token: Some(_)
                }
            }
        ));
    }

    #[test]
    fn parses_proxmox_token_principal() {
        assert_eq!(
            token_principal_from_api_token("vmctl@pve!automation=0123456789abcdef"),
            Some("vmctl@pve!automation".to_string())
        );
    }

    #[test]
    fn missing_existing_vm_image_error_mentions_recovery_steps() {
        let image = ResolvedImage {
            name: "ubuntu_24_cloud_image".to_string(),
            kind: ImageKind::Vm,
            source: ImageSource::Existing,
            node: "mini".to_string(),
            storage: "local-lvm".to_string(),
            content_type: "vm-template".to_string(),
            file_name: "ubuntu-24-04-cloudinit-template".to_string(),
            volume_id: "local-lvm:vm-template/ubuntu-24-04-cloudinit-template".to_string(),
            vmid: Some(9000),
            url: None,
            checksum_algorithm: None,
            checksum: None,
        };

        let err = missing_existing_vm_image_error(&image, "9000");

        assert!(err.contains("source = \"existing\""));
        assert!(err.contains("source = \"url\""));
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
            ..DesiredState::default()
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
    fn disable_vm_start_only_mutates_vms() {
        let mut desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([
                (
                    "media-stack".to_string(),
                    vmctl_domain::NormalizedResource {
                        name: "media-stack".to_string(),
                        kind: "vm".to_string(),
                        started: Some(true),
                        ..vmctl_domain::NormalizedResource::default()
                    },
                ),
                (
                    "gateway".to_string(),
                    vmctl_domain::NormalizedResource {
                        name: "gateway".to_string(),
                        kind: "lxc".to_string(),
                        started: Some(true),
                        ..vmctl_domain::NormalizedResource::default()
                    },
                ),
            ]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        disable_vm_start(&mut desired);

        assert_eq!(
            desired.normalized_resources["media-stack"].started,
            Some(false)
        );
        assert_eq!(desired.normalized_resources["gateway"].started, Some(true));
    }

    #[test]
    fn disable_vm_start_preserves_existing_running_vm_state() {
        let mut desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    started: Some(false),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        disable_vm_start_with_status(&mut desired, |vmid| (vmid == 210).then_some(true));

        assert_eq!(
            desired.normalized_resources["media-stack"].started,
            Some(true)
        );
    }

    #[test]
    fn parses_qm_started_status() {
        assert_eq!(parse_qm_started("status: running\n"), Some(true));
        assert_eq!(parse_qm_started("status: stopped\n"), Some(false));
        assert_eq!(parse_qm_started("status: paused\n"), None);
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
    fn apply_preflight_rejects_iothread_on_scsi_disk() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    disk_interface: Some("scsi0".to_string()),
                    iothread: Some(true),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        let err = validate_apply_preflight(&desired).unwrap_err();

        assert!(err.to_string().contains("iothread=true"));
    }

    #[test]
    fn apply_preflight_rejects_unindexed_vm_disk_interface() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    disk_interface: Some("scsi".to_string()),
                    iothread: Some(false),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        let err = validate_apply_preflight(&desired).unwrap_err();

        assert!(err.to_string().contains("disk_interface=scsi"));
        assert!(err.to_string().contains("scsi0"));
    }

    #[test]
    fn apply_preflight_allows_lxc_tailscale_when_disk_and_memory_are_valid() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "tailscale-gateway".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "tailscale-gateway".to_string(),
                    kind: "lxc".to_string(),
                    features: BTreeMap::from([(
                        "tailscale".to_string(),
                        toml::Value::Table(toml::map::Map::from_iter([(
                            "enabled".to_string(),
                            toml::Value::Boolean(true),
                        )])),
                    )]),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        validate_apply_preflight(&desired).unwrap();
    }

    #[test]
    fn parses_memtotal_from_proc_meminfo() {
        assert_eq!(
            parse_memtotal_mib("MemTotal:       16144204 kB\nMemFree:        13306740 kB\n"),
            Some(15765)
        );
    }

    #[test]
    fn apply_preflight_rejects_vm_memory_that_exhausts_host() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    memory: Some(16384),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        let err = validate_apply_preflight_with_host_memory(&desired, Some(15765)).unwrap_err();

        assert!(err.to_string().contains("leaves less than"));
        assert!(err.to_string().contains("Proxmox host"));
    }

    #[test]
    fn apply_preflight_accepts_vm_memory_with_host_reserve() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    memory: Some(8192),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        validate_apply_preflight_with_host_memory(&desired, Some(15765)).unwrap();
    }

    #[test]
    fn detects_stale_state_entries_for_missing_backend_resources() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };
        let state_addresses = BTreeSet::from([
            "module.media_stack.proxmox_virtual_environment_vm.this[0]".to_string(),
        ]);

        let stale =
            missing_backend_state_resources(&desired, &state_addresses, |_kind, _vmid| false);

        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "media-stack");
        assert_eq!(stale[0].vmid, 210);
    }

    #[test]
    fn ignores_state_entries_when_backend_resource_exists() {
        let desired = DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources: Vec::new(),
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                vmctl_domain::NormalizedResource {
                    name: "media-stack".to_string(),
                    kind: "vm".to_string(),
                    vmid: Some(210),
                    ..vmctl_domain::NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };
        let state_addresses = BTreeSet::from([
            "module.media_stack.proxmox_virtual_environment_vm.this[0]".to_string(),
        ]);

        let stale =
            missing_backend_state_resources(&desired, &state_addresses, |_kind, _vmid| true);

        assert!(stale.is_empty());
    }

    #[test]
    fn vm_runtime_repairs_detect_memory_and_iothread_controller_fix() {
        let resource = vmctl_domain::NormalizedResource {
            name: "media-stack".to_string(),
            kind: "vm".to_string(),
            memory: Some(8192),
            ..vmctl_domain::NormalizedResource::default()
        };
        let config = "\
memory: 16384
scsi0: local-lvm:vm-210-disk-0,iothread=1,size=64G
scsihw: virtio-scsi-pci
";

        assert_eq!(
            vm_runtime_repairs(&resource, config),
            vec![
                VmRuntimeRepair::Memory { desired_mib: 8192 },
                VmRuntimeRepair::ScsiHardware {
                    value: "virtio-scsi-single"
                }
            ]
        );
    }

    #[test]
    fn vm_runtime_repairs_skip_when_config_already_matches() {
        let resource = vmctl_domain::NormalizedResource {
            name: "media-stack".to_string(),
            kind: "vm".to_string(),
            memory: Some(8192),
            ..vmctl_domain::NormalizedResource::default()
        };
        let config = "\
memory: 8192
scsi0: local-lvm:vm-210-disk-0,iothread=1,size=64G
scsihw: virtio-scsi-single
";

        assert!(vm_runtime_repairs(&resource, config).is_empty());
    }

    #[test]
    fn lxc_runtime_repairs_adds_tun_passthrough_when_missing() {
        let resource = vmctl_domain::NormalizedResource {
            name: "tailscale-gateway".to_string(),
            kind: "lxc".to_string(),
            features: BTreeMap::from([(
                "tailscale".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "enabled".to_string(),
                    toml::Value::Boolean(true),
                )])),
            )]),
            ..vmctl_domain::NormalizedResource::default()
        };
        let config = "\
arch: amd64
cores: 1
net0: name=eth0,bridge=vmbr0,ip=dhcp,type=veth
";

        assert_eq!(
            lxc_runtime_repairs(&resource, config),
            vec![LxcRuntimeRepair::TunDevice {
                slot: "dev0".to_string()
            }]
        );
    }

    #[test]
    fn lxc_runtime_repairs_skip_when_tun_passthrough_present() {
        let resource = vmctl_domain::NormalizedResource {
            name: "tailscale-gateway".to_string(),
            kind: "lxc".to_string(),
            features: BTreeMap::from([(
                "tailscale".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "enabled".to_string(),
                    toml::Value::Boolean(true),
                )])),
            )]),
            ..vmctl_domain::NormalizedResource::default()
        };
        let config = "\
dev0: /dev/net/tun,mode=0666
net0: name=eth0,bridge=vmbr0,ip=dhcp,type=veth
";

        assert!(lxc_runtime_repairs(&resource, config).is_empty());
    }

    #[test]
    fn lxc_runtime_repairs_include_root_authorized_key_when_public_key_exists() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        let key_path = root.join("id_ed25519.pub");
        std::fs::write(&key_path, "ssh-ed25519 AAAATESTKEY root@mini\n").unwrap();
        let resource = vmctl_domain::NormalizedResource {
            name: "tailscale-gateway".to_string(),
            kind: "lxc".to_string(),
            cloud_init: Some(vmctl_domain::CloudInitConfig {
                user: None,
                ssh_key_file: Some(key_path.to_string_lossy().to_string()),
            }),
            ..vmctl_domain::NormalizedResource::default()
        };
        let config = "net0: name=eth0,bridge=vmbr0,ip=dhcp,type=veth\n";

        let repairs = lxc_runtime_repairs(&resource, config);

        assert!(repairs.iter().any(|repair| matches!(
            repair,
            LxcRuntimeRepair::RootAuthorizedKey { local_key_file }
            if local_key_file == &key_path.to_string_lossy()
        )));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_primary_vm_mac_and_bridge() {
        let config = "net0: virtio=BC:24:11:1D:8A:AE,bridge=vmbr0,firewall=0\n";

        assert_eq!(primary_config_mac(config), Some("BC:24:11:1D:8A:AE"));
        assert_eq!(primary_config_bridge(config), Some("vmbr0"));
    }

    #[test]
    fn parses_primary_lxc_mac_and_bridge() {
        let config = "net0: name=eth0,bridge=vmbr0,hwaddr=BC:24:11:12:7E:8B,ip=dhcp,type=veth\n";

        assert_eq!(primary_config_mac(config), Some("BC:24:11:12:7E:8B"));
        assert_eq!(primary_config_bridge(config), Some("vmbr0"));
    }

    #[test]
    fn finds_ip_for_mac_from_arp_scan_output() {
        let output = "\
Interface: vmbr0
192.168.86.1 b0:e4:d5:6b:bb:08 Google, Inc.
192.168.86.103 bc:24:11:1d:8a:ae (Unknown)
";

        assert_eq!(
            ip_for_mac_from_arp_scan(output, "BC:24:11:1D:8A:AE"),
            Some("192.168.86.103".to_string())
        );
    }

    #[test]
    fn discover_with_retry_returns_successful_attempt() {
        let mut attempts = 0u32;
        let resolved = discover_with_retry(3, Duration::from_millis(0), || {
            attempts += 1;
            (attempts == 2).then(|| "192.168.86.110".to_string())
        });

        assert_eq!(resolved, Some("192.168.86.110".to_string()));
        assert_eq!(attempts, 2);
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
        let config_path = root.join("vmctl.toml");
        std::fs::write(&config_path, "version = \"2.0.0\"\n").unwrap();
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
                enabled: true,
                image: None,
                role: None,
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        };

        let lockfile = write_lockfile(
            &workspace,
            &desired,
            Some(&config_path),
            Path::new("resources"),
            Path::new("services"),
        )
        .unwrap();

        assert!(!lockfile.resources[0].exists);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn proxmox_host_tailscale_script_is_host_only() {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("resources/proxmox-host/hooks/setup-tailscale.sh");
        let content = std::fs::read_to_string(&script).unwrap();

        assert!(content.contains("tailscale up"));
        assert!(content.contains("--accept-dns=false"));
        assert!(!content.contains("--advertise-routes"));
        assert!(!content.contains("--advertise-exit-node"));
        assert!(!content.contains("vmctl apply"));

        command_runner::run(
            CommandOptions::new("bash", ["-n", &script.to_string_lossy()])
                .stream(false)
                .timeout(Duration::from_secs(10)),
        )
        .unwrap();
    }

    #[test]
    fn proxmox_host_ui_serve_script_is_tailnet_only() {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("resources/proxmox-host/hooks/serve-ui.sh");
        let content = std::fs::read_to_string(&script).unwrap();

        assert!(content.contains("tailscale serve"));
        assert!(content.contains("https+insecure://localhost:8006"));
        assert!(content.contains("--bg"));
        assert!(content.contains("--yes"));
        assert!(content.contains("--wait-seconds"));
        assert!(content.contains("off"));
        assert!(!content.contains("tailscale funnel"));

        command_runner::run(
            CommandOptions::new("bash", ["-n", &script.to_string_lossy()])
                .stream(false)
                .timeout(Duration::from_secs(10)),
        )
        .unwrap();
    }

    #[test]
    fn workspace_source_fingerprint_changes_when_service_source_changes() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("resources/services")).unwrap();
        std::fs::write(root.join("vmctl.toml"), "title = 'example'\n").unwrap();
        std::fs::write(
            root.join("resources/services/seerr.toml"),
            "name = 'seerr'\n[settings]\ndefault_movie_quality_profile = 'Any'\n",
        )
        .unwrap();

        let first = workspace_source_fingerprint(
            &root.join("vmctl.toml"),
            &[root.join("resources"), root.join("services")],
        )
        .unwrap();

        std::fs::write(
            root.join("resources/services/seerr.toml"),
            "name = 'seerr'\n[settings]\ndefault_movie_quality_profile = 'HD - 720p/1080p'\n",
        )
        .unwrap();

        let second = workspace_source_fingerprint(
            &root.join("vmctl.toml"),
            &[root.join("resources"), root.join("services")],
        )
        .unwrap();

        assert_ne!(first, second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_rendered_workspace_overwrites_stale_generated_media_env() {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let original_cwd = std::env::current_dir().unwrap();
        let temp_root = unique_temp_dir();
        std::fs::create_dir_all(&temp_root).unwrap();
        std::env::set_current_dir(&temp_root).unwrap();

        let result = (|| -> Result<()> {
            let (workspace, desired, registry, service_registry) = load_workspace(
                Some(&repo_root.join("vmctl.toml")),
                &repo_root.join("resources"),
                &repo_root.join("services"),
                None,
            )?;
            let stale_media_env = workspace
                .root
                .join(&workspace.generated_dir)
                .join("resources/media-stack/media.env");
            std::fs::create_dir_all(stale_media_env.parent().unwrap())?;
            std::fs::write(&stale_media_env, "RADARR_DEFAULT_QUALITY_PROFILE=\"\"\n")?;

            refresh_rendered_workspace(&workspace, &desired, &registry, &service_registry)?;

            let rendered = std::fs::read_to_string(&stale_media_env)?;
            assert!(
                rendered.contains("RADARR_DEFAULT_QUALITY_PROFILE=\"HD - 720p/1080p\""),
                "rendered media.env was:\n{}",
                rendered
            );
            Ok(())
        })();

        std::env::set_current_dir(&original_cwd).unwrap();
        std::fs::remove_dir_all(&temp_root).unwrap();
        result.unwrap();
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
