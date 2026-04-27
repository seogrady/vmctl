#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="${STACK_DIR:-/opt/media}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

resolve_env_file() {
  local candidate
  if [[ -n "${ENV_FILE:-}" && -f "${ENV_FILE:-}" ]]; then
    printf '%s\n' "$ENV_FILE"
    return 0
  fi

  for candidate in \
    "$STACK_DIR/.env" \
    "$REPO_ROOT/backend/generated/workspace/resources/media-stack/media.env" \
    "$REPO_ROOT/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/media.env"
  do
    if [[ -f "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  return 1
}

ENV_FILE="$(resolve_env_file || true)"
if [[ -z "$ENV_FILE" ]]; then
  cat >&2 <<EOF
missing media env file
checked:
  - ${ENV_FILE:-<unset>}
  - $STACK_DIR/.env
  - $REPO_ROOT/backend/generated/workspace/resources/media-stack/media.env
  - $REPO_ROOT/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/media.env
EOF
  exit 1
fi

export VMCTL_MEDIA_ENV_FILE="$ENV_FILE"
export VMCTL_MEDIA_STACK_DIR="$STACK_DIR"

set -a
. "$ENV_FILE"
set +a

python3 - "$@" <<'PY'
import json
import os
import shutil
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ENV_FILE = Path(os.environ["VMCTL_MEDIA_ENV_FILE"])
STACK_DIR = Path(os.environ.get("STACK_DIR") or os.environ.get("VMCTL_MEDIA_STACK_DIR") or "/opt/media")
COMPOSE_FILE = STACK_DIR / "docker-compose.yml"
COMPOSE_PROJECT_NAME = os.environ.get("COMPOSE_PROJECT_NAME", "media")
CONFIG_ROOT = Path(os.environ.get("CONFIG_PATH") or str(STACK_DIR / "config"))
VMCTL_HOST_SHORT = (os.environ.get("VMCTL_HOST_SHORT") or "").strip()
VMCTL_RESOURCE_NAME = (os.environ.get("VMCTL_RESOURCE_NAME") or "").strip()
SONARR_URL = (os.environ.get("SONARR_URL") or "").rstrip("/")
RADARR_URL = (os.environ.get("RADARR_URL") or "").rstrip("/")
QBITTORRENT_URL = (os.environ.get("QBITTORRENT_URL") or "").rstrip("/")
JELLYFIN_URL = (os.environ.get("JELLYFIN_INTERNAL_URL") or "http://127.0.0.1:8096").rstrip("/")
JELLYFIN_ADMIN_USER = os.environ.get("JELLYFIN_ADMIN_USER", "admin")
JELLYFIN_ADMIN_PASSWORD = os.environ.get("JELLYFIN_ADMIN_PASSWORD", "")
MEDIA_SERVICES = {
    item.strip()
    for item in (os.environ.get("MEDIA_SERVICES") or "").split(",")
    if item.strip()
}
QBITTORRENT_WEBUI_PORT = int(os.environ.get("QBITTORRENT_WEBUI_PORT", "8080"))
API_KEY_WAIT_SECONDS = int(os.environ.get("VMCTL_MEDIA_CLEANUP_API_KEY_WAIT_SECONDS", "2"))
SONARR_ROOT_FOLDER = Path(os.environ.get("SONARR_ROOT_FOLDER", "/data/media/tv"))
RADARR_ROOT_FOLDER = Path(os.environ.get("RADARR_ROOT_FOLDER", "/data/media/movies"))
QBITTORRENT_DOWNLOADS = Path(os.environ.get("QBITTORRENT_DOWNLOADS", "/data/torrents"))
STATE_FILE = Path("/var/lib/vmctl/download-unpack/processed.json")
COMPATIBILITY_FILE = Path("/var/lib/vmctl/download-unpack/compatibility.json")
COMPATIBILITY_SUMMARY_JSON = Path("/var/lib/vmctl/download-unpack/compatibility-summary.json")
COMPATIBILITY_SUMMARY_TXT = Path("/var/lib/vmctl/download-unpack/compatibility-summary.txt")
STALE_STATE_FILE = Path("/var/lib/vmctl/download-unpack/stale-state.json")
VIDEO_SUFFIXES = {".mkv", ".mp4", ".m4v", ".avi", ".mov", ".wmv", ".ts", ".webm", ".iso"}
RADARR_CATEGORIES = {"radarr", "movies"}
SONARR_CATEGORIES = {"sonarr", "tv", "tv-sonarr"}


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


def service_enabled(name: str) -> bool:
    return name in MEDIA_SERVICES


def docker_compose_available() -> bool:
    try:
        subprocess.run(
            ["docker", "compose", "version"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
            timeout=10,
        )
        return True
    except Exception:
        return False


def docker_available() -> bool:
    try:
        subprocess.run(
            ["docker", "version"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
            timeout=10,
        )
        return True
    except Exception:
        return False


def docker_compose(*args: str, capture_output: bool = True) -> subprocess.CompletedProcess:
    cmd = [
        "docker",
        "compose",
        "-p",
        COMPOSE_PROJECT_NAME,
        "--project-directory",
        str(STACK_DIR),
        "--env-file",
        str(ENV_FILE),
        "-f",
        str(COMPOSE_FILE),
        *args,
    ]
    return subprocess.run(
        cmd,
        check=False,
        capture_output=capture_output,
        text=True,
        timeout=20,
    )


def docker_container_id(service: str) -> str:
    if not docker_available():
        return ""

    candidates = []
    if docker_compose_available():
        result = docker_compose("ps", "-q", service)
        container_id = (result.stdout or "").strip()
        if container_id:
            candidates.append(container_id)

    label_filters = [
        f"label=com.docker.compose.project={COMPOSE_PROJECT_NAME}",
        f"label=com.docker.compose.service={service}",
    ]
    for filters in (label_filters, [f"label=com.docker.compose.service={service}"]):
        inspect = subprocess.run(
            ["docker", "ps", "-q", *sum([["--filter", value] for value in filters], [])],
            check=False,
            capture_output=True,
            text=True,
            timeout=20,
        )
        candidates.extend([line.strip() for line in (inspect.stdout or "").splitlines() if line.strip()])

    for container_id in candidates:
        if container_id:
            return container_id
    return ""


def compose_container_ip(service: str) -> str:
    if not docker_available():
        return ""
    container_id = docker_container_id(service)
    if not container_id:
        return ""
    inspect = subprocess.run(
        [
            "docker",
            "inspect",
            "-f",
            "{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            container_id,
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=20,
    )
    return (inspect.stdout or "").strip()


def probe_http(url: str) -> bool:
    try:
        with urllib.request.urlopen(url, timeout=5):
            return True
    except Exception:
        return False


def resolve_base_url(port: int, direct_candidates: list[str], compose_service: str | None = None) -> str:
    for candidate in direct_candidates:
        candidate = candidate.rstrip("/")
        if candidate and probe_http(candidate):
            return candidate

    if compose_service:
        ip = compose_container_ip(compose_service)
        if ip:
            candidate = f"http://{ip}:{port}"
            if probe_http(candidate):
                return candidate

    return direct_candidates[0].rstrip("/")


def host_base_candidates(port: int) -> list[str]:
    candidates = []
    for raw in [
        f"http://{VMCTL_HOST_SHORT}:{port}" if VMCTL_HOST_SHORT else "",
        f"http://{VMCTL_RESOURCE_NAME}:{port}" if VMCTL_RESOURCE_NAME else "",
    ]:
        if raw and raw not in candidates:
            candidates.append(raw)
    return candidates


def load_json_file(path: Path):
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return {}


def save_json_file(path: Path, payload) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def path_has_media(path: Path) -> bool:
    if path.is_file():
        return path.suffix.lower() in VIDEO_SUFFIXES
    if not path.is_dir():
        return False
    for child in path.rglob("*"):
        if child.is_file() and child.suffix.lower() in VIDEO_SUFFIXES:
            return True
    return False


def read_api_key(app: str) -> str:
    path = CONFIG_ROOT / app / "config.xml"
    started = time.time()
    while time.time() - started < API_KEY_WAIT_SECONDS:
        if path.exists():
            try:
                import xml.etree.ElementTree as ET

                root = ET.parse(path).getroot()
            except Exception:
                time.sleep(1)
                continue
            key = (root.findtext("ApiKey") or "").strip()
            if key:
                return key
        time.sleep(1)

    container_id = docker_container_id(app)
    if container_id:
        result = subprocess.run(
            ["docker", "exec", "-T", container_id, "cat", f"/config/{app}/config.xml"],
            check=False,
            capture_output=True,
            text=True,
            timeout=20,
        )
        if result.returncode == 0 and (result.stdout or "").strip():
            try:
                import xml.etree.ElementTree as ET

                root = ET.fromstring(result.stdout)
                key = (root.findtext("ApiKey") or "").strip()
                if key:
                    return key
            except Exception:
                pass
    raise RuntimeError(f"missing API key for {app} at {path}")


def qbit_login() -> str:
    username = os.environ.get("QBITTORRENT_USERNAME", "admin")
    password = os.environ.get("QBITTORRENT_PASSWORD", "adminadmin")
    data = urllib.parse.urlencode({"username": username, "password": password}).encode("utf-8")
    direct_candidates = []
    for raw in [
        QBITTORRENT_URL,
        f"http://localhost:{QBITTORRENT_WEBUI_PORT}",
        f"http://127.0.0.1:{QBITTORRENT_WEBUI_PORT}",
        *host_base_candidates(QBITTORRENT_WEBUI_PORT),
    ]:
        if raw and raw not in direct_candidates:
            direct_candidates.append(raw)
    base_url = resolve_base_url(
        QBITTORRENT_WEBUI_PORT,
        direct_candidates,
        "gluetun" if service_enabled("qbittorrent-vpn") else "qbittorrent",
    )
    req = urllib.request.Request(f"{base_url}/api/v2/auth/login", data=data, method="POST")
    with urllib.request.urlopen(req, timeout=20) as response:
        cookie = response.headers.get("Set-Cookie", "")
    return cookie.split(";", 1)[0]


def qbit_get(path: str, cookie: str):
    direct_candidates = []
    for raw in [
        QBITTORRENT_URL,
        f"http://localhost:{QBITTORRENT_WEBUI_PORT}",
        f"http://127.0.0.1:{QBITTORRENT_WEBUI_PORT}",
        *host_base_candidates(QBITTORRENT_WEBUI_PORT),
    ]:
        if raw and raw not in direct_candidates:
            direct_candidates.append(raw)
    base_url = resolve_base_url(
        QBITTORRENT_WEBUI_PORT,
        direct_candidates,
        "gluetun" if service_enabled("qbittorrent-vpn") else "qbittorrent",
    )
    req = urllib.request.Request(f"{base_url}{path}", headers={"Cookie": cookie}, method="GET")
    with urllib.request.urlopen(req, timeout=30) as response:
        return json.loads(response.read().decode("utf-8"))


def qbit_post(path: str, cookie: str, payload: dict):
    data = urllib.parse.urlencode(payload).encode("utf-8")
    direct_candidates = []
    for raw in [
        QBITTORRENT_URL,
        f"http://localhost:{QBITTORRENT_WEBUI_PORT}",
        f"http://127.0.0.1:{QBITTORRENT_WEBUI_PORT}",
        *host_base_candidates(QBITTORRENT_WEBUI_PORT),
    ]:
        if raw and raw not in direct_candidates:
            direct_candidates.append(raw)
    base_url = resolve_base_url(
        QBITTORRENT_WEBUI_PORT,
        direct_candidates,
        "gluetun" if service_enabled("qbittorrent-vpn") else "qbittorrent",
    )
    req = urllib.request.Request(
        f"{base_url}{path}",
        data=data,
        headers={"Cookie": cookie, "Content-Type": "application/x-www-form-urlencoded"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=30) as response:
        raw = response.read().decode("utf-8")
        return json.loads(raw) if raw else None


def resolve_path(path_text: str) -> Path | None:
    path_text = str(path_text or "").strip()
    if not path_text:
        return None
    return Path(path_text)


def arr_scan(app: str, folder: str, download_client_id: str) -> None:
    api_key = read_api_key(app)
    if app == "radarr":
        direct_candidates = []
        for raw in [
            RADARR_URL,
            "http://127.0.0.1:7878",
            "http://localhost:7878",
            *host_base_candidates(7878),
        ]:
            if raw and raw not in direct_candidates:
                direct_candidates.append(raw)
        base_url = resolve_base_url(7878, direct_candidates, "radarr")
        command = "DownloadedMoviesScan"
    elif app == "sonarr":
        direct_candidates = []
        for raw in [
            SONARR_URL,
            "http://127.0.0.1:8989",
            "http://localhost:8989",
            *host_base_candidates(8989),
        ]:
            if raw and raw not in direct_candidates:
                direct_candidates.append(raw)
        base_url = resolve_base_url(8989, direct_candidates, "sonarr")
        command = "DownloadedEpisodesScan"
    else:
        raise RuntimeError(f"unsupported app {app}")

    payload = {
        "name": command,
        "path": folder,
        "downloadClientId": download_client_id,
        "importMode": "Move",
    }
    request_json(
        "POST",
        f"{base_url}/api/v3/command",
        payload,
        headers={"X-Api-Key": api_key},
        allow=(200, 201, 202, 204),
    )


def jellyfin_refresh() -> None:
    headers = {
        "Content-Type": "application/json",
        "Authorization": 'MediaBrowser Client="vmctl", Device="cleanup", DeviceId="vmctl-cleanup", Version="1.0"',
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
            "Authorization": 'MediaBrowser Client="vmctl", Device="cleanup", DeviceId="vmctl-cleanup", Version="1.0"',
        },
        allow=(200, 204, 400),
    )


def rebuild_compatibility_summary() -> None:
    reports = load_json_file(COMPATIBILITY_FILE)
    incompatible = []
    for item_id, report in sorted(reports.items(), key=lambda item: item[0]):
        if not isinstance(report, dict):
            continue
        if report.get("compatible", False):
            continue
        incompatible.append(
            {
                "id": item_id,
                "path": report.get("path") or "",
                "container": report.get("container") or "",
                "videoCodecs": report.get("videoCodecs") or [],
                "audioCodecs": report.get("audioCodecs") or [],
                "audioLanguages": report.get("audioLanguages") or [],
                "subtitleCodecs": report.get("subtitleCodecs") or [],
                "reason": report.get("reason") or "unknown",
            }
        )

    payload = {
        "updatedAt": int(time.time()),
        "incompatibleCount": len(incompatible),
        "items": incompatible,
    }
    save_json_file(COMPATIBILITY_SUMMARY_JSON, payload)
    lines = [
        f"incompatible_count={len(incompatible)}",
        f"updated_at={payload['updatedAt']}",
    ]
    for item in incompatible:
        lines.append(
            " | ".join(
                [
                    str(item["id"]),
                    f"path={item['path'] or 'unknown'}",
                    f"container={item['container'] or 'unknown'}",
                    f"video={','.join(item['videoCodecs']) or '-'}",
                    f"audio={','.join(item['audioCodecs']) or '-'}",
                    f"subtitles={','.join(item['subtitleCodecs']) or '-'}",
                    f"reason={item['reason']}",
                ]
            )
        )
    COMPATIBILITY_SUMMARY_TXT.parent.mkdir(parents=True, exist_ok=True)
    COMPATIBILITY_SUMMARY_TXT.write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


def is_under(path: Path, parent: Path) -> bool:
    try:
        path.resolve().relative_to(parent.resolve())
        return True
    except Exception:
        return False


def path_matches_managed(path: Path, managed_paths: set[Path]) -> bool:
    for candidate in managed_paths:
        if path == candidate or is_under(path, candidate):
            return True
    return False


def managed_arr_paths(app: str) -> set[Path]:
    try:
        api_key = read_api_key(app)
    except Exception as exc:
        print(f"warning: {app} path sync skipped: {exc}")
        return set()

    if app == "radarr":
        direct_candidates = []
        for raw in [
            RADARR_URL,
            "http://127.0.0.1:7878",
            "http://localhost:7878",
            *host_base_candidates(7878),
        ]:
            if raw and raw not in direct_candidates:
                direct_candidates.append(raw)
        base_url = resolve_base_url(7878, direct_candidates, "radarr")
        items = request_json("GET", f"{base_url}/api/v3/movie", headers={"X-Api-Key": api_key}) or []
    elif app == "sonarr":
        direct_candidates = []
        for raw in [
            SONARR_URL,
            "http://127.0.0.1:8989",
            "http://localhost:8989",
            *host_base_candidates(8989),
        ]:
            if raw and raw not in direct_candidates:
                direct_candidates.append(raw)
        base_url = resolve_base_url(8989, direct_candidates, "sonarr")
        items = request_json("GET", f"{base_url}/api/v3/series", headers={"X-Api-Key": api_key}) or []
    else:
        raise RuntimeError(f"unsupported app {app}")

    managed: set[Path] = set()
    for item in items:
        if not isinstance(item, dict):
            continue
        path = resolve_path(item.get("path") or "")
        if path is None:
            continue
        managed.add(path)
    return managed


def remove_path(path: Path, dry_run: bool) -> bool:
    if not path.exists():
        return False
    if dry_run:
        return True
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()
    return True


def prune_arr_orphans(dry_run: bool = False, include_seeding: bool = False) -> dict:
    cleanup = {
        "updatedAt": int(time.time()),
        "removed": [],
        "refreshedJellyfin": False,
        "prunedTorrents": [],
    }

    managed_roots = [
        ("sonarr", SONARR_ROOT_FOLDER, managed_arr_paths("sonarr") if service_enabled("sonarr") else set()),
        ("radarr", RADARR_ROOT_FOLDER, managed_arr_paths("radarr") if service_enabled("radarr") else set()),
    ]

    for app, root, managed_paths in managed_roots:
        if not root.exists():
            continue
        for child in sorted(root.iterdir(), key=lambda item: item.name.lower()):
            if path_matches_managed(child, managed_paths):
                continue
            if child.name.startswith("."):
                continue
            if child.is_dir() and not path_has_media(child):
                cleanup["removed"].append(
                    {
                        "id": str(child),
                        "app": app,
                        "path": str(child),
                        "reason": "media folder is not represented in Arr",
                    }
                )
                remove_path(child, dry_run)
            elif child.is_dir() or child.is_file():
                cleanup["removed"].append(
                    {
                        "id": str(child),
                        "app": app,
                        "path": str(child),
                        "reason": "media folder is not represented in Arr",
                    }
                )
                remove_path(child, dry_run)

    if service_enabled("qbittorrent-vpn") or service_enabled("qbittorrent"):
        try:
            cookie = qbit_login()
            torrents = qbit_get("/api/v2/torrents/info", cookie)
        except Exception as exc:
            print(f"warning: qBittorrent prune skipped: {exc}")
        else:
            for torrent in torrents or []:
                if not isinstance(torrent, dict):
                    continue
                is_complete = float(torrent.get("progress") or 0.0) >= 1.0 or int(torrent.get("amount_left") or 1) == 0
                if not is_complete and not include_seeding:
                    continue
                torrent_hash = str(torrent.get("hash") or "").strip().upper()
                if not torrent_hash:
                    continue
                raw_path = resolve_path(torrent.get("content_path") or "")
                if raw_path is None:
                    continue
                content_path = raw_path if raw_path.is_dir() else raw_path.parent
                if content_path == QBITTORRENT_DOWNLOADS or not content_path.exists():
                    continue
                if path_has_media(content_path) and not include_seeding:
                    continue
                if not is_under(content_path, QBITTORRENT_DOWNLOADS):
                    continue
                torrent_state = str(torrent.get("state") or "").strip().lower()
                is_seeding = torrent_state in {"uploading", "stalledup", "forcedup", "queuedup"}
                cleanup["prunedTorrents"].append(
                    {
                        "hash": torrent_hash,
                        "name": torrent.get("name") or "",
                        "path": str(content_path),
                        "reason": "active seeding torrent was explicitly included in prune-orphans"
                        if include_seeding and is_seeding
                        else "torrent download path is missing or no longer contains media",
                    }
                )
                if not dry_run:
                    try:
                        qbit_post(
                            "/api/v2/torrents/delete",
                            cookie,
                            {"hashes": torrent_hash, "deleteFiles": "true"},
                        )
                    except Exception as exc:
                        print(f"warning: failed to delete qBittorrent torrent {torrent_hash}: {exc}")

    if dry_run:
        return cleanup

    if cleanup["removed"] or cleanup["prunedTorrents"]:
        rebuild_compatibility_summary()
        try:
            jellyfin_refresh()
            cleanup["refreshedJellyfin"] = True
        except Exception as exc:
            print(f"warning: Jellyfin refresh skipped: {exc}")

    save_json_file(STALE_STATE_FILE, cleanup)
    return cleanup


def restore_torrents(dry_run: bool = False) -> dict:
    cleanup = {
        "updatedAt": int(time.time()),
        "restored": [],
        "refreshedJellyfin": False,
    }

    if not (service_enabled("qbittorrent-vpn") or service_enabled("qbittorrent")):
        return cleanup
    if not (service_enabled("radarr") or service_enabled("sonarr")):
        return cleanup

    try:
        cookie = qbit_login()
        torrents = qbit_get("/api/v2/torrents/info", cookie)
    except Exception as exc:
        print(f"warning: qBittorrent restore skipped: {exc}")
        save_json_file(STALE_STATE_FILE, cleanup)
        return cleanup
    state = load_json_file(STATE_FILE)
    changed = False

    for torrent in torrents or []:
        if not isinstance(torrent, dict):
            continue
        category = (torrent.get("category") or "").strip().lower()
        if category in RADARR_CATEGORIES:
            app = "radarr"
        elif category in SONARR_CATEGORIES:
            app = "sonarr"
        else:
            continue

        if float(torrent.get("progress") or 0.0) < 1.0 and int(torrent.get("amount_left") or 1) != 0:
            continue

        raw_path = resolve_path(torrent.get("content_path") or torrent.get("save_path") or "")
        content_path = raw_path if raw_path and raw_path.is_dir() else (raw_path.parent if raw_path else None)
        if content_path is None or not content_path.exists() or not content_path.is_dir():
            continue
        if not path_has_media(content_path):
            continue

        torrent_id = str(torrent.get("hash") or "").upper()
        if not torrent_id:
            continue

        existing = state.get(torrent_id, {})
        if existing.get("imported") and existing.get("path") == str(content_path):
            continue

        cleanup["restored"].append(
            {
                "hash": torrent_id,
                "app": app,
                "path": str(content_path),
                "reason": "completed torrent payload was re-scanned into Arr",
            }
        )
        if dry_run:
            continue

        try:
            arr_scan(app, str(content_path), torrent_id)
        except Exception as exc:
            print(f"warning: {app} restore scan skipped for {torrent_id}: {exc}")
            continue
        state[torrent_id] = {
            "app": app,
            "path": str(content_path),
            "imported": True,
            "updatedAt": int(time.time()),
        }
        changed = True

    if dry_run:
        return cleanup

    if changed:
        save_json_file(STATE_FILE, state)
        rebuild_compatibility_summary()
        try:
            jellyfin_refresh()
            cleanup["refreshedJellyfin"] = True
        except Exception as exc:
            print(f"warning: Jellyfin refresh skipped: {exc}")

    save_json_file(STALE_STATE_FILE, cleanup)
    return cleanup


def cleanup_stale_state(dry_run: bool = False) -> dict:
    state = load_json_file(STATE_FILE)
    compatibility = load_json_file(COMPATIBILITY_FILE)
    cleanup = {
        "updatedAt": int(time.time()),
        "removed": [],
        "refreshedJellyfin": False,
    }

    for item_id, entry in list(state.items()):
        path_text = str((entry or {}).get("path") or "").strip()
        path = Path(path_text) if path_text else None
        if path is None or not path_has_media(path):
            cleanup["removed"].append(
                {
                    "id": item_id,
                    "app": (entry or {}).get("app") or "",
                    "path": path_text,
                    "reason": "imported path missing or no longer contains media",
                }
            )
            if not dry_run:
                state.pop(item_id, None)

    for item_id, report in list(compatibility.items()):
        if not isinstance(report, dict):
            continue
        path_text = str(report.get("path") or "").strip()
        path = Path(path_text) if path_text else None
        if path is None or not path_has_media(path):
            cleanup["removed"].append(
                {
                    "id": item_id,
                    "app": "compatibility",
                    "path": path_text,
                    "reason": "compatibility report path missing or no longer contains media",
                }
            )
            if not dry_run:
                compatibility.pop(item_id, None)

    if dry_run:
        return cleanup

    if cleanup["removed"]:
        save_json_file(STATE_FILE, state)
        save_json_file(COMPATIBILITY_FILE, compatibility)
        rebuild_compatibility_summary()
        try:
            jellyfin_refresh()
            cleanup["refreshedJellyfin"] = True
        except Exception as exc:
            print(f"warning: Jellyfin refresh skipped: {exc}")
    save_json_file(STALE_STATE_FILE, cleanup)
    return cleanup


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Prune stale media-stack state and refresh Jellyfin")
    parser.add_argument("--dry-run", action="store_true", help="Report stale entries without modifying state")
    parser.add_argument(
        "--prune-orphans",
        action="store_true",
        help="Also delete media folders not represented in Sonarr or Radarr and stale qBittorrent torrents",
    )
    parser.add_argument(
        "--include-seeding",
        action="store_true",
        help="When pruning orphans, also remove active seeding torrents and their download files",
    )
    parser.add_argument(
        "--restore-torrents",
        action="store_true",
        help="Re-scan completed qBittorrent payloads into Sonarr and Radarr when the local import state is missing",
    )
    args = parser.parse_args()

    cleanup = cleanup_stale_state(dry_run=args.dry_run)
    if args.prune_orphans:
        try:
            prune_report = prune_arr_orphans(dry_run=args.dry_run, include_seeding=args.include_seeding)
        except Exception as exc:
            prune_report = {"removed": [], "prunedTorrents": [], "refreshedJellyfin": False}
            print(f"warning: prune-orphans skipped: {exc}")
        cleanup["prunedOrphans"] = prune_report["removed"]
        cleanup["prunedTorrents"] = prune_report["prunedTorrents"]
        cleanup["prunedJellyfin"] = prune_report["refreshedJellyfin"]
    if args.restore_torrents:
        restore_report = restore_torrents(dry_run=args.dry_run)
        cleanup["restoredTorrents"] = restore_report["restored"]
        cleanup["restoredJellyfin"] = restore_report["refreshedJellyfin"]
    print(json.dumps(cleanup, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
PY
