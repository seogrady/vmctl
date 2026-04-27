use anyhow::{bail, Result};
use vmctl_backend::{EngineBackend, RenderResult};
use vmctl_domain::{DesiredState, Workspace};
use vmctl_resources::ResourceRegistry;

#[derive(Debug, Default)]
pub struct NativeBackend;

impl EngineBackend for NativeBackend {
    fn validate_backend(&self, _workspace: &Workspace) -> Result<()> {
        bail!("native backend is a placeholder for the future direct Proxmox engine")
    }

    fn render(
        &self,
        _workspace: &Workspace,
        _desired: &DesiredState,
        _registry: &ResourceRegistry,
    ) -> Result<RenderResult> {
        bail!("native backend rendering is not implemented yet")
    }
}
