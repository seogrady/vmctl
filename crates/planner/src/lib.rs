use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use vmctl_config::Config;
use vmctl_domain::{
    CloudInitConfig, DesiredState, ImageConfig, ImageKind, NetworkConfig, NormalizedResource,
    ProvisionConfig, ResolvedImage, Resource,
};
use vmctl_resources::ResourceRegistry;
use vmctl_services::ServiceRegistry;

pub fn build_desired_state(
    config: Config,
    registry: &ResourceRegistry,
    target: Option<&str>,
) -> Result<DesiredState> {
    build_desired_state_with_services(config, registry, &ServiceRegistry::default(), target)
}

pub fn build_desired_state_with_services(
    mut config: Config,
    registry: &ResourceRegistry,
    service_registry: &ServiceRegistry,
    target: Option<&str>,
) -> Result<DesiredState> {
    let images = resolve_images(&config)?;
    config
        .resources
        .extend(registry.resources().iter().cloned());
    let resources = config
        .resources
        .into_iter()
        .map(|resource| apply_defaults(resource, &config.defaults))
        .filter(|resource| resource.enabled)
        .collect::<Vec<_>>();
    let resources = select_resources(resources, target)?;

    let mut expansions = resources
        .iter()
        .map(|resource| {
            registry
                .expand_resource(resource)
                .map(|expansion| (resource.name.clone(), expansion))
        })
        .collect::<Result<_>>()?;
    let normalized_resources = resources
        .iter()
        .map(|resource| {
            normalize_resource(resource, &images)
                .map(|normalized| (resource.name.clone(), normalized))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;

    validate_image_references(&resources, &images)?;

    validate_normalized_resources(&normalized_resources)?;
    let service_plan = service_registry.build_plan(&config.services, &resources, target)?;
    merge_service_scripts_into_expansions(&mut expansions, &service_plan);

    Ok(DesiredState {
        backend: config.backend,
        runtime: config.runtime,
        services: config.services,
        service_plan,
        images,
        resources,
        normalized_resources,
        expansions,
    })
}

fn merge_service_scripts_into_expansions(
    expansions: &mut BTreeMap<String, vmctl_domain::Expansion>,
    service_plan: &vmctl_domain::ServiceExecutionPlan,
) {
    for instance in &service_plan.instances {
        let Some(target) = &instance.target else {
            continue;
        };
        let Some(expansion) = expansions.get_mut(target) else {
            continue;
        };
        expansion.bootstrap_steps.extend(
            instance
                .provision_scripts
                .iter()
                .map(|script| format!("{}/{}", instance.service, script)),
        );
        expansion.validation_steps.extend(
            instance
                .validation_scripts
                .iter()
                .map(|script| format!("{}/{}", instance.service, script)),
        );
        expansion.bootstrap_steps.dedup();
        expansion.validation_steps.dedup();
    }
}

fn apply_defaults(mut resource: Resource, defaults: &BTreeMap<String, toml::Value>) -> Resource {
    for (key, value) in defaults {
        if key == "vm" || key == "lxc" {
            continue;
        }
        insert_default_setting(&mut resource.settings, key, value);
    }

    if let Some(kind_defaults) = defaults.get(&resource.kind).and_then(toml::Value::as_table) {
        for (key, value) in kind_defaults {
            insert_default_setting(&mut resource.settings, key, value);
        }
    }

    resource
}

fn insert_default_setting(
    settings: &mut BTreeMap<String, toml::Value>,
    key: &str,
    value: &toml::Value,
) {
    match (settings.get_mut(key), value) {
        (Some(toml::Value::Table(existing)), toml::Value::Table(defaults)) => {
            for (nested_key, nested_value) in defaults {
                existing
                    .entry(nested_key.clone())
                    .or_insert_with(|| nested_value.clone());
            }
        }
        (Some(_), _) => {}
        (None, _) => {
            settings.insert(key.to_string(), value.clone());
        }
    }
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

fn resolve_images(config: &Config) -> Result<BTreeMap<String, ResolvedImage>> {
    let default_node = config
        .backend
        .settings
        .get("proxmox")
        .and_then(toml::Value::as_table)
        .and_then(|settings| settings.get("node"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();

    config
        .images
        .iter()
        .map(|(name, image)| resolve_image(name, image, default_node))
        .collect()
}

fn resolve_image(
    name: &str,
    image: &ImageConfig,
    default_node: &str,
) -> Result<(String, ResolvedImage)> {
    let file_name = image
        .file_name
        .as_deref()
        .or(image.template.as_deref())
        .unwrap_or_default()
        .trim();
    if file_name.is_empty() && image.vmid.is_none() {
        bail!("image `{name}` requires file_name or template");
    }
    let file_name = if file_name.is_empty() {
        image.vmid.map(|vmid| vmid.to_string()).unwrap_or_default()
    } else {
        file_name.to_string()
    };
    let node = image.node.as_deref().unwrap_or(default_node).trim();
    let volume_id = format!("{}:{}/{}", image.storage, image.content_type, file_name);
    Ok((
        name.to_string(),
        ResolvedImage {
            name: name.to_string(),
            kind: image.kind,
            source: image.source,
            node: node.to_string(),
            storage: image.storage.clone(),
            content_type: image.content_type.clone(),
            file_name,
            volume_id,
            vmid: image.vmid,
            url: image.url.clone(),
            checksum_algorithm: image.checksum_algorithm.clone(),
            checksum: image.checksum.clone(),
        },
    ))
}

fn validate_image_references(
    resources: &[Resource],
    images: &BTreeMap<String, ResolvedImage>,
) -> Result<()> {
    for resource in resources {
        let Some(image_name) = &resource.image else {
            continue;
        };
        let image = images.get(image_name).ok_or_else(|| {
            anyhow::anyhow!(
                "resource `{}` references missing image `{}`",
                resource.name,
                image_name
            )
        })?;
        let expected_kind = match resource.kind.as_str() {
            "vm" => ImageKind::Vm,
            "lxc" => ImageKind::Lxc,
            _ => continue,
        };
        if image.kind != expected_kind {
            bail!(
                "resource `{}` kind `{}` cannot use {:?} image `{}`",
                resource.name,
                resource.kind,
                image.kind,
                image.name
            );
        }
    }
    Ok(())
}

fn normalize_resource(
    resource: &Resource,
    images: &BTreeMap<String, ResolvedImage>,
) -> Result<NormalizedResource> {
    let image = resource.image.clone();
    let resolved_image = image.as_ref().and_then(|name| images.get(name));
    let template = resolved_image
        .map(|image| image.volume_id.clone())
        .or_else(|| string_setting(resource, "template"));
    let clone_vmid = u32_setting(resource, "clone_vmid")
        .or_else(|| resolved_image.and_then(|image| image.vmid))
        .or_else(|| {
            if resolved_image.is_some() {
                None
            } else {
                template_as_vmid(resource)
            }
        });
    let searchdomain = string_setting(resource, "searchdomain");
    if resource.settings.contains_key("hostname") {
        bail!(
            "resource `{}` setting `hostname` is not supported; use `name` as the canonical hostname",
            resource.name
        );
    }

    Ok(NormalizedResource {
        name: resource.name.clone(),
        kind: resource.kind.clone(),
        image,
        role: resource.role.clone(),
        vmid: resource.vmid,
        depends_on: resource.depends_on.clone(),
        node: string_setting(resource, "node"),
        bridge: string_setting(resource, "bridge"),
        storage: string_setting(resource, "storage"),
        template,
        template_storage: string_setting(resource, "template_storage"),
        machine: string_setting(resource, "machine").or_else(|| inferred_machine(resource)),
        scsi_hardware: string_setting(resource, "scsi_hardware"),
        disk_interface: string_setting(resource, "disk_interface")
            .as_deref()
            .map(normalize_vm_disk_interface),
        iothread: bool_setting(resource, "iothread"),
        clone_vmid,
        cores: u32_setting(resource, "cores"),
        memory: u32_setting(resource, "memory"),
        disk_gb: u32_setting(resource, "disk_gb"),
        rootfs_gb: u32_setting(resource, "rootfs_gb"),
        start_on_boot: bool_setting(resource, "start_on_boot"),
        started: bool_setting(resource, "started"),
        agent: bool_setting(resource, "agent"),
        nameserver: string_setting(resource, "nameserver"),
        searchdomain: searchdomain.clone(),
        hostname: None,
        description: string_setting(resource, "description"),
        tags: string_array_setting(resource, "tags"),
        os_type: string_setting(resource, "os_type"),
        network: network_config(resource),
        cloud_init: cloud_init_config(resource),
        provision: provision_config(resource, None, searchdomain.as_deref()),
        features: resource.features.clone(),
    })
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

fn inferred_machine(resource: &Resource) -> Option<String> {
    let intel_igpu_enabled = resource
        .features
        .get("intel_igpu")
        .and_then(toml::Value::as_table)
        .and_then(|feature| feature.get("enabled"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    intel_igpu_enabled.then(|| "q35".to_string())
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

fn normalize_vm_disk_interface(value: &str) -> String {
    let trimmed = value.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return "virtio0".to_string();
    }
    if trimmed.chars().last().is_some_and(|ch| ch.is_ascii_digit()) {
        return trimmed;
    }
    match trimmed.as_str() {
        "virtio" => "virtio0".to_string(),
        "scsi" => "scsi0".to_string(),
        "sata" => "sata0".to_string(),
        "ide" => "ide0".to_string(),
        _ => trimmed,
    }
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

fn provision_config(
    resource: &Resource,
    hostname: Option<&str>,
    _searchdomain: Option<&str>,
) -> Option<ProvisionConfig> {
    let table = resource.settings.get("provision")?.as_table()?;
    let explicit_host = table
        .get("host")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let host = explicit_host
        .or_else(|| hostname.map(str::to_string))
        .or_else(|| Some(resource.name.clone()));
    Some(ProvisionConfig {
        host,
        user: table
            .get("user")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        private_key_file: table
            .get("private_key_file")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        retries: table
            .get("retries")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok()),
        retry_delay_seconds: table
            .get("retry_delay_seconds")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u64::try_from(value).ok()),
    })
}

fn validate_normalized_resources(resources: &BTreeMap<String, NormalizedResource>) -> Result<()> {
    for resource in resources.values() {
        if let Some(cloud_init) = &resource.cloud_init {
            if cloud_init
                .ssh_key_file
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                bail!(
                    "resource `{}` cloud_init requires ssh_key_file; set defaults.cloud_init.ssh_key_file or resources.cloud_init.ssh_key_file",
                    resource.name
                );
            }
        }
    }
    Ok(())
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
        defaults.insert(
            "cloud_init".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "ssh_key_file".to_string(),
                toml::Value::String("/home/me/.ssh/id_ed25519.pub".to_string()),
            )])),
        );

        let mut input = resource("media-stack", "vm", vec![]);
        input
            .settings
            .insert("memory".to_string(), toml::Value::Integer(8192));
        input.settings.insert(
            "cloud_init".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "user".to_string(),
                toml::Value::String("ubuntu".to_string()),
            )])),
        );
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
        assert_eq!(
            resolved
                .settings
                .get("cloud_init")
                .and_then(toml::Value::as_table)
                .and_then(|cloud_init| cloud_init.get("user"))
                .and_then(toml::Value::as_str),
            Some("ubuntu")
        );
        assert_eq!(
            resolved
                .settings
                .get("cloud_init")
                .and_then(toml::Value::as_table)
                .and_then(|cloud_init| cloud_init.get("ssh_key_file"))
                .and_then(toml::Value::as_str),
            Some("/home/me/.ssh/id_ed25519.pub")
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

        let normalized = normalize_resource(&input, &BTreeMap::new()).unwrap();

        assert_eq!(normalized.vmid, Some(210));
        assert_eq!(normalized.cores, Some(6));
        assert_eq!(normalized.memory, Some(16384));
        assert_eq!(normalized.clone_vmid, Some(9000));
        assert_eq!(normalized.machine, None);
        assert_eq!(normalized.disk_interface, None);
        assert_eq!(normalized.iothread, None);
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
    fn rejects_cloud_init_without_ssh_key_file() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.settings.insert(
            "cloud_init".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "user".to_string(),
                toml::Value::String("ubuntu".to_string()),
            )])),
        );

        let err = validate_normalized_resources(&BTreeMap::from([(
            input.name.clone(),
            normalize_resource(&input, &BTreeMap::new()).unwrap(),
        )]))
        .unwrap_err();

        assert!(err.to_string().contains("cloud_init requires ssh_key_file"));
    }

    #[test]
    fn accepts_media_services_without_searchdomain() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.features.insert(
            "media_services".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "enabled".to_string(),
                toml::Value::Boolean(true),
            )])),
        );

        let result = validate_normalized_resources(&BTreeMap::from([(
            input.name.clone(),
            normalize_resource(&input, &BTreeMap::new()).unwrap(),
        )]));
        assert!(result.is_ok());
    }

    #[test]
    fn normalizes_vm_hardware_fields() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.settings.insert(
            "machine".to_string(),
            toml::Value::String("q35".to_string()),
        );
        input.settings.insert(
            "disk_interface".to_string(),
            toml::Value::String("virtio0".to_string()),
        );
        input
            .settings
            .insert("iothread".to_string(), toml::Value::Boolean(true));

        let normalized = normalize_resource(&input, &BTreeMap::new()).unwrap();

        assert_eq!(normalized.machine, Some("q35".to_string()));
        assert_eq!(normalized.disk_interface, Some("virtio0".to_string()));
        assert_eq!(normalized.iothread, Some(true));
    }

    #[test]
    fn normalizes_vm_disk_interface_without_slot_suffix() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.settings.insert(
            "disk_interface".to_string(),
            toml::Value::String("scsi".to_string()),
        );

        let normalized = normalize_resource(&input, &BTreeMap::new()).unwrap();

        assert_eq!(normalized.disk_interface, Some("scsi0".to_string()));
    }

    #[test]
    fn infers_q35_for_intel_igpu() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.features.insert(
            "intel_igpu".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "enabled".to_string(),
                toml::Value::Boolean(true),
            )])),
        );

        let normalized = normalize_resource(&input, &BTreeMap::new()).unwrap();

        assert_eq!(normalized.machine, Some("q35".to_string()));
    }

    #[test]
    fn cloud_init_ssh_key_file_is_preserved_as_path() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        let key_path = root.join("id_ed25519.pub");
        std::fs::write(&key_path, "ssh-ed25519 from-file\n").unwrap();
        let mut input = resource("from-file", "vm", vec![]);
        input.settings.insert(
            "cloud_init".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "ssh_key_file".to_string(),
                toml::Value::String(key_path.to_string_lossy().to_string()),
            )])),
        );

        assert_eq!(
            normalize_resource(&input, &BTreeMap::new())
                .unwrap()
                .cloud_init
                .and_then(|cloud_init| cloud_init.ssh_key_file),
            Some(key_path.to_string_lossy().to_string())
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provision_private_key_file_is_preserved_as_path() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        let key_path = root.join("id_ed25519");
        std::fs::write(&key_path, "PRIVATE KEY FROM FILE\n").unwrap();
        let mut input = resource("from-file", "vm", vec![]);
        input.settings.insert(
            "provision".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "private_key_file".to_string(),
                toml::Value::String(key_path.to_string_lossy().to_string()),
            )])),
        );

        let provision = normalize_resource(&input, &BTreeMap::new())
            .unwrap()
            .provision
            .unwrap();

        assert_eq!(
            provision.private_key_file,
            Some(key_path.to_string_lossy().to_string())
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provision_defaults_to_resource_name_without_hostname_setting() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.settings.insert(
            "provision".to_string(),
            toml::Value::Table(toml::map::Map::new()),
        );

        let normalized = normalize_resource(&input, &BTreeMap::new()).unwrap();

        assert_eq!(normalized.hostname, None);
        assert_eq!(
            normalized.provision.and_then(|provision| provision.host),
            Some("media-stack".to_string())
        );
    }

    #[test]
    fn rejects_hostname_setting() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.settings.insert(
            "hostname".to_string(),
            toml::Value::String("media".to_string()),
        );
        let err = normalize_resource(&input, &BTreeMap::new()).unwrap_err();
        assert!(err
            .to_string()
            .contains("setting `hostname` is not supported"));
    }

    #[test]
    fn explicit_provision_host_still_works() {
        let mut input = resource("media-stack", "vm", vec![]);
        input.settings.insert(
            "provision".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "host".to_string(),
                toml::Value::String("custom-host".to_string()),
            )])),
        );

        let normalized = normalize_resource(&input, &BTreeMap::new()).unwrap();
        assert_eq!(
            normalized.provision.and_then(|provision| provision.host),
            Some("custom-host".to_string())
        );
    }

    #[test]
    fn rejects_missing_dependencies() {
        let err = select_resources(vec![resource("media-stack", "vm", vec!["gateway"])], None)
            .unwrap_err();

        assert!(err.to_string().contains("depends on missing resource"));
    }

    #[test]
    fn resolves_lxc_image_reference_to_volume_id() {
        let image = ImageConfig {
            kind: ImageKind::Lxc,
            source: vmctl_domain::ImageSource::Pveam,
            node: Some("mini".to_string()),
            storage: "local".to_string(),
            content_type: "vztmpl".to_string(),
            file_name: None,
            vmid: None,
            template: Some("debian-12-standard_12.7-1_amd64.tar.zst".to_string()),
            url: None,
            checksum_algorithm: None,
            checksum: None,
        };
        let images = BTreeMap::from([resolve_image("debian_12_lxc", &image, "mini").unwrap()]);
        let mut input = resource("gateway", "lxc", vec![]);
        input.image = Some("debian_12_lxc".to_string());

        let normalized = normalize_resource(&input, &images).unwrap();

        assert_eq!(
            normalized.template,
            Some("local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst".to_string())
        );
    }

    fn resource(name: &str, kind: &str, depends_on: Vec<&str>) -> Resource {
        Resource {
            name: name.to_string(),
            kind: kind.to_string(),
            enabled: true,
            image: None,
            role: None,
            vmid: None,
            depends_on: depends_on.into_iter().map(str::to_string).collect(),
            features: BTreeMap::new(),
            settings: BTreeMap::new(),
        }
    }

    #[test]
    fn disabled_resources_are_pruned_before_planning() {
        let mut disabled = resource("disabled", "vm", vec![]);
        disabled.enabled = false;
        let registry = ResourceRegistry::default();
        let config = Config {
            backend: Default::default(),
            runtime: Default::default(),
            services: BTreeMap::new(),
            defaults: BTreeMap::new(),
            consts: BTreeMap::new(),
            env: BTreeMap::new(),
            images: BTreeMap::new(),
            resources: vec![resource("enabled", "vm", vec![]), disabled],
        };

        let state = build_desired_state(config, &registry, None).unwrap();
        assert_eq!(state.resources.len(), 1);
        assert_eq!(state.resources[0].name, "enabled");
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-planner-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    #[allow(dead_code)]
    fn desired(resources: Vec<Resource>) -> DesiredState {
        DesiredState {
            backend: BackendConfig::default(),
            images: BTreeMap::new(),
            resources,
            normalized_resources: BTreeMap::new(),
            expansions: BTreeMap::new(),
            ..DesiredState::default()
        }
    }
}
