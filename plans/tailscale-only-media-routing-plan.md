# Tailscale-Only Media Routing Plan (vmctl)

Date: 2026-04-24

## 1. Summary

Refactor vmctl’s media routing so it no longer provisions custom LAN DNS names (notably `*.home.arpa`) or mutates `/etc/hosts` to “make names work”. Instead:

- Use **Tailscale MagicDNS** for discovery (`media-stack`, `media-stack.<tailnet>.ts.net`).
- Use **Tailscale Serve** (and optional Funnel) for HTTPS exposure of the media stack UI/addon routes.
- Remove **LAN-specific Stremio manifest variants** (LAN / LAN IP / LAN short host), keeping only a **Tailscale-first** manifest and **Cloudflare optional** manifest.

This plan is a TDD-driven refactor: first add regression tests that fail under the current LAN/home.arpa behavior, then implement changes until tests pass, and finally update fixtures/docs.

Compatibility policy:

- This is a breaking-change refactor. Backward compatibility is not a requirement.
- Remove legacy LAN endpoints, env vars, manifest alias files, and template/script branches outright (do not keep shims, fallbacks, or “legacy but still present” outputs).

## 2. Current Problem

### 2.1 DNS Reality vs vmctl Assumptions

Observed behavior (confirmed by macOS resolver output):

- `http://media-stack` resolves because **Tailscale MagicDNS** answers it.
- `media-stack.<tailnet>.ts.net` resolves via Tailscale DNS.
- `media-stack.home.arpa` returns `NXDOMAIN` because there is no authoritative LAN DNS zone for `home.arpa`.
- `/etc/hosts` edits on the server do not propagate to other devices (Mac/iOS/TV).

Current vmctl/media stack behavior relies on LAN naming assumptions:

- `vmctl.toml` defaults `searchdomain = "${domain}"` with `domain = "home.arpa"`.
- `packs/templates/caddyfile.media.hbs` binds `media-stack.home.arpa` and contains alias routes for LAN manifests.
- `packs/scripts/bootstrap-media.sh` mutates `/etc/hosts` (unmarked) to force `media-stack` and `media-stack.home.arpa` resolution.
- `crates/cli/src/main.rs` mutates `/etc/hosts` (marked entries `# vmctl:<resource>`) during `vmctl apply`.
- Media index UI and env templates export multiple LAN manifest variants that are either broken or misleading.

Result: confusing, brittle routing; clients behave inconsistently depending on whether they have Tailscale, a search domain, or a local hosts entry.

### 2.2 Why Tailscale-Only is the Right Default Here

Tailscale already provides:

- a consistent hostname (`media-stack`) and tailnet FQDN for all tailnet clients
- a trustworthy HTTPS story via Tailscale Serve certificates (no custom PKI, no self-signed distribution)
- a single “discovery mechanism” that works across Mac/iOS/TV (if the TV runs Tailscale or you use public exposure)

Therefore, vmctl should stop pretending a LAN DNS zone exists, and stop trying to “fix” client DNS by editing server `/etc/hosts`.

## 3. Chosen Approach: Tailscale-Only Media Routing

### 3.1 Target Routing Model

Canonical endpoints:

- HTTP (MagicDNS, no redirects): `http://media-stack`
- Tailnet HTTPS (Tailscale Serve): `https://media-stack.<tailnet>.ts.net`
- Optional public HTTPS: Cloudflare tunnel (existing “Cloudflare optional” manifest) or Tailscale Funnel (already supported by `packs/scripts/bootstrap-ui-routing.sh`)

Key policy decisions:

- **No `home.arpa`** in generated artifacts for `media-stack`.
- **No LAN-IP manifests**.
- **No `/etc/hosts` mutation** in provisioning or bootstrap scripts.
- **Stremio integration is Tailscale-first** (tailnet HTTPS) with a Cloudflare optional alternative.

### 3.2 Request Flow After Refactor

1. Client resolves `media-stack` via MagicDNS (or uses the `.ts.net` FQDN).
2. Client loads `http://media-stack/` (Caddy UI index) or `https://media-stack.<tailnet>.ts.net/` (Tailscale Serve).
3. The UI index exposes:
   - `Stremio Manifest (Tailscale)` via a short alias URL file (served from `/srv/ui-index`)
   - `Stremio Manifest (Cloudflare optional)` when configured
4. Manifest and subsequent add-on requests hit Caddy on port 80 locally; tailnet HTTPS is handled by Tailscale Serve proxying to `http://127.0.0.1:80`.

## 4. Files to Update

### Config / docs

- `vmctl.toml`
- `README.md`

### Templates

- `packs/templates/caddyfile.media.hbs`
- `packs/templates/media-index.html.hbs`
- `packs/templates/media.env.hbs`

### Bootstrap scripts

- `packs/scripts/bootstrap-media.sh`
- `packs/scripts/bootstrap-jellio.sh`
- `packs/scripts/bootstrap-ui-routing.sh` (minor alignment; this already uses `tailscale serve/funnel`)
- `packs/scripts/bootstrap-validate-streaming-stack.sh`
- `packs/scripts/bootstrap-jellyfin-discovery.sh`
- `packs/scripts/bootstrap-kodi.sh`
- `packs/scripts/bootstrap-kodi-jellyfin.sh`

### Rust core

- `crates/planner/src/lib.rs` (remove “media_services requires searchdomain” invariant when routing is tailscale-only)
- `crates/cli/src/main.rs` (remove `/etc/hosts` provisioning behavior)
- `crates/provision/src/lib.rs` (defaults currently reference `*.home.arpa`)
- `crates/backend-terraform/src/lib.rs` (tests assert LAN-bearing outputs)
- `crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/*` (update generated fixtures)

## 5. Config Changes

### 5.1 vmctl: introduce a media routing mode

Add an explicit routing mode under media services exposure and make `tailscale_only` the recommended setting for `media-stack`.

Example `vmctl.toml` snippet:

```toml
[resources.media-stack.features.media_services.exposure]
routing = "tailscale_only"

# Keep tailnet HTTPS via tailscale serve (recommended).
tailscale_https_enabled = true
tailscale_https_target = "http://127.0.0.1:80"

# Public exposure should remain opt-in.
tailscale_funnel_enabled = false

# Cloudflare remains optional and explicit.
cloudflare_enabled = false
cloudflare_public_base_url = ""
```

### 5.2 Stop defaulting to home.arpa for media-stack

Current default: `vmctl.toml` sets `searchdomain = "${domain}"` with `domain = "home.arpa"`.

Refactor goal:

- For `media-stack` in `tailscale_only` mode, **do not require** or use `searchdomain`.
- Keep `searchdomain` available for non-media resources that still want it, but do not apply it blindly to media routing.

Implementation detail:

- Update `crates/planner/src/lib.rs` so `media_services` validation does not hard-require `searchdomain` when `routing == "tailscale_only"`.
- Ensure templates/scripts for media stack do not interpolate `resource.searchdomain` in this mode.

### 5.3 DRY: single-source URL derivation

Eliminate duplicated hostname variants by deriving all externally-consumed URLs from exactly two inputs:

- `magicdns_name`: `media-stack` (resource name)
- `tailscale_dns_name`: discovered at runtime from `tailscale status --json` (e.g. `media-stack.<tailnet>.ts.net`)

Rules:

- “Tailscale manifest” bases on `https://{tailscale_dns_name}`.
- “Cloudflare manifest” bases on `https://{cloudflare_public_base_url}` (only when enabled).
- No other host variants are generated (no `home.arpa`, no LAN IP, no “short host” manifest variant).

## 6. Template Changes

### 6.1 `packs/templates/caddyfile.media.hbs`: remove custom LAN host bindings and LAN manifest aliases

Current:

- server block binds `{{resource.name}}:80, {{resource.name}}.{{resource.searchdomain}}:80, :80`
- alias routes for `/jellio-lan/*`, `/jellio-lan-short/*`, `/jellio-lan-ip/*` exist

Target:

- Bind only `:80` (Caddy serves regardless of Host header).
- Remove LAN alias routes entirely.
- Keep tailnet and cloudflare alias routes (or rename to “tailscale” for clarity).
- Keep existing Tizen-specific workarounds (User-Agent routing) unchanged unless they depend on LAN variants.

Example Caddy snippet (target output):

```caddyfile
{
  auto_https off
}

:80 {
  handle_path /healthz {
    respond "ok" 200
  }

  header -Strict-Transport-Security

  log {
    output stdout
    format console
  }

  # Short alias routes to avoid Stremio manifest URL length limits.
  handle_path /jellio-tailscale/* {
    rewrite * /jellio/{$JELLIO_CONFIG_B64_TAILNET}{path}?{query}
    reverse_proxy jellio-shim:8098 {
      header_up Host {host}
      header_up X-Forwarded-Host {host}
      header_up X-Forwarded-Proto https
    }
  }

  handle_path /jellio-cloudflare/* {
    rewrite * /jellio/{$JELLIO_CONFIG_B64_CLOUDFLARE}{path}?{query}
    reverse_proxy {$JELLYFIN_INTERNAL_URL}
  }

  handle /jellio/* {
    reverse_proxy {$JELLYFIN_INTERNAL_URL}
  }

  # Keep existing /jf, /Items, /Videos proxying and Tizen UA behaviors.

  handle {
    root * /srv/ui-index
    file_server
  }
}
```

Notes:

- Standardize on a single tailscale-first alias surface (`/jellio-tailscale/*`) and remove the legacy LAN alias surfaces entirely.
- The `host *.*.ts.net` matchers can remain if they are still required for the Tizen rewrite behavior. They should not be used to generate `home.arpa` variants.

### 6.2 `packs/templates/media-index.html.hbs`: remove LAN manifest links, present Tailscale-first

Remove:

- Stremio Manifest (LAN)
- Stremio Manifest (LAN IP)
- Stremio Manifest (LAN Short Host)

Keep:

- Stremio Manifest (Tailnet) but rename to `Stremio Manifest (Tailscale)` in UI text.
- Stremio Manifest (Cloudflare optional)

Also update wiring:

- Delete `wire("...lan...")` calls.
- Add/keep `wire("...tailscale...")` and `wire("...cloudflare...")`.

Example UI section:

```html
<li>
  <a id="jellio-manifest-tailscale-link" href="#">Stremio Manifest (Tailscale)</a>
  <p>Primary manifest for private access via Tailscale MagicDNS + Tailscale Serve.</p>
</li>
<li>
  <a id="jellio-manifest-cloudflare-link" href="#">Stremio Manifest (Cloudflare optional)</a>
  <p>Only active when Cloudflare tunnel is explicitly configured.</p>
</li>
```

### 6.3 `packs/templates/media.env.hbs`: drop LAN env vars, keep tailscale/cloudflare only

Remove or deprecate:

- `VMCTL_SEARCHDOMAIN`
- `VMCTL_HOST_FQDN`
- `VMCTL_HTTP_BASE_URL_FQDN`
- `JELLIO_STREMIO_MANIFEST_URL_LAN`
- `JELLIO_STREMIO_MANIFEST_URL_LAN_IP`
- `JELLIO_STREMIO_MANIFEST_URL_LAN_SHORT`

Keep:

- `JELLIO_STREMIO_MANIFEST_URL_TAILNET` (or rename to `_TAILSCALE`)
- `JELLIO_STREMIO_MANIFEST_URL_CLOUDFLARE`
- `TAILSCALE_HTTPS_ENABLED`, `TAILSCALE_HTTPS_TARGET`, `TAILSCALE_FUNNEL_ENABLED`

Do not retain old keys “for compatibility”. Delete them from the template and remove all downstream references. Tests should assert that `home.arpa` does not appear in the rendered env file.

## 7. Script / Provisioning Changes

### 7.1 `packs/scripts/bootstrap-media.sh`: remove `/etc/hosts` mutation and FQDN computation

Delete `ensure_hostname_aliases()` and any calls to it.

Replace any logic that depends on:

- `VMCTL_HOST_FQDN`
- `VMCTL_SEARCHDOMAIN`
- `VMCTL_HTTP_BASE_URL_FQDN`

with:

- `VMCTL_HTTP_BASE_URL_SHORT=http://media-stack` (MagicDNS)
- runtime-discovered tailnet DNS name for HTTPS use cases (if needed)

If something must refer to the server locally, prefer:

- `http://127.0.0.1:<port>` inside the host
- container names (`http://jellyfin:8096`) inside Docker networks

### 7.2 `packs/scripts/bootstrap-jellio.sh`: generate only tailscale and cloudflare manifests

Remove creation of:

- LAN base manifests (`lan_base`, `lan_short_public_base`, `lan_ip_public_base`)
- their b64 selectors and env vars
- their alias files in `/opt/media/config/caddy/ui-index`

Keep:

- tailnet manifest (`tailnet_base` derived from Tailscale DNS name)
- cloudflare manifest (when enabled)

Update alias file outputs:

- Replace `jellio-manifest.lan*.url` and `jellio-manifest.tailnet.url` with a single `jellio-manifest.tailscale.url`.

Example desired alias files:

- `/srv/ui-index/jellio-manifest.tailscale.url`
- `/srv/ui-index/jellio-manifest.cloudflare.url`

### 7.3 `packs/scripts/bootstrap-validate-streaming-stack.sh`: validate only active surfaces

Remove checks that require:

- short/FQDN LAN hostnames
- LAN IP manifest files

Keep checks for:

- local healthz: `http://127.0.0.1/healthz`
- UI index: `http://127.0.0.1/`
- tailscale serve status when enabled
- presence of `jellio-manifest.tailscale.url` and its URL being non-empty when Tailscale is authenticated
- cloudflare URL file being empty unless cloudflare is configured

Example post-provision validation checks (shell-level, suitable to embed in the script):

```bash
curl -fsS http://127.0.0.1/healthz | rg -q '^ok$'
curl -fsS http://127.0.0.1/ >/dev/null

# Ensure legacy LAN manifests are not present anymore.
test ! -f /opt/media/config/caddy/ui-index/jellio-manifest.lan.url
test ! -f /opt/media/config/caddy/ui-index/jellio-manifest.lan-ip.url
test ! -f /opt/media/config/caddy/ui-index/jellio-manifest.lan-short.url

# Validate the tailscale manifest alias file exists and is non-empty when Tailscale is authenticated.
test -f /opt/media/config/caddy/ui-index/jellio-manifest.tailscale.url
tailscale status --json >/dev/null 2>&1 && test -s /opt/media/config/caddy/ui-index/jellio-manifest.tailscale.url || true

# Generated config must not refer to home.arpa.
rg -n "home\\.arpa" /opt/media/config/caddy/Caddyfile && exit 1 || true
```

### 7.4 `packs/scripts/bootstrap-ui-routing.sh`: align names and required envs

This script already uses:

- `tailscale serve --yes --bg "$TAILSCALE_HTTPS_TARGET"`
- optional `tailscale funnel`

Refactor scope:

- Ensure it never assumes `home.arpa`.
- Ensure `TAILSCALE_FUNNEL_ENABLED` is configurable via `media.env` (it currently exists, but might be forced/preserved).
- Optionally add logging that prints the discovered Tailscale DNS name and the configured `TAILSCALE_HTTPS_TARGET`.

### 7.5 `packs/scripts/bootstrap-jellyfin-discovery.sh`: remove home.arpa fallback

Currently uses:

`Environment=JELLYFIN_DISCOVERY_ADDRESS=${JELLYFIN_URL:-http://${VMCTL_HOST_FQDN:-${VMCTL_HOST_SHORT:-media-stack}.${VMCTL_SEARCHDOMAIN:-home.arpa}}:8096}`

Replace fallback with:

- `http://media-stack:8096` (MagicDNS) or
- `http://127.0.0.1:8096` if the discovery shim is only intended for local host diagnostics

Given this plan is Tailscale-first, the recommended default is:

- `JELLYFIN_DISCOVERY_ADDRESS=${JELLYFIN_URL:-http://media-stack:8096}`

### 7.6 Kodi scripts: remove `home.arpa`/`.lan` assumptions

Update:

- `packs/scripts/bootstrap-kodi.sh`
- `packs/scripts/bootstrap-kodi-jellyfin.sh`

to reference `http://media-stack` / tailnet endpoints as appropriate, and remove hardcoded `.home.arpa` / `.lan` values.

### 7.7 `crates/cli/src/main.rs`: stop mutating `/etc/hosts`

Remove `ensure_provision_hosts_resolve()` from the apply flow, and remove its helpers:

- `ensure_provision_hosts_resolve`
- `upsert_hosts_file`
- `upsert_hosts_content`

This ensures vmctl does not attempt to “fix” DNS on the provisioning host.

If there is a legitimate need for “provision-time reachability”, replace it with:

- direct IP discovery and use of IP for provisioning SSH/cloud-init steps
- or require users to fix DNS at the right layer (Tailscale MagicDNS, router DNS, etc.)

### 7.8 `crates/planner/src/lib.rs`: make `searchdomain` optional for tailscale-only media services

Current behavior rejects media services without `searchdomain`.

Update validation:

- If `features.media_services.exposure.routing == "tailscale_only"`, do not require `searchdomain`.
- Ensure `resource.searchdomain` can remain set for other resources, but media templates and scripts do not depend on it.

### 7.9 `crates/provision/src/lib.rs`: stop defaulting provisioning hostnames to `*.home.arpa` for media-stack

Refactor defaults so that for media resources, the recommended or default provisioning hostname is:

- `media-stack` (MagicDNS) when Tailscale is enabled
- otherwise leave it unset and require explicit host/IP for provisioning

## 8. Stremio Manifest Changes

### 8.1 Eliminate LAN variants end-to-end

Remove from:

- UI index: all LAN links and wiring
- env template: all LAN env vars
- bootstrap-jellio: all LAN manifest generation and URL alias files
- caddyfile: all LAN alias routes
- tests/fixtures: references to LAN manifests

### 8.2 Keep and standardize the Tailscale manifest

Define:

- `Stremio Manifest (Tailscale)` = manifest URL based on `https://<tailscale_dns_name>`
- expose via a short alias path to avoid Stremio URL length limits

Example: `https://media-stack.<tailnet>.ts.net/jellio-tailscale/manifest.json`

### 8.3 Keep Cloudflare optional manifest

Keep “Cloudflare optional” behavior:

- link is present but disabled/greyed when not configured
- alias file exists but empty when not configured

## 9. /etc/hosts Cleanup Strategy

Goal: remove previous vmctl-managed hosts entries safely without deleting user-owned lines.

### 9.1 Rust-managed entries (`# vmctl:<resource>`)

`crates/cli/src/main.rs` writes `/etc/hosts` entries with a marker like:

`# vmctl:media-stack`

Cleanup strategy:

- remove all lines containing `# vmctl:`
- or remove only for `media-stack` if requested

### 9.2 Legacy bootstrap-media entries (unmarked)

`packs/scripts/bootstrap-media.sh` historically injected entries without a marker, by rewriting lines matching:

- `127.0.1.1 ... media-stack ...`
- or inserting `<primary_ip> media-stack media-stack.home.arpa`

Safe cleanup approach:

1. Implement a cleanup helper that only removes entries where:
   - the hostname list contains `media-stack.home.arpa` (explicitly removed by this refactor)
   - and the line does not contain `localhost`
2. Run in “dry-run” mode by default, printing a unified diff.
3. Apply changes only when invoked with a `--apply` flag.

Example Python helper (implement as a vmctl pack script or a `vmctl` subcommand):

```python
import re
from pathlib import Path

HOSTS = Path("/etc/hosts")
content = HOSTS.read_text(encoding="utf-8").splitlines()

def should_drop(line: str) -> bool:
    s = line.strip()
    if not s or s.startswith("#"):
        return False
    if "# vmctl:" in s:
        return True
    # Legacy: drop removed home.arpa aliases for media-stack only.
    if re.search(r"\\bmedia-stack\\.home\\.arpa\\b", s):
        return True
    return False

new_lines = [ln for ln in content if not should_drop(ln)]
HOSTS.write_text("\n".join(new_lines).rstrip() + "\n", encoding="utf-8")
```

Important guardrail:

- Do not delete short-host-only lines like `192.168.x.y media-stack` without a marker; those might be user-managed.

## 10. TDD Approach

### 10.1 Add failing tests first (capture “undesired LAN output”)

Update/add tests to assert the new invariants:

- Rendered media caddyfile does not contain `home.arpa`
- Rendered media caddyfile does not contain `/jellio-lan`, `/jellio-lan-ip`, `/jellio-lan-short`
- Rendered media index does not include “Stremio Manifest (LAN*)” entries
- Rendered media env does not contain `VMCTL_SEARCHDOMAIN=home.arpa` or `VMCTL_HOST_FQDN=...home.arpa`
- `vmctl apply` no longer includes `/etc/hosts` update logic

Concrete test targets already exist:

- `crates/backend-terraform/src/lib.rs` currently asserts LAN-bearing outputs (these should be inverted/updated)
- `crates/planner/src/lib.rs` currently rejects missing `searchdomain` for media services (should accept in tailscale-only mode)

Example (Rust test intent):

```rust
assert!(!caddy.contains("home.arpa"));
assert!(!caddy.contains("/jellio-lan/"));
assert!(!index.contains("Stremio Manifest (LAN"));
assert!(!env.contains("VMCTL_SEARCHDOMAIN=home.arpa"));
```

### 10.2 Implement refactor in small slices

1. Planner validation: make `searchdomain` optional for tailscale-only routing.
2. Templates: remove home.arpa bindings and LAN manifest routes/links/env vars.
3. Scripts: remove LAN manifest generation and `/etc/hosts` edits.
4. CLI: remove `/etc/hosts` provisioning behavior.
5. Fixtures and docs: update backend terraform fixtures and README.

After each slice:

- run unit tests
- run `vmctl backend render` (or equivalent) and check generated artifacts for banned strings

### 10.3 Add regression coverage

Add a “ban list” test suite to prevent reintroducing LAN assumptions, for example:

- `home.arpa`
- `.lan`
- `/jellio-lan`
- `JELLIO_STREMIO_MANIFEST_URL_LAN`
- `VMCTL_HOST_FQDN`

## 11. Task List

### Investigation (short, local)

1. Identify all references to `home.arpa`, `.lan`, `/etc/hosts`, and `JELLIO_STREMIO_MANIFEST_URL_LAN*` in `packs/` and `crates/`.
2. Confirm which generated artifacts are consumed by Stremio (UI index URL files, env vars, Caddy routes).

### Reproduction (current undesired behavior)

1. Run render/apply in a fixture workspace and confirm:
   - Caddyfile binds `media-stack.home.arpa`
   - media index shows LAN manifests
   - scripts generate `.lan.url` files
   - CLI writes `/etc/hosts` markers

### Remediation (implementation order)

1. `crates/planner/src/lib.rs`: add `routing = "tailscale_only"` path and relax `searchdomain` requirement.
2. `packs/templates/caddyfile.media.hbs`: delete LAN alias routes and remove `{{resource.name}}.{{resource.searchdomain}}` binding.
3. `packs/templates/media-index.html.hbs`: delete LAN links and wiring; rename Tailnet to Tailscale.
4. `packs/templates/media.env.hbs`: remove LAN manifest vars and home.arpa-derived vars; keep tailscale/cloudflare.
5. `packs/scripts/bootstrap-jellio.sh`: generate only tailscale + cloudflare; update alias files accordingly.
6. `packs/scripts/bootstrap-media.sh`: remove `/etc/hosts` mutation and any reliance on `VMCTL_HOST_FQDN`.
7. `packs/scripts/bootstrap-jellyfin-discovery.sh` + Kodi scripts: remove home.arpa fallback.
8. `crates/cli/src/main.rs`: remove `/etc/hosts` provisioning functions and call site.
9. `crates/provision/src/lib.rs`: adjust default provisioning host behavior away from `*.home.arpa`.
10. Update `crates/backend-terraform/src/lib.rs` tests and `crates/backend-terraform/tests/fixtures/example-workspace/...` fixtures.
11. Update `README.md` sections that describe `home.arpa`-based derivation.

### Validation

1. `cargo test` for affected crates.
2. Render media-stack templates and assert:
   - no `home.arpa` strings
   - no LAN manifest routes/URLs
3. On a tailnet client:
   - `http://media-stack/healthz` returns `ok`
   - `https://media-stack.<tailnet>.ts.net/` works when `tailscale serve` is enabled
4. In Stremio:
   - addon can be added via the Tailscale manifest URL
   - Cloudflare manifest is present only when configured

### Regression Prevention

1. Add/keep tests that explicitly fail if banned LAN strings reappear.
2. Keep fixtures updated to reflect tailscale-only outputs.

## 12. Definition of Done

### System Outputs

- `vmctl apply` no longer provisions custom LAN domains for `media-stack`.
- No generated config references `media-stack.home.arpa` or `home.arpa`.
- No generated config relies on LAN IP manifest variants.
- No provisioning step modifies `/etc/hosts` (neither Rust CLI nor media bootstrap scripts).
- Generated Caddy config is simpler and contains only active routing paths.

### Stremio Manifests

- Removed manifests are gone from UI/config/routes:
  - Stremio Manifest (LAN)
  - Stremio Manifest (LAN IP)
  - Stremio Manifest (LAN Short Host)
- Available manifests:
  - Stremio Manifest (Tailscale)
  - Stremio Manifest (Cloudflare optional)

### Tailscale Routing

- MagicDNS access remains working: `http://media-stack`
- Tailscale FQDN remains supported: `media-stack.<tailnet>.ts.net`
- Tailscale Serve remains the default HTTPS exposure mechanism (Funnel remains opt-in)

### Tests

- Tests prevent LAN-domain regressions (home.arpa, LAN manifests, /etc/hosts mutation).
