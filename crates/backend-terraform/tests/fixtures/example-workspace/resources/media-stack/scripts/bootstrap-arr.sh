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
export CONFIG_ROOT
ARR_RESTART_MARKER="/tmp/vmctl-arr-restart.list"
export ARR_RESTART_MARKER

python3 <<'PY'
import os
import time
import xml.etree.ElementTree as ET

config_root = os.environ.get("CONFIG_ROOT", "/opt/media/config")
apps = {
    "sonarr": "",
    "radarr": "",
    "prowlarr": "",
}

changed = []
for app, base in apps.items():
    xml_path = os.path.join(config_root, app, "config.xml")
    for _ in range(120):
        if os.path.exists(xml_path):
            break
        time.sleep(1)
    if not os.path.exists(xml_path):
        continue
    normalized = (base or "").strip()
    if normalized and not normalized.startswith("/"):
        normalized = f"/{normalized}"
    if normalized == "/":
        normalized = ""

    root = ET.parse(xml_path).getroot()
    node = root.find("UrlBase")
    if node is None:
        node = ET.SubElement(root, "UrlBase")
    current = (node.text or "").strip()
    dirty = False
    if current != normalized:
        node.text = normalized
        dirty = True
        changed.append(app)

    auth_settings = {
        "AuthenticationMethod": "External",
        "AuthenticationRequired": "DisabledForLocalAddresses",
    }
    auth_changed = False
    for key, value in auth_settings.items():
        auth_node = root.find(key)
        if auth_node is None:
            auth_node = ET.SubElement(root, key)
        current_auth = (auth_node.text or "").strip()
        if current_auth != value:
            auth_node.text = value
            auth_changed = True
    if auth_changed:
        dirty = True
    if dirty:
        ET.ElementTree(root).write(xml_path, encoding="utf-8", xml_declaration=True)
        if app not in changed:
            changed.append(app)

marker = os.environ.get("ARR_RESTART_MARKER", "/tmp/vmctl-arr-restart.list")
with open(marker, "w", encoding="utf-8") as handle:
    for app in changed:
        handle.write(f"{app}\n")
PY

if [[ -s "$ARR_RESTART_MARKER" ]]; then
  mapfile -t arr_services < "$ARR_RESTART_MARKER"
  if ((${#arr_services[@]} > 0)); then
    docker_compose up -d "${arr_services[@]}"
    docker_compose restart "${arr_services[@]}"
  fi
fi

python3 <<'PY'
import json
import os
import time
import urllib.parse
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET

CONFIG_PATH = os.environ.get("CONFIG_PATH", "/opt/media/config")
VPN_ENABLED = os.environ.get("MEDIA_VPN_ENABLED", "").lower() == "true"
QBIT_HOST = "gluetun" if VPN_ENABLED else "qbittorrent-vpn"
QBIT_PORT = int(os.environ.get("QBITTORRENT_WEBUI_PORT", "8080"))
QBIT_USERNAME = os.environ.get("QBITTORRENT_USERNAME", "admin")
QBIT_PASSWORD = os.environ.get("QBITTORRENT_PASSWORD", "adminadmin")


def read_api_key(app):
    path = os.path.join(CONFIG_PATH, app, "config.xml")
    for _ in range(60):
        if os.path.exists(path):
            root = ET.parse(path).getroot()
            key = root.findtext("ApiKey")
            if key:
                return key
        time.sleep(2)
    raise RuntimeError(f"{app} API key was not created at {path}")


def request(method, url, api_key, payload=None, allow=(400, 409)):
    data = None
    headers = {"X-Api-Key": api_key}
    if payload is not None:
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            body = response.read()
            return json.loads(body.decode() or "null")
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        raise


def parse_root_and_base(url):
    parsed = urllib.parse.urlparse(url)
    root = f"{parsed.scheme}://{parsed.netloc}"
    base = parsed.path.rstrip("/")
    if base == "/":
        base = ""
    return root, base


def detect_api_base(name, configured_url, api_key, api_prefix, expected_base=""):
    root, configured_base = parse_root_and_base(configured_url)
    candidates = []
    for base in [configured_base, expected_base, ""]:
        candidate = (base or "").rstrip("/")
        if candidate == "/":
            candidate = ""
        if candidate not in candidates:
            candidates.append(candidate)
    bases = candidates
    for base in bases:
        for _ in range(180):
            try:
                request("GET", f"{root}{base}{api_prefix}/system/status", api_key, allow=())
                return root, base
            except Exception:
                time.sleep(2)
    raise RuntimeError(f"{name} did not become ready at {configured_url}")


def app_base(url, discovered_base):
    root, _ = parse_root_and_base(url)
    return f"{root}{discovered_base}"


def ensure_prowlarr_app_sync(prowlarr_url, prowlarr_key, arr_name, arr_url, arr_key):
    apps = request("GET", f"{prowlarr_url}/api/v1/applications", prowlarr_key, allow=()) or []
    for app in apps:
        if app.get("name") != arr_name:
            continue
        updated = dict(app)
        fields = updated.get("fields", [])
        desired = {
            "prowlarrUrl": os.environ.get("PROWLARR_INTERNAL_URL", "http://prowlarr:9696"),
            "baseUrl": arr_url,
            "apiKey": arr_key,
            "syncCategories": [5000, 5030, 5040],
        }
        changed = False
        for field in fields:
            name = field.get("name")
            if name in desired and field.get("value") != desired[name]:
                field["value"] = desired[name]
                changed = True
        if changed:
            request("PUT", f"{prowlarr_url}/api/v1/applications/{app['id']}", prowlarr_key, updated)
        return
    payload = {
        "name": arr_name,
        "syncLevel": "fullSync",
        "implementation": arr_name,
        "configContract": f"{arr_name}Settings",
        "enable": True,
        "fields": [
            {"name": "prowlarrUrl", "value": os.environ.get("PROWLARR_INTERNAL_URL", "http://prowlarr:9696")},
            {"name": "baseUrl", "value": arr_url},
            {"name": "apiKey", "value": arr_key},
            {"name": "syncCategories", "value": [5000, 5030, 5040]},
        ],
    }
    request("POST", f"{prowlarr_url}/api/v1/applications", prowlarr_key, payload, allow=(400, 409))


def ensure_default_indexers(prowlarr_url, prowlarr_key):
    existing = request("GET", f"{prowlarr_url}/api/v1/indexer", prowlarr_key, allow=()) or []
    existing_names = {item.get("name") for item in existing if item.get("name")}

    schemas = request("GET", f"{prowlarr_url}/api/v1/indexer/schema", prowlarr_key, allow=()) or []
    profiles = request("GET", f"{prowlarr_url}/api/v1/appProfile", prowlarr_key, allow=()) or []
    profile_id = profiles[0]["id"] if profiles else 1

    preferred = ["Nyaa.si", "1337x", "EZTV", "The Cowboy TV", "YTS"]
    selected = [
        schema for schema in schemas
        if schema.get("name") in preferred and schema.get("name") not in existing_names
    ]
    if not selected:
        return

    for schema in selected:
        candidate = dict(schema)
        candidate["enable"] = True
        candidate["priority"] = 25
        candidate["appProfileId"] = profile_id
        request("POST", f"{prowlarr_url}/api/v1/indexer", prowlarr_key, candidate, allow=(400, 409))


def ensure_indexer_sync_clients(prowlarr_url, prowlarr_key):
    request("POST", f"{prowlarr_url}/api/v1/indexer/sync", prowlarr_key, {}, allow=(400, 404, 405, 409))


def ensure_root_folder(url, api_key, path):
    existing = request("GET", f"{url}/api/v3/rootfolder", api_key) or []
    if any(item.get("path") == path for item in existing):
        return
    os.makedirs(path, exist_ok=True)
    request("POST", f"{url}/api/v3/rootfolder", api_key, {"path": path})


def ensure_qbittorrent_download_client(app, url, api_key, category):
    if app == "sonarr":
        category_field = "tvCategory"
        recent_priority_field = "recentTvPriority"
        older_priority_field = "olderTvPriority"
    elif app == "radarr":
        category_field = "movieCategory"
        recent_priority_field = "recentMoviePriority"
        older_priority_field = "olderMoviePriority"
    else:
        raise RuntimeError(f"unsupported app for qBittorrent download client: {app}")

    desired = {
        "host": QBIT_HOST,
        "port": QBIT_PORT,
        "urlBase": "",
        "username": QBIT_USERNAME,
        "password": QBIT_PASSWORD,
        category_field: category,
    }
    comparable_desired = {key: value for key, value in desired.items() if key != "password"}

    def current_client():
        existing = request("GET", f"{url}/api/v3/downloadclient", api_key, allow=()) or []
        return next((item for item in existing if item.get("name") == "qBittorrent"), None)

    for _ in range(60):
        item = current_client()
        if item is not None:
            updated = dict(item)
            fields = updated.get("fields", [])
            current = {field.get("name"): field.get("value") for field in fields}
            if all(current.get(name) == value for name, value in comparable_desired.items()):
                return
            for field in fields:
                name = field.get("name")
                if name in desired:
                    field["value"] = desired[name]
            try:
                request("PUT", f"{url}/api/v3/downloadclient/{item['id']}", api_key, updated, allow=())
            except urllib.error.HTTPError:
                time.sleep(2)
                continue
        else:
            fields = [
                {"name": "host", "value": QBIT_HOST},
                {"name": "port", "value": QBIT_PORT},
                {"name": "urlBase", "value": ""},
                {"name": "username", "value": QBIT_USERNAME},
                {"name": "password", "value": QBIT_PASSWORD},
                {"name": category_field, "value": category},
                {"name": recent_priority_field, "value": 0},
                {"name": older_priority_field, "value": 0},
                {"name": "initialState", "value": 0},
            ]
            payload = {
                "enable": True,
                "protocol": "torrent",
                "priority": 1,
                "removeCompletedDownloads": True,
                "removeFailedDownloads": True,
                "name": "qBittorrent",
                "implementation": "QBittorrent",
                "configContract": "QBittorrentSettings",
                "fields": fields,
            }
            try:
                request("POST", f"{url}/api/v3/downloadclient", api_key, payload, allow=(409,))
            except urllib.error.HTTPError:
                time.sleep(2)
                continue

        refreshed = current_client()
        if refreshed is not None:
            refreshed_fields = {field.get("name"): field.get("value") for field in refreshed.get("fields") or []}
            if all(refreshed_fields.get(name) == value for name, value in comparable_desired.items()):
                return
        time.sleep(2)

    raise RuntimeError(f"{app} qBittorrent download client did not converge")


apps = {
    "sonarr": {
        "url": os.environ.get("SONARR_URL", "http://sonarr:8989"),
        "internal_url": os.environ.get("SONARR_INTERNAL_URL", "http://sonarr:8989"),
        "base": "",
        "root": "/media/tv",
        "category": "tv",
    },
    "radarr": {
        "url": os.environ.get("RADARR_URL", "http://radarr:7878"),
        "internal_url": os.environ.get("RADARR_INTERNAL_URL", "http://radarr:7878"),
        "base": "",
        "root": "/media/movies",
        "category": "movies",
    },
}

resolved = {}
for app, cfg in apps.items():
    key = read_api_key(app)
    root, discovered_base = detect_api_base(app, cfg["url"], key, "/api/v3", cfg["base"])
    api_url = f"{root}{discovered_base}"
    ensure_root_folder(api_url, key, cfg["root"])
    ensure_qbittorrent_download_client(app, api_url, key, cfg["category"])
    resolved[app] = {"url": app_base(cfg["internal_url"], cfg["base"]), "key": key}

prowlarr_url = os.environ.get("PROWLARR_URL", "http://localhost:9696")
prowlarr_base = ""
prowlarr_key = read_api_key("prowlarr")
prowlarr_root, prowlarr_discovered_base = detect_api_base("prowlarr", prowlarr_url, prowlarr_key, "/api/v1", prowlarr_base)
prowlarr_api = f"{prowlarr_root}{prowlarr_discovered_base}"
ensure_default_indexers(prowlarr_api, prowlarr_key)

for app_name, values in resolved.items():
    ensure_prowlarr_app_sync(
        prowlarr_api,
        prowlarr_key,
        app_name.capitalize(),
        values["url"],
        values["key"],
    )
ensure_indexer_sync_clients(prowlarr_api, prowlarr_key)
PY
