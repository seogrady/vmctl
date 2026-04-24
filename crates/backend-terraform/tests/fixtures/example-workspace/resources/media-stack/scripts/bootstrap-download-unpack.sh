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

if ! service_enabled "qbittorrent-vpn"; then
  exit 0
fi

if ! service_enabled "radarr" && ! service_enabled "sonarr"; then
  exit 0
fi

install -d /usr/local/lib/vmctl /var/lib/vmctl/download-unpack
cat >/usr/local/lib/vmctl/media_download_unpack.py <<'PY'
#!/usr/bin/env python3
import json
import os
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

STACK_DIR = Path("/opt/media")
ENV_FILE = STACK_DIR / ".env"
CONFIG_ROOT = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
DOWNLOAD_ROOT = Path(os.environ.get("QBITTORRENT_DOWNLOADS") or "/media/downloads/complete")
STATE_FILE = Path("/var/lib/vmctl/download-unpack/processed.json")
VIDEO_SUFFIXES = {".mkv", ".mp4", ".m4v", ".avi", ".mov", ".wmv", ".ts", ".webm", ".iso"}
ARCHIVE_SUFFIXES = {".rar", ".r00", ".r01", ".r02", ".zip", ".7z"}
RADARR_CATEGORIES = {"radarr", "movies"}
SONARR_CATEGORIES = {"sonarr", "tv", "tv-sonarr"}


def read_env() -> dict[str, str]:
    env: dict[str, str] = {}
    if not ENV_FILE.exists():
        return env
    for line in ENV_FILE.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or "=" not in stripped:
            continue
        key, value = stripped.split("=", 1)
        env[key] = value
    return env


ENV = read_env()
QBIT_URL = (ENV.get("QBITTORRENT_URL") or "http://localhost:8080").rstrip("/")
QBIT_USERNAME = ENV.get("QBITTORRENT_USERNAME", "admin")
QBIT_PASSWORD = ENV.get("QBITTORRENT_PASSWORD", "adminadmin")
JELLYFIN_URL = (ENV.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/")
JELLYFIN_ADMIN_USER = ENV.get("JELLYFIN_ADMIN_USER", "admin")
JELLYFIN_ADMIN_PASSWORD = ENV.get("JELLYFIN_ADMIN_PASSWORD", "")


def request_json(method: str, url: str, payload=None, headers=None, allow=(200, 204)):
    data = None
    req_headers = dict(headers or {})
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        req_headers.setdefault("Content-Type", "application/json")
    req = urllib.request.Request(url, data=data, headers=req_headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            raw = response.read().decode("utf-8")
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as err:
        if err.code in allow:
            return None
        detail = err.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{method} {url} failed with HTTP {err.code}: {detail}") from err


def wait_for(url: str, timeout_seconds: int = 180):
    started = time.time()
    while time.time() - started < timeout_seconds:
        try:
            with urllib.request.urlopen(url, timeout=10):
                return
        except Exception:
            time.sleep(2)
    raise RuntimeError(f"timed out waiting for {url}")


def read_api_key(app: str) -> str:
    path = CONFIG_ROOT / app / "config.xml"
    started = time.time()
    while time.time() - started < 180:
        if path.exists():
            try:
                import xml.etree.ElementTree as ET

                root = ET.parse(path).getroot()
            except Exception:
                time.sleep(2)
                continue
            key = (root.findtext("ApiKey") or "").strip()
            if key:
                return key
        time.sleep(2)
    raise RuntimeError(f"missing API key for {app} at {path}")


def qbit_login() -> str:
    data = urllib.parse.urlencode({"username": QBIT_USERNAME, "password": QBIT_PASSWORD}).encode("utf-8")
    req = urllib.request.Request(f"{QBIT_URL}/api/v2/auth/login", data=data, method="POST")
    with urllib.request.urlopen(req, timeout=20) as response:
        cookie = response.headers.get("Set-Cookie", "")
    return cookie.split(";", 1)[0]


def qbit_get(path: str, cookie: str):
    req = urllib.request.Request(f"{QBIT_URL}{path}", headers={"Cookie": cookie}, method="GET")
    with urllib.request.urlopen(req, timeout=30) as response:
        return json.loads(response.read().decode("utf-8"))


def arr_scan(app: str, folder: str, download_client_id: str):
    api_key = read_api_key(app)
    if app == "radarr":
        base_url = "http://127.0.0.1:7878"
        command = "DownloadedMoviesScan"
    elif app == "sonarr":
        base_url = "http://127.0.0.1:8989"
        command = "DownloadedEpisodesScan"
    else:
        raise RuntimeError(f"unsupported app {app}")
    payload = {
        "name": command,
        "path": folder,
        "downloadClientId": download_client_id,
        "importMode": "Move",
    }
    request_json("POST", f"{base_url}/api/v3/command", payload, headers={"X-Api-Key": api_key}, allow=(200, 201, 202, 204))


def jellyfin_refresh():
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="unpack", DeviceId="vmctl-unpack", Version="1.0"',
    }
    auth = request_json(
        "POST",
        f"{JELLYFIN_URL}/Users/AuthenticateByName",
        {"Username": JELLYFIN_ADMIN_USER, "Pw": JELLYFIN_ADMIN_PASSWORD},
        headers=headers,
        allow=(),
    )
    token = auth.get("AccessToken")
    if not token:
        return
    request_json(
        "POST",
        f"{JELLYFIN_URL}/Library/Refresh",
        headers={
            "X-Emby-Token": token,
            "Authorization": 'MediaBrowser Client="vmctl", Device="unpack", DeviceId="vmctl-unpack", Version="1.0"',
        },
        allow=(200, 204, 400),
    )


def load_state():
    if not STATE_FILE.exists():
        return {}
    try:
        return json.loads(STATE_FILE.read_text(encoding="utf-8"))
    except Exception:
        return {}


def save_state(state):
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    STATE_FILE.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def has_media_file(path: Path) -> bool:
    for child in path.rglob("*"):
        if child.is_file() and child.suffix.lower() in VIDEO_SUFFIXES:
            return True
    return False


def archive_candidates(path: Path):
    files = [child for child in path.iterdir() if child.is_file() and child.suffix.lower() in ARCHIVE_SUFFIXES]
    if not files:
        return []
    priority = {".rar": 0, ".zip": 1, ".7z": 2, ".r00": 3, ".r01": 4, ".r02": 5}
    return sorted(files, key=lambda item: (priority.get(item.suffix.lower(), 99), item.name.lower()))


def extract_archive(path: Path) -> bool:
    archives = archive_candidates(path)
    if not archives:
        return False
    if has_media_file(path):
        return False
    archive = next((candidate for candidate in archives if candidate.suffix.lower() in {".rar", ".zip", ".7z"}), archives[0])
    subprocess.run(["7z", "x", "-y", f"-o{path}", str(archive)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return True


def process():
    cookie = qbit_login()
    torrents = qbit_get("/api/v2/torrents/info", cookie)
    state = load_state()
    changed = False

    for torrent in torrents:
        category = (torrent.get("category") or "").strip().lower()
        if category in RADARR_CATEGORIES:
            app = "radarr"
        elif category in SONARR_CATEGORIES:
            app = "sonarr"
        else:
            continue

        if float(torrent.get("progress") or 0.0) < 1.0 and int(torrent.get("amount_left") or 1) != 0:
            continue

        content_path = Path(torrent.get("content_path") or torrent.get("save_path") or "")
        if not content_path.exists() or not content_path.is_dir():
            continue

        torrent_id = str(torrent.get("hash") or "").upper()
        if not torrent_id:
            continue
        if state.get(torrent_id, {}).get("imported"):
            continue

        extract_archive(content_path)
        if not has_media_file(content_path):
            continue

        arr_scan(app, str(content_path), torrent_id)
        state[torrent_id] = {
            "app": app,
            "path": str(content_path),
            "imported": True,
            "updatedAt": int(time.time()),
        }
        changed = True

    if changed:
        save_state(state)
        try:
            jellyfin_refresh()
        except Exception as exc:
            print(f"warning: Jellyfin refresh skipped: {exc}")


wait_for("http://127.0.0.1:8080/api/v2/app/version", 240)
wait_for("http://127.0.0.1:7878/ping", 240)
wait_for("http://127.0.0.1:8989/ping", 240)
try:
    process()
except Exception as exc:
    print(f"warning: media unpack/import pass failed: {exc}")
PY
chmod +x /usr/local/lib/vmctl/media_download_unpack.py

cat >/etc/systemd/system/vmctl-media-unpack.service <<EOF
[Unit]
Description=vmctl archive download unpack and import
After=network-online.target docker.service
Wants=network-online.target docker.service

[Service]
Type=oneshot
EnvironmentFile=/opt/media/.env
ExecStart=/usr/local/lib/vmctl/media_download_unpack.py
User=root

[Install]
WantedBy=multi-user.target
EOF

cat >/etc/systemd/system/vmctl-media-unpack.timer <<EOF
[Unit]
Description=Run vmctl media unpack and import periodically

[Timer]
OnBootSec=2m
OnUnitActiveSec=5m
AccuracySec=1m
Persistent=true
Unit=vmctl-media-unpack.service

[Install]
WantedBy=timers.target
EOF

systemctl daemon-reload
systemctl enable --now vmctl-media-unpack.timer
systemctl start vmctl-media-unpack.service || true
