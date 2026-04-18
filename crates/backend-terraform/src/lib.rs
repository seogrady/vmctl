use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::json;
use vmctl_backend::{ApplyResult, BackendPlan, EngineBackend, RenderResult, TargetSelector};
use vmctl_domain::{DesiredState, Workspace};
use vmctl_packs::PackRegistry;

#[derive(Debug, Default)]
pub struct TerraformBackend;

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
        let generated = workspace.root.join(&workspace.generated_dir);
        std::fs::create_dir_all(&generated)?;

        let mut files = Vec::new();
        write_json(&generated.join("desired-state.json"), desired, &mut files)?;
        write_json(
            &generated.join("terraform.tfvars.json"),
            &json!({
                "backend": desired.backend,
                "resources": desired.resources,
                "expansions": desired.expansions,
            }),
            &mut files,
        )?;
        write_json(
            &generated.join("variables.tf.json"),
            &variables_json(),
            &mut files,
        )?;
        write_json(&generated.join("main.tf.json"), &main_json(), &mut files)?;
        write_json(
            &generated.join("outputs.tf.json"),
            &outputs_json(),
            &mut files,
        )?;

        files.extend(registry.render_artifacts(
            &generated,
            &desired.resources,
            &desired.expansions,
        )?);

        Ok(RenderResult {
            summary: format!("rendered {} files to {}", files.len(), generated.display()),
            files,
        })
    }

    fn plan(&self, workspace: &Workspace, _desired: &DesiredState) -> Result<BackendPlan> {
        run_terraform(workspace, &["init", "-input=false"])?;
        let output = run_terraform(workspace, &["plan", "-input=false", "-no-color"])?;
        Ok(BackendPlan {
            summary: output_summary("terraform plan", &output),
        })
    }

    fn apply(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &PackRegistry,
    ) -> Result<ApplyResult> {
        self.render(workspace, desired, registry)?;
        run_terraform(workspace, &["init", "-input=false"])?;
        let output = run_terraform(
            workspace,
            &["apply", "-auto-approve", "-input=false", "-no-color"],
        )?;
        Ok(ApplyResult {
            summary: output_summary("terraform apply", &output),
        })
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
            }
        }
    })
}

fn main_json() -> serde_json::Value {
    json!({
        "terraform": {
            "required_version": ">= 1.6.0"
        },
        "locals": {
            "vmctl_resource_names": "${[for resource in var.resources : resource.name]}",
            "vmctl_resource_count": "${length(var.resources)}"
        }
    })
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
    let binary = terraform_binary()?;
    let generated = workspace.root.join(&workspace.generated_dir);
    let output = std::process::Command::new(binary)
        .args(args)
        .current_dir(&generated)
        .output()
        .with_context(|| format!("failed to run `{binary} {}`", args.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.trim().is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };

    if !output.status.success() {
        bail!("`{binary} {}` failed:\n{combined}", args.join(" "));
    }

    Ok(combined)
}

fn output_summary(prefix: &str, output: &str) -> String {
    let tail = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("completed");
    format!("{prefix}: {tail}")
}
