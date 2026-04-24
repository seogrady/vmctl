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

config_dir="${CONFIG_PATH:-/opt/media/config}/qbittorrent/qBittorrent"
install -d "$config_dir" "${QBITTORRENT_DOWNLOADS:-/media/downloads/complete}" "${QBITTORRENT_INCOMPLETE:-/media/downloads/incomplete}"
QBITTORRENT_USERNAME="${QBITTORRENT_USERNAME:-admin}"
QBITTORRENT_PASSWORD="${QBITTORRENT_PASSWORD:-adminadmin}"

cat > "$config_dir/qBittorrent.conf" <<EOF
[Application]
FileLogger\\Enabled=true
FileLogger\\Path=/config/qBittorrent/logs

[BitTorrent]
Session\\DefaultSavePath=${QBITTORRENT_DOWNLOADS:-/media/downloads/complete}
Session\\TempPath=${QBITTORRENT_INCOMPLETE:-/media/downloads/incomplete}
Session\\TempPathEnabled=true

[LegalNotice]
Accepted=true

[Preferences]
WebUI\\Address=*
WebUI\\AuthSubnetWhitelist=10.0.0.0/8,100.64.0.0/10,172.16.0.0/12,192.168.0.0/16
WebUI\\AuthSubnetWhitelistEnabled=true
WebUI\\LocalHostAuth=false
WebUI\\Username=${QBITTORRENT_USERNAME}
WebUI\\Port=${QBITTORRENT_WEBUI_PORT:-8080}
WebUI\\RootFolder=/
EOF

docker_compose up -d qbittorrent-vpn

for _ in $(seq 1 60); do
  if curl -sS "http://localhost:${QBITTORRENT_WEBUI_PORT:-8080}/api/v2/app/version" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

temporary_password="$(docker_compose logs qbittorrent-vpn 2>&1 \
  | sed -n 's/.*temporary password is provided for this session: //p' \
  | tail -1)"

if [[ -n "$temporary_password" ]]; then
  cookie_file="$(mktemp)"
  trap 'rm -f "$cookie_file"' EXIT
  curl -fsS -c "$cookie_file" \
    --data-urlencode "username=admin" \
    --data-urlencode "password=${temporary_password}" \
    "http://localhost:${QBITTORRENT_WEBUI_PORT:-8080}/api/v2/auth/login" >/dev/null
  preferences="$(mktemp)"
  cat > "$preferences" <<JSON
{
  "bypass_auth_subnet_whitelist": "10.0.0.0/8,100.64.0.0/10,172.16.0.0/12,192.168.0.0/16",
  "bypass_auth_subnet_whitelist_enabled": true,
  "bypass_local_auth": true,
  "web_ui_username": "${QBITTORRENT_USERNAME}",
  "web_ui_password": "${QBITTORRENT_PASSWORD}",
  "web_ui_root_folder": "/",
  "save_path": "${QBITTORRENT_DOWNLOADS:-/media/downloads/complete}",
  "temp_path": "${QBITTORRENT_INCOMPLETE:-/media/downloads/incomplete}",
  "temp_path_enabled": true,
  "web_ui_host_header_validation_enabled": false,
  "web_ui_csrf_protection_enabled": false,
  "web_ui_clickjacking_protection_enabled": false
}
JSON
  curl -fsS -b "$cookie_file" \
    --data-urlencode "json=$(cat "$preferences")" \
    "http://localhost:${QBITTORRENT_WEBUI_PORT:-8080}/api/v2/app/setPreferences" >/dev/null
  for _ in $(seq 1 30); do
    if curl -fsS \
      --data-urlencode "username=${QBITTORRENT_USERNAME}" \
      --data-urlencode "password=${QBITTORRENT_PASSWORD}" \
      "http://localhost:${QBITTORRENT_WEBUI_PORT:-8080}/api/v2/auth/login" >/dev/null 2>&1; then
      break
    fi
    sleep 2
  done
  rm -f "$preferences"
fi
