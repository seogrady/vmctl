use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use vmctl_domain::{DesiredState, Workspace};
use vmctl_resources::ResourceRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderResult {
    pub summary: String,
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendPlan {
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendValidation {
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanMode {
    Online,
    DryRun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActualState {
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedState {
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSelector {
    pub name: String,
}

pub trait EngineBackend {
    fn validate_backend(&self, workspace: &Workspace) -> Result<()>;

    fn refresh_actual_state(&self, _workspace: &Workspace) -> Result<ActualState> {
        bail!("refresh actual state is not implemented for this backend")
    }

    fn render(
        &self,
        workspace: &Workspace,
        desired: &DesiredState,
        registry: &ResourceRegistry,
    ) -> Result<RenderResult>;

    fn plan(
        &self,
        _workspace: &Workspace,
        _desired: &DesiredState,
        _mode: PlanMode,
    ) -> Result<BackendPlan> {
        bail!("backend plan execution is not implemented")
    }

    fn validate_rendered(&self, _workspace: &Workspace) -> Result<BackendValidation> {
        bail!("backend rendered validation is not implemented")
    }

    fn apply(
        &self,
        _workspace: &Workspace,
        _desired: &DesiredState,
        _registry: &ResourceRegistry,
    ) -> Result<ApplyResult> {
        bail!("backend apply execution is not implemented")
    }

    fn destroy(&self, _workspace: &Workspace, _target: &TargetSelector) -> Result<ApplyResult> {
        bail!("backend destroy execution is not implemented")
    }

    fn import(&self, _workspace: &Workspace) -> Result<ImportedState> {
        bail!("backend import is not implemented")
    }
}
