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

For the normal path, edit `vmctl.toml` and run one command:

```bash
vmctl apply
```

or:

```bash
vmctl up
```

`apply` and `up` run the required operational pipeline: config resolution and
validation, command-scoped dependency checks, image/template ensure, live
OpenTofu/Terraform render validation, deployment, lockfile update, and
post-boot provisioning. Use `--skip-provision` only when you want to create or
update Proxmox resources without running pack bootstrap scripts. Use
`--no-image-ensure` only when the required Proxmox templates are known to exist
and you want to skip host-side image checks.

The lower-level commands are useful for inspection and troubleshooting:

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
6. `vmctl images plan` prints the image/template actions needed by resources.
7. `vmctl images ensure` downloads missing `pveam` LXC templates and validates
   `existing` VM/LXC images before resource creation.
8. `vmctl backend render` writes the live OpenTofu/Terraform workspace.
9. `vmctl apply` ensures required images, validates the live rendered
   workspace, and runs `tofu apply` by default. If `tofu` is unavailable,
   `terraform` is accepted as a compatibility fallback. This requires reachable
   Proxmox and `TF_VAR_proxmox_api_token`.
10. `vmctl provision` uploads and executes pack-generated bootstrap scripts over
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

The generated provider constraint currently pins `bpg/proxmox` below `0.98.1`.
That release deprecated `network_device.enabled`, but the provider schema still
requires the attribute for configured VM network devices. The pin keeps plans
valid and quiet until the provider supports network devices without that field.

Proxmox VM/LXC base images are declared once in `[images]` and referenced by
logical name from resources. For Proxmox appliance templates,
`vmctl images ensure` uses `pveam` on the host to download the template only
when it is missing:

```toml
[images.debian_12_lxc]
kind = "lxc"
source = "pveam"
node = "mini"
storage = "local"
content_type = "vztmpl"
template = "debian-12-standard_12.12-1_amd64.tar.zst"
file_name = "debian-12-standard_12.12-1_amd64.tar.zst"

[[resources]]
name = "tailscale-gateway"
kind = "lxc"
image = "debian_12_lxc"
```

URL images use provider-managed `proxmox_virtual_environment_download_file`
resources in the generated OpenTofu workspace. Existing LXC volumes are
validated with Proxmox storage metadata before apply. Existing VM templates are
validated by VMID with `qm status`:

```toml
[images.ubuntu_24_cloudinit_template]
kind = "vm"
source = "existing"
node = "mini"
storage = "local-lvm"
content_type = "vm-template"
file_name = "ubuntu-24-04-cloudinit-template"
vmid = 9000

[[resources]]
name = "media-stack"
kind = "vm"
image = "ubuntu_24_cloudinit_template"
```

Direct `template` values still work as a compatibility escape hatch.

Docker service images are separate from Proxmox base images. Entries in
`resources.features.media_services.services` reference service packs under
`packs/services/`; those packs define container images such as
`lscr.io/linuxserver/jellyfin:latest`. The media role renders
`docker-compose.media` and `media.env`, then `bootstrap-media.sh` uploads those
artifacts to the guest, copies them to `/opt/media`, runs
`docker compose pull`, and starts the stack with `docker compose up -d`.

Hostnames are config driven. Set `defaults.hostnames = true` to assign each
resource a local hostname using its resource name. A resource can override that
with `hostname = "my-host"`, force the default with `hostname = true`, or opt
out with `hostname = false`. When a resource has a hostname and a
`searchdomain`, vmctl derives a default provisioning host such as
`media.home.arpa` unless `[resources.provision].host` is set explicitly:

```toml
[defaults]
searchdomain = "home.arpa"
hostnames = true

[[resources]]
name = "media-stack"
hostname = "media"
```

Tailscale does not automatically create public internet URLs for every
resource. There are three separate access modes:

- Subnet router: `tailscale-gateway` advertises LAN routes such as
  `192.168.86.0/24`. Tailnet clients can reach private LAN addresses after the
  route is approved in Tailscale. This is the default gateway model.
- Per-resource client: a resource with `features.tailscale.enabled = true`
  joins the tailnet and gets a Tailscale/MagicDNS name based on its configured
  hostname. The media role runs this setup when enabled.
- Public internet access: expose only selected services through a deliberate
  public ingress, such as Tailscale Funnel or a reverse proxy. vmctl should not
  publish every resource by default.

The Tailscale gateway should not be an exit node by default. Exit nodes route a
client's general internet traffic through your homelab; they are useful for
egress, not required for accessing homelab services. Enable
`features.tailscale.exit_node = true` only when that is the intended behavior.

iGPU passthrough is disabled in the example because raw PCI devices such as
`00:02.0` can only be assigned by `root@pam`. When using a Proxmox API token,
create a Proxmox PCI resource mapping and configure that mapping instead:

```toml
[resources.features.intel_igpu]
enabled = true
mapping = "intel-igpu"
pci_device = "00:02.0"
```

The `pci_device` value is used by `vmctl passthrough prepare` to create the
mapping. The generated VM hardware uses the mapping name, not raw hostpci, when
`mapping` is set.

Passthrough checks run automatically during `vmctl apply` before OpenTofu
applies changes. You can run them directly with:

```bash
vmctl passthrough doctor
```

To create missing Proxmox PCI resource mappings from config:

```bash
vmctl passthrough prepare --dry-run
vmctl passthrough prepare
```

`prepare` can create Proxmox PCI mappings with `pvesh`, but it will not change
BIOS settings or kernel boot arguments. If IOMMU groups are missing under
`/sys/kernel/iommu_groups`, enable VT-d/IOMMU in BIOS and configure the Proxmox
kernel for IOMMU, then reboot.

Use raw `pci_device = "00:02.0"` without `mapping` only when applying as an
interactive `root@pam` session and you intend to grant raw host PCI access.

Image commands:

```bash
vmctl images list
vmctl images plan
vmctl images ensure --dry-run
vmctl images ensure debian_12_lxc
vmctl up --auto-approve
vmctl up --auto-approve --no-image-ensure
```

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

Destructive destroy operations require explicit approval at the `vmctl` layer:
`vmctl destroy` fails unless `--auto-approve` is supplied. `vmctl apply` and
`vmctl up` are intended to be the normal one-command deployment path. The live
renderer checks for the Proxmox endpoint, node, VMID, bridge, storage,
template/image, and VM clone VMID before it writes provider-backed artifacts.
Dependency checks are command scoped: OpenTofu/Terraform is required only for
backend commands that run the backend, while SSH/SCP are required only for
provisioning.

Provisioning is pack driven. OpenTofu/Terraform creates VM/LXC resources and
cloud-init handles first boot identity. Post-boot, `vmctl provision` uploads
the full generated resource directory from
`backend/generated/workspace/resources/<name>/` and runs bootstrap scripts from
that directory with SSH.
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
