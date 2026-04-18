#!/usr/bin/env bash
set -euo pipefail

# --- edit these ---
TEMPLATE_ID=9000
VMID=210
NAME="media-stack"
TARGET_NODE="$(hostname)"
STORAGE="local-lvm"
BRIDGE="vmbr0"

CORES=6
MEMORY=16384
DISK_SIZE="64G"

CIUSER="ubuntu"
SSHKEY_FILE="/root/.ssh/media_stack.pub"

# Preferred: use DHCP, then set a DHCP reservation in your router
# for the VM MAC address shown in `qm config $VMID`
IPCONFIG="ip=dhcp"

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
  --machine q35 \
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
echo "  Node: $TARGET_NODE"
echo "  Network: DHCP"
echo
echo "Next steps:"
echo "  1. Find the IP from your router/DHCP leases or install qemu-guest-agent in the VM."
echo "  2. Set a DHCP reservation in your router for this VM's MAC address."
echo "  3. SSH using:"
echo "     ssh -i ${SSHKEY_FILE%.pub} ${CIUSER}@<vm-ip>"