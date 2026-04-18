use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vmctl_backend::{EngineBackend, TargetSelector};
use vmctl_backend_terraform::TerraformBackend;
use vmctl_config::Config;
use vmctl_domain::{DesiredState, Workspace};
use vmctl_lockfile::Lockfile;
use vmctl_packs::PackRegistry;

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
            let (_workspace, desired, _registry) = load_workspace(&cli.config, &cli.packs, None)?;
            println!(
                "valid: {} resources, {} expanded roles",
                desired.resources.len(),
                desired.expansions.len()
            );
            Ok(())
        }
        Command::Plan { target } => {
            let (_workspace, desired, _registry) =
                load_workspace(&cli.config, &cli.packs, target.as_deref())?;
            print!("{}", vmctl_render::render_plan(&desired));
            Ok(())
        }
        Command::Apply { target } => {
            let (workspace, desired, registry) =
                load_workspace(&cli.config, &cli.packs, target.as_deref())?;
            let result = TerraformBackend.apply(&workspace, &desired, &registry)?;
            let lockfile = Lockfile::from_desired(&desired)?;
            lockfile.write_to_path(&workspace.root.join("vmctl.lock"))?;
            println!("{}; wrote vmctl.lock", result.summary);
            Ok(())
        }
        Command::Destroy { target } => {
            let workspace = default_workspace()?;
            let result = TerraformBackend.destroy(&workspace, &TargetSelector { name: target })?;
            println!("{}", result.summary);
            Ok(())
        }
        Command::Import => vmctl_import::import_workspace(),
        Command::Sync => anyhow::bail!("sync is not implemented yet"),
        Command::Backend { command } => match command {
            BackendCommand::Doctor => {
                let workspace = default_workspace()?;
                TerraformBackend.validate_backend(&workspace)
            }
            BackendCommand::Render => {
                let (workspace, desired, registry) = load_workspace(&cli.config, &cli.packs, None)?;
                let result = TerraformBackend.render(&workspace, &desired, &registry)?;
                let lockfile = Lockfile::from_desired(&desired)?;
                lockfile.write_to_path(&workspace.root.join("vmctl.lock"))?;
                println!("{}; wrote vmctl.lock", result.summary);
                Ok(())
            }
            BackendCommand::ShowState => show_backend_state(&default_workspace()?),
        },
    }
}

fn load_workspace(
    config_path: &Path,
    packs_path: &Path,
    target: Option<&str>,
) -> Result<(Workspace, DesiredState, PackRegistry)> {
    let workspace = default_workspace()?;
    let raw = std::fs::read_to_string(config_path)
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
