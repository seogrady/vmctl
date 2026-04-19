#!/usr/bin/env bash
set -euo pipefail

VMCTL_TAILSCALE_ENABLED=1
if [[ "$VMCTL_TAILSCALE_ENABLED" != "1" ]]; then
  echo "tailscale disabled for this resource"
  exit 0
fi

args=(--reset --auth-key "tskey-fixture")
args+=(--hostname "tailscale-gateway")
args+=(--advertise-routes "192.168.86.0/24")
args+=(--advertise-tags "tag:homelab")

cat >/etc/sysctl.d/99-tailscale-forwarding.conf <<'EOF'
net.ipv4.ip_forward = 1
net.ipv6.conf.all.forwarding = 1
EOF
sysctl -w net.ipv4.ip_forward=1
sysctl -w net.ipv6.conf.all.forwarding=1

tailscale up "${args[@]}"
