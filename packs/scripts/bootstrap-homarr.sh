#!/usr/bin/env bash
set -euo pipefail

RESOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"
COMPOSE_FILE="$STACK_DIR/docker-compose.yml"

if [[ ! -f "$ENV_FILE" || ! -f "$COMPOSE_FILE" ]]; then
  exit 0
fi

. "$ENV_FILE"
HOMARR_USER="${HOMARR_USER:-${JELLYFIN_ADMIN_USER:-admin}}"
HOMARR_PASSWORD="${HOMARR_PASSWORD:-${JELLYFIN_ADMIN_PASSWORD:-}}"
MEDIA_SERVICES_CSV="${MEDIA_SERVICES:-}"

service_enabled() {
  local name="$1"
  case ",${MEDIA_SERVICES_CSV}," in
    *,"$name",*) return 0 ;;
    *) return 1 ;;
  esac
}

if ! service_enabled "homarr"; then
  exit 0
fi

if [[ -z "$HOMARR_PASSWORD" ]]; then
  exit 0
fi

compose() {
  docker compose --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"
}

run_homarr_cli() {
  local -a cmd=(
    docker compose --env-file "$ENV_FILE" -f "$COMPOSE_FILE"
    exec -T homarr node /app/apps/cli/cli.cjs "$@"
  )
  timeout 10 "${cmd[@]}" >/dev/null 2>&1 || {
    local rc=$?
    if [[ "$rc" -ne 124 ]]; then
      return "$rc"
    fi
  }
}

wait_for_homarr() {
  local retries=60
  while ((retries > 0)); do
    if curl -fsS "http://127.0.0.1:7575/" >/dev/null 2>&1; then
      return 0
    fi
    retries=$((retries - 1))
    sleep 2
  done
  return 1
}

homarr_db_query() {
  local query="$1"
  python3 - "$query" <<'PY'
import sqlite3
import sys

query = sys.argv[1]
con = sqlite3.connect('/opt/media/config/homarr/db/db.sqlite')
cur = con.cursor()
cur.execute(query)
row = cur.fetchone()
if row and row[0] is not None:
    print(row[0])
PY
}

user_exists() {
  local username="$1"
  local count
  count="$(homarr_db_query "SELECT COUNT(1) FROM user WHERE provider='credentials' AND name='${username}';")"
  [[ "${count:-0}" != "0" ]]
}

first_user_name() {
  homarr_db_query "SELECT name FROM user WHERE provider='credentials' ORDER BY rowid LIMIT 1;"
}

compose up -d homarr
wait_for_homarr

if ! user_exists "$HOMARR_USER"; then
  run_homarr_cli recreate-admin --username "$HOMARR_USER"
fi

TARGET_USER="$HOMARR_USER"
if ! user_exists "$TARGET_USER"; then
  TARGET_USER="$(first_user_name || true)"
fi

if [[ -z "$TARGET_USER" ]]; then
  echo "homarr bootstrap failed: no credentials user exists after recreate-admin" >&2
  exit 1
fi

run_homarr_cli users update-password --username "$TARGET_USER" --password "$HOMARR_PASSWORD"

python3 - <<'PY'
import sqlite3

con = sqlite3.connect('/opt/media/config/homarr/db/db.sqlite')
cur = con.cursor()
cur.execute('SELECT id FROM onboarding LIMIT 1')
row = cur.fetchone()
if row is None:
    cur.execute("INSERT INTO onboarding (id, step, previous_step) VALUES ('vmctl-onboarding', 'completed', 'start')")
else:
    cur.execute("UPDATE onboarding SET previous_step = step, step = 'completed' WHERE id = ?", (row[0],))
con.commit()
PY
