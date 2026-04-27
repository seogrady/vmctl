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

config_dir="${CONFIG_PATH:-/opt/media/config}/autobrr"
install -d "$config_dir"
AUTOBRR_USERNAME="${AUTOBRR_USERNAME:-admin}"
if [[ -z "${AUTOBRR_PASSWORD:-}" ]]; then
  AUTOBRR_PASSWORD="$(python3 - <<'PY'
import secrets
print(secrets.token_hex(20))
PY
)"
fi

python3 <<'PY'
import os
import secrets
from pathlib import Path

config_dir = Path(os.environ.get("CONFIG_PATH", "/opt/media/config")) / "autobrr"
config_dir.mkdir(parents=True, exist_ok=True)
config_path = config_dir / "config.toml"

def env(name: str, default: str = "") -> str:
    return (os.environ.get(name) or default).strip()

session_secret = env("AUTOBRR_SESSION_SECRET") or secrets.token_hex(32)
password = env("AUTOBRR_PASSWORD")
username = env("AUTOBRR_USERNAME", "admin")
base_url = env("AUTOBRR_BASE_URL", "/autobrr/")
if not base_url.startswith("/"):
    base_url = f"/{base_url}"
if not base_url.endswith("/"):
    base_url = f"{base_url}/"

config_path.write_text(
    "\n".join(
        [
            'host = "0.0.0.0"',
            'port = 7474',
            f'baseUrl = "{base_url}"',
            'baseUrlModeLegacy = false',
            f'sessionSecret = "{session_secret}"',
            'customDefinitions = "/config/definitions"',
            'logLevel = "info"',
        ]
    )
    + "\n",
    encoding="utf-8",
)

env_path = Path(os.environ.get("ENV_FILE", "/opt/media/.env"))
lines = env_path.read_text(encoding="utf-8").splitlines() if env_path.exists() else []
out = []
seen = set()
for line in lines:
    if line.startswith("AUTOBRR_SESSION_SECRET="):
        out.append(f"AUTOBRR_SESSION_SECRET={session_secret}")
        seen.add("AUTOBRR_SESSION_SECRET")
    elif line.startswith("AUTOBRR_PASSWORD="):
        out.append(f"AUTOBRR_PASSWORD={password}")
        seen.add("AUTOBRR_PASSWORD")
    elif line.startswith("AUTOBRR_USERNAME="):
        out.append(f"AUTOBRR_USERNAME={username}")
        seen.add("AUTOBRR_USERNAME")
    else:
        out.append(line)
if "AUTOBRR_SESSION_SECRET" not in seen:
    out.append(f"AUTOBRR_SESSION_SECRET={session_secret}")
if "AUTOBRR_PASSWORD" not in seen:
    out.append(f"AUTOBRR_PASSWORD={password}")
if "AUTOBRR_USERNAME" not in seen:
    out.append(f"AUTOBRR_USERNAME={username}")
env_path.write_text("\n".join(out).rstrip() + "\n", encoding="utf-8")
PY

docker_compose up -d autobrr

for _ in $(seq 1 180); do
  if curl -sS --max-time 5 "http://127.0.0.1:7474/" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

if [[ -n "$AUTOBRR_USERNAME" && -n "$AUTOBRR_PASSWORD" ]]; then
  printf '%s\n%s\n' "$AUTOBRR_PASSWORD" "$AUTOBRR_PASSWORD" | docker_compose exec -T autobrr sh -lc "autobrrctl --config /config create-user '$AUTOBRR_USERNAME'" || true
fi
