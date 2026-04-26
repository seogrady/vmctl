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

CONFIG_ROOT="${CONFIG_PATH:-/opt/media/config}"
BASE_URL_VALUE="${JELLYFIN_BASE_URL:-/jf}"
JELLYFIN_NETWORK_XML="$CONFIG_ROOT/jellyfin/network.xml"
JELLYFIN_ENCODING_XML="$CONFIG_ROOT/jellyfin/encoding.xml"
mkdir -p "$(dirname "$JELLYFIN_NETWORK_XML")"
export BASE_URL_VALUE
export JELLYFIN_NETWORK_XML
export JELLYFIN_ENCODING_XML
export JELLYFIN_ENV_FILE="$ENV_FILE"

jellyfin_base_updated="$(
python3 <<'PY'
import os
import xml.etree.ElementTree as ET

xml_path = os.environ["JELLYFIN_NETWORK_XML"]
base_url = (os.environ.get("BASE_URL_VALUE") or "").strip()
if not base_url.startswith("/"):
    base_url = f"/{base_url}"
if base_url == "/":
    base_url = ""

root = None
if os.path.exists(xml_path):
    root = ET.parse(xml_path).getroot()
else:
    root = ET.Element("NetworkConfiguration")

node = root.find("BaseUrl")
if node is None:
    node = ET.SubElement(root, "BaseUrl")

current = (node.text or "").strip()
if current == base_url:
    print("0")
else:
    node.text = base_url
    ET.ElementTree(root).write(xml_path, encoding="utf-8", xml_declaration=True)
    print("1")
PY
)"

jellyfin_encoding_updated="$(
python3 <<'PY'
import os
import xml.etree.ElementTree as ET

xml_path = os.environ["JELLYFIN_ENCODING_XML"]
transcoding_temp_path = (os.environ.get("JELLYFIN_TRANSCODING_TEMP_PATH") or "/config/transcodes").strip()
hwaccel_type = (os.environ.get("JELLYFIN_HWACCEL_TYPE") or "qsv").strip()
vaapi_device = (os.environ.get("JELLYFIN_HWACCEL_DEVICE") or "/dev/dri/renderD128").strip()
enable_hardware_encoding = (os.environ.get("JELLYFIN_HWACCEL_ENABLE_ENCODING") or "true").strip().lower() in {"1", "true", "yes", "on"}
enable_tonemapping_raw = (os.environ.get("JELLYFIN_HWACCEL_ENABLE_TONEMAPPING") or "auto").strip().lower()
enable_vpp_tonemapping = (os.environ.get("JELLYFIN_HWACCEL_ENABLE_VPP_TONEMAPPING") or "true").strip().lower() in {"1", "true", "yes", "on"}
enable_10bit_hevc = (os.environ.get("JELLYFIN_HWACCEL_ENABLE_10BIT_HEVC_DECODING") or "true").strip().lower() in {"1", "true", "yes", "on"}
enable_10bit_vp9 = (os.environ.get("JELLYFIN_HWACCEL_ENABLE_10BIT_VP9_DECODING") or "true").strip().lower() in {"1", "true", "yes", "on"}
enable_low_power_h264 = (os.environ.get("JELLYFIN_HWACCEL_ENABLE_INTEL_LOW_POWER_H264") or "true").strip().lower() in {"1", "true", "yes", "on"}
enable_low_power_hevc = (os.environ.get("JELLYFIN_HWACCEL_ENABLE_INTEL_LOW_POWER_HEVC") or "true").strip().lower() in {"1", "true", "yes", "on"}

def probe_opencl_support() -> bool:
    # Jellyfin's Dolby Vision path needs OpenCL on this Intel stack. If the
    # runtime is missing or broken, leave tonemapping off so playback does not
    # hard-fail during FFmpeg device initialization.
    import subprocess

    try:
        subprocess.run(
            [
                "/usr/lib/jellyfin-ffmpeg/ffmpeg",
                "-v",
                "error",
                "-init_hw_device",
                f"vaapi=va:{vaapi_device}",
                "-init_hw_device",
                "opencl=ocl@va",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:d=1",
                "-f",
                "null",
                "-",
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return True
    except Exception:
        return False

if enable_tonemapping_raw in {"", "auto"}:
    enable_tonemapping = probe_opencl_support()
else:
    enable_tonemapping = enable_tonemapping_raw in {"1", "true", "yes", "on"}

root = None
if os.path.exists(xml_path):
    root = ET.parse(xml_path).getroot()
else:
    root = ET.Element("EncodingOptions")

values = {
    "EncodingThreadCount": "-1",
    "TranscodingTempPath": transcoding_temp_path,
    "FallbackFontPath": "",
    "EnableFallbackFont": "false",
    "DownMixAudioBoost": "2",
    "DownMixStereoAlgorithm": "None",
    "MaxMuxingQueueSize": "2048",
    "EnableThrottling": "false",
    "ThrottleDelaySeconds": "180",
    "EnableSegmentDeletion": "false",
    "SegmentKeepSeconds": "720",
    "HardwareAccelerationType": hwaccel_type,
    "EncoderAppPathDisplay": "/usr/lib/jellyfin-ffmpeg/ffmpeg",
    "VaapiDevice": vaapi_device,
    "EnableTonemapping": str(enable_tonemapping).lower(),
    "EnableVppTonemapping": str(enable_vpp_tonemapping).lower(),
    "TonemappingAlgorithm": "bt2390",
    "TonemappingMode": "auto",
    "TonemappingRange": "auto",
    "TonemappingDesat": "0",
    "TonemappingPeak": "100",
    "TonemappingParam": "0",
    "VppTonemappingBrightness": "16",
    "VppTonemappingContrast": "1",
    "EnableHardwareEncoding": str(enable_hardware_encoding).lower(),
    "EnableDecodingColorDepth10Hevc": str(enable_10bit_hevc).lower(),
    "EnableDecodingColorDepth10Vp9": str(enable_10bit_vp9).lower(),
    "PreferSystemNativeHwDecoder": "true",
    "EnableIntelLowPowerH264HwEncoder": str(enable_low_power_h264).lower(),
    "EnableIntelLowPowerHevcHwEncoder": str(enable_low_power_hevc).lower(),
    "AllowHevcEncoding": "true",
}

current = {child.tag: (child.text or "") for child in list(root)}
changed = current != values
if changed:
    for tag, value in values.items():
        node = root.find(tag)
        if node is None:
            node = ET.SubElement(root, tag)
        node.text = value
    ET.ElementTree(root).write(xml_path, encoding="utf-8", xml_declaration=True)
    print("1")
else:
    print("0")
PY
)"

if [[ "$jellyfin_base_updated" == "1" || "$jellyfin_encoding_updated" == "1" ]]; then
  docker_compose up -d jellyfin
  docker_compose restart jellyfin
fi

python3 <<'PY'
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

base_candidates = []
for candidate in [
    "http://127.0.0.1:8096",
    (os.environ.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/"),
]:
    if candidate not in base_candidates:
        base_candidates.append(candidate)
user = os.environ.get("JELLYFIN_ADMIN_USER") or "admin"
password = os.environ.get("JELLYFIN_ADMIN_PASSWORD") or ""
base_url = ""
auto_login_user = (os.environ.get("JELLYFIN_AUTOLOGIN_USER") or "media").strip() or "media"
env_file = Path(os.environ.get("JELLYFIN_ENV_FILE") or "/opt/media/.env")


def call(method, path, payload=None, token=None, allow=(200, 204)):
    data = None
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="bootstrap", DeviceId="vmctl", Version="1.0"',
    }
    if token:
        headers["X-Emby-Token"] = token
    if payload is not None:
        data = json.dumps(payload).encode()
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as response:
            body = response.read().decode()
            if body:
                return json.loads(body)
            return None
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        raise


def _item_locations(item):
    locations = []
    for location in item.get("Locations") or []:
        location = str(location).strip().rstrip("/")
        if location:
            locations.append(location)
    path = str(item.get("Path") or "").strip().rstrip("/")
    if path:
        locations.append(path)
    for path_info in (item.get("LibraryOptions") or {}).get("PathInfos") or []:
        location = str(path_info.get("Path") or "").strip().rstrip("/")
        if location:
            locations.append(location)
    seen = set()
    ordered = []
    for location in locations:
        if location not in seen:
            seen.add(location)
            ordered.append(location)
    return ordered


def ensure_library(name, path, collection_type, token, admin_user_id):
    current = call("GET", "/Library/VirtualFolders", token=token, allow=(200, 204)) or []
    views = call("GET", f"/Users/{admin_user_id}/Views", token=token, allow=(200, 204)) or {}
    view_items = views.get("Items") or []
    desired_path = path.rstrip("/")
    canonical = None
    canonical_locations = []
    duplicates = []
    for item in current:
        item_name = (item.get("Name") or "").strip()
        locations = [str(location).rstrip("/") for location in (item.get("Locations") or []) if str(location).strip()]
        if item_name.lower() == name.lower():
            canonical = item
            canonical_locations = locations
            continue
        if desired_path in locations:
            duplicates.append(item_name)

    if canonical is None:
        for item in view_items:
            item_name = (item.get("Name") or "").strip()
            locations = _item_locations(item)
            if desired_path in locations or item_name.lower() == name.lower():
                canonical = item
                canonical_locations = locations
                break

    for duplicate in duplicates:
        call(
            "DELETE",
            f"/Library/VirtualFolders?name={urllib.parse.quote(duplicate)}",
            token=token,
            allow=(200, 204, 404),
        )

    if canonical is None:
        # If a stale non-canonical view already points at the desired path,
        # do not create a suffixed duplicate library. Refresh and let Jellyfin
        # converge the existing metadata in place.
        if any(desired_path in _item_locations(item) for item in view_items):
            call("POST", "/Library/Refresh", token=token, allow=(200, 204, 400))
            return
        query = urllib.parse.urlencode(
            {
                "name": name,
                "collectionType": collection_type,
                "paths": path,
                "refreshLibrary": "true",
            },
            doseq=True,
        )
        call(
            "POST",
            f"/Library/VirtualFolders?{query}",
            {"LibraryOptions": {"Enabled": True, "PathInfos": [{"Path": path}]}},
            token=token,
            allow=(200, 204, 400),
        )
        if duplicates:
            call("POST", "/Library/Refresh", token=token, allow=(200, 204, 400))
        return

    locations = canonical_locations
    if locations == [desired_path]:
        if duplicates:
            call("POST", "/Library/Refresh", token=token, allow=(200, 204, 400))
        return

    # Jellyfin's library path API mutates Locations through the add/remove
    # endpoints, not the media-path update endpoint. Remove stale paths first,
    # then add the TRaSH-aligned path so the library converges deterministically.
    for location in locations:
        if location == desired_path:
            continue
        call(
            "DELETE",
            f"/Library/VirtualFolders/Paths?name={urllib.parse.quote(name)}&path={urllib.parse.quote(location, safe='')}",
            token=token,
            allow=(200, 204, 404),
        )
    if desired_path not in locations:
        call(
            "POST",
            "/Library/VirtualFolders/Paths?refreshLibrary=true",
            {"Name": name, "Path": desired_path},
            token=token,
            allow=(200, 204, 400),
        )
    # Re-run a refresh so Jellyfin reindexes items against the updated path.
    call("POST", "/Library/Refresh", token=token, allow=(200, 204, 400))


def set_env_value(path: Path, key: str, value: str) -> None:
    lines = path.read_text(encoding="utf-8").splitlines() if path.exists() else []
    out = []
    seen = False
    for line in lines:
        if line.startswith(f"{key}="):
            out.append(f"{key}={value}")
            seen = True
        else:
            out.append(line)
    if not seen:
        out.append(f"{key}={value}")
    path.write_text("\n".join(out).rstrip() + "\n", encoding="utf-8")


def ensure_user(username: str, token: str) -> str:
    users = call("GET", "/Users", token=token, allow=(200, 204)) or []
    for item in users:
        if (item.get("Name") or "").lower() == username.lower():
            return item["Id"]
    created = call("POST", "/Users/New", {"Name": username}, token=token, allow=(200, 204, 400)) or {}
    if created.get("Id"):
        return created["Id"]
    users = call("GET", "/Users", token=token, allow=(200, 204)) or []
    for item in users:
        if (item.get("Name") or "").lower() == username.lower():
            return item["Id"]
    raise RuntimeError(f"failed to create Jellyfin user {username}")


def ensure_blank_password(user_id: str, token: str) -> None:
    call(
        "POST",
        f"/Users/{user_id}/Password",
        {"CurrentPw": "", "NewPw": "", "ResetPassword": False},
        token=token,
        allow=(200, 204, 400),
    )


def try_call(method, path, payload=None, token=None):
    try:
        return call(method, path, payload, token, allow=(200, 204))
    except urllib.error.HTTPError:
        return None


base = None
for candidate_base in base_candidates:
    base = candidate_base
    for _ in range(90):
        try:
            call("GET", "/System/Info/Public", allow=(200, 204, 302))
            break
        except Exception:
            time.sleep(2)
    else:
        continue
    break
else:
    raise RuntimeError(f"Jellyfin did not become ready at any of: {', '.join(base_candidates)}")

try:
    call("POST", "/Startup/Configuration", {
        "UICulture": "en-US",
        "MetadataCountryCode": "US",
        "PreferredMetadataLanguage": "en",
    }, allow=(200, 204, 400))
    if password:
        call("POST", "/Startup/User", {"Name": user, "Password": password}, allow=(200, 204, 400))
    call("POST", "/Startup/RemoteAccess", {
        "EnableRemoteAccess": True,
        "EnableAutomaticPortMapping": False,
    }, allow=(200, 204, 400))
    call("POST", "/Startup/Complete", allow=(200, 204, 400))
except urllib.error.HTTPError:
    pass

token = None
auth = None
if password:
    auth = try_call("POST", "/Users/AuthenticateByName", {"Username": user, "Pw": password})
if not auth:
    startup_user = try_call("GET", "/Startup/User")
    existing_user = startup_user.get("Name") if startup_user else None
    if existing_user:
        auth = try_call("POST", "/Users/AuthenticateByName", {"Username": existing_user, "Pw": ""})
token = auth.get("AccessToken") if auth else None

if token:
    info = try_call("GET", "/System/Info/Public", token=token) or {}
    server_id = (info.get("Id") or "").strip()
    admin_user_id = ensure_user(user, token)
    network = try_call("GET", "/System/Configuration/network", token=token) or {}
    if not network.get("EnablePublishedServerUriByRequest"):
        network["EnablePublishedServerUriByRequest"] = True
        call("POST", "/System/Configuration/network", network, token=token, allow=(200, 204, 400))

    config = try_call("GET", "/System/Configuration", token=token) or {}
    auto_user_id = ensure_user(auto_login_user, token)
    ensure_blank_password(auto_user_id, token)
    auto_auth = try_call("POST", "/Users/AuthenticateByName", {"Username": auto_login_user, "Pw": ""})
    auto_token = (auto_auth or {}).get("AccessToken") or token

    if config.get("AutoLoginUserId") != auto_user_id:
        config["AutoLoginUserId"] = auto_user_id
        call("POST", "/System/Configuration", config, token=token, allow=(200, 204, 400))

    if config.get("BaseUrl") != base_url:
        config["BaseUrl"] = base_url
        call("POST", "/System/Configuration", config, token=token, allow=(200, 204, 400))
    for name, path, collection_type in [
        ("Movies", "/data/media/movies", "movies"),
        ("TV", "/data/media/tv", "tvshows"),
    ]:
        os.makedirs(path, exist_ok=True)
        ensure_library(name, path, collection_type, token, admin_user_id)
    call("POST", "/Library/Refresh", token=token, allow=(200, 204, 400))
    set_env_value(env_file, "JELLYFIN_AUTOLOGIN_USER", auto_login_user)
    set_env_value(env_file, "JELLYFIN_AUTO_AUTH_TOKEN", auto_token)
    autologin_params = urllib.parse.urlencode(
        {
            "serverid": server_id,
            "serverId": server_id,
            "userid": auto_user_id,
            "userId": auto_user_id,
            "api_key": auto_token,
            "accessToken": auto_token,
        }
    )
    default_public_base = f"http://{os.environ.get('VMCTL_RESOURCE_NAME', 'media-stack')}"
    autologin_base = (os.environ.get("VMCTL_HTTP_BASE_URL_SHORT") or default_public_base).rstrip("/")
    autologin_url = f"{autologin_base}:8097/web/#/home.html?{autologin_params}"
    set_env_value(env_file, "JELLYFIN_AUTOLOGIN_URL", autologin_url)
    ui_index = Path("/opt/media/config/caddy/ui-index")
    ui_index.mkdir(parents=True, exist_ok=True)
    (ui_index / "jellyfin-autologin.url").write_text(autologin_url + "\n", encoding="utf-8")
PY

if docker_compose config --services | grep -qx "caddy"; then
  set -a
  . "$ENV_FILE"
  set +a
  docker_compose up -d --force-recreate caddy
fi
