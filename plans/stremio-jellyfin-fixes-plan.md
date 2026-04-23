# Stremio + Jellyfin (Jellio+) Integration Remediation Plan

Date: 2026-04-23

## Objective

Fix two issues when using the **“Stremio Manifest (LAN)”** addon against the Jellyfin server provisioned by this repo:

1. **Playback failure in Stremio**: selecting a Jellyfin-backed item results in `Loading failed`.
2. **Missing metadata/artwork on Stremio Home (catalog tiles)**: items show default icon + title only on Home, while the detail page shows artwork.

Constraints:

- This plan is implementation-ready, tied to the current repo architecture (templates + bootstrap scripts).
- The remediation uses TDD: reproduce, fail test, implement, verify, add regression coverage.

## LAN HTTP Availability Now (HTTPS Optional Later)

You reported that `http://media-stack.home.arpa` redirects to `https://media-stack.home.arpa` and HTTPS is currently unreachable. If Stremio is currently fetching metadata/artwork successfully, we can **defer LAN HTTPS** and focus on the core integration bug(s) first, as long as **plain HTTP remains usable** by the Stremio clients you care about.

Why this is safe to defer (conditionally):

- If your Stremio clients already installed the addon and can fetch `manifest` + `catalog` + `meta` over either `http://media-stack` or `http://media-stack.home.arpa`, then HTTP is not currently a blocker for browse/artwork.
- Playback failures are commonly caused by **wrong routing/base URLs** and **auth handling** for the returned stream/image URLs; those can be fixed over HTTP.

Plan requirement:

- Ensure **HTTP on `:80`** is available on LAN and does not force clients onto an unreachable HTTPS endpoint.
- Keep **LAN HTTPS** as a later enhancement (self-signed/internal CA or publicly trusted via a real domain), because device trust distribution can be non-trivial (especially on TVs).
- Continue to support both LAN hostnames:
  - `http://media-stack` (current short hostname in use)
  - `http://media-stack.home.arpa` (FQDN on the LAN)

Important caveat:

- The server-side configuration can support both hostnames, but the hostname must still resolve on the client network (LAN DNS/search domain). This plan assumes you either have LAN DNS set up for `media-stack` and `media-stack.home.arpa`, or you are using a client that appends the search domain automatically.

### Redirect Root Causes To Check (And How To Prove Which One It Is)

The “HTTP redirects to HTTPS” behavior can come from multiple places. Before changing config, prove where the redirect originates:

1. Server-side redirect from Caddy (or another reverse proxy):
   - `curl -svI http://media-stack/healthz` or `curl -svI http://media-stack.home.arpa/healthz` shows `HTTP/1.1 301`/`302` plus a `Location: https://...`.
   - The response headers may include `Server: Caddy` if it is Caddy doing it.
2. Client-side HTTPS-first mode (browser setting) without an actual redirect:
   - `curl -svI http://media-stack/healthz` (or the FQDN variant) returns `200`, but a browser still goes to `https://...`.
3. HSTS cached in a browser (previously set by the host):
   - If the host ever served `Strict-Transport-Security`, the browser may auto-upgrade to HTTPS even if the server no longer redirects.
   - Proof is the same as (2): `curl` returns `200` over HTTP, but browser upgrades anyway.
   - Fix is client-side: clear HSTS for the domain in that browser, or use a different hostname until HTTPS is ready.

This plan addresses (1) by ensuring the server does not issue redirects and does not enable automatic HTTPS behavior in Caddy’s config. (2) and (3) are client policy/caches and require client-side action if they occur.

Terminology used below:

- `MEDIA_PUBLIC_BASE_URL_LAN` is the base URL that LAN devices will use, including scheme:
  - Example (HTTP short): `http://media-stack`
  - Example (HTTP): `http://media-stack.home.arpa`
  - Example (HTTPS): `https://media-stack.home.arpa`

Note on your message typo:

- Interpreting `https://media-stacj.home.arpa` as `https://media-stack.home.arpa`.

About `http://*.home.arpa`:

- This repo currently exposes services primarily via `media-stack.home.arpa` plus ports (e.g. `:8097`, `:5056`), not via subdomains.
- If you want `*.home.arpa` to work as a wildcard, you need LAN DNS to resolve all subdomains to the media-stack IP and you must configure Caddy with matching hostnames (and, for HTTPS, a cert that covers them). That is a separate decision from fixing Stremio/Jellio, and should be treated as an optional extension.

## System Map (Current Architecture / Request Flow)

### Relevant components owned by this repo

- **Jellyfin** (container `media-jellyfin-1`), HTTP on `:8096`.
  - Internal address is configured via `JELLYFIN_INTERNAL_URL` in `/opt/media/.env`.
- **Caddy** (container `media-caddy-1`) running multiple listeners from the generated `caddyfile.media`:
  - `:80` serves the portal UI and a reverse proxy for selected paths.
  - `:8097` is a “no-login” reverse proxy to Jellyfin that injects `X-MediaBrowser-Token: $JELLYFIN_AUTO_AUTH_TOKEN`.
- **Jellio+ Jellyfin plugin** (installed by `packs/scripts/bootstrap-jellyfin-plugins.sh`).
  - Exposes Stremio addon endpoints under the `/jellio/` path prefix *inside* Jellyfin.
- **Stremio clients** that install a manifest URL produced by `packs/scripts/bootstrap-jellio.sh`.

### What Stremio talks to, and what talks to Jellyfin

**Browse path (works today):**

```text
Stremio
  -> GET ${MEDIA_PUBLIC_BASE_URL_LAN}/jellio/${CFG}/manifest.json
  -> GET ${MEDIA_PUBLIC_BASE_URL_LAN}/jellio/${CFG}/catalog/${TYPE}/${CATALOG_ID}.json
  -> GET ${MEDIA_PUBLIC_BASE_URL_LAN}/jellio/${CFG}/meta/${TYPE}/${ID}.json
  -> Caddy :80 (reverse_proxy /jellio/*)
    -> Jellyfin :8096
      -> Jellio+ plugin controllers
        -> Jellyfin library APIs
```

**Playback + artwork path (currently failing for Stremio):**

Jellio+ returns URLs for:

- playback (usually Jellyfin `/Videos/` endpoints or HLS endpoints)
- posters/backdrops (usually Jellyfin `/Items/${ITEM_ID}/Images/` endpoints)

Those URLs are built relative to the Jellio config value `PublicBaseUrl`.

If those URLs point at a host/port/path that does not actually route to Jellyfin, Stremio can browse (addon endpoints work) but cannot load posters or play streams.

## Repo-Level Findings (Evidence From Current Code/Config)

### Finding 1: `PublicBaseUrl` is currently set to a base that is not a Jellyfin reverse proxy

In `packs/scripts/bootstrap-jellio.sh`:

- LAN “base” is hardcoded as `http://media-stack`
- the encoded Jellio config sets `PublicBaseUrl = base`
- the manifest URL is also built on that same base: `{base}/jellio/${CFG}/manifest.json`

This couples:

- addon hosting base (where `/jellio/` must work)
- Jellyfin public base (where `/Videos/` and `/Items/${ITEM_ID}/Images/` must work)

…even though those are **not the same endpoint** in this repo’s current routing.

### Finding 2: Caddy `:80` currently proxies only `/jellio/*` to Jellyfin

In `packs/templates/caddyfile.media.hbs` (and the generated `caddyfile.media`):

- `handle /jellio/*` reverse proxies to `$JELLYFIN_INTERNAL_URL`
- all other paths fall through to the portal `file_server`

So if Stremio is told to load (examples):

- `http://media-stack/Items/${ITEM_ID}/Images/Primary?${QUERY}`
- `http://media-stack/Videos/${ITEM_ID}/stream?${QUERY}`

…those requests will hit the portal file server and will not reach Jellyfin.

### Finding 3: Both user-reported symptoms match a single failure mode

Symptom correlation:

- Browse works: requests go to `/jellio/*` and are proxied to Jellyfin.
- Playback fails: stream URLs likely point to non-`/jellio/*` endpoints that are not proxied.
- Home tiles missing posters: tile posters usually come from `catalog` entries; if their `poster` URL is unreachable, Stremio shows a default icon. Detail page can still show artwork because it fetches `meta` on demand.

This makes “wrong/misrouted URL base for streams + images” the leading root cause candidate.

## Structured Investigation (Deep, Deterministic, No Placeholders)

### Investigation goals

For both issues, collect:

- the exact JSON Stremio would receive from addon endpoints (`manifest`, `catalog`, `meta`, `stream`)
- every media/artwork URL returned to Stremio
- HTTP status + headers for those URLs when fetched from a LAN machine
- server-side logs that prove whether requests are reaching Jellyfin

### 0) Pre-flight: confirm HTTP works end-to-end and identify the LAN manifest URL

From a LAN machine, confirm that HTTP is actually usable (no forced redirect to unreachable HTTPS):

```bash
curl -svI http://media-stack/ 2>&1 | sed -n '1,80p'
curl -svI http://media-stack.home.arpa/ 2>&1 | sed -n '1,80p'
```

If the response is a redirect to `https://...` and HTTPS is unreachable, fix that redirect first (serve content over HTTP without redirect). After HTTP returns `200`, proceed.

Set the portal base for subsequent investigation calls (HTTP baseline):

```bash
PORTAL_BASE="http://media-stack"
printf 'PORTAL_BASE=%s\n' "$PORTAL_BASE"
```

Finally, fetch the manifest URL the portal advertises:

```bash
MANIFEST_URL="$(curl -fsS "${PORTAL_BASE}/jellio-manifest.lan.url" | tr -d '\n\r')"
printf 'MANIFEST_URL=%s\n' "$MANIFEST_URL"
```

If you want to validate both hostnames serve the same portal content, repeat with:

```bash
curl -fsS http://media-stack/jellio-manifest.lan.url | tr -d '\n\r'
curl -fsS http://media-stack.home.arpa/jellio-manifest.lan.url | tr -d '\n\r'
```

If this fails, fix basic reachability/DNS/ports before continuing (Stremio browse wouldn’t be reliable otherwise).

### 1) Fetch and inspect the manifest JSON

```bash
curl -fsS "$MANIFEST_URL" | python3 -m json.tool | sed -n '1,220p'
```

Record:

- `resources` (must include `catalog`, `meta`, `stream`)
- `catalogs` (type + id)

Derive:

```bash
ADDON_BASE="${MANIFEST_URL%/manifest.json}"
printf 'ADDON_BASE=%s\n' "$ADDON_BASE"
```

### 2) Auto-discover a catalog to reproduce the Home-tile poster problem

Use a one-liner to pick the first declared catalog and fetch it:

```bash
python3 - <<'PY'
import json, os, urllib.request
manifest_url = os.environ["MANIFEST_URL"]
data = json.loads(urllib.request.urlopen(manifest_url, timeout=20).read().decode("utf-8"))
cat = (data.get("catalogs") or [])[0]
if not cat:
    raise SystemExit("manifest has no catalogs")
stremio_type = cat.get("type")
catalog_id = cat.get("id")
if not stremio_type or not catalog_id:
    raise SystemExit(f"unexpected catalog entry: {cat}")
addon_base = manifest_url.removesuffix("/manifest.json")
url = f"{addon_base}/catalog/{stremio_type}/{catalog_id}.json"
print(url)
PY
```

Then:

```bash
CATALOG_URL="$(MANIFEST_URL="$MANIFEST_URL" python3 - <<'PY'
import json, os, urllib.request
manifest_url = os.environ["MANIFEST_URL"]
data = json.loads(urllib.request.urlopen(manifest_url, timeout=20).read().decode("utf-8"))
cat = (data.get("catalogs") or [])[0]
stremio_type = cat["type"]
catalog_id = cat["id"]
addon_base = manifest_url.removesuffix("/manifest.json")
print(f"{addon_base}/catalog/{stremio_type}/{catalog_id}.json")
PY
)"
curl -fsS "$CATALOG_URL" | python3 -m json.tool | sed -n '1,260p'
```

From this catalog response, record:

- whether `metas[*].poster` exists
- the first meta id: `metas[0].id`
- the first poster URL (if present)

### 3) Auto-discover the first meta id and fetch its stream response

```bash
python3 - <<'PY'
import json, os, urllib.request
catalog_url = os.environ["CATALOG_URL"]
data = json.loads(urllib.request.urlopen(catalog_url, timeout=20).read().decode("utf-8"))
metas = data.get("metas") or []
if not metas:
    raise SystemExit("catalog has no metas")
meta_id = metas[0].get("id")
if not meta_id:
    raise SystemExit("metas[0] has no id")
stream_type = metas[0].get("type") or "series"
addon_base = catalog_url.split("/catalog/")[0]
print(f"{addon_base}/stream/{stream_type}/{meta_id}.json")
PY
```

Then:

```bash
STREAM_URL="$(CATALOG_URL="$CATALOG_URL" python3 - <<'PY'
import json, os, urllib.request
catalog_url = os.environ["CATALOG_URL"]
data = json.loads(urllib.request.urlopen(catalog_url, timeout=20).read().decode("utf-8"))
metas = data.get("metas") or []
meta = metas[0]
meta_id = meta["id"]
stream_type = meta.get("type") or "series"
addon_base = catalog_url.split("/catalog/")[0]
print(f"{addon_base}/stream/{stream_type}/{meta_id}.json")
PY
)"
curl -fsS "$STREAM_URL" | python3 -m json.tool | sed -n '1,260p'
```

From this stream response, record:

- `streams[*].url` (every one)
- `streams[*].behaviorHints` (especially anything about proxying/headers)

### 4) Prove why playback fails: check returned `streams[*].url` directly

For each `streams[*].url`, run a deterministic loop (no manual copy/paste):

```bash
curl -fsS "$STREAM_URL" > /tmp/stremio-stream.json
python3 - <<'PY' | while IFS= read -r url; do
import json
data = json.load(open("/tmp/stremio-stream.json", encoding="utf-8"))
for s in (data.get("streams") or []):
    u = (s or {}).get("url") or ""
    if u:
        print(u)
PY
  echo
  echo "==> HEAD $url"
  curl -svI "$url" 2>&1 | sed -n '1,120p'
done
```

Interpretation:

- `404` + HTML-ish content means the portal file server is being hit, not Jellyfin.
- `401/403` means auth is missing for that URL.
- `200` but still failing in Stremio shifts focus to codecs/HLS segment reachability.

For HLS URLs (`.m3u8`), automatically fetch the first playlist returned and preview it:

```bash
HLS_URL="$(python3 - <<'PY'
import json
data = json.load(open("/tmp/stremio-stream.json", encoding="utf-8"))
for s in (data.get("streams") or []):
    u = (s or {}).get("url") or ""
    if ".m3u8" in u:
        print(u)
        break
PY
)"
if [ -n "$HLS_URL" ]; then
  echo "==> GET $HLS_URL"
  curl -fsS "$HLS_URL" | sed -n '1,80p'
fi
```

If segments are relative, ensure they resolve under the same base and return `200`.

### 5) Prove why posters are missing on Home: check `metas[*].poster` URLs

If `metas[0].poster` exists:

```bash
POSTER_URL="$(CATALOG_URL="$CATALOG_URL" python3 - <<'PY'
import json, os, urllib.request
catalog_url = os.environ["CATALOG_URL"]
data = json.loads(urllib.request.urlopen(catalog_url, timeout=20).read().decode("utf-8"))
meta = (data.get("metas") or [])[0]
print(meta.get("poster") or "")
PY
)"
printf 'POSTER_URL=%s\n' "$POSTER_URL"
curl -svI "$POSTER_URL" 2>&1 | sed -n '1,80p'
```

Interpretation:

- `404` indicates wrong routing/base (most likely `PublicBaseUrl` mismatch).
- `401/403` indicates missing auth for Jellyfin image endpoints.
- `200` with `Content-Type: image/*` should render correctly in Stremio tiles (if it doesn’t, caching becomes the next suspect).

### 6) Server-side request tracing to confirm routing

On the VM:

- enable/inspect Caddy access logs for `:80` and `:8097`
- inspect Jellyfin logs at `/opt/media/config/jellyfin/log/` around the test time

Evidence to collect:

- do requests for `/Items/` or `/Videos/` hit Caddy `:80` and return `404`?
- do they appear at all in Jellyfin logs?

## Root Cause Analysis (Likely, With Proof/Disproof Checks)

### Issue 1: Playback failure (`Loading failed`)

#### Root cause candidate 1: Stream URLs are routed to a base that doesn’t proxy Jellyfin playback endpoints

Most likely based on repo config:

- browse endpoints are under `/jellio/*` which is proxied
- playback endpoints are under `/Videos/*` or similar, which are not proxied on `:80`

Proof:

- `streams[*].url` points to `http://media-stack/${PATH}` (no port) with a non-`/jellio/*` path
- `curl -I` returns `404` or portal content

Fix vector:

- make the stream base routable (reverse proxy or different PublicBaseUrl)

#### Root cause candidate 2: Stream URLs require auth that Stremio isn’t sending

Proof:

- `curl -I` returns `401/403` for stream URLs
- stream URLs do not include an `api_key` query token
- adding `X-MediaBrowser-Token` header makes it succeed

Fix vector:

- ensure the URLs Stremio uses do not require client-sent auth headers (proxy + injected token, or embed query token)

#### Root cause candidate 3: Client codec/container incompatibility (direct play vs transcode)

Proof:

- stream URL is a direct file stream
- URL is reachable (`200`) but fails in Stremio
- forcing HLS/transcoding yields a reachable `.m3u8` and playback succeeds

Fix vector:

- make Jellio return HLS/transcoded streams for Stremio clients (or enforce a proxy endpoint that requests a transcode profile)

### Issue 2: Missing posters/artwork on Home tiles

#### Root cause candidate 1: `catalog` response has missing/unreachable `poster` URLs

Proof:

- `catalog` JSON shows `poster` absent or present but `curl -I` is non-200
- `meta` endpoint returns art (detail page works), but Home tiles rely on `catalog` art

Fix vector:

- make poster URLs reachable and stable (same as playback base/auth fix)

#### Root cause candidate 2: Stremio caching makes Home stale after fixes

Proof:

- after poster URLs return `200`, Stremio Home still shows defaults until cache refresh
- Stremio detail view shows correct art immediately (meta fetch is fresh)

Fix vector:

- plan for cache busting: reinstall addon, clear cache where possible, wait for refresh intervals

## Implementation Strategy (Concrete Remediation)

### High-level approach

Decouple “addon hosting base” (where `/jellio/*` is reachable) from “Jellyfin public base” used for playback/artwork by:

1. Ensuring the **LAN public base URL** (`MEDIA_PUBLIC_BASE_URL_LAN`) is reachable in practice over **HTTP**.
2. Adding a **dedicated Jellyfin reverse-proxy prefix** (recommended: `/jf/*`) on the LAN public base (keeps the portal working).
3. Setting Jellio’s `PublicBaseUrl` to that prefix.
4. Injecting Jellyfin auth server-side so Stremio doesn’t need to send headers.

This approach fixes both:

- playback (stream URLs route correctly and are authorized)
- posters (image URLs route correctly and are authorized)

### Concrete design: `/jf/*` Jellyfin proxy with token injection

1. Extend Caddy `:80` to add the Jellyfin prefix proxy:

- `handle_path /jf/*` reverse proxy to `$JELLYFIN_INTERNAL_URL`
- inject header: `X-MediaBrowser-Token: $JELLYFIN_STREMIO_AUTH_TOKEN`
- `handle_path` strips `/jf`, so Jellyfin receives normal paths

2. Update `bootstrap-jellio.sh` to:

- persist `JELLYFIN_STREMIO_AUTH_TOKEN` into `/opt/media/.env`
- set the encoded Jellio config field `PublicBaseUrl = "${ADDON_BASE}/jf"` (where `ADDON_BASE` is the base of the installed manifest URL)

3. Preserve `JELLYFIN_STREMIO_AUTH_TOKEN` across re-renders of `/opt/media/.env`.

### Code/config change list (files and exact intent)

#### 1) Caddy template: add `/jf/*` handler (HTTP baseline)

File: `packs/templates/caddyfile.media.hbs`

Update the `:80` site to include `/jf/*`, bind both hostnames, and **explicitly prevent any automatic HTTP->HTTPS behavior**.

Requirements this config must satisfy:

- `http://media-stack.home.arpa/healthz` returns `200` (not `3xx`).
- No `Location: https://...` is returned by the server for HTTP requests.
- No `Strict-Transport-Security` header is returned (avoid browsers caching HSTS while HTTPS is not deployed).

```caddyfile
{
  # LAN HTTP baseline: do not attempt ACME/HTTPS and do not create automatic redirects.
  auto_https off
}

media-stack:80, media-stack.home.arpa:80 {
  encode gzip

  handle_path /healthz {
    respond "ok" 200
  }

  # Avoid browsers getting "stuck" on HTTPS due to previously cached HSTS.
  header -Strict-Transport-Security

  handle /jellio/* {
    reverse_proxy {$JELLYFIN_INTERNAL_URL}
  }

  # Jellyfin playback + image endpoints used by Stremio
  handle_path /jf/* {
    reverse_proxy {$JELLYFIN_INTERNAL_URL} {
      header_up X-MediaBrowser-Token {$JELLYFIN_STREMIO_AUTH_TOKEN}
    }
  }

  handle {
    root * /srv/ui-index
    file_server
  }
}
```

Post-change verification (LAN client):

```bash
curl -svI http://media-stack/healthz 2>&1 | sed -n '1,80p'
curl -svI http://media-stack.home.arpa/healthz 2>&1 | sed -n '1,80p'
```

Expected:

- status is `200`
- there is no `Location:` header
- there is no `Strict-Transport-Security:` header

If `curl` still shows a redirect, the redirect is not coming from a browser cache; it is server-side (Caddy or another proxy/router). Confirm the responding server by checking headers like `Server:` and ensure DNS for `media-stack.home.arpa` resolves directly to the media-stack VM IP (not to another gateway).

#### 2) Manifest generator: persist token and set `PublicBaseUrl` to `/jf`

File: `packs/scripts/bootstrap-jellio.sh`

Changes:

- After authenticating the `stremio` user, persist:

```python
set_env_value(env_file, "JELLYFIN_STREMIO_AUTH_TOKEN", stremio_token)
```

- When building the payload, set:

```python
payload["PublicBaseUrl"] = f"{addon_base}/jf"
```

and keep manifest URL as:

```python
return f"{addon_base}/jellio/{encoded}/manifest.json"
```

This preserves addon endpoints while fixing the base used for playback/artwork URLs.

#### 3) Env sync: preserve the new token

File: `packs/scripts/bootstrap-media.sh`

Add `JELLYFIN_STREMIO_AUTH_TOKEN` to the `preserve` set inside `sync_env_from_template()`.

#### 4) Env template: make LAN public base explicit (HTTP baseline)

File: `packs/templates/media.env.hbs`

Add:

```env
MEDIA_PUBLIC_BASE_URL_LAN=http://media-stack.home.arpa
```

Then in `bootstrap-jellio.sh`, use `MEDIA_PUBLIC_BASE_URL_LAN` instead of hardcoding `http://media-stack`.

Note:

- Keep `MEDIA_PUBLIC_BASE_URL_LAN` as the single canonical base used for generating manifest URLs.
- Independently, continue to serve the portal and addon routes on both `http://media-stack` and `http://media-stack.home.arpa` so existing clients/bookmarks keep working.

Rationale:

- The Stremio device must resolve the host; VM-local `/etc/hosts` aliases do not help the TV.
- This makes the “public address” an explicit, configurable contract.

## Optional Later: LAN HTTPS

If/when you revisit HTTPS, there are two main options:

1. Self-signed/internal CA (`tls internal` in Caddy): good for desktops/phones where you can install the CA; often problematic on TVs.
2. Publicly trusted cert (Let’s Encrypt): requires a real domain you control and DNS-01 validation (split-horizon LAN DNS then points that hostname at your LAN IP).

Do not enable HTTP->HTTPS redirects until `https://...` is reachable and trusted on the target Stremio devices.

### Response Payload Examples (What “Good” Looks Like)

These are representative shapes you should see after fixes.

Catalog response includes a reachable poster URL (Home tiles work):

```json
{
  "metas": [
    {
      "id": "jellio:2f0b9f8d-4d2a-4a2a-9c6d-0c2e51d4f5e1",
      "type": "series",
      "name": "Avatar: The Last Airbender",
      "poster": "http://media-stack.home.arpa/jf/Items/2f0b9f8d4d2a4a2a9c6d0c2e51d4f5e1/Images/Primary?maxWidth=400"
    }
  ]
}
```

Stream response URL routes via `/jf` (playback is routable and authorized):

```json
{
  "streams": [
    {
      "name": "Jellyfin",
      "url": "http://media-stack.home.arpa/jf/Videos/9c2a4c621a2b3c4d5e6f7a8b9c0d1e2f/master.m3u8"
    }
  ]
}
```

### Logging/diagnostics improvements (to shorten future incidents)

1. Enable Caddy access logs (if not already) for `:80` and `:8097` and keep them in a predictable location.
2. Add a small validator output summary that prints:
   - manifest URL
   - derived addon base
   - the first poster URL and its HTTP status
   - the first stream URL and its HTTP status/content-type

## Verification Strategy (Implementation Verification + Operational Verification)

### 1) Automated verification on the VM (extend existing validator)

File: `packs/scripts/bootstrap-validate-streaming-stack.sh`

Add a validation step that:

1. Loads `http://127.0.0.1:80/jellio-manifest.lan.url`
2. Fetches the manifest JSON
3. Picks the first declared catalog and fetches it
4. Validates:
   - `metas` is non-empty
   - `metas[0].poster` exists and returns `200` with `Content-Type: image/*`
5. Fetches the corresponding stream JSON and validates:
   - `streams` is non-empty
   - `streams[0].url` returns `200` (or, if HLS, that the `.m3u8` fetch succeeds)

This makes the integration break detectable during `vmctl apply`.

### 2) Manual verification on LAN clients (Stremio reality check)

1. Reinstall the addon using the LAN manifest link on the portal.
2. Confirm:
   - Home tiles render posters for a library row
   - playback works for multiple items and does not show `Loading failed`
3. Repeat on the target problematic device (Samsung TV Stremio).

### 3) Tailnet verification

If you use the Tailnet manifest:

- repeat the same validator logic against the Tailnet manifest URL (HTTPS)
- ensure Tailscale `serve` still targets `http://127.0.0.1:80` so `/jf/*` is available over Tailnet HTTPS

## TDD Plan (Tests, Then Fix, Then Regression Coverage)

### Test suite goals

- Catch regressions in generated config (Caddy routing and env contracts).
- Catch regressions in manifest generation logic (`PublicBaseUrl` correctness).
- Catch runtime regressions with a smoke check that exercises real HTTP paths.

### 1) Reproduce and capture the broken behavior (pre-fix)

Add a failing runtime smoke check (in `bootstrap-validate-streaming-stack.sh`) that:

- fetches catalog
- attempts to `HEAD` the poster URL and expects HTTP 200

On current code, this should fail if the poster URL is built on a non-proxied base.

### 2) Add deterministic unit/golden tests (repo-level)

Add/adjust tests in the repo’s existing backend/template test area:

- Test file: `crates/backend-terraform/src/lib.rs`
- Fixtures: `crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/`

1. Assert generated `caddyfile.media` contains the `/jf/*` handler and token injection header.
2. Assert `media.env` includes `MEDIA_PUBLIC_BASE_URL_LAN` (or whatever env contract is adopted).
3. Assert `bootstrap-media.sh` preserves `JELLYFIN_STREMIO_AUTH_TOKEN`.

Concrete edits expected in `crates/backend-terraform/src/lib.rs`:

- Update `media_caddy_fixture_uses_service_port_mode_without_prefix_routes()` to assert `handle_path /jf/*` exists and that it injects `X-MediaBrowser-Token {$JELLYFIN_STREMIO_AUTH_TOKEN}`.
- Update the fixture `caddyfile.media` accordingly (the test reads the fixture content via `include_str!`).
- Add assertions that the generated `caddyfile.media` includes `auto_https off` and removes `Strict-Transport-Security`, to prevent accidental reintroduction of HTTP->HTTPS redirects while LAN HTTPS is deferred.
- Add assertions that the generated `caddyfile.media` binds both `media-stack:80` and `media-stack.home.arpa:80` so both hostnames remain supported.
If/when you later add LAN HTTPS, extend these fixture assertions to cover `443:443` and persistent Caddy `/data` and `/config` mounts.

### 3) Implement the fix to satisfy tests

Apply the code/config changes described above.

### 4) Extend runtime validation (post-fix regression guard)

Keep the manifest->catalog->poster->stream smoke checks as permanent coverage so that future routing/template refactors don’t silently break Stremio again.

## DRY Considerations (Avoiding Duplication)

### DRY base URL construction

Create one canonical “public base” contract and use it everywhere:

- `MEDIA_PUBLIC_BASE_URL_LAN` for LAN devices
- derived Tailnet base (from `tailscale status --json`) for Tailnet devices
- optional Cloudflare base (`CLOUDFLARE_PUBLIC_BASE_URL`)

Then compute:

- `PUBLIC_BASE_FOR_STREMIO = ${MEDIA_PUBLIC_BASE_URL}/jf`

Do not re-hardcode `http://media-stack` in multiple places.

### DRY manifest generation helpers

In `bootstrap-jellio.sh`, keep one helper to:

- normalize base URLs (strip trailing `/`)
- build `PublicBaseUrl` (append `/jf` exactly once)
- b64url encode the JSON payload

### DRY tests and fixtures

Write one small helper for tests to:

- decode the Jellio config b64url string
- assert fields (`PublicBaseUrl`, `AuthToken` existence)

Reuse this across tests that validate manifest generation.

## Task Breakdown (Step-by-Step)

### Investigation

1. Run the deterministic investigation commands in “Structured Investigation” and save:
   - manifest JSON
   - one catalog JSON
   - one stream JSON
   - `curl -I` results for the first poster and first stream URL
2. Collect Caddy + Jellyfin logs around those requests.

### Reproduction

1. From the stream response, pick a stream URL that Stremio uses and confirm it fails in the same way (404/401/other).
2. From the catalog response, confirm the poster URL fails in the same way (if Home tiles are missing).

### Fixes (code/config)

1. Update `packs/templates/caddyfile.media.hbs` to add `/jf/*` Jellyfin reverse proxy with token injection.
2. Update `packs/scripts/bootstrap-jellio.sh` to:
   - persist `JELLYFIN_STREMIO_AUTH_TOKEN`
   - set `PublicBaseUrl` to `${ADDON_BASE}/jf`
   - use `MEDIA_PUBLIC_BASE_URL_LAN` for LAN base instead of hardcoding
3. Update `packs/templates/media.env.hbs` to add `MEDIA_PUBLIC_BASE_URL_LAN`.
4. Update `packs/scripts/bootstrap-media.sh` to preserve `JELLYFIN_STREMIO_AUTH_TOKEN`.

### Validation

1. Extend `packs/scripts/bootstrap-validate-streaming-stack.sh` with the manifest->catalog->poster->stream smoke checks.
2. Add an explicit HTTP no-redirect check:
   - `curl -sSI -o /dev/null -w '%{http_code}\n' http://127.0.0.1:80/healthz` must return `200`.
   - `curl -sSI -H 'Host: media-stack.home.arpa' http://127.0.0.1:80/healthz` must not include a `Location:` header.
2. Run the validator on an actual VM and verify it passes.
3. Validate in Stremio clients (desktop + TV).

### Regression prevention

1. Add unit/golden tests for generated Caddyfile and env contracts.
2. Keep the runtime validator checks permanently enabled.

## Definition of Done

### Playback (Issue 1)

- On LAN, at least 3 Jellyfin library items play in Stremio without `Loading failed`.
- Stream URLs returned by the addon are reachable from a LAN machine:
  - `curl -I "$URL"` returns HTTP 200 (or HLS playlist fetch returns 200 and segments are reachable).
- Caddy/Jellyfin logs show the requests routing through the intended path (`/jf/*` to Jellyfin).
- `vmctl apply` remains idempotent and does not break the manifest URLs across runs.
- LAN base URL is reachable end-to-end:
  - `curl -I http://media-stack/healthz` returns `200` (no 3xx).
  - `curl -I http://media-stack.home.arpa/healthz` returns `200` (no 3xx).
  - The response does not contain `Location:` or `Strict-Transport-Security:`.

### Metadata / Artwork (Issue 2)

- For library rows in Stremio Home, items with Jellyfin artwork show posters (no default icons where Jellyfin has primary images).
- Catalog poster URLs are reachable:
  - `curl -I "$POSTER_URL"` returns HTTP 200 with an image content-type.
- Home tiles and detail page show consistent artwork for the same item (no “detail-only art” discrepancy).
