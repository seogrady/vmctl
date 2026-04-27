#!/usr/bin/env bash
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

. /etc/os-release

missing=()
for package in ca-certificates curl cloud-guest-utils; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed' || missing+=("$package")
done

if [[ "${ID:-}" == "ubuntu" ]]; then
  kernel_extra_package="linux-modules-extra-$(uname -r)"
  dpkg-query -W -f='${Status}' "$kernel_extra_package" 2>/dev/null | grep -q 'install ok installed' || missing+=("$kernel_extra_package")
fi

if [[ -e /dev/virtio-ports/org.qemu.guest_agent.0 ]]; then
  dpkg-query -W -f='${Status}' qemu-guest-agent 2>/dev/null | grep -q 'install ok installed' || missing+=(qemu-guest-agent)
fi

if ((${#missing[@]} > 0)); then
  apt-get update
  apt-get install -y "${missing[@]}"
fi

if [[ "${ID:-}" == "ubuntu" ]]; then
  systemctl enable --now ssh
fi

resize_root_filesystem() {
  local root_source root_fstype root_disk root_part_num

  root_source="$(findmnt -no SOURCE /)"
  root_fstype="$(findmnt -no FSTYPE /)"
  [[ "$root_source" == /dev/* ]] || return 0
  [[ "$root_source" == /dev/mapper/* ]] && return 0

  if [[ "$root_source" =~ ^(/dev/.+?)(p?)([0-9]+)$ ]]; then
    root_disk="${BASH_REMATCH[1]}"
    root_part_num="${BASH_REMATCH[3]}"
  else
    return 0
  fi

  echo "resizing root filesystem on $root_source from $root_disk partition $root_part_num"
  growpart "$root_disk" "$root_part_num" || true

  case "$root_fstype" in
    ext2|ext3|ext4)
      resize2fs "$root_source" || true
      ;;
    xfs)
      xfs_growfs / || true
      ;;
  esac
}

resize_root_filesystem

if [[ -e /dev/virtio-ports/org.qemu.guest_agent.0 ]]; then
  systemctl start qemu-guest-agent
fi
