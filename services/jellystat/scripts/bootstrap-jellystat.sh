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

if ! service_enabled "jellystat" || ! service_enabled "jellystat-db"; then
  exit 0
fi

python3 <<'PY'
import hashlib
import json
import os
import time
import urllib.error
import urllib.request

JS_URL = os.environ.get("JELLYSTAT_URL", "http://localhost:3000")
JF_URL = os.environ.get("JELLYFIN_URL", "http://localhost:8096")
JF_BASE_PATH = (os.environ.get("JELLYFIN_BASE_URL") or "").strip().rstrip("/")
if JF_BASE_PATH == "/":
    JF_BASE_PATH = ""
JF_INTERNAL_URL = f"{os.environ.get('JELLYFIN_INTERNAL_URL', 'http://jellyfin:8096').rstrip('/')}{JF_BASE_PATH}"
JF_USER = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
JF_PASSWORD = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")
JS_USER = os.environ.get("JELLYSTAT_USER", JF_USER or "admin")
JS_PASSWORD = os.environ.get("JELLYSTAT_PASSWORD", JF_PASSWORD)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


opener = urllib.request.build_opener(NoRedirect)


def wait_for(url: str, timeout_seconds: int = 180) -> None:
    started = time.time()
    while time.time() - started < timeout_seconds:
        try:
            with opener.open(url, timeout=10):
                return
        except Exception:
            time.sleep(2)
    raise RuntimeError(f"timed out waiting for {url}")


def request_json(method: str, url: str, payload=None, headers=None, allow=(200, 201, 204)):
    data = None
    req_headers = dict(headers or {})
    if payload is not None:
        data = json.dumps(payload).encode()
        req_headers.setdefault("Content-Type", "application/json")
    req = urllib.request.Request(url, data=data, headers=req_headers, method=method)
    try:
        with opener.open(req, timeout=20) as resp:
            body = resp.read().decode()
            return json.loads(body) if body else None
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        raise


def jellyfin_token() -> str:
    auth_headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="bootstrap", DeviceId="vmctl-jellystat", Version="1.0"',
    }
    auth = request_json(
        "POST",
        f"{JF_INTERNAL_URL}/Users/AuthenticateByName",
        {"Username": JF_USER, "Pw": JF_PASSWORD},
        headers=auth_headers,
        allow=(),
    )
    return auth["AccessToken"]


def configure_jellystat():
    configured = request_json("GET", f"{JS_URL}/auth/isConfigured", allow=())
    state = int(configured.get("state", 0))

    password_hash = hashlib.sha3_512(JS_PASSWORD.encode()).hexdigest()

    if state < 1:
        request_json(
            "POST",
            f"{JS_URL}/auth/createuser",
            {"username": JS_USER, "password": password_hash},
            allow=(200, 201, 204, 403),
        )
        configured = request_json("GET", f"{JS_URL}/auth/isConfigured", allow=())
        state = int(configured.get("state", 0))

    if state < 2:
        token = jellyfin_token()
        request_json(
            "POST",
            f"{JS_URL}/auth/configSetup",
            {"JF_HOST": JF_INTERNAL_URL, "JF_API_KEY": token},
            allow=(200, 201, 204),
        )


wait_for(f"{JF_INTERNAL_URL}/System/Info/Public")
wait_for(f"{JS_URL}/auth/isConfigured")
configure_jellystat()
PY

JELLYSTAT_JF_TOKEN="$(
python3 <<'PY'
import json
import os
import urllib.request

JF_BASE_PATH = (os.environ.get("JELLYFIN_BASE_URL") or "").strip().rstrip("/")
if JF_BASE_PATH == "/":
    JF_BASE_PATH = ""
JF_INTERNAL_URL = f"{os.environ.get('JELLYFIN_INTERNAL_URL', 'http://jellyfin:8096').rstrip('/')}{JF_BASE_PATH}"
JF_USER = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
JF_PASSWORD = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")

req = urllib.request.Request(
    f"{JF_INTERNAL_URL}/Users/AuthenticateByName",
    data=json.dumps({"Username": JF_USER, "Pw": JF_PASSWORD}).encode("utf-8"),
    headers={
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="bootstrap", DeviceId="vmctl-jellystat", Version="1.0"',
    },
    method="POST",
)
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


opener = urllib.request.build_opener(NoRedirect)

with opener.open(req, timeout=20) as response:
    payload = json.loads(response.read().decode("utf-8"))
token = payload.get("AccessToken", "").strip()
if not token:
    raise RuntimeError("failed to obtain Jellyfin access token for jellystat")
print(token)
PY
)"

# Disable web login requirement and force Jellystat to use the current Jellyfin runtime endpoint/token.
JELLYSTAT_JF_BASE_PATH="${JELLYFIN_BASE_URL:-}"
JELLYSTAT_JF_BASE_PATH="${JELLYSTAT_JF_BASE_PATH%/}"
if [[ "$JELLYSTAT_JF_BASE_PATH" == "/" ]]; then
  JELLYSTAT_JF_BASE_PATH=""
fi
JELLYSTAT_JF_HOST_SQL="${JELLYFIN_INTERNAL_URL:-http://jellyfin:8096}${JELLYSTAT_JF_BASE_PATH}"
JELLYSTAT_JF_HOST_SQL="${JELLYSTAT_JF_HOST_SQL//\'/\'\'}"
JELLYSTAT_JF_TOKEN_SQL="${JELLYSTAT_JF_TOKEN//\'/\'\'}"

docker_compose exec -T jellystat-db \
  psql -v ON_ERROR_STOP=1 -U "${POSTGRES_USER:-jellystat}" -d "${POSTGRES_DB:-jellystat}" \
  -c "UPDATE app_config SET \"REQUIRE_LOGIN\" = false, \"JF_HOST\" = '${JELLYSTAT_JF_HOST_SQL}', \"JF_API_KEY\" = '${JELLYSTAT_JF_TOKEN_SQL}' WHERE \"ID\" = 1;"

# Jellystat reads connection settings at startup; restart to pick up updated app_config values.
docker_compose restart jellystat

docker_compose exec -T jellystat-db \
  psql -U "${POSTGRES_USER:-jellystat}" -d "${POSTGRES_DB:-jellystat}" \
  -c 'UPDATE app_config SET "REQUIRE_LOGIN" = false WHERE "ID" = 1;'

python3 <<'PY'
import json
import os
import time
import urllib.request
import urllib.error

url = os.environ.get("JELLYSTAT_URL", "http://localhost:3000")
deadline = time.time() + 180
state = 0
last_error = None
while time.time() < deadline:
    try:
        with urllib.request.urlopen(f"{url}/auth/isConfigured", timeout=20) as resp:
            state = int(json.loads(resp.read().decode()).get("state", 0))
        if state >= 2:
            break
    except (urllib.error.URLError, ConnectionResetError) as exc:
        last_error = exc
    time.sleep(2)

if state < 2:
    if last_error is not None:
        raise RuntimeError(f"jellystat failed to reach configured state: {last_error}") from last_error
    raise RuntimeError("jellystat failed to reach configured state")
PY
