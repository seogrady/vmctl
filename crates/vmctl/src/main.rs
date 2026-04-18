mod config;
mod packs;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, DesiredState, Workspace};
use packs::PackRegistry;

#[derive(Debug, Parser)]
#[command(name = "vmctl", version, about = "Declarative Proxmox homelab manager")]
struct Cli {
    #[arg(short, long, default_value = "vmctl.toml")]
    config: PathBuf,

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
        target: Option<String>,
    },
    Destroy {
        target: String,
    },
    Import,
    Sync,
    Backend {
        #[command(subcommand)]
        command: BackendCommand,
    },
}

#[derive(Debug, Subcommand)]
enum BackendCommand {
    Doctor,
    Render,
    ShowState,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => init_workspace(&cli.config, &cli.packs),
        Command::Validate => {
            let (_workspace, desired) = load_workspace(&cli.config, &cli.packs, None)?;
            println!(
                "valid: {} resources, {} expanded roles",
                desired.resources.len(),
                desired.expansions.len()
            );
            Ok(())
        }
        Command::Plan { target } => {
            let (_workspace, desired) = load_workspace(&cli.config, &cli.packs, target.as_deref())?;
            print_plan(&desired);
            Ok(())
        }
        Command::Apply { .. } => {
            anyhow::bail!("apply is not implemented yet; run `vmctl backend render` to inspect generated artifacts")
        }
        Command::Destroy { .. } => anyhow::bail!("destroy is not implemented yet"),
        Command::Import => anyhow::bail!("import is not implemented yet"),
        Command::Sync => anyhow::bail!("sync is not implemented yet"),
        Command::Backend { command } => match command {
            BackendCommand::Doctor => backend_doctor(),
            BackendCommand::Render => {
                let (workspace, desired) = load_workspace(&cli.config, &cli.packs, None)?;
                render_backend(&workspace, &desired)
            }
            BackendCommand::ShowState => anyhow::bail!("backend show-state is not implemented yet"),
        },
    }
}

fn load_workspace(
    config_path: &Path,
    packs_path: &Path,
    target: Option<&str>,
) -> Result<(Workspace, DesiredState)> {
    let workspace_root = std::env::current_dir().context("failed to read current directory")?;
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let process_env = std::env::vars().collect();
    let config = Config::from_toml(&raw, &process_env)?;
    let registry = PackRegistry::load(packs_path)?;
    let desired = DesiredState::from_config(config, &registry, target)?;
    Ok((
        Workspace {
            root: workspace_root,
            generated_dir: PathBuf::from("backend/generated/workspace"),
        },
        desired,
    ))
}

fn init_workspace(config_path: &Path, packs_path: &Path) -> Result<()> {
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

fn backend_doctor() -> Result<()> {
    let terraform = command_exists("tofu") || command_exists("terraform");
    println!("backend: terraform");
    println!(
        "binary: {}",
        if terraform {
            "found tofu/terraform"
        } else {
            "missing tofu/terraform"
        }
    );
    Ok(())
}

fn command_exists(command: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|path| path.join(command).is_file())
}

fn print_plan(desired: &DesiredState) {
    println!("vmctl plan");
    for resource in &desired.resources {
        println!(
            "- {} {} ({})",
            resource.kind,
            resource.name,
            resource.role.as_deref().unwrap_or("no role")
        );
        if let Some(expansion) = desired.expansions.get(&resource.name) {
            if !expansion.service_defs.is_empty() {
                println!("  services: {}", expansion.service_defs.join(", "));
            }
            if !expansion.files.is_empty() {
                println!("  files: {}", expansion.files.join(", "));
            }
            if !expansion.bootstrap_steps.is_empty() {
                println!("  bootstrap: {}", expansion.bootstrap_steps.join(", "));
            }
        }
    }
}

fn render_backend(workspace: &Workspace, desired: &DesiredState) -> Result<()> {
    let generated = workspace.root.join(&workspace.generated_dir);
    std::fs::create_dir_all(&generated)?;

    let desired_json = serde_json::to_string_pretty(desired)?;
    std::fs::write(generated.join("desired-state.json"), desired_json)?;

    let tfvars = serde_json::json!({
        "backend": desired.backend,
        "resources": desired.resources,
        "expansions": desired.expansions,
    });
    std::fs::write(
        generated.join("terraform.tfvars.json"),
        serde_json::to_string_pretty(&tfvars)?,
    )?;

    println!("rendered {}", generated.display());
    Ok(())
}
