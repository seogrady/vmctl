#!/usr/bin/env bash
set -euo pipefail

# --- edit these ---
VMID=210
PCI_DEVICE="00:02.0"
# --- end edit ---

echo "Available VGA/3D devices:"
lspci | grep -Ei 'vga|3d|display' || true
echo

qm set "$VMID" --hostpci0 "$PCI_DEVICE"

echo "Added PCI device $PCI_DEVICE to VM $VMID."
echo
echo "Important:"
echo "- This only updates the VM config."
echo "- Your Proxmox host must already have IOMMU/passthrough configured."
echo "- After booting the VM, check: ls /dev/dri"
