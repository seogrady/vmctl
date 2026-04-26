# Resilient Media Download/Import/Playback Pipeline Remediation Plan (Production-Ready)

Current date: 2026-04-26 (Australia/Melbourne)

## 1. Summary

This plan makes the end-to-end pipeline resilient, deterministic, and self-validating under real-world partial availability:

`Seerr request -> Radarr/Sonarr search -> Prowlarr indexers -> Download clients (SABnzbd +/or qBittorrent) -> Arr import/organize -> Jellyfin library + metadata -> Jellystat -> Stremio playback via Jellyfin proxy/addon`

Primary remediation themes:

- Treat SABnzbd and qBittorrent as independently optional services.
- Introduce a deterministic “usable download client” selection model (enabled + configured + healthy).
- Gate indexers and Arr download clients by “usable protocols” so Arr never grabs a release it cannot send.
- Make `vmctl apply` fail early and clearly when no usable download path exists.
- Add post-provision validation that asserts the full pipeline invariants (including the fallback behavior) and becomes the regression test harness (TDD).
- Add a media compatibility strategy for Stremio (codec/container/audio) based on evidence and `ffprobe`, with a minimal remediation path when needed.

Repo grounding (source of truth in this repo today):

- Service list and compose rendering: [packs/roles/media_stack.toml](/root/vmctl/packs/roles/media_stack.toml), [packs/templates/docker-compose.media.hbs](/root/vmctl/packs/templates/docker-compose.media.hbs)
- Stack env template: [packs/templates/media.env.hbs](/root/vmctl/packs/templates/media.env.hbs)
- Bootstrap scripts (generated from fixtures today): [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts)
- Current “streaming stack” validation script (extend to cover download pipeline deterministically): [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-validate-streaming-stack.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-validate-streaming-stack.sh)

## 2. Current Symptoms

### 2.1 Radarr Requests Not Reaching qBittorrent

Example: “Bluey at the Cinema - Let’s Play Chef Collection (2025)”

Observed behavior:

- Requested in Seerr
- Picked up by Radarr
- Radarr status shows `Missing`
- Never sent to qBittorrent

Environment constraints:

- Usenet is intended to be priority 1, torrents priority 2
- SABnzbd may be disabled, or running but not usable
- Required `SABNZBD_*` env vars may be missing
- `SABNZBD_SERVER_ENABLE=false` may be set, or effectively “unset” in current templates

Expected behavior:

- If SABnzbd is disabled/unconfigured/unhealthy, provisioning must bypass it and route directly to qBittorrent.
- Radarr/Sonarr must never get stuck on a protocol whose download client is unavailable.

### 2.2 Optional Download Clients

Requirements:

- qBittorrent optional
- SABnzbd optional
- If disabled, the service does not run (compose omission)
- If enabled but not configured enough, provisioning bypasses it (do not configure Arr to use it)
- Selection must be deterministic and testable
- Sonarr/Radarr should only be configured with healthy/usable clients

### 2.3 Many “Missing” / “Not Available” Items

Need to determine whether the cause is:

- Searches not triggered after Seerr requests
- Invalid/missing download clients
- Indexers not returning results
- Quality/profile restrictions blocking grabs
- Unmonitored items
- Wrong categories/root folders
- Import failures after download
- qBittorrent/SABnzbd API calls failing

### 2.4 Periodic Recovery Service

Need a decision:

- Whether an additional “stuck request recovery” service is needed
- Whether it would duplicate Arr behavior
- Whether fixing provisioning + relying on built-in monitoring is sufficient

### 2.5 Completed Downloads Not Appearing in Jellyfin

Potential causes:

- Wrong download directories
- Wrong import directories
- Mismatched `/data` paths between containers
- Hardlink/copy/move problems
- Arr import failures
- Jellyfin library scan not triggered
- Permissions issues

Clarify responsibility:

- qBittorrent/SABnzbd: download to `/data/torrents` or `/data/usenet/*`
- Radarr/Sonarr: import + rename/organize into `/data/media/*`
- Jellyfin: scan libraries under `/data/media/*`
- Jellystat: reads Jellyfin, does not import content

### 2.6 Stremio Playback Failure for Specific Media

Example: “Apex (2026)”

Observed behavior:

- Movie appears in Jellyfin catalog
- Does not play in Stremio, stuck loading

Potential causes:

- Unsupported codec/container/audio/subtitle
- Bitrate too high for client
- Stremio addon returning direct stream that the client cannot decode
- Jellyfin transcoding not being used (or not possible with current stream endpoint)
- Proxy/range request/header issues (Caddy or client)

## 3. Expected Resilient Pipeline

### 3.1 Pipeline Invariants (Must Always Hold)

- All containers see a single canonical path root: `/data`.
- Downloads and media libraries are on the same filesystem mount inside containers:
  - Torrents: `/data/torrents/{movies,tv}`
  - Usenet: `/data/usenet/{incomplete,complete}/{movies,tv}`
  - Libraries: `/data/media/{movies,tv}`
- Sonarr/Radarr root folders match Jellyfin library paths exactly.
- Arr never has an enabled indexer for a protocol without a usable download client for that protocol.
- Download client selection is computed once per apply run and reused (DRY) by:
  - Arr provisioning
  - Prowlarr provisioning
  - Validation
  - Any recovery tooling
- `vmctl apply` is idempotent: repeated applies converge configuration without creating duplicates.

### 3.2 Deterministic Download Routing (Protocol Gating)

We do not “fallback per-release” when a protocol’s client is broken. We “disable the protocol” upstream so Arr never chooses it.

Rules:

- If SABnzbd is usable: allow Usenet protocol
- If SABnzbd is not usable: disable Usenet protocol end-to-end (no usenet indexers in Arr, no SAB download clients configured)
- If qBittorrent is usable: allow torrent protocol
- If qBittorrent is not usable: disable torrent protocol end-to-end
- If no protocols are allowed: fail provisioning clearly and stop

Effect:

- When SAB is down/unconfigured, Arr only sees torrent indexers and a torrent download client, so it will reliably route to qBittorrent.

## 4. Root-Cause Hypotheses

This section lists likely root causes mapped to the current repo state and how to confirm/reject them.

### 4.1 Radarr Not Sending to qBittorrent

Hypothesis A: SABnzbd is present but not usable, yet is configured in Radarr/Sonarr as priority 1, causing the pipeline to try NZBs and stall.

- Why likely in this repo: current env template sets `SABNZBD_SERVER_ENABLE=` empty and `SABNZBD_SERVER_HOST=` empty. Current SAB config generation in `bootstrap-media.sh` treats empty enable as “true” and defaults empty host to `127.0.0.1`, effectively enabling a non-functional server definition.
- Evidence:
  - Radarr history contains “Unable to send to SABnzbd” or download client errors.
  - Radarr/Sonarr `GET /api/v3/health` shows download client connectivity warnings.
  - Prowlarr has NZB indexers enabled and synced into Arr.
  - Radarr indexers include Usenet results and Radarr chooses them.

Hypothesis B: qBittorrent is not reachable from Radarr/Sonarr containers due to VPN routing/network mode.

- Why plausible: qBittorrent container uses `network_mode: service:gluetun`, so the correct hostname inside docker is `gluetun` when VPN enabled.
- Evidence:
  - Radarr download client test fails.
  - From Radarr container, `curl http://gluetun:8080/api/v2/app/version` fails.

Hypothesis C: Indexer sync to Radarr is incomplete or wrong; Radarr is not getting torrent indexers/categories, so there is nothing to grab.

- Evidence:
  - Radarr `GET /api/v3/indexer` shows missing torrent indexers.
  - Prowlarr `GET /api/v1/applications` shows Radarr integration misconfigured.

### 4.2 Optional Download Clients Not Truly Optional

Hypothesis: Service enablement and “usable client” are conflated; the stack can run SABnzbd but it should not be considered usable unless configured and healthy.

- Evidence:
  - SABnzbd container is running, but server config is missing or disabled.
  - Arr still has SAB download clients configured and usenet indexers synced.

### 4.3 “Missing” / “Not Available” Items

Hypothesis A: Searches are not being triggered after Seerr requests.

- Evidence:
  - Seerr request exists, but Radarr history shows no “search” or “grab” attempt around the request time.
  - Seerr settings show `preventSearch=true` or integration missing.

Hypothesis B: Indexers return results but are rejected due to quality profile / custom format / availability constraints.

- Evidence:
  - Radarr `GET /api/v3/history` shows “Rejected” events with reasons.
  - Radarr release results exist but all are rejected. Confirm with:
    - `RADARR_MOVIE_ID="$(curl -fsS -H \"X-Api-Key: $RADARR_KEY\" http://localhost:7878/api/v3/movie | jq -r '.[] | select(.title==\"Apex\") | .id' | head -n1)"`
    - `curl -fsS -H "X-Api-Key: $RADARR_KEY" "http://localhost:7878/api/v3/release?movieId=$RADARR_MOVIE_ID" | jq .`

Hypothesis C: Items are unmonitored or have wrong root folder/category mapping.

- Evidence:
  - `monitored=false` in Radarr movie payloads.
  - Root folder not set to `/data/media/movies` or `/data/media/tv`.

Hypothesis D: Download client API calls fail (auth/host/wrong categories).

- Evidence:
  - Arr health errors: “Download client not available”.
  - qBittorrent API login fails or categories are missing.
  - SABnzbd API key missing or wizard still active.

### 4.4 Completed Downloads Not Appearing in Jellyfin

Hypothesis A: Arr is not importing (Completed Download Handling broken), leaving files in download directories.

- Evidence:
  - qBittorrent shows completed torrents in `/data/torrents/movies` or `/data/torrents/tv` but `/data/media/movies` and `/data/media/tv` are unchanged.
  - Radarr history shows “Import failed” entries.

Hypothesis B: Jellyfin libraries point to wrong paths or library refresh isn’t happening.

- Evidence:
  - Jellyfin VirtualFolders do not include `/data/media/movies` and `/data/media/tv`.
  - Jellyfin logs show filesystem access/permission errors.

Hypothesis C: Permissions mismatch.

- Evidence:
  - Media files under `/data/media` are not readable by Jellyfin (linuxserver container uses PUID/PGID 1000:1000).

### 4.5 Stremio Playback Failures

Hypothesis A: Codec/container/audio unsupported by the Stremio client/device (especially Tizen).

- Evidence:
  - `ffprobe` shows HEVC Main10, Dolby Vision, TrueHD, DTS-HD, or unusual subtitles.
  - Jellyfin can transcode when using Jellyfin clients, but Stremio path is direct streaming without transcode.

Hypothesis B: Streaming path requires proper range requests/headers and proxy is interfering.

- Evidence:
  - `curl -I` against stream URL doesn’t show `Accept-Ranges: bytes`, or range requests return 416/403/500.
  - Caddy logs show upstream errors or header munging.

## 5. Investigation Checklist

All commands below assume you are on the media VM (the stack host) where `/opt/media` exists.

### 5.1 Baseline: Confirm Running Services and Paths

1. Confirm env and services list:
   - `cd /opt/media && sed -n '1,200p' .env`
   - Verify `MEDIA_SERVICES=` includes/excludes `sabnzbd` and `qbittorrent-vpn` as expected.
2. Confirm containers:
   - `cd /opt/media && docker compose --env-file .env -p media ps`
3. Confirm filesystem invariants:
   - `ls -la /data`
   - `ls -la /data/torrents /data/usenet /data/media`

Expected healthy state:

- `/data/torrents`, `/data/usenet`, `/data/media` exist
- Containers that are “disabled” are absent from the compose file, not just stopped

### 5.2 Seerr: Request Triggering and Integrations

Logs:

- `docker compose --env-file .env -p media logs --tail=300 seerr`
- Seerr config file: `/opt/media/config/seerr/settings.json`

API checks:

- `curl -fsS http://localhost:5055/api/v1/status`
- `curl -fsS http://localhost:5055/api/v1/settings/public`
- List requests (needs API key if protected; in this stack it is usually local/open):
  - `curl -fsS http://localhost:5055/api/v1/request?take=50&skip=0`

Hypothesis confirmation:

- Confirm `settings.json` has both `sonarr` and `radarr` entries with `preventSearch=false`.
- Confirm Seerr logs show request → Arr call.

Expected healthy state:

- A request creates a monitored item in Radarr/Sonarr and a search is initiated.

### 5.3 Radarr/Sonarr: Requests, Searches, Health, and Download Clients

Logs:

- Radarr: `/opt/media/config/radarr/logs/` and `docker compose logs radarr`
- Sonarr: `/opt/media/config/sonarr/logs/` and `docker compose logs sonarr`

API checks (Radarr):

- `RADARR_KEY="$(python3 -c 'import xml.etree.ElementTree as ET; print(ET.parse(\"/opt/media/config/radarr/config.xml\").getroot().findtext(\"ApiKey\") or \"\")')"`
- Base URL note:
  - When running on the media VM, `localhost:7878` is correct.
  - When running from another machine, use `media-stack:7878` (or the VM IP) instead.
  - Example: `curl -fsS -H "X-Api-Key: $RADARR_KEY" http://media-stack:7878/api/v3/queue | jq .`
- `curl -fsS -H "X-Api-Key: $RADARR_KEY" http://localhost:7878/api/v3/system/status`
- `curl -fsS -H "X-Api-Key: $RADARR_KEY" http://localhost:7878/api/v3/health`
- `curl -fsS -H "X-Api-Key: $RADARR_KEY" http://localhost:7878/api/v3/downloadclient`
- `curl -fsS -H "X-Api-Key: $RADARR_KEY" http://localhost:7878/api/v3/indexer`
- `curl -fsS -H "X-Api-Key: $RADARR_KEY" http://localhost:7878/api/v3/queue`
- `curl -fsS -H "X-Api-Key: $RADARR_KEY" http://localhost:7878/api/v3/history?page=1&pageSize=50&sortKey=date&sortDirection=descending`
- Missing list (server-side view):
  - `curl -fsS -H "X-Api-Key: $RADARR_KEY" "http://localhost:7878/api/v3/wanted/missing?page=1&pageSize=50&sortKey=title&sortDirection=ascending" | jq .`
- Force a deterministic repro search (use sparingly; good for investigation/TDD):
  - `RADARR_MOVIE_ID="$(curl -fsS -H \"X-Api-Key: $RADARR_KEY\" http://localhost:7878/api/v3/movie | jq -r '.[] | select(.title==\"Apex\") | .id' | head -n1)"`
  - `curl -fsS -H "X-Api-Key: $RADARR_KEY" -H "Content-Type: application/json" --data "{\"name\":\"MoviesSearch\",\"movieIds\":[$RADARR_MOVIE_ID]}" http://localhost:7878/api/v3/command | jq .`
- Find a specific movie:
  - `curl -fsS -H "X-Api-Key: $RADARR_KEY" "http://localhost:7878/api/v3/movie?sortKey=title&sortDirection=ascending"`

API checks (Sonarr):

- `SONARR_KEY="$(python3 -c 'import xml.etree.ElementTree as ET; print(ET.parse(\"/opt/media/config/sonarr/config.xml\").getroot().findtext(\"ApiKey\") or \"\")')"`
- `curl -fsS -H "X-Api-Key: $SONARR_KEY" http://localhost:8989/api/v3/system/status`
- `curl -fsS -H "X-Api-Key: $SONARR_KEY" http://localhost:8989/api/v3/health`
- `curl -fsS -H "X-Api-Key: $SONARR_KEY" http://localhost:8989/api/v3/downloadclient`
- `curl -fsS -H "X-Api-Key: $SONARR_KEY" http://localhost:8989/api/v3/indexer`
- `curl -fsS -H "X-Api-Key: $SONARR_KEY" http://localhost:8989/api/v3/queue`
- `curl -fsS -H "X-Api-Key: $SONARR_KEY" http://localhost:8989/api/v3/history?page=1&pageSize=50&sortKey=date&sortDirection=descending`
- Missing list:
  - `curl -fsS -H "X-Api-Key: $SONARR_KEY" "http://localhost:8989/api/v3/wanted/missing?page=1&pageSize=50&sortKey=series.title&sortDirection=ascending" | jq .`
- Force a deterministic repro search (use sparingly; good for investigation/TDD):
  - `SONARR_SERIES_ID="$(curl -fsS -H \"X-Api-Key: $SONARR_KEY\" http://localhost:8989/api/v3/series | jq -r '.[] | select(.title==\"Bluey\") | .id' | head -n1)"`
  - `curl -fsS -H "X-Api-Key: $SONARR_KEY" -H "Content-Type: application/json" --data "{\"name\":\"SeriesSearch\",\"seriesId\":$SONARR_SERIES_ID}" http://localhost:8989/api/v3/command | jq .`

Hypothesis confirmation:

- If SAB is unconfigured/unhealthy, `downloadclient` must not include “SABnzbd”, and `indexer` must not include Usenet indexers.
- If SAB is usable, both “SABnzbd” and “qBittorrent” should exist, with SAB priority 1 and qB priority 2.

Expected healthy state:

- `/api/v3/health` has no download client connectivity errors.
- `/api/v3/history` shows “Grabbed” entries followed by import activity.

### 5.4 Prowlarr: Indexers, App Sync, and Health

Logs:

- `/opt/media/config/prowlarr/logs/` and `docker compose logs prowlarr`

API checks:

- `PROWLARR_KEY="$(python3 -c 'import xml.etree.ElementTree as ET; print(ET.parse(\"/opt/media/config/prowlarr/config.xml\").getroot().findtext(\"ApiKey\") or \"\")')"`
- `curl -fsS -H "X-Api-Key: $PROWLARR_KEY" http://localhost:9696/api/v1/system/status`
- `curl -fsS -H "X-Api-Key: $PROWLARR_KEY" http://localhost:9696/api/v1/health`
- `curl -fsS -H "X-Api-Key: $PROWLARR_KEY" http://localhost:9696/api/v1/indexer`
- `curl -fsS -H "X-Api-Key: $PROWLARR_KEY" http://localhost:9696/api/v1/applications`
- `curl -fsS -H "X-Api-Key: $PROWLARR_KEY" http://localhost:9696/api/v1/indexerproxy`

Hypothesis confirmation:

- Validate that Radarr and Sonarr are registered applications and enabled.
- Validate that indexers align with usable protocols (torrent-only if SAB unusable).

Expected healthy state:

- `/api/v1/health` is empty or contains only benign warnings.
- App sync is enabled and categories are correct per app.

### 5.5 qBittorrent: Health, Auth, Categories, and Save Paths

Logs:

- qBittorrent file logs in `/opt/media/config/qbittorrent/qBittorrent/logs/`
- Container logs: `docker compose logs qbittorrent-vpn` and `docker compose logs gluetun`

API checks (host):

- `curl -fsS http://localhost:8080/api/v2/app/version`
- Login check:
  - `curl -fsS --data-urlencode "username=$(grep -E '^QBITTORRENT_USERNAME=' /opt/media/.env | cut -d= -f2-)" --data-urlencode "password=$(grep -E '^QBITTORRENT_PASSWORD=' /opt/media/.env | cut -d= -f2-)" http://localhost:8080/api/v2/auth/login`
- Categories:
  - Obtain cookie and call:
    - `curl -fsS -b "$COOKIE" http://localhost:8080/api/v2/torrents/categories`

Connectivity checks (from Arr containers when VPN is enabled):

- `docker compose exec -T radarr sh -lc 'python3 - <<PY\nimport urllib.request\nprint(urllib.request.urlopen(\"http://gluetun:8080/api/v2/app/version\", timeout=10).read().decode())\nPY'`

Expected healthy state:

- qBittorrent is reachable at `gluetun:8080` from other containers when VPN is enabled.
- Categories exist: `tv` and `movies`, and save paths are `/data/torrents/tv` and `/data/torrents/movies`.

### 5.6 SABnzbd: Usability (Not Just “Running”)

Logs:

- `docker compose logs sabnzbd`
- Config: `/opt/media/config/sabnzbd/sabnzbd.ini`

API checks:

- `SAB_KEY="$(python3 -c 'import configparser; p=configparser.ConfigParser(); t=open(\"/opt/media/config/sabnzbd/sabnzbd.ini\",\"r\",encoding=\"utf-8\").read(); p.read_string(\"[root]\\n\"+t); print((p.get(\"misc\",\"api_key\",fallback=\"\") or \"\").strip())')"`
- `curl -fsS "http://localhost:8085/api?mode=version&apikey=$SAB_KEY"`
- Wizard redirect check:
  - `curl -sS -D - -o /dev/null http://localhost:8085/ | head -n 20`
- Categories:
  - `curl -fsS "http://localhost:8085/api?mode=get_cats&apikey=$SAB_KEY"`
- Server config and status:
  - `curl -fsS "http://localhost:8085/api?mode=get_config&section=servers&apikey=$SAB_KEY"`
  - `curl -fsS "http://localhost:8085/api?mode=server_stats&apikey=$SAB_KEY"`

Definition of “usable SABnzbd” (must be true):

- SAB API reachable
- Wizard not active
- At least one server is configured with `enable=1`
- Server stats show an enabled server is not in a permanent error state (for example “cannot connect”)

Expected healthy state:

- SAB is only considered “usable” when a real Usenet provider is configured and enabled.

### 5.7 Import Stage: Arr Import -> Jellyfin Refresh -> Jellystat

Checks:

1. Confirm the periodic import helper is running (this repo currently provisions it via `bootstrap-download-unpack.sh`):
   - `systemctl status vmctl-media-unpack.timer --no-pager`
   - `systemctl status vmctl-media-unpack.service --no-pager`
   - `journalctl -u vmctl-media-unpack.service -n 200 --no-pager`
2. Confirm downloads exist:
   - `find /data/torrents -maxdepth 3 -type f | head`
   - `find /data/usenet/complete -maxdepth 4 -type f | head`
3. Confirm imports exist:
   - `find /data/media/movies -maxdepth 3 -type f | head`
   - `find /data/media/tv -maxdepth 5 -type f | head`
4. Confirm Arr history/import errors:
   - Radarr `/api/v3/history` and `/api/v3/queue`
   - Sonarr `/api/v3/history` and `/api/v3/queue`
5. Confirm Jellyfin libraries include `/data/media/movies` and `/data/media/tv`:
   - `curl -fsS http://localhost:8096/System/Info/Public`
   - Use Jellyfin admin auth to query `/Library/VirtualFolders`
6. Confirm Jellyfin is seeing recent items:
   - `/Items/Latest?IncludeItemTypes=Movie&Limit=10`
7. Confirm Jellystat is connected (if enabled):
   - `curl -fsS http://localhost:3000`

Expected healthy state:

- Completed downloads are moved/linked into `/data/media/movies` and `/data/media/tv` by Arr.
- Jellyfin libraries point to `/data/media/movies` and `/data/media/tv` and items appear within a refresh cycle.

### 5.8 Stremio Playback: Identify if Failure is Decode vs Transport

1. Identify the file:
   - Find the exact path in Jellyfin (Admin UI or API item metadata).
2. Inspect codecs:
   - `ffprobe -hide_banner -show_streams -show_format -of json "/data/media/movies/Apex (2026)/Apex (2026).mkv" | jq .`
3. Determine whether Stremio is using:
   - A direct stream endpoint (no transcode)
   - An HLS/transcode endpoint (Jellyfin should produce segments)
4. Validate transport:
   - Get a Jellyfin access token and the item id:
     - `JFTOKEN="$(curl -fsS -H 'Content-Type: application/json' -H 'Authorization: MediaBrowser Client=\"vmctl\", Device=\"debug\", DeviceId=\"vmctl-debug\", Version=\"1.0\"' --data "{\"Username\":\"$JELLYFIN_ADMIN_USER\",\"Pw\":\"$JELLYFIN_ADMIN_PASSWORD\"}" http://localhost:8096/Users/AuthenticateByName | jq -r .AccessToken)"`
     - `ITEM_ID="$(curl -fsS -H "X-Emby-Token: $JFTOKEN" "http://localhost:8096/Items?SearchTerm=Apex&IncludeItemTypes=Movie&Limit=1" | jq -r '.Items[0].Id')"`
   - Validate stream endpoint headers via the Caddy Jellyfin proxy route:
     - `curl -sS -D - -o /dev/null "http://media-stack/jf/Videos/$ITEM_ID/stream" | head -n 40`
   - Validate range support:
     - `curl -sS -D - -o /dev/null -H "Range: bytes=0-1048575" "http://media-stack/jf/Videos/$ITEM_ID/stream" | head -n 40`
5. Correlate server-side logs to distinguish decode vs transport:
   - Caddy proxy logs: `docker compose --env-file /opt/media/.env -p media logs --tail=300 caddy`
   - Jellyfin logs: `/opt/media/config/jellyfin/log/` and `docker compose --env-file /opt/media/.env -p media logs --tail=300 jellyfin`
   - Look for:
     - Transcode attempts and ffmpeg errors (decode/encode)
     - HTTP 206/416 patterns (range/transport)

Expected healthy state:

- For direct play media, clients receive 206 Partial Content for range requests.
- For incompatible media, a transcode/HLS strategy is available and used, or the file is remediated post-import.

## 6. Download Client Selection Model

### 6.1 Design Goals

- Deterministic: same inputs yield same selected protocol set and priorities
- Strict about usability: “running” is not “usable”
- DRY: one selection function used by provisioning + validation
- Observable: selection result is logged and persisted in `/opt/media/.env` (or a generated JSON next to it)
- Safe: never enables an Arr indexer or download client for a protocol without a usable download client

### 6.2 Feature Flags (Config-Level)

Enablement for download clients should come only from the existing `services = [...]` list under `[resources.features.media_services]` (include to enable, omit to disable). This avoids duplicating truth across multiple flag systems.

Add only routing policy flags (prefer/fallback + “require at least one usable client”):

```toml
[resources.features.media_services.download_routing]
prefer = "usenet"      # "usenet" | "torrent"
fallback = "torrent"   # "usenet" | "torrent"
require_client = true  # fail provisioning if neither usable
```

Optional explicit protocol metadata (recommended) so protocol gating is data-driven and DRY:

- Add `download_type = "usenet"` to `packs/services/sabnzbd.toml`
- Add `download_type = "torrent"` to `packs/services/qbittorrent-vpn.toml`

Also set a safe default in the env template for SAB server enablement:

- In [packs/templates/media.env.hbs](/root/vmctl/packs/templates/media.env.hbs): set `SABNZBD_SERVER_ENABLE=false` by default.

### 6.3 Required Env Vars (Configured-Enough Rules)

qBittorrent configured enough:

- `QBITTORRENT_USERNAME` non-empty
- `QBITTORRENT_PASSWORD` non-empty
- `QBITTORRENT_WEBUI_PORT` set (default 8080 is fine)

SABnzbd configured enough:

- `SABNZBD_API_KEY` present (can be generated deterministically at bootstrap)
- At least one server has:
  - `SABNZBD_SERVER_HOST` non-empty
  - `SABNZBD_SERVER_ENABLE=true`
  - If auth is required by provider, username/password non-empty

Important rule:

- If `SABNZBD_SERVER_HOST` is empty, SAB is not configured and must be treated as unusable even if the container runs.

### 6.4 Health Checks (Healthy Rules)

qBittorrent healthy:

- `GET http://localhost:8080/api/v2/app/version` returns 200 on host
- From within docker network, `GET http://gluetun:8080/api/v2/app/version` returns 200 when VPN enabled, else `http://qbittorrent-vpn:8080`
- Auth works with configured credentials:
  - `POST /api/v2/auth/login` returns “Ok.” and sets a cookie

SABnzbd healthy:

- `GET http://localhost:8085/api?mode=version&apikey=$SAB_KEY` returns 200 and a version
- UI root does not redirect to `/wizard`
- `mode=get_config&section=servers` shows at least one `enable=1`
- `mode=server_stats` indicates at least one enabled server is not permanently failing

### 6.5 Usable Client = Enabled AND Configured AND Healthy

Represent the selection as:

```json
{
  "usenet": { "enabled": true, "configured": false, "healthy": false, "usable": false, "reason": "SABNZBD_SERVER_HOST is empty" },
  "torrent": { "enabled": true, "configured": true, "healthy": true, "usable": true, "reason": "" },
  "routing": { "prefer": "usenet", "fallback": "torrent", "allowed_protocols": ["torrent"] }
}
```

### 6.6 Fallback Behavior

If prefer is `usenet` but `usenet.usable=false` and `torrent.usable=true`:

- Disable Usenet protocol end-to-end:
  - Do not configure SAB download clients in Arr
  - Ensure Arr indexers are torrent-only
- Ensure qBittorrent is configured as priority 1 (or priority 2 with no SAB present; either is fine as long as only torrent protocol exists)

If prefer is `usenet` and both are usable:

- Configure both in Arr:
  - SAB priority 1
  - qBittorrent priority 2
- Allow both protocols in Arr indexers
- Optionally bias Prowlarr indexer priorities (usenet higher) for deterministic preference

If neither is usable and `require_client=true`:

- Fail provisioning clearly with a single actionable error message:
  - “No usable download clients. Enable and configure either qBittorrent or SABnzbd (Usenet server host + enable) before applying.”

## 7. Provisioning Changes

Goal: `vmctl apply` must only configure enabled + usable clients and must validate the full pipeline.

### 7.1 vmctl / Pack Rendering (Service Optionality)

Changes:

1. Treat download client enablement as purely driven by the existing `services = [...]` list under `[resources.features.media_services]`:
   - Include `sabnzbd` to enable SAB
   - Include `qbittorrent-vpn` to enable qBittorrent
   - Omit a service to disable it (compose omission, not just “stopped”)
2. Ensure compose generation omits disabled services entirely (the template already only emits `service_packs` for selected services).
3. Optionally add explicit `download_type` metadata to the service pack TOMLs so provisioning/validation can reason about protocols without hardcoding service names.
4. Enforce DRY configuration by generating exactly one canonical set of values and reusing it everywhere:
   - Routing policy: `download_routing.*`
   - Service URLs: `*_URL` and `*_INTERNAL_URL`
   - Credentials and API keys: `SEERR_API_KEY`, `SABNZBD_API_KEY`, Arr API keys (read from `config.xml`), qB credentials
   - Categories and paths: `QBITTORRENT_CATEGORY_*`, `SONARR_ROOT_FOLDER`, `RADARR_ROOT_FOLDER`, `/data` invariants
   - Indexer definitions split by protocol: `PROWLARR_BOOTSTRAP_INDEXERS_TORRENT`, `PROWLARR_BOOTSTRAP_INDEXERS_USENET`
   - Compatibility rules: explicit “Stremio-safe” policy inputs (Section 9) and any remediation toggles

Where to implement:

- Service list filtering: [crates/packs/src/lib.rs](/root/vmctl/crates/packs/src/lib.rs)
- Role defaults remain in: [packs/roles/media_stack.toml](/root/vmctl/packs/roles/media_stack.toml)

TDD check:

- Add unit tests in `crates/packs` verifying that when a service is omitted from `services = [...]`, it does not appear in `expansion.service_defs` and is not rendered in the compose template.

### 7.2 bootstrap-media.sh (SAB Config Defaults Must Not Create a “Fake Enabled” Server)

Required behavior changes:

- Treat empty `SABNZBD_SERVER_ENABLE` as `false` by default.
- Never default `SABNZBD_SERVER_HOST` to `127.0.0.1` when the user did not configure a provider.
- Persist computed booleans in `.env` for DRY downstream use:
  - `VMCTL_SABNZBD_CONFIGURED=true|false`
  - `VMCTL_SABNZBD_HEALTHY=true|false` (optional, can be computed later)
  - `VMCTL_QBITTORRENT_CONFIGURED=true|false`

Where to implement:

- [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh)
- [packs/templates/media.env.hbs](/root/vmctl/packs/templates/media.env.hbs)

### 7.3 bootstrap-sabnzbd.sh (Make SAB Optional and Usability-Aware)

Required behavior changes:

- If SAB service is disabled, the script must be a no-op.
- If SAB service is enabled but not configured enough, the script should still:
  - Start SAB (so UI is reachable for configuration), but
  - Not claim it is usable (do not enable Arr SAB download clients downstream)
- Add a clear log line stating whether SAB is “usable” and why.

Where to implement:

- [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-sabnzbd.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-sabnzbd.sh)

### 7.4 bootstrap-qbittorrent.sh (Make qB Optional and Verify API Usability)

Required behavior changes:

- If qBittorrent is disabled, script must be a no-op.
- If enabled, validate:
  - API reachable
  - Credentials work
  - Categories exist and save paths match env

Where to implement:

- [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-qbittorrent.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-qbittorrent.sh)

### 7.5 bootstrap-arr.sh (Protocol Gating + Deterministic Client Provisioning)

Required behavior changes:

1. Compute selection once:
   - `sab.usable` and `qbit.usable` using the rules in Section 6
2. Configure Arr download clients:
   - If `qbit.usable=true`: ensure qB client exists and enabled
   - If `qbit.usable=false`: ensure qB client is removed or disabled
   - If `sab.usable=true`: ensure SAB client exists and enabled
   - If `sab.usable=false`: ensure SAB client is removed or disabled
3. Configure indexers via Prowlarr sync so Arr only receives indexers for allowed protocols:
   - If `sab.usable=false`: ensure no Usenet indexers are synced into Arr (disable them in Prowlarr, or tag-based sync, or remove from app sync)
   - If `qbit.usable=false`: ensure no torrent indexers are synced into Arr
4. If neither usable and `require_client=true`: fail with a single actionable error.

Where to implement:

- [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh)

### 7.6 Prowlarr Bootstrap Indexers (Separate Usenet vs Torrent Lists)

Add DRY configuration that explicitly lists indexers by protocol so gating is reliable.

Example env keys (in `media.env.hbs`):

- `PROWLARR_BOOTSTRAP_INDEXERS_TORRENT="1337x,TorrentGalaxyClone,LimeTorrents,RuTracker,showRSS,EZTV"`
- `PROWLARR_BOOTSTRAP_INDEXERS_USENET="NZBFinder,NZBGeek,NinjaCentral,DrunkenSlug,Usenet Crawler,altHUB,SceneNZB"`

Provisioning behavior:

- If `sab.usable=false`, do not configure USENET indexers at all.
- If `qbit.usable=false`, do not configure TORRENT indexers at all.

Where to implement:

- [packs/templates/media.env.hbs](/root/vmctl/packs/templates/media.env.hbs)
- [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh)

### 7.7 Jellyfin/Jellystat/Autobrr

Jellyfin:

- Keep `/data/media/movies` and `/data/media/tv` as the only canonical libraries.
- Ensure library refresh is triggered after imports:
  - Keep the existing periodic importer (currently `vmctl-media-unpack.timer`) but ensure it is scoped to “import only”, not “download routing”.

Jellystat:

- Treat as read-only analytics. It should never be in the critical path of downloads/imports.

Autobrr:

- If qBittorrent is disabled/unusable, Autobrr should still provision but report that it cannot route downloads until a torrent client exists, or be optionally disabled via the same download client feature flags.

## 8. Recovery / Retry Strategy

### 8.1 Are Radarr/Sonarr Already Supposed to Retry?

Yes, partially:

- Monitored items are checked via RSS sync and scheduled tasks.
- Arr can upgrade and grab when new releases appear.

But:

- A failed immediate search (e.g., due to indexer outage or download client misconfig) may not be retried in the way a user expects without manual intervention.
- A request can remain “Missing” indefinitely if the protocol gating is wrong (for example, NZB indexers enabled but SAB unusable).

### 8.2 Recommendation

Do not build a full custom “request recovery” system first.

Instead:

1. Fix provisioning and protocol gating so Arr never grabs what it cannot send.
2. Extend the existing validation harness to detect “stuck due to configuration” states deterministically.
3. Add a minimal periodic checker only if real-world evidence shows that transient failures leave requests stuck even when configuration is correct.

Rationale:

- Most “stuck forever” failure modes in this stack are configuration gating failures, not missing retry logic.
- A custom “retry search” service risks duplicating and fighting Arr’s own logic unless strictly limited.

### 8.3 If a Minimal Recovery Service Is Still Desired

Scope (strictly limited):

- Only runs when both of these are true:
  - At least one protocol is usable (`sab.usable` or `qbit.usable`)
  - Arr health is green enough to accept commands (no download client errors)
- Only acts on a bounded set:
  - Seerr requests created/updated in the last 7 days
  - Items still `Missing` and `monitored=true`
- Only triggers safe Arr commands:
  - Radarr: `MoviesSearch` for a specific movie id
  - Sonarr: `SeriesSearch` for a specific series id
- Rate-limited and idempotent:
  - Do not re-trigger search more than once per 6 hours per item
  - Persist state in `/var/lib/vmctl/media-recovery/state.json`

Implementation location:

- A small Python oneshot + systemd timer similar to existing `vmctl-media-unpack.timer` (see `bootstrap-download-unpack.sh` pattern).

## 9. Media Compatibility Strategy (Jellyfin + Stremio)

### 9.1 Goals

- Jellyfin can always play (via direct play or transcode).
- Stremio should reliably play on the target clients (including constrained devices like Tizen).
- Compatibility enforcement should be measurable and automated.

### 9.2 Compatibility Policy (Pragmatic Defaults)

Target “Stremio-safe” baseline (maximize device compatibility):

- Container: `mp4` preferred, `mkv` acceptable
- Video: H.264/AVC (8-bit), Level <= 4.1 for 1080p content
- Audio: AAC-LC (stereo) or AC3 (device-dependent); avoid TrueHD/DTS-HD for widest support
- Subtitles: SRT/VTT preferred; avoid PGS-only when clients can’t render

### 9.3 Strategy Decision Tree

- If file meets Stremio-safe policy:
  - Leave untouched
- If container is problematic but streams are compatible:
  - Remux only (fast, no re-encode)
- If video/audio is incompatible:
  - Transcode to the baseline (CPU-heavy)

### 9.4 Recommended Approach (Order of Operations)

1. First, determine whether Stremio is failing due to:
   - decode incompatibility, or
   - transport/proxy issues (range/headers)
2. If it is decode incompatibility:
   - Prefer a post-import remediation workflow that only touches incompatible files.
3. If it is transport/proxy:
   - Fix Caddy/Jellyfin headers/range handling first; do not transcode as a workaround.

### 9.5 Tooling Evaluation (Tdarr vs Unmanic vs FileFlows vs Jellyfin Transcoding)

Jellyfin transcoding:

- Pros: zero additional services; on-demand
- Cons: Stremio addon may not request a transcode path; realtime transcoding may be too heavy without hardware acceleration

Tdarr:

- Pros: scalable; powerful rule engine; good for large libraries
- Cons: operational overhead; node/worker model complexity

Unmanic:

- Pros: simpler; “set and forget” for targeted transcodes
- Cons: less flexible than Tdarr for complex policies

FileFlows:

- Pros: workflow-based; flexible
- Cons: more moving parts; similar overhead to Tdarr

Recommendation for this stack:

- Start with “detect + report” (ffprobe) and confirm the actual failure mode.
- If decode incompatibility is common and hardware acceleration is not available, add Unmanic (minimal) first.
- If you have multiple nodes or want more control, choose Tdarr.

## 10. API / Code Examples

### 10.1 vmctl.toml Feature Flags

```toml
[resources.features.media_services.download_routing]
prefer = "usenet"
fallback = "torrent"
require_client = true
```

### 10.2 Environment Variable Validation (Bootstrap Preflight)

Example (bash + python pattern used in this repo’s scripts):

```bash
python3 <<'PY'
import os, sys

def b(name: str) -> bool:
    return (os.environ.get(name) or "").strip().lower() not in {"", "0", "false", "no", "off"}

errors = []

services = {item.strip() for item in (os.environ.get("MEDIA_SERVICES") or "").split(",") if item.strip()}
sab_enabled = "sabnzbd" in services
qbit_enabled = "qbittorrent-vpn" in services

if sab_enabled:
    host = (os.environ.get("SABNZBD_SERVER_HOST") or "").strip()
    enabled = b("SABNZBD_SERVER_ENABLE")
    if not host or not enabled:
        # SAB can run, but it is not usable.
        pass

if not sab_enabled and not qbit_enabled:
    errors.append("Both SABnzbd and qBittorrent are disabled; no download path exists.")

if errors:
    for err in errors:
        print(f"error: {err}", file=sys.stderr)
    sys.exit(1)
PY
```

### 10.3 Download Client Selection Logic (Single Source of Truth)

```python
def select_clients(env):
    # Inputs:
    # - service enablement (from MEDIA_SERVICES: include sabnzbd/qbittorrent-vpn to enable, omit to disable)
    # - configured-enough signals (env vars, sab server fields)
    # - health checks (API reachable + auth + sab server enabled)
    #
    # Output:
    # - allowed protocols: {"torrent", "usenet"}
    # - reasons for disabled protocols
    #
    result = {"usenet": {}, "torrent": {}, "routing": {}}

    result["routing"]["prefer"] = env["DOWNLOAD_PREFER"]
    result["routing"]["fallback"] = env["DOWNLOAD_FALLBACK"]

    # Normalize once (DRY); scripts in this repo already use a `service_enabled()` helper for this.
    env.setdefault("MEDIA_SERVICES_SET", {item.strip() for item in (env.get("MEDIA_SERVICES") or "").split(",") if item.strip()})

    qbit_enabled = "qbittorrent-vpn" in env["MEDIA_SERVICES_SET"]
    qbit_configured = bool(env["QBITTORRENT_USERNAME"] and env["QBITTORRENT_PASSWORD"])
    qbit_healthy = check_qbit_health(env)
    result["torrent"] = {"enabled": qbit_enabled, "configured": qbit_configured, "healthy": qbit_healthy}
    result["torrent"]["usable"] = qbit_enabled and qbit_configured and qbit_healthy

    sab_enabled = "sabnzbd" in env["MEDIA_SERVICES_SET"]
    sab_configured = bool(env["SABNZBD_API_KEY"] and env["SABNZBD_SERVER_HOST"] and env["SABNZBD_SERVER_ENABLE"])
    sab_healthy = check_sab_health(env)
    result["usenet"] = {"enabled": sab_enabled, "configured": sab_configured, "healthy": sab_healthy}
    result["usenet"]["usable"] = sab_enabled and sab_configured and sab_healthy

    allowed = []
    if result["usenet"]["usable"]:
        allowed.append("usenet")
    if result["torrent"]["usable"]:
        allowed.append("torrent")
    result["routing"]["allowed_protocols"] = allowed
    return result
```

### 10.4 Sonarr/Radarr API Payloads (Download Client Creation)

qBittorrent download client payload (Radarr variant):

```json
{
  "enable": true,
  "protocol": "torrent",
  "priority": 2,
  "removeCompletedDownloads": true,
  "removeFailedDownloads": true,
  "name": "qBittorrent",
  "implementation": "QBittorrent",
  "configContract": "QBittorrentSettings",
  "fields": [
    { "name": "host", "value": "gluetun" },
    { "name": "port", "value": 8080 },
    { "name": "urlBase", "value": "" },
    { "name": "username", "value": "admin" },
    { "name": "password", "value": "${QBITTORRENT_PASSWORD}" },
    { "name": "movieCategory", "value": "movies" }
  ]
}
```

SABnzbd download client payload (Sonarr variant):

```json
{
  "enable": true,
  "protocol": "usenet",
  "priority": 1,
  "removeCompletedDownloads": true,
  "removeFailedDownloads": true,
  "name": "SABnzbd",
  "implementation": "SABnzbd",
  "configContract": "SABnzbdSettings",
  "fields": [
    { "name": "host", "value": "sabnzbd" },
    { "name": "port", "value": 8080 },
    { "name": "urlBase", "value": "" },
    { "name": "apiKey", "value": "${SABNZBD_API_KEY}" },
    { "name": "tvCategory", "value": "tv" }
  ]
}
```

Disable/remove an unusable download client (must be done as part of convergence when a protocol is not allowed):

```bash
# list clients and delete by id (Radarr example)
RADARR_CLIENT_ID="$(curl -fsS -H "X-Api-Key: $RADARR_KEY" http://localhost:7878/api/v3/downloadclient | jq -r '.[] | select(.name=="SABnzbd") | .id' | head -n1)"
if [ -n "$RADARR_CLIENT_ID" ] && [ "$RADARR_CLIENT_ID" != "null" ]; then
  curl -fsS -X DELETE -H "X-Api-Key: $RADARR_KEY" "http://localhost:7878/api/v3/downloadclient/$RADARR_CLIENT_ID"
fi
```

### 10.5 qBittorrent Category Setup (API)

```bash
# login
cookie="$(curl -fsS -D - \
  --data-urlencode "username=$QBITTORRENT_USERNAME" \
  --data-urlencode "password=$QBITTORRENT_PASSWORD" \
  http://localhost:8080/api/v2/auth/login \
  | awk -F': ' 'tolower($1)=="set-cookie" {print $2}' | head -n1 | cut -d';' -f1)"

# create/edit categories
curl -fsS -b "$cookie" --data-urlencode "category=tv" --data-urlencode "savePath=/data/torrents/tv" \
  http://localhost:8080/api/v2/torrents/createCategory || true
curl -fsS -b "$cookie" --data-urlencode "category=tv" --data-urlencode "savePath=/data/torrents/tv" \
  http://localhost:8080/api/v2/torrents/editCategory || true
```

### 10.6 SABnzbd Category Setup (API)

```bash
curl -fsS "http://localhost:8085/api?mode=set_config&section=categories&name=tv&dir=/data/usenet/complete/tv&apikey=$SAB_KEY"
curl -fsS "http://localhost:8085/api?mode=set_config&section=categories&name=movies&dir=/data/usenet/complete/movies&apikey=$SAB_KEY"
```

### 10.7 Post-Provision Validation Script (Protocol Gating Assertions)

Conceptual structure (extend existing `bootstrap-validate-streaming-stack.sh`):

```bash
python3 <<'PY'
# 1) compute usable protocols (same selection logic as provisioning)
# 2) assert Arr download clients exactly match usable protocols
# 3) assert Arr indexers contain only allowed protocols
# 4) assert Prowlarr indexers enabled align with allowed protocols
# 5) fail with actionable messages
PY
```

### 10.8 Stremio Compatibility Check Using ffprobe

```bash
ffprobe -hide_banner -v error \
  -select_streams v:0 -show_entries stream=codec_name,profile,pix_fmt,level \
  -select_streams a:0 -show_entries stream=codec_name,channels \
  -show_entries format=format_name \
  -of json \
  "$MEDIA_FILE" | jq .
```

Decision logic example:

```text
If video codec != h264:
  mark incompatible
If pix_fmt contains "10" (10-bit):
  mark risky for some clients
If audio codec in {truehd, dts, dca}:
  mark incompatible
If container not in {mp4,mkv}:
  mark incompatible
```

### 10.9 Optional Recovery Service Pseudocode (Strictly Limited)

```python
def tick():
    usable = compute_usable_protocols()
    if not usable:
        return

    requests = seerr_recent_requests(days=7)
    for req in requests:
        item = map_request_to_arr_item(req)
        if not item.monitored or item.has_file:
            continue
        if recently_retried(item.id, hours=6):
            continue
        if arr_health_has_download_errors():
            continue
        trigger_arr_search(item)
        record_retry(item.id)
```

## 11. TDD Strategy

TDD is enforced via two layers:

1. Generator/unit tests (fast, deterministic)
2. Post-provision validation (integration checks run on the VM)

### 11.1 Reproduce the Failure

- Use the existing real example (Bluey 2025) and capture:
  - Seerr request id
  - Radarr movie id
  - Radarr history events around the request
  - Current configured download clients and indexers

Per-issue TDD loop (apply this structure to each issue below):

1. Reproduce the failure deterministically.
2. Capture the current broken behavior with logs + API responses.
3. Add a failing automated check (prefer extending `bootstrap-validate-streaming-stack.sh` so `vmctl apply` becomes the test runner).
4. Implement the smallest fix that makes the check pass.
5. Re-run `vmctl apply` until the fix is idempotent.
6. Add regression coverage in unit tests (rendering/selection) plus on-host validation (integration).

Issue-specific repro/expected assertions:

- Issue 1 (Radarr requests not reaching qBittorrent):
  - Repro: `SABNZBD_SERVER_HOST=""` and `SABNZBD_SERVER_ENABLE=false` with torrents enabled; request a movie in Seerr.
  - Failing check: Radarr has any enabled Usenet indexer or an enabled SAB download client.
  - Passing check: Radarr has only torrent indexers and qBittorrent is the only enabled client; Radarr history shows a grab sent to qBittorrent.
- Issue 2 (Optional download clients):
  - Repro: omit `qbittorrent-vpn` from `[resources.features.media_services].services` and verify compose omits it and Arr is not configured with qBittorrent.
  - Repro: omit `sabnzbd` from `[resources.features.media_services].services` and verify compose omits it and Arr is not configured with SABnzbd.
  - Repro: omit both and assert provisioning fails with a single clear error (when `download_routing.require_client=true`).
- Issue 3 (Missing/Not Available items):
  - Repro: pick one “Missing” and one “Not Available” title and capture `/api/v3/movie` payloads, history, and release search output.
  - Passing check: each title has an explicit reason:
    - Missing: indexer rejection reason or client/health reason surfaced in logs
    - Not Available: availability setting explains it (pre-release), or minimum availability configured intentionally
- Issue 4 (Recovery service):
  - Repro: temporarily break indexers (or block outbound) during a request; restore later.
  - Passing check without custom service: monitored items eventually get grabbed via Arr tasks once the world is healthy again.
  - If not, implement minimal recovery timer with strict rate limits and add tests asserting it does not spam commands.
- Issue 5 (Completed downloads not in Jellyfin):
  - Repro: complete a download into `/data/torrents/*` and verify import to `/data/media/*`.
  - Failing check: file exists in torrents but not in media after a full import cycle, and Arr history shows import failure.
  - Passing check: Arr imports; Jellyfin virtual folders include the correct paths; Jellyfin latest items include it.
- Issue 6 (Stremio playback failure):
  - Repro: pick a known failing item, capture stream URL behavior (range) and `ffprobe` output.
  - Passing check: either (a) range/transport fixed and direct play works, or (b) file remediated to Stremio-safe policy, or (c) Stremio uses a transcode-capable endpoint successfully.

### 11.2 Capture Broken Behavior

Add a validation that currently fails in the broken environment:

- “When SAB is enabled but not configured, Arr must not have SAB download client configured, and must not have usenet indexers.”

### 11.3 Write Failing Tests/Checks (Before Fix)

Add or extend tests:

- Pack/unit tests:
  - Omitting `sabnzbd` from `services = [...]` removes it from rendered compose
  - Omitting `qbittorrent-vpn` from `services = [...]` removes it from rendered compose
- Validation checks (on-host):
  - SAB disabled/unconfigured → Arr has only qB download client and torrent indexers only
  - SAB enabled + configured + healthy → Arr has SAB + qB, and both protocol indexers
  - No usable clients → validation fails with the intended error message

### 11.4 Implement Fix (Later Work)

- Implement selection model + protocol gating (Sections 6 and 7).

### 11.5 Verify Behavior

- `vmctl apply` passes validation in all 4 scenarios:
  - SAB usable, qB usable
  - SAB unusable, qB usable
  - SAB usable, qB unusable
  - Neither usable (expected failure)

### 11.6 Add Regression Coverage

Tests/checks to cover explicitly (required):

- SABnzbd disabled → Radarr/Sonarr use qBittorrent
- SABnzbd enabled but missing env vars → Radarr/Sonarr use qBittorrent
- qBittorrent disabled but SABnzbd usable → use SABnzbd
- no usable clients → provisioning fails clearly
- Seerr request creates Radarr/Sonarr monitored item
- Radarr/Sonarr search sends item to download client
- completed download imports into `/data/media`
- Jellyfin library sees imported media
- Jellystat sees recently added
- Stremio stream URL returns playable response (range works; content type sane)
- unsupported media is detected for remediation

## 12. Task List

All tasks are phrased as implementation-ready changes mapped to repo paths.

1. Keep download client enablement single-sourced in `services = [...]` (no extra `*_enabled` flags). Add only routing policy config (`download_routing`) and make it available to bootstrap/validation scripts via env.
   - Update: [packs/templates/media.env.hbs](/root/vmctl/packs/templates/media.env.hbs)
   - Update: [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh)
2. (Optional, if you want protocol logic to be data-driven rather than name-driven) Add explicit `download_type = "usenet"|"torrent"` metadata to service packs and plumb it into rendered context.
   - Update: [packs/services/sabnzbd.toml](/root/vmctl/packs/services/sabnzbd.toml)
   - Update: [packs/services/qbittorrent-vpn.toml](/root/vmctl/packs/services/qbittorrent-vpn.toml)
   - Update: [crates/packs/src/lib.rs](/root/vmctl/crates/packs/src/lib.rs)
3. Change SAB defaults to safe values (unconfigured by default).
   - Update: [packs/templates/media.env.hbs](/root/vmctl/packs/templates/media.env.hbs)
4. Implement the “usable download client” selection function (single source).
   - Update: [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-arr.sh)
   - Update: [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-media.sh)
5. Protocol gate indexers:
   - Add `PROWLARR_BOOTSTRAP_INDEXERS_TORRENT` and `PROWLARR_BOOTSTRAP_INDEXERS_USENET` to env template.
   - Update Prowlarr bootstrap logic to enable/disable indexers based on usable protocols.
6. Make download clients truly optional:
   - Ensure compose omission works via service list filtering
   - Ensure bootstrap scripts no-op when services are disabled
7. Update validation to match the new model:
   - Update: [crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-validate-streaming-stack.sh](/root/vmctl/crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/scripts/bootstrap-validate-streaming-stack.sh)
8. Add TDD unit tests for service list filtering and env defaults.
   - Add tests in `crates/packs` for service filtering
   - Add tests in `crates/backend-terraform` asserting fixtures contain new logic (string-contains style used today)
9. Improve “import to Jellyfin” verification:
   - Extend validation to check that `/data/media/movies` and `/data/media/tv` exist and are readable by PUID/PGID 1000:1000
   - Validate that Jellyfin virtual folders contain those paths
10. Investigate Stremio failure mode and decide remediation path:
   - Add a “compatibility report” script that runs `ffprobe` on recently imported files and logs a risk classification
11. If needed, add an optional media remediation service (Unmanic or Tdarr) as an optional service in `services = [...]` (same enablement model as the rest of the stack).

## 13. Definition of Done

This plan is complete when implementation delivers verifiable outcomes for:

- Disabled/unconfigured SABnzbd is bypassed automatically.
- qBittorrent receives torrent downloads when SABnzbd is unavailable.
- Radarr/Sonarr do not get stuck on missing clients or unusable protocols.
- Seerr requests trigger searches/downloads deterministically.
- “Missing” and “Not Available” are explained with concrete evidence:
  - “Not Available” maps to release/availability settings
  - “Missing” maps to indexer/client/quality/import reasons
- Completed downloads are imported into `/data/media`.
- Jellyfin sees new content without manual intervention.
- Jellystat shows recently added content when enabled.
- Stremio can play supported media reliably.
- Unsupported media is detected and either remediated (transcode/remux) or reported clearly.
- The pipeline is idempotent after repeated `vmctl apply`.

## 14. Rollback / Recovery Considerations

Rollback must be safe and fast:

- Service-list rollback:
  - Re-enable SAB/qB by adding them back to `[resources.features.media_services].services` without tearing down the stack
  - Flip routing preference via `download_routing` without changing path invariants
- If protocol gating causes unexpected “no results” behavior:
  - Temporarily allow both protocols and rely on manual selection while investigating indexer availability
- Preserve user data:
  - Never delete `/opt/media/config/*` or `/data/*` as part of rollback
- Operational recovery:
  - If a provision step fails, `vmctl apply` should be rerunnable without manual cleanup
  - Validation errors should be actionable, single-cause messages (not cascading noise)
