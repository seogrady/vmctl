#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"
COMPOSE_FILE="$STACK_DIR/docker-compose.yml"

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

if ! service_enabled "meilisearch" || ! service_enabled "jellysearch"; then
  exit 0
fi

docker_compose up -d meilisearch jellysearch

python3 <<'PY'
import time
import urllib.request


def wait_ok(url: str, timeout: int = 240) -> None:
    started = time.time()
    while time.time() - started < timeout:
        try:
            with urllib.request.urlopen(url, timeout=10) as response:
                if 200 <= response.status < 300:
                    return
        except Exception:
            time.sleep(2)
            continue
        time.sleep(2)
    raise RuntimeError(f"timed out waiting for {url}")


wait_ok("http://127.0.0.1:7700/health")
wait_ok("http://127.0.0.1:5000/Items?SearchTerm=test&Limit=1")
PY
