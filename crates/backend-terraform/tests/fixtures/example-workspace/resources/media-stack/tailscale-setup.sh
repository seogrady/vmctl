#!/usr/bin/env bash
set -euo pipefail

VMCTL_TAILSCALE_ENABLED=1
if [[ "$VMCTL_TAILSCALE_ENABLED" != "1" ]]; then
  echo "tailscale disabled for this resource"
  exit 0
fi

args=(--reset --auth-key "tskey-fixture")
args+=(--hostname "media-stack")
args+=(--advertise-tags "tag:homelab")


tailscale up "${args[@]}"
