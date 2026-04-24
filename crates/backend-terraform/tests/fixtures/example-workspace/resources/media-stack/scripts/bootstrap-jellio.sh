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
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-media}"
docker_compose() {
  docker compose -p "$COMPOSE_PROJECT_NAME" --project-directory "$STACK_DIR" --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"
}

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

python3 - "$ENV_FILE" <<'PY'
import base64
import json
import os
import secrets
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

PLUGIN_ID = "e874be83fe364568abacf5ce0574b409"
env_file = Path(os.sys.argv[1])

api_base_url = (os.environ.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/")
host_server_name = (os.environ.get("VMCTL_RESOURCE_NAME") or "media-stack").strip()
admin_user = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
admin_password = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")
stremio_user = (os.environ.get("JELLYFIN_STREMIO_USER") or "stremio").strip()
stremio_password = (os.environ.get("JELLYFIN_STREMIO_PASSWORD") or "").strip()
if not stremio_password:
    stremio_password = secrets.token_hex(20)
seerr_api_key = (os.environ.get("JELLYSEERR_API_KEY") or "").strip()
seerr_url = (os.environ.get("JELLYSEERR_INTERNAL_URL") or "http://jellyseerr:5055").rstrip("/")
cloudflare_base = (os.environ.get("CLOUDFLARE_PUBLIC_BASE_URL") or "").strip().rstrip("/")
cloudflare_token = (os.environ.get("CLOUDFLARED_TOKEN") or "").strip()


def set_env_value(path: Path, key: str, value: str) -> None:
    lines = path.read_text(encoding="utf-8").splitlines() if path.exists() else []
    out = []
    seen = False
    for line in lines:
        if line.startswith(f"{key}="):
            out.append(f"{key}={value}")
            seen = True
        else:
            out.append(line)
    if not seen:
        out.append(f"{key}={value}")
    path.write_text("\n".join(out).rstrip() + "\n", encoding="utf-8")


def request_json(method: str, path: str, payload=None, token=None, allow=(200, 204)):
    url = f"{api_base_url}{path}"
    data = None
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="bootstrap", DeviceId="vmctl-jellio", Version="1.0"',
    }
    if token:
        headers["X-Emby-Token"] = token
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as response:
            raw = response.read().decode("utf-8")
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        raise


def authenticate(username: str, password: str):
    return request_json(
        "POST",
        "/Users/AuthenticateByName",
        {"Username": username, "Pw": password},
        allow=(),
    )


def wait_for_jellyfin() -> None:
    started = time.time()
    while time.time() - started < 240:
        try:
            request_json("GET", "/System/Info/Public", allow=())
            return
        except Exception:
            time.sleep(2)
    raise RuntimeError(f"jellyfin did not become ready at {api_base_url}")


def ensure_user(admin_token: str) -> str:
    users = request_json("GET", "/Users", token=admin_token, allow=()) or []
    for user in users:
        if (user.get("Name") or "").lower() == stremio_user.lower():
            return user["Id"]
    created = request_json(
        "POST",
        "/Users/New",
        {"Name": stremio_user},
        token=admin_token,
        allow=(),
    )
    if created and created.get("Id"):
        return created["Id"]
    users = request_json("GET", "/Users", token=admin_token, allow=()) or []
    for user in users:
        if (user.get("Name") or "").lower() == stremio_user.lower():
            return user["Id"]
    raise RuntimeError("unable to create stremio Jellyfin user")


def ensure_user_password(admin_token: str, user_id: str) -> None:
    try:
        authenticate(stremio_user, stremio_password)
        return
    except Exception:
        pass
    request_json(
        "POST",
        f"/Users/{user_id}/Password",
        {"CurrentPw": "", "NewPw": stremio_password, "ResetPassword": False},
        token=admin_token,
        allow=(200, 204, 400),
    )
    authenticate(stremio_user, stremio_password)


def preferred_libraries(admin_token: str):
    folders = request_json("GET", "/Library/VirtualFolders", token=admin_token, allow=()) or []
    preferred = []
    fallback = []
    for folder in folders:
        item_id = (folder.get("ItemId") or "").strip()
        if not item_id:
            continue
        fallback.append(item_id)
        name = (folder.get("Name") or "").strip().lower()
        ctype = (folder.get("CollectionType") or "").strip().lower()
        if ctype in {"movies", "tvshows"} or name in {"movies", "tv"}:
            preferred.append(item_id)
    return preferred or fallback


def b64url(payload: dict) -> str:
    raw = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    return base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")


def hyphenate_guid(value: str) -> str:
    compact = (value or "").replace("-", "").strip()
    if len(compact) != 32:
        return value
    return f"{compact[:8]}-{compact[8:12]}-{compact[12:16]}-{compact[16:20]}-{compact[20:]}"


def tailscale_dns_name() -> str:
    try:
        out = subprocess.check_output(["tailscale", "status", "--json"], text=True)
        data = json.loads(out)
        name = (data.get("Self", {}).get("DNSName") or "").rstrip(".")
        return name
    except Exception:
        return ""


wait_for_jellyfin()
admin_auth = authenticate(admin_user, admin_password)
admin_token = admin_auth["AccessToken"]

stremio_user_id = ensure_user(admin_token)
ensure_user_password(admin_token, stremio_user_id)
stremio_auth = authenticate(stremio_user, stremio_password)
stremio_token = stremio_auth["AccessToken"]

libraries = preferred_libraries(admin_token)
if not libraries:
    raise RuntimeError("no Jellyfin libraries available for Jellio manifest generation")

plugin_config = None
for _ in range(120):
    try:
        plugin_config = request_json(
            "GET",
            f"/Plugins/{PLUGIN_ID}/Configuration",
            token=admin_token,
            allow=(),
        )
        if plugin_config is not None:
            break
    except urllib.error.HTTPError as err:
        if err.code != 404:
            raise
    time.sleep(2)
if plugin_config is None:
    raise RuntimeError("jellio plugin configuration endpoint unavailable")

plugin_config["SelectedLibraries"] = libraries
plugin_config["JellyseerrEnabled"] = bool(seerr_api_key)
if seerr_api_key:
    plugin_config["JellyseerrUrl"] = seerr_url
    plugin_config["JellyseerrApiKey"] = seerr_api_key
else:
    plugin_config["JellyseerrUrl"] = ""
    plugin_config["JellyseerrApiKey"] = ""

request_json(
    "POST",
    f"/Plugins/{PLUGIN_ID}/Configuration",
    plugin_config,
    token=admin_token,
    allow=(),
)

tail_dns = tailscale_dns_name()
tailscale_base = f"https://{tail_dns}" if tail_dns else ""
cloudflare_enabled = bool(cloudflare_base and cloudflare_token)


def make_manifest(addon_base: str) -> str:
    jellyfin_public_base = f"{addon_base.rstrip('/')}/jf"
    payload = {
        "ServerName": host_server_name,
        "AuthToken": stremio_token,
        "LibrariesGuids": [hyphenate_guid(lib) for lib in libraries],
        "PublicBaseUrl": jellyfin_public_base,
    }
    if seerr_api_key:
        payload["JellyseerrEnabled"] = True
        payload["JellyseerrUrl"] = seerr_url
        payload["JellyseerrApiKey"] = seerr_api_key
    encoded = b64url(payload)
    return f"{addon_base.rstrip('/')}/jellio/{encoded}/manifest.json"

def make_manifest_with_config(addon_base: str) -> tuple[str, str]:
    jellyfin_public_base = f"{addon_base.rstrip('/')}/jf"
    payload = {
        "ServerName": host_server_name,
        "AuthToken": stremio_token,
        "LibrariesGuids": [hyphenate_guid(lib) for lib in libraries],
        "PublicBaseUrl": jellyfin_public_base,
    }
    if seerr_api_key:
        payload["JellyseerrEnabled"] = True
        payload["JellyseerrUrl"] = seerr_url
        payload["JellyseerrApiKey"] = seerr_api_key
    encoded = b64url(payload)
    return encoded, f"{addon_base.rstrip('/')}/jellio/{encoded}/manifest.json"


tailscale_b64, tailscale_manifest = make_manifest_with_config(tailscale_base) if tailscale_base else ("", "")
cloudflare_b64, cloudflare_manifest = make_manifest_with_config(cloudflare_base) if cloudflare_enabled else ("", "")

# Stremio clients appear to have a relatively strict URL length limit for manifest URLs.
# The generated /jellio/<b64>/manifest.json URL can exceed it (especially for Tailscale HTTPS).
# To keep manifests addable, we expose short aliases that are rewritten by Caddy to the full path.
tailscale_manifest_alias = f"{tailscale_base.rstrip('/')}/jellio-tailscale/manifest.json" if tailscale_base else ""
cloudflare_manifest_alias = f"{cloudflare_base.rstrip('/')}/jellio-cloudflare/manifest.json" if cloudflare_enabled else ""

set_env_value(env_file, "JELLYFIN_STREMIO_PASSWORD", stremio_password)
set_env_value(env_file, "JELLYFIN_STREMIO_AUTH_TOKEN", stremio_token)
# Config selectors used by Caddy alias routes.
set_env_value(env_file, "JELLIO_CONFIG_B64_TAILSCALE", tailscale_b64)
set_env_value(env_file, "JELLIO_CONFIG_B64_CLOUDFLARE", cloudflare_b64)
set_env_value(env_file, "JELLIO_STREMIO_MANIFEST_URL_TAILSCALE", tailscale_manifest)
set_env_value(env_file, "JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE", cloudflare_manifest)

ui_index = Path("/opt/media/config/caddy/ui-index")
ui_index.mkdir(parents=True, exist_ok=True)
(ui_index / "jellio-manifest.tailscale.url").write_text((tailscale_manifest_alias or "") + "\n", encoding="utf-8")
(ui_index / "jellio-manifest.cloudflare.url").write_text((cloudflare_manifest_alias or "") + "\n", encoding="utf-8")
PY

# Caddy expands env vars at load time, so we must recreate it after writing JELLIO_CONFIG_B64_*.
if docker_compose config --services 2>/dev/null | grep -qx "caddy"; then
  docker_compose up -d --force-recreate caddy
fi
