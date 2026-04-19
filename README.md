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
and you want to skip host-side image checks. By default, `apply` keeps
OpenTofu/Terraform output concise; use `vmctl apply --verbose` when you want the
full provider plan and apply log in the console.

`vmctl apply` exits when the full workflow finishes. Interactive terminals show
spinner progress for long-running phases such as OpenTofu apply and post-boot
SSH provisioning. Provisioning progress includes the resource name, script name,
and retry attempt. When a script finishes inside a resource, vmctl prints a
persistent status line such as `ran bootstrap-media.sh on media-stack`; when all
scripts for that resource finish, it prints `provisioned media-stack`. A
successful run ends with:

```text
vmctl apply complete
```

If a previous apply was interrupted, `vmctl apply` tries to recover before it
creates resources. When a configured VMID/CTID already exists in Proxmox but is
missing from OpenTofu state, vmctl first runs a non-destructive `tofu import`
using the generated module address and `<node>/<vmid>` import ID. If import
fails, vmctl asks whether to destroy the existing Proxmox resource and continue.
Answer `n` to leave the resource untouched and print manual import/remove
commands.

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
   workspace, imports recoverable existing VMIDs/CTIDs into state, and runs
   `tofu apply` by default. If `tofu` is unavailable, `terraform` is accepted as
   a compatibility fallback. This requires reachable Proxmox and
   `TF_VAR_proxmox_api_token`.
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

### iGPU Passthrough

iGPU passthrough is useful for media servers because Jellyfin and related
services can use hardware transcoding through Intel Quick Sync. It requires
firmware support, Proxmox host support, and a safe way for vmctl to attach the
device to the VM.

1. Enable firmware settings in BIOS/UEFI.

   The exact names vary by motherboard, but look for:

   - Intel VT-d
   - IOMMU
   - Intel Virtualization Technology for Directed I/O
   - Above 4G Decoding, if available
   - SR-IOV, if available

   Save settings and boot back into Proxmox.

2. Verify the Proxmox host sees the iGPU.

   On the Proxmox host:

   ```bash
   lspci -nn | grep -Ei 'vga|display|intel'
   ```

   The example config assumes the iGPU is:

   ```text
   00:02.0
   ```

   Use the PCI address from your host if it differs.

3. Verify IOMMU is active.

   Run:

   ```bash
   find /sys/kernel/iommu_groups -type l | head
   ```

   If that prints nothing, firmware or kernel-side IOMMU is not active. vmctl
   will detect this, but it will not change BIOS settings or bootloader
   arguments automatically because those changes can take the host offline.

4. Prefer a Proxmox PCI resource mapping.

   Raw PCI passthrough, such as `hostpci0: 00:02.0`, can only be configured by
   `root@pam`. That is why an API-token apply can fail with:

   ```text
   only root can set 'hostpci0' config for non-mapped devices
   ```

   A Proxmox PCI resource mapping gives the device a stable logical name, such
   as `intel-igpu`. vmctl can then reference the mapping instead of sending the
   raw PCI address in VM config. This is better for API-token workflows, easier
   to audit, and safer if the cluster grows later.

   Configure vmctl with both the mapping name and the physical PCI device:

   ```toml
   [resources.features.intel_igpu]
   enabled = true
   mapping = "intel-igpu"
   pci_device = "00:02.0"
   ```

   `pci_device` is used by `vmctl passthrough prepare` to create the Proxmox
   mapping. The generated VM hardware uses `mapping`, not raw hostpci, when
   `mapping` is set. VMs with `features.intel_igpu.enabled = true` also default
   to `machine = "q35"`, which Proxmox requires for PCIe passthrough. Set a
   resource-level `machine` only if you need to override that default.

5. Run vmctl passthrough checks.

   Passthrough checks run automatically during `vmctl apply` before OpenTofu
   applies changes. You can run them directly with:

   ```bash
   vmctl passthrough doctor
   ```

   `doctor` checks whether passthrough is enabled in config, whether IOMMU
   groups exist, whether the configured Proxmox PCI mapping exists, and whether
   the mapping includes the expected `iommugroup` and `subsystem-id` for the
   physical device.

6. Create the Proxmox PCI mapping from config.

   Preview first:

   ```bash
   vmctl passthrough prepare --dry-run
   ```

   Then create it:

   ```bash
   vmctl passthrough prepare
   ```

   `prepare` resolves the vendor/device ID and subsystem ID with `lspci`, reads
   the device IOMMU group from `/sys/bus/pci/devices/<device>/iommu_group`, and
   creates or updates a mapping similar to:

   ```bash
   pvesh create /cluster/mapping/pci --id intel-igpu \
     --map node=mini,path=0000:00:02.0,id=8086:46a6,iommugroup=0,subsystem-id=8086:7270
   ```

   `vmctl apply` also runs this preparation step automatically for enabled
   passthrough resources that include both `mapping` and `pci_device`.

   If you create the mapping manually instead, use the Proxmox UI:
   `Datacenter` -> `Resource Mappings` -> `PCI Devices` -> `Add`.

7. Grant the API token mapping permission.

   The token used by vmctl needs permission to use the mapping:

   ```text
   Mapping.Use on /mapping/pci/intel-igpu
   ```

   It also needs the normal VM configuration permissions used by vmctl.

8. Apply the config.

   ```bash
   vmctl apply
   ```

Using `root@pam` for passthrough is possible, but it is not the recommended
default for vmctl automation. `root@pam` can configure raw host PCI devices and
is useful for one-off manual recovery or debugging, but it gives the automation
full host control. A dedicated API token plus a PCI resource mapping limits the
dangerous part to a named mapping and keeps vmctl's normal apply path more
auditable.

Use raw `pci_device = "00:02.0"` without `mapping` only when applying through an
interactive `root@pam` session and you intentionally want raw host PCI access.

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
invokes the CLI and talks directly to the configured Proxmox API endpoint. Host
side operations such as image ensure, passthrough checks, and interrupted-apply
recovery also use local Proxmox commands such as `pveam`, `qm`, `pct`, `pvesh`,
and `lspci`. The generated OpenTofu/Terraform workspace is useful for
inspection or manual execution elsewhere, but artifact-copy deployment is not
yet a first-class workflow.

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
