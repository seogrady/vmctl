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

check_http_ok() {
  local url="$1"
  local label="$2"
  local attempts=30
  local delay=2
  local tmp code
  tmp="$(mktemp)"
  for _ in $(seq 1 "$attempts"); do
    code="$(curl -sS -o "$tmp" -w '%{http_code}' --max-time 20 "$url" || true)"
    if [[ "$code" == "200" ]]; then
      rm -f "$tmp"
      return 0
    fi
    sleep "$delay"
  done
  echo "validation failed: ${label} returned HTTP ${code} (${url})" >&2
  echo "response preview:" >&2
  head -c 300 "$tmp" >&2 || true
  rm -f "$tmp"
  return 1
}

check_http_no_auth() {
  local url="$1"
  local label="$2"
  local code
  code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 "$url" || true)"
  case "$code" in
    200|204) return 0 ;;
    *)
      echo "validation failed: ${label} appears to require auth (HTTP ${code}) at ${url}" >&2
      return 1
      ;;
  esac
}

check_container_running() {
  local name="$1"
  if ! docker ps --format '{{.Names}}' | grep -qx "$name"; then
    echo "validation failed: container not running: $name" >&2
    return 1
  fi
}

python3 <<'PY'
import json
import os
import subprocess
import urllib.error
import urllib.request

JELLYFIN_BASE = (os.environ.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/")
STREAMYFIN_ID = "1e9e5d386e6746158719e98a5c34f004"
JELLIO_ID = "e874be83fe364568abacf5ce0574b409"
ADMIN_USER = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
ADMIN_PASSWORD = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")


def get_json(url: str):
    with urllib.request.urlopen(url, timeout=20) as response:
        return json.loads(response.read().decode("utf-8"))


def jellyfin_token() -> str:
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="validate", DeviceId="vmctl-validate", Version="1.0"',
    }
    req = urllib.request.Request(
        f"{JELLYFIN_BASE}/Users/AuthenticateByName",
        data=json.dumps({"Username": ADMIN_USER, "Pw": ADMIN_PASSWORD}).encode("utf-8"),
        headers=headers,
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=20) as response:
        payload = json.loads(response.read().decode("utf-8"))
    return payload["AccessToken"]


if "jellyfin" in (os.environ.get("MEDIA_SERVICES", "")):
    get_json(f"{JELLYFIN_BASE}/System/Info/Public")
    token = jellyfin_token()
    headers = {
        "Authorization": 'MediaBrowser Client="vmctl", Device="validate", DeviceId="vmctl-validate", Version="1.0"',
        "X-Emby-Token": token,
    }
    req = urllib.request.Request(f"{JELLYFIN_BASE}/Plugins", headers=headers, method="GET")
    with urllib.request.urlopen(req, timeout=20) as response:
        plugins = json.loads(response.read().decode("utf-8"))
    ids = {plugin.get("Id") for plugin in plugins}
    if STREAMYFIN_ID not in ids:
        raise RuntimeError("streamyfin plugin not installed")
    if JELLIO_ID not in ids:
        raise RuntimeError("jellio plugin not installed")

if "meilisearch" in (os.environ.get("MEDIA_SERVICES", "")):
    with urllib.request.urlopen("http://127.0.0.1:7700/health", timeout=20) as response:
        if response.status != 200:
            raise RuntimeError("meilisearch health check failed")

if "jellysearch" in (os.environ.get("MEDIA_SERVICES", "")):
    with urllib.request.urlopen("http://127.0.0.1:5000/Items?SearchTerm=test&Limit=1", timeout=20) as response:
        if response.status != 200:
            raise RuntimeError("jellysearch integration check failed")

for key in ("JELLIO_STREMIO_MANIFEST_URL_LAN", "JELLIO_STREMIO_MANIFEST_URL_TAILNET"):
    value = (os.environ.get(key) or "").strip()
    if not value:
        continue
    try:
        manifest = get_json(value)
        if "resources" not in manifest:
            raise RuntimeError(f"{key} does not point to a valid stremio manifest")
    except (urllib.error.HTTPError, urllib.error.URLError) as err:
        print(f"warning: unable to validate {key}: {err}")

if os.environ.get("TAILSCALE_HTTPS_ENABLED", "true").lower() not in {"false", "0"}:
    try:
        status_raw = subprocess.check_output(["tailscale", "status", "--json"], text=True)
        status = json.loads(status_raw)
        if status.get("BackendState") not in {"Running", "Starting"}:
            raise RuntimeError("tailscale backend is not running")
        serve_status = subprocess.check_output(["tailscale", "serve", "status"], text=True)
        if "http://127.0.0.1:80" not in serve_status:
            raise RuntimeError("tailscale serve target mismatch")
    except FileNotFoundError:
        raise RuntimeError("tailscale binary is not installed")
PY

if service_enabled "caddy"; then
  check_container_running "media-caddy-1"
  check_http_ok "http://127.0.0.1:80/" "caddy portal"
fi

if service_enabled "jellyfin"; then
  check_container_running "media-jellyfin-1"
  check_http_ok "${JELLYFIN_INTERNAL_URL:-http://127.0.0.1:8096}/System/Info/Public" "jellyfin public info"
  if service_enabled "caddy"; then
    check_http_no_auth "http://127.0.0.1:8097/Users/Me" "jellyfin no-login proxy"
    autologin_url="$(curl -fsS http://127.0.0.1:80/jellyfin-autologin.url | tr -d '\n\r' || true)"
    if [[ -z "$autologin_url" ]]; then
      echo "validation failed: empty jellyfin autologin URL" >&2
      exit 1
    fi
  fi
fi

if service_enabled "jellyseerr"; then
  check_container_running "media-jellyseerr-1"
  check_http_ok "http://127.0.0.1:5055/api/v1/status" "jellyseerr status"
  if service_enabled "caddy"; then
    check_http_no_auth "http://127.0.0.1:5056/api/v1/auth/me" "jellyseerr no-login proxy"
  fi
fi

if service_enabled "bazarr"; then
  check_container_running "media-bazarr-1"
  check_http_ok "http://127.0.0.1:6767" "bazarr ui"
  check_http_no_auth "http://127.0.0.1:6767" "bazarr no-login ui"
fi

if service_enabled "jellystat"; then
  check_container_running "media-jellystat-1"
  check_http_ok "http://127.0.0.1:3000" "jellystat ui"
  check_http_no_auth "http://127.0.0.1:3000" "jellystat no-login ui"
fi

if service_enabled "sonarr"; then
  check_container_running "media-sonarr-1"
  check_http_ok "http://127.0.0.1:8989/ping" "sonarr ping"
  check_http_no_auth "http://127.0.0.1:8989/ping" "sonarr no-login ping"
fi

if service_enabled "radarr"; then
  check_container_running "media-radarr-1"
  check_http_ok "http://127.0.0.1:7878/ping" "radarr ping"
  check_http_no_auth "http://127.0.0.1:7878/ping" "radarr no-login ping"
fi

if service_enabled "prowlarr"; then
  check_container_running "media-prowlarr-1"
  check_http_ok "http://127.0.0.1:9696/ping" "prowlarr ping"
  check_http_no_auth "http://127.0.0.1:9696/ping" "prowlarr no-login ping"
fi

if service_enabled "jellysearch"; then
  check_container_running "media-jellysearch-1"
  check_http_ok "http://127.0.0.1:5000/Items?SearchTerm=test&Limit=1" "jellysearch query"
  check_http_no_auth "http://127.0.0.1:5000/Items?SearchTerm=test&Limit=1" "jellysearch no-login query"
fi

if service_enabled "qbittorrent-vpn"; then
  check_container_running "media-qbittorrent-vpn-1"
  check_http_no_auth "http://127.0.0.1:${QBITTORRENT_WEBUI_PORT:-8080}/api/v2/app/version" "qbittorrent no-login api"
fi

if service_enabled "jellyfin"; then
  python3 <<'PY'
import json
import socket
import urllib.request

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.settimeout(3)
sock.bind(("0.0.0.0", 0))
sock.sendto(b"Who is JellyfinServer?", ("127.0.0.1", 7359))
try:
    data, _ = sock.recvfrom(4096)
except Exception as exc:
    raise SystemExit(f"validation failed: jellyfin discovery did not respond: {exc}")
payload = json.loads(data.decode("utf-8"))
address = payload.get("Address", "")
if "127.0.0.1" in address or "[::1]" in address:
    raise SystemExit(f"validation failed: jellyfin discovery advertised loopback address: {address}")
with urllib.request.urlopen(f"{address}/System/Info/Public", timeout=10) as response:
    if response.status != 200:
        raise SystemExit(f"validation failed: discovery address not reachable: {address} (HTTP {response.status})")
PY
fi

if service_enabled "caddy"; then
  for key in lan tailnet; do
    url_file="http://127.0.0.1:80/jellio-manifest.${key}.url"
    manifest_url="$(curl -fsS "$url_file" | tr -d '\n\r')"
    if [[ -z "$manifest_url" ]]; then
      echo "validation failed: empty manifest URL in ${url_file}" >&2
      exit 1
    fi
    check_http_ok "$manifest_url" "jellio manifest (${key})"
  done
fi

# Optional custom validators: /opt/media/validators.d/<name>.sh
if [[ -d "$STACK_DIR/validators.d" ]]; then
  shopt -s nullglob
  for validator in "$STACK_DIR"/validators.d/*.sh; do
    if [[ -x "$validator" ]]; then
      "$validator"
    fi
  done
  shopt -u nullglob
fi
