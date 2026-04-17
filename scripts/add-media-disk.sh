#!/usr/bin/env bash
set -euo pipefail

# --- edit these ---
VMID=210
STORAGE="local-lvm"
DISK_SIZE_GB=500
SLOT="scsi1"
# --- end edit ---

qm set "$VMID" --"$SLOT" "${STORAGE}:${DISK_SIZE_GB}"

echo "Added disk to VM $VMID:"
echo "  Slot:    $SLOT"
echo "  Storage: $STORAGE"
echo "  Size:    ${DISK_SIZE_GB}G"
echo
echo "Next: partition and mount it inside the guest."
