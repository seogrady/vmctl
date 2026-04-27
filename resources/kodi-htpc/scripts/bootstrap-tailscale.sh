#!/usr/bin/env bash
set -euo pipefail

RESOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SETUP_SCRIPT="$RESOURCE_DIR/tailscale-setup.sh"

if [[ ! -f "$SETUP_SCRIPT" ]]; then
  echo "no tailscale setup script found"
  exit 0
fi

if ! grep -q '^VMCTL_TAILSCALE_ENABLED=1$' "$SETUP_SCRIPT"; then
  bash "$SETUP_SCRIPT"
  exit 0
fi

missing=()
command -v curl >/dev/null 2>&1 || missing+=(curl)
command -v python3 >/dev/null 2>&1 || missing+=(python3)
dpkg-query -W -f='${Status}' ca-certificates 2>/dev/null | grep -q 'install ok installed' || missing+=(ca-certificates)
if ((${#missing[@]} > 0)); then
  apt-get update
  apt-get install -y "${missing[@]}"
fi

if ! command -v tailscale >/dev/null 2>&1; then
  curl -fsSL https://tailscale.com/install.sh | sh
fi
systemctl enable --now tailscaled
bash "$SETUP_SCRIPT"
