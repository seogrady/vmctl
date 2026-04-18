# vmctl

`vmctl` is a Rust-first control plane for declarative Proxmox homelab resources.
This repository now includes the initial hybrid architecture implementation from
`plans/vmctl-hybrid-plan-packs.md`: a CLI, TOML interpolation, data-driven role
and service packs, and deterministic backend artifact rendering.

## CLI quick start

```bash
cargo run -q -p vmctl -- --config vmctl.example.toml validate
cargo run -q -p vmctl -- --config vmctl.example.toml plan
cargo run -q -p vmctl -- --config vmctl.example.toml backend validate
cargo run -q -p vmctl -- --config vmctl.example.toml backend validate --live
cargo run -q -p vmctl -- --config vmctl.example.toml backend plan --dry-run
cargo run -q -p vmctl -- --config vmctl.example.toml backend render
cargo run -q -p vmctl -- --config vmctl.example.toml backend show-state
```

The example config expects these environment variables when validating or
rendering:

```bash
export PROXMOX_TOKEN_ID=...
export PROXMOX_TOKEN_SECRET=...
export TAILSCALE_AUTH_KEY=...
export TF_VAR_proxmox_api_token="${PROXMOX_TOKEN_ID}=${PROXMOX_TOKEN_SECRET}"
```

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
   without contacting Proxmox. It may still use network access to install
   OpenTofu providers or modules if they are not already cached.
6. `vmctl backend render` writes the live Terraform/OpenTofu workspace.
7. `vmctl apply --auto-approve` renders the live workspace and runs
   `tofu apply` or `terraform apply`; this requires reachable Proxmox and
   `TF_VAR_proxmox_api_token`.

The current Terraform backend generates deterministic scaffold modules under
`backend/generated/workspace/modules/` and maps each `vmctl` resource to a
backend module with `depends_on` preserved from the domain model. It also emits
`provider.tf.json` for the `bpg/proxmox` provider from `[backend.proxmox]`,
threads resolved node, bridge, storage, and template values into each module,
and emits `proxmox_virtual_environment_vm` / `proxmox_virtual_environment_container`
resources. Secrets are redacted from generated debug JSON; Terraform receives
the Proxmox token via the sensitive `TF_VAR_proxmox_api_token` variable.
`vmctl.lock` stores resource digests and generated artifact digests, excluding
secret-valued fields from resource digests.

Current deployment assumption: `vmctl apply` runs on the local machine that
invokes the CLI and talks directly to the configured Proxmox API endpoint. The
generated Terraform/OpenTofu workspace is also useful for inspection or manual
execution elsewhere, but artifact-copy deployment is not yet a first-class
workflow.

Live operations require explicit approval at the `vmctl` layer. `vmctl apply`
and `vmctl destroy` fail unless `--auto-approve` is supplied, and the live
renderer checks for the Proxmox endpoint, node, VMID, bridge, storage, template,
and VM clone VMID before it writes provider-backed artifacts.

## Workspace crates

The implementation follows the crate layout in `plans/vmctl-hybrid-plan-packs.md`:

- `crates/cli/` owns clap parsing, command dispatch, and terminal output.
- `crates/config/`, `crates/domain/`, `crates/planner/`, and `crates/packs/`
  own config loading, backend-agnostic models, desired-state construction, and
  pack expansion.
- `crates/backend/`, `crates/backend-terraform/`, and `crates/backend-native/`
  define the backend interface, the Terraform renderer, and the future native
  engine placeholder.
- `crates/lockfile/`, `crates/import/`, `crates/render/`, and `crates/util/`
  own lockfile persistence, import placeholders, human-facing rendering, and
  shared helpers.

## Pack layout

- `packs/roles/` contains declarative role packs such as `media_stack` and
  `tailscale_gateway`.
- `packs/services/` contains service packs that can be referenced by role packs.
- `packs/templates/` and `packs/scripts/` contain backend-independent render and
  bootstrap assets.

## Legacy Proxmox scripts

Reusable shell scripts for creating an Ubuntu cloud-init template and cloning a media VM from it on a Proxmox host.

These scripts are intended to be run on the Proxmox host as root.

## Included

- `scripts/create-template.sh`
- `scripts/create-media-vm.sh`
- `scripts/add-media-disk.sh`
- `scripts/add-igpu-passthrough.sh`

## Recommended location on the Proxmox host

```bash
mkdir -p /root/scripts
cd /root/scripts
```

## Quick start

### 1. Unzip and make scripts executable

```bash
unzip proxmox-vm-scripts-repo.zip
cd proxmox-vm-scripts-repo
chmod +x scripts/*.sh
```

### 2. Create the Ubuntu cloud-init template

Edit `scripts/create-template.sh` if needed, then run:

```bash
./scripts/create-template.sh
```

### 3. Create your media VM

Edit `scripts/create-media-vm.sh`, then run:

```bash
./scripts/create-media-vm.sh
```

### 4. Optional: add a second disk

```bash
./scripts/add-media-disk.sh
```

### 5. Optional: add Intel iGPU passthrough

```bash
./scripts/add-igpu-passthrough.sh
```

## Typical defaults for a Jellyfin + Docker VM

- Ubuntu Server 24.04 cloud image
- 6 vCPU
- 16 GB RAM
- 64 GB OS disk
- VirtIO NIC on `vmbr0`
- optional second disk for media/downloads
- optional Intel iGPU passthrough for `/dev/dri`

## After the VM boots

SSH into the guest and install Docker:

```bash
sudo apt update
sudo apt install -y docker.io docker-compose-plugin
sudo systemctl enable docker
sudo usermod -aG docker $USER
```

Log out and back in after adding your user to the `docker` group.

## Jellyfin hardware transcoding

If passthrough works, inside the VM you should see:

```bash
ls /dev/dri
```

Expected:

```text
card0
renderD128
```

Then your Jellyfin container can mount:

```yaml
devices:
  - /dev/dri:/dev/dri
```

## Notes

- These scripts assume your Proxmox host already has a working storage backend like `local-lvm` and a bridge like `vmbr0`.
- `add-igpu-passthrough.sh` only adds the PCI device to the VM config. IOMMU and passthrough still need to be enabled on the Proxmox host.
- These scripts are designed for VMs, not LXCs.
