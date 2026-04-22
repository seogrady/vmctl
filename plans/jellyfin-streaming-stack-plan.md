# Jellyfin Streaming Stack Extension Plan (Streamyfin + Jellysearch + Jellio/Stremio)

Date: 2026-04-22

## Objective

Extend the existing `media-stack` Jellyfin VM managed by `vmctl apply` with:

- Streamyfin Jellyfin companion plugin (server-side configuration + notifications endpoint)
- Jellysearch (Meilisearch-backed full-text search proxy) wired transparently behind the reverse proxy
- Jellio+ (Jellyfin plugin) configured as a Stremio addon, accessible over Tailscale HTTPS with no Cloudflare dependency

Constraints satisfied:

- Full automation via `vmctl apply` (no dashboard clicks, no manual edits on the VM)
- Idempotent provisioning
- External access primarily via Tailscale (`tailscale serve`), local LAN via hostnames
- DRY config: one set of canonical URLs/ports/credentials reused by all components

## Baseline (Current Repo State)

This plan assumes the repo’s current architecture:

- `media-stack` VM runs Docker + `docker compose` under `/opt/media`
- Compose and env are generated from `packs/templates/*` and `packs/services/*.toml`
- Provisioning is handled by scripts in `packs/scripts/*` (copied into the resource workspace and executed during apply)
- Tailscale is installed on `media-stack` and `bootstrap-ui-routing.sh` configures `tailscale serve` to expose `http://127.0.0.1:80` as tailnet HTTPS
- Jellyfin is deployed as `lscr.io/linuxserver/jellyfin` with config at `/opt/media/config/jellyfin`

## Architecture

### Components

- Jellyfin (existing)
- Caddy (existing, currently static index; will be upgraded to reverse proxy routing)
- Streamyfin plugin (inside Jellyfin)
  - Plugin id: `1e9e5d38-6e67-4615-8719-e98a5c34f004`
  - Latest manifest version pinned in this plan: `0.66.0.0`
- Jellysearch (new Docker service)
  - Image: `domistyle/jellysearch`
  - Requires Meilisearch and read-only access to Jellyfin config/db
- Meilisearch (new Docker service)
  - Image: `getmeili/meilisearch:v1.9`
- Jellio+ plugin (inside Jellyfin)
  - Plugin guid: `e874be83-fe36-4568-abac-f5ce0574b409`
  - Latest manifest version pinned in this plan: `1.4.0.0`
- Stremio client(s) (not provisioned by vmctl; consumes the addon URL we generate)

### Canonical URLs (LAN + Tailnet)

We standardize on path-prefix routing via Caddy on port 80, then expose that single ingress over Tailscale HTTPS:

- LAN (no TLS): `http://media-stack.${domain}`
- Tailnet (TLS via `tailscale serve`): derive from `tailscale status --json`:

  ```bash
  TAILSCALE_DNS_NAME="$(tailscale status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"Self\"][\"DNSName\"].rstrip(\".\"))')"
  echo "https://${TAILSCALE_DNS_NAME}"
  ```

Service paths:

- Jellyfin: `/jellyfin`
- Jellio config UI: `/jellyfin/jellio/configure`
- Jellio addon manifest (static, pre-generated): `/jellyfin/jellio/${JELLIO_CONFIG_B64}/manifest.json`
  - generated for LAN and tailnet during apply
  - persisted as `JELLIO_STREMIO_MANIFEST_URL_LAN` and `JELLIO_STREMIO_MANIFEST_URL_TAILNET`
  - optional Cloudflare manifest persisted as `JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE`
- Streamyfin notifications endpoint: `/jellyfin/Streamyfin/notification`

### Request/Data Flow Diagrams

#### 1) Jellyfin UI + API (normal)

```
Client (LAN or Tailnet)
  -> Caddy :80 (LAN) / tailscale serve -> Caddy
    -> /jellyfin/* reverse_proxy -> jellyfin:8096
      -> Jellyfin API + web UI
```

#### 2) Jellysearch-accelerated search (transparent to clients)

Jellysearch works by intercepting requests that include `searchTerm`/`SearchTerm` query args.

```
Client search request
  -> GET /jellyfin/Items?...&SearchTerm=robot
  -> Caddy matcher (query contains searchTerm/SearchTerm)
    -> reverse_proxy jellysearch:5000
      -> jellysearch queries meilisearch index
      -> jellysearch forwards resolved item ids to Jellyfin
      -> jellysearch returns a Jellyfin-compatible response
```

#### 3) Stremio playback via Jellio+

```
Stremio (on tailnet) installs addon from:
  https://${TAILSCALE_DNS_NAME}/jellyfin/jellio/${JELLIO_CONFIG_B64}/manifest.json

Stremio (on LAN, no Tailscale) installs addon from:
  http://media-stack.${domain}/jellyfin/jellio/${JELLIO_CONFIG_B64}/manifest.json

Stremio browse:
  -> /catalog/* or /meta/* on that same base
  -> Caddy -> Jellyfin -> Jellio+ plugin controllers -> Jellyfin library APIs

Stremio stream:
  -> /stream/* from Jellio+ plugin
  -> Jellyfin playback endpoints (tokenized) -> media stream/transcode
```

## Networking & Access Design (No Manual Setup)

### Tailscale (primary external access)

- Keep the existing model: `bootstrap-ui-routing.sh` asserts
  - `tailscale serve --yes --bg http://127.0.0.1:80`
- This yields a trusted HTTPS URL on the tailnet without Cloudflare.
- Ensure `tailscale funnel` is not enabled (private-only access).

### Local Network Hostnames

Use the repo’s existing search domain (`home.arpa`) and VM name:

- `media-stack.${domain}` resolves on the LAN via your existing DHCP/DNS environment

All services are reachable through the single host + paths above.

### Samsung TV (Stremio) operational notes

- The Samsung (Tizen) Stremio client typically relies on your Stremio account sync rather than offering a robust “paste manifest URL” flow on the TV itself.
- The reliable workflow is:
  1. Open the manifest URL on a phone/PC where you are logged into Stremio (or `web.stremio.com`).
  2. Add/install the addon.
  3. On the Samsung TV Stremio app, trigger sync and test playback.

This plan supports three manifest URL types so you can pick what your Samsung client accepts:

- LAN HTTP manifest: works if the Samsung app accepts `http://` addon manifests and the TV can reach the LAN hostname.
- Tailnet HTTPS manifest: works if the Samsung TV device is on Tailscale (not your scenario).
- Optional Cloudflare HTTPS manifest: works without Tailscale on the TV, but is publicly reachable and adds a Cloudflare dependency (disabled by default).

## Jellysearch: Production Notes and Risk

Jellysearch upstream warning (must be acknowledged in production):

- Search results may include items from libraries the user is not authorized to view (permission filtering not enforced by Jellysearch).

Mitigation strategy in this plan:

- Expose Jellyfin primarily via Tailscale (private tailnet) and LAN only.
- Create a dedicated “stremio” Jellyfin user for the addon with limited libraries; do not use admin tokens in Stremio.
- Avoid exposing Jellysearch directly to the public internet.

## Stremio + Jellyseerr: What’s Possible

What you can achieve without writing a custom Stremio addon:

- Global search and discovery happens in Stremio via Cinemeta (and other metadata addons).
- Playback is provided by Jellio+ when the media exists in Jellyfin.
- If the media does not exist, Jellio+ can return a “Request via Jellyseerr” stream entry for IMDB-based ids (`tt...`), which triggers a Jellyseerr request.
- After Jellyseerr fulfills the request and Jellyfin indexes the media, the same Stremio title becomes streamable via Jellio+ (the stream endpoint checks Jellyfin each time).

What you cannot do with stock Jellio+ today:

- Replace Stremio’s search UI to be “Jellyseerr search” (Jellio+ catalogs/search are library-scoped; they do not query Jellyseerr discover/search endpoints).
- Auto-play on the TV “the moment it becomes available” without user action (Stremio does not provide an addon-driven push mechanism). You can approximate this with notifications (see below) and a manual retry.

Optional future enhancement (custom dev):

- Add Jellyseerr-backed Stremio catalogs (Trending, Popular, Requested, etc.) by extending Jellio+ to call Jellyseerr `/api/v1/discover/*` and `/api/v1/request` endpoints and returning `tt...` ids. This is feasible but out of scope for this implementation plan.

## vmctl Integration (Concrete Repo Changes)

### 1) `vmctl.toml` changes

Update the `media-stack` resource to:

1. Enable path-prefix routing and set Jellyfin BaseUrl to `/jellyfin`.
2. Add `jellysearch` + `meilisearch` services to the media stack.
3. Centralize URLs so all bootstrap scripts use the same canonical base.

Example (drop-in for your `[[resources]] name = "media-stack"` block):

```toml
[resources.features.media_services]
enabled = true
minimal = true
services = [
  "caddy",
  "jellyfin",
  "jellysearch",
  "meilisearch",
  "sonarr",
  "radarr",
  "prowlarr",
  "qbittorrent-vpn",
  "jellyseerr",
  "bazarr",
  "jellystat-db",
  "jellystat",
]

# Caddy base + path-prefix routing.
jellyfin_url = "http://media-stack.${domain}/jellyfin"
jellyfin_base_url = "/jellyfin"

# Keep existing admin credentials wiring.
jellyfin_admin_user = "${JELLYFIN_ADMIN_USER}"
jellyfin_admin_password = "${JELLYFIN_ADMIN_PASSWORD}"

ui_homepage_enabled = true
ui_homepage_title = "Media Stack"

[resources.features.media_services.exposure]
tailscale_https_enabled = true
tailscale_https_target = "http://127.0.0.1:80"
```

### 2) New service packs (`packs/services`)

Add:

- `packs/services/meilisearch.toml`
- `packs/services/jellysearch.toml`

This plan requires a small compose-generation enhancement so service packs can define container-specific environment variables (needed for Jellysearch). The definitive service pack definitions below assume that enhancement is implemented (see `docker-compose.media.hbs` section).

`packs/services/meilisearch.toml`:

```toml
name = "meilisearch"
container_type = "docker"

[image]
name = "getmeili/meilisearch"
tag = "v1.9"

[ports]
published = ["7700:7700"]

[volumes]
mounts = ["${CONFIG_PATH}/meilisearch:/meili_data"]

[environment]
MEILI_MASTER_KEY = "${MEILI_MASTER_KEY}"
```

`packs/services/jellysearch.toml`:

```toml
name = "jellysearch"
container_type = "docker"

[image]
name = "domistyle/jellysearch"
tag = "latest"

[ports]
published = ["5000:5000"]

[volumes]
# Must match the host directory mounted as Jellyfin's /config.
mounts = ["${CONFIG_PATH}/jellyfin:/config:ro"]

[environment]
MEILI_URL = "http://meilisearch:7700"
MEILI_MASTER_KEY = "${MEILI_MASTER_KEY}"
JELLYFIN_URL = "http://jellyfin:8096"
JELLYFIN_CONFIG_DIR = "/config"
INDEX_CRON = "0 0/5 * ? * * *"
```

Notes:

- This plan keeps the host ports published for compatibility with the current `packs/templates/docker-compose.media.hbs`. The canonical ingress is still Caddy on `:80`.

### 3) Template updates (`packs/templates`)

#### 3.1 `packs/templates/media.env.hbs`

Add secrets and bootstrap state (generated/preserved by `bootstrap-media.sh` just like existing secrets).

Important: Jellysearch requires env vars named exactly as upstream documents (`JELLYFIN_URL`, `JELLYFIN_CONFIG_DIR`, `INDEX_CRON`, `MEILI_URL`, `MEILI_MASTER_KEY`). Those names conflict with existing stack env (`JELLYFIN_URL` already has meaning for other bootstrap scripts). To keep the stack DRY and correct, this plan makes per-service environment supported in compose generation (see `docker-compose.media.hbs` changes below) so we do not overload global `.env` names.

```env
MEILI_MASTER_KEY=
# Streamyfin + Jellio bootstrap
JELLYFIN_STREMIO_USER=stremio
JELLYFIN_STREMIO_PASSWORD=
JELLIO_STREMIO_MANIFEST_URL_LAN=
JELLIO_STREMIO_MANIFEST_URL_TAILNET=
JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE=

# Optional: Cloudflare Tunnel support (disabled unless CLOUDFLARED_TOKEN is set and cloudflared service is enabled)
CLOUDFLARE_PUBLIC_BASE_URL=
CLOUDFLARED_TOKEN=

# Jellyseerr request integration for Jellio+ (auto-generated and injected into the Jellyseerr container as API_KEY)
JELLYSEERR_API_KEY=
JELLYSEERR_INTERNAL_URL=http://jellyseerr:5055
```

Rationale:

- `MEILI_MASTER_KEY` is generated once and reused by `meilisearch` and `jellysearch`.
- Jellio manifest URLs are computed during apply and preserved for subsequent runs:
  - `JELLIO_STREMIO_MANIFEST_URL_LAN`
  - `JELLIO_STREMIO_MANIFEST_URL_TAILNET`
  - optional `JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE`

`bootstrap-media.sh` must be updated so these values are preserved across reruns:

- Extend the `preserve = {...}` set to include:
  - `MEILI_MASTER_KEY`
  - `JELLYFIN_STREMIO_PASSWORD`
  - `JELLIO_STREMIO_MANIFEST_URL_LAN`
  - `JELLIO_STREMIO_MANIFEST_URL_TAILNET`
  - `JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE`
  - `CLOUDFLARE_PUBLIC_BASE_URL`
  - `CLOUDFLARED_TOKEN`
  - `JELLYSEERR_API_KEY`

#### 3.2 `packs/templates/docker-compose.media.hbs` (support per-service environment)

Add a supported `environment` stanza on service packs and render it into compose so Jellysearch can receive the exact env vars it requires without polluting the shared `.env`.

Proposed service pack schema addition (Rust): extend `ServicePack` with:

```rust
#[serde(default)]
pub environment: BTreeMap<String, Value>,
```

Then update the compose template to render it:

```hbs
{{#if environment}}
    environment:
{{#each environment}}
      - "{{@key}}={{this}}"
{{/each}}
{{/if}}
```

With this, the Jellysearch service pack becomes:

```toml
name = "jellysearch"
container_type = "docker"

[image]
name = "domistyle/jellysearch"
tag = "latest"

[ports]
published = ["5000:5000"]

[volumes]
mounts = ["${CONFIG_PATH}/jellyfin:/config:ro"]

[environment]
MEILI_URL = "http://meilisearch:7700"
MEILI_MASTER_KEY = "${MEILI_MASTER_KEY}"
JELLYFIN_URL = "http://jellyfin:8096"
JELLYFIN_CONFIG_DIR = "/config"
INDEX_CRON = "0 0/5 * ? * * *"
```

And Meilisearch gets:

```toml
[environment]
MEILI_MASTER_KEY = "${MEILI_MASTER_KEY}"
```

#### 3.2 `packs/templates/caddyfile.media.hbs`

Replace the current static-only config with a reverse proxy that:

- serves the portal homepage at `/`
- proxies Jellyfin under `/jellyfin/*`
- routes Jellyfin search requests (query `searchTerm` or `SearchTerm`) to Jellysearch

```hbs
:80 {
  encode gzip

  handle_path /healthz {
    respond "ok" 200
  }

  # Portal (service directory)
  handle / {
    root * /srv/ui-index
    file_server
  }

  # Jellyfin under /jellyfin
  handle_path /jellyfin/* {
    # Intercept search requests and send them to Jellysearch.
    @jellysearch query searchTerm=* SearchTerm=*
    handle @jellysearch {
      reverse_proxy jellysearch:5000
    }

    reverse_proxy jellyfin:8096
  }
}
```

This makes Jellysearch compatible with Streamyfin (and any other Jellyfin client) without client changes: clients simply use the `/jellyfin` base URL.

#### 3.3 `packs/templates/media-index.html.hbs` (show Jellio manifest URL without re-rendering templates)

Templates are rendered before bootstrap scripts run, but the Jellio Stremio manifest URL is generated during bootstrap. To avoid adding a “re-render templates after bootstrap” step, the portal should load the manifest URL from a small static file that bootstrap writes.

Bootstrap writes:

- `/opt/media/config/caddy/ui-index/jellio-manifest.lan.url` (single line: the LAN manifest URL)
- `/opt/media/config/caddy/ui-index/jellio-manifest.tailnet.url` (single line: the tailnet manifest URL)
- `/opt/media/config/caddy/ui-index/jellio-manifest.cloudflare.url` (single line: the Cloudflare manifest URL, only when enabled)

Portal HTML loads it at runtime from:

- `GET /jellio-manifest.lan.url` (served by Caddy’s static file_server at `/`)
- `GET /jellio-manifest.tailnet.url` (served by Caddy’s static file_server at `/`)
- `GET /jellio-manifest.cloudflare.url` (served by Caddy’s static file_server at `/`, only present when enabled)

Portal snippet (path-prefix links + dynamic Jellio manifest link):

```hbs
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>{{features.media_services.ui_homepage_title}}</title>
  <style>
    :root { color-scheme: dark light; }
    body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; max-width: 760px; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; }
    li { margin: .75rem 0; padding: .75rem; border: 1px solid #8884; border-radius: 8px; }
    a { font-weight: 600; text-decoration: none; }
    code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  </style>
</head>
<body>
  <h1>{{features.media_services.ui_homepage_title}}</h1>
  <ul>
    <li>
      <a href="/jellyfin">Jellyfin</a>
      <div>Media server</div>
    </li>
    <li>
      <a href="/jellyfin/jellio/configure">Jellio+ (Stremio addon)</a>
      <div>Configuration UI for Stremio addon generation</div>
    </li>
    <li>
      <a id="jellio-manifest-lan-link" href="#">Stremio Manifest (LAN HTTP)</a>
      <div>Use this if your device is on the LAN and Stremio accepts http manifests.</div>
    </li>
    <li>
      <a id="jellio-manifest-tailnet-link" href="#">Stremio Manifest (Tailnet HTTPS)</a>
      <div>Use this for devices on Tailscale (works best, private by default).</div>
    </li>
    <li>
      <a id="jellio-manifest-cloudflare-link" href="#">Stremio Manifest (Cloudflare HTTPS, optional)</a>
      <div>Use this for devices not on Tailscale (public HTTPS; disabled unless configured).</div>
    </li>
  </ul>
  <script>
    (function () {
      function wire(id, urlPath) {
        fetch(urlPath, { cache: "no-store" })
          .then(function (r) { return r.ok ? r.text() : ""; })
          .then(function (t) {
            var url = (t || "").trim();
            var a = document.getElementById(id);
            if (!a) return;
            if (!url) {
              a.style.opacity = "0.4";
              a.style.pointerEvents = "none";
              return;
            }
            a.href = url;
          })
          .catch(function () {});
      }

      wire("jellio-manifest-lan-link", "/jellio-manifest.lan.url");
      wire("jellio-manifest-tailnet-link", "/jellio-manifest.tailnet.url");
      wire("jellio-manifest-cloudflare-link", "/jellio-manifest.cloudflare.url");
    })();
  </script>
</body>
</html>
```

### 4) Role changes (`packs/roles/media_stack.toml`)

1. Add new services to the default list (or expect the resource override in `vmctl.toml`).
2. Append new bootstrap scripts in a safe order:

Recommended bootstrap ordering:

1. `bootstrap-media.sh` (directories, env merge, compose install)
2. `bootstrap-ui-routing.sh` (Caddy + tailscale serve exposure)
3. `bootstrap-jellyfin.sh` (Jellyfin base url, admin bootstrap)
4. `bootstrap-jellysearch.sh` (meili + jellysearch readiness + indexing validation)
5. `bootstrap-jellyfin-plugins.sh` (Streamyfin + Jellio plugin install, restart Jellyfin if changed)
6. `bootstrap-streamyfin.sh` (Streamyfin plugin config + api key wiring)
7. `bootstrap-jellio.sh` (Jellio config + generate Stremio manifest URL)
8. Existing *arr/jellyseerr/jellystat* scripts (if you want Seerr integration values available early, run `bootstrap-jellyseerr.sh` before `bootstrap-streamyfin.sh` and `bootstrap-jellio.sh`)

## Provisioning Scripts (Implementation-Ready)

### `packs/scripts/bootstrap-jellysearch.sh`

Responsibilities:

- Start `meilisearch` and `jellysearch`
- Ensure `MEILI_MASTER_KEY` is set (generate once, preserve across reruns)
- Validate meilisearch health (`GET http://127.0.0.1:7700/health`)
- Validate jellysearch integration by making a Jellyfin search call through Caddy and checking HTTP 200

Example (core logic):

```bash
#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"
COMPOSE_FILE="$STACK_DIR/docker-compose.yml"

set -a
. "$ENV_FILE"
set +a

docker compose --env-file "$ENV_FILE" -f "$COMPOSE_FILE" up -d meilisearch jellysearch

python3 - <<'PY'
import os, time, urllib.request, urllib.error

def wait_ok(url, timeout=180):
    started = time.time()
    while time.time() - started < timeout:
        try:
            with urllib.request.urlopen(url, timeout=10) as r:
                if 200 <= r.status < 300:
                    return
        except Exception:
            time.sleep(2)
    raise RuntimeError(f"timed out waiting for {url}")

wait_ok("http://127.0.0.1:7700/health")

# Jellysearch is query-driven; validate via Jellyfin search through Caddy.
wait_ok("http://127.0.0.1/jellyfin/Items?SearchTerm=test&Limit=1")
PY
```

### `packs/scripts/bootstrap-jellyfin-plugins.sh` (Streamyfin + Jellio)

Install method (fully automated):

- Download the pinned release zip
- Verify checksum from upstream manifest
- Extract into Jellyfin plugins directory under `/opt/media/config/jellyfin/plugins/Streamyfin` and `/opt/media/config/jellyfin/plugins/Jellio`
- Restart Jellyfin container if any plugin files changed

Pinned upstream artifacts in this plan:

- Streamyfin plugin `0.66.0.0`
  - URL: `https://github.com/streamyfin/jellyfin-plugin-streamyfin/releases/download/0.66.0.0/streamyfin-0.66.0.0.zip`
  - checksum: `6c4daa669154318ba2b73ba2289ecf2c`
- Jellio+ plugin `1.4.0.0`
  - URL: `https://github.com/InfiniteAvenger/jellio-plus/releases/download/v1.4.0/jellio_1.4.0.0.zip`
  - checksum: `54e908fa8ba0fdb3b40cc10125e0d364`

Example install logic:

```bash
PLUGIN_DIR="/opt/media/config/jellyfin/plugins"

install_plugin_zip() {
  local name="$1" url="$2" checksum="$3"
  local tmp="/tmp/${name}.zip"
  curl -fsSL "$url" -o "$tmp"
  echo "${checksum}  ${tmp}" | md5sum -c -
  install -d "${PLUGIN_DIR}/${name}"
  unzip -o "$tmp" -d "${PLUGIN_DIR}/${name}" >/dev/null
}
```

### `packs/scripts/bootstrap-streamyfin.sh` (configure Streamyfin plugin)

Automation approach:

1. Authenticate to Jellyfin as admin (reuse existing env vars).
2. GET Streamyfin plugin configuration via:
   - `GET /Plugins/1e9e5d38-6e67-4615-8719-e98a5c34f004/Configuration`
3. Patch the JSON to set real URLs (no “Enter ...” values):
   - `Config.settings.seerrServerUrl.value` = the Jellyseerr base you expose (LAN or tailnet)
   - `Config.settings.hiddenLibraries.value` = `[]`
4. POST config back via:
   - `POST /Plugins/1e9e5d38-6e67-4615-8719-e98a5c34f004/Configuration`

Key detail:

- The Streamyfin plugin configuration class uses lowercase JSON fields under `Config` (e.g. `settings`, `seerrServerUrl`, `value`). Patch the fetched object rather than hand-constructing it.

### `packs/scripts/bootstrap-jellio.sh` (configure Jellio+ and generate a static Stremio manifest URL)

Config schema (from upstream `PluginConfiguration.cs`):

- `JellyseerrEnabled` (bool)
- `JellyseerrUrl` (string)
- `JellyseerrApiKey` (string)
- `PublicBaseUrl` (string)
- `SelectedLibraries` (list of GUIDs)

Automated steps:

1. Determine which Jellyfin libraries to expose (at minimum Movies + TV).
   - `GET /Library/VirtualFolders` to map to library GUIDs
2. Set Jellio plugin config via Jellyfin plugin config API:
   - `POST /Plugins/e874be83-fe36-4568-abac-f5ce0574b409/Configuration`
   - If `JELLYSEERR_API_KEY` is present, set:
     - `JellyseerrEnabled = true`
     - `JellyseerrUrl = ${JELLYSEERR_INTERNAL_URL}` (internal Docker URL)
     - `JellyseerrApiKey = ${JELLYSEERR_API_KEY}`
   - Otherwise set `JellyseerrEnabled = false` and omit Jellyseerr fields in the addon config payloads.
3. Ensure a dedicated Jellyfin user exists for Stremio addon auth:
   - username: `stremio`
   - password: generated once and preserved in `/opt/media/.env` (`JELLYFIN_STREMIO_PASSWORD`)
4. Authenticate the `stremio` user:
   - `POST /Users/AuthenticateByName` -> `AccessToken`
5. Compute the LAN Jellyfin base URL from `JELLYFIN_URL` (this is what LAN clients should use):

   - `JELLYFIN_URL` is already set by vmctl to the canonical external Jellyfin base, e.g. `http://media-stack.home.arpa/jellyfin`
   - Reuse it for LAN manifest generation:

   ```bash
   JELLYFIN_LAN_BASE_URL="${JELLYFIN_URL}"
   ```

6. Compute the tailnet Jellyfin base URL (used for Stremio access):

   ```bash
   TAILSCALE_DNS_NAME="$(tailscale status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"Self\"][\"DNSName\"].rstrip(\".\"))')"
   JELLYFIN_TAILNET_BASE_URL="https://${TAILSCALE_DNS_NAME}/jellyfin"
   ```

7. Optional: if Cloudflare tunnel is enabled, define the public Jellyfin base URL (must include `/jellyfin`):

   - `CLOUDFLARE_PUBLIC_BASE_URL` is provided via `.env` and must be a full URL like `https://media.example.com/jellyfin`.

8. Build the Jellio base64url config payload (one per access mode):

   - LAN config: `PublicBaseUrl = ${JELLYFIN_LAN_BASE_URL}`
   - Tailnet config: `PublicBaseUrl = ${JELLYFIN_TAILNET_BASE_URL}`
   - Cloudflare config (optional): `PublicBaseUrl = ${CLOUDFLARE_PUBLIC_BASE_URL}`
   - If Jellyseerr integration is enabled, include `JellyseerrEnabled`, `JellyseerrUrl`, `JellyseerrApiKey` in each payload.

```json
{
  "ServerName": "media-stack",
  "AuthToken": "${STREMIO_ACCESS_TOKEN}",
  "LibrariesGuids": ["${LIB_GUID_1}", "${LIB_GUID_2}"],
  "JellyseerrEnabled": true,
  "JellyseerrUrl": "http://jellyseerr:5055",
  "JellyseerrApiKey": "${JELLYSEERR_API_KEY}",
  "PublicBaseUrl": "${BASE_URL_FOR_THIS_MANIFEST}"
}
```

9. Base64url-encode each UTF-8 JSON (no padding) and form each manifest URL:

```
${BASE_URL_FOR_THIS_MANIFEST}/jellio/${JELLIO_CONFIG_B64}/manifest.json
```

10. Persist the manifest URLs into `/opt/media/.env` for stable idempotence:

- `JELLIO_STREMIO_MANIFEST_URL_LAN`
- `JELLIO_STREMIO_MANIFEST_URL_TAILNET`
- `JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE` (only when Cloudflare is enabled)

11. Write URL files for the portal (same values):

- `/opt/media/config/caddy/ui-index/jellio-manifest.lan.url`
- `/opt/media/config/caddy/ui-index/jellio-manifest.tailnet.url`
- `/opt/media/config/caddy/ui-index/jellio-manifest.cloudflare.url` (only when Cloudflare is enabled)

12. Persist the *tailnet* manifest URL as the “default” for validation steps that need a single value:

- set `JELLIO_STREMIO_MANIFEST_URL_TAILNET` and use it in validators when a single URL is required.

## Optional: Cloudflare Tunnel (Disabled By Default)

This is optional support for TVs/devices that cannot use Tailscale and cannot install an addon from an `http://` LAN manifest URL.

### Security posture

- Cloudflare makes the addon base URL reachable from the public internet.
- If enabled, treat this as an internet-facing service:
  - do not use admin tokens in Stremio
  - use a dedicated Jellyfin user with only the libraries you intend to expose
  - keep Jellyfin behind the reverse proxy and do not publish additional ports

### vmctl and pack changes

1. Add a new service pack `packs/services/cloudflared.toml`:

```toml
name = "cloudflared"
container_type = "docker"

[image]
name = "cloudflare/cloudflared"
tag = "2026.3.0"

[environment]
TUNNEL_TOKEN = "${CLOUDFLARED_TOKEN}"

[settings]
command = "tunnel --no-autoupdate run --token ${CLOUDFLARED_TOKEN}"
```

2. Extend `packs/templates/docker-compose.media.hbs` to render optional `command` from a service pack:

```hbs
{{#if settings.command}}
    command: {{settings.command}}
{{/if}}
```

3. Enable Cloudflare tunnel by configuration only:

- add `cloudflared` to the `media-stack` `services = [...]` list
- set `CLOUDFLARED_TOKEN` in `.env` via `vmctl.toml [env]`
- set `CLOUDFLARE_PUBLIC_BASE_URL` in `.env` via `vmctl.toml [env]` (must include `/jellyfin`)

## Optional: Notifications (“Available Now”)

Stremio itself won’t auto-refresh, but you can deliver “request completed” notifications to phones/tablets via Streamyfin push notifications:

- Configure Jellyseerr Webhook notifications to call Streamyfin’s notifications endpoint:
  - endpoint: `${JELLYFIN_TAILNET_BASE_URL}/Streamyfin/notification`
  - header: `Authorization: MediaBrowser Token="<JELLYFIN_API_KEY>"`

If you want this fully automated, extend provisioning to:

- generate a Jellyfin API key dedicated to Streamyfin notifications
- set Jellyseerr’s webhook agent configuration to post “Request Available” events to the endpoint


### Cloudflare side (required when enabling)

- Create a Cloudflare Tunnel and obtain `CLOUDFLARED_TOKEN`.
- In the tunnel’s public hostname config, point the hostname to the local service URL `http://caddy:80`.
- Ensure the hostname you choose matches `CLOUDFLARE_PUBLIC_BASE_URL` (including the `/jellyfin` path on the client side).

## Streamyfin “Service” Clarification

The `streamyfin/streamyfin` repository is a client application (mobile/TV). There is no production server component to deploy from that repo.

In this stack:

- The “server-side Streamyfin component” is the Jellyfin plugin (installed + configured automatically).
- Remote access for Streamyfin clients is provided by Tailscale HTTPS ingress to Jellyfin (`/jellyfin`).

This meets the “Streamyfin plugin + service” intent without deploying deprecated companion servers.

## Task Breakdown (Step-by-Step)

### A) Service Installation

1. Extend service pack schema to support per-service environment variables.
2. Update `crates/packs/src/lib.rs` (`ServicePack`) to include `environment`.
3. Update `packs/templates/docker-compose.media.hbs` to render `environment:` for each service when present.
4. Add `packs/services/meilisearch.toml`.
5. Add `packs/services/jellysearch.toml`.
6. Add `jellysearch` and `meilisearch` to the `media-stack` service list in `vmctl.toml`.
7. Update `packs/templates/media.env.hbs` with Meili/Jellio/Cloudflare secrets and preserved state.
8. Update `packs/scripts/bootstrap-media.sh` to create `/opt/media/config/meilisearch` and preserve the new env keys.
9. Add `packs/scripts/bootstrap-jellysearch.sh`.

### B) Configuration

1. Update `packs/templates/caddyfile.media.hbs` to:
   - proxy `/jellyfin/*` to Jellyfin
   - divert `searchTerm`/`SearchTerm` query traffic to Jellysearch
2. Ensure `bootstrap-jellyfin.sh` sets Jellyfin BaseUrl to `/jellyfin` (already supported by existing script).
3. Add `packs/scripts/bootstrap-jellyfin-plugins.sh` to install:
   - Streamyfin plugin `0.66.0.0`
   - Jellio+ plugin `1.4.0.0`
4. Add `packs/scripts/bootstrap-streamyfin.sh` to patch Streamyfin plugin config.
5. Add `packs/scripts/bootstrap-jellio.sh` to:
   - patch Jellio plugin config
   - create `stremio` user
   - generate and persist `JELLIO_STREMIO_MANIFEST_URL_LAN` and `JELLIO_STREMIO_MANIFEST_URL_TAILNET`
   - optionally generate and persist `JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE` when Cloudflare is enabled

### C) Integration

1. Ensure Streamyfin plugin’s `seerrServerUrl` points at your Jellyseerr URL.
   - Tailnet-friendly value (no extra exposure): `https://${TAILSCALE_DNS_NAME}/jellyseerr` if you also route Jellyseerr through Caddy, otherwise keep it empty.
2. Ensure Jellio manifests are generated for:
   - LAN: `JELLYFIN_LAN_BASE_URL = ${JELLYFIN_URL}`
   - Tailnet: `JELLYFIN_TAILNET_BASE_URL = https://${TAILSCALE_DNS_NAME}/jellyfin`
   - Cloudflare (optional): `CLOUDFLARE_PUBLIC_BASE_URL` (must include `/jellyfin`)
3. Ensure the Jellysearch *container env* is set via its service pack (not global `.env`) to:
   - `JELLYFIN_URL=http://jellyfin:8096`
   - `JELLYFIN_CONFIG_DIR=/config`
   - `MEILI_URL=http://meilisearch:7700`
   - `MEILI_MASTER_KEY=${MEILI_MASTER_KEY}`
   - `INDEX_CRON=0 0/5 * ? * * *`

### D) Networking

1. `bootstrap-ui-routing.sh` asserts `tailscale serve` to `http://127.0.0.1:80`.
2. Caddy provides the only “canonical” ingress for Jellyfin + addons:
   - LAN: `http://media-stack.${domain}/jellyfin`
   - Tailnet: `https://${TAILSCALE_DNS_NAME}/jellyfin`

### E) Validation (must run during apply)

Add a single end-to-end validator script `packs/scripts/bootstrap-validate-streaming-stack.sh` and run it as the last bootstrap step for `media-stack`. It must check:

1. Jellyfin ready:
   - `GET http://127.0.0.1/jellyfin/System/Info/Public` returns 200
2. Streamyfin plugin installed:
   - `GET /jellyfin/Plugins` includes plugin id `1e9e5d38-6e67-4615-8719-e98a5c34f004`
3. Jellio plugin installed:
   - `GET /jellyfin/Plugins` includes plugin guid `e874be83-fe36-4568-abac-f5ce0574b409`
4. Meilisearch healthy:
   - `GET http://127.0.0.1:7700/health` returns 200
5. Jellysearch wiring works:
   - `GET http://127.0.0.1/jellyfin/Items?SearchTerm=test&Limit=1` returns 200
6. Jellio manifest resolves:
   - `curl -fsSL "$JELLIO_STREMIO_MANIFEST_URL_TAILNET" | python3 -c 'import json,sys; j=json.load(sys.stdin); assert \"resources\" in j'`
   - `curl -fsSL "$JELLIO_STREMIO_MANIFEST_URL_LAN" | python3 -c 'import json,sys; j=json.load(sys.stdin); assert \"resources\" in j'`
7. Tailnet exposure asserted:
   - `tailscale status --json` shows backend `Running|Starting`
   - `tailscale serve status` contains the configured target (`http://127.0.0.1:80`)

## TDD Approach (Per Component)

### Streamyfin plugin

- Failing state:
  - `/jellyfin/Plugins` does not list Streamyfin plugin id
  - Streamyfin config still contains placeholder `seerrServerUrl.value`
- Tests:
  - HTTP: `GET /jellyfin/Plugins`
  - HTTP: `GET /jellyfin/Plugins/1e9e5d38-6e67-4615-8719-e98a5c34f004/Configuration` and assert `seerrServerUrl.value` matches expected
- Fix:
  - plugin zip install + restart Jellyfin
  - config patch via plugin configuration API
- Regression coverage:
  - Idempotence test: re-run scripts and assert no changes + no restarts required

### Jellysearch

- Failing state:
  - Meilisearch not healthy
  - Jellyfin search requests are not diverted (still slow / wrong backend)
- Tests:
  - HTTP: `GET http://127.0.0.1:7700/health`
  - HTTP: `GET /jellyfin/Items?SearchTerm=...` returns 200
- Fix:
  - bring up containers
  - ensure Caddy query matcher route exists for Jellyfin searchTerm
- Regression coverage:
  - Re-render template tests ensuring Caddyfile includes matcher and upstream `jellysearch:5000`

### Jellio+ (Stremio addon)

- Failing state:
  - `/jellyfin/jellio/configure` not reachable
  - generated manifest URL returns non-200
  - streams return 401 due to token problems
- Tests:
  - HTTP: `GET /jellyfin/jellio/configure` returns 200 HTML
  - HTTP: `GET $JELLIO_STREMIO_MANIFEST_URL_TAILNET` returns JSON with `resources`
  - HTTP: `GET $JELLIO_STREMIO_MANIFEST_URL_LAN` returns JSON with `resources`
  - HTTP: `GET /jellyfin/jellio/${JELLIO_CONFIG_B64}/catalog/movie/${MOVIES_LIBRARY_GUID}/skip=0.json` returns 200 (basic browse, using either base)
- Fix:
  - plugin install + config patch
  - ensure stremio user token generation and persistence
- Regression coverage:
  - On rerun, token generation step must not rotate unless explicitly forced

## Definition of Done

System is complete when, after a fresh `vmctl apply` and on subsequent reruns:

- Streamyfin plugin is installed and available in Jellyfin
- Streamyfin plugin config is set (no placeholder values; Seerr URL is correct)
- Jellysearch + Meilisearch containers are running and Jellyfin search requests are routed to Jellysearch when `searchTerm` is present
- Jellio+ plugin is installed, configured with selected libraries and correct `PublicBaseUrl`
- Stable Stremio manifest URLs exist for:
  - LAN (`JELLIO_STREMIO_MANIFEST_URL_LAN`)
  - Tailnet (`JELLIO_STREMIO_MANIFEST_URL_TAILNET`)
  - optional Cloudflare (`JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE`)
- All functionality works with:
  - LAN base `http://media-stack.${domain}`
  - Tailnet base `https://${TAILSCALE_DNS_NAME}`
- Cloudflare is not required for default operation (Cloudflare tunnel is optional and disabled unless explicitly enabled)
- Cloudflare tunnel support exists but is disabled unless explicitly enabled by config
- Provisioning is zero-touch and idempotent

## Tradeoffs: Tailscale vs LAN vs Optional Cloudflare

The Jellio+ README recommends Cloudflare Tunnel because it provides:

- publicly reachable HTTPS without opening router ports
- a stable hostname and valid certificates for clients like Stremio

This plan defaults to Tailscale because:

- `tailscale serve` provides trusted HTTPS on a stable tailnet DNS name (`.ts.net`) without Cloudflare
- the addon stays private to the tailnet (reduced attack surface)
- no third-party dependency or account is required beyond Tailscale itself (already part of this stack)

Tradeoffs:

- With Tailscale, every Stremio device must be on the tailnet (Tailscale client installed/logged in), including TVs/streaming boxes.
- Cloudflare Tunnel can serve devices without a tailnet client, but it makes the addon internet-accessible and adds Cloudflare as an operational dependency.

Chosen default (Tailscale) matches the stated constraint: “must NOT rely on Cloudflare (prefer Tailscale)” and keeps the addon private while still satisfying Stremio’s HTTPS expectations. The LAN HTTP manifest exists for convenience on trusted networks, and Cloudflare is available as an explicit opt-in escape hatch for TVs that require public HTTPS.
