#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"
COMPOSE_FILE="$STACK_DIR/docker-compose.yml"

if [[ -f "$ENV_FILE" ]]; then
  set -a
  . "$ENV_FILE"
  set +a
fi

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-media}"
docker_compose() {
  docker compose -p "$COMPOSE_PROJECT_NAME" --project-directory "$STACK_DIR" --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"
}

CONFIG_ROOT="${CONFIG_PATH:-/opt/media/config}"
BASE_URL_VALUE=""
JELLYFIN_NETWORK_XML="$CONFIG_ROOT/jellyfin/network.xml"
mkdir -p "$(dirname "$JELLYFIN_NETWORK_XML")"
export BASE_URL_VALUE
export JELLYFIN_NETWORK_XML
export JELLYFIN_ENV_FILE="$ENV_FILE"

jellyfin_base_updated="$(
python3 <<'PY'
import os
import xml.etree.ElementTree as ET

xml_path = os.environ["JELLYFIN_NETWORK_XML"]
base_url = (os.environ.get("BASE_URL_VALUE") or "").strip()
if not base_url.startswith("/"):
    base_url = f"/{base_url}"
if base_url == "/":
    base_url = ""

root = None
if os.path.exists(xml_path):
    root = ET.parse(xml_path).getroot()
else:
    root = ET.Element("NetworkConfiguration")

node = root.find("BaseUrl")
if node is None:
    node = ET.SubElement(root, "BaseUrl")

current = (node.text or "").strip()
if current == base_url:
    print("0")
else:
    node.text = base_url
    ET.ElementTree(root).write(xml_path, encoding="utf-8", xml_declaration=True)
    print("1")
PY
)"

if [[ "$jellyfin_base_updated" == "1" ]]; then
  docker_compose up -d jellyfin
  docker_compose restart jellyfin
fi

python3 <<'PY'
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

base = (os.environ.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/")
user = os.environ.get("JELLYFIN_ADMIN_USER") or "admin"
password = os.environ.get("JELLYFIN_ADMIN_PASSWORD") or ""
base_url = ""
auto_login_user = (os.environ.get("JELLYFIN_AUTOLOGIN_USER") or "media").strip() or "media"
env_file = Path(os.environ.get("JELLYFIN_ENV_FILE") or "/opt/media/.env")


def call(method, path, payload=None, token=None, allow=(200, 204)):
    data = None
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="bootstrap", DeviceId="vmctl", Version="1.0"',
    }
    if token:
        headers["X-Emby-Token"] = token
    if payload is not None:
        data = json.dumps(payload).encode()
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as response:
            body = response.read().decode()
            if body:
                return json.loads(body)
            return None
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        raise


def ensure_library(name, path, collection_type, token):
    current = call("GET", "/Library/VirtualFolders", token=token, allow=(200, 204)) or []
    if any((item.get("Name") or "").lower() == name.lower() for item in current):
        return
    query = urllib.parse.urlencode(
        {
            "name": name,
            "collectionType": collection_type,
            "paths": path,
            "refreshLibrary": "true",
        },
        doseq=True,
    )
    call(
        "POST",
        f"/Library/VirtualFolders?{query}",
        {"LibraryOptions": {"Enabled": True, "PathInfos": [{"Path": path}]}},
        token=token,
        allow=(200, 204, 400),
    )


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


def ensure_user(username: str, token: str) -> str:
    users = call("GET", "/Users", token=token, allow=(200, 204)) or []
    for item in users:
        if (item.get("Name") or "").lower() == username.lower():
            return item["Id"]
    created = call("POST", "/Users/New", {"Name": username}, token=token, allow=(200, 204, 400)) or {}
    if created.get("Id"):
        return created["Id"]
    users = call("GET", "/Users", token=token, allow=(200, 204)) or []
    for item in users:
        if (item.get("Name") or "").lower() == username.lower():
            return item["Id"]
    raise RuntimeError(f"failed to create Jellyfin user {username}")


def ensure_blank_password(user_id: str, token: str) -> None:
    call(
        "POST",
        f"/Users/{user_id}/Password",
        {"CurrentPw": "", "NewPw": "", "ResetPassword": False},
        token=token,
        allow=(200, 204, 400),
    )


def try_call(method, path, payload=None, token=None):
    try:
        return call(method, path, payload, token, allow=(200, 204))
    except urllib.error.HTTPError:
        return None


for _ in range(90):
    try:
        call("GET", "/System/Info/Public")
        break
    except Exception:
        time.sleep(2)
else:
    raise RuntimeError(f"Jellyfin did not become ready at {base}")

try:
    call("POST", "/Startup/Configuration", {
        "UICulture": "en-US",
        "MetadataCountryCode": "US",
        "PreferredMetadataLanguage": "en",
    }, allow=(200, 204, 400))
    if password:
        call("POST", "/Startup/User", {"Name": user, "Password": password}, allow=(200, 204, 400))
    call("POST", "/Startup/RemoteAccess", {
        "EnableRemoteAccess": True,
        "EnableAutomaticPortMapping": False,
    }, allow=(200, 204, 400))
    call("POST", "/Startup/Complete", allow=(200, 204, 400))
except urllib.error.HTTPError:
    pass

token = None
auth = None
if password:
    auth = try_call("POST", "/Users/AuthenticateByName", {"Username": user, "Pw": password})
if not auth:
    startup_user = try_call("GET", "/Startup/User")
    existing_user = startup_user.get("Name") if startup_user else None
    if existing_user:
        auth = try_call("POST", "/Users/AuthenticateByName", {"Username": existing_user, "Pw": ""})
token = auth.get("AccessToken") if auth else None

if token:
    info = try_call("GET", "/System/Info/Public", token=token) or {}
    server_id = (info.get("Id") or "").strip()
    network = try_call("GET", "/System/Configuration/network", token=token) or {}
    if not network.get("EnablePublishedServerUriByRequest"):
        network["EnablePublishedServerUriByRequest"] = True
        call("POST", "/System/Configuration/network", network, token=token, allow=(200, 204, 400))

    config = try_call("GET", "/System/Configuration", token=token) or {}
    auto_user_id = ensure_user(auto_login_user, token)
    ensure_blank_password(auto_user_id, token)
    auto_auth = try_call("POST", "/Users/AuthenticateByName", {"Username": auto_login_user, "Pw": ""})
    auto_token = (auto_auth or {}).get("AccessToken") or token

    if config.get("AutoLoginUserId") != auto_user_id:
        config["AutoLoginUserId"] = auto_user_id
        call("POST", "/System/Configuration", config, token=token, allow=(200, 204, 400))

    if config.get("BaseUrl") != base_url:
        config["BaseUrl"] = base_url
        call("POST", "/System/Configuration", config, token=token, allow=(200, 204, 400))
    for name, path, collection_type in [
        ("Movies", "/media/movies", "movies"),
        ("TV", "/media/tv", "tvshows"),
    ]:
        os.makedirs(path, exist_ok=True)
        ensure_library(name, path, collection_type, token)
    call("POST", "/Library/Refresh", token=token, allow=(200, 204, 400))
    set_env_value(env_file, "JELLYFIN_AUTOLOGIN_USER", auto_login_user)
    set_env_value(env_file, "JELLYFIN_AUTO_AUTH_TOKEN", auto_token)
    autologin_params = urllib.parse.urlencode(
        {
            "serverid": server_id,
            "serverId": server_id,
            "userid": auto_user_id,
            "userId": auto_user_id,
            "api_key": auto_token,
            "accessToken": auto_token,
        }
    )
    default_public_base = f"http://{os.environ.get('VMCTL_RESOURCE_NAME', 'media-stack')}"
    autologin_base = (os.environ.get("VMCTL_HTTP_BASE_URL_SHORT") or default_public_base).rstrip("/")
    autologin_url = f"{autologin_base}:8097/web/#/home.html?{autologin_params}"
    set_env_value(env_file, "JELLYFIN_AUTOLOGIN_URL", autologin_url)
    ui_index = Path("/opt/media/config/caddy/ui-index")
    ui_index.mkdir(parents=True, exist_ok=True)
    (ui_index / "jellyfin-autologin.url").write_text(autologin_url + "\n", encoding="utf-8")
PY

if docker_compose config --services | grep -qx "caddy"; then
  set -a
  . "$ENV_FILE"
  set +a
  docker_compose up -d --force-recreate caddy
fi
