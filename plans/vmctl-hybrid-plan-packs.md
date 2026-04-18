# vmctl – Hybrid Architecture Plan
## Rust Frontend / Orchestration Layer + Terraform/OpenTofu Backend
### With an Explicit Upgrade Path to a Native vmctl Engine

## 1. Executive Summary

`vmctl` will be a Rust-based CLI and orchestration layer that manages Proxmox infrastructure declaratively from a TOML configuration file. In the initial implementation, `vmctl` will use Terraform/OpenTofu as the execution backend for provisioning and lifecycle operations. However, the architecture will be deliberately designed so that Terraform is an implementation detail behind a backend interface, not the core domain model.

This allows us to:

- move quickly using mature Terraform/OpenTofu primitives
- avoid rebuilding state/plan/apply infrastructure from scratch on day one
- keep a rich, opinionated, homelab-friendly Rust UX
- centralize shared configuration across VMs/LXCs in our own TOML model
- preserve a future migration path to a native Rust execution engine

The key principle is:

> `vmctl` owns the domain model, config, planning UX, and orchestration semantics. Terraform owns only one possible execution backend.

That means we are **not** designing a “Terraform wrapper.” We are designing a **Rust-first control plane** with a Terraform backend adapter.

---

## 2. Goals

## 2.1 Primary Goals

- Use a single TOML config as the source of truth
- Provide a minimal and ergonomic CLI
- Support shared configuration between resources
- Support higher-level roles such as:
  - Tailscale gateway LXC
  - media stack VM/LXC
- Generate plans and apply infrastructure changes
- Support import and sync workflows
- Avoid scattering Terraform concerns throughout the user-facing model
- Preserve a future path to replace Terraform with a native Rust engine

## 2.2 Non-Goals (Phase 1)

- Replace Terraform immediately
- Fully manage all guest internals without scripts/bootstrap
- Build a general-purpose IaC competitor
- Implement full transactional rollback

---

## 3. Why a Hybrid Architecture

Terraform/OpenTofu already provides:

- state management
- plan/apply workflow
- change detection
- import workflows
- mature resource lifecycle semantics

Rust `vmctl` can provide:

- better UX
- TOML-first ergonomics
- opinionated domain abstractions
- shared cross-resource configuration
- custom plan rendering
- import/merge workflows tailored to Proxmox homelabs
- a stable internal model that can survive backend replacement later

This hybrid design lets us move faster **without** locking ourselves permanently into Terraform.

---

## 4. Core Architectural Principle

## 4.1 Separation of Concerns

### `vmctl` owns:
- domain model
- config schema
- validation
- target selection
- role expansion
- shared configuration resolution
- orchestration semantics
- generated artifacts
- plan rendering UX
- lockfile design
- import/sync UX
- backend abstraction

### Terraform/OpenTofu owns:
- provisioning execution
- state persistence
- lifecycle transitions
- apply graph execution
- import mechanics
- provider-specific resource CRUD

### Future native engine would own:
- direct Proxmox API execution
- native state engine
- native lifecycle apply logic
- native import and drift management

The domain model must not depend on Terraform types or file layout.

---

## 5. High-Level System Architecture

```text
vmctl.toml
   ↓
Rust config loader + validator
   ↓
shared defaults + role expansion + normalization
   ↓
internal desired-state model
   ↓
planner / renderer / sync logic
   ↓
backend adapter
   ├── Terraform/OpenTofu backend (Phase 1)
   └── Native Proxmox engine backend (Future)
```

Expanded view:

```text
                +----------------------+
                | vmctl CLI            |
                | (clap)               |
                +----------+-----------+
                           |
                           v
                +----------------------+
                | config + validation  |
                +----------+-----------+
                           |
                           v
                +----------------------+
                | domain model         |
                | role expansion       |
                | shared config        |
                +----------+-----------+
                           |
                           v
                +----------------------+
                | planning layer       |
                | diff UX              |
                | target selection     |
                +----------+-----------+
                           |
                           v
                +----------------------+
                | backend trait        |
                +----------+-----------+
                           |
             +-------------+-------------+
             |                           |
             v                           v
+--------------------------+   +--------------------------+
| Terraform/OpenTofu       |   | Native vmctl engine      |
| backend adapter          |   | (future)                 |
+--------------------------+   +--------------------------+
             |
             v
+--------------------------+
| Proxmox provider         |
+--------------------------+
```

---

## 6. Rust Workspace Layout

```text
vmctl/
├── Cargo.toml
├── rust-toolchain.toml
├── vmctl.toml
├── vmctl.lock
├── backend/
│   ├── modules/
│   │   ├── vm/
│   │   ├── lxc/
│   │   ├── tailscale_gateway/
│   │   └── media_stack/
│   ├── generated/
│   │   └── .gitkeep
│   └── templates/
├── plans/
│   ├── vmctl-plan.md
│   └── vmctl-hybrid-plan.md
├── crates/
│   ├── cli/
│   ├── config/
│   ├── domain/
│   ├── planner/
│   ├── packs/
│   ├── backend/
│   ├── backend-terraform/
│   ├── backend-native/      # placeholder crate in phase 1
│   ├── lockfile/
│   ├── import/
│   ├── render/
│   └── util/
└── tests/
```

## 6.1 Crate Responsibilities

### `cli`
- clap parsing
- command dispatch
- stdout/stderr rendering
- process exit codes

### `config`
- TOML parsing
- config schema
- interpolation resolution
- default resolution
- static validation

### `domain`
- backend-agnostic desired-state model
- resource kinds
- features
- roles
- dependency graph model

### `planner`
- target selection
- resource expansion orchestration
- orchestration plan
- diff rendering
- change classification

### `packs`
- load role packs
- load service packs
- validate pack schemas
- expand packs into normalized feature/service/file/bootstrap models
- merge pack outputs into desired state

### `backend`
- defines backend traits and common result types
- no Terraform-specific dependencies

### `backend-terraform`
- compiles desired state into Terraform/OpenTofu artifacts
- runs `tofu/terraform init/plan/apply/import`
- reads outputs/state metadata as needed

### `backend-native`
- future crate for direct Proxmox API implementation
- initially only contains interfaces/tests/placeholders

### `lockfile`
- lockfile model and IO
- persisted metadata and mappings

### `import`
- reads current Proxmox/Terraform reality
- generates config fragments
- compare/merge helpers

### `render`
- plan formatting
- markdown/table/tree output
- human-first CLI UX

### `util`
- path helpers
- error helpers
- command wrappers
- temporary directory handling

---

## 7. Decision: Terraform as Backend, Not as Product Model

## 7.1 What This Means

We will **not** expose Terraform concepts directly in the user config unless unavoidable.

The user should define:

- what resources exist
- their shared defaults
- their roles
- their dependencies
- their network/storage/feature settings

The user should **not** need to define:
- Terraform resource addresses
- Terraform variable graph
- Terraform module internals
- provider boilerplate

`vmctl` will compile its own model into backend artifacts.

## 7.2 Benefits

- backend independence
- better user-facing config
- easier migration to native engine later
- controlled abstraction boundary
- ability to support multiple backends later

---

## 7.1 Backend Configuration

Backend-specific connection settings should live under `[backend.*]`, for example `[backend.proxmox]`. This keeps backend integration concerns separate from the core domain model while still allowing interpolation from `[const]` and `[env]`.

---

## 8. Domain Model

## 8.1 Top-Level Concepts

- **Workspace config**
- **Global defaults**
- **Groups**
- **Resources**
- **Roles**
- **Features**
- **Dependencies**
- **Shared settings**
- **Lockfile**
- **Backend-generated artifacts**

## 8.2 Resource Kinds

- `vm`
- `lxc`

## 8.3 Roles

Roles are higher-level semantic bundles. A role expands into resource features, validations, and generated backend artifacts.

Initial roles:

- `tailscale_gateway`
- `media_stack`
- `generic_vm`
- `generic_lxc`

## 8.4 Features

Features are composable toggles or structured capabilities:
- `tailscale`
- `docker`
- `intel_igpu`
- `cloud_init`
- `media_services`

## 8.5 Dependencies

Dependencies are explicit and backend-agnostic.

Examples:
- media stack depends on tailscale gateway existing
- media stack may depend on shared storage
- bootstrap sequencing may depend on network/gateway

Terraform may use these dependencies to generate `depends_on`, but they originate in the `vmctl` model.

---

## 9. Shared Configuration Strategy

This is the most important part of making hybrid work.

## 9.1 Principle

Shared configuration is resolved in Rust before backend generation.

Do **not** rely on Terraform as the primary place to express shared business logic.

## 9.2 Example Shared Settings

- Proxmox node, bridge, storage, nameserver
- shared feature settings like Tailscale auth, tags, and routes
- default DHCP/static modes
- common cloud-init user + SSH keys
- media storage mounts
- common Docker installation settings

## 9.3 Tailscale Example

```toml

[[resources]]
name = "tailscale-gateway"
kind = "lxc"
role = "tailscale_gateway"
vmid = 101

[[resources]]
name = "media-stack"
kind = "vm"
role = "media_stack"
vmid = 210
depends_on = ["tailscale-gateway"]

[resources.features.tailscale]
enabled = true
mode = "client"
auth_key = "${TAILSCALE_AUTH_KEY}"
tags = ["${const.default_tailnet_tag}"]
```

Resolved by Rust into:
- gateway LXC bootstrap inputs
- media-stack Tailscale client bootstrap inputs
- dependency ordering metadata
- Terraform variables/module config

This keeps shared config consistent even if Terraform is replaced later.

---

## 10. Example User Config

```toml
[backend]
kind = "terraform"

[backend.proxmox]
endpoint = "https://mini:8006/api2/json"
node = "mini"
auth = "token"
token_id = "${PROXMOX_TOKEN_ID}"
token_secret = "${PROXMOX_TOKEN_SECRET}"
tls_insecure = true

[defaults]
bridge = "vmbr0"
storage = "local-lvm"
nameserver = "1.1.1.1"
searchdomain = "home.arpa"
start_on_boot = true
agent = true

[defaults.vm]
cores = 2
memory = 4096
cpu_type = "host"
machine = "q35"
disk_gb = 32
network = "dhcp"

[defaults.lxc]
cores = 1
memory = 1024
rootfs_gb = 8
unprivileged = true
network = "dhcp"

[const]
tailscale_gateway = "tailscale-gateway"
default_tailnet_tag = "tag:homelab"

[env]
TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"

[[resources]]
name = "tailscale-gateway"
kind = "lxc"
role = "tailscale_gateway"
vmid = 101
template = "debian-12-standard_12.7-1_amd64.tar.zst"

[resources.network]
mode = "dhcp"

[resources.features.tailscale]
enabled = true
mode = "router"
auth_key = "${TAILSCALE_AUTH_KEY}"
advertise_routes = ["192.168.86.0/24"]
tags = ["${const.default_tailnet_tag}"]

[[resources]]
name = "media-stack"
kind = "vm"
role = "media_stack"
vmid = 210
template = "ubuntu-24-04-cloudinit-template"
cores = 6
memory = 16384
disk_gb = 64
depends_on = ["tailscale-gateway"]

[resources.network]
mode = "dhcp"
mac = "BC:24:11:B7:5E:27"

[resources.cloud_init]
user = "ubuntu"
ssh_key_file = "/root/.ssh/media_stack.pub"

[resources.features.docker]
enabled = true

[resources.features.tailscale]
enabled = true
mode = "client"
auth_key = "${TAILSCALE_AUTH_KEY}"
tags = ["${const.default_tailnet_tag}"]

[resources.features.intel_igpu]
enabled = true
pci_device = "00:02.0"

[resources.features.media_services]
enabled = true
minimal = true
services = [
  "jellyfin",
  "sonarr",
  "radarr",
  "prowlarr",
  "qbittorrent",
  "jellyseerr",
  "bazarr",
  "homarr",
  "jellystat-db",
  "jellystat"
]
```

---


## 10.6 Global Interpolation Model (`[const]` and `[env]`)

Instead of introducing many special-purpose top-level sections like `[tailscale]`, `vmctl` should support a general interpolation system that can be used anywhere in the config.

### Goals

- reduce top-level special cases
- support reusable values across resources
- allow shared config without backend coupling
- make config more expressive and DRY
- support future feature modules without expanding the top-level schema unnecessarily

### Top-Level Reserved Namespaces

The config should support:

- `[const]` for reusable constant values
- `[env]` for environment-derived values or symbolic env bindings

Example:

```toml
[const]
domain = "home.arpa"
bridge = "vmbr0"
tailscale_gateway = "tailscale-gateway"
media_profile = "minimal"

[env]
PROXMOX_TOKEN_ID = "${PROXMOX_TOKEN_ID}"
PROXMOX_TOKEN_SECRET = "${PROXMOX_TOKEN_SECRET}"
TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"

[backend]
kind = "terraform"

[backend.proxmox]
endpoint = "https://mini:8006/api2/json"
node = "mini"
token_id = "${PROXMOX_TOKEN_ID}"
token_secret = "${PROXMOX_TOKEN_SECRET}"

[defaults]
bridge = "${bridge}"
searchdomain = "${domain}"

[[resources]]
name = "media-stack"
kind = "vm"
depends_on = ["${tailscale_gateway}"]

[resources.features.tailscale]
enabled = true
auth_key = "${TAILSCALE_AUTH_KEY}"
```

### Naming Conventions

#### `[const]`
- keys should be lower-case by default
- recommended format: `snake_case`

#### `[env]`
- keys should be uppercase by default
- recommended format: `SCREAMING_SNAKE_CASE`

### Resolution Order

Interpolation must be deterministic and occur in this order:

1. shorthand direct references against `[const]` and `[env]`
2. explicit `const.*`
3. explicit `env.*`
4. full-path config references

This means all of these are valid:

- `${some_const_value}`
- `${TAILSCALE_AUTH_KEY}`
- `${const.some_const_value}`
- `${env.TAILSCALE_AUTH_KEY}`
- `${defaults.bridge}`
- `${backend.proxmox.node}`

### Supported Reference Forms

#### Direct const reference
```toml
"${bridge}"
```

#### Direct env reference
```toml
"${TAILSCALE_AUTH_KEY}"
```

#### Explicit const reference
```toml
"${const.bridge}"
```

#### Explicit env reference
```toml
"${env.TAILSCALE_AUTH_KEY}"
```

#### Full-path config reference
```toml
"${defaults.bridge}"
"${backend.proxmox.node}"
```

### Direct Access Rule

Both `[const]` and `[env]` support direct access without full path.

So these are equivalent when the keys exist:

```toml
"${bridge}" == "${const.bridge}"
"${TAILSCALE_AUTH_KEY}" == "${env.TAILSCALE_AUTH_KEY}"
```

Full path is optional, not required.

### Semantics of `[env]`

`[env]` acts as an environment binding and transformation layer.

Example identity binding:

```toml
[env]
TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"
```

This means:
- read `TAILSCALE_AUTH_KEY` from the external process environment
- expose it to interpolation as `TAILSCALE_AUTH_KEY` or `env.TAILSCALE_AUTH_KEY`

This may look like a no-op, but it provides a consistent indirection layer and allows composition.

For example:

```toml
[env]
VMCTL_PREFIX = "${VMCTL_PREFIX}"
TAILSCALE_AUTH_KEY = "${VMCTL_PREFIX}_${TAILSCALE_AUTH_KEY}"
```

or aliasing:

```toml
[env]
PROXMOX_TOKEN_SECRET = "${PVE_TOKEN_SECRET}"
```

Now the config can consistently refer to `${PROXMOX_TOKEN_SECRET}` even if the real external environment variable is named `PVE_TOKEN_SECRET`.

### Important Distinction

There are two layers:

1. **config interpolation**
   - replaces placeholders with config-defined values or external env bindings
   - e.g. `${bridge}` -> `vmbr0`

2. **runtime secret/env resolution**
   - for `[env]`, values may themselves come from the external process environment
   - missing required external variables should be validation errors

### Path Reference Rules

Full-path references use dot notation and refer to the normalized config tree.

Examples:

```toml
"${backend.proxmox.node}"
"${defaults.storage}"
"${const.domain}"
"${env.TAILSCALE_AUTH_KEY}"
```

To keep complexity manageable in Phase 1:

- path references should only target scalar values
- array element indexing can be deferred
- resource-name path references can be added later if needed

### Interpolation Timing

Interpolation should happen after:
- TOML parsing
- initial syntactic validation

But before:
- semantic validation
- role expansion
- backend rendering

### Example Resolution Pipeline

Input:

```toml
[const]
bridge = "vmbr0"
domain = "home.arpa"

[env]
TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"

[defaults]
bridge = "${bridge}"
searchdomain = "${domain}"

[[resources]]
name = "media-stack"
kind = "vm"

[resources.features.tailscale]
enabled = true
auth_key = "${TAILSCALE_AUTH_KEY}"
```

Resolved form:

```toml
[defaults]
bridge = "vmbr0"
searchdomain = "home.arpa"

[[resources]]
name = "media-stack"
kind = "vm"

[resources.features.tailscale]
enabled = true
auth_key = "<resolved-from-process-env>"
```

### Validation Rules

Validation should enforce:

- `[const]` keys are unique
- `[env]` keys are unique
- unresolved placeholders are errors
- cyclic references are errors
- direct `${name}` may resolve from either `[const]` or `[env]`
- if a direct reference name exists in both `[const]` and `[env]`, validation should fail due to ambiguity unless one is referenced explicitly
- explicit `${const.name}` and `${env.NAME}` always work
- full-path references must resolve to existing scalar values
- interpolation into non-string fields must result in a value that can be parsed/coerced into the target type

### Cycle Detection

The resolver must detect cycles like:

```toml
[const]
a = "${const.b}"
b = "${const.a}"
```

and also env-binding cycles like:

```toml
[env]
A = "${B}"
B = "${A}"
```

### Rust Design

Interpolation should be implemented in the `config` crate as a separate pass.

Suggested modules:

```text
config/
├── parse.rs
├── model.rs
├── interpolate.rs
├── validate.rs
└── defaults.rs
```

### Example Resolver API

```rust
pub fn resolve_interpolations(cfg: RawConfig, process_env: &std::collections::BTreeMap<String, String>)
    -> anyhow::Result<ResolvedConfig>
{
    // 1. build lookup tables for const/env/full-path references
    // 2. resolve direct const/env references
    // 3. resolve explicit const/env references
    // 4. resolve full-path references
    // 5. detect cycles, ambiguity, and missing references
    // 6. return resolved config
    todo!()
}
```

### Example Placeholder Extraction

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefKind {
    Direct(String),
    ConstPath(String),
    EnvPath(String),
    FullPath(String),
}

pub fn parse_placeholder(input: &str) -> Option<RefKind> {
    let inner = input.strip_prefix("${")?.strip_suffix("}")?;

    if let Some(rest) = inner.strip_prefix("const.") {
        return Some(RefKind::ConstPath(rest.to_string()));
    }

    if let Some(rest) = inner.strip_prefix("env.") {
        return Some(RefKind::EnvPath(rest.to_string()));
    }

    if inner.contains('.') {
        return Some(RefKind::FullPath(inner.to_string()));
    }

    Some(RefKind::Direct(inner.to_string()))
}
```

### Example Resolution Semantics

For `RefKind::Direct(name)`:

1. if `name` exists only in `[const]`, resolve from `[const]`
2. if `name` exists only in `[env]`, resolve from `[env]`
3. if `name` exists in both, error as ambiguous
4. if `name` exists in neither, error unresolved

### Example Test Cases

```rust
#[test]
fn resolves_direct_const_reference() {
    let cfg = sample_config(r#"
        [const]
        bridge = "vmbr0"

        [defaults]
        bridge = "${bridge}"
    "#);

    let resolved = resolve_interpolations(cfg, &Default::default()).unwrap();
    assert_eq!(resolved.defaults.bridge.as_deref(), Some("vmbr0"));
}

#[test]
fn resolves_direct_env_reference() {
    let cfg = sample_config(r#"
        [env]
        TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"

        [defaults.features.tailscale]
        auth_key = "${TAILSCALE_AUTH_KEY}"
    "#);

    let mut env = std::collections::BTreeMap::new();
    env.insert("TAILSCALE_AUTH_KEY".into(), "tskey-123".into());

    let resolved = resolve_interpolations(cfg, &env).unwrap();
    assert_eq!(
        resolved.defaults.features.tailscale.unwrap().auth_key.as_deref(),
        Some("tskey-123")
    );
}

#[test]
fn rejects_ambiguous_direct_reference() {
    let cfg = sample_config(r#"
        [const]
        value = "const-value"

        [env]
        value = "${VALUE}"

        [defaults]
        bridge = "${value}"
    "#);

    let env = std::collections::BTreeMap::from([("VALUE".into(), "env-value".into())]);
    assert!(resolve_interpolations(cfg, &env).is_err());
}
```

### Recommended Phase 1 Constraint

To keep implementation production-friendly but manageable:

- allow interpolation only in string-valued TOML fields initially
- after resolution, coerce into typed target values where needed
- defer richer composite interpolation until later

### Impact on Feature Modeling

This means feature-specific top-level sections like `[tailscale]` are unnecessary.

Instead of:

```toml
[tailscale]
auth_key = "TAILSCALE_AUTH_KEY"
subnet_router = "tailscale-gateway"
```

you can write:

```toml
[const]
tailscale_gateway = "tailscale-gateway"

[env]
TAILSCALE_AUTH_KEY = "${TAILSCALE_AUTH_KEY}"

[[resources]]
name = "tailscale-gateway"
kind = "lxc"
role = "tailscale_gateway"

[[resources]]
name = "media-stack"
kind = "vm"
depends_on = ["${tailscale_gateway}"]

[resources.features.tailscale]
enabled = true
auth_key = "${TAILSCALE_AUTH_KEY}"
```

This is more general, cleaner, and better aligned with the future custom backend path.

---


## 10.7 Generic Rust Engine, Config-Driven Feature Packs

A core requirement for `vmctl` is that the Rust implementation should remain generic and stable even as higher-level platform features are added, updated, or removed.

This means:

- the CLI Rust code should **not** need to change every time a new role, service bundle, bootstrap file, or platform feature is introduced
- feature behavior should be expressed through **structured configuration and templates/files**
- Rust should provide the **generic engine** that loads, validates, resolves, composes, and renders those feature definitions

### Architectural Principle

Use:

- **Rust code** for the engine
- **config/templates/files** for feature definitions and expansions

So the engine knows **how** to process a feature pack, but not necessarily the hardcoded contents of every feature.

### What stays in Rust

The following remain code-driven:

- TOML parsing
- interpolation
- validation
- default resolution
- role/feature composition engine
- dependency graph and orchestration
- lockfile management
- backend abstraction
- backend compilation and execution
- rendering pipeline
- test harnesses and schemas

### What becomes configuration-driven

The following should be customizable without changing CLI code:

- resource roles (e.g. `media_stack`, `tailscale_gateway`)
- service catalogs
- feature bundles
- bootstrap command definitions
- file generation rules
- compose fragments
- cloud-init snippets
- provisioning templates
- package lists
- role defaults
- service enable/disable lists

### Result

This gives us a Rust platform engine that is:

- generic
- reusable
- testable
- future-proof

while still allowing users to evolve platform behavior by editing feature definitions and templates rather than recompiling the CLI.

### Recommended Filesystem Layout

```text
vmctl/
├── vmctl.toml
├── vmctl.lock
├── packs/
│   ├── roles/
│   │   ├── media_stack.toml
│   │   ├── tailscale_gateway.toml
│   │   └── docker_guest.toml
│   ├── services/
│   │   ├── jellyfin.toml
│   │   ├── sonarr.toml
│   │   ├── radarr.toml
│   │   ├── qbittorrent.toml
│   │   └── prowlarr.toml
│   ├── templates/
│   │   ├── docker-compose.media.hbs
│   │   ├── media.env.hbs
│   │   ├── cloud-init.vm.hbs
│   │   └── tailscale-setup.sh.hbs
│   └── scripts/
│       ├── bootstrap-media.sh
│       └── bootstrap-tailscale.sh
└── crates/
    └── ...
```

### Example Role Pack

```toml
name = "media_stack"
kind = "vm"

[defaults]
requires = ["docker"]

[features.media_services]
enabled = true
minimal = true
services = [
  "jellyfin",
  "sonarr",
  "radarr",
  "prowlarr",
  "qbittorrent",
  "jellyseerr",
  "bazarr",
  "homarr",
  "jellystat-db",
  "jellystat"
]

[render]
templates = [
  "docker-compose.media.hbs",
  "media.env.hbs"
]

[scripts]
bootstrap = ["bootstrap-media.sh"]
```

### Example Service Pack

```toml
name = "jellyfin"
container_type = "docker"

[image]
name = "lscr.io/linuxserver/jellyfin"
tag = "latest"

[ports]
published = ["8096:8096"]

[volumes]
mounts = [
  "${MEDIA_PATH}:/media",
  "${CONFIG_PATH}/jellyfin:/config"
]
```

### Generic Engine Flow

1. load `vmctl.toml`
2. resolve interpolation and defaults
3. load pack definitions from `packs/`
4. resolve resource roles into feature packs
5. resolve service packs and template references
6. merge all expansions into a normalized desired-state model
7. validate resulting graph
8. pass desired state to backend adapter
9. render/apply backend artifacts

### Important Constraint

Feature packs must remain declarative.

Do **not** introduce arbitrary scripting logic into feature pack definitions.

The engine should support:
- declarative data
- templated files
- explicit bootstrap hooks

But not:
- embedded custom programming languages
- unrestricted feature execution logic hidden in config

### Why This Matters for the Terraform Upgrade Path

If higher-level behavior lives in feature packs and generic Rust expansion logic, then:

- the Terraform backend can consume the same normalized desired state now
- a future native backend can consume the same normalized desired state later

This makes feature behavior backend-independent.

### Rust Module Suggestion

Add a dedicated crate or module for packs:

```text
crates/
├── packs/
│   ├── loader.rs
│   ├── model.rs
│   ├── validate.rs
│   ├── expand.rs
│   └── merge.rs
```

### Example Rust Trait

```rust
pub trait PackExpander {
    fn expand(
        &self,
        resource: &ResolvedResource,
        pack_registry: &PackRegistry,
    ) -> anyhow::Result<Expansion>;
}
```

### Example Expansion Model

```rust
#[derive(Debug, Default)]
pub struct Expansion {
    pub files: Vec<GeneratedFile>,
    pub service_defs: Vec<ServiceDef>,
    pub bootstrap_steps: Vec<BootstrapStep>,
    pub dependencies: Vec<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
}
```

### Example TDD Requirements

Tests should verify that:

- adding a new role pack does not require CLI changes
- changing a service pack changes rendered output deterministically
- removing a service pack produces a validation error or diff as expected
- role expansion remains backend-agnostic
- packs are schema-validated on load

### Definition of Done

This requirement is complete when:

- new roles can be added by dropping files into `packs/roles/`
- new services can be added by dropping files into `packs/services/`
- templates can be modified without Rust CLI changes
- CLI commands remain unchanged for add/update/remove workflows
- planner and backend receive a stable normalized desired-state model


---

## 11. CLI Design

## 11.1 Commands

```bash
vmctl init
vmctl validate
vmctl plan
vmctl plan media-stack
vmctl apply
vmctl apply media-stack
vmctl destroy media-stack
vmctl import
vmctl sync
vmctl backend doctor
```

## 11.2 Backend-Agnostic UX

The CLI should not expose Terraform unless necessary.

Prefer:
- `vmctl plan`
- `vmctl apply`

Avoid:
- forcing users to run `terraform` directly as part of the primary workflow

For debugging, backend-specific commands can exist under a namespace:
- `vmctl backend doctor`
- `vmctl backend render`
- `vmctl backend show-state`

## 11.3 clap Example

```rust
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "vmctl", version, about = "Declarative Proxmox homelab manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Init,
    Validate,
    Import,
    Sync,
    Plan {
        target: Option<String>,
    },
    Apply {
        target: Option<String>,
    },
    Destroy {
        target: String,
    },
    Backend {
        #[command(subcommand)]
        command: BackendCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum BackendCommand {
    Doctor,
    Render,
    ShowState,
}
```

---

## 12. Backend Abstraction

This is the key to the future migration path.

## 12.1 Core Backend Trait

```rust
use async_trait::async_trait;

#[async_trait]
pub trait EngineBackend {
    async fn validate_backend(&self, workspace: &Workspace) -> anyhow::Result<()>;
    async fn refresh_actual_state(&self, workspace: &Workspace) -> anyhow::Result<ActualState>;
    async fn render(&self, workspace: &Workspace, desired: &DesiredState) -> anyhow::Result<RenderResult>;
    async fn plan(&self, workspace: &Workspace, desired: &DesiredState) -> anyhow::Result<BackendPlan>;
    async fn apply(&self, workspace: &Workspace, desired: &DesiredState) -> anyhow::Result<ApplyResult>;
    async fn destroy(&self, workspace: &Workspace, target: &TargetSelector) -> anyhow::Result<ApplyResult>;
    async fn import(&self, workspace: &Workspace) -> anyhow::Result<ImportedState>;
}
```

## 12.2 Backend Types

```rust
pub enum BackendKind {
    Terraform,
    Native,
}
```

## 12.3 Why This Matters

If the domain/planning layers only depend on `EngineBackend`, then replacing Terraform later becomes:
- implementing the same trait for `backend-native`
- keeping the same config model and CLI surface
- gradually changing default backend

This is the cleanest upgrade path.

---

## 13. Terraform Backend Design

## 13.1 Terraform/OpenTofu Position in the Architecture

Terraform is a compiled target, not your public domain model.

`backend-terraform` responsibilities:
- generate Terraform module structure or JSON HCL
- generate `terraform.tfvars.json` or equivalent inputs
- manage working directory layout
- run `terraform/tofu init`, `plan`, `apply`, `import`
- parse outputs when needed

## 13.2 Artifact Layout

```text
backend/generated/
└── workspace/
    ├── main.tf.json
    ├── variables.tf.json
    ├── terraform.tfvars.json
    ├── outputs.tf.json
    ├── modules/
    │   ├── vm/
    │   ├── lxc/
    │   ├── tailscale_gateway/
    │   └── media_stack/
    └── .terraform/
```

Generated artifacts can be:
- ephemeral
- committed optionally for debugging
- re-rendered deterministically

## 13.3 Compilation Strategy

Rust compiles:
- config -> desired state -> backend graph -> Terraform files

Do **not** handwrite complex Terraform as the primary source of truth.

## 13.4 Terraform Module Strategy

Base modules:
- `vm`
- `lxc`

Role modules:
- `tailscale_gateway`
- `media_stack`

These may internally compose base modules and bootstrap artifacts.

## 13.5 Backend Render Example

```rust
pub struct TerraformBackend;

#[async_trait::async_trait]
impl EngineBackend for TerraformBackend {
    async fn render(&self, workspace: &Workspace, desired: &DesiredState) -> anyhow::Result<RenderResult> {
        let rendered = compile_to_terraform(desired)?;
        write_terraform_files(workspace, &rendered)?;
        Ok(RenderResult {
            summary: "rendered terraform backend".into(),
        })
    }

    async fn plan(&self, workspace: &Workspace, desired: &DesiredState) -> anyhow::Result<BackendPlan> {
        self.render(workspace, desired).await?;
        run_terraform_init(workspace)?;
        let output = run_terraform_plan(workspace)?;
        Ok(parse_terraform_plan(output)?)
    }

    async fn apply(&self, workspace: &Workspace, desired: &DesiredState) -> anyhow::Result<ApplyResult> {
        self.render(workspace, desired).await?;
        run_terraform_init(workspace)?;
        let output = run_terraform_apply(workspace)?;
        Ok(parse_terraform_apply(output)?)
    }

    async fn validate_backend(&self, workspace: &Workspace) -> anyhow::Result<()> {
        ensure_binary_exists("tofu").or_else(|_| ensure_binary_exists("terraform"))?;
        Ok(())
    }

    async fn refresh_actual_state(&self, _workspace: &Workspace) -> anyhow::Result<ActualState> {
        anyhow::bail!("not implemented")
    }

    async fn destroy(&self, _workspace: &Workspace, _target: &TargetSelector) -> anyhow::Result<ApplyResult> {
        anyhow::bail!("not implemented")
    }

    async fn import(&self, _workspace: &Workspace) -> anyhow::Result<ImportedState> {
        anyhow::bail!("not implemented")
    }
}
```

---

## 14. Planning Model

## 14.1 Important Distinction

There are two “plans” in a hybrid architecture:

### `vmctl` Plan
Domain-aware, high-level, user-friendly plan:
- create media-stack VM
- update tailscale routes
- attach iGPU passthrough
- enable Docker bootstrap

### Terraform Plan
Low-level provider execution plan:
- add proxmox_virtual_environment_vm.x
- update memory field
- change network config

`vmctl` should present the first as the primary UX.

Terraform plan may be used under the hood, and optionally displayed in debug mode.

## 14.2 Planning Stages

1. config parsed and validated
2. roles expanded
3. desired state normalized
4. `vmctl` diff rendered
5. backend render performed
6. backend plan generated
7. backend result summarized back into `vmctl` UX

This preserves UX control.

---

## 15. Import / Sync / Lockfile Strategy

## 15.1 Hybrid Reality

Because Terraform has its own state and Proxmox has live state, `vmctl` must mediate among:
- `vmctl.toml`
- `vmctl.lock`
- Terraform state
- Proxmox actual state

## 15.2 Recommended Rule

`vmctl.toml` remains the source of truth for desired state.

`vmctl.lock` stores:
- name ↔ vmid mappings
- backend metadata
- last rendered backend artifact checksum
- imported/observed metadata
- selected actual-state snapshots

Terraform state remains backend-owned.

## 15.3 Why Keep a vmctl Lockfile Anyway

Because even with Terraform:
- `vmctl` still needs its own stable model and mapping layer
- future backend replacement becomes easier
- import/merge can be backend-independent
- user-facing tooling does not need to parse Terraform state directly everywhere

## 15.4 Lockfile Example

```toml
version = 1
backend = "terraform"
generated_at = "2026-04-18T12:00:00Z"

[[resources]]
name = "media-stack"
kind = "vm"
vmid = 210
backend_address = "module.media_stack.proxmox_virtual_environment_vm.this"
digest = "sha256:..."
exists = true

[[resources]]
name = "tailscale-gateway"
kind = "lxc"
vmid = 101
backend_address = "module.tailscale_gateway.proxmox_virtual_environment_container.this"
digest = "sha256:..."
exists = true
```

---

## 16. Upgrade Path to a Native Engine

This is the most important design constraint in the plan.

## 16.1 Rule: Never Leak Terraform into the Domain Model

Bad:
- `resource_address`
- `terraform_module_name`
- HCL fragments in user config

Good:
- `role`
- `kind`
- `network.mode`
- `features.tailscale`
- `depends_on`

Terraform-specific metadata may exist in:
- lockfile
- backend-generated artifacts
- backend implementation crate

But not in the core model.

## 16.2 Rule: Define Native Semantics Now

Even while using Terraform:
- define your own action model
- define your own plan renderer
- define your own desired-state types
- define your own import model

This makes backend replacement feasible.

## 16.3 Migration Stages

### Stage 1
Terraform backend only

### Stage 2
Introduce `backend-native` with read-only Proxmox inventory support

### Stage 3
Support native `plan` alongside Terraform backend

### Stage 4
Support native `apply` for a subset of actions

### Stage 5
Flip default backend for supported workloads

This allows incremental migration, not a rewrite.

## 16.4 Native Engine Target Trait Compatibility

If both backends implement `EngineBackend`, the rest of the application remains unchanged.

That is the upgrade path.

---

## 17. Initial Resource Profiles

## 17.1 Tailscale Gateway LXC

Purpose:
- provides subnet routing
- optional gateway role for remote access

Profile semantics:
- LXC kind
- Debian/Ubuntu template
- Tailscale router bootstrap
- may advertise subnet routes
- optional dependency target for other resources

Terraform backend initially:
- provisions LXC
- injects bootstrap/user-data/artifacts
- may orchestrate post-create scripts

Future native engine:
- same desired model
- direct Proxmox API + optional guest bootstrap

## 17.2 Media Stack VM

Purpose:
- Jellyfin + minimal media services
- optional Tailscale client
- optional Docker bootstrap
- optional iGPU passthrough

Profile semantics:
- VM kind
- Docker enabled
- media services profile enabled
- Tailscale client enabled
- iGPU passthrough optional

Terraform backend initially:
- create VM
- cloud-init bootstrap
- generate install artifacts/compose
- optional post-boot provisioning hooks

Future native engine:
- same role semantics, different execution path

---

## 18. TDD Strategy

## 18.1 Why TDD Matters More in Hybrid

Because there are now **two levels of logic**:
- backend-agnostic domain/orchestration logic
- backend-specific compilation/execution logic

TDD protects the domain from backend coupling.

## 18.2 What Must Be Tested First

1. config parsing
2. interpolation resolution
3. default inheritance
4. role expansion
5. dependency resolution
6. target selection
7. lockfile model
8. backend trait contract
9. Terraform compilation output
10. `vmctl` plan rendering

## 18.3 Example Tests

### Config validation
```rust
#[test]
fn rejects_duplicate_vmids() {
    let cfg = sample_config_with_duplicate_vmids();
    let err = validate_config(&cfg).unwrap_err();
    assert!(err.to_string().contains("duplicate VMID"));
}
```

### Role expansion
```rust
#[test]
fn media_stack_role_enables_expected_features() {
    let resource = sample_media_stack_resource();
    let expanded = expand_roles(resource).unwrap();
    assert!(expanded.features.docker.as_ref().unwrap().enabled);
    assert!(expanded.features.media_services.as_ref().unwrap().enabled);
}
```

### Backend-agnostic planner
```rust
#[test]
fn planner_preserves_dependency_order() {
    let desired = sample_desired_state_with_gateway_and_media();
    let plan = build_vmctl_plan(&desired).unwrap();
    assert_eq!(plan.steps[0].resource_name(), "tailscale-gateway");
}
```

### Terraform compiler
```rust
#[test]
fn terraform_render_contains_expected_vm_module() {
    let desired = sample_media_stack_desired_state();
    let rendered = compile_to_terraform(&desired).unwrap();
    assert!(rendered.main_tf_json.contains("media-stack"));
}
```

---

## 19. DRY and Code Quality Rules

- one normalization pipeline for desired state
- one role expansion engine
- one validation pipeline
- one backend trait
- one target selection mechanism
- one lockfile mapping model

Avoid:
- duplicating config field resolution in planner and backend
- embedding backend assumptions in validators
- making CLI commands rebuild logic manually

---

## 20. Task Breakdown

## Phase 0 – Foundation
Tasks:
- create workspace
- add crates
- add linting/CI
- add common dependencies

Definition of Done:
- workspace builds
- fmt/clippy configured
- CI baseline exists

## Phase 1 – Config + Domain
Tasks:
- define config schema
- parse TOML
- implement interpolation engine for `[const]`, `[env]`, and full-path references
- validate config
- resolve defaults
- define desired-state model
- define pack schema model

Definition of Done:
- config model stable
- interpolation resolution works with deterministic precedence
- cycle detection and unresolved references are tested
- role expansion tested
- backend-free domain model complete

## Phase 2 – Backend Abstraction
Tasks:
- define backend trait
- define backend result types
- define workspace/context abstractions
- add placeholder native backend crate

Definition of Done:
- planner and CLI depend only on backend trait
- native backend placeholder compiles

## Phase 3 – Terraform Backend
Tasks:
- implement Terraform renderer
- generate module inputs/files
- run terraform/tofu commands
- parse plan/apply summaries
- add backend doctor command

Definition of Done:
- backend can render and apply sample config
- no Terraform types leak into core crates

## Phase 4 – vmctl Planner UX
Tasks:
- implement high-level vmctl planner
- render human-friendly plan
- summarize backend plan
- target selection support

Definition of Done:
- `vmctl plan` is useful without reading raw Terraform output

## Phase 5 – Import / Lockfile
Tasks:
- define lockfile model
- map resources to backend addresses
- implement sync/import flow
- compare/merge helpers

Definition of Done:
- `vmctl sync` updates lockfile
- `vmctl import` generates useful config fragments

## Phase 6 – Initial Packs and Roles
Tasks:
- implement pack loader and validation
- tailscale gateway role pack
- media stack role pack
- service packs for initial media services
- Docker bootstrap artifact generation
- Tailscale bootstrap generation
- iGPU passthrough support

Definition of Done:
- both initial role profiles are usable end-to-end through Terraform backend

## Phase 7 – Migration Readiness
Tasks:
- implement read-only native inventory backend
- backend contract tests
- identify Terraform-specific assumptions
- reduce backend leakage further

Definition of Done:
- architecture demonstrably supports a second backend

---

## 21. Definition of Done by Major Component

## 21.1 Config Layer
Done when:
- config parses
- validation catches invalid resource graphs
- defaults resolve deterministically
- examples round-trip
- tests pass

## 21.2 Role Expansion
Done when:
- roles expand into normalized features
- feature conflicts are rejected
- shared config inheritance is preserved
- tests pass

## 21.3 Backend Trait
Done when:
- CLI does not know whether backend is Terraform or native
- planner can use backend trait only
- trait is covered by contract tests

## 21.4 Terraform Backend
Done when:
- generated artifacts are deterministic
- `plan` and `apply` work for sample resources
- backend doctor validates required binaries
- module generation is tested

## 21.5 Upgrade Path
Done when:
- native backend placeholder compiles
- no Terraform-specific config required from user
- lockfile contains backend metadata, not domain metadata
- migration path is documented

---

## 22. Risks and Mitigations

## Risk: Terraform leaks into core model
Mitigation:
- strict crate boundaries
- code review rule: no Terraform terms in `domain` crate

## Risk: vmctl becomes a thin wrapper
Mitigation:
- keep `vmctl` plan UX separate from Terraform output
- implement role expansion and shared config resolution in Rust first

## Risk: future native migration becomes painful
Mitigation:
- build backend trait now
- keep lockfile backend-neutral with backend metadata extensions
- create placeholder native backend crate immediately

## Risk: cross-resource shared config becomes tangled
Mitigation:
- centralize shared config resolution before backend generation
- represent dependencies explicitly

---

## 23. Recommended Final Direction

For your use case, the best architecture is:

### Now
- Rust-first `vmctl`
- Terraform/OpenTofu backend adapter
- TOML source of truth
- role- and pack-based shared config
- lockfile owned by vmctl

### Later
- add native backend in parallel
- migrate selected workflows off Terraform
- keep same CLI and config

This gives you:
- fast time to value
- strong shared-config ergonomics
- long-term independence from Terraform

---

## 24. Immediate Next Steps

1. revise current `vmctl` plan to adopt backend abstraction
2. scaffold workspace with:
   - `config`
   - `domain`
   - `planner`
   - `backend`
   - `backend-terraform`
   - `backend-native`
   - `cli`
3. implement TOML schema and validators
4. implement pack schema and loader
5. implement role expansion using packs for:
   - `tailscale_gateway`
   - `media_stack`
6. implement Terraform backend renderer for the initial resource set
7. add contract tests ensuring a native backend can be added later

---

## 25. Appendix: Example Backend Trait Contract Test

```rust
pub async fn backend_contract_can_validate<B: EngineBackend>(
    backend: &B,
    workspace: &Workspace,
) {
    backend.validate_backend(workspace).await.unwrap();
}
```

## 26. Appendix: Example Render Flow

```rust
pub async fn run_plan(
    backend: &dyn EngineBackend,
    workspace: &Workspace,
    desired: &DesiredState,
) -> anyhow::Result<()> {
    let vmctl_plan = build_vmctl_plan(desired)?;
    println!("{}", render_vmctl_plan(&vmctl_plan));

    let backend_plan = backend.plan(workspace, desired).await?;
    println!("{}", render_backend_summary(&backend_plan));

    Ok(())
}
```

## 27. Appendix: Example Role Expansion

```rust
pub fn expand_media_stack(mut resource: ResourceConfig) -> anyhow::Result<ResourceConfig> {
    resource.features.docker.get_or_insert(EnabledFeature { enabled: true });
    resource.features.media_services.get_or_insert(MediaServicesFeature {
        enabled: true,
        minimal: true,
        services: vec![
            "jellyfin".into(),
            "sonarr".into(),
            "radarr".into(),
            "prowlarr".into(),
            "qbittorrent".into(),
            "jellyseerr".into(),
            "bazarr".into(),
            "homarr".into(),
            "jellystat-db".into(),
            "jellystat".into(),
        ],
    });
    Ok(resource)
}
```

---

End of hybrid plan.
