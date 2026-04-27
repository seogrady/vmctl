use vmctl_domain::DesiredState;

pub fn render_plan(desired: &DesiredState) -> String {
    let mut output = String::from("vmctl plan\n");
    for resource in &desired.resources {
        output.push_str(&format!(
            "- {} {} ({})\n",
            resource.kind,
            resource.name,
            resource.role.as_deref().unwrap_or("no role")
        ));
        if let Some(expansion) = desired.expansions.get(&resource.name) {
            if !expansion.service_defs.is_empty() {
                output.push_str(&format!(
                    "  services: {}\n",
                    expansion.service_defs.join(", ")
                ));
            }
            if !expansion.files.is_empty() {
                output.push_str(&format!("  files: {}\n", expansion.files.join(", ")));
            }
            if !expansion.bootstrap_steps.is_empty() {
                output.push_str(&format!(
                    "  bootstrap: {}\n",
                    expansion.bootstrap_steps.join(", ")
                ));
            }
        }
    }
    if !desired.service_plan.instances.is_empty() {
        output.push_str("services\n");
        for instance in &desired.service_plan.instances {
            output.push_str(&format!(
                "- {} {} ({})\n",
                instance.scope, instance.service, instance.version
            ));
            if !instance.required_dependencies.is_empty() {
                output.push_str(&format!(
                    "  requires: {}\n",
                    instance.required_dependencies.join(", ")
                ));
            }
        }
    }
    output
}
