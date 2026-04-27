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

python3 <<'PY'
import http.cookiejar
import json
import os
import sqlite3
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

SEERR_URL = os.environ.get("SEERR_URL", "http://localhost:5055")
JELLYFIN_URL = os.environ.get("JELLYFIN_URL", "http://localhost:8096")
JELLYFIN_INTERNAL_URL = os.environ.get("JELLYFIN_INTERNAL_URL", "http://jellyfin:8096")
JELLYFIN_BASE_PATH = ""
JELLYFIN_ADMIN_USER = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
JELLYFIN_ADMIN_PASSWORD = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")
CONFIG_PATH = os.environ.get("CONFIG_PATH", "/opt/media/config")
SONARR_URL = os.environ.get("SONARR_URL", "http://localhost:8989")
RADARR_URL = os.environ.get("RADARR_URL", "http://localhost:7878")
SONARR_INTERNAL_URL = os.environ.get("SONARR_INTERNAL_URL", "http://sonarr:8989")
RADARR_INTERNAL_URL = os.environ.get("RADARR_INTERNAL_URL", "http://radarr:7878")
SONARR_EXTERNAL_URL = os.environ.get("SONARR_EXTERNAL_URL", "")
RADARR_EXTERNAL_URL = os.environ.get("RADARR_EXTERNAL_URL", "")
SONARR_ROOT_FOLDER = os.environ.get("SONARR_ROOT_FOLDER", "/data/media/tv")
RADARR_ROOT_FOLDER = os.environ.get("RADARR_ROOT_FOLDER", "/data/media/movies")
SEERR_DB = Path(CONFIG_PATH) / "seerr" / "db" / "db.sqlite3"
SEERR_SETTINGS = Path(CONFIG_PATH) / "seerr" / "settings.json"


def normalize_base(value: str) -> str:
    base = (value or "").strip()
    if not base:
        return ""
    if not base.startswith("/"):
        base = f"/{base}"
    return "" if base == "/" else base.rstrip("/")


JELLYFIN_BASE_PATH = normalize_base(os.environ.get("JELLYFIN_BASE_URL", ""))


def jellyfin_url_base(parsed: urllib.parse.ParseResult) -> str:
    return normalize_base(parsed.path) or JELLYFIN_BASE_PATH


def jellyfin_api_base(url: str) -> str:
    parsed = urllib.parse.urlparse(url)
    base = jellyfin_url_base(parsed)
    return f"{url.rstrip('/')}{base}" if not normalize_base(parsed.path) else url.rstrip("/")


def build_external_url(explicit: str, port: int) -> str:
    value = (explicit or "").strip().rstrip("/")
    if value:
        return value
    parsed = urllib.parse.urlparse(JELLYFIN_URL)
    if not parsed.hostname:
        return ""
    scheme = parsed.scheme or "http"
    return f"{scheme}://{parsed.hostname}:{port}"


def wait_for(url: str, timeout_seconds: int = 180):
    started = time.time()
    while time.time() - started < timeout_seconds:
        try:
            with urllib.request.urlopen(url, timeout=10):
                return
        except Exception:
            time.sleep(2)
    raise RuntimeError(f"timed out waiting for {url}")


def request_json(method: str, url: str, payload=None, headers=None, allow=(200, 201, 204), opener=None):
    body = None
    req_headers = dict(headers or {})
    if payload is not None:
        body = json.dumps(payload).encode()
        req_headers.setdefault("Content-Type", "application/json")
    req = urllib.request.Request(url, data=body, headers=req_headers, method=method)
    try:
        if opener is None:
            response = urllib.request.urlopen(req, timeout=20)
        else:
            response = opener.open(req, timeout=20)
        with response:
            raw = response.read().decode()
            if not raw:
                return None
            return json.loads(raw)
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        raise


def read_arr_api_key(app: str) -> str:
    config_path = Path(CONFIG_PATH) / app / "config.xml"
    started = time.time()
    while time.time() - started < 180:
        if config_path.exists():
            root = ET.parse(config_path).getroot()
            key = root.findtext("ApiKey")
            if key:
                return key
        time.sleep(2)
    raise RuntimeError(f"missing API key for {app} at {config_path}")


def pick_profile(items, preferred_name: str):
    preferred_name = (preferred_name or "").strip()
    if preferred_name:
        for item in items or []:
            if (item.get("name") or "").strip().lower() == preferred_name.lower():
                return item
    return (items or [{}])[0] if items else {"id": 1, "name": "Any"}


def pick_sonarr_defaults(api_base: str, api_key: str):
    headers = {"X-Api-Key": api_key}
    quality = request_json("GET", f"{api_base}/api/v3/qualityprofile", headers=headers, allow=()) or []
    root_folders = request_json("GET", f"{api_base}/api/v3/rootfolder", headers=headers, allow=()) or []
    quality_profile = pick_profile(quality, os.environ.get("SONARR_DEFAULT_QUALITY_PROFILE"))
    root_candidates = [item.get("path") for item in root_folders if item.get("path")]
    root_folder = SONARR_ROOT_FOLDER if SONARR_ROOT_FOLDER in root_candidates else (root_candidates[0] if root_candidates else SONARR_ROOT_FOLDER)
    return quality_profile, root_folder


def pick_radarr_defaults(api_base: str, api_key: str):
    headers = {"X-Api-Key": api_key}
    quality = request_json("GET", f"{api_base}/api/v3/qualityprofile", headers=headers, allow=()) or []
    root_folders = request_json("GET", f"{api_base}/api/v3/rootfolder", headers=headers, allow=()) or []
    quality_profile = pick_profile(quality, os.environ.get("RADARR_DEFAULT_QUALITY_PROFILE"))
    root_candidates = [item.get("path") for item in root_folders if item.get("path")]
    root_folder = RADARR_ROOT_FOLDER if RADARR_ROOT_FOLDER in root_candidates else (root_candidates[0] if root_candidates else RADARR_ROOT_FOLDER)
    return quality_profile, root_folder


def db_user_count() -> int:
    if not SEERR_DB.exists():
        return 0
    conn = sqlite3.connect(str(SEERR_DB))
    try:
        cur = conn.cursor()
        row = cur.execute("select count(*) from user").fetchone()
        return int(row[0]) if row else 0
    finally:
        conn.close()


def ensure_jellyfin_admin_login_seed():
    settings = {}
    if SEERR_SETTINGS.exists():
        settings = json.loads(SEERR_SETTINGS.read_text(encoding="utf-8"))
    settings.setdefault("public", {})
    settings.setdefault("main", {})
    settings.setdefault("jellyfin", {})
    settings["public"]["initialized"] = False
    settings["public"]["mediaServerLogin"] = True
    settings["public"]["localLogin"] = False
    settings["main"]["mediaServerType"] = 4
    settings["main"]["mediaServerLogin"] = True
    settings["main"]["localLogin"] = False
    # Ensure /auth/jellyfin allows host+port based setup bootstrap.
    settings["jellyfin"]["ip"] = ""
    settings["jellyfin"]["apiKey"] = ""
    settings["jellyfin"]["serverId"] = ""
    SEERR_SETTINGS.parent.mkdir(parents=True, exist_ok=True)
    SEERR_SETTINGS.write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")

    jellyfin_internal = urllib.parse.urlparse(JELLYFIN_INTERNAL_URL)
    jellyfin_host = jellyfin_internal.hostname or "jellyfin"
    jellyfin_port = jellyfin_internal.port or (443 if jellyfin_internal.scheme == "https" else 8096)
    jellyfin_use_ssl = jellyfin_internal.scheme == "https"
    jellyfin_base = jellyfin_url_base(jellyfin_internal)

    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))

    request_json(
        "POST",
        f"{SEERR_URL}/api/v1/auth/jellyfin",
        {
            "email": JELLYFIN_ADMIN_USER,
            "username": JELLYFIN_ADMIN_USER,
            "password": JELLYFIN_ADMIN_PASSWORD,
            "hostname": jellyfin_host,
            "port": jellyfin_port,
            "useSsl": jellyfin_use_ssl,
            "urlBase": jellyfin_base,
            "serverType": 2,  # Jellyfin
        },
        allow=(),
        opener=opener,
    )
    # Session cookie exists in opener after successful login.
    request_json("POST", f"{SEERR_URL}/api/v1/settings/initialize", {}, allow=(200, 204, 400), opener=opener)


wait_for(f"{SEERR_URL}/api/v1/status")
wait_for(f"{jellyfin_api_base(JELLYFIN_INTERNAL_URL)}/System/Info/Public")
wait_for(f"{SONARR_URL}/ping")
wait_for(f"{RADARR_URL}/ping")

if db_user_count() == 0:
    ensure_jellyfin_admin_login_seed()
    # Reload after mutable state changes.
    wait_for(f"{SEERR_URL}/api/v1/status")

sonarr_api_key = read_arr_api_key("sonarr")
radarr_api_key = read_arr_api_key("radarr")
sonarr_quality, sonarr_root = pick_sonarr_defaults(SONARR_URL, sonarr_api_key)
radarr_quality, radarr_root = pick_radarr_defaults(RADARR_URL, radarr_api_key)

settings = {}
if SEERR_SETTINGS.exists():
    settings = json.loads(SEERR_SETTINGS.read_text(encoding="utf-8"))
settings.setdefault("main", {})
settings.setdefault("public", {})
settings.setdefault("jellyfin", {})
settings.setdefault("sonarr", [])
settings.setdefault("radarr", [])
settings["public"]["initialized"] = True
settings["public"]["mediaServerLogin"] = True
settings["public"]["localLogin"] = False
settings["main"]["mediaServerType"] = 2
settings["main"]["mediaServerLogin"] = True
settings["main"]["localLogin"] = False

jellyfin_internal = urllib.parse.urlparse(JELLYFIN_INTERNAL_URL)
jellyfin_external = urllib.parse.urlparse(JELLYFIN_URL)
jellyfin_settings = settings["jellyfin"]
server_name = (os.environ.get("VMCTL_RESOURCE_NAME") or jellyfin_settings.get("name") or "media-stack").strip()
settings["jellyfin"] = {
    "name": jellyfin_settings.get("name", server_name),
    "ip": jellyfin_internal.hostname or "jellyfin",
    "port": jellyfin_internal.port or (443 if jellyfin_internal.scheme == "https" else 8096),
    "useSsl": jellyfin_internal.scheme == "https",
    "urlBase": jellyfin_url_base(jellyfin_internal),
    "externalHostname": jellyfin_external.hostname or "",
    "jellyfinForgotPasswordUrl": jellyfin_settings.get("jellyfinForgotPasswordUrl", ""),
    "libraries": jellyfin_settings.get("libraries", []),
    "serverId": jellyfin_settings.get("serverId", ""),
    "apiKey": jellyfin_settings.get("apiKey", ""),
}

sonarr_parsed = urllib.parse.urlparse(SONARR_INTERNAL_URL)
radarr_parsed = urllib.parse.urlparse(RADARR_INTERNAL_URL)

settings["sonarr"] = [{
    "id": settings["sonarr"][0]["id"] if settings["sonarr"] else 0,
    "name": "Sonarr",
    "hostname": sonarr_parsed.hostname or "sonarr",
    "port": sonarr_parsed.port or 8989,
    "apiKey": sonarr_api_key,
    "useSsl": sonarr_parsed.scheme == "https",
    "baseUrl": "",
    "activeProfileId": sonarr_quality.get("id", 1),
    "activeProfileName": sonarr_quality.get("name", "Any"),
    "activeDirectory": sonarr_root,
    "activeLanguageProfileId": 1,
    "activeAnimeProfileId": None,
    "activeAnimeLanguageProfileId": None,
    "activeAnimeProfileName": None,
    "activeAnimeDirectory": None,
    "is4k": False,
    "enableSeasonFolders": True,
    "isDefault": True,
    "externalUrl": build_external_url(SONARR_EXTERNAL_URL, 8989),
    "syncEnabled": True,
    "preventSearch": False,
}]

settings["radarr"] = [{
    "id": settings["radarr"][0]["id"] if settings["radarr"] else 0,
    "name": "Radarr",
    "hostname": radarr_parsed.hostname or "radarr",
    "port": radarr_parsed.port or 7878,
    "apiKey": radarr_api_key,
    "useSsl": radarr_parsed.scheme == "https",
    "baseUrl": "",
    "activeProfileId": radarr_quality.get("id", 1),
    "activeProfileName": radarr_quality.get("name", "Any"),
    "activeDirectory": radarr_root,
    "is4k": False,
    "minimumAvailability": "released",
    "isDefault": True,
    "externalUrl": build_external_url(RADARR_EXTERNAL_URL, 7878),
    "syncEnabled": True,
    "preventSearch": False,
}]

SEERR_SETTINGS.write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")
PY

docker_compose up -d seerr
docker_compose restart seerr

for _ in $(seq 1 90); do
  if curl -fsS "http://localhost:5055/api/v1/settings/public" | grep -q '"initialized":true'; then
    exit 0
  fi
  sleep 2
done

echo "seerr failed to finish initialization bootstrap" >&2
exit 1
