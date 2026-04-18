# vmctl

`vmctl` is a Rust-first control plane for declarative Proxmox homelab resources.
This repository now includes the initial hybrid architecture implementation from
`plans/vmctl-hybrid-plan-packs.md`: a CLI, TOML interpolation, data-driven role
and service packs, and deterministic backend artifact rendering.

## CLI quick start

Install the host CLI from this checkout:

```bash
cargo install --path crates/cli --locked
```

Cargo installs the binary as `vmctl` in `~/.cargo/bin` by default. Ensure that
directory is on your `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
vmctl --help
```

To upgrade after pulling new changes, run the same install command again with
`--force`:

```bash
cargo install --path crates/cli --locked --force
```

To remove the host binary:

```bash
cargo uninstall vmctl
```

After installation, run commands from a vmctl workspace that contains
`vmctl.toml` or `vmctl.example.toml` and `packs/`:

```bash
vmctl --config vmctl.example.toml validate
vmctl --config vmctl.example.toml plan
```

During development, you can still run the CLI directly through Cargo:

```bash
cargo run -q -p vmctl -- --config vmctl.example.toml validate
cargo run -q -p vmctl -- --config vmctl.example.toml plan
cargo run -q -p vmctl -- --config vmctl.example.toml backend validate
cargo run -q -p vmctl -- --config vmctl.example.toml backend validate --live
cargo run -q -p vmctl -- --config vmctl.example.toml backend plan --dry-run
cargo run -q -p vmctl -- --config vmctl.example.toml backend render
cargo run -q -p vmctl -- --config vmctl.example.toml provision
cargo run -q -p vmctl -- --config vmctl.example.toml backend show-state
```

The example config expects these environment variables when validating or
rendering:

```bash
export PROXMOX_TOKEN_ID=...
export PROXMOX_TOKEN_SECRET=...
export TAILSCALE_AUTH_KEY=...
export DEFAULT_SSH_KEY_FILE="$HOME/.ssh/id_ed25519.pub"
export DEFAULT_SSH_PRIVATE_KEY_FILE="$HOME/.ssh/id_ed25519"
export TF_VAR_proxmox_api_token="${PROXMOX_TOKEN_ID}=${PROXMOX_TOKEN_SECRET}"
```

Create a Proxmox API token in the Proxmox web UI:

1. Open `Datacenter` -> `Permissions` -> `API Tokens`.
2. Click `Add`.
3. Choose a user such as `root@pam`, or a dedicated user such as `vmctl@pve`.
4. Enter a token ID such as `vmctl`.
5. Save the token secret when Proxmox shows it. The secret is shown only once.

You can also create a token from a Proxmox node shell with `pveum`:

```bash
pveum user token add root@pam vmctl --privsep 0
```

That command creates the token `root@pam!vmctl`. The command output includes a
secret value; save it immediately because Proxmox only shows token secrets when
they are created.

For a dedicated user, create the user, grant it appropriate permissions, then
create a token for that user:

```bash
pveum user add vmctl@pve --comment "vmctl automation"
pveum aclmod / -user vmctl@pve -role Administrator
pveum user token add vmctl@pve automation --privsep 0
```

This creates the token `vmctl@pve!automation`. `--privsep 0` means the token
inherits the user's permissions. If you use privilege-separated tokens, grant
permissions to the token itself according to your Proxmox policy.

`PROXMOX_TOKEN_ID` uses this format:

```text
USER@REALM!TOKEN_NAME
```

For example:

```bash
export PROXMOX_TOKEN_ID="root@pam!vmctl"
export PROXMOX_TOKEN_SECRET="your-token-secret"
```

Prefer a dedicated Proxmox user and token for normal operation. The token needs
permission to manage the target VM/LXC resources, storage, networking, and any
clone or template resources used by the config.

`TF_VAR_proxmox_api_token` is required by OpenTofu/Terraform. OpenTofu and
Terraform map environment variables named `TF_VAR_<terraform_variable_name>`
into input variables. The generated provider config defines a variable named
`proxmox_api_token`, so the matching environment variable is:

```bash
export TF_VAR_proxmox_api_token="${PROXMOX_TOKEN_ID}=${PROXMOX_TOKEN_SECRET}"
```

The lowercase suffix is intentional because it matches the Terraform variable
name exactly. The combined value has this shape:

```text
USER@REALM!TOKEN_NAME=SECRET
```

`vmctl` keeps `PROXMOX_TOKEN_ID` and `PROXMOX_TOKEN_SECRET` separate for config
resolution, while OpenTofu/Terraform receives the combined token through
`TF_VAR_proxmox_api_token` so the secret is not written into generated
Terraform JSON.

SSH key settings are file paths only. Public keys use `ssh_key_file`, private
keys use `private_key_file`, and the `_file` suffix is intentional so config
readers can tell the value is a path rather than inline key material.

By default the CLI loads `vmctl.toml`. If `vmctl.toml` is missing, it falls back
to `vmctl.example.toml` so a fresh checkout can still validate and render. Use
`--config <path>` for an explicit config file. `vmctl.example.toml` is a
reference file; copy it to `vmctl.toml` for local overrides.

Generated backend files are written to `backend/generated/workspace/`, and
`vmctl.lock` is written at the workspace root.

Recommended workflow:

1. `vmctl validate` parses config, resolves interpolation/defaults, and expands
   packs.
2. `vmctl plan` prints the high-level domain plan.
3. `vmctl backend validate` renders a provider-free validation workspace and
   runs `tofu init` + `tofu validate`.
4. `vmctl backend validate --live` renders the provider-backed workspace and
   runs `tofu init` + `tofu validate` without planning or applying.
5. `vmctl backend plan --dry-run` additionally runs `tofu plan -refresh=false`
   against a provider-free workspace without contacting Proxmox. This verifies
   the generated OpenTofu graph and prints the plan body, but it is not a live
   Proxmox change preview. It may still use network access to install OpenTofu
   providers or modules if they are not already cached.
6. `vmctl backend render` writes the live OpenTofu/Terraform workspace.
7. `vmctl apply --auto-approve` renders the live workspace and runs
   `tofu apply` by default. If `tofu` is unavailable, `terraform` is accepted as
   a compatibility fallback. This requires reachable Proxmox and
   `TF_VAR_proxmox_api_token`.
8. `vmctl provision` uploads and executes pack-generated bootstrap scripts over
   SSH using each resource's `[resources.provision]` settings.

The default backend is `tofu`, with `terraform` still accepted as a config
compatibility alias for the same renderer. The current OpenTofu/Terraform
backend generates deterministic scaffold modules under
`backend/generated/workspace/modules/` and maps each `vmctl` resource to a
backend module with `depends_on` preserved from the domain model. It also emits
`provider.tf.json` for the `bpg/proxmox` provider from `[backend.proxmox]`,
threads resolved node, bridge, storage, and template values into each module,
and emits `proxmox_virtual_environment_vm` / `proxmox_virtual_environment_container`
resources. Secrets are redacted from generated debug JSON; OpenTofu/Terraform
receives the Proxmox token via the sensitive `TF_VAR_proxmox_api_token`
variable.
`vmctl.lock` stores resource digests and generated artifact digests, excluding
secret-valued fields from resource digests.

LXC feature flags are explicit because Proxmox restricts most container feature
changes to `root@pam`. The example enables only nesting for the Tailscale
gateway:

```toml
[resources.features.lxc]
nesting = true
```

Do not enable other LXC feature flags unless the Proxmox user/token has the
required privilege for that operation.

Current deployment assumption: `vmctl apply` runs on the local machine that
invokes the CLI and talks directly to the configured Proxmox API endpoint. The
generated OpenTofu/Terraform workspace is also useful for inspection or manual
execution elsewhere, but artifact-copy deployment is not yet a first-class
workflow.

Live operations require explicit approval at the `vmctl` layer. `vmctl apply`
and `vmctl destroy` fail unless `--auto-approve` is supplied, and the live
renderer checks for the Proxmox endpoint, node, VMID, bridge, storage, template,
and VM clone VMID before it writes provider-backed artifacts. Dependency checks
are command scoped: OpenTofu/Terraform is required only for backend commands
that run the backend, while SSH/SCP are required only for provisioning.

Provisioning is pack driven. OpenTofu/Terraform creates VM/LXC resources and
cloud-init handles first boot identity. Post-boot, `vmctl provision` uploads
scripts from `backend/generated/workspace/resources/<name>/scripts/` and runs
them with SSH.
Provisioning supports retries, logs failed attempts, and uses generated scripts
from role packs.

## Workspace crates

The implementation follows the crate layout in `plans/vmctl-hybrid-plan-packs.md`:

- `crates/cli/` owns clap parsing, command dispatch, and terminal output.
- `crates/config/`, `crates/domain/`, `crates/planner/`, `crates/packs/`, and
  `crates/dependencies/` own config loading, backend-agnostic models,
  desired-state construction, pack expansion, and command-scoped dependency
  checks.
- `crates/backend/`, `crates/backend-terraform/`, and `crates/backend-native/`
  define the backend interface, the OpenTofu/Terraform renderer, and the future
  native engine placeholder.
- `crates/lockfile/`, `crates/import/`, `crates/provision/`, `crates/render/`,
  and `crates/util/` own lockfile persistence, import/reconciliation,
  SSH-based provisioning, human-facing rendering, and shared helpers.

## Pack layout

- `packs/roles/` contains declarative role packs such as `media_stack` and
  `tailscale_gateway`.
- `packs/services/` contains service packs that can be referenced by role packs.
- `packs/templates/` and `packs/scripts/` contain backend-independent render and
  bootstrap assets.
