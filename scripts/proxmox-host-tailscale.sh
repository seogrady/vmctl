#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Install and configure Tailscale on the Proxmox host for host SSH access.

This script is intentionally separate from vmctl resources and from the
tailscale-gateway container. It does not advertise LAN routes, does not enable
exit-node mode, and does not modify vmctl.toml.

Usage:
  scripts/proxmox-host-tailscale.sh [options]

Options:
  --auth-key KEY          Tailscale auth key. Defaults to TAILSCALE_AUTH_KEY.
  --hostname NAME        Tailnet hostname. Defaults to the host's short name.
  --tag TAG              Advertise a tag, for example tag:homelab. May repeat.
  --tailscale-ssh        Enable Tailscale SSH. Default: off.
  --no-up                Install tailscaled but do not run tailscale up.
  --no-reset             Do not pass --reset to tailscale up.
  -h, --help             Show this help.

Examples:
  sudo TAILSCALE_AUTH_KEY=tskey-auth-... scripts/proxmox-host-tailscale.sh \
    --hostname proxmox-mini --tag tag:homelab

  ssh root@proxmox-mini.tailnet-name.ts.net
EOF
}

auth_key="${TAILSCALE_AUTH_KEY:-}"
hostname="$(hostname -s)"
run_up=1
reset=1
enable_tailscale_ssh=0
tags=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --auth-key)
      auth_key="${2:-}"
      shift 2
      ;;
    --hostname)
      hostname="${2:-}"
      shift 2
      ;;
    --tag)
      tags+=("${2:-}")
      shift 2
      ;;
    --tailscale-ssh)
      enable_tailscale_ssh=1
      shift
      ;;
    --no-up)
      run_up=0
      shift
      ;;
    --no-reset)
      reset=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  echo "must run as root on the Proxmox host" >&2
  exit 1
fi

if [[ -z "$hostname" ]]; then
  echo "--hostname cannot be empty" >&2
  exit 1
fi

for tag in "${tags[@]}"; do
  if [[ "$tag" != tag:* ]]; then
    echo "invalid tag '$tag'; Tailscale tags must include the tag: prefix" >&2
    exit 1
  fi
done

if ! command -v curl >/dev/null 2>&1; then
  apt-get update
  apt-get install -y ca-certificates curl
fi

curl -fsSL https://tailscale.com/install.sh | sh
systemctl enable --now tailscaled

if [[ "$run_up" -eq 0 ]]; then
  echo "tailscaled is installed and running; skipped tailscale up"
  exit 0
fi

args=(--accept-dns=false --hostname "$hostname")

if [[ "$reset" -eq 1 ]]; then
  args=(--reset "${args[@]}")
fi

if [[ -n "$auth_key" ]]; then
  args+=(--auth-key "$auth_key")
else
  echo "TAILSCALE_AUTH_KEY/--auth-key not set; tailscale may print a login URL" >&2
fi

if [[ "${#tags[@]}" -gt 0 ]]; then
  IFS=,
  args+=(--advertise-tags "${tags[*]}")
  unset IFS
fi

if [[ "$enable_tailscale_ssh" -eq 1 ]]; then
  args+=(--ssh)
fi

tailscale up "${args[@]}"
tailscale status --self

cat <<EOF

Proxmox host Tailscale setup complete.

For normal OpenSSH over Tailscale:
  ssh root@${hostname}

If MagicDNS is enabled, the FQDN is shown in:
  tailscale status --self
EOF
