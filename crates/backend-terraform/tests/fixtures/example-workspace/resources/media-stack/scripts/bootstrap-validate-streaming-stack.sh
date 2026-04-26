#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"
COMPOSE_FILE="$STACK_DIR/docker-compose.yml"

if [[ ! -f "$ENV_FILE" ]]; then
  exit 0
fi

set -a
. "$ENV_FILE"
set +a

MEDIA_SERVICES_CSV="${MEDIA_SERVICES:-}"
VMCTL_HOST_SHORT="${VMCTL_HOST_SHORT:-${VMCTL_RESOURCE_NAME:-media-stack}}"
VMCTL_HTTP_BASE_URL_SHORT="${VMCTL_HTTP_BASE_URL_SHORT:-http://${VMCTL_HOST_SHORT}}"
TIZEN_STREMIO_USER_AGENT="${TIZEN_STREMIO_USER_AGENT:-Mozilla/5.0 (SMART-TV; Linux; Tizen 6.5) AppleWebKit/537.36 Stremio}"

service_enabled() {
  local name="$1"
  case ",${MEDIA_SERVICES_CSV}," in
    *,"$name",*) return 0 ;;
    *) return 1 ;;
  esac
}

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-media}"
docker_compose() {
  docker compose -p "$COMPOSE_PROJECT_NAME" --project-directory "$STACK_DIR" --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"
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

check_http_no_redirect() {
  local url="$1"
  local label="$2"
  local headers code
  headers="$(curl -sS -D - -o /dev/null --max-time 20 "$url" || true)"
  code="$(printf '%s\n' "$headers" | awk 'NR==1 {print $2}')"
  case "$code" in
    200|204) ;;
    301|302|307|308)
      echo "validation failed: ${label} redirected instead of serving HTTP (${url})" >&2
      printf '%s\n' "$headers" >&2
      return 1
      ;;
    *)
      echo "validation failed: ${label} returned HTTP ${code:-unknown} (${url})" >&2
      printf '%s\n' "$headers" >&2
      return 1
      ;;
  esac
  if printf '%s\n' "$headers" | grep -qi '^Strict-Transport-Security:'; then
    echo "validation failed: ${label} returned Strict-Transport-Security on LAN HTTP (${url})" >&2
    return 1
  fi
}

check_container_running() {
  local service="$1"
  if ! docker_compose ps --status running --services | grep -qx "$service"; then
    echo "validation failed: compose service not running: $service" >&2
    docker_compose ps >&2 || true
    return 1
  fi
}

python3 <<'PY'
import json
import os
import base64
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

for key in ("JELLIO_STREMIO_MANIFEST_URL_TAILSCALE",):
    value = (os.environ.get(key) or "").strip()
    if not value:
        continue
    try:
        manifest = get_json(value)
        if "resources" not in manifest:
            raise RuntimeError(f"{key} does not point to a valid stremio manifest")
        encoded = value.rstrip("/").split("/jellio/", 1)[1].split("/", 1)[0]
        padded = encoded + ("=" * (-len(encoded) % 4))
        payload = json.loads(base64.urlsafe_b64decode(padded).decode("utf-8"))
        public_base = (payload.get("PublicBaseUrl") or "").rstrip("/")
        if not public_base.endswith("/jf"):
            raise RuntimeError(f"{key} PublicBaseUrl must use the /jf Jellyfin proxy, got {public_base!r}")
        if "/jf/jellio/" in value:
            raise RuntimeError(f"{key} manifest URL must stay on the /jellio addon route")
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
  check_container_running "caddy"
  check_http_ok "http://127.0.0.1:80/" "caddy portal"
  check_http_no_redirect "${VMCTL_HTTP_BASE_URL_SHORT}/healthz" "${VMCTL_HOST_SHORT} LAN HTTP"
fi

if service_enabled "jellyfin"; then
  check_container_running "jellyfin"
  check_http_ok "${JELLYFIN_INTERNAL_URL:-http://127.0.0.1:8096}/System/Info/Public" "jellyfin public info"
  if service_enabled "caddy"; then
    check_http_no_auth "http://127.0.0.1:8097/Users/Me" "jellyfin no-login proxy"
    check_http_ok "${VMCTL_HTTP_BASE_URL_SHORT}/jf/System/Info/Public" "jellyfin stremio proxy"
    autologin_url="$(curl -fsS http://127.0.0.1:80/jellyfin-autologin.url | tr -d '\n\r' || true)"
    if [[ -z "$autologin_url" ]]; then
      echo "validation failed: empty jellyfin autologin URL" >&2
      exit 1
    fi
  fi
fi

if service_enabled "seerr"; then
  check_container_running "seerr"
  check_http_ok "http://127.0.0.1:5055/api/v1/status" "seerr status"
  if service_enabled "caddy"; then
    check_http_ok "http://127.0.0.1:5056/api/v1/settings/public" "seerr proxied public settings"
  fi

  python3 <<'PY'
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
settings_path = config_root / "seerr" / "settings.json"
if not settings_path.exists():
    raise SystemExit(f"validation failed: missing Seerr settings file: {settings_path}")

settings = json.loads(settings_path.read_text(encoding="utf-8"))
if not (settings.get("public") or {}).get("initialized"):
    raise SystemExit("validation failed: Seerr public settings are not initialized")

jellyfin_settings = settings.get("jellyfin") or {}
configured_jellyfin = bool((jellyfin_settings.get("ip") or "").strip())

for app in ("sonarr", "radarr"):
    config_path = config_root / app / "config.xml"
    if not config_path.exists():
        raise SystemExit(f"validation failed: missing {app} config at {config_path}")
    root = ET.parse(config_path).getroot()
    api_key = (root.findtext("ApiKey") or "").strip()
    if not api_key:
        raise SystemExit(f"validation failed: missing {app} API key in {config_path}")

expected = {
    "sonarr": {
        "hostname": "sonarr",
        "port": 8989,
        "root": os.environ.get("SONARR_ROOT_FOLDER", "/data/media/tv"),
        "profile": os.environ.get("SONARR_DEFAULT_QUALITY_PROFILE", "WEB-1080p"),
        "download_host": "gluetun" if (os.environ.get("MEDIA_VPN_ENABLED") or "").lower() == "true" else "qbittorrent-vpn",
        "category": os.environ.get("QBITTORRENT_CATEGORY_TV", "tv"),
        "category_field": "tvCategory",
    },
    "radarr": {
        "hostname": "radarr",
        "port": 7878,
        "root": os.environ.get("RADARR_ROOT_FOLDER", "/data/media/movies"),
        "profile": os.environ.get("RADARR_DEFAULT_QUALITY_PROFILE", "HD - 720p/1080p"),
        "download_host": "gluetun" if (os.environ.get("MEDIA_VPN_ENABLED") or "").lower() == "true" else "qbittorrent-vpn",
        "category": os.environ.get("QBITTORRENT_CATEGORY_MOVIES", "movies"),
        "category_field": "movieCategory",
    },
}

for app, status_url in (
    ("sonarr", "http://127.0.0.1:8989/api/v3/system/status"),
    ("radarr", "http://127.0.0.1:7878/api/v3/system/status"),
):
    api_key = ET.parse(config_root / app / "config.xml").getroot().findtext("ApiKey") or ""
    req = urllib.request.Request(status_url, headers={"X-Api-Key": api_key}, method="GET")
    with urllib.request.urlopen(req, timeout=20) as response:
        if response.status != 200:
            raise SystemExit(f"validation failed: {app} status endpoint returned HTTP {response.status}")

    root_req = urllib.request.Request(
        status_url.replace("/system/status", "/rootfolder"),
        headers={"X-Api-Key": api_key},
        method="GET",
    )
    with urllib.request.urlopen(root_req, timeout=20) as response:
        roots = json.loads(response.read().decode("utf-8"))
    if expected[app]["root"] not in {item.get("path") for item in roots}:
        raise SystemExit(f"validation failed: {app} missing expected root folder {expected[app]['root']}")

    media_req = urllib.request.Request(
        status_url.replace("/system/status", "/config/mediamanagement"),
        headers={"X-Api-Key": api_key},
        method="GET",
    )
    with urllib.request.urlopen(media_req, timeout=20) as response:
        media = json.loads(response.read().decode("utf-8"))
    if media.get("skipFreeSpaceCheckWhenImporting") is not False:
        raise SystemExit(f"validation failed: {app} free-space import check must remain enabled")
    if int(media.get("minimumFreeSpaceWhenImporting") or -1) != 100:
        raise SystemExit(
            f"validation failed: {app} minimumFreeSpaceWhenImporting mismatch: {media.get('minimumFreeSpaceWhenImporting')!r} != 100"
        )
    if media.get("copyUsingHardlinks") is not True:
        raise SystemExit(f"validation failed: {app} copyUsingHardlinks must remain enabled")
    if (media.get("rescanAfterRefresh") or "").lower() != "always":
        raise SystemExit(
            f"validation failed: {app} rescanAfterRefresh mismatch: {media.get('rescanAfterRefresh')!r} != 'always'"
        )

    dl_req = urllib.request.Request(
        status_url.replace("/system/status", "/downloadclient"),
        headers={"X-Api-Key": api_key},
        method="GET",
    )
    with urllib.request.urlopen(dl_req, timeout=20) as response:
        clients = json.loads(response.read().decode("utf-8"))

    target = next((item for item in clients if item.get("name") == "qBittorrent"), None)
    sab_target = next((item for item in clients if item.get("name") == "SABnzbd"), None)
    if not target:
        raise SystemExit(f"validation failed: {app} missing qBittorrent download client")
    expected_qbit_priority = 2 if sab_target else 1
    if int(target.get("priority") or 0) != expected_qbit_priority:
        raise SystemExit(
            f"validation failed: {app} qBittorrent priority mismatch: {target.get('priority')!r} != {expected_qbit_priority!r}"
        )
    fields = {field.get("name"): field.get("value") for field in target.get("fields") or []}
    if fields.get("host") != expected[app]["download_host"]:
        raise SystemExit(
            f"validation failed: {app} qBittorrent host mismatch: {fields.get('host')!r} != {expected[app]['download_host']!r}"
        )
    if str(fields.get(expected[app]["category_field"]) or "") != expected[app]["category"]:
        raise SystemExit(
            f"validation failed: {app} qBittorrent category mismatch: {fields.get(expected[app]['category_field'])!r} != {expected[app]['category']!r}"
        )
    if app == "sonarr" and fields.get("username") != os.environ.get("QBITTORRENT_USERNAME", "admin"):
        raise SystemExit(
            f"validation failed: sonarr qBittorrent username mismatch: {fields.get('username')!r}"
        )

for key, url in (
    ("direct", "http://127.0.0.1:5055/api/v1/settings/public"),
    ("proxied", "http://127.0.0.1:5056/api/v1/settings/public"),
):
    with urllib.request.urlopen(url, timeout=20) as response:
        if response.status != 200:
            raise SystemExit(f"validation failed: Seerr {key} settings endpoint returned HTTP {response.status}")
        payload = json.loads(response.read().decode("utf-8"))
    if "applicationTitle" not in payload:
        raise SystemExit(f"validation failed: Seerr {key} settings payload is missing applicationTitle")
    if not payload.get("initialized"):
        raise SystemExit(f"validation failed: Seerr {key} settings payload is not initialized")
    if not payload.get("mediaServerLogin"):
        raise SystemExit(f"validation failed: Seerr {key} settings payload has mediaServerLogin disabled")

login_payload = {
    "email": os.environ.get("JELLYFIN_ADMIN_USER", "admin"),
    "username": os.environ.get("JELLYFIN_ADMIN_USER", "admin"),
    "password": os.environ.get("JELLYFIN_ADMIN_PASSWORD", ""),
}
if not configured_jellyfin:
    jellyfin_internal = urllib.parse.urlparse(os.environ.get("JELLYFIN_INTERNAL_URL", "http://jellyfin:8096"))
    login_payload.update(
        {
            "hostname": jellyfin_internal.hostname or "jellyfin",
            "port": jellyfin_internal.port or (443 if jellyfin_internal.scheme == "https" else 8096),
            "useSsl": jellyfin_internal.scheme == "https",
            "urlBase": jellyfin_internal.path.rstrip("/"),
            "serverType": 2,
        }
    )
login_req = urllib.request.Request(
    "http://127.0.0.1:5055/api/v1/auth/jellyfin",
    data=json.dumps(login_payload).encode("utf-8"),
    headers={"Content-Type": "application/json"},
    method="POST",
)
try:
    with urllib.request.urlopen(login_req, timeout=20) as response:
        if response.status not in (200, 204):
            raise SystemExit(f"validation failed: Jellyfin login returned HTTP {response.status}")
except urllib.error.HTTPError as err:
    detail = err.read().decode("utf-8", errors="replace")
    raise SystemExit(f"validation failed: Jellyfin login returned HTTP {err.code}: {detail}") from err

for app in ("sonarr", "radarr"):
    entries = settings.get(app) or []
    if not entries:
        raise SystemExit(f"validation failed: Seerr settings missing {app} integration")
    entry = entries[0]
    if entry.get("hostname") != expected[app]["hostname"]:
        raise SystemExit(
            f"validation failed: Seerr {app} hostname mismatch: {entry.get('hostname')!r} != {expected[app]['hostname']!r}"
        )
    if int(entry.get("port") or 0) != expected[app]["port"]:
        raise SystemExit(
            f"validation failed: Seerr {app} port mismatch: {entry.get('port')!r} != {expected[app]['port']!r}"
        )
    if entry.get("activeDirectory") != expected[app]["root"]:
        raise SystemExit(
            f"validation failed: Seerr {app} root mismatch: {entry.get('activeDirectory')!r} != {expected[app]['root']!r}"
        )
    if entry.get("activeProfileName") != expected[app]["profile"]:
        raise SystemExit(
            f"validation failed: Seerr {app} profile mismatch: {entry.get('activeProfileName')!r} != {expected[app]['profile']!r}"
        )
    if not (entry.get("apiKey") or "").strip():
        raise SystemExit(f"validation failed: Seerr {app} API key is empty")
PY
fi

if service_enabled "sabnzbd"; then
  check_container_running "sabnzbd"
  api_key="$(python3 <<'PY'
import configparser
import os
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
config_path = config_root / "sabnzbd" / "sabnzbd.ini"
if not config_path.exists():
    raise SystemExit(f"validation failed: missing sabnzbd config at {config_path}")

parser = configparser.ConfigParser()
parser.read_string("[root]\n" + config_path.read_text(encoding="utf-8"))
api_key = ""
if parser.has_section("misc"):
    api_key = (parser.get("misc", "api_key", fallback="") or "").strip()
if not api_key:
    raise SystemExit("validation failed: SABnzbd API key is empty")
print(api_key)
PY
)"
  check_http_ok "http://127.0.0.1:8085/api?mode=version&apikey=${api_key}" "sabnzbd version"

python3 <<'PY'
import configparser
import json
import os
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
config_path = config_root / "sabnzbd" / "sabnzbd.ini"
if not config_path.exists():
    raise SystemExit(f"validation failed: missing sabnzbd config at {config_path}")

config_text = config_path.read_text(encoding="utf-8")

parser = configparser.ConfigParser()
parser.read_string("[root]\n" + config_text)
api_key = ""
if parser.has_section("misc"):
    api_key = (parser.get("misc", "api_key", fallback="") or "").strip()
if not api_key:
    raise SystemExit("validation failed: SABnzbd API key is empty")
server_host = (os.environ.get("SABNZBD_SERVER_HOST") or "").strip()
server_enabled = (os.environ.get("SABNZBD_SERVER_ENABLE") or "").strip().lower() not in {"", "0", "false", "no", "off"}
if server_enabled and server_host:
    if f"[[{server_host}]]" not in config_text:
        raise SystemExit(f"validation failed: SABnzbd server subsection missing [[{server_host}]]")
host_whitelist = ""
if parser.has_section("misc"):
    host_whitelist = (parser.get("misc", "host_whitelist", fallback="") or "").strip()
if server_enabled and server_host:
    host_tokens = {item.strip() for item in host_whitelist.split(",") if item.strip()}
    for required in ("sabnzbd", "media-stack"):
        if required not in host_tokens:
            raise SystemExit(
                f"validation failed: SABnzbd host_whitelist missing {required}: {host_whitelist!r}"
            )
    local_ranges = ""
    if parser.has_section("misc"):
        local_ranges = (parser.get("misc", "local_ranges", fallback="") or "").strip()
    local_tokens = {item.strip() for item in local_ranges.split(",") if item.strip()}
    for required in ("100.64.0.0/10", "172.18.0.0/16", "192.168.0.0/16"):
        if required not in local_tokens:
            raise SystemExit(
                f"validation failed: SABnzbd local_ranges missing {required}: {local_ranges!r}"
            )

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None

opener = urllib.request.build_opener(NoRedirect())
request = urllib.request.Request("http://127.0.0.1:8085/", method="GET")
try:
    with opener.open(request, timeout=20) as response:
        final_url = response.geturl()
        if "/wizard" in final_url:
            raise SystemExit(f"validation failed: sabnzbd still redirects to wizard: {final_url}")
except urllib.error.HTTPError as err:
    location = err.headers.get("Location", "")
    if err.code in (301, 302, 303, 307, 308) and "/wizard" in location:
        raise SystemExit(f"validation failed: sabnzbd still redirects to wizard: {location}")
    raise

req = urllib.request.Request(
    f"http://127.0.0.1:8085/api?mode=get_cats&apikey={api_key}",
    method="GET",
)
with urllib.request.urlopen(req, timeout=20) as response:
    cats = json.loads(response.read().decode("utf-8"))
names = set(cats.get("categories") or [])
if not {"tv", "movies"}.issubset(names):
    raise SystemExit(f"validation failed: SABnzbd categories missing tv/movies: {sorted(names)!r}")

download_protocols = [
    item.strip()
    for item in (os.environ.get("VMCTL_DOWNLOAD_PROTOCOLS") or "").split(",")
    if item.strip()
]
if not download_protocols:
    raise SystemExit("validation failed: VMCTL_DOWNLOAD_PROTOCOLS is empty")
selected_priorities = {protocol: index + 1 for index, protocol in enumerate(download_protocols)}
sab_expected = "usenet" in selected_priorities

expected = {
    "sonarr": {
        "host": urllib.parse.urlparse(os.environ.get("SABNZBD_INTERNAL_URL", "http://sabnzbd:8080")).hostname or "sabnzbd",
        "port": 8080,
        "category_field": "tvCategory",
        "category": os.environ.get("QBITTORRENT_CATEGORY_TV", "tv"),
    },
    "radarr": {
        "host": urllib.parse.urlparse(os.environ.get("SABNZBD_INTERNAL_URL", "http://sabnzbd:8080")).hostname or "sabnzbd",
        "port": 8080,
        "category_field": "movieCategory",
        "category": os.environ.get("QBITTORRENT_CATEGORY_MOVIES", "movies"),
    },
}

for app, status_url in (
    ("sonarr", "http://127.0.0.1:8989/api/v3/downloadclient"),
    ("radarr", "http://127.0.0.1:7878/api/v3/downloadclient"),
):
    key = (ET.parse(config_root / app / "config.xml").getroot().findtext("ApiKey") or "").strip()
    req = urllib.request.Request(status_url, headers={"X-Api-Key": key}, method="GET")
    with urllib.request.urlopen(req, timeout=20) as response:
        clients = json.loads(response.read().decode("utf-8"))
    target = next((item for item in clients if item.get("name") == "SABnzbd"), None)
    if not sab_expected:
        if target:
            raise SystemExit(f"validation failed: {app} unexpectedly still has SABnzbd configured")
        continue
    if not target:
        raise SystemExit(f"validation failed: {app} missing SABnzbd download client")
    if int(target.get("priority") or 0) != selected_priorities["usenet"]:
        raise SystemExit(
            f"validation failed: {app} SABnzbd priority mismatch: {target.get('priority')!r} != {selected_priorities['usenet']!r}"
        )
    fields = {field.get("name"): field.get("value") for field in target.get("fields") or []}
    if fields.get("host") != expected[app]["host"]:
        raise SystemExit(
            f"validation failed: {app} SABnzbd host mismatch: {fields.get('host')!r} != {expected[app]['host']!r}"
        )
    if int(fields.get("port") or 0) != expected[app]["port"]:
        raise SystemExit(
            f"validation failed: {app} SABnzbd port mismatch: {fields.get('port')!r} != {expected[app]['port']!r}"
        )
    if str(fields.get(expected[app]["category_field"]) or "") != expected[app]["category"]:
        raise SystemExit(
            f"validation failed: {app} SABnzbd category mismatch: {fields.get(expected[app]['category_field'])!r} != {expected[app]['category']!r}"
        )
PY
fi

if service_enabled "recyclarr"; then
  check_container_running "recyclarr"
  python3 <<'PY'
import os
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
config_path = config_root / "recyclarr" / "recyclarr.yml"
if not config_path.exists():
    raise SystemExit(f"validation failed: missing Recyclarr config at {config_path}")
text = config_path.read_text(encoding="utf-8")
for token in (
    "sonarr:",
    "radarr:",
    "quality_profiles:",
    "delete_old_custom_formats: true",
    "trash_id: 72dae194fc92bf828f32cde7744e51a1",
    "trash_id: d1d67249d3890e49bc12e275d989a7e9",
):
    if token not in text:
        raise SystemExit(f"validation failed: Recyclarr config missing {token!r}")
PY
fi

if service_enabled "bazarr"; then
  check_container_running "bazarr"
  check_http_ok "http://127.0.0.1:6767" "bazarr ui"
  check_http_no_auth "http://127.0.0.1:6767" "bazarr no-login ui"
fi

if service_enabled "jellystat"; then
  check_container_running "jellystat"
  check_http_ok "http://127.0.0.1:3000" "jellystat ui"
  check_http_no_auth "http://127.0.0.1:3000" "jellystat no-login ui"
fi

if service_enabled "sonarr"; then
  check_container_running "sonarr"
  check_http_ok "http://127.0.0.1:8989/ping" "sonarr ping"
  check_http_no_auth "http://127.0.0.1:8989/ping" "sonarr no-login ping"
fi

if service_enabled "radarr"; then
  check_container_running "radarr"
  check_http_ok "http://127.0.0.1:7878/ping" "radarr ping"
  check_http_no_auth "http://127.0.0.1:7878/ping" "radarr no-login ping"
fi

if service_enabled "prowlarr"; then
  check_container_running "prowlarr"
  check_http_ok "http://127.0.0.1:9696/ping" "prowlarr ping"
  check_http_no_auth "http://127.0.0.1:9696/ping" "prowlarr no-login ping"
  python3 <<'PY'
import json
import os
import urllib.request
from pathlib import Path

api_key = (os.environ.get("PROWLARR_API_KEY") or "").strip()
if not api_key:
    config_path = os.path.join(os.environ.get("CONFIG_PATH") or "/opt/media/config", "prowlarr", "config.xml")
    if os.path.exists(config_path):
        import xml.etree.ElementTree as ET

        api_key = (ET.parse(config_path).getroot().findtext("ApiKey") or "").strip()
if not api_key:
    raise SystemExit("validation failed: Prowlarr API key is missing")
req = urllib.request.Request(
    "http://127.0.0.1:9696/api/v1/indexerproxy",
    headers={"X-Api-Key": api_key},
    method="GET",
)
with urllib.request.urlopen(req, timeout=20) as response:
    proxies = json.loads(response.read().decode("utf-8"))
target = next((item for item in proxies if (item.get("name") or "").lower() == "flaresolverr"), None)
if not target:
    raise SystemExit("validation failed: Prowlarr missing FlareSolverr proxy")
fields = {field.get("name"): field.get("value") for field in target.get("fields") or []}
if not str(fields.get("host") or "").rstrip("/").endswith("flaresolverr:8191"):
    raise SystemExit(f"validation failed: Prowlarr FlareSolverr host mismatch: {fields.get('host')!r}")

download_protocols = [
    item.strip()
    for item in (os.environ.get("VMCTL_DOWNLOAD_PROTOCOLS") or "").split(",")
    if item.strip()
]
selected_protocols = set(download_protocols)
managed_torrent = {
    item.strip()
    for item in (os.environ.get("PROWLARR_BOOTSTRAP_INDEXERS_TORRENT") or "").split(",")
    if item.strip()
}
managed_usenet = {
    item.strip()
    for item in (os.environ.get("PROWLARR_BOOTSTRAP_INDEXERS_USENET") or "").split(",")
    if item.strip()
}
req = urllib.request.Request("http://127.0.0.1:9696/api/v1/indexer", headers={"X-Api-Key": api_key}, method="GET")
with urllib.request.urlopen(req, timeout=20) as response:
    indexers = json.loads(response.read().decode("utf-8"))
enabled_names = {item.get("name") for item in indexers if item.get("enable")}
req = urllib.request.Request("http://127.0.0.1:9696/api/v1/indexer/schema", headers={"X-Api-Key": api_key}, method="GET")
with urllib.request.urlopen(req, timeout=20) as response:
    schemas = json.loads(response.read().decode("utf-8"))
schema_names = {item.get("name") for item in schemas if item.get("name")}
managed_torrent = managed_torrent & schema_names
managed_usenet = managed_usenet & schema_names
if "torrent" in selected_protocols and not managed_torrent:
    raise SystemExit("validation failed: no schema-backed torrent indexers are available in Prowlarr")
if "usenet" in selected_protocols and not managed_usenet:
    raise SystemExit("validation failed: no schema-backed usenet indexers are available in Prowlarr")

if "torrent" in selected_protocols:
    if not managed_torrent.issubset(enabled_names):
        raise SystemExit(
            f"validation failed: Prowlarr torrent indexers missing or disabled: {sorted(managed_torrent - enabled_names)!r}"
        )
else:
    if managed_torrent & enabled_names:
        raise SystemExit(
            f"validation failed: Prowlarr torrent indexers should be disabled: {sorted(managed_torrent & enabled_names)!r}"
        )

if "usenet" in selected_protocols:
    if not managed_usenet.issubset(enabled_names):
        raise SystemExit(
            f"validation failed: Prowlarr usenet indexers missing or disabled: {sorted(managed_usenet - enabled_names)!r}"
        )
else:
    if managed_usenet & enabled_names:
        raise SystemExit(
            f"validation failed: Prowlarr usenet indexers should be disabled: {sorted(managed_usenet & enabled_names)!r}"
        )

compatibility_path = Path("/var/lib/vmctl/download-unpack/compatibility.json")
media_roots = {
    Path(os.environ.get("RADARR_ROOT_FOLDER", "/data/media/movies")).resolve(),
    Path(os.environ.get("SONARR_ROOT_FOLDER", "/data/media/tv")).resolve(),
}
if compatibility_path.exists():
    compatibility = json.loads(compatibility_path.read_text(encoding="utf-8"))
    incompatible = {
        key: value
        for key, value in compatibility.items()
        if isinstance(value, dict)
        and not value.get("compatible", False)
        and any(
            str(value.get("path") or "").startswith(str(root))
            for root in media_roots
        )
    }
    if incompatible:
        preview = []
        for key, value in list(incompatible.items())[:10]:
            preview.append(
                " | ".join(
                    [
                        str(key),
                        f"path={value.get('path') or 'unknown'}",
                        f"container={value.get('container') or 'unknown'}",
                        f"video={','.join(value.get('videoCodecs') or []) or '-'}",
                        f"audio={','.join(value.get('audioCodecs') or []) or '-'}",
                        f"subtitles={','.join(value.get('subtitleCodecs') or []) or '-'}",
                        f"reason={value.get('reason') or 'unknown'}",
                    ]
                )
            )
        raise SystemExit("validation failed: unsupported media detected: " + "; ".join(preview))
PY
fi

if service_enabled "jellysearch"; then
  check_container_running "jellysearch"
  check_http_ok "http://127.0.0.1:5000/Items?SearchTerm=test&Limit=1" "jellysearch query"
  check_http_no_auth "http://127.0.0.1:5000/Items?SearchTerm=test&Limit=1" "jellysearch no-login query"
fi

if service_enabled "qbittorrent-vpn"; then
  check_container_running "qbittorrent-vpn"
  check_http_no_auth "http://127.0.0.1:${QBITTORRENT_WEBUI_PORT:-8080}/api/v2/app/version" "qbittorrent no-login api"
  python3 <<'PY'
import json
import os
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

download_protocols = [
    item.strip()
    for item in (os.environ.get("VMCTL_DOWNLOAD_PROTOCOLS") or "").split(",")
    if item.strip()
]
if not download_protocols:
    raise SystemExit("validation failed: VMCTL_DOWNLOAD_PROTOCOLS is empty")
selected_priorities = {protocol: index + 1 for index, protocol in enumerate(download_protocols)}
torrent_expected = "torrent" in selected_priorities

base = f"http://127.0.0.1:{os.environ.get('QBITTORRENT_WEBUI_PORT', '8080')}"
username = os.environ.get("QBITTORRENT_USERNAME", "admin")
password = os.environ.get("QBITTORRENT_PASSWORD", "adminadmin")
cookiejar = urllib.request.HTTPCookieProcessor()
opener = urllib.request.build_opener(cookiejar)
login = urllib.request.Request(
    f"{base}/api/v2/auth/login",
    data=f"username={username}&password={password}".encode(),
    headers={"Content-Type": "application/x-www-form-urlencoded"},
    method="POST",
)
with opener.open(login, timeout=20) as response:
    if response.status != 200:
        raise SystemExit(f"validation failed: qBittorrent login returned HTTP {response.status}")
cats = urllib.request.Request(f"{base}/api/v2/torrents/categories", method="GET")
with opener.open(cats, timeout=20) as response:
    payload = json.loads(response.read().decode("utf-8"))
expected_tv = os.environ.get("QBITTORRENT_CATEGORY_TV_PATH", "/data/torrents/tv")
expected_movies = os.environ.get("QBITTORRENT_CATEGORY_MOVIES_PATH", "/data/torrents/movies")

def qbit_effective_path(category: dict) -> str:
    raw = category.get("savePath") or category.get("save_path") or ""
    raw = str(raw).strip().rstrip("/")
    if not raw:
        return ""
    if raw.startswith("/"):
        return raw
    return f"{os.environ.get('QBITTORRENT_DOWNLOADS', '/data/torrents').rstrip('/')}/{raw}"

if qbit_effective_path(payload.get(os.environ.get("QBITTORRENT_CATEGORY_TV", "tv"), {})) != expected_tv:
    raise SystemExit("validation failed: qBittorrent TV category save path mismatch")
if qbit_effective_path(payload.get(os.environ.get("QBITTORRENT_CATEGORY_MOVIES", "movies"), {})) != expected_movies:
    raise SystemExit("validation failed: qBittorrent movies category save path mismatch")

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
expected = {
    "sonarr": {
        "hostname": "sonarr",
        "port": 8989,
        "root": os.environ.get("SONARR_ROOT_FOLDER", "/data/media/tv"),
        "profile": os.environ.get("SONARR_DEFAULT_QUALITY_PROFILE", "WEB-1080p"),
        "download_host": "gluetun" if (os.environ.get("MEDIA_VPN_ENABLED") or "").lower() == "true" else "qbittorrent-vpn",
        "category": os.environ.get("QBITTORRENT_CATEGORY_TV", "tv"),
        "category_field": "tvCategory",
    },
    "radarr": {
        "hostname": "radarr",
        "port": 7878,
        "root": os.environ.get("RADARR_ROOT_FOLDER", "/data/media/movies"),
        "profile": os.environ.get("RADARR_DEFAULT_QUALITY_PROFILE", "HD - 720p/1080p"),
        "download_host": "gluetun" if (os.environ.get("MEDIA_VPN_ENABLED") or "").lower() == "true" else "qbittorrent-vpn",
        "category": os.environ.get("QBITTORRENT_CATEGORY_MOVIES", "movies"),
        "category_field": "movieCategory",
    },
}

for app, status_url in (
    ("sonarr", "http://127.0.0.1:8989/api/v3/downloadclient"),
    ("radarr", "http://127.0.0.1:7878/api/v3/downloadclient"),
):
    key = (ET.parse(Path(os.environ.get("CONFIG_PATH") or "/opt/media/config") / app / "config.xml").getroot().findtext("ApiKey") or "").strip()
    req = urllib.request.Request(status_url, headers={"X-Api-Key": key}, method="GET")
    with urllib.request.urlopen(req, timeout=20) as response:
        clients = json.loads(response.read().decode("utf-8"))
    target = next((item for item in clients if item.get("name") == "qBittorrent"), None)
    if not torrent_expected:
        if target:
            raise SystemExit(f"validation failed: {app} unexpectedly still has qBittorrent configured")
        continue
    if not target:
        raise SystemExit(f"validation failed: {app} missing qBittorrent download client")
    if int(target.get("priority") or 0) != (2 if any(item.get("name") == "SABnzbd" for item in clients) else 1):
        raise SystemExit(
            f"validation failed: {app} qBittorrent priority mismatch: {target.get('priority')!r} != {(2 if any(item.get('name') == 'SABnzbd' for item in clients) else 1)!r}"
        )
    fields = {field.get("name"): field.get("value") for field in target.get("fields") or []}
    if fields.get("host") != expected[app]["download_host"]:
        raise SystemExit(
            f"validation failed: {app} qBittorrent host mismatch: {fields.get('host')!r} != {expected[app]['download_host']!r}"
        )
    if str(fields.get(expected[app]["category_field"]) or "") != expected[app]["category"]:
        raise SystemExit(
            f"validation failed: {app} qBittorrent category mismatch: {fields.get(expected[app]['category_field'])!r} != {expected[app]['category']!r}"
        )

PY
fi

if service_enabled "prowlarr"; then
  check_container_running "prowlarr"
  check_http_ok "http://127.0.0.1:9696/ping" "prowlarr ping"
  check_http_no_auth "http://127.0.0.1:9696/ping" "prowlarr no-login ping"
  python3 <<'PY'
import json
import os
import urllib.request

api_key = (os.environ.get("PROWLARR_API_KEY") or "").strip()
if not api_key:
    config_path = os.path.join(os.environ.get("CONFIG_PATH") or "/opt/media/config", "prowlarr", "config.xml")
    if os.path.exists(config_path):
        import xml.etree.ElementTree as ET

        api_key = (ET.parse(config_path).getroot().findtext("ApiKey") or "").strip()
if not api_key:
    raise SystemExit("validation failed: Prowlarr API key is missing")
req = urllib.request.Request(
    "http://127.0.0.1:9696/api/v1/indexerproxy",
    headers={"X-Api-Key": api_key},
    method="GET",
)
with urllib.request.urlopen(req, timeout=20) as response:
    proxies = json.loads(response.read().decode("utf-8"))
target = next((item for item in proxies if (item.get("name") or "").lower() == "flaresolverr"), None)
if not target:
    raise SystemExit("validation failed: Prowlarr missing FlareSolverr proxy")
fields = {field.get("name"): field.get("value") for field in target.get("fields") or []}
if not str(fields.get("host") or "").rstrip("/").endswith("flaresolverr:8191"):
    raise SystemExit(f"validation failed: Prowlarr FlareSolverr host mismatch: {fields.get('host')!r}")

download_protocols = [
    item.strip()
    for item in (os.environ.get("VMCTL_DOWNLOAD_PROTOCOLS") or "").split(",")
    if item.strip()
]
selected_protocols = set(download_protocols)
managed_torrent = {
    item.strip()
    for item in (os.environ.get("PROWLARR_BOOTSTRAP_INDEXERS_TORRENT") or "").split(",")
    if item.strip()
}
managed_usenet = {
    item.strip()
    for item in (os.environ.get("PROWLARR_BOOTSTRAP_INDEXERS_USENET") or "").split(",")
    if item.strip()
}
req = urllib.request.Request("http://127.0.0.1:9696/api/v1/indexer", headers={"X-Api-Key": api_key}, method="GET")
with urllib.request.urlopen(req, timeout=20) as response:
    indexers = json.loads(response.read().decode("utf-8"))
enabled_names = {item.get("name") for item in indexers if item.get("enable")}
req = urllib.request.Request("http://127.0.0.1:9696/api/v1/indexer/schema", headers={"X-Api-Key": api_key}, method="GET")
with urllib.request.urlopen(req, timeout=20) as response:
    schemas = json.loads(response.read().decode("utf-8"))
schema_names = {item.get("name") for item in schemas if item.get("name")}
managed_torrent = managed_torrent & schema_names
managed_usenet = managed_usenet & schema_names
if "torrent" in selected_protocols and not managed_torrent:
    raise SystemExit("validation failed: no schema-backed torrent indexers are available in Prowlarr")
if "usenet" in selected_protocols and not managed_usenet:
    raise SystemExit("validation failed: no schema-backed usenet indexers are available in Prowlarr")

if "torrent" in selected_protocols:
    if not managed_torrent.issubset(enabled_names):
        raise SystemExit(
            f"validation failed: Prowlarr torrent indexers missing or disabled: {sorted(managed_torrent - enabled_names)!r}"
        )
else:
    if managed_torrent & enabled_names:
        raise SystemExit(
            f"validation failed: Prowlarr torrent indexers should be disabled: {sorted(managed_torrent & enabled_names)!r}"
        )

if "usenet" in selected_protocols:
    if not managed_usenet.issubset(enabled_names):
        raise SystemExit(
            f"validation failed: Prowlarr usenet indexers missing or disabled: {sorted(managed_usenet - enabled_names)!r}"
        )
else:
    if managed_usenet & enabled_names:
        raise SystemExit(
            f"validation failed: Prowlarr usenet indexers should be disabled: {sorted(managed_usenet & enabled_names)!r}"
        )
PY
fi

if service_enabled "autobrr"; then
  check_container_running "autobrr"
  check_http_ok "http://127.0.0.1:7474/autobrr/" "autobrr ui"
  python3 <<'PY'
import os
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
config_path = config_root / "autobrr" / "config.toml"
if not config_path.exists():
    raise SystemExit(f"validation failed: missing autobrr config at {config_path}")
text = config_path.read_text(encoding="utf-8")
for token in ("baseUrl = \"/autobrr/\"", "baseUrlModeLegacy = false", "customDefinitions = \"/config/definitions\""):
    if token not in text:
        raise SystemExit(f"validation failed: autobrr config missing {token!r}")
PY
fi

if service_enabled "flaresolverr"; then
  check_container_running "flaresolverr"
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
  for key in tailscale; do
    url_file="http://127.0.0.1:80/jellio-manifest.${key}.url"
    manifest_url="$(curl -fsS "$url_file" | tr -d '\n\r')"
    if [[ -z "$manifest_url" ]]; then
      echo "validation failed: empty manifest URL in ${url_file}" >&2
      exit 1
    fi
    check_http_ok "$manifest_url" "jellio manifest (${key})"
  done

  python3 <<'PY'
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request

JELLYFIN_BASE = (os.environ.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/")
manifest_url = (os.environ.get("JELLIO_STREMIO_MANIFEST_URL_TAILSCALE") or "").strip()
ua = os.environ.get("TIZEN_STREMIO_USER_AGENT") or "Mozilla/5.0 (SMART-TV; Linux; Tizen 6.5) Stremio"
admin_user = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
admin_password = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")
if not manifest_url:
    raise SystemExit("validation failed: missing JELLIO_STREMIO_MANIFEST_URL_TAILSCALE")


def get_json(url: str, extra_headers: dict[str, str] | None = None):
    headers = {"User-Agent": ua, "Accept": "application/json", "Accept-Encoding": "identity"}
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(
        url,
        headers=headers,
        method="GET",
    )
    with urllib.request.urlopen(req, timeout=30) as response:
        content_type = response.headers.get("Content-Type", "")
        if response.status != 200:
            raise RuntimeError(f"{url} returned HTTP {response.status}")
        if "json" not in content_type.lower():
            raise RuntimeError(f"{url} returned non-json content type {content_type!r}")
        return json.loads(response.read().decode("utf-8"))


def addon_base(url: str) -> str:
    if not url.endswith("/manifest.json"):
        raise RuntimeError(f"manifest URL has unexpected shape: {url}")
    return url[: -len("/manifest.json")]


def jellyfin_token() -> str:
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="validate", DeviceId="vmctl-validate", Version="1.0"',
    }
    req = urllib.request.Request(
        f"{JELLYFIN_BASE}/Users/AuthenticateByName",
        data=json.dumps({"Username": admin_user, "Pw": admin_password}).encode("utf-8"),
        headers=headers,
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=20) as response:
        payload = json.loads(response.read().decode("utf-8"))
    return payload["AccessToken"]


manifest = get_json(manifest_url)
catalogs = manifest.get("catalogs") or []
if not catalogs:
    raise SystemExit("validation failed: Jellio manifest has no catalogs")

base = addon_base(manifest_url)
non_empty = []
first_movie_meta = None
for catalog in catalogs:
    raw_catalog_type = str(catalog.get("type") or "")
    catalog_type = urllib.parse.quote(raw_catalog_type, safe="")
    catalog_id = urllib.parse.quote(str(catalog.get("id") or ""), safe="")
    if not catalog_type or not catalog_id:
        continue
    url = f"{base}/catalog/{catalog_type}/{catalog_id}.json"
    payload = get_json(url)
    metas = payload.get("metas") or []
    if metas:
        non_empty.append(url)
        if raw_catalog_type == "movie":
            first_movie_meta = first_movie_meta or (raw_catalog_type, metas[0].get("id"))

if not non_empty:
    raise SystemExit("validation failed: Tizen-like Jellio catalog requests returned empty metas")

token = jellyfin_token()
headers = {
    "Authorization": 'MediaBrowser Client="vmctl", Device="validate", DeviceId="vmctl-validate", Version="1.0"',
    "X-Emby-Token": token,
}
with urllib.request.urlopen(
    urllib.request.Request(f"{JELLYFIN_BASE}/Library/VirtualFolders", headers=headers, method="GET"),
    timeout=20,
) as response:
    folders = json.loads(response.read().decode("utf-8"))
expected_locations = {
    "movies": "/data/media/movies",
    "tv": "/data/media/tv",
}
for expected_name, expected_path in expected_locations.items():
    match = None
    for folder in folders:
        if (folder.get("Name") or "").strip().lower() == expected_name:
            match = folder
            break
    if not match:
        raise SystemExit(f"validation failed: missing Jellyfin library {expected_name!r}")
    locations = [str(location).rstrip("/") for location in (match.get("Locations") or []) if str(location).strip()]
    if locations != [expected_path]:
        raise SystemExit(
            f"validation failed: Jellyfin library {expected_name} locations mismatch: {locations!r} != {[expected_path]!r}"
        )

if first_movie_meta and first_movie_meta[1]:
    stream_type = urllib.parse.quote(str(first_movie_meta[0]), safe="")
    stream_id = urllib.parse.quote(str(first_movie_meta[1]), safe="")
    stream_url = f"{base}/stream/{stream_type}/{stream_id}.json"
    try:
        streams = get_json(
            stream_url,
            {
                "X-Emby-Token": token,
                "X-MediaBrowser-Token": token,
            },
        ).get("streams") or []
        if streams:
            url = streams[0].get("url") or streams[0].get("externalUrl") or ""
            parsed = urllib.parse.urlparse(url)
            if parsed.path.lower().startswith("/videos/") and parsed.path.lower().endswith("/stream"):
                internal_url = f"{JELLYFIN_BASE}{parsed.path}"
                if parsed.query:
                    internal_url = f"{internal_url}?{parsed.query}"
                candidate_urls = [url]
                if internal_url != url:
                    candidate_urls.append(internal_url)
                stream_headers = [
                    {"User-Agent": ua, "Accept-Encoding": "identity"},
                    {
                        "User-Agent": ua,
                        "Accept-Encoding": "identity",
                        "X-Emby-Token": token,
                        "X-MediaBrowser-Token": token,
                    },
                ]
                last_error = None
                for _ in range(12):
                    for candidate in candidate_urls:
                        for headers in stream_headers:
                            req = urllib.request.Request(candidate, headers=headers, method="GET")
                            try:
                                with urllib.request.urlopen(req, timeout=30) as response:
                                    preview = response.read(7).decode("utf-8", errors="ignore")
                                    content_type = response.headers.get("Content-Type", "").lower()
                                    if response.status == 200 and "#EXTM3U" in preview and "mpegurl" in content_type:
                                        last_error = None
                                        break
                                    last_error = RuntimeError(
                                        f"Tizen stream did not return HLS playlist: HTTP {response.status}, {content_type!r}"
                                    )
                            except (urllib.error.HTTPError, urllib.error.URLError) as exc:
                                last_error = exc
                        if last_error is None:
                            break
                    if last_error is None:
                        break
                    time.sleep(5)
                if last_error is not None:
                    raise last_error
    except (urllib.error.HTTPError, urllib.error.URLError, RuntimeError) as exc:
        raise SystemExit(f"validation failed: Tizen-like stream validation failed: {exc}")
else:
    print("warning: Tizen-like playback validation skipped because no movie catalog item is available")
PY
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
