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
from pathlib import Path

CONFIG_PATH = os.environ.get("CONFIG_PATH", "/opt/media/config")
VPN_ENABLED = os.environ.get("MEDIA_VPN_ENABLED", "").lower() == "true"
QBIT_HOST = "gluetun" if VPN_ENABLED else "qbittorrent-vpn"
QBIT_PORT = int(os.environ.get("QBITTORRENT_WEBUI_PORT", "8080"))
QBIT_USERNAME = os.environ.get("QBITTORRENT_USERNAME", "admin")
QBIT_PASSWORD = os.environ.get("QBITTORRENT_PASSWORD", "adminadmin")
QBIT_CATEGORY_TV = os.environ.get("QBITTORRENT_CATEGORY_TV", "tv")
QBIT_CATEGORY_MOVIES = os.environ.get("QBITTORRENT_CATEGORY_MOVIES", "movies")
QBIT_CATEGORY_TV_PATH = os.environ.get("QBITTORRENT_CATEGORY_TV_PATH", "/data/torrents/tv")
QBIT_CATEGORY_MOVIES_PATH = os.environ.get("QBITTORRENT_CATEGORY_MOVIES_PATH", "/data/torrents/movies")
SAB_URL = os.environ.get("SABNZBD_INTERNAL_URL", "http://sabnzbd:8080")
SAB_USERNAME = os.environ.get("SABNZBD_USERNAME", "admin")
SAB_PASSWORD = os.environ.get("SABNZBD_PASSWORD", "")
SONARR_ROOT_FOLDER = os.environ.get("SONARR_ROOT_FOLDER", "/data/media/tv")
RADARR_ROOT_FOLDER = os.environ.get("RADARR_ROOT_FOLDER", "/data/media/movies")
SONARR_PROWLARR_CATEGORIES = [int(item.strip()) for item in (os.environ.get("SONARR_PROWLARR_CATEGORIES", "5000,5030,5040").split(",")) if item.strip()]
RADARR_PROWLARR_CATEGORIES = [int(item.strip()) for item in (os.environ.get("RADARR_PROWLARR_CATEGORIES", "2000,2010,2020,2030,2040,2045,2060").split(",")) if item.strip()]
DOWNLOAD_ROUTING_PREFER = (os.environ.get("DOWNLOAD_ROUTING_PREFER") or "usenet").strip().lower() or "usenet"
DOWNLOAD_ROUTING_FALLBACK = (os.environ.get("DOWNLOAD_ROUTING_FALLBACK") or "torrent").strip().lower() or "torrent"
DOWNLOAD_ROUTING_REQUIRE_CLIENT = (os.environ.get("DOWNLOAD_ROUTING_REQUIRE_CLIENT") or "true").strip().lower() not in {"0", "false", "no", "off"}
PROWLARR_INDEXERS_TORRENT = [
    item.strip()
    for item in (os.environ.get("PROWLARR_BOOTSTRAP_INDEXERS_TORRENT") or "").split(",")
    if item.strip()
]
PROWLARR_INDEXERS_USENET = [
    item.strip()
    for item in (os.environ.get("PROWLARR_BOOTSTRAP_INDEXERS_USENET") or "").split(",")
    if item.strip()
]


def service_enabled(name: str) -> bool:
    services = {item.strip() for item in (os.environ.get("MEDIA_SERVICES", "") or "").split(",") if item.strip()}
    return name in services


def truthy(value: str | None) -> bool:
    return (value or "").strip().lower() not in {"", "0", "false", "no", "off"}


def env_list(name: str) -> list[str]:
    return [
        item.strip()
        for item in (os.environ.get(name) or "").split(",")
        if item.strip()
    ]


def write_env_value(key: str, value: str) -> None:
    path = Path(os.environ.get("ENV_FILE") or "/opt/media/.env")
    lines = path.read_text(encoding="utf-8").splitlines() if path.exists() else []
    updated = []
    seen = False
    for line in lines:
        if line.startswith(f"{key}="):
            updated.append(f"{key}={value}")
            seen = True
        else:
            updated.append(line)
    if not seen:
        updated.append(f"{key}={value}")
    path.write_text("\n".join(updated).rstrip() + "\n", encoding="utf-8")


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


def read_sabnzbd_api_key():
    path = os.path.join(CONFIG_PATH, "sabnzbd", "sabnzbd.ini")
    for _ in range(120):
        if os.path.exists(path):
            import configparser

            parser = configparser.ConfigParser()
            parser.read_string("[root]\n" + Path(path).read_text(encoding="utf-8"))
            if parser.has_section("misc"):
                key = (parser.get("misc", "api_key", fallback="") or "").strip()
                if key:
                    return key
        env_key = (os.environ.get("SABNZBD_API_KEY") or "").strip()
        if env_key:
            return env_key
        time.sleep(2)
    raise RuntimeError(f"SABnzbd API key was not created at {path}")


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


def ensure_prowlarr_app_sync(prowlarr_url, prowlarr_key, arr_name, arr_url, arr_key, sync_categories):
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
            "syncCategories": sync_categories,
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
            {"name": "syncCategories", "value": sync_categories},
        ],
    }
    request("POST", f"{prowlarr_url}/api/v1/applications", prowlarr_key, payload, allow=(400, 409))


def ensure_default_indexers(prowlarr_url, prowlarr_key, allowed_protocols):
    managed_names = set(PROWLARR_INDEXERS_TORRENT + PROWLARR_INDEXERS_USENET)
    allowed_names = set()
    for protocol in allowed_protocols:
        if protocol == "torrent":
            allowed_names.update(PROWLARR_INDEXERS_TORRENT)
        elif protocol == "usenet":
            allowed_names.update(PROWLARR_INDEXERS_USENET)

    existing = request("GET", f"{prowlarr_url}/api/v1/indexer", prowlarr_key, allow=()) or []
    existing_names = {item.get("name") for item in existing if item.get("name")}

    schemas = request("GET", f"{prowlarr_url}/api/v1/indexer/schema", prowlarr_key, allow=()) or []
    schema_names = {schema.get("name") for schema in schemas if schema.get("name")}
    usable_torrent_names = [name for name in PROWLARR_INDEXERS_TORRENT if name in schema_names]
    usable_usenet_names = [name for name in PROWLARR_INDEXERS_USENET if name in schema_names]
    desired_usable_names = set()
    if "torrent" in allowed_protocols:
        desired_usable_names.update(usable_torrent_names)
    if "usenet" in allowed_protocols:
        desired_usable_names.update(usable_usenet_names)
    if "torrent" in allowed_protocols and not usable_torrent_names:
        raise RuntimeError("no schema-backed torrent indexers are available in Prowlarr")
    if "usenet" in allowed_protocols and not usable_usenet_names:
        raise RuntimeError("no schema-backed usenet indexers are available in Prowlarr")
    profiles = request("GET", f"{prowlarr_url}/api/v1/appProfile", prowlarr_key, allow=()) or []
    profile_id = profiles[0]["id"] if profiles else 1

    for item in existing:
        name = item.get("name")
        if name not in managed_names:
            continue
        updated = dict(item)
        should_enable = name in desired_usable_names
        changed = False
        if updated.get("enable") != should_enable:
            updated["enable"] = should_enable
            changed = True
        if should_enable:
            if updated.get("priority") != 25:
                updated["priority"] = 25
                changed = True
            if updated.get("appProfileId") != profile_id:
                updated["appProfileId"] = profile_id
                changed = True
        if changed:
            request("PUT", f"{prowlarr_url}/api/v1/indexer/{item['id']}", prowlarr_key, updated)

    selected = [
        schema for schema in schemas
        if schema.get("name") in desired_usable_names and schema.get("name") not in existing_names
    ]
    for schema in selected:
        candidate = dict(schema)
        candidate["enable"] = True
        candidate["priority"] = 25
        candidate["appProfileId"] = profile_id
        request("POST", f"{prowlarr_url}/api/v1/indexer", prowlarr_key, candidate, allow=(400, 409))


def ensure_flaresolverr_proxy(prowlarr_url, prowlarr_key):
    host_url = (os.environ.get("PROWLARR_FLARESOLVERR_URL") or "http://flaresolverr:8191").rstrip("/") + "/"
    existing = request("GET", f"{prowlarr_url}/api/v1/indexerproxy", prowlarr_key, allow=()) or []
    for item in existing:
        if (item.get("name") or "").lower() == "flaresolverr":
            updated = dict(item)
            fields = updated.get("fields", [])
            desired = {
                "host": host_url,
                "requestTimeout": 60,
            }
            changed = False
            for field in fields:
                name = field.get("name")
                if name in desired and field.get("value") != desired[name]:
                    field["value"] = desired[name]
                    changed = True
            if changed:
                request("PUT", f"{prowlarr_url}/api/v1/indexerproxy/{item['id']}", prowlarr_key, updated)
            return
    payload = {
        "name": "FlareSolverr",
        "implementation": "FlareSolverr",
        "configContract": "FlareSolverrSettings",
        "enable": True,
        "fields": [
            {"name": "host", "value": host_url},
            {"name": "requestTimeout", "value": 60},
        ],
    }
    request("POST", f"{prowlarr_url}/api/v1/indexerproxy", prowlarr_key, payload, allow=(400, 409))


def ensure_indexer_sync_clients(prowlarr_url, prowlarr_key):
    request("POST", f"{prowlarr_url}/api/v1/indexer/sync", prowlarr_key, {}, allow=(400, 404, 405, 409))


def ensure_root_folder(url, api_key, path):
    existing = request("GET", f"{url}/api/v3/rootfolder", api_key) or []
    if any(item.get("path") == path for item in existing):
        return
    os.makedirs(path, exist_ok=True)
    request("POST", f"{url}/api/v3/rootfolder", api_key, {"path": path})


def ensure_media_management(url, api_key):
    payload = request("GET", f"{url}/api/v3/config/mediamanagement", api_key, allow=())
    desired = {
        "skipFreeSpaceCheckWhenImporting": False,
        "minimumFreeSpaceWhenImporting": 100,
        "copyUsingHardlinks": True,
        "rescanAfterRefresh": "always",
    }
    changed = False
    for key, value in desired.items():
        if payload.get(key) != value:
            payload[key] = value
            changed = True
    if changed:
        request("PUT", f"{url}/api/v3/config/mediamanagement", api_key, payload, allow=())


def remove_download_client(app, url, api_key, name):
    existing = request("GET", f"{url}/api/v3/downloadclient", api_key, allow=()) or []
    target = next((item for item in existing if item.get("name") == name), None)
    if not target:
        return
    request("DELETE", f"{url}/api/v3/downloadclient/{target['id']}", api_key, allow=(200, 204, 404))


def ensure_qbittorrent_download_client(app, url, api_key, category, priority):
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
            updated["priority"] = priority
            fields = updated.get("fields", [])
            current = {field.get("name"): field.get("value") for field in fields}
            if item.get("priority") == priority and all(
                current.get(name) == value for name, value in comparable_desired.items()
            ):
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
                "priority": 2,
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
            if refreshed.get("priority") == priority and all(
                refreshed_fields.get(name) == value for name, value in comparable_desired.items()
            ):
                return
        time.sleep(2)

    raise RuntimeError(f"{app} qBittorrent download client did not converge")


def ensure_sabnzbd_download_client(app, url, arr_api_key, sab_api_key, category, priority):
    if app == "sonarr":
        category_field = "tvCategory"
        recent_priority_field = "recentTvPriority"
        older_priority_field = "olderTvPriority"
    elif app == "radarr":
        category_field = "movieCategory"
        recent_priority_field = "recentMoviePriority"
        older_priority_field = "olderMoviePriority"
    else:
        raise RuntimeError(f"unsupported app for SABnzbd download client: {app}")

    sab_parsed = urllib.parse.urlparse(SAB_URL)
    desired = {
        "host": sab_parsed.hostname or "sabnzbd",
        "port": sab_parsed.port or 8080,
        "urlBase": sab_parsed.path.rstrip("/"),
        "apiKey": sab_api_key,
        category_field: category,
    }
    comparable_desired = {key: value for key, value in desired.items() if key != "apiKey"}

    def current_client():
        existing = request("GET", f"{url}/api/v3/downloadclient", arr_api_key, allow=()) or []
        return next((item for item in existing if item.get("name") == "SABnzbd"), None)

    for _ in range(60):
        item = current_client()
        if item is not None:
            updated = dict(item)
            updated["priority"] = priority
            fields = updated.get("fields", [])
            current = {field.get("name"): field.get("value") for field in fields}
            if item.get("priority") == priority and all(
                current.get(name) == value for name, value in comparable_desired.items()
            ):
                return
            for field in fields:
                name = field.get("name")
                if name in desired:
                    field["value"] = desired[name]
            try:
                request("PUT", f"{url}/api/v3/downloadclient/{item['id']}", arr_api_key, updated, allow=())
            except urllib.error.HTTPError:
                time.sleep(2)
                continue
        else:
            fields = [
                {"name": "host", "value": desired["host"]},
                {"name": "port", "value": desired["port"]},
                {"name": "urlBase", "value": desired["urlBase"]},
                {"name": "apiKey", "value": sab_api_key},
                {"name": category_field, "value": category},
                {"name": recent_priority_field, "value": 0},
                {"name": older_priority_field, "value": 0},
                {"name": "initialState", "value": 0},
            ]
            payload = {
                "enable": True,
                "protocol": "usenet",
                "priority": 1,
                "removeCompletedDownloads": True,
                "removeFailedDownloads": True,
                "name": "SABnzbd",
                "implementation": "SABnzbd",
                "configContract": "SABnzbdSettings",
                "fields": fields,
            }
            try:
                request("POST", f"{url}/api/v3/downloadclient", arr_api_key, payload, allow=(409,))
            except urllib.error.HTTPError:
                time.sleep(2)
                continue

        refreshed = current_client()
        if refreshed is not None:
            refreshed_fields = {field.get("name"): field.get("value") for field in refreshed.get("fields") or []}
            if refreshed.get("priority") == priority and all(
                refreshed_fields.get(name) == value for name, value in comparable_desired.items()
            ):
                return
        time.sleep(2)

    raise RuntimeError(f"{app} SABnzbd download client did not converge")


def qbit_state():
    enabled = service_enabled("qbittorrent-vpn")
    configured = enabled and bool(QBIT_USERNAME.strip()) and bool(QBIT_PASSWORD.strip())
    healthy = False
    if enabled:
        try:
            urllib.request.urlopen(
                urllib.request.Request(
                    f"http://127.0.0.1:{QBIT_PORT}/api/v2/app/version",
                    method="GET",
                ),
                timeout=20,
            ).read()
            if configured:
                login_payload = urllib.parse.urlencode(
                    {"username": QBIT_USERNAME, "password": QBIT_PASSWORD}
                ).encode()
                urllib.request.urlopen(
                    urllib.request.Request(
                        f"http://127.0.0.1:{QBIT_PORT}/api/v2/auth/login",
                        data=login_payload,
                        method="POST",
                    ),
                    timeout=20,
                ).read()
                healthy = True
            else:
                healthy = True
        except Exception:
            healthy = False
    usable = enabled and configured and healthy
    return {
        "enabled": enabled,
        "configured": configured,
        "healthy": healthy,
        "usable": usable,
    }


def sab_state():
    enabled = service_enabled("sabnzbd")
    config_path = Path(CONFIG_PATH) / "sabnzbd" / "sabnzbd.ini"
    configured = False
    healthy = False
    api_key = ""
    if enabled:
        api_key = (os.environ.get("SABNZBD_API_KEY") or "").strip()
        if not api_key and config_path.exists():
            try:
                import configparser

                parser = configparser.ConfigParser()
                parser.read_string("[root]\n" + config_path.read_text(encoding="utf-8"))
                if parser.has_section("misc"):
                    api_key = (parser.get("misc", "api_key", fallback="") or "").strip()
            except Exception:
                api_key = ""
        server_host = (os.environ.get("SABNZBD_SERVER_HOST") or "").strip()
        server_enabled = truthy(os.environ.get("SABNZBD_SERVER_ENABLE"))
        configured = bool(api_key and server_host and server_enabled)
        if configured:
            try:
                urllib.request.urlopen(
                    urllib.request.Request(
                        f"http://127.0.0.1:8085/api?mode=version&apikey={api_key}",
                        method="GET",
                    ),
                    timeout=20,
                ).read()
                req = urllib.request.Request(
                    f"http://127.0.0.1:8085/api?mode=get_config&section=servers&apikey={api_key}",
                    method="GET",
                )
                with urllib.request.urlopen(req, timeout=20) as response:
                    payload = json.loads(response.read().decode("utf-8") or "{}")
                servers = payload.get("servers") or {}
                healthy = any(
                    bool((server.get("enable") or "").strip() in {"1", "true", "True"})
                    for server in servers.values()
                    if isinstance(server, dict)
                )
            except Exception:
                healthy = False
    usable = enabled and configured and healthy
    return {
        "enabled": enabled,
        "configured": configured,
        "healthy": healthy,
        "usable": usable,
        "api_key": api_key,
    }


def select_protocols():
    qbit = qbit_state()
    sab = sab_state()
    write_env_value("VMCTL_QBITTORRENT_CONFIGURED", str(qbit["configured"]).lower())
    write_env_value("VMCTL_QBITTORRENT_HEALTHY", str(qbit["healthy"]).lower())
    write_env_value("VMCTL_QBITTORRENT_USABLE", str(qbit["usable"]).lower())
    write_env_value("VMCTL_SABNZBD_CONFIGURED", str(sab["configured"]).lower())
    write_env_value("VMCTL_SABNZBD_HEALTHY", str(sab["healthy"]).lower())
    write_env_value("VMCTL_SABNZBD_USABLE", str(sab["usable"]).lower())

    ordered = []
    for protocol in [DOWNLOAD_ROUTING_PREFER, DOWNLOAD_ROUTING_FALLBACK, "usenet", "torrent"]:
        if protocol not in {"usenet", "torrent"}:
            continue
        if protocol in ordered:
            continue
        if protocol == "usenet" and sab["usable"]:
            ordered.append(protocol)
        elif protocol == "torrent" and qbit["usable"]:
            ordered.append(protocol)

    if not ordered:
        write_env_value("VMCTL_DOWNLOAD_PROTOCOLS", "")
        if DOWNLOAD_ROUTING_REQUIRE_CLIENT:
            raise RuntimeError(
                "No usable download clients. Enable and configure either qBittorrent or SABnzbd before applying."
            )
        return qbit, sab, ordered

    write_env_value("VMCTL_DOWNLOAD_PROTOCOLS", ",".join(ordered))
    return qbit, sab, ordered


apps = {
    "sonarr": {
        "url": os.environ.get("SONARR_URL", "http://sonarr:8989"),
        "internal_url": os.environ.get("SONARR_INTERNAL_URL", "http://sonarr:8989"),
        "base": "",
        "root": SONARR_ROOT_FOLDER,
        "category": QBIT_CATEGORY_TV,
        "prowlarr_categories": SONARR_PROWLARR_CATEGORIES,
    },
    "radarr": {
        "url": os.environ.get("RADARR_URL", "http://radarr:7878"),
        "internal_url": os.environ.get("RADARR_INTERNAL_URL", "http://radarr:7878"),
        "base": "",
        "root": RADARR_ROOT_FOLDER,
        "category": QBIT_CATEGORY_MOVIES,
        "prowlarr_categories": RADARR_PROWLARR_CATEGORIES,
    },
}

resolved = {}
qbit, sab, allowed_protocols = select_protocols()
protocol_priority = {protocol: index + 1 for index, protocol in enumerate(allowed_protocols)}
for app, cfg in apps.items():
    key = read_api_key(app)
    root, discovered_base = detect_api_base(app, cfg["url"], key, "/api/v3", cfg["base"])
    api_url = f"{root}{discovered_base}"
    ensure_root_folder(api_url, key, cfg["root"])
    ensure_media_management(api_url, key)
    if qbit["usable"]:
        ensure_qbittorrent_download_client(
            app,
            api_url,
            key,
            cfg["category"],
            protocol_priority.get("torrent", 1),
        )
    else:
        remove_download_client(app, api_url, key, "qBittorrent")
    if sab["usable"]:
        sab_api_key = sab["api_key"] or read_sabnzbd_api_key()
        ensure_sabnzbd_download_client(
            app,
            api_url,
            key,
            sab_api_key,
            cfg["category"],
            protocol_priority.get("usenet", 1),
        )
    else:
        remove_download_client(app, api_url, key, "SABnzbd")
    resolved[app] = {"url": app_base(cfg["internal_url"], cfg["base"]), "key": key}

prowlarr_url = os.environ.get("PROWLARR_URL", "http://localhost:9696")
prowlarr_base = ""
prowlarr_key = read_api_key("prowlarr")
prowlarr_root, prowlarr_discovered_base = detect_api_base("prowlarr", prowlarr_url, prowlarr_key, "/api/v1", prowlarr_base)
prowlarr_api = f"{prowlarr_root}{prowlarr_discovered_base}"
ensure_default_indexers(prowlarr_api, prowlarr_key, allowed_protocols)
ensure_flaresolverr_proxy(prowlarr_api, prowlarr_key)

for app_name, values in resolved.items():
    ensure_prowlarr_app_sync(
        prowlarr_api,
        prowlarr_key,
        app_name.capitalize(),
        values["url"],
        values["key"],
        apps[app_name]["prowlarr_categories"],
    )
ensure_indexer_sync_clients(prowlarr_api, prowlarr_key)
PY
