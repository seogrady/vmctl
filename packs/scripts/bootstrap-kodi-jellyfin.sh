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
KODI_WEB_PORT="${KODI_WEB_PORT:-80}"
JELLYFIN_URL="${JELLYFIN_URL:-http://media-stack.home.arpa:8096}"
JELLYFIN_ADMIN_USER="${JELLYFIN_ADMIN_USER:-admin}"
JELLYFIN_ADMIN_PASSWORD="${JELLYFIN_ADMIN_PASSWORD:-}"

JELLYFIN_URL="$(
  JELLYFIN_URL="$JELLYFIN_URL" python3 <<'PY'
import json
import os
import socket
import subprocess
import sys
import urllib.parse
import urllib.request

configured = os.environ["JELLYFIN_URL"].rstrip("/")
parsed = urllib.parse.urlparse(configured)
scheme = parsed.scheme or "http"
host = parsed.hostname or "media-stack"
port = parsed.port or 8096
short_host = host.split(".")[0]

def candidate_url(candidate_host):
    if ":" in candidate_host and not candidate_host.startswith("["):
        candidate_host = f"[{candidate_host}]"
    return f"{scheme}://{candidate_host}:{port}"

def reachable(url):
    try:
        with urllib.request.urlopen(f"{url}/System/Info/Public", timeout=3) as response:
            return response.status == 200
    except Exception:
        return False

seen = set()
candidates = []

def add(url):
    url = url.rstrip("/")
    if url not in seen:
        seen.add(url)
        candidates.append(url)

add(configured)
add(candidate_url(short_host))

try:
    for info in socket.getaddrinfo(short_host, port, proto=socket.IPPROTO_TCP):
        add(candidate_url(info[4][0]))
except socket.gaierror:
    pass

try:
    status = subprocess.check_output(["tailscale", "status", "--json"], text=True, timeout=5)
    peers = json.loads(status).get("Peer", {}).values()
    for peer in peers:
        if peer.get("HostName") != short_host:
            continue
        dns_name = peer.get("DNSName", "").rstrip(".")
        if dns_name:
            add(candidate_url(dns_name))
            add(candidate_url(dns_name.split(".")[0]))
        for address in peer.get("TailscaleIPs", []):
            add(candidate_url(address))
except Exception:
    pass

for url in candidates:
    if reachable(url):
        print(url)
        break
else:
    print(configured)
PY
)"

install -d -o "$KODI_USER" -g "$KODI_USER" \
  "$KODI_HOME/.kodi/addons" \
  "$KODI_HOME/.kodi/userdata/addon_data/plugin.video.jellyfin" \
  "$KODI_HOME/.kodi/userdata/addon_data/service.jellyfin"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

repo_zip="$tmp/repository.jellyfin.kodi.zip"
curl -fsSL "https://repo.jellyfin.org/releases/client/kodi/repository.jellyfin.kodi.zip" -o "$repo_zip"
if [[ -s "$repo_zip" ]]; then
  unzip -oq "$repo_zip" -d "$KODI_HOME/.kodi/addons"
fi

KODI_ADDONS_DIR="$KODI_HOME/.kodi/addons" python3 <<'PY'
import gzip
import os
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
import zipfile

addons_dir = os.environ["KODI_ADDONS_DIR"]
addons = {}
sources = [
    ("https://repo.jellyfin.org/releases/client/kodi/py3", "addons.xml", False),
    ("https://mirrors.kodi.tv/addons/nexus", "addons.xml.gz", True),
]

for base_url, index_name, compressed in sources:
    data = urllib.request.urlopen(f"{base_url}/{index_name}", timeout=60).read()
    if compressed:
        data = gzip.decompress(data)
    root = ET.fromstring(data)
    for addon in root.findall("addon"):
        addon_id = addon.get("id")
        if addon_id and addon_id not in addons:
            addons[addon_id] = (base_url, addon)

installed = set()

def install_addon(addon_id):
    if addon_id.startswith(("xbmc.", "kodi.")) or addon_id in installed:
        return
    found = addons.get(addon_id)
    if found is None:
        return
    base_url, addon = found
    for dependency in addon.findall("./requires/import"):
        install_addon(dependency.get("addon", ""))
    version = addon.get("version")
    if not version:
        return
    installed_addon = os.path.join(addons_dir, addon_id, "addon.xml")
    if os.path.exists(installed_addon):
        try:
            current = ET.parse(installed_addon).getroot().get("version")
            if current == version:
                installed.add(addon_id)
                return
        except ET.ParseError:
            pass
    filename = f"{addon_id}-{version}.zip"
    quoted = urllib.parse.quote(filename)
    url = f"{base_url}/{addon_id}/{quoted}"
    target = os.path.join("/tmp", filename)
    urllib.request.urlretrieve(url, target)
    with zipfile.ZipFile(target) as archive:
        archive.extractall(addons_dir)
    installed.add(addon_id)

install_addon("plugin.video.jellyfin")
PY

cat > "$KODI_HOME/.kodi/userdata/sources.xml" <<EOF
<sources>
  <video>
    <default pathversion="1"></default>
    <source>
      <name>Jellyfin</name>
      <path pathversion="1">${JELLYFIN_URL}</path>
      <allowsharing>true</allowsharing>
    </source>
  </video>
</sources>
EOF

cat > "$KODI_HOME/.kodi/userdata/addon_data/plugin.video.jellyfin/settings.xml" <<EOF
<settings version="2">
  <setting id="username">${JELLYFIN_ADMIN_USER}</setting>
  <setting id="serverName">media-stack</setting>
  <setting id="server">${JELLYFIN_URL}</setting>
  <setting id="connectMsg">false</setting>
  <setting id="useDirectPaths">false</setting>
  <setting id="syncEmptyShows">true</setting>
</settings>
EOF

cat > "$KODI_HOME/.kodi/userdata/addon_data/service.jellyfin/settings.xml" <<EOF
<settings version="2">
  <setting id="username">${JELLYFIN_ADMIN_USER}</setting>
  <setting id="password">${JELLYFIN_ADMIN_PASSWORD}</setting>
  <setting id="server">${JELLYFIN_URL}</setting>
  <setting id="serverName">media-stack</setting>
  <setting id="enableContext">true</setting>
  <setting id="remoteControl">true</setting>
</settings>
EOF

if [[ -n "$JELLYFIN_ADMIN_PASSWORD" ]]; then
  KODI_ADDON_DATA="$KODI_HOME/.kodi/userdata/addon_data/plugin.video.jellyfin" \
  JELLYFIN_URL="$JELLYFIN_URL" \
  JELLYFIN_ADMIN_USER="$JELLYFIN_ADMIN_USER" \
  JELLYFIN_ADMIN_PASSWORD="$JELLYFIN_ADMIN_PASSWORD" \
  python3 <<'PY'
from datetime import UTC, datetime
import json
import os
import sys
import urllib.error
import urllib.request

base = os.environ["JELLYFIN_URL"].rstrip("/")
username = os.environ["JELLYFIN_ADMIN_USER"]
password = os.environ["JELLYFIN_ADMIN_PASSWORD"]
addon_data = os.environ["KODI_ADDON_DATA"]

headers = {
    "Content-Type": "application/json",
    "Authorization": 'MediaBrowser Client="vmctl", Device="Kodi HTPC", DeviceId="vmctl-kodi-htpc", Version="1.0"',
}

def request_json(path, payload=None, extra_headers=None):
    data = None if payload is None else json.dumps(payload).encode()
    req_headers = dict(headers)
    if extra_headers:
        req_headers.update(extra_headers)
    req = urllib.request.Request(base + path, data=data, headers=req_headers)
    with urllib.request.urlopen(req, timeout=60) as response:
        return json.loads(response.read().decode())

try:
    public_info = request_json("/System/Info/Public")
    auth = request_json(
        "/Users/AuthenticateByName",
        {"Username": username, "Pw": password},
    )
except urllib.error.HTTPError as exc:
    if exc.code in (401, 403):
        print(
            "warning: Jellyfin credentials rejected (401/403), skipping Kodi Jellyfin token bootstrap",
            file=sys.stderr,
        )
        auth = None
    else:
        raise SystemExit(f"failed to create Kodi Jellyfin credentials: {exc}")
except (urllib.error.URLError, TimeoutError) as exc:
    raise SystemExit(f"failed to create Kodi Jellyfin credentials: {exc}")

if auth:
    server = {
        "AccessToken": auth["AccessToken"],
        "DateLastAccessed": datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "Id": auth.get("ServerId") or public_info["Id"],
        "Name": public_info.get("ServerName", "media-stack"),
        "UserId": auth["User"]["Id"],
        "Users": [{"Id": auth["User"]["Id"], "IsSignedInOffline": True}],
        "address": base,
    }

    os.makedirs(addon_data, exist_ok=True)
    with open(os.path.join(addon_data, "data.json"), "w", encoding="utf-8") as handle:
        json.dump({"Servers": [server]}, handle, indent=4, sort_keys=True)
PY
fi

cat > "$KODI_HOME/.kodi/userdata/autoexec.py" <<'EOF'
import xbmc

xbmc.executebuiltin("InstallAddon(plugin.video.jellyfin)")
xbmc.executebuiltin("UpdateLibrary(video)")
EOF

chown -R "$KODI_USER:$KODI_USER" "$KODI_HOME/.kodi"
systemctl restart kodi-htpc.service

for _ in {1..60}; do
  if curl -fsS http://127.0.0.1:${KODI_WEB_PORT:-80}/jsonrpc \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"JSONRPC.Ping","id":1}' >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

curl -fsS http://127.0.0.1:${KODI_WEB_PORT:-80}/jsonrpc \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"Addons.SetAddonEnabled","params":{"addonid":"plugin.video.jellyfin","enabled":true},"id":1}' >/dev/null 2>&1 || true

curl -fsS http://127.0.0.1:${KODI_WEB_PORT:-80}/jsonrpc \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"VideoLibrary.Scan","id":1}' >/dev/null 2>&1 || true
