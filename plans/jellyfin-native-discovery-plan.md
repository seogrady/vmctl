# Jellyfin Native Discovery Migration Plan

Date: 2026-04-23

## Objective

Restore reliable Streamyfin local server discovery (`Find local servers`) by running Jellyfin with native LAN UDP behavior.

Target outcome:

- Streamyfin discovery via UDP broadcast works on LAN
- Existing media stack remains `vmctl apply` managed and idempotent
- No manual post-provisioning steps

## Root Cause

Streamyfin discovery sends UDP broadcast to `255.255.255.255:7359` with payload `Who is JellyfinServer?`.
In the current Docker bridge model, Jellyfin responds to direct unicast on `7359`, but broadcast discovery is not reliably bridged end-to-end for clients on the LAN.

## Recommended Approach (Primary)

Use **Docker host networking** for Jellyfin.

Why:

- Keeps Jellyfin containerized (minimal operational change)
- Preserves existing config/media mounts
- Provides native host network stack behavior for UDP discovery
- Lowest migration risk vs full non-containerized install

## Alternative Approach (Fallback)

Run **non-containerized Jellyfin** as a systemd service on the VM.

Use only if host networking container path is blocked by image/runtime constraints or operational policy.

## Architecture Changes

Current:

- Jellyfin in Docker bridge network with explicit port publishing (`8096/tcp`, `7359/udp`)

Target (primary):

- Jellyfin container uses `network_mode: host`
- Remove per-container `ports` for Jellyfin
- Other services remain on bridge networking

Notes:

- `jellysearch` still reaches Jellyfin via LAN URL (`http://media-stack:8096`) or `http://127.0.0.1:8096` from host context.
- Caddy can continue reverse proxying to `127.0.0.1:8096` on host.

## vmctl Implementation Plan

### 1) Extend service schema for networking mode

Files:

- `crates/packs/src/lib.rs`
- `packs/templates/docker-compose.media.hbs`

Changes:

- Add optional field to service pack model:
  - `settings.network_mode` (string)
- Render in compose template when set:
  - `network_mode: "{{settings.network_mode}}"`
- Validation rule: if `network_mode == "host"`, do not render `ports` for that service.

### 2) Add Jellyfin host-network variant in service pack

File:

- `packs/services/jellyfin.toml`

Changes:

- Set:
  - `[settings]`
  - `network_mode = "host"`
- Remove published ports for Jellyfin (host mode ignores them).
- Keep volume mounts unchanged.

### 3) Update bootstrap scripts to use stable Jellyfin URL source

Files:

- `packs/scripts/bootstrap-jellyfin.sh`
- `packs/scripts/bootstrap-streamyfin.sh`
- `packs/scripts/bootstrap-jellio.sh`
- `packs/scripts/bootstrap-jellysearch.sh`
- `packs/scripts/bootstrap-validate-streaming-stack.sh`

Changes:

- Standardize internal endpoint resolution:
  - Prefer env `JELLYFIN_INTERNAL_URL`
  - Default `http://127.0.0.1:8096`
- Avoid hard assumptions about Docker service DNS (`jellyfin:8096`) for components that may run outside same network namespace.

### 4) Ensure Jellysearch connectivity remains valid

Files:

- `packs/services/jellysearch.toml`
- `packs/templates/media.env.hbs`

Changes:

- Set `JELLYFIN_URL` for jellysearch to host-reachable Jellyfin URL in this topology:
  - `http://media-stack:8096` (preferred in VM LAN context)
  - fallback `http://127.0.0.1:8096` if service shares host network in future

### 5) Fixture and test updates

Files:

- `crates/backend-terraform/tests/fixtures/example-workspace/resources/media-stack/docker-compose.media`
- Tests covering rendered compose equality / fixture sync

Changes:

- Reflect host-network jellyfin block in fixture output.
- Add explicit assertion that Jellyfin renders `network_mode: "host"` and no `ports`.

## Optional Fallback Plan (Non-containerized Jellyfin)

If host networking is not acceptable:

### A) Disable Jellyfin Docker service

- Remove `jellyfin` from `media_services.services`.

### B) Add native Jellyfin install script

New script:

- `packs/scripts/bootstrap-jellyfin-native.sh`

Responsibilities:

- Install Jellyfin package repo + package
- Configure data/config paths to existing `/opt/media/config/jellyfin`
- Systemd enable/start/restart

### C) Rewire dependents

- Caddy upstream to `127.0.0.1:8096`
- Streamyfin/Jellio/Jellysearch bootstrap URLs unchanged (`127.0.0.1`/`media-stack`)

## Rollout Steps

1. Implement schema + template support for `settings.network_mode`.
2. Switch Jellyfin service pack to host networking.
3. Update scripts/env wiring for topology-neutral Jellyfin URL.
4. Update fixtures and unit tests.
5. Run:
   - `cargo test`
   - `cargo run -q -p vmctl -- apply`
6. Validate discovery and playback.

## Validation Checklist

Functional:

- From LAN client, Streamyfin `Find local servers` lists Jellyfin.
- Manual login succeeds with Jellyfin user credentials.
- Stream playback and transcoding work.

Network:

- VM host listening on UDP `7359` and TCP `8096`.
- Broadcast probe from another LAN host receives Jellyfin response.

Integration:

- Streamyfin plugin still patched/configured.
- Jellysearch queries return results.
- Jellio Stremio manifest endpoints still return `200`.

Idempotency:

- Second `vmctl apply` performs no destructive churn and remains green.

## TDD Matrix

### Failing state

- Discovery returns zero servers on LAN.

### Tests

- Unit: compose render includes `network_mode: host` for Jellyfin and omits `ports`.
- Integration script test: bootstrap scripts honor `JELLYFIN_INTERNAL_URL`.
- Runtime check script: UDP discovery probe receives reply from LAN client.

### Fix

- Apply host-network migration changes above.

### Verify

- Repeat discovery check and `vmctl apply`.

### Regression coverage

- Keep fixture and render tests to prevent accidental reversion to bridge-only behavior.

## Risks and Mitigations

- Port conflicts on host (`8096`, `7359`):
  - Mitigation: preflight check before compose up.
- Tight coupling to host namespace:
  - Mitigation: keep fallback native install path documented.
- Service-to-service DNS assumptions:
  - Mitigation: enforce env-driven Jellyfin URL in scripts and service env.

## Definition of Done

- Streamyfin auto-discovery works on LAN without manual URL entry.
- `vmctl apply` fully provisions and configures stack automatically.
- Tests and fixtures pass.
- No Cloudflare dependency required for discovery behavior.
