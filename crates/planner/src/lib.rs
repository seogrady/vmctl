use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use vmctl_config::Config;
use vmctl_domain::{CloudInitConfig, DesiredState, NetworkConfig, NormalizedResource, Resource};
use vmctl_packs::PackRegistry;

pub fn build_desired_state(
    config: Config,
    registry: &PackRegistry,
    target: Option<&str>,
) -> Result<DesiredState> {
    let resources = config
        .resources
        .into_iter()
        .map(|resource| apply_defaults(resource, &config.defaults))
        .collect::<Vec<_>>();
    let resources = select_resources(resources, target)?;

    let expansions = resources
        .iter()
        .map(|resource| {
            registry
                .expand_resource(resource)
                .map(|expansion| (resource.name.clone(), expansion))
        })
        .collect::<Result<_>>()?;
    let normalized_resources = resources
        .iter()
        .map(|resource| (resource.name.clone(), normalize_resource(resource)))
        .collect::<BTreeMap<_, _>>();

    Ok(DesiredState {
        backend: config.backend,
        resources,
        normalized_resources,
        expansions,
    })
}

fn apply_defaults(mut resource: Resource, defaults: &BTreeMap<String, toml::Value>) -> Resource {
    for (key, value) in defaults {
        if key == "vm" || key == "lxc" {
            continue;
        }
        resource
            .settings
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }

    if let Some(kind_defaults) = defaults.get(&resource.kind).and_then(toml::Value::as_table) {
        for (key, value) in kind_defaults {
            resource
                .settings
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }

    resource
}

fn select_resources(resources: Vec<Resource>, target: Option<&str>) -> Result<Vec<Resource>> {
    validate_dependencies(&resources)?;

    let Some(target) = target else {
        return Ok(resources);
    };

    let resources_by_name = resources
        .iter()
        .map(|resource| (resource.name.clone(), resource.clone()))
        .collect::<BTreeMap<_, _>>();
    if !resources_by_name.contains_key(target) {
        bail!("target resource `{target}` was not found");
    }

    let mut selected = BTreeSet::new();
    collect_dependencies(target, &resources_by_name, &mut selected)?;

    Ok(resources
        .into_iter()
        .filter(|resource| selected.contains(&resource.name))
        .collect())
}

fn normalize_resource(resource: &Resource) -> NormalizedResource {
    NormalizedResource {
        name: resource.name.clone(),
        kind: resource.kind.clone(),
        role: resource.role.clone(),
        vmid: resource.vmid,
        depends_on: resource.depends_on.clone(),
        node: string_setting(resource, "node"),
        bridge: string_setting(resource, "bridge"),
        storage: string_setting(resource, "storage"),
        template: string_setting(resource, "template"),
        template_storage: string_setting(resource, "template_storage"),
        clone_vmid: u32_setting(resource, "clone_vmid").or_else(|| template_as_vmid(resource)),
        cores: u32_setting(resource, "cores"),
        memory: u32_setting(resource, "memory"),
        disk_gb: u32_setting(resource, "disk_gb"),
        rootfs_gb: u32_setting(resource, "rootfs_gb"),
        start_on_boot: bool_setting(resource, "start_on_boot"),
        agent: bool_setting(resource, "agent"),
        nameserver: string_setting(resource, "nameserver"),
        searchdomain: string_setting(resource, "searchdomain"),
        description: string_setting(resource, "description"),
        tags: string_array_setting(resource, "tags"),
        os_type: string_setting(resource, "os_type"),
        network: network_config(resource),
        cloud_init: cloud_init_config(resource),
        features: resource.features.clone(),
    }
}

fn string_setting(resource: &Resource, key: &str) -> Option<String> {
    resource
        .settings
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

fn u32_setting(resource: &Resource, key: &str) -> Option<u32> {
    resource
        .settings
        .get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|value| u32::try_from(value).ok())
}

fn bool_setting(resource: &Resource, key: &str) -> Option<bool> {
    resource.settings.get(key).and_then(toml::Value::as_bool)
}

fn string_array_setting(resource: &Resource, key: &str) -> Vec<String> {
    resource
        .settings
        .get(key)
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn template_as_vmid(resource: &Resource) -> Option<u32> {
    string_setting(resource, "template").and_then(|value| value.parse().ok())
}

fn network_config(resource: &Resource) -> Option<NetworkConfig> {
    let table = resource.settings.get("network")?.as_table()?;
    Some(NetworkConfig {
        mode: table
            .get("mode")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        mac: table
            .get("mac")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        address: table
            .get("address")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        gateway: table
            .get("gateway")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        vlan_id: table
            .get("vlan_id")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok()),
        mtu: table
            .get("mtu")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok()),
        firewall: table.get("firewall").and_then(toml::Value::as_bool),
    })
}

fn cloud_init_config(resource: &Resource) -> Option<CloudInitConfig> {
    let table = resource.settings.get("cloud_init")?.as_table()?;
    Some(CloudInitConfig {
        user: table
            .get("user")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        ssh_key_file: table
            .get("ssh_key_file")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
    })
}

fn validate_dependencies(resources: &[Resource]) -> Result<()> {
    let names = resources
        .iter()
        .map(|resource| resource.name.as_str())
        .collect::<BTreeSet<_>>();

    for resource in resources {
        for dependency in &resource.depends_on {
            if !names.contains(dependency.as_str()) {
                bail!(
                    "resource `{}` depends on missing resource `{dependency}`",
                    resource.name
                );
            }
        }
    }
    Ok(())
}

fn collect_dependencies(
    name: &str,
    resources: &BTreeMap<String, Resource>,
    selected: &mut BTreeSet<String>,
) -> Result<()> {
    if !selected.insert(name.to_string()) {
        return Ok(());
    }

    let resource = resources
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("missing dependency `{name}`"))?;
    for dependency in &resource.depends_on {
        collect_dependencies(dependency, resources, selected)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use vmctl_domain::BackendConfig;

    #[test]
    fn target_selection_includes_dependencies() {
        let resources = vec![
            resource("gateway", "lxc", vec![]),
            resource("media-stack", "vm", vec!["gateway"]),
        ];

        let selected = select_resources(resources, Some("media-stack")).unwrap();

        assert_eq!(
            selected
                .into_iter()
                .map(|resource| resource.name)
                .collect::<Vec<_>>(),
            vec!["gateway".to_string(), "media-stack".to_string()]
        );
    }

    #[test]
    fn applies_global_and_kind_defaults_without_overriding_resource_values() {
        let mut defaults = BTreeMap::new();
        defaults.insert(
            "bridge".to_string(),
            toml::Value::String("vmbr0".to_string()),
        );
        defaults.insert(
            "vm".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([
                ("cores".to_string(), toml::Value::Integer(2)),
                ("memory".to_string(), toml::Value::Integer(4096)),
            ])),
        );

        let mut input = resource("media-stack", "vm", vec![]);
        input
            .settings
            .insert("memory".to_string(), toml::Value::Integer(8192));
        input.settings.insert(
            "tags".to_string(),
            toml::Value::Array(vec![toml::Value::String("vmctl".to_string())]),
        );

        let resolved = apply_defaults(input, &defaults);

        assert_eq!(
            resolved
                .settings
                .get("bridge")
                .and_then(toml::Value::as_str),
            Some("vmbr0")
        );
        assert_eq!(
            resolved
                .settings
                .get("cores")
                .and_then(toml::Value::as_integer),
            Some(2)
        );
        assert_eq!(
            resolved
                .settings
                .get("memory")
                .and_then(toml::Value::as_integer),
            Some(8192)
        );
    }

    #[test]
    fn normalizes_common_resource_fields() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.vmid = Some(210);
        input
            .settings
            .insert("cores".to_string(), toml::Value::Integer(6));
        input
            .settings
            .insert("memory".to_string(), toml::Value::Integer(16384));
        input
            .settings
            .insert("clone_vmid".to_string(), toml::Value::Integer(9000));
        input.settings.insert(
            "nameserver".to_string(),
            toml::Value::String("1.1.1.1".to_string()),
        );
        input.settings.insert(
            "network".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([
                (
                    "mode".to_string(),
                    toml::Value::String("static".to_string()),
                ),
                (
                    "address".to_string(),
                    toml::Value::String("192.168.1.20/24".to_string()),
                ),
                (
                    "gateway".to_string(),
                    toml::Value::String("192.168.1.1".to_string()),
                ),
                ("vlan_id".to_string(), toml::Value::Integer(20)),
                ("mtu".to_string(), toml::Value::Integer(1500)),
                ("firewall".to_string(), toml::Value::Boolean(true)),
            ])),
        );

        let normalized = normalize_resource(&input);

        assert_eq!(normalized.vmid, Some(210));
        assert_eq!(normalized.cores, Some(6));
        assert_eq!(normalized.memory, Some(16384));
        assert_eq!(normalized.clone_vmid, Some(9000));
        assert_eq!(normalized.nameserver, Some("1.1.1.1".to_string()));
        let network = normalized.network.unwrap();
        assert_eq!(network.mode, Some("static".to_string()));
        assert_eq!(network.address, Some("192.168.1.20/24".to_string()));
        assert_eq!(network.gateway, Some("192.168.1.1".to_string()));
        assert_eq!(network.vlan_id, Some(20));
        assert_eq!(network.mtu, Some(1500));
        assert_eq!(network.firewall, Some(true));
    }

    #[test]
    fn rejects_missing_dependencies() {
        let err = select_resources(vec![resource("media-stack", "vm", vec!["gateway"])], None)
            .unwrap_err();

        assert!(err.to_string().contains("depends on missing resource"));
    }

    fn resource(name: &str, kind: &str, depends_on: Vec<&str>) -> Resource {
        Resource {
            name: name.to_string(),
            kind: kind.to_string(),
            role: None,
            vmid: None,
            depends_on: depends_on.into_iter().map(str::to_string).collect(),
            features: BTreeMap::new(),
            settings: BTreeMap::new(),
        }
    }

    #[allow(dead_code)]
    fn desired(resources: Vec<Resource>) -> DesiredState {
        DesiredState {
            backend: BackendConfig::default(),
            resources,
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
        }
    }
}
