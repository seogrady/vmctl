#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"
COMPOSE_FILE="$STACK_DIR/docker-compose.yml"
PLUGIN_DIR="$STACK_DIR/config/jellyfin/data/plugins"

if [[ ! -f "$ENV_FILE" || ! -f "$COMPOSE_FILE" ]]; then
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

install -d "$PLUGIN_DIR"

STREAMYFIN_VERSION="0.66.0.0"
STREAMYFIN_URL="https://github.com/streamyfin/jellyfin-plugin-streamyfin/releases/download/0.66.0.0/streamyfin-0.66.0.0.zip"
STREAMYFIN_MD5="6c4daa669154318ba2b73ba2289ecf2c"

JELLIO_VERSION="1.4.0.0"
JELLIO_URL="https://github.com/InfiniteAvenger/jellio-plus/releases/download/v1.4.0/jellio_1.4.0.0.zip"
JELLIO_MD5="54e908fa8ba0fdb3b40cc10125e0d364"

plugin_changed=0

install_plugin() {
  local name="$1"
  local version="$2"
  local url="$3"
  local checksum="$4"

  local target_dir="${PLUGIN_DIR}/${name}"
  local marker="${target_dir}/.vmctl-version"
  if [[ -f "$marker" ]] && [[ "$(cat "$marker")" == "$version" ]]; then
    return 0
  fi

  local tmp_zip="/tmp/${name}-${version}.zip"
  local tmp_dir="/tmp/${name}-${version}-extract"
  rm -rf "$tmp_dir"
  mkdir -p "$tmp_dir"

  curl -fsSL "$url" -o "$tmp_zip"
  echo "${checksum}  ${tmp_zip}" | md5sum -c -
  rm -rf "$target_dir"
  install -d "$target_dir"
  unzip -o "$tmp_zip" -d "$tmp_dir" >/dev/null
  cp -a "$tmp_dir"/. "$target_dir"/
  printf '%s\n' "$version" >"$marker"
  chown -R 1000:1000 "$target_dir"
  rm -rf "$tmp_zip" "$tmp_dir"
  plugin_changed=1
}

install_plugin "Streamyfin" "$STREAMYFIN_VERSION" "$STREAMYFIN_URL" "$STREAMYFIN_MD5"
install_plugin "Jellio" "$JELLIO_VERSION" "$JELLIO_URL" "$JELLIO_MD5"

if [[ "$plugin_changed" == "1" ]]; then
  docker_compose up -d jellyfin
  docker_compose restart jellyfin
fi
