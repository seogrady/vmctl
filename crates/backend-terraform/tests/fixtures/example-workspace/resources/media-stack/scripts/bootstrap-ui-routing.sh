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

docker_compose up -d caddy
docker_compose restart caddy

python3 <<'PY'
import time
import urllib.error
import urllib.request


def wait_status(path, strict=True):
    url = f"http://127.0.0.1{path}"
    for _ in range(60):
        try:
            req = urllib.request.Request(url, method="GET")
            with urllib.request.urlopen(req, timeout=10) as resp:
                if 200 <= resp.status < 500:
                    return True
        except urllib.error.HTTPError as err:
            if 200 <= err.code < 500:
                return True
        except Exception:
            time.sleep(2)
            continue
        time.sleep(2)
    if strict:
        raise RuntimeError(f"route check failed: {path}")
    print(f"warning: route check failed: {path}")
    return False


wait_status("/healthz")
wait_status("/")
PY

tailscale_https_enabled="${TAILSCALE_HTTPS_ENABLED:-true}"
tailscale_https_target="${TAILSCALE_HTTPS_TARGET:-http://127.0.0.1:80}"
tailscale_funnel_enabled="${TAILSCALE_FUNNEL_ENABLED:-false}"

if [[ "${tailscale_https_enabled,,}" == "false" || "${tailscale_https_enabled}" == "0" ]]; then
  if command -v tailscale >/dev/null 2>&1; then
    tailscale serve reset >/dev/null 2>&1 || true
    tailscale funnel reset >/dev/null 2>&1 || true
  fi
  exit 0
fi

if ! command -v tailscale >/dev/null 2>&1; then
  echo "tailscale not installed; skipping media UI tailnet HTTPS exposure"
  exit 0
fi

if ! tailscale status --json >/tmp/vmctl-tailscale-status.json 2>/dev/null; then
  echo "tailscale is not authenticated; skipping media UI tailnet HTTPS exposure"
  exit 0
fi

tailscale_ready="$(python3 <<'PY'
import json
try:
    with open("/tmp/vmctl-tailscale-status.json", encoding="utf-8") as handle:
        status = json.load(handle)
    print(1 if status.get("BackendState") in {"Running", "Starting"} else 0)
except Exception:
    print(0)
PY
)"
if [[ "$tailscale_ready" != "1" ]]; then
  echo "tailscale backend is not running; skipping media UI tailnet HTTPS exposure"
  exit 0
fi

if [[ "${tailscale_funnel_enabled,,}" == "true" || "${tailscale_funnel_enabled}" == "1" ]]; then
  tailscale funnel --yes --bg "$tailscale_https_target"
else
  tailscale serve --yes --bg "$tailscale_https_target"
fi
