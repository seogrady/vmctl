#!/usr/bin/env bash
set -euo pipefail

RESOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$RESOURCE_DIR/kodi.env"

if [[ -f "$ENV_FILE" ]]; then
  set -a
  . "$ENV_FILE"
  set +a
fi

KODI_USER="${KODI_USER:-kodi}"
KODI_HOME="${KODI_HOME:-/home/$KODI_USER}"
KODI_WEB_SKIN="${KODI_WEB_SKIN:-webinterface.default}"
KODI_WEB_PORT="${KODI_WEB_PORT:-8080}"
KODI_EVENT_SERVER_PORT="${KODI_EVENT_SERVER_PORT:-9777}"
KODI_TAILSCALE_HTTPS_ENABLED="${KODI_TAILSCALE_HTTPS_ENABLED:-true}"
KODI_TAILSCALE_HTTPS_TARGET="${KODI_TAILSCALE_HTTPS_TARGET:-http://127.0.0.1:${KODI_WEB_PORT}}"
KODI_CHORUS2_REF="${KODI_CHORUS2_REF:-21.x-1.0.1}"
KODI_MEDIA_EXPORT_PATH="${KODI_MEDIA_EXPORT_PATH:-/data}"
KODI_MEDIA_NFS_HOST="${KODI_MEDIA_NFS_HOST:-media-stack.lan}"

export DEBIAN_FRONTEND=noninteractive
packages=(
  software-properties-common ca-certificates curl xorg xinit openbox dbus-x11 libcap2-bin caddy nfs-common
  pulseaudio alsa-utils avahi-daemon kodi kodi-eventclients-kodi-send cec-utils
)
missing=()
for package in "${packages[@]}"; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed' || missing+=("$package")
done
if ((${#missing[@]} > 0)); then
  apt-get update
  apt-get install -y "${missing[@]}"
fi

if ! id "$KODI_USER" >/dev/null 2>&1; then
  useradd --create-home --home-dir "$KODI_HOME" --shell /bin/bash "$KODI_USER"
fi
usermod -aG audio,video,input,render,dialout "$KODI_USER"

kodi_binary="$(command -v kodi.bin || true)"
if [[ -z "$kodi_binary" ]]; then
  kodi_binary="$(dpkg -L kodi 2>/dev/null | awk '/\/kodi\.bin$/ { print; exit }' || true)"
fi
if [[ -n "$kodi_binary" ]]; then
  setcap 'cap_net_bind_service=+ep' "$kodi_binary" || true
fi

install -d -o "$KODI_USER" -g "$KODI_USER" "$KODI_HOME/.kodi/userdata"
# Clear stale/legacy mount state before enforcing the current media export path.
umount -fl /media >/dev/null 2>&1 || true
install -d /media

opts="ro,vers=4,proto=tcp,_netdev,nofail,x-systemd.automount"
# Remove any legacy media-stack NFS /media entry (including media-stack.lan forms),
# then add a single canonical mount line.
sed -i -E '/^media-stack[^:]*:.+ \/media nfs4 /d' /etc/fstab
echo "${KODI_MEDIA_NFS_HOST}:${KODI_MEDIA_EXPORT_PATH} /media nfs4 ${opts} 0 0" >> /etc/fstab

for _ in {1..60}; do
  if mountpoint -q /media || mount /media; then
    break
  fi
  sleep 2
done

if ! mountpoint -q /media; then
  echo "Kodi media mount failed: /media from ${KODI_MEDIA_NFS_HOST}:${KODI_MEDIA_EXPORT_PATH}"
  exit 1
fi

install_chorus2_webinterface() {
  local addon_dir="/usr/share/kodi/addons/${KODI_WEB_SKIN}"
  local marker="$addon_dir/addon.xml"
  if [[ -f "$marker" ]] && grep -q 'name="Kodi web interface - Chorus2"' "$marker"; then
    return 0
  fi

  local tmp_dir archive_file
  tmp_dir="$(mktemp -d)"
  archive_file="$tmp_dir/chorus2.zip"
  curl -fsSL -H "User-Agent: vmctl" \
    "https://github.com/xbmc/chorus2/archive/refs/tags/${KODI_CHORUS2_REF}.zip" \
    -o "$archive_file"

  rm -rf "$addon_dir"
  install -d "$addon_dir"

  python3 - "$archive_file" "$addon_dir" <<'PY'
from pathlib import Path
import sys
import zipfile

archive = Path(sys.argv[1])
addon_dir = Path(sys.argv[2])

with zipfile.ZipFile(archive) as zf:
    dist_prefix = next(
        name[:-len("dist/addon.xml")]
        for name in zf.namelist()
        if name.endswith("/dist/addon.xml")
    ) + "dist/"

    for name in zf.namelist():
        if not name.startswith(dist_prefix):
            continue
        relative = Path(name[len(dist_prefix):])
        if not relative.parts:
            continue
        destination = addon_dir / relative
        if name.endswith("/"):
            destination.mkdir(parents=True, exist_ok=True)
            continue
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(zf.read(name))
PY

  chown -R root:root "$addon_dir"
  chmod -R a+rX "$addon_dir"
  rm -rf "$tmp_dir"
}

install_chorus2_webinterface

cat > "$KODI_HOME/.kodi/userdata/advancedsettings.xml" <<EOF
<advancedsettings>
  <services>
    <devicename>Kodi HTPC</devicename>
    <esallinterfaces>true</esallinterfaces>
    <escontinuousdelay>25</escontinuousdelay>
    <esenabled>true</esenabled>
    <esinitialdelay>750</esinitialdelay>
    <esmaxclients>20</esmaxclients>
    <esport>${KODI_EVENT_SERVER_PORT}</esport>
    <webserver>true</webserver>
    <webserverallinterfaces>true</webserverallinterfaces>
    <webserverport>${KODI_WEB_PORT}</webserverport>
    <webserverusername></webserverusername>
    <webserverpassword></webserverpassword>
  </services>
</advancedsettings>
EOF
chown "$KODI_USER:$KODI_USER" "$KODI_HOME/.kodi/userdata/advancedsettings.xml"

guisettings="$KODI_HOME/.kodi/userdata/guisettings.xml"
python3 - "$guisettings" "$KODI_WEB_PORT" "$KODI_EVENT_SERVER_PORT" <<'PY'
import os
import sys
import xml.etree.ElementTree as ET

path, web_port, event_port = sys.argv[1:4]
try:
    tree = ET.parse(path)
    root = tree.getroot()
except (FileNotFoundError, ET.ParseError):
    root = ET.Element("settings", {"version": "2"})
    tree = ET.ElementTree(root)

settings = {
    "services.devicename": "Kodi HTPC",
    "services.zeroconf": "true",
    "services.webserver": "true",
    "services.webserverallinterfaces": "true",
    "services.webserverport": web_port,
    "services.webserverauthentication": "false",
    "services.webserverusername": "",
    "services.webserverpassword": "",
    "services.webskin": os.environ.get("KODI_WEB_SKIN", "webinterface.default"),
    "services.esenabled": "true",
    "services.esport": event_port,
    "services.esallinterfaces": "true",
    "services.esinitialdelay": "750",
    "services.escontinuousdelay": "25",
    "services.esmaxclients": "20",
}

existing = {item.get("id"): item for item in root.findall("setting")}
for key, value in settings.items():
    item = existing.get(key)
    if item is None:
        item = ET.SubElement(root, "setting", {"id": key})
    item.attrib.pop("default", None)
    item.text = value

ET.indent(tree, space="    ")
tree.write(path, encoding="unicode", xml_declaration=False)
PY
chown "$KODI_USER:$KODI_USER" "$guisettings"

install -d /etc/caddy
cat > /etc/caddy/Caddyfile <<EOF
:80 {
  encode gzip

  reverse_proxy 127.0.0.1:${KODI_WEB_PORT} {
    header_up Host {host}
    header_up X-Real-IP {remote_host}
  }
}
EOF

cat > /etc/avahi/services/kodi-http.service <<EOF
<?xml version="1.0" standalone='no'?>
<!DOCTYPE service-group SYSTEM "avahi-service.dtd">
<service-group>
  <name replace-wildcards="yes">Kodi HTPC on %h</name>
  <service>
    <type>_http._tcp</type>
    <port>80</port>
  </service>
</service-group>
EOF

cat > /etc/avahi/services/kodi-eventserver.service <<EOF
<?xml version="1.0" standalone='no'?>
<!DOCTYPE service-group SYSTEM "avahi-service.dtd">
<service-group>
  <name replace-wildcards="yes">Kodi Event Server on %h</name>
  <service>
    <type>_xbmc-events._udp</type>
    <port>${KODI_EVENT_SERVER_PORT}</port>
  </service>
</service-group>
EOF
systemctl enable --now avahi-daemon
systemctl enable --now caddy

cat > /usr/local/bin/vmctl-kodi-session <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
xset -dpms || true
xset s off || true
openbox-session &
exec kodi --standalone --fullscreen
EOF
chmod 0755 /usr/local/bin/vmctl-kodi-session

cat > /etc/systemd/system/kodi-htpc.service <<EOF
[Unit]
Description=Kodi HTPC full-screen session
After=systemd-user-sessions.service sound.target network-online.target
Wants=network-online.target

[Service]
User=${KODI_USER}
Group=${KODI_USER}
SupplementaryGroups=audio video input render dialout
Environment=HOME=${KODI_HOME}
WorkingDirectory=${KODI_HOME}
TTYPath=/dev/tty7
TTYReset=yes
TTYVHangup=yes
StandardInput=tty
StandardOutput=journal
StandardError=journal
ExecStart=/usr/bin/xinit /usr/local/bin/vmctl-kodi-session -- :0 vt7 -nolisten tcp
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable kodi-htpc.service
systemctl restart kodi-htpc.service
systemctl restart caddy

for _ in {1..60}; do
  if curl -fsS "http://127.0.0.1:${KODI_WEB_PORT}/jsonrpc" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"JSONRPC.Ping","id":1}' >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

for _ in {1..60}; do
  if curl -fsS "http://127.0.0.1/" | grep -q 'Chorus 2 - Kodi web interface'; then
    break
  fi
  sleep 2
done

for _ in {1..60}; do
  if curl -fsS "http://127.0.0.1/js/kodi-webinterface.js" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

if ! curl -fsS "http://127.0.0.1/" | grep -q 'Chorus 2 - Kodi web interface'; then
  echo "Kodi web UI check failed: expected Chorus2 title"
  exit 1
fi

if ! curl -fsS "http://127.0.0.1/js/kodi-webinterface.js" >/dev/null 2>&1; then
  echo "Kodi web UI asset check failed: /js/kodi-webinterface.js"
  exit 1
fi

if [[ "${KODI_TAILSCALE_HTTPS_ENABLED,,}" == "false" || "${KODI_TAILSCALE_HTTPS_ENABLED}" == "0" ]]; then
  if command -v tailscale >/dev/null 2>&1; then
    tailscale serve reset >/dev/null 2>&1 || true
  fi
  exit 0
fi

if ! command -v tailscale >/dev/null 2>&1; then
  echo "tailscale not installed; skipping Kodi tailnet HTTPS exposure"
  exit 0
fi

if ! tailscale status --json >/tmp/vmctl-tailscale-status.json 2>/dev/null; then
  echo "tailscale is not authenticated; skipping Kodi tailnet HTTPS exposure"
  exit 0
fi

tailscale_ready="$(python3 <<'PY'
import json
try:
    with open("/tmp/vmctl-tailscale-status.json", encoding="utf-8") as handle:
        status = json.load(handle)
    print(1 if status.get("BackendState") in {"Running", "Starting"} else 0)
except Exception:
    print(0)
PY
)"
if [[ "$tailscale_ready" != "1" ]]; then
  echo "tailscale backend is not running; skipping Kodi tailnet HTTPS exposure"
  exit 0
fi

tailscale serve --yes --bg "$KODI_TAILSCALE_HTTPS_TARGET"
