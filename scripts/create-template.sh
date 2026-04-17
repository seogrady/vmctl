#!/usr/bin/env bash
set -euo pipefail

# --- edit these if needed ---
TEMPLATE_ID=9000
TEMPLATE_NAME="ubuntu-24-04-cloudinit-template"
STORAGE="local-lvm"
BRIDGE="vmbr0"
IMAGE_URL="https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img"
IMAGE_FILE="ubuntu-24.04-server-cloudimg-amd64.img"
IMAGE_DIR="/var/lib/vz/template/iso"
# --- end edit ---

mkdir -p "$IMAGE_DIR"
cd "$IMAGE_DIR"

if [[ ! -f "$IMAGE_FILE" ]]; then
  wget -O "$IMAGE_FILE" "$IMAGE_URL"
fi

qm destroy "$TEMPLATE_ID" --purge 2>/dev/null || true

qm create "$TEMPLATE_ID" \
  --name "$TEMPLATE_NAME" \
  --memory 2048 \
  --cores 2 \
  --cpu host \
  --net0 virtio,bridge="$BRIDGE"

qm importdisk "$TEMPLATE_ID" "$IMAGE_FILE" "$STORAGE"

qm set "$TEMPLATE_ID" \
  --scsihw virtio-scsi-pci \
  --scsi0 "${STORAGE}:vm-${TEMPLATE_ID}-disk-0"

qm set "$TEMPLATE_ID" --ide2 "${STORAGE}:cloudinit"
qm set "$TEMPLATE_ID" --boot c --bootdisk scsi0
qm set "$TEMPLATE_ID" --serial0 socket --vga serial0
qm set "$TEMPLATE_ID" --agent enabled=1

qm template "$TEMPLATE_ID"

echo "Template created:"
echo "  ID:   $TEMPLATE_ID"
echo "  Name: $TEMPLATE_NAME"
