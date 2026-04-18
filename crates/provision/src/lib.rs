use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use vmctl_domain::{DesiredState, Expansion, NormalizedResource, Resource, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionPlan {
    pub steps: Vec<ProvisionStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionStep {
    pub resource: String,
    pub host: String,
    pub user: String,
    pub private_key_file: String,
    pub local_script: PathBuf,
    pub remote_script: String,
    pub retries: u32,
    pub retry_delay: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionResult {
    pub summary: String,
}

pub trait SshExecutor {
    fn upload(&self, step: &ProvisionStep) -> Result<()>;
    fn execute(&self, step: &ProvisionStep) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct SystemSshExecutor;

impl SshExecutor for SystemSshExecutor {
    fn upload(&self, step: &ProvisionStep) -> Result<()> {
        let destination = format!("{}@{}:{}", step.user, step.host, step.remote_script);
        let status = std::process::Command::new("scp")
            .args([
                "-i",
                &step.private_key_file,
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
            ])
            .arg(&step.local_script)
            .arg(&destination)
            .status()
            .with_context(|| format!("failed to run scp for {}", step.resource))?;
        if !status.success() {
            bail!("failed to upload provisioning script for {}", step.resource);
        }
        Ok(())
    }

    fn execute(&self, step: &ProvisionStep) -> Result<()> {
        let target = format!("{}@{}", step.user, step.host);
        let command = format!("chmod +x {0} && sudo {0}", step.remote_script);
        let status = std::process::Command::new("ssh")
            .args([
                "-i",
                &step.private_key_file,
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                &command,
            ])
            .status()
            .with_context(|| format!("failed to run ssh for {}", step.resource))?;
        if !status.success() {
            bail!(
                "failed to execute provisioning script for {}",
                step.resource
            );
        }
        Ok(())
    }
}

pub fn build_provision_plan(
    workspace: &Workspace,
    desired: &DesiredState,
) -> Result<ProvisionPlan> {
    let mut steps = Vec::new();
    for resource in &desired.resources {
        let normalized = desired
            .normalized_resources
            .get(&resource.name)
            .with_context(|| format!("missing normalized resource `{}`", resource.name))?;
        let Some(expansion) = desired.expansions.get(&resource.name) else {
            continue;
        };
        steps.extend(resource_steps(workspace, resource, normalized, expansion)?);
    }
    Ok(ProvisionPlan { steps })
}

pub fn run_provision_plan(
    plan: &ProvisionPlan,
    executor: &dyn SshExecutor,
) -> Result<ProvisionResult> {
    for step in &plan.steps {
        run_with_retries(step, || {
            executor.upload(step)?;
            executor.execute(step)
        })?;
    }

    Ok(ProvisionResult {
        summary: format!("provisioned {} scripts", plan.steps.len()),
    })
}

fn resource_steps(
    workspace: &Workspace,
    resource: &Resource,
    normalized: &NormalizedResource,
    expansion: &Expansion,
) -> Result<Vec<ProvisionStep>> {
    if expansion.bootstrap_steps.is_empty() {
        return Ok(Vec::new());
    }

    let provision = normalized.provision.as_ref().with_context(|| {
        format!(
            "resource `{}` has bootstrap scripts but no provision config",
            resource.name
        )
    })?;
    let host = provision
        .host
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("resource `{}` provision.host is required", resource.name))?;
    let user = provision
        .user
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("resource `{}` provision.user is required", resource.name))?;
    let private_key_file = provision
        .private_key_file
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| {
            format!(
                "resource `{}` provision.private_key_file is required",
                resource.name
            )
        })?;
    let retries = provision.retries.unwrap_or(20);
    let retry_delay = Duration::from_secs(provision.retry_delay_seconds.unwrap_or(15));

    let mut steps = Vec::new();
    for script in &expansion.bootstrap_steps {
        let local_script = workspace
            .root
            .join(&workspace.generated_dir)
            .join("resources")
            .join(&resource.name)
            .join("scripts")
            .join(script);
        steps.push(ProvisionStep {
            resource: resource.name.clone(),
            host: host.to_string(),
            user: user.to_string(),
            private_key_file: private_key_file.to_string(),
            remote_script: format!("/tmp/vmctl-{}-{script}", resource.name),
            local_script,
            retries,
            retry_delay,
        });
    }
    Ok(steps)
}

fn run_with_retries(step: &ProvisionStep, mut action: impl FnMut() -> Result<()>) -> Result<()> {
    let attempts = step.retries.max(1);
    let mut last_error = None;
    for attempt in 1..=attempts {
        match action() {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!(
                    "provision {} failed attempt {attempt}/{attempts}: {error}",
                    step.resource
                );
                last_error = Some(error);
                if attempt < attempts {
                    std::thread::sleep(step.retry_delay);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("provision failed")))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use vmctl_domain::{BackendConfig, Expansion, ProvisionConfig};

    #[test]
    fn builds_steps_from_pack_bootstrap_scripts() {
        let workspace = Workspace {
            root: PathBuf::from("/repo"),
            generated_dir: PathBuf::from("backend/generated/workspace"),
        };
        let desired = DesiredState {
            backend: BackendConfig::default(),
            resources: vec![Resource {
                name: "media-stack".to_string(),
                kind: "vm".to_string(),
                role: Some("media_stack".to_string()),
                vmid: Some(210),
                depends_on: Vec::new(),
                features: BTreeMap::new(),
                settings: BTreeMap::new(),
            }],
            normalized_resources: BTreeMap::from([(
                "media-stack".to_string(),
                NormalizedResource {
                    name: "media-stack".to_string(),
                    provision: Some(ProvisionConfig {
                        host: Some("media-stack.home.arpa".to_string()),
                        user: Some("ubuntu".to_string()),
                        private_key_file: Some("/home/me/.ssh/id_ed25519".to_string()),
                        retries: Some(3),
                        retry_delay_seconds: Some(1),
                    }),
                    ..NormalizedResource::default()
                },
            )]),
            expansions: BTreeMap::from([(
                "media-stack".to_string(),
                Expansion {
                    bootstrap_steps: vec!["bootstrap-media.sh".to_string()],
                    ..Expansion::default()
                },
            )]),
        };

        let plan = build_provision_plan(&workspace, &desired).unwrap();

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].host, "media-stack.home.arpa");
        assert_eq!(plan.steps[0].user, "ubuntu");
        assert_eq!(
            plan.steps[0].local_script,
            PathBuf::from("/repo/backend/generated/workspace/resources/media-stack/scripts/bootstrap-media.sh")
        );
    }

    #[test]
    fn executor_uploads_then_executes() {
        struct Recorder {
            calls: RefCell<Vec<String>>,
        }

        impl SshExecutor for Recorder {
            fn upload(&self, step: &ProvisionStep) -> Result<()> {
                self.calls
                    .borrow_mut()
                    .push(format!("upload:{}", step.resource));
                Ok(())
            }

            fn execute(&self, step: &ProvisionStep) -> Result<()> {
                self.calls
                    .borrow_mut()
                    .push(format!("execute:{}", step.resource));
                Ok(())
            }
        }

        let executor = Recorder {
            calls: RefCell::new(Vec::new()),
        };
        let plan = ProvisionPlan {
            steps: vec![ProvisionStep {
                resource: "media-stack".to_string(),
                host: "media-stack.home.arpa".to_string(),
                user: "ubuntu".to_string(),
                private_key_file: "/home/me/.ssh/id_ed25519".to_string(),
                local_script: PathBuf::from("bootstrap-media.sh"),
                remote_script: "/tmp/bootstrap-media.sh".to_string(),
                retries: 1,
                retry_delay: Duration::from_secs(0),
            }],
        };

        let result = run_provision_plan(&plan, &executor).unwrap();

        assert_eq!(result.summary, "provisioned 1 scripts");
        assert_eq!(
            executor.calls.into_inner(),
            vec![
                "upload:media-stack".to_string(),
                "execute:media-stack".to_string()
            ]
        );
    }
}
