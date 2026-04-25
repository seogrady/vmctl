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

docker_compose up -d sabnzbd

python3 <<'PY'
import configparser
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
env_file = Path(os.environ.get("ENV_FILE") or "/opt/media/.env")
public_url = (os.environ.get("SABNZBD_URL") or "http://localhost:8085").rstrip("/")
internal_url = (os.environ.get("SABNZBD_INTERNAL_URL") or "http://sabnzbd:8080").rstrip("/")
download_root = "/data/usenet/incomplete"
complete_root = "/data/usenet/complete"
tv_root = f"{complete_root}/tv"
movies_root = f"{complete_root}/movies"


def read_ini_api_key(path: Path) -> str:
    parser = configparser.ConfigParser()
    parser.read_string("[root]\n" + path.read_text(encoding="utf-8"))
    for section in ("misc", "server"):
        if parser.has_section(section):
            for key in ("api_key", "apikey"):
                value = (parser.get(section, key, fallback="") or "").strip()
                if value:
                    return value
    return ""


def set_env_value(key: str, value: str) -> None:
    lines = env_file.read_text(encoding="utf-8").splitlines() if env_file.exists() else []
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
    env_file.write_text("\n".join(updated).rstrip() + "\n", encoding="utf-8")


def set_section_value(path: Path, section: str, key: str, value: str) -> None:
    lines = path.read_text(encoding="utf-8").splitlines()
    section_header = f"[{section}]"
    output: list[str] = []
    in_section = False
    section_seen = False
    replaced = False

    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            if in_section and not replaced:
                output.append(f"{key} = {value}")
                replaced = True
            in_section = stripped == section_header
            if in_section:
                section_seen = True
            output.append(line)
            continue
        if in_section and stripped.startswith(f"{key} ="):
            output.append(f"{key} = {value}")
            replaced = True
        else:
            output.append(line)

    if in_section and not replaced:
        output.append(f"{key} = {value}")
        replaced = True

    if section_seen and not replaced:
        output.append("")
        output.append(section_header)
        output.append(f"{key} = {value}")
        replaced = True

    if not section_seen:
        output.append("")
        output.append(section_header)
        output.append(f"{key} = {value}")

    path.write_text("\n".join(output).rstrip("\n") + "\n", encoding="utf-8")


def request(url: str, api_key: str, params: dict[str, str | int | bool]):
    query = dict(params)
    query["apikey"] = api_key
    full = f"{url}/api?{urllib.parse.urlencode(query, doseq=True)}"
    req = urllib.request.Request(full, method="GET")
    with urllib.request.urlopen(req, timeout=20) as response:
        body = response.read().decode("utf-8")
        return body


config_path = config_root / "sabnzbd" / "sabnzbd.ini"
for _ in range(180):
    if config_path.exists():
        api_key = read_ini_api_key(config_path)
        if api_key:
            break
    time.sleep(2)
else:
    raise RuntimeError(f"SABnzbd API key not found in {config_path}")

for _ in range(180):
    try:
        request(public_url, api_key, {"mode": "version"})
        break
    except Exception:
        time.sleep(2)
else:
    raise RuntimeError(f"SABnzbd did not become ready at {public_url}")

request(public_url, api_key, {"mode": "set_config", "section": "misc", "keyword": "download_dir", "value": download_root})
request(public_url, api_key, {"mode": "set_config", "section": "misc", "keyword": "complete_dir", "value": complete_root})
request(public_url, api_key, {"mode": "set_config", "section": "misc", "keyword": "dirscan_dir", "value": download_root})
request(public_url, api_key, {"mode": "set_config", "section": "misc", "keyword": "host_whitelist", "value": "sabnzbd,media-stack,localhost,127.0.0.1"})
request(public_url, api_key, {"mode": "set_config", "section": "misc", "keyword": "api_key", "value": api_key})
request(public_url, api_key, {"mode": "set_config", "section": "categories", "name": "tv", "dir": tv_root})
request(public_url, api_key, {"mode": "set_config", "section": "categories", "name": "movies", "dir": movies_root})
set_section_value(config_path, "misc", "local_ranges", "100.64.0.0/10,172.18.0.0/16,192.168.0.0/16")

set_env_value("SABNZBD_API_KEY", api_key)
set_env_value("SABNZBD_URL", os.environ.get("SABNZBD_URL", "http://localhost:8085"))
set_env_value("SABNZBD_INTERNAL_URL", internal_url)
PY
docker_compose restart sabnzbd
