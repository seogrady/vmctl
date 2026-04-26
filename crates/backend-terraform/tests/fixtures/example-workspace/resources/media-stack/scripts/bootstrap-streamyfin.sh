#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"

if [[ ! -f "$ENV_FILE" ]]; then
  exit 0
fi

set -a
. "$ENV_FILE"
set +a

MEDIA_SERVICES_CSV="${MEDIA_SERVICES:-}"

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

python3 <<'PY'
import json
import os
import sys
import time
import urllib.error
import urllib.request

PLUGIN_ID = "1e9e5d386e6746158719e98a5c34f004"
base_candidates = []
for candidate in [
    "http://127.0.0.1:8096",
    (os.environ.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/"),
]:
    if candidate not in base_candidates:
        base_candidates.append(candidate)
BASE_PATH = (os.environ.get("JELLYFIN_BASE_URL") or "").strip().rstrip("/")
if BASE_PATH == "/":
    BASE_PATH = ""
ADMIN_USER = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
ADMIN_PASSWORD = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")
SEERR_URL = (os.environ.get("SEERR_INTERNAL_URL") or "http://seerr:5055").rstrip("/")


def request_json(method: str, path: str, payload=None, token=None, allow=(200, 204)):
    url = f"{BASE_URL}{BASE_PATH}{path}"
    body = None
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="bootstrap", DeviceId="vmctl-streamyfin", Version="1.0"',
    }
    if token:
        headers["X-Emby-Token"] = token
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as response:
            raw = response.read().decode("utf-8")
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        raise


BASE_URL = None
for candidate_base in base_candidates:
    BASE_URL = candidate_base
    for _ in range(120):
        try:
        request_json("GET", "/System/Info/Public", allow=(200, 204, 302))
        break
        except Exception:
            time.sleep(2)
    else:
        continue
    break
else:
    raise RuntimeError(f"jellyfin did not become ready at any of: {', '.join(base_candidates)}")

auth = request_json(
    "POST",
    "/Users/AuthenticateByName",
    {"Username": ADMIN_USER, "Pw": ADMIN_PASSWORD},
    allow=(),
)
token = auth["AccessToken"]

config = None
for _ in range(120):
    try:
        config = request_json("GET", f"/Plugins/{PLUGIN_ID}/Configuration", token=token, allow=())
        if config:
            break
    except urllib.error.HTTPError as err:
        if err.code != 404:
            raise
    time.sleep(2)
if config is None:
    print("warning: streamyfin plugin configuration endpoint unavailable; skipping config patch")
    sys.exit(0)

config_root = None
for key in ("Config", "config"):
    candidate = config.get(key)
    if isinstance(candidate, dict):
        config_root = candidate
        break
if config_root is None:
    print("warning: streamyfin payload missing Config object; skipping config patch")
    sys.exit(0)

settings = None
for key in ("settings", "Settings"):
    candidate = config_root.get(key)
    if isinstance(candidate, dict):
        settings = candidate
        break
if settings is None:
    print("warning: streamyfin payload missing settings object; skipping config patch")
    sys.exit(0)

def get_setting(settings_obj: dict, expected_key: str):
    for key, value in settings_obj.items():
        if key.lower() == expected_key.lower() and isinstance(value, dict):
            return value
    return None


changed = False

# Streamyfin v0.66.0.0 schema is Lockable<string> for seerrServerUrl.
seerr = get_setting(settings, "seerrServerUrl")
if isinstance(seerr, dict):
    if seerr.get("value") != SEERR_URL:
        seerr["value"] = SEERR_URL
        changed = True

# Streamyfin v0.66.0.0 schema is Lockable<string[]> for hiddenLibraries.
hidden = get_setting(settings, "hiddenLibraries")
if isinstance(hidden, dict):
    hidden_value = hidden.get("value")
    if not isinstance(hidden_value, list):
        hidden["value"] = []
        changed = True
    elif hidden_value != []:
        hidden["value"] = []
        changed = True

# Streamyfin v0.66.0.0 schema is Lockable<Bitrate?> for defaultBitrate.
# Some plugin responses omit `value` when null; add explicit null before POST.
default_bitrate = get_setting(settings, "defaultBitrate")
if isinstance(default_bitrate, dict) and "value" not in default_bitrate:
    default_bitrate["value"] = None
    changed = True

if changed:
    try:
        request_json(
            "POST",
            f"/Plugins/{PLUGIN_ID}/Configuration",
            config,
            token=token,
            allow=(),
        )
    except urllib.error.HTTPError as err:
        if err.code >= 500:
            print(f"warning: streamyfin configuration patch failed ({err.code}); leaving defaults")
        else:
            raise
PY
