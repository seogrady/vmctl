#!/usr/bin/env bash
set -euo pipefail

missing=()
for package in ca-certificates curl python3; do
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
MEDIA_PATH="${MEDIA_PATH:-/media}"
MEDIA_SERVICES_CSV="${MEDIA_SERVICES:-}"

service_enabled() {
  local name="$1"
  case ",${MEDIA_SERVICES_CSV}," in
    *,"$name",*) return 0 ;;
    *) return 1 ;;
  esac
}

install -d "$STACK_DIR" "$STACK_DIR/config" \
  "$MEDIA_PATH/downloads/complete" "$MEDIA_PATH/downloads/incomplete" \
  "$MEDIA_PATH/movies" "$MEDIA_PATH/tv"
if service_enabled "caddy"; then
  install -d "$STACK_DIR/config/caddy" "$STACK_DIR/config/caddy/ui-index"
fi
if service_enabled "jellyfin"; then
  install -d "$STACK_DIR/config/jellyfin"
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
if service_enabled "qbittorrent-vpn"; then
  install -d "$STACK_DIR/config/qbittorrent"
fi
if service_enabled "jellyseerr"; then
  install -d "$STACK_DIR/config/jellyseerr"
fi
if service_enabled "bazarr"; then
  install -d "$STACK_DIR/config/bazarr"
fi
if service_enabled "homarr"; then
  install -d "$STACK_DIR/config/homarr"
fi
if service_enabled "jellystat"; then
  install -d "$STACK_DIR/config/jellystat"
fi
if service_enabled "jellystat-db"; then
  install -d "$STACK_DIR/config/jellystat-db"
fi
install -m 0644 "$RESOURCE_DIR/docker-compose.media" "$STACK_DIR/docker-compose.yml"
if [[ ! -f "$STACK_DIR/.env" ]]; then
  install -m 0644 "$RESOURCE_DIR/media.env" "$STACK_DIR/.env"
fi

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

ensure_env_value "$STACK_DIR/.env" "SECRET_ENCRYPTION_KEY" "$(random_hex 32)"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_USER" "jellystat"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_DB" "jellystat"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_PASSWORD" "$(random_hex 24)"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_IP" "jellystat-db"
ensure_env_value "$STACK_DIR/.env" "POSTGRES_PORT" "5432"
ensure_env_value "$STACK_DIR/.env" "JWT_SECRET" "$(random_hex 32)"

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

  docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" up -d jellystat-db
  docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" exec -T -u root jellystat-db \
    sh -lc 'chown -R postgres:postgres /var/lib/postgresql/data'
  docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" restart jellystat-db
  sleep 3

  if docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" logs --tail=120 jellystat-db \
    | grep -q "password authentication failed for user"; then
    echo "jellystat-db credential drift detected; recreating database volume"
    docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" stop jellystat jellystat-db || true
    rm -rf "$STACK_DIR/config/jellystat-db"/*
    chown -R 70:70 "$STACK_DIR/config/jellystat-db"
    docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" up -d jellystat-db
  fi
}

if grep -q '^MEDIA_VPN_CONFIGURED=true$' "$STACK_DIR/.env" && grep -q '^MEDIA_VPN_ENABLED=false$' "$STACK_DIR/.env"; then
  echo "media VPN is configured but incomplete; running qBittorrent without VPN until WireGuard values are set"
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
if service_enabled "caddy"; then
  chown -R 1000:1000 "$STACK_DIR/config/caddy"
fi
if service_enabled "jellyfin"; then
  chown -R 1000:1000 "$STACK_DIR/config/jellyfin"
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
if service_enabled "qbittorrent-vpn"; then
  chown -R 1000:1000 "$STACK_DIR/config/qbittorrent"
fi
if service_enabled "jellyseerr"; then
  chown -R 1000:1000 "$STACK_DIR/config/jellyseerr"
fi
if service_enabled "bazarr"; then
  chown -R 1000:1000 "$STACK_DIR/config/bazarr"
fi
if service_enabled "homarr"; then
  chown -R 1000:1000 "$STACK_DIR/config/homarr"
fi
if service_enabled "jellystat"; then
  chown -R 1000:1000 "$STACK_DIR/config/jellystat"
fi
if service_enabled "jellystat-db"; then
  chown -R 70:70 "$STACK_DIR/config/jellystat-db"
fi
chown -R 1000:1000 "$MEDIA_PATH"

docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" pull
docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" up -d --remove-orphans
recover_jellystat_db
docker compose --env-file "$STACK_DIR/.env" -f "$STACK_DIR/docker-compose.yml" up -d jellystat
