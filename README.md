# vmctl

`vmctl` is a Rust-first control plane for declarative Proxmox homelab resources.
This repository now includes the initial hybrid architecture implementation from
`plans/vmctl-hybrid-plan-packs.md`: a CLI, TOML interpolation, data-driven role
and service packs, and deterministic backend artifact rendering.

## CLI quick start

```bash
cargo run -q -- --config vmctl.example.toml validate
cargo run -q -- --config vmctl.example.toml plan
cargo run -q -- --config vmctl.example.toml backend render
```

The example config expects these environment variables when validating or
rendering:

```bash
export PROXMOX_TOKEN_ID=...
export PROXMOX_TOKEN_SECRET=...
export TAILSCALE_AUTH_KEY=...
```

Generated backend files are written to `backend/generated/workspace/`.

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
