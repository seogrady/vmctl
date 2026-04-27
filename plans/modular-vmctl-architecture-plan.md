# vmctl – Modular Architecture Plan
## Production-Ready Services, Orchestrator DAG, and Docker/Podman Runtime Abstraction

This document defines a production-ready modular architecture for `vmctl` that improves separation of concerns, introduces a self-contained service domain model, and adds an orchestrator that resolves dependencies, deduplicates work, and executes in deterministic order. It also defines a container runtime abstraction so services can run on either Docker or Podman without changing service definitions.

This is a design plan only. It intentionally does not implement code.

---

## 1. Executive Summary

`vmctl` currently uses a “services/resources” concept that spreads ownership across global directories and couples:

- environment variables and config
- templates and rendering concerns
- provisioning scripts and runtime execution
- service/container definitions

The proposed architecture replaces “services/resources” with a **first-class service system**:

- Each service is self-contained and owns its config schema, templates, provisioning, validation, and container definitions.
- A new orchestrator builds a **service dependency DAG**, deduplicates repeated dependencies, supports partial runs, and executes deterministically via topological sorting.
- `vmctl.toml` becomes minimal, turning service configuration “inside out” so services own their defaults and schema.
- A runtime abstraction allows services to run under Docker or Podman interchangeably.

---

## 2. Goals and Non-Goals

### 2.1 Primary Goals

1. Improve separation of concerns across all resources and services.
2. Introduce a domain model for self-contained, composable services/packages.
3. Add an orchestrator to resolve dependencies, dedupe execution, and determine correct order.
4. Reduce complexity/size of `vmctl.toml` by pushing config ownership into services.
5. Support Docker and Podman as interchangeable container runtimes.
6. Produce a clean, scalable, maintainable system with clear ownership boundaries.

### 2.2 Non-Goals (Explicit)

- Replacing the Terraform/OpenTofu backend in the same refactor.
- Building a general-purpose package manager.
- Solving DRM or “prevent downloads” problems (out of scope for infrastructure orchestration).
- Making Dolby Vision tone mapping universally available on all GPUs (runtime capability remains hardware/driver-dependent).

---

## 3. Current Problems (Why This Change)

### 3.1 Ownership and Coupling Issues

- Global `services/resources/templates`, `services/resources/scripts`, and `services/resources/services` create cross-cutting coupling.
- “Role” definitions fan out into templates + scripts + services, but no single owner owns “the jellyfin stack” end-to-end.
- Environment variables are defined and used across multiple files without a schema or authoritative source of truth.
- No formal dependency graph at the “service/service” level, so ordering and dedupe are ad hoc.

### 3.2 Scaling and Extensibility Issues

- Adding a new service requires editing multiple global locations.
- Swapping container runtime (Docker → Podman) requires touching scripts everywhere.
- Duplicated logic appears across scripts/templates for common patterns (ARR apps, base routing, shared volumes).

---

## 4. Target Architecture Overview

The target system has four core concepts:

1. **Service Manifest**: declaratively defines inputs, defaults, outputs, dependencies, templates, scripts, and runtime requirements.
2. **Service Instance**: a service bound to a specific target (workspace, resource, or scope).
3. **Orchestrator**: loads services, resolves dependencies, builds a DAG, deduplicates nodes, and produces an ordered execution plan.
4. **Runtime Abstraction**: a uniform interface for container operations (compose up/down, networks, volumes, exec, logs) backed by Docker or Podman adapters.

Key design rule:

> Services do not call Docker or Podman directly. Services call a stable `vmctl` runtime interface that is implemented by adapters.

---

## 5. Domain Model Definition

This is the minimal domain model needed to meet the objective. It deliberately mirrors the repo’s existing structure (workspace/config/planner/provision) while formalizing modular ownership.

### 5.1 Service Identity and Scoping

**ServiceId**
- `name`: `jellyfin`, `sonarr`, `media-stack`, `caddy`, etc.
- `version`: semantic version (example: `1.2.0`) for lockfile pinning and reproducibility.

**ServiceScope**
- `workspace`: runs once per workspace (example: `ui-routing`, `global-dns`)
- `resource`: runs per resource (example: `media-stack` VM service)
- `host`: runs on Proxmox host (example: `hostpci-mapping`, `bridge-config`) when needed

**ServiceInstanceKey**
- `(module_id, scope, target_id)`
- Used for dedupe: the orchestrator executes a service instance exactly once per key.

### 5.2 Service Inputs and Config Ownership

Each service defines:

- input schema (required vs optional)
- defaults
- validation rules (type + allowed values + constraints)
- how its inputs are sourced (workspace config overrides; resource-level overrides; env passthrough)

Important property:

> `vmctl.toml` should never contain service-specific “implementation config” (ports, volumes, image tags) unless explicitly exposed as service inputs.

### 5.3 Service Outputs

Services publish outputs for downstream services to consume:

- endpoints (host/port/path, base URLs)
- generated file paths (in rendered directory)
- runtime handles (compose project name, network names)
- integration secrets/keys (references to secret material, not raw values)

Outputs must be serializable and stable (for caching, planning UX, and reproducibility).

### 5.4 Service Dependencies

Each service defines:

- required dependencies: always included
- optional dependencies: included if enabled by config or capability detection
- capability constraints (example: “requires VAAPI device present”; “requires `docker compose` or `podman compose`”)

Dependencies are expressed in service coordinates (not file paths) so they can be deduped and resolved by the orchestrator.

### 5.5 Service Lifecycle

Each service instance progresses through a strict lifecycle. The orchestrator owns transitions and ordering.

States:

1. `Discovered`: manifest found on disk.
2. `Loaded`: manifest parsed; basic schema validity checked.
3. `Configured`: inputs resolved via precedence (defaults → workspace overrides → resource overrides).
4. `Planned`: dependency closure computed; instance is in DAG with deterministic ordering.
5. `Rendered`: templates/materialized files written into `generated/<target>/<service>/`.
6. `Provisioned`: service provision script executed successfully (idempotent convergence).
7. `Validated`: PDV script executed successfully; service outputs considered “trusted”.

Failure semantics:

- Fail fast at `Loaded`/`Configured` on schema errors (actionable messages: which key, expected type, allowed values).
- Fail fast at `Planned` on cycles (actionable cycle path).
- Fail at `Provisioned`/`Validated` with last-known logs and concrete endpoints/commands to debug.

---

## 6. Service Structure (Filesystem Layout)

Standard service layout (workspace-relative):

```text
services/
  jellyfin/
    service.toml
    templates/
      docker-compose.yml.hbs
      caddyfile.hbs
      jellyfin.encoding.xml.hbs
    scripts/
      provision.sh
      validate.sh
      lib/
        runtime.sh
        http.sh
    images/
      README.md
```

Rules:

- `service.toml` is the authoritative manifest and entrypoint.
- `service.toml` defines the service environment contract, schema, defaults, secret references, and container runtime entities in a runtime-neutral representation.
- `templates/` only contains templates owned by the service.
- `scripts/` only contains scripts owned by the service. Shared logic lives in `scripts/lib/` (service-local) or in a shared helper service.

---

## 7. Service Manifest and Schema

### 7.1 `service.toml` Example

```toml
name = "jellyfin"
version = "1.3.0"
scope = "resource" # "workspace" | "resource" | "host"
targets = ["vm", "lxc"]

[description]
summary = "Jellyfin media server with HW acceleration support"
ui_name = "Jellyfin"
ui_description = "Media server UI and streaming endpoints"

[inputs]
# Declares which config keys the service owns and what is configurable.
# Types are validated at load time.
schema = [
  { key = "enabled", type = "bool", default = true },
  { key = "base_url", type = "string", default = "/jf" },
  { key = "http_port", type = "u16", default = 8096 },
  { key = "data_dir", type = "string", default = "/opt/media" },
  { key = "enable_hwaccel", type = "bool", default = true },
  { key = "hwaccel_device", type = "string", default = "/dev/dri/renderD128" },
  { key = "tonemap_mode", type = "string", default = "auto", allowed = ["auto", "force_on", "force_off"] },
]

[dependencies]
requires = ["container-runtime", "reverse-proxy"]
optional = ["intel-opencl-runtime"]

[runtime]
container_spec = "service.toml"
templates = [
  { src = "templates/docker-compose.yml.hbs", dst = "compose.d/jellyfin.yml" },
  { src = "templates/jellyfin.encoding.xml.hbs", dst = "jellyfin/encoding.xml" },
]

[scripts]
provision = "scripts/provision.sh"
validate = "scripts/validate.sh"

[outputs]
# Named outputs that other services can depend on.
publish = [
  { key = "jellyfin.base_url", type = "string" },
  { key = "jellyfin.http_port", type = "u16" },
  { key = "jellyfin.internal_url", type = "string" },
]
```

### 7.2 Environment Contract Example

The environment contract is defined inside `service.toml`. Values can come from:

- service defaults
- `vmctl.toml` overrides
- references to secrets (which are resolved at provision time)

```toml
[environment]
# Visible to scripts and containers.
JELLYFIN_BASE_URL = { from = "input.base_url" }
JELLYFIN_HTTP_PORT = { from = "input.http_port" }
JELLYFIN_DATA_DIR = { from = "input.data_dir" }

# Auto capability toggles:
JELLYFIN_HWACCEL_ENABLE = { from = "input.enable_hwaccel" }
JELLYFIN_HWACCEL_DEVICE = { from = "input.hwaccel_device" }
JELLYFIN_TONEMAP_MODE = { from = "input.tonemap_mode" }
```

### 7.3 `service.toml` Example (Runtime-Neutral)

`service.toml` is a runtime-neutral representation that can be rendered to Compose for both Docker and Podman.

```toml
[compose]
project = "media"
network = "media-net"

[services.jellyfin]
image = "jellyfin/jellyfin:10.10.0"
network_mode = "host"
devices = ["/dev/dri:/dev/dri"]
group_add = ["render"]
volumes = [
  "${JELLYFIN_DATA_DIR}/config/jellyfin:/config",
  "${JELLYFIN_DATA_DIR}/data:/data",
]
environment = [
  "JELLYFIN_PublishedServerUrl=http://media-stack:${JELLYFIN_HTTP_PORT}${JELLYFIN_BASE_URL}",
]
```

---

## 8. Orchestrator Design

### 8.1 Responsibilities

The orchestrator:

- loads service manifests from `services/`
- resolves enabled service instances (workspace + resources)
- resolves required/optional dependencies
- builds a dependency graph (DAG)
- deduplicates service instances by `(service, scope, target_id)`
- produces a deterministic execution plan via topological sort
- supports partial runs by target/service selection
- enforces idempotency through action fingerprints and replay-safe execution

### 8.2 Orchestrator Inputs and Outputs

Inputs:

- `vmctl.toml` (minimal service enablement + overrides)
- discovered services from `services/`
- current desired state (resources/images backend config), as today’s planner already constructs

Outputs:

- render plan (which service templates/scripts generate which artifacts)
- provision plan (ordered service provision actions)
- PDV plan (ordered validations)
- runtime plan (container runtime operations, abstracted)

### 8.3 Dependency Graph and Deduplication

Graph nodes:

- `ServiceInstance` (the unit of dedupe and ordering)

Graph edges:

- `A -> B` means “A requires outputs of B” and “B must run before A”

Deduplication:

- If services `sonarr` and `radarr` both require `reverse-proxy`, the orchestrator includes `reverse-proxy` once per relevant instance key.
- If `reverse-proxy` is `workspace` scope, it runs once for the workspace.
- If `reverse-proxy` is `resource` scope, it runs once per target resource.

### 8.4 Topological Sort and Cycle Detection (Pseudocode)

Kahn’s algorithm (deterministic):

```text
function topo_sort(graph):
  in_degree = map[node] = count_incoming_edges(node)
  ready = priority_queue(sorted by node.key) of nodes where in_degree[node] == 0
  order = []

  while ready not empty:
    n = pop(ready)
    order.push(n)
    for each m in graph.outgoing(n):
      in_degree[m] -= 1
      if in_degree[m] == 0:
        push(ready, m)

  if len(order) != graph.node_count:
    cycle = graph.find_cycle()
    fail("dependency cycle detected", cycle)

  return order
```

Determinism rule:

- Stable sort order uses `(scope, target_id, module_name, version)` to make execution repeatable.

### 8.5 Partial Runs

Support:

- `--resource media-stack` (current behavior conceptually)
- `--service jellyfin`
- `--service jellyfin --resource media-stack`

Selection algorithm:

- Build full DAG, then select requested nodes and all prerequisites via reverse-closure (dependencies).
- Topo-sort the induced subgraph.

### 8.6 Idempotency and Dedupe of Work

Each service instance produces a set of actions:

- render templates
- provision scripts
- runtime operations (compose up/down)
- validations

Each action has:

- `action_id` (stable identity)
- `fingerprint` (hash of relevant inputs, templates, scripts, and referenced versions)

Execution policy:

- If fingerprint matches last applied fingerprint in `vmctl.lock`, skip action.
- If not, execute action and update lockfile.

This reduces repeated re-provisioning and makes “apply” faster and safer.

### 8.7 Orchestrator Flow by Command

The orchestrator should produce consistent behavior across the CLI surface:

- `vmctl validate-config`: load services, validate manifests + config schema, but do not render or execute.
- `vmctl render`: resolve DAG, render all service artifacts into `generated/`, but do not provision.
- `vmctl plan`: resolve DAG, show ordered service instances, their actions, and which actions would run vs be skipped (fingerprint-based).
- `vmctl apply`: run render → provision → optional PDV in DAG order; update `vmctl.lock`.
- `vmctl provision`: run only provision actions (assumes render is available; can force render first).
- `vmctl validate`: run only PDV actions (optionally after ensuring runtime is up).
- `vmctl doctor`: verify host + guest dependencies (compose support, runtime availability), and optionally run selected PDV checks.

---

## 9. Configuration Model (Reducing `vmctl.toml`)

### 9.1 Minimal `vmctl.toml` (Example)

`vmctl.toml` becomes a service enablement and override file, not a service implementation file.

```toml
[backend]
kind = "tofu"

[resources.media_stack]
kind = "vm"
role = "media-stack" # optional; can become "services = [...]" in later iterations
vmid = 210

[runtime]
engine = "docker" # or "podman"

[services]
jellyfin = true
sonarr = true
radarr = true
prowlarr = true
reverse-proxy = true

[service.jellyfin]
base_url = "/jf"
enable_hwaccel = true
tonemap_mode = "auto"
```

### 9.2 Resource-Level Overrides (Example)

When the same service can be instantiated per resource, resource-specific overrides live under the resource:

```toml
[resources.media_stack.services.jellyfin]
http_port = 8096
data_dir = "/opt/media"
```

Precedence:

1. service defaults (in service files)
2. workspace service overrides (`[service.<name>]`)
3. resource service overrides (`[resources.<r>.services.<name>]`)

### 9.3 Service Defaults Live With Services

- Ports, volumes, image tags, and file locations are service defaults.
- `vmctl.toml` should only override what users explicitly want to control.

### 9.4 Replacing Resource Roles With Resource-Owned Composition

Resource-owned composition replaces global role files:

- A resource manifest is a named composition (example: `resources/media-stack/resource.toml`) that expands to an ordered list of services plus default overrides.
- Resource-owned composition keeps templates, provisioning scripts, validation scripts, and defaults next to the resource that owns them.

Example resource composition:

```toml
name = "media-stack"
kind = "vm"
services = ["container-runtime", "reverse-proxy", "jellyfin", "prowlarr", "sonarr", "radarr"]

[overrides.jellyfin]
base_url = "/jf"
```

This allows:

- users to keep `role = "media-stack"` while the internals shift to services, and
- advanced users to move to `resources.<r>.services = [...]` when they want fine-grained composition.

---

## 10. Runtime Abstraction (Docker + Podman)

### 10.1 Requirements

- Same service definitions must work on Docker and Podman.
- Runtime is selected via config: `[runtime].engine = "docker" | "podman"`.
- Abstract container lifecycle, networking, volumes, logs, and exec operations.
- Provide a stable CLI interface that scripts can call without branching everywhere.

### 10.2 Runtime Interface (Rust-Side)

The orchestrator talks to an abstract runtime interface (trait).

```rust
pub trait ContainerRuntime {
    fn engine_name(&self) -> &'static str;

    fn compose_up(&self, project: &str, compose_files: &[String], env_file: &str) -> Result<()>;
    fn compose_down(&self, project: &str, compose_files: &[String]) -> Result<()>;
    fn compose_ps(&self, project: &str, compose_files: &[String]) -> Result<String>;

    fn exec(&self, container: &str, cmd: &[String]) -> Result<String>;
    fn logs(&self, container: &str, tail: usize) -> Result<String>;

    fn ensure_network(&self, name: &str) -> Result<()>;
    fn ensure_volume(&self, name: &str) -> Result<()>;
}
```

### 10.3 Runtime Adapters (Docker vs Podman)

Docker adapter:

```text
compose_up: docker compose -p <project> --env-file <env> -f <file...> up -d
exec:       docker exec <container> <cmd...>
logs:       docker logs --tail <n> <container>
```

Podman adapter (preferred approach: Podman v4+ with compose support):

```text
compose_up: podman compose -p <project> --env-file <env> -f <file...> up -d
exec:       podman exec <container> <cmd...>
logs:       podman logs --tail <n> <container>
```

Compatibility rule:

- The orchestrator must validate runtime capabilities during `vmctl doctor` and at plan time (dependency checks), and fail with a clear error if the required compose subcommand is unavailable.

### 10.4 Guest-Side Stable Runtime Wrapper

Provision scripts run in guests. They should call a stable wrapper (generated once per resource), not `docker`/`podman` directly:

```bash
#!/usr/bin/env bash
set -euo pipefail

# vmctl-runtime: stable guest entrypoint
# VMCTL_RUNTIME_ENGINE=docker|podman

engine="${VMCTL_RUNTIME_ENGINE:-docker}"
case "${engine}" in
  docker)  exec docker "$@" ;;
  podman)  exec podman "$@" ;;
  *) echo "unsupported engine: ${engine}" >&2; exit 2 ;;
esac
```

Then service scripts always do:

```bash
vmctl-runtime compose -p "$PROJECT" --env-file "$ENV" -f "$COMPOSE" up -d
```

This prevents runtime branching from spreading across service scripts.

### 10.5 Compose Assembly Strategy (Scalable Multi-Service Stacks)

To keep services isolated while still producing a single runnable stack per resource, use a two-stage approach:

1. **Service-owned service specs** (`service.toml`) compile into compose fragments.
2. The orchestrator assembles a final compose config for the resource (or project) deterministically.

Recommended assembly model:

- Each service renders a compose fragment into `generated/<resource>/compose.d/<service>.yml`.
- The orchestrator produces a stable ordered list of compose files for the project:
  - base file first (networks/volumes), then service fragments in topo order.
- Runtime adapters run `compose up` with multiple `-f` arguments in that stable order.

This yields:

- strict ownership: each service owns only its fragment
- deterministic merges: order is defined by the DAG
- deduping: shared networks/volumes are declared once by a base service (example: `container-runtime` or `compose-base`)

Compose portability requirements:

- Use Compose-spec features supported by both Docker Compose and Podman Compose in the supported versions.
- Avoid engine-specific keys inside service fragments; gate them behind service runtime capability checks when unavoidable.

---

## 11. Separation of Concerns (Clean Boundaries)

### 11.1 What Lives Where

- `service.toml`: declarative metadata, dependencies, schema ownership, environment contract, entrypoints, and runtime-neutral container/service definition
- `templates/`: rendering only; no imperative logic
- `scripts/provision.sh`: imperative provisioning logic (idempotent)
- `scripts/validate.sh`: PDV logic; fails clearly and provides actionable diagnostics
- Orchestrator: dependency resolution, execution ordering, caching/fingerprints
- Runtime adapters: engine-specific details (Docker vs Podman)

### 11.2 Avoiding Cross-Service Coupling

Hard rules:

- A service may only depend on another service through declared outputs.
- A service cannot read another service’s files directly.
- Shared scripts/templates must move into a dedicated “shared helper service” (example: `services/runtime/`) or a versioned library folder that services import via manifest, not filesystem reach-through.

---

## 12. Provisioning and PDV (Post-Deploy Validation)

### 12.1 Provisioning Lifecycle

For each service instance, provision is:

1. Render artifacts into `generated/<target>/<service>/...`
2. Upload the service’s rendered directory to the guest
3. Execute `scripts/provision.sh` in the guest
4. Execute `scripts/validate.sh` (PDV) unless `--skip-validate`

### 12.2 Provision Script Contract

`scripts/provision.sh` must:

- be idempotent (re-running should converge)
- only depend on its service env + declared dependency outputs
- use `vmctl-runtime` wrapper for container operations
- write status to stdout in a stable form (human readable + optionally JSON)

Example (provision skeleton):

```bash
#!/usr/bin/env bash
set -euo pipefail

source "./env.sh"          # generated from service.toml environment mapping
source "./lib/runtime.sh"  # service-owned helpers

ensure_dirs "$JELLYFIN_DATA_DIR"
compose_up "media" "./compose.yml" "./.env"
```

### 12.3 Validation Script Contract

`scripts/validate.sh` must:

- validate service reachability (port open, HTTP expected status)
- validate integration edges (example: Sonarr can reach qBittorrent API)
- provide actionable errors (service name, URL, last logs snippet)
- exit non-zero on failure

Example (PDV skeleton):

```bash
#!/usr/bin/env bash
set -euo pipefail

source "./env.sh"
require_http_ok "http://127.0.0.1:${JELLYFIN_HTTP_PORT}${JELLYFIN_BASE_URL}/web/"
```

---

## 13. DRY Strategy (Eliminate Duplication)

### 13.1 Shared Helpers

Create shared helpers as either:

1. a dedicated shared-helper service with versioning (preferred), or
2. a dedicated Rust crate for render-time helpers (Handlebars helpers, schema utilities)

Shared helper examples:

- `http.sh`: retrying curl checks, redirects handling
- `runtime.sh`: compose up/down wrappers, logs helpers
- `arr.sh`: common ARR provisioning patterns (API key creation, download client wiring)
- `config.sh`: idempotent config edits, XML/JSON patch helpers

### 13.2 Shared Config Patterns

Create reusable schema fragments:

- “reverse proxy UI route”
- “compose service baseline”
- “standard media paths”

These are composed into service schemas without copy/paste.

---

## 14. Extraction and Migration Plan (From Current Codebase)

This is an incremental plan that minimizes risk and keeps the system usable while refactoring.

### Step 1: Introduce Service Loader (No Behavior Change)

- Add a service loader that can read `services/*/service.toml`.
- Keep existing `services/resources/` execution as the default path initially.
- Add a “service registry” alongside `ResourceRegistry`, without swapping it in yet.

### Step 2: Define Service Schema and Validation

- Implement TOML schema validation for service manifests and env/service specs.
- Add clear errors for missing scripts/templates and bad dependency references.

### Step 3: Implement Orchestrator DAG (Read-Only)

- Build DAG for enabled service instances.
- Implement:
  - dependency closure
  - dedupe by service instance key
  - deterministic topological sorting
  - cycle detection errors
- Output a “service plan” during `vmctl plan`, even if execution still uses services/resources.

### Step 4: Migrate One Vertical Slice Service (Example: `jellyfin`)

- Create `services/jellyfin/` owning its:
  - env spec
  - compose fragment or service spec
  - templates
  - provision + validate scripts
- Run it via orchestrator behind a feature flag:
  - `vmctl apply --use-services` or config flag.

### Step 5: Refactor Existing Services Into Services

Iterate service-by-service:

- `reverse-proxy` (caddy routing)
- `prowlarr`, `sonarr`, `radarr`, `qbittorrent`, `sabnzbd`
- `flaresolverr`, `jellyseerr`, `streamyfin` or stremio-related components

Each migration removes global references from `services/resources/` and consolidates ownership per service.

### Step 6: Runtime Abstraction Integration

- Introduce runtime selection in config: `[runtime].engine`.
- Implement Docker and Podman adapters.
- Replace direct docker usage in service scripts with `vmctl-runtime` wrapper.

### Step 7: PDV Layer as a First-Class Plan Phase

- Add `vmctl validate` and ensure `vmctl apply` optionally runs PDV.
- Standardize PDV result reporting in the CLI and lockfile.

### Step 8: Shrink `vmctl.toml`

- Move service-specific implementation config into service defaults.
- Keep only:
  - service enablement
  - user-facing service overrides
  - resource definitions and infrastructure concerns

---

## 15. TDD Plan (Dependency Resolution, Dedupe, Execution Order)

### 15.1 Define Expected Behavior (Tests First)

Write failing tests for:

1. dependency resolution:
   - required deps are always included
   - optional deps are included only when enabled
2. deduplication:
   - shared deps are executed once per service instance key
3. execution order:
   - dependencies always appear before dependents
   - ordering is deterministic across runs
4. cycle detection:
   - a cycle produces a clear error listing the cycle path
5. partial selection:
   - selecting a service includes its dependency closure

### 15.2 Example Test Cases

Case: shared dependency dedupe

- Services: `sonarr -> reverse-proxy`, `radarr -> reverse-proxy`
- Expect: `reverse-proxy` appears once in the plan

Case: cycle detection

- Services: `a -> b`, `b -> a`
- Expect: error with `a -> b -> a` cycle string

Case: deterministic order

- Two independent services in same scope: order sorts by key, not by filesystem traversal

### 15.3 Implement Orchestrator After Tests

- Implement loader and DAG builder to satisfy tests.
- Add regression tests as new service patterns emerge (workspace vs resource scoping, multi-instance services).

---

## 16. Definition of Done

This architecture is complete when:

- Services are self-contained and isolated (own env/config, templates, scripts, service spec).
- `vmctl.toml` is minimal and does not contain service implementation details.
- Orchestrator resolves dependencies correctly, dedupes shared services, and orders deterministically.
- Cycles are detected and errors are clear and actionable.
- Docker and Podman are interchangeable by config with identical service definitions.
- No duplicated env/template/script/service logic across services; shared logic is centralized.
- Provisioning is idempotent and repeatable.
- Validation scripts confirm reachability and integration edges, failing clearly on breakage.
