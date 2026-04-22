#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"

if [[ ! -f "$ENV_FILE" ]]; then
  exit 0
fi

set -a
. "$ENV_FILE"
set +a

MEDIA_SERVICES_CSV="${MEDIA_SERVICES:-}"

service_enabled() {
  local name="$1"
  case ",${MEDIA_SERVICES_CSV}," in
    *,"$name",*) return 0 ;;
    *) return 1 ;;
  esac
}

if ! service_enabled "jellyfin"; then
  exit 0
fi

install -d /usr/local/lib/vmctl
cat >/usr/local/lib/vmctl/jellyfin_discovery.py <<'PY'
#!/usr/bin/env python3
import json
import os
import socket
import time
import urllib.request

LISTEN_ADDR = os.environ.get("JELLYFIN_DISCOVERY_BIND", "0.0.0.0")
LISTEN_PORT = int(os.environ.get("JELLYFIN_DISCOVERY_PORT", "7359"))
JELLYFIN_INFO_URL = os.environ.get("JELLYFIN_INFO_URL", "http://127.0.0.1:8096/System/Info/Public")
ADVERTISED_ADDRESS = (os.environ.get("JELLYFIN_DISCOVERY_ADDRESS") or "").rstrip("/")


def current_payload():
    try:
        with urllib.request.urlopen(JELLYFIN_INFO_URL, timeout=5) as response:
            info = json.loads(response.read().decode("utf-8"))
    except Exception:
        info = {}
    address = ADVERTISED_ADDRESS or (info.get("LocalAddress") or "http://127.0.0.1:8096")
    return {
        "Address": address,
        "Id": info.get("Id") or "",
        "Name": info.get("ServerName") or "Jellyfin",
        "EndpointAddress": None,
    }


sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
sock.bind((LISTEN_ADDR, LISTEN_PORT))
sock.settimeout(1.0)

payload = current_payload()
last_refresh = time.time()

while True:
    now = time.time()
    if now - last_refresh > 30:
        payload = current_payload()
        last_refresh = now
    try:
        data, addr = sock.recvfrom(2048)
    except socket.timeout:
        continue
    msg = data.decode("utf-8", errors="ignore").strip().lower()
    if "who is jellyfinserver?" in msg:
        sock.sendto(json.dumps(payload, separators=(",", ":")).encode("utf-8"), addr)
PY
chmod +x /usr/local/lib/vmctl/jellyfin_discovery.py

cat >/etc/systemd/system/vmctl-jellyfin-discovery.service <<EOF
[Unit]
Description=vmctl Jellyfin UDP discovery shim
After=network-online.target docker.service
Wants=network-online.target

[Service]
Type=simple
Environment=JELLYFIN_DISCOVERY_ADDRESS=${JELLYFIN_URL:-http://media-stack.home.arpa:8096}
ExecStart=/usr/local/lib/vmctl/jellyfin_discovery.py
Restart=always
RestartSec=2
User=root

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now vmctl-jellyfin-discovery.service
systemctl restart vmctl-jellyfin-discovery.service
