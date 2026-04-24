# Jellyseerr + Request Flow + Stremio-on-Tizen Remediation Plan

Date: 2026-04-24

Scope:
- Jellyseerr (UI regression + upgrade/migration to Seerr)
- End-to-end request flow: Jellyseerr -> Sonarr/Radarr -> qBittorrent -> import -> Jellyfin library
- Stremio on Samsung Tizen OS: catalogs empty and playback not yet testable

Non-goals:
- Implementing fixes in this document (this is a plan only)
- Solving codec/container support beyond what’s required to get deterministic, observable playback on Tizen

---

## Current System (As Implemented Today)

### Deployed components (vmctl packs)

The media stack VM (`vmctl.toml`) enables these services:
- `caddy` (ports `80`, `5056`, `8097`)
- `jellyfin` (`lscr.io/linuxserver/jellyfin:latest`, host network)
- `jellyseerr` (`fallenbagel/jellyseerr:latest`, port `5055`)
- `sonarr` (`lscr.io/linuxserver/sonarr:latest`, port `8989`)
- `radarr` (`lscr.io/linuxserver/radarr:latest`, port `7878`)
- `prowlarr` (`lscr.io/linuxserver/prowlarr:latest`, port `9696`)
- `qbittorrent-vpn` (`lscr.io/linuxserver/qbittorrent:latest`, port `8080`, may route via `gluetun`)

Key files that define this wiring:
- `vmctl.toml` (service list + feature flags)
- `packs/services/jellyseerr.toml`, `packs/services/sonarr.toml`, `packs/services/radarr.toml`, `packs/services/qbittorrent-vpn.toml`
- `packs/templates/media.env.hbs` (all inter-service URLs + secrets inputs)
- `packs/templates/caddyfile.media.hbs` (public routes and proxy behavior)
- `packs/scripts/bootstrap-jellyseerr.sh` (writes Jellyseerr `settings.json` and wires *arr + Jellyfin)
- `packs/scripts/bootstrap-arr.sh` (ensures root folders and qBittorrent download clients in Sonarr/Radarr + Prowlarr sync)
- `packs/scripts/bootstrap-jellyfin.sh` (creates libraries `/media/movies`, `/media/tv`, refreshes library)
- `packs/scripts/bootstrap-validate-streaming-stack.sh` (smoke validation, including Tizen-like addon requests)

### Request flow (intended)

1. User requests a movie/TV series in Jellyseerr UI.
2. Jellyseerr calls Radarr (movie) or Sonarr (TV) using API keys, creating the item.
3. Radarr/Sonarr send the download to qBittorrent using configured download client + category (`movies` / `tv`).
4. After download completes, Radarr/Sonarr import into:
   - `/media/movies` (Radarr)
   - `/media/tv` (Sonarr)
5. Jellyfin library scan (manual or automated) discovers the imported media.

### Stremio on Tizen (current state)

Stremio addon endpoints are served via the Jellyfin Jellio plugin, with Caddy providing LAN routing:
- Addon manifest is written to `/opt/media/config/caddy/ui-index/jellio-manifest.lan.url` and then used by clients.
- Jellyfin is exposed to Stremio via `http://<lan-base>/jf/...` with `X-MediaBrowser-Token` injected by Caddy.
- Tizen-specific rewrite handlers exist for `.../Videos/<id>/stream` -> `.../Videos/<id>/master.m3u8` for Tizen User-Agent.

Reported behavior on Samsung Tizen OS:
- Catalog rows exist, but catalogs are empty (no content visible)
- Playback is not testable yet because there is no content visible

---

## Investigation Playbooks (Deep, Structured)

Each section is written as: hypotheses -> evidence to gather -> exact checks -> prove/disprove criteria.

### 1) Jellyseerr Media Pages Broken (UI)

Symptom:
- Clicking a media item in Jellyseerr shows “Oops, Return Home”
- Browser console: `TypeError: can't access property "applicationTitle", c is undefined`

#### Architecture / request flow for this problem

Browser -> Caddy (port `5056`) -> Jellyseerr container (port `5055`) -> Jellyseerr API (`/api/v1/...`) -> Jellyseerr SQLite + upstream providers (TMDB) + Jellyfin/*arr.

`packs/templates/caddyfile.media.hbs` currently proxies:
- `:5056` -> `reverse_proxy jellyseerr:5055` and sets `header_up X-API-Key {$JELLYSEERR_API_KEY}`

This means the UI and API are always accessed through a proxy that injects an API key header (possibly empty).

#### Primary hypotheses

H1. UI bundle expects settings payload, but the settings endpoint call is failing (non-200) and the UI doesn’t handle it.
- Likely failing endpoints:
  - `GET /api/v1/settings/public`
  - `GET /api/v1/settings/main`
  - `GET /api/v1/status`

H2. Proxy behavior is altering responses.
- Injected `X-API-Key` could be changing auth flow/permissions or triggering a different code path.
- Compression/content-encoding issues are less likely on modern browsers but still worth checking quickly.

H3. `fallenbagel/jellyseerr:latest` is now a moving target and a bad/partial release got deployed; the error is a Jellyseerr regression.

H4. Upstream provider failures (TMDB blocked / network issue) break the media-details API response and the UI crashes.
- Jellyseerr docs note TMDB connectivity issues can break functionality; a UI crash on details pages is plausible if error handling regressed.

#### Evidence to gather

From the client (browser):
- The first failing HTTP request in DevTools network tab when clicking a media item.
- Response status code, response body (even if HTML error), and which endpoint is missing `applicationTitle`.

From the server (media stack VM):
- Current runtime Jellyseerr version/build and image digest.
- Jellyseerr API health + settings endpoints via both direct container port and proxied port.
- Jellyseerr logs around the time of the click.

#### Exact checks to perform

1) Confirm what’s actually running (not just the TOML tag).

On the media-stack VM:
```bash
cd /opt/media
docker ps --format 'table {{.Names}}\t{{.Image}}\t{{.Status}}'
docker inspect --format '{{.RepoDigests}}' media-jellyseerr-1
docker logs --tail 200 media-jellyseerr-1
curl -fsS http://127.0.0.1:5055/api/v1/status
curl -fsS http://127.0.0.1:5055/api/v1/settings/public
```

2) Compare direct vs proxied behavior.

```bash
curl -fsS -D - -o /dev/null http://127.0.0.1:5055/ | head
curl -fsS -D - -o /dev/null http://127.0.0.1:5056/ | head

curl -fsS http://127.0.0.1:5055/api/v1/settings/public | python3 -m json.tool >/dev/null
curl -fsS http://127.0.0.1:5056/api/v1/settings/public | python3 -m json.tool >/dev/null
```

If `5055` works and `5056` fails, the proxy is implicated.

3) Identify which endpoint is missing `applicationTitle` (browser-based, fastest).
- Open Jellyseerr in browser.
- DevTools:
  - Console: capture stack trace (file + line; even minified helps).
  - Network: filter `api/v1`, find non-200 responses.
  - Confirm `settings/public` JSON includes the expected keys.

4) Check for upstream dependency failures that could break detail pages.

```bash
docker logs --tail 400 media-jellyseerr-1 | grep -iE 'tmdb|error|exception|failed'
```

If TMDB calls fail systematically, this can explain “details pages broken” while the rest of the UI loads.

#### Prove/disprove criteria

- H1 confirmed if:
  - Network tab shows a failing request (non-200), and the UI crash happens immediately after.
  - Direct curl shows missing keys or errors in `settings/public`/media detail endpoints.
- H2 confirmed if:
  - `5055` works but `5056` differs (status/body/headers) for the same endpoint.
- H3 confirmed if:
  - The running Jellyseerr build differs from previous known-good, or the same error is reported upstream for that version.
- H4 confirmed if:
  - Media detail endpoints return 5xx/4xx due to TMDB failures; logs show provider errors at click time.

---

### 2) Jellyseerr Requests Not Flowing Correctly (End-to-End)

Symptom:
- Jellyseerr requests behave inconsistently:
  - not picked up by Jellyfin
  - not picked up by Sonarr
  - visible in Radarr

Interpretation:
- Movies might be working partially (Radarr sees them) while TV is broken (Sonarr doesn’t).
- Or Jellyseerr is creating items in Radarr but the downstream download/import path is broken.

#### Architecture / request flow for this problem

Jellyseerr creates requests -> Sonarr/Radarr items -> download client (qBittorrent) -> import to `/media/...` -> Jellyfin libraries.

Current bootstrap wiring (important details):
- `packs/scripts/bootstrap-arr.sh` configures:
  - Sonarr root `/media/tv`, category `tv`
  - Radarr root `/media/movies`, category `movies`
  - qBittorrent host:
    - `gluetun` when VPN enabled
    - `qbittorrent-vpn` when VPN disabled
- `packs/scripts/bootstrap-jellyseerr.sh` writes `settings.json` for:
  - Sonarr/Radarr internal hostnames + API keys (from `config.xml`)
  - Directories (root folders) based on *arr `/api/v3/rootfolder` discovery

#### Primary hypotheses

H1. Jellyseerr -> Sonarr connectivity/config is broken (bad API key, wrong host/port, wrong URL base).
- Would explain “TV requests not picked up by Sonarr”.

H2. Sonarr/Radarr -> qBittorrent is misconfigured (wrong host due to VPN mode, auth mismatch, category mismatch).
- Would explain “requests exist in Radarr but downloads don’t start”.

H3. Import paths are inconsistent across containers (download path differs, incomplete/complete paths differ, permissions).
- Would explain “downloads happen but import fails”.

H4. Jellyfin library paths differ from the actual import destination, or scans are not happening.
- Would explain “download/import ok but Jellyfin doesn’t show it”.

H5. Jellyseerr request policy settings are gating: requests remain pending approval or are routed to the wrong service (movie vs TV mapping).

#### Evidence to gather

From Jellyseerr:
- Services configuration snapshot (Sonarr/Radarr settings, default flags, root folders, quality profiles).
- A single sample request history record for a movie and a TV series, including status transitions (requested -> approved -> processing -> available).

From Sonarr/Radarr:
- Whether the requested item exists in the library.
- If it exists, whether it is monitored, queued for download, has a wanted/missing status, and whether a download client is configured/enabled.

From qBittorrent:
- Whether a torrent appears for the request and whether it’s tagged/categorized as expected.

From filesystem:
- Presence of imported media under `/media/movies` and `/media/tv` on the host.

#### Exact checks to perform

1) Confirm *arr API keys and readiness.

On media-stack VM:
```bash
cd /opt/media
SONARR_KEY="$(python3 - <<'PY'\nimport xml.etree.ElementTree as ET\nroot=ET.parse('/opt/media/config/sonarr/config.xml').getroot();print(root.findtext('ApiKey') or '')\nPY\n)"
RADARR_KEY="$(python3 - <<'PY'\nimport xml.etree.ElementTree as ET\nroot=ET.parse('/opt/media/config/radarr/config.xml').getroot();print(root.findtext('ApiKey') or '')\nPY\n)"
curl -fsS http://127.0.0.1:8989/api/v3/system/status -H "X-Api-Key: $SONARR_KEY" | python3 -m json.tool >/dev/null
curl -fsS http://127.0.0.1:7878/api/v3/system/status -H "X-Api-Key: $RADARR_KEY" | python3 -m json.tool >/dev/null
```

2) Validate Sonarr/Radarr root folders and download clients.

```bash
curl -fsS http://127.0.0.1:8989/api/v3/rootfolder -H "X-Api-Key: $SONARR_KEY" | python3 -m json.tool
curl -fsS http://127.0.0.1:7878/api/v3/rootfolder -H "X-Api-Key: $RADARR_KEY" | python3 -m json.tool

curl -fsS http://127.0.0.1:8989/api/v3/downloadclient -H "X-Api-Key: $SONARR_KEY" | python3 -m json.tool
curl -fsS http://127.0.0.1:7878/api/v3/downloadclient -H "X-Api-Key: $RADARR_KEY" | python3 -m json.tool
```

Confirm:
- Root folders include exactly `/media/tv` (Sonarr) and `/media/movies` (Radarr).
- Download client points at the correct host:
  - VPN enabled: `gluetun`
  - VPN disabled: `qbittorrent-vpn`
- Categories match:
  - Sonarr category `tv`
  - Radarr category `movies`

3) Validate qBittorrent API and category behavior.

```bash
curl -fsS http://127.0.0.1:8080/api/v2/app/version
```

If authentication is enabled, validate Sonarr/Radarr are using the configured username/password from `.env`.

4) Validate Jellyseerr has *arr configured with correct internal hostnames.

```bash
cat /opt/media/config/jellyseerr/settings.json | python3 -m json.tool | sed -n '1,220p'
```

Confirm:
- `sonarr[0].hostname == "sonarr"`
- `radarr[0].hostname == "radarr"`
- `activeDirectory` matches `/media/tv` / `/media/movies`
- `syncEnabled == true`
- `preventSearch` matches desired behavior (see remediation section)

5) Request-level tracing (single movie + single series).
- In Jellyseerr UI:
  - Pick one movie request and one TV request.
  - Capture their request IDs and status history.
- In Radarr/Sonarr UI:
  - Confirm whether the item exists, and whether a search/download is triggered.
- In qBittorrent UI:
  - Confirm whether a torrent was created and what category it has.
- On disk:
  - Confirm imported files appear under `/media/...`

6) Jellyfin library visibility.

On media-stack VM:
```bash
curl -fsS http://127.0.0.1:8096/System/Info/Public | python3 -m json.tool >/dev/null
ls -la /media/movies | head
ls -la /media/tv | head
```

If files exist but Jellyfin doesn’t show them:
- Check Jellyfin library configuration (Movies/TV paths).
- Trigger a library refresh from Jellyfin UI or via API.

#### Prove/disprove criteria

- H1 confirmed if Sonarr is missing in Jellyseerr services settings or API calls fail (401/404/timeouts).
- H2 confirmed if download clients in *arr point at the wrong host/credentials or show failed test state; qBittorrent never receives torrents.
- H3 confirmed if torrents download but *arr logs show import failures (permissions/path mismatch).
- H4 confirmed if *arr imported successfully but Jellyfin library path/refresh is wrong.
- H5 confirmed if Jellyseerr requests remain pending approval or are not routed to the expected service type.

---

### 3) Stremio on Samsung Tizen OS (Catalog Empty, Playback Not Testable Yet)

Symptom:
- Stremio on Mac works (catalogs and playback)
- Stremio on Samsung Tizen OS shows catalog rows but they are empty (no content visible)
- Playback cannot be tested yet because no content is visible

#### Architecture / request flow for this problem

Tizen Stremio app -> addon manifest URL -> `/catalog/...` requests -> addon responds with `metas[]` -> user selects item -> `/stream/...` -> Stremio player requests the `stream.url`.

The addon here is the Jellyfin Jellio plugin, reachable via Caddy and backed by Jellyfin’s libraries.

#### Investigation Path A: Documentation / Online Research

Goals:
- Confirm which stream object fields are supported on TV clients (`url` vs `externalUrl`, headers, proxying).
- Confirm if Tizen clients have stricter requirements around:
  - content encoding (`gzip`/`br` vs identity)
  - JSON content-type parsing
  - TLS or mixed content restrictions
  - HLS-only playback expectations

Actions:
- Review Stremio addon protocol and response specs:
  - `/catalog/{type}/{id}.json` shape and `metas[]` expectations
  - `/stream/{type}/{videoId}.json` stream object expectations (prefer `url` for TV clients)
- Identify known TV-client limitations:
  - Some TV clients have issues when addons return `externalUrl` instead of `url`
  - Some clients are brittle around compressed responses or non-standard headers

Output:
- A compatibility matrix specifically for “Tizen Stremio app” with required/forbidden fields and header behaviors.

#### Investigation Path B: Reverse Engineering via Catch-All Debug Route (Recommended if A is insufficient)

Rationale:
- The fastest way to resolve “catalog rows exist but empty” is to capture what requests the TV actually makes, and whether our server responds with non-empty `metas`.

Two-step workflow requiring user participation:

Step 1 (engineering change):
- Add a Tizen-focused request capture layer that:
  - logs full request path/query, method, status
  - logs key request headers: `User-Agent`, `Accept`, `Accept-Encoding`, `Origin`, `Referer`
  - logs response headers: `Content-Type`, `Content-Encoding`, response length
  - logs a short hash of the response body and (for JSON) whether it contains `metas` and how many

Step 2 (user action + analysis):
- User opens Stremio on the Samsung TV, navigates to the addon catalogs.
- Engineer collects logs and compares:
  - Mac requests vs Tizen requests
  - response shapes and encodings
  - whether the TV is reaching the correct base URL

#### Primary hypotheses for “empty catalogs on Tizen”

H1. Network reachability/DNS from TV to `http://media-stack` or `http://media-stack.home.arpa` is broken.
- Manifest may load from cache/sync, but subsequent catalog requests fail silently and appear as empty.

H2. Tizen client cannot handle the server’s response encoding (compressed/chunked), causing parse failure and resulting in empty UI.
- The existing validator explicitly forces `Accept-Encoding: identity`; the TV may not.
- Fix would be: ensure addon responses are served uncompressed to Tizen.

H3. Returned addon URLs (for posters/backgrounds or catalog endpoints) are not reachable from TV due to hostname selection (`media-stack` vs FQDN vs Tailscale).
- TV cannot resolve the short hostname or cannot resolve `.home.arpa` depending on LAN DNS.

H4. Addon response shape is tolerated by Mac but rejected by TV.
- Example: relying on `externalUrl` or missing `behaviorHints`, or returning relative image URLs.

#### Exact checks to perform

1) Confirm TV can resolve and reach the host.
- On the TV (via built-in browser or network diagnostics):
  - Load `http://media-stack/healthz`
  - Load `http://media-stack.home.arpa/healthz`
  - If either fails, fix LAN DNS/search domain before continuing.

2) On media-stack VM, tail Caddy logs while opening Stremio on TV.
```bash
cd /opt/media
docker logs -f --tail 200 media-caddy-1
```

Confirm whether Tizen requests for:
- `/jellio/.../manifest.json`
- `/jellio/.../catalog/...`
- `/jellio/.../stream/...`
are actually hitting the server.

3) Compare Mac vs Tizen requests.
- Perform the same navigation in Mac Stremio and capture the request set from Caddy logs.
- Differences to focus on:
  - `Accept-Encoding`
  - whether requests include query params for pagination/search (`skip`, `genre`, etc.)
  - whether Tizen requests different catalog IDs

4) If requests hit but catalogs are empty, fetch the exact catalog URL from logs and test it with Tizen-like headers:
```bash
set -euo pipefail

MANIFEST_URL="$(curl -fsS http://media-stack/jellio-manifest.lan.url | tr -d '\n\r')"
python3 - "$MANIFEST_URL" <<'PY'
import json
import sys
import urllib.parse
import urllib.request

manifest_url = sys.argv[1].strip()
ua = "Mozilla/5.0 (SMART-TV; Linux; Tizen 6.5) AppleWebKit/537.36 Stremio"

def get_json(url: str):
    req = urllib.request.Request(
        url,
        headers={"User-Agent": ua, "Accept": "application/json", "Accept-Encoding": "identity"},
        method="GET",
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read().decode("utf-8"))

if not manifest_url.endswith("/manifest.json"):
    raise SystemExit(f"unexpected manifest URL shape: {manifest_url}")

base = manifest_url[: -len("/manifest.json")]
manifest = get_json(manifest_url)
catalogs = manifest.get("catalogs") or []
if not catalogs:
    raise SystemExit("manifest contains no catalogs")

first = catalogs[0]
ctype = urllib.parse.quote(str(first.get("type") or ""), safe="")
cid = urllib.parse.quote(str(first.get("id") or ""), safe="")
catalog_url = f"{base}/catalog/{ctype}/{cid}.json"
payload = get_json(catalog_url)
metas = payload.get("metas") or []
print("catalog_url:", catalog_url)
print("metas_count:", len(metas))
if metas:
    print("first_meta_id:", metas[0].get("id"))
PY
```

If curl returns non-empty `metas` but Tizen shows empty, the problem is client-side parsing (encoding/headers/shape), and reverse-engineering is mandatory.

---

## Implementation Strategy (Concrete Remediation)

### 1) Fix Jellyseerr UI Regression + Evaluate Upgrade to Seerr v3.2.0

#### Strategy overview

1. Stop treating Jellyseerr as an unpinned moving target.
2. Isolate proxy side effects (remove “always inject X-API-Key” from the UI proxy unless proven necessary).
3. If the bug is in Jellyseerr itself or if upstream has moved on: migrate to Seerr v3.2.0 and pin the image version.

#### Implementation steps (code/config level)

1) Make the deployed version explicit.
- Change `packs/services/jellyseerr.toml` to use a pinned, reproducible image tag.
- Prefer the official Seerr image for long-term support:
  - `ghcr.io/seerr-team/seerr:v3.2.0` (pin exact version)
- Add `init: true` in docker-compose for Seerr (the official docs require it).
  - This likely requires extending the vmctl service model + `packs/templates/docker-compose.media.hbs` to support emitting `init: true` per service.

Example service pack TOML change:
```toml
name = "seerr"
container_type = "docker"

[image]
name = "ghcr.io/seerr-team/seerr"
tag = "v3.2.0"

[ports]
published = ["5055:5055"]

[volumes]
mounts = ["${CONFIG_PATH}/jellyseerr:/app/config"]

[environment]
LOG_LEVEL = "info"
PORT = "5055"
```

Example compose output change needed:
```yaml
services:
  seerr:
    image: "ghcr.io/seerr-team/seerr:v3.2.0"
    init: true
    ports:
      - "5055:5055"
    volumes:
      - "${CONFIG_PATH}/jellyseerr:/app/config"
```

2) Proxy correctness: split “UI proxy” from “API key injection”.
- Update `packs/templates/caddyfile.media.hbs` so the Jellyseerr UI reverse proxy does not inject `X-API-Key` by default.
- If an API-key-based no-login endpoint is desired, create a separate, explicit route for that (different port or path) that injects the key only for those endpoints.

Concrete approach:
- `:5056` remains UI proxy (no header injection).
- Optional `:5057` (or `/seerr-api/*`) injects `X-API-Key` and is used by automation/validators only.

3) Upgrade/migration workflow (Jellyseerr -> Seerr).
- Back up config folder before changing anything:
  - `/opt/media/config/jellyseerr/` including `db/db.sqlite3` and `settings.json`
- Ensure permissions match UID 1000 (Seerr runs as `node`/UID 1000 in official container):
  - Current bootstrap already `chown -R 1000:1000 /opt/media/config/jellyseerr`; verify and keep.
- Roll forward:
  - Stop old container
  - Start Seerr container pointing at the same config volume
  - Verify Seerr performs automatic migration on first start (watch container logs)
- Rollback strategy:
  - Stop Seerr
  - Restore config backup
  - Restart the previous known-good image tag

4) Add deterministic validation.
- Extend `packs/scripts/bootstrap-validate-streaming-stack.sh` to include a strict Jellyseerr UI health check:
  - `GET /api/v1/settings/public` must be `200` and JSON must contain `applicationTitle` (and any other keys confirmed by investigation).
  - Add a “details endpoint” sanity check with a known TMDB ID if TMDB connectivity is required.

#### Validation after upgrade

Minimum checks:
- `GET http://127.0.0.1:5055/api/v1/status` returns 200
- `GET http://127.0.0.1:5055/api/v1/settings/public` returns JSON with expected keys
- UI loads and media pages render without console errors
- Existing SQLite DB is readable and request history remains intact
- Sonarr/Radarr integration shows connected and can create test requests

---

### 2) Fix Request Flow Determinism (Jellyseerr -> Arr -> qBittorrent -> Import -> Jellyfin)

#### Strategy overview

1. Make configuration observable (queryable) and consistent (DRY).
2. Add an automated “request flow validator” that confirms connectivity and configuration correctness without starting real downloads.
3. Fix the first broken hop discovered; do not change multiple hops at once.

#### DRY model (single source of truth)

Centralize in `vmctl.toml` under `[const]` and `[env]`:
- Hostnames and public bases: `const.media_stack`, `const.domain`
- Root folders: `const.media_paths.movies`, `const.media_paths.tv`, `const.media_paths.downloads_complete`
- Categories: `const.arr_categories.movies`, `const.arr_categories.tv`
- Service base URLs (internal vs external) derived and exported in `packs/templates/media.env.hbs`

Then:
- `packs/scripts/bootstrap-arr.sh` reads from env (not literals)
- `packs/scripts/bootstrap-jellyseerr.sh` reads from the same env
- Validators also read from env

#### Implementation steps

1) Add a dedicated validator script for request flow.

Add `/opt/media/validators.d/20-request-flow.sh` (generated by vmctl) that:
- Reads Sonarr/Radarr API keys from `config.xml`
- Verifies:
  - Root folder paths match the env-derived expected paths
  - Download client exists, points at correct host, and has correct category
  - qBittorrent is reachable from within the Sonarr and Radarr containers:
    - `source /opt/media/.env; [[ "${MEDIA_VPN_ENABLED,,}" == "true" ]] && QBIT_HOST=gluetun || QBIT_HOST=qbittorrent-vpn; docker exec media-sonarr-1 curl -fsS "http://${QBIT_HOST}:8080/api/v2/app/version"`
    - `source /opt/media/.env; [[ "${MEDIA_VPN_ENABLED,,}" == "true" ]] && QBIT_HOST=gluetun || QBIT_HOST=qbittorrent-vpn; docker exec media-radarr-1 curl -fsS "http://${QBIT_HOST}:8080/api/v2/app/version"`
- Verifies Jellyseerr settings reflect the same:
  - Sonarr hostname, port, and activeDirectory
  - Radarr hostname, port, and activeDirectory

2) Tighten bootstrap idempotency.

Ensure `bootstrap-jellyseerr.sh` does not partially overwrite `settings.json` (and does not erase user changes unintentionally). Implementation should:
- Read current `settings.json`
- Only mutate the specific nested fields needed (jellyfin/sonarr/radarr connection blocks)
- Preserve user-facing UI settings

3) Optional: add a “dry-run request” mode.

To avoid triggering downloads:
- Configure Jellyseerr to `preventSearch=true` during validator-run only.
- Create a request via Jellyseerr API (requires a non-empty API key and a known TMDB ID).
- Assert that:
  - Radarr has the movie in its list but no download was started
  - Sonarr has the series in its list (for TV) but no download was started

If Jellyseerr cannot support “no-search” for requests without user impact, keep dry-run disabled and rely on the config/connection checks above plus one manual request per media type.

4) Make Jellyfin visibility deterministic.
- Ensure `bootstrap-jellyfin.sh` libraries exist and are pinned to `/media/movies` and `/media/tv`.
- Add a validation that:
  - `/Library/VirtualFolders` includes Movies/TV with those exact paths
  - a library refresh can be triggered

---

### 3) Fix Stremio on Tizen (Catalog Empty First, Then Playback)

#### Strategy overview

1. Fix catalog visibility first; until catalogs show content, playback debugging is wasted effort.
2. Prefer compatibility-safe response behavior for Tizen:
  - uncompressed JSON
  - strict `Content-Type: application/json`
  - absolute URLs with a hostname the TV can resolve
3. If still broken, reverse engineer with Tizen request capture.

#### Implementation steps

1) Ensure TV-reachable base URL selection is correct and DRY.
- Ensure the manifest URL you install into Stremio uses a hostname the TV can resolve.
- For LAN: prefer `http://media-stack` only if the TV has DNS for short names; otherwise use `http://media-stack.home.arpa`.
- Keep both hostnames served (already required by earlier LAN constraints).

2) Force identity encoding to the addon upstream for Tizen requests.

Change `packs/templates/caddyfile.media.hbs` for the `/jellio/*` reverse proxy to ensure Jellyfin responses are not compressed when the client looks like Tizen:

Example Caddy snippet:
```caddy
@tizen_ua header_regexp User-Agent (?i).*tizen.*

handle @tizen_ua {
  handle /jellio/* {
    reverse_proxy {$JELLYFIN_INTERNAL_URL} {
      header_up Accept-Encoding identity
    }
  }
}
```

This targets the most common “TV client can’t parse gzip/br JSON” failure mode while limiting behavior changes to the Tizen UA.

3) Add structured request capture for the addon routes.

Add a dedicated log block for `/jellio/*` requests with:
- request path/query
- user-agent
- accept-encoding
- status + response size

If Caddy logging cannot capture enough detail, add a small debug service pack:
- `stremio-debug` container that:
  - logs requests to disk under `${CONFIG_PATH}/stremio-debug/`
  - optionally proxies upstream and records response metadata
- Route `/debug/*` to that service.

4) After catalogs show content, validate playback.
- Ensure streams returned to Tizen are `url`-based (avoid `externalUrl` for TV clients).
- Ensure returned stream URLs are reachable from TV and use a resolvable hostname.
- Ensure Tizen receives HLS (`master.m3u8`) for Jellyfin streams (already supported via rewrite handlers).

---

## TDD and Regression Strategy

This repo is best validated via a mix of:
- Rust unit tests for generated artifacts (existing pattern in `crates/backend-terraform`)
- Provision-time validators (`packs/scripts/bootstrap-validate-streaming-stack.sh` + `/opt/media/validators.d`)
- Optional UI-level tests (Playwright) for Jellyseerr UI regression detection

### 1) Jellyseerr UI regression (TDD)

1. Reproduce:
   - Open Jellyseerr UI and click a media item; confirm the console error.
2. Capture broken behavior:
   - Identify the first failing API request (endpoint + status code + body).
3. Add failing check:
   - Add a validator assertion that the relevant endpoint returns 200 and contains required JSON keys.
4. Implement fix:
   - Remove proxy header injection or migrate/pin to Seerr v3.2.0.
5. Verify:
   - Validator passes; UI click path no longer produces console error.
6. Regression coverage:
   - Add a Rust fixture test ensuring the validator check remains present and the compose image tag is pinned (no `latest`).
   - Optional: Playwright test that loads the UI and checks for absence of specific console errors.

### 2) Request flow (TDD)

1. Reproduce:
   - Create one movie request and one TV request; record outcomes at each hop.
2. Capture broken behavior:
   - Identify the first hop that diverges (Jellyseerr->Sonarr, Sonarr->qBit, import, Jellyfin).
3. Add failing check:
   - Add `validators.d/20-request-flow.sh` checks that fail when the hop is misconfigured.
4. Implement fix:
   - Correct service URLs, API keys propagation, download client host/category, root paths.
5. Verify:
   - Validator passes; manual request succeeds for both movie and TV.
6. Regression coverage:
   - Rust fixture test validates generated validator script exists and contains the required assertions.

### 3) Tizen catalogs (TDD)

1. Reproduce:
   - Tizen shows empty catalogs.
2. Capture broken behavior:
   - Using request capture, determine whether:
     - catalog requests are not arriving, or
     - responses are arriving but are encoded/invalid, or
     - responses are valid but contain empty `metas`.
3. Add failing check:
   - Extend `bootstrap-validate-streaming-stack.sh` to simulate Tizen headers and confirm non-empty `metas` for at least one catalog.
4. Implement fix:
   - Force identity encoding for Tizen UA, fix base URLs, fix any response shape issues discovered.
5. Verify:
   - Tizen shows content; validator passes.
6. Regression coverage:
   - Keep request capture optional but available behind a feature flag for future TV-client breakages.

---

## Task Breakdown (Actionable, Grouped)

### Investigation

1. Jellyseerr UI:
   - Capture failing endpoint from browser DevTools and correlate with `docker logs media-jellyseerr-1`.
   - Compare direct port `5055` vs proxied `5056` behavior for the same endpoints.
   - Record actual running image digest and app version.

2. Request flow:
   - Verify Sonarr/Radarr root folders and download clients via API.
   - Verify qBittorrent reachability from within Sonarr/Radarr containers.
   - Verify Jellyseerr `settings.json` has correct hostnames, API keys, and directories.

3. Tizen:
   - Verify TV can reach `http://media-stack/healthz` and `http://media-stack.home.arpa/healthz`.
   - Tail Caddy logs while opening Stremio on TV; confirm whether catalog requests hit the server.
   - If requests arrive, reproduce catalog fetch with identical headers and compare Mac vs Tizen behavior.

### Reproduction

1. Record:
   - exact URL used to install the addon on the TV
   - Tizen Stremio app version and TV model/OS version
   - which catalogs are empty and whether any search works

### Remediation

1. Jellyseerr:
   - Pin image version and remove `latest`.
   - Decide on migration to Seerr v3.2.0 and implement `init: true` support in compose generation.
   - Split “UI proxy” and “API-key proxy” responsibilities in Caddy.

2. Request flow:
   - Add request-flow validator script under `/opt/media/validators.d`.
   - DRY env exports for root folders and categories.
   - Update bootstraps to consume those env vars.

3. Tizen:
   - Ensure Tizen requests are served uncompressed (`Accept-Encoding: identity` upstream).
   - Add structured request capture (Caddy log or debug service).

### Validation

1. Jellyseerr:
   - Validator checks `settings/public` contains expected keys.
   - Manual UI click regression test: media details pages load and no console errors.

2. Request flow:
   - Validator checks root folders/download clients/qBit reachability.
   - Manual request test: one movie + one TV show end-to-end.

3. Tizen:
   - Validator confirms at least one non-empty catalog with Tizen headers.
   - TV shows visible content for both Movies and Series catalogs.

### Regression Prevention

1. Ban `:latest` tags for critical services (Seerr/Jellyfin/*arr/qBit) in pack validation.
2. Keep a stable “known-good” set of sample IDs for non-destructive API checks.
3. Maintain an opt-in debug capture mode for TV clients that can be enabled temporarily without redeploying the whole stack.

---

## Definition of Done (Must Be Verifiable)

### Jellyseerr UI
- Media detail pages load without JS errors.
- The `applicationTitle` undefined error is not reproducible.
- Jellyseerr is either:
  - pinned to a known-good Jellyseerr version, or
  - migrated to Seerr v3.2.0 with documented rollback steps.

### Request Flow
- A movie request creates the item in Radarr, downloads (or is queued), imports to `/media/movies`, and appears in Jellyfin.
- A TV request creates the series in Sonarr, downloads (or is queued), imports to `/media/tv`, and appears in Jellyfin.
- Automated validators confirm the correctness of root folders, download clients, and inter-service connectivity.

### Stremio on Tizen
- Catalogs show content on Samsung Tizen OS (not empty).
- Playback is either confirmed working for at least one supported item, or:
  - the reverse-engineering workflow is implemented, documented, and produces logs that explain the failure.
