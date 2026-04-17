#!/usr/bin/env bash
set -euo pipefail

# --- edit these ---
TEMPLATE_ID=9000
VMID=210
NAME="media-stack"
TARGET_NODE="pve"
STORAGE="local-lvm"
BRIDGE="vmbr0"

CORES=6
MEMORY=16384
DISK_SIZE="64G"

CIUSER="ubuntu"
SSHKEY_FILE="/root/.ssh/id_ed25519.pub"

IPCONFIG="ip=192.168.1.50/24,gw=192.168.1.1"
NAMESERVER="1.1.1.1"
SEARCHDOMAIN="home.arpa"
# --- end edit ---

if [[ ! -f "$SSHKEY_FILE" ]]; then
  echo "SSH public key not found: $SSHKEY_FILE"
  exit 1
fi

qm destroy "$VMID" --purge 2>/dev/null || true

qm clone "$TEMPLATE_ID" "$VMID" \
  --name "$NAME" \
  --full true \
  --target "$TARGET_NODE"

qm set "$VMID" \
  --cores "$CORES" \
  --memory "$MEMORY" \
  --cpu host \
  --agent enabled=1 \
  --scsihw virtio-scsi-pci \
  --net0 virtio,bridge="$BRIDGE"

qm resize "$VMID" scsi0 "$DISK_SIZE"

qm set "$VMID" \
  --ciuser "$CIUSER" \
  --sshkeys "$SSHKEY_FILE" \
  --ipconfig0 "$IPCONFIG" \
  --nameserver "$NAMESERVER" \
  --searchdomain "$SEARCHDOMAIN"

qm start "$VMID"

echo "VM created and started:"
echo "  VMID: $VMID"
echo "  Name: $NAME"
echo "  IP:   $IPCONFIG"
