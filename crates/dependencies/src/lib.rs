use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Terraform,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    ValidateConfig,
    Render,
    ValidateRendered { live: bool },
    Plan { dry_run: bool },
    Apply,
    Destroy,
    Provision,
    Import,
    Sync,
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCheck {
    pub command: &'static str,
    pub alternatives: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyPlan {
    checks: Vec<DependencyCheck>,
}

impl DependencyPlan {
    pub fn for_command(backend: BackendKind, scope: CommandScope) -> Self {
        let checks = match (backend, scope) {
            (
                BackendKind::Terraform,
                CommandScope::Plan { .. }
                | CommandScope::Apply
                | CommandScope::Destroy
                | CommandScope::ValidateRendered { .. },
            ) => vec![DependencyCheck {
                command: "OpenTofu/Terraform",
                alternatives: vec!["tofu", "terraform"],
            }],
            (BackendKind::Terraform, CommandScope::Doctor) => vec![DependencyCheck {
                command: "OpenTofu/Terraform",
                alternatives: vec!["tofu", "terraform"],
            }],
            (_, CommandScope::Provision) => vec![
                DependencyCheck {
                    command: "ssh",
                    alternatives: vec!["ssh"],
                },
                DependencyCheck {
                    command: "scp",
                    alternatives: vec!["scp"],
                },
            ],
            _ => Vec::new(),
        };
        Self { checks }
    }

    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    pub fn checks(&self) -> &[DependencyCheck] {
        &self.checks
    }

    pub fn verify(&self, path: Option<&str>) -> Result<()> {
        for check in &self.checks {
            if !check
                .alternatives
                .iter()
                .any(|command| command_exists(command, path))
            {
                bail!(
                    "missing dependency for this command: {} (install one of: {})",
                    check.command,
                    check.alternatives.join(", ")
                );
            }
        }
        Ok(())
    }
}

pub fn backend_kind(value: &str) -> BackendKind {
    if value == "terraform" {
        BackendKind::Terraform
    } else {
        BackendKind::Other
    }
}

fn command_exists(command: &str, path: Option<&str>) -> bool {
    path.map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| candidate(&dir, command))
        .any(|candidate| candidate.is_file())
}

fn candidate(dir: &Path, command: &str) -> PathBuf {
    dir.join(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terraform_required_for_terraform_plan() {
        let plan = DependencyPlan::for_command(
            BackendKind::Terraform,
            CommandScope::Plan { dry_run: true },
        );

        assert_eq!(plan.checks().len(), 1);
        assert_eq!(plan.checks()[0].alternatives, vec!["tofu", "terraform"]);
        assert!(plan.verify(Some("")).is_err());
    }

    #[test]
    fn terraform_not_required_for_config_validation() {
        let plan =
            DependencyPlan::for_command(BackendKind::Terraform, CommandScope::ValidateConfig);

        assert!(plan.is_empty());
        assert!(plan.verify(Some("")).is_ok());
    }

    #[test]
    fn terraform_not_required_for_unused_backend() {
        let plan =
            DependencyPlan::for_command(BackendKind::Other, CommandScope::Plan { dry_run: true });

        assert!(plan.is_empty());
    }
}
