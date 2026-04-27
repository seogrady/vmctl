#!/usr/bin/env bash
set -euo pipefail

missing=()
for package in ca-certificates curl python3 unzip nfs-kernel-server p7zip-full ffmpeg; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed' || missing+=("$package")
done
if ((${#missing[@]} > 0)); then
  apt-get update
  apt-get install -y "${missing[@]}"
fi

. /etc/os-release
if ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL "https://download.docker.com/linux/${ID}/gpg" -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  cat > /etc/apt/sources.list.d/docker.list <<EOF
deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable
EOF

  apt-get update
  apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
fi
systemctl enable --now docker

RESOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STACK_DIR="/opt/media"

. "$RESOURCE_DIR/media.env"
STORAGE_PATH="${STORAGE_PATH:-/data}"
MEDIA_SERVICES_CSV="${MEDIA_SERVICES:-}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-media}"

service_enabled() {
  local name="$1"
  case ",${MEDIA_SERVICES_CSV}," in
    *,"$name",*) return 0 ;;
    *) return 1 ;;
  esac
}

docker_compose() {
  docker compose -p "$COMPOSE_PROJECT_NAME" --project-directory "$STACK_DIR" --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" "$@"
}

install -d "$STACK_DIR" "$STACK_DIR/config" \
  "$STORAGE_PATH/torrents/movies" "$STORAGE_PATH/torrents/tv" "$STORAGE_PATH/torrents/.incomplete" \
  "$STORAGE_PATH/usenet/incomplete" "$STORAGE_PATH/usenet/complete/movies" "$STORAGE_PATH/usenet/complete/tv" \
  "$STORAGE_PATH/media/movies" "$STORAGE_PATH/media/tv"
if service_enabled "caddy"; then
  install -d "$STACK_DIR/config/caddy" "$STACK_DIR/config/caddy/ui-index"
fi
if service_enabled "jellyfin"; then
  install -d "$STACK_DIR/config/jellyfin"
  install -d "$STACK_DIR/config/jellyfin/transcodes"
fi
if service_enabled "meilisearch"; then
  install -d "$STACK_DIR/config/meilisearch"
fi
if service_enabled "sonarr"; then
  install -d "$STACK_DIR/config/sonarr"
fi
if service_enabled "radarr"; then
  install -d "$STACK_DIR/config/radarr"
fi
if service_enabled "prowlarr"; then
  install -d "$STACK_DIR/config/prowlarr"
fi
if service_enabled "autobrr"; then
  install -d "$STACK_DIR/config/autobrr"
fi
if service_enabled "qbittorrent-vpn"; then
  install -d "$STACK_DIR/config/qbittorrent"
fi
if service_enabled "sabnzbd"; then
  install -d "$STACK_DIR/config/sabnzbd"
fi
if service_enabled "recyclarr"; then
  install -d "$STACK_DIR/config/recyclarr"
  install -d "$STACK_DIR/config/recyclarr/state"
  chown -R ubuntu:ubuntu "$STACK_DIR/config/recyclarr"
fi
if service_enabled "seerr"; then
  install -d "$STACK_DIR/config/seerr"
fi
if service_enabled "bazarr"; then
  install -d "$STACK_DIR/config/bazarr"
  install -d "$STACK_DIR/config/bazarr/config"
fi
if service_enabled "jellystat"; then
  install -d "$STACK_DIR/config/jellystat"
fi
if service_enabled "jellystat-db"; then
  install -d "$STACK_DIR/config/jellystat-db"
fi
if service_enabled "jellio-shim"; then
  install -d "$STACK_DIR/config/jellio-shim"
fi
install -m 0644 "$RESOURCE_DIR/docker-compose.media" "$STACK_DIR/docker-compose.yml"

install -d /etc/exports.d
cat > /etc/exports.d/vmctl-media.exports <<EOF
$STORAGE_PATH 192.168.86.0/24(ro,sync,no_subtree_check,insecure)
EOF
systemctl enable --now nfs-kernel-server
exportfs -ra

sync_env_from_template() {
  local template_file="$1"
  local env_file="$2"
  python3 - "$template_file" "$env_file" <<'PY'
from collections import OrderedDict
import html
from pathlib import Path
import sys

template_path = Path(sys.argv[1])
env_path = Path(sys.argv[2])
preserve = {
    "SECRET_ENCRYPTION_KEY",
    "POSTGRES_PASSWORD",
    "JWT_SECRET",
    "MEILI_MASTER_KEY",
    "JELLYFIN_STREMIO_PASSWORD",
    "JELLYFIN_STREMIO_AUTH_TOKEN",
    "JELLIO_STREMIO_MANIFEST_URL_TAILSCALE",
    "JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE",
    "CLOUDFLARE_PUBLIC_BASE_URL",
    "CLOUDFLARED_TOKEN",
    "SABNZBD_API_KEY",
    "SEERR_API_KEY",
    "TAILSCALE_FUNNEL_ENABLED",
}


def parse_env(path):
    values = OrderedDict()
    if not path.exists():
        return values
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or "=" not in stripped:
            continue
        key, value = stripped.split("=", 1)
        value = html.unescape(value)
        if key == "WIREGUARD_ADDRESSES":
            parts = [part.strip() for part in value.split(",") if part.strip()]
            ipv4 = [part for part in parts if ":" not in part]
            if ipv4:
                value = ",".join(ipv4)
        values[key] = value
    return values


template = parse_env(template_path)
current = parse_env(env_path)
merged = OrderedDict(current)

for key, value in template.items():
    if key in preserve:
        if not merged.get(key):
            merged[key] = value
    else:
        merged[key] = value

for key in preserve:
    merged.setdefault(key, "")

ordered_keys = list(template.keys()) + [key for key in current.keys() if key not in template]
seen = set()
lines = []
for key in ordered_keys:
    if key in seen or key not in merged:
        continue
    seen.add(key)
    lines.append(f"{key}={merged[key]}")

env_path.write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")
PY
}

sync_env_from_template "$RESOURCE_DIR/media.env" "$STACK_DIR/.env"
# Remove deprecated key after STORAGE_PATH migration.
sed -i '/^MEDIA_PATH=/d' "$STACK_DIR/.env"

random_hex() {
  local bytes="$1"
  python3 - "$bytes" <<'PY'
import secrets
import sys
print(secrets.token_hex(int(sys.argv[1])))
PY
}

ensure_env_value() {
  local file="$1"
  local key="$2"
  local value="$3"
  if grep -q "^${key}=" "$file"; then
    local current
    current="$(grep -E "^${key}=" "$file" | tail -n1 | cut -d= -f2-)"
    if [[ -z "$current" ]]; then
      sed -i "s|^${key}=.*|${key}=${value}|" "$file"
    fi
  else
    printf '%s=%s\n' "$key" "$value" >>"$file"
  fi
}

set_env_value() {
  local file="$1"
  local key="$2"
  local value="$3"
  if grep -q "^${key}=" "$file"; then
    sed -i "s|^${key}=.*|${key}=${value}|" "$file"
  else
    printf '%s=%s\n' "$key" "$value" >>"$file"
  fi
}

detect_primary_ipv4() {
  ip -4 route get 1.1.1.1 2>/dev/null | awk '{
    for (i = 1; i <= NF; i++) {
      if ($i == "src" && (i + 1) <= NF) {
        print $(i + 1)
        exit
      }
    }
  }'
}

sync_template_env_defaults() {
  local template_file="$1"
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" ]] && continue
    [[ "$line" =~ ^# ]] && continue
    if [[ "$line" != *=* ]]; then
      continue
    fi
    local key="${line%%=*}"
    local value="${line#*=}"
    ensure_env_value "$STACK_DIR/.env" "$key" "$value"
  done <"$template_file"
}

sync_template_env_defaults "$RESOURCE_DIR/media.env"

primary_ip="$(detect_primary_ipv4 || true)"
if [[ -n "$primary_ip" ]]; then
  set_env_value "$STACK_DIR/.env" "VMCTL_PRIMARY_IPV4" "$primary_ip"
  set_env_value "$STACK_DIR/.env" "VMCTL_HTTP_BASE_URL_IP" "http://${primary_ip}"
fi

if service_enabled "jellyfin"; then
  current_jellyfin_internal_url="$(grep -E '^JELLYFIN_INTERNAL_URL=' "$STACK_DIR/.env" | tail -n1 | cut -d= -f2- || true)"
  case "$current_jellyfin_internal_url" in
    ""|http://127.0.0.1:8096|http://127.0.1.1:8096|http://localhost:8096|http://media-stack:8096)
      primary_ip="$(detect_primary_ipv4 || true)"
      if [[ -n "$primary_ip" ]]; then
        set_env_value "$STACK_DIR/.env" "JELLYFIN_INTERNAL_URL" "http://${primary_ip}:8096"
      fi
      ;;
  esac
fi

ensure_env_value "$STACK_DIR/.env" "SECRET_ENCRYPTION_KEY" "$(random_hex 32)"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_USER" "jellystat"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_DB" "jellystat"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_PASSWORD" "$(random_hex 24)"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_IP" "jellystat-db"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_PORT" "5432"
ensure_env_value "$STACK_DIR/.env" "JWT_SECRET" "$(random_hex 32)"
ensure_env_value "$STACK_DIR/.env" "MEILI_MASTER_KEY" "$(random_hex 32)"
ensure_env_value "$STACK_DIR/.env" "SEERR_API_KEY" "$(random_hex 24)"
ensure_env_value "$STACK_DIR/.env" "JELLYFIN_STREMIO_PASSWORD" "$(random_hex 20)"

recover_jellystat_db() {
  if ! service_enabled "jellystat-db"; then
    return 0
  fi
  local db_user db_name
  db_user="$(grep -E '^POSTGRES_USER=' "$STACK_DIR/.env" | tail -n1 | cut -d= -f2-)"
  db_name="$(grep -E '^POSTGRES_DB=' "$STACK_DIR/.env" | tail -n1 | cut -d= -f2-)"
  if [[ -z "$db_user" || -z "$db_name" ]]; then
    return 0
  fi

  docker_compose up -d jellystat-db
  docker_compose exec -T -u root jellystat-db \
    sh -lc 'chown -R postgres:postgres /var/lib/postgresql/data'
  docker_compose restart jellystat-db
  sleep 3

  if docker_compose logs --tail=120 jellystat-db \
    | grep -q "password authentication failed for user"; then
    echo "jellystat-db credential drift detected; recreating database volume"
    docker_compose stop jellystat jellystat-db || true
    rm -rf "$STACK_DIR/config/jellystat-db"/*
    chown -R 70:70 "$STACK_DIR/config/jellystat-db"
    docker_compose up -d jellystat-db
  fi
}

write_storage_health_snapshot() {
  if ! service_enabled "caddy"; then
    return 0
  fi

  python3 - "$STORAGE_PATH" "$STACK_DIR/config/caddy/ui-index/storage-health.json" <<'PY'
import json
import os
import sys
import time
from pathlib import Path

storage_path = Path(sys.argv[1])
output_path = Path(sys.argv[2])

try:
    stat = os.statvfs(storage_path)
except FileNotFoundError:
    raise SystemExit(f"storage path not found: {storage_path}")

total_bytes = stat.f_frsize * stat.f_blocks
free_bytes = stat.f_frsize * stat.f_bavail
used_bytes = max(total_bytes - free_bytes, 0)
gb = 1024 ** 3
payload = {
    "storagePath": str(storage_path),
    "totalBytes": total_bytes,
    "usedBytes": used_bytes,
    "freeBytes": free_bytes,
    "totalGb": round(total_bytes / gb, 2),
    "usedGb": round(used_bytes / gb, 2),
    "freeGb": round(free_bytes / gb, 2),
    "usedPercent": round((used_bytes * 100.0 / total_bytes) if total_bytes else 0.0, 2),
    "freePercent": round((free_bytes * 100.0 / total_bytes) if total_bytes else 0.0, 2),
    "updatedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
}
output_path.parent.mkdir(parents=True, exist_ok=True)
output_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
}

configure_sabnzbd() {
  if ! service_enabled "sabnzbd"; then
    return 0
  fi

  python3 <<'PY'
import configparser
import os
import secrets
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
env_file = Path(os.environ.get("ENV_FILE") or "/opt/media/.env")
ini_path = config_root / "sabnzbd" / "sabnzbd.ini"
ini_path.parent.mkdir(parents=True, exist_ok=True)

download_root = "/data/usenet/incomplete"
complete_root = "/data/usenet/complete"
tv_root = f"{complete_root}/tv"
movies_root = f"{complete_root}/movies"


def env(name: str, default: str = "") -> str:
    return (os.environ.get(name) or default).strip()


def ini_escape(value: str) -> str:
    return value.replace("\n", " ").replace("\r", " ").strip()


def read_existing_api_key(path: Path) -> str:
    if not path.exists():
        return ""
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


api_key = env("SABNZBD_API_KEY") or read_existing_api_key(ini_path) or secrets.token_hex(24)
web_username = env("SABNZBD_USERNAME")
web_password = env("SABNZBD_PASSWORD")

server_host = env("SABNZBD_SERVER_HOST")
server_port = int(env("SABNZBD_SERVER_PORT", "563"))
server_username = env("SABNZBD_SERVER_USERNAME")
server_password = env("SABNZBD_SERVER_PASSWORD")
server_connections = int(env("SABNZBD_SERVER_CONNECTIONS", "10"))
server_timeout = int(env("SABNZBD_SERVER_TIMEOUT", "120"))
server_retention = int(env("SABNZBD_SERVER_RETENTION", "0"))
server_ssl = env("SABNZBD_SERVER_SSL", "true").lower() not in {"0", "false", "no", "off"}
server_enable_env = env("SABNZBD_SERVER_ENABLE", "false")
server_enabled = server_enable_env.lower() not in {"0", "false", "no", "off"}
if not server_host:
    server_enabled = False
server_name = env("SABNZBD_SERVER_NAME") or server_host or "sabnzbd"
server_display_name = env("SABNZBD_SERVER_DISPLAY_NAME") or server_name

text = "\n".join(
    [
        "__version__ = 19",
        "__encoding__ = utf-8",
        "",
        "[misc]",
        "host = 0.0.0.0",
        "port = 8080",
        f"username = {ini_escape(web_username)}",
        f"password = {ini_escape(web_password)}",
        f"api_key = {api_key}",
        f"download_dir = {download_root}",
        f"complete_dir = {complete_root}",
        f"dirscan_dir = {download_root}",
        "host_whitelist = sabnzbd,media-stack,localhost,127.0.0.1",
        "local_ranges = 100.64.0.0/10,172.18.0.0/16,192.168.0.0/16",
        "auto_browser = 0",
        "enable_https = 0",
        "disable_api_key = 0",
        "folder_rename = 1",
        "enable_movie_sorting = 1",
        "enable_tv_sorting = 1",
        "movie_categories = movies,",
        "tv_categories = tv,",
        "movie_sort_string = %title (%y).%ext",
        "tv_sort_string = %sn/Season %s/%sn - %sx%0e - %en.%ext",
        "",
        "[servers]",
        f"[[{server_name}]]",
        f"displayname = {ini_escape(server_display_name)}",
        f"host = {server_host}",
        f"port = {server_port}",
        f"timeout = {server_timeout}",
        f"username = {ini_escape(server_username)}",
        f"password = {ini_escape(server_password)}",
        f"connections = {server_connections}",
        f"ssl = {1 if server_ssl else 0}",
        "ssl_verify = 3",
        "ssl_ciphers = ",
        f"enable = {1 if server_enabled else 0}",
        "required = 0",
        "optional = 0",
        "pipelining_requests = 10",
        f"retention = {server_retention}",
        "priority = 0",
        "notes = ",
        "",
        "[categories]",
        "[[movies]]",
        "name = movies",
        "order = 1",
        "pp = 3",
        "script = Default",
        "priority = -100",
        f"dir = {movies_root}",
        "[[tv]]",
        "name = tv",
        "order = 2",
        "pp = 3",
        "script = Default",
        "priority = -100",
        f"dir = {tv_root}",
        "",
    ]
)

ini_path.write_text(text, encoding="utf-8")
set_env_value("SABNZBD_API_KEY", api_key)
set_env_value("SABNZBD_URL", env("SABNZBD_URL", "http://localhost:8085"))
set_env_value("SABNZBD_INTERNAL_URL", env("SABNZBD_INTERNAL_URL", "http://sabnzbd:8080"))
PY
}

if grep -q '^MEDIA_VPN_CONFIGURED=true$' "$STACK_DIR/.env" && grep -q '^MEDIA_VPN_ENABLED=false$' "$STACK_DIR/.env"; then
  echo "media VPN is configured but incomplete; refusing to start qBittorrent without VPN"
  exit 1
fi

if [[ -f "$RESOURCE_DIR/caddyfile.media" ]]; then
  if service_enabled "caddy"; then
    install -d "$STACK_DIR/config/caddy" "$STACK_DIR/config/caddy/ui-index"
    install -m 0644 "$RESOURCE_DIR/caddyfile.media" "$STACK_DIR/config/caddy/Caddyfile"
  fi
fi
if [[ -f "$RESOURCE_DIR/media-index.html" ]]; then
  if service_enabled "caddy"; then
    install -d "$STACK_DIR/config/caddy/ui-index"
    install -m 0644 "$RESOURCE_DIR/media-index.html" "$STACK_DIR/config/caddy/ui-index/index.html"
  fi
fi
write_storage_health_snapshot
if [[ -f "$RESOURCE_DIR/jellio-shim.py" ]]; then
  if service_enabled "jellio-shim"; then
    install -d "$STACK_DIR/config/jellio-shim"
    install -m 0644 "$RESOURCE_DIR/jellio-shim.py" "$STACK_DIR/config/jellio-shim/jellio-shim.py"
  fi
fi
if service_enabled "caddy"; then
  chown -R 1000:1000 "$STACK_DIR/config/caddy"
fi
if service_enabled "jellio-shim"; then
  chown -R 1000:1000 "$STACK_DIR/config/jellio-shim"
fi
if service_enabled "jellyfin"; then
  chown -R 1000:1000 "$STACK_DIR/config/jellyfin"
fi
if service_enabled "meilisearch"; then
  chown -R 1000:1000 "$STACK_DIR/config/meilisearch"
fi
if service_enabled "sonarr"; then
  chown -R 1000:1000 "$STACK_DIR/config/sonarr"
fi
if service_enabled "radarr"; then
  chown -R 1000:1000 "$STACK_DIR/config/radarr"
fi
if service_enabled "prowlarr"; then
  chown -R 1000:1000 "$STACK_DIR/config/prowlarr"
fi
if service_enabled "autobrr"; then
  chown -R 1000:1000 "$STACK_DIR/config/autobrr"
fi
if service_enabled "qbittorrent-vpn"; then
  chown -R 1000:1000 "$STACK_DIR/config/qbittorrent"
fi
if service_enabled "seerr"; then
  chown -R 1000:1000 "$STACK_DIR/config/seerr"
fi
if service_enabled "bazarr"; then
  chown -R 1000:1000 "$STACK_DIR/config/bazarr"
fi
if service_enabled "jellystat"; then
  chown -R 1000:1000 "$STACK_DIR/config/jellystat"
fi
if service_enabled "jellystat-db"; then
  chown -R 70:70 "$STACK_DIR/config/jellystat-db"
fi
chown -R 1000:1000 "$STORAGE_PATH"

configure_sabnzbd

docker_compose pull
docker_compose up -d --remove-orphans
recover_jellystat_db
docker_compose up -d jellystat

configure_bazarr() {
  if ! service_enabled "bazarr" || ! service_enabled "sonarr" || ! service_enabled "radarr"; then
    return 0
  fi

  python3 <<'PY'
import json
import os
import pathlib
import time
import urllib.parse
import xml.etree.ElementTree as ET

config_root = pathlib.Path(os.environ.get("CONFIG_PATH", "/opt/media/config"))
bazarr_path = config_root / "bazarr" / "config" / "config.yaml"
targets = {
    "sonarr": {
        "section": "sonarr",
        "url": os.environ.get("SONARR_INTERNAL_URL", "http://sonarr:8989"),
        "base_url": "/",
    },
    "radarr": {
        "section": "radarr",
        "url": os.environ.get("RADARR_INTERNAL_URL", "http://radarr:7878"),
        "base_url": "/",
    },
}

def wait_for_api_key(service):
    path = config_root / service / "config.xml"
    for _ in range(180):
        if path.exists():
            try:
                root = ET.parse(path).getroot()
            except ET.ParseError:
                time.sleep(2)
                continue
            key = (root.findtext("ApiKey") or "").strip()
            if key:
                return key
        time.sleep(2)
    raise RuntimeError(f"could not read API key for {service} from {path}")

def split_url(url):
    parsed = urllib.parse.urlparse(url)
    host = parsed.hostname or parsed.netloc or url
    port = parsed.port or (8989 if host == "sonarr" else 7878)
    return host, port

def yaml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(json.dumps(item) for item in value) + "]"
    return json.dumps(value)

def update_yaml(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = path.read_text().splitlines() if path.exists() else []
    updates = {
        "general": {
            "use_sonarr": True,
            "use_radarr": True,
            "enabled_integrations": ["sonarr", "radarr"],
        },
        "sonarr": {
            "apikey": wait_for_api_key("sonarr"),
            "ip": split_url(targets["sonarr"]["url"])[0],
            "port": split_url(targets["sonarr"]["url"])[1],
            "base_url": targets["sonarr"]["base_url"] or "/",
        },
        "radarr": {
            "apikey": wait_for_api_key("radarr"),
            "ip": split_url(targets["radarr"]["url"])[0],
            "port": split_url(targets["radarr"]["url"])[1],
            "base_url": targets["radarr"]["base_url"] or "/",
        },
    }

    out = []
    section = None
    seen_sections = set()
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        if stripped.endswith(":") and not line.startswith(" "):
            section = stripped[:-1]
            seen_sections.add(section)
            out.append(line)
            i += 1
            section_lines = []
            while i < len(lines) and (lines[i].startswith("  ") or not lines[i].strip()):
                section_lines.append(lines[i])
                i += 1
            if section in updates:
                present = set()
                skip_list_continuation_for = None
                for entry in section_lines:
                    entry_stripped = entry.strip()
                    if skip_list_continuation_for and entry_stripped.startswith("- "):
                        continue
                    if entry_stripped and not entry_stripped.startswith("- "):
                        skip_list_continuation_for = None
                    if ":" in entry_stripped and not entry_stripped.startswith("#"):
                        key = entry_stripped.split(":", 1)[0].strip()
                        if key in updates[section]:
                            out.append(f"  {key}: {yaml_value(updates[section][key])}")
                            present.add(key)
                            skip_list_continuation_for = key
                            continue
                    out.append(entry)
                for key, value in updates[section].items():
                    if key not in present:
                        out.append(f"  {key}: {yaml_value(value)}")
            else:
                out.extend(section_lines)
            continue
        out.append(line)
        i += 1

    for section_name, values in updates.items():
        if section_name not in seen_sections:
            out.append(f"{section_name}:")
            for key, value in values.items():
                out.append(f"  {key}: {yaml_value(value)}")

    path.write_text("\n".join(out).rstrip() + "\n", encoding="utf-8")

update_yaml(bazarr_path)
PY
}

configure_bazarr

if service_enabled "bazarr"; then
  docker_compose up -d bazarr
  docker_compose restart bazarr
fi
