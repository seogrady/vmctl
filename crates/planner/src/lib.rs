use anyhow::{bail, Result};
use vmctl_config::Config;
use vmctl_domain::DesiredState;
use vmctl_packs::PackRegistry;

pub fn build_desired_state(
    config: Config,
    registry: &PackRegistry,
    target: Option<&str>,
) -> Result<DesiredState> {
    let resources: Vec<_> = config
        .resources
        .into_iter()
        .filter(|resource| target.map_or(true, |name| resource.name == name))
        .collect();

    if let Some(target) = target {
        if resources.is_empty() {
            bail!("target resource `{target}` was not found");
        }
    }

    let expansions = resources
        .iter()
        .map(|resource| {
            registry
                .expand_resource(resource)
                .map(|expansion| (resource.name.clone(), expansion))
        })
        .collect::<Result<_>>()?;

    Ok(DesiredState {
        backend: config.backend,
        resources,
        expansions,
    })
}
