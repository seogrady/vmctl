# vmctl.toml Image Configuration Plan

Date: 2026-04-18

## Objective

Add first-class image/template management to `vmctl.toml` so `vmctl up` can provision Proxmox VMs and LXCs without requiring operators to manually pre-download cloud images, ISO/import images, or LXC templates.

The immediate failure this addresses is:

```text
unable to create CT 101 - volume 'local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst' does not exist
```

Generated Terraform/OpenTofu files must not be hand-patched. The source config and generator/backend must model required images explicitly.

## Product Requirements

- `vmctl.toml` supports declaring reusable images by logical name.
- VM and LXC definitions reference logical image names instead of hard-coded Proxmox volume strings.
- `vmctl up` ensures all required images exist before resources that consume them are created.
- Image handling supports both:
  - Proxmox-managed LXC appliance templates via `pveam`.
  - Provider-managed URL downloads through Terraform/OpenTofu where supported.
- Image state is deterministic and cache-aware.
- Missing images produce actionable diagnostics before partially creating VMs/LXCs.
- Generated `main.tf.json` contains resources/dependencies derived from source config.
- Deprecated provider fields such as `network_device.enabled` are removed at the generator/source-template layer.

## Current Problem

The generated LXC resource references a concrete template:

```text
local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst
```

but the workflow does not ensure that template is present on the target Proxmox storage before creating the container.

Terraform/OpenTofu core does not natively manage Proxmox image availability. The `bpg/proxmox` provider has `proxmox_virtual_environment_download_file` for provider-managed downloads, and Proxmox itself has `pveam download <storage> <template>` for appliance templates. `vmctl` should choose the correct path based on source config.

## Proposed vmctl.toml Schema

Add a top-level `[images]` table keyed by logical image name.

### LXC Template From Proxmox Appliance Catalog

```toml
[images.debian_12_lxc]
kind = "lxc"
source = "pveam"
node = "mini"
storage = "local"
content_type = "vztmpl"
template = "debian-12-standard_12.7-1_amd64.tar.zst"
file_name = "debian-12-standard_12.7-1_amd64.tar.zst"
```

The rendered Proxmox volume string is:

```text
local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst
```

### LXC Template From URL

```toml
[images.debian_12_lxc_url]
kind = "lxc"
source = "url"
node = "mini"
storage = "local"
content_type = "vztmpl"
file_name = "debian-12-rootfs.tar.zst"
url = "https://example.invalid/images/debian-12-rootfs.tar.zst"
checksum_algorithm = "sha256"
checksum = "..."
```

### VM Cloud Image

```toml
[images.debian_12_cloud]
kind = "vm"
source = "url"
node = "mini"
storage = "local"
content_type = "import"
file_name = "debian-12-generic-amd64.qcow2"
url = "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-amd64.qcow2"
checksum_algorithm = "sha512"
checksum = "..."
```

### Consumer References

LXC source config should reference the logical image:

```toml
[containers.tailscale_gateway]
id = 101
image = "debian_12_lxc"
```

VM source config should do the same:

```toml
[vms.media_stack]
id = 201
image = "debian_12_cloud"
```

Avoid embedding provider-specific volume strings in VM/LXC definitions unless an explicit escape hatch is needed.

## Source Model

Introduce typed image model structs/enums:

```rust
enum ImageKind {
    Vm,
    Lxc,
}

enum ImageSource {
    Pveam,
    Url,
    Existing,
}

struct ImageConfig {
    name: String,
    kind: ImageKind,
    source: ImageSource,
    node: String,
    storage: String,
    content_type: String,
    file_name: Option<String>,
    template: Option<String>,
    url: Option<String>,
    checksum_algorithm: Option<String>,
    checksum: Option<String>,
}
```

Resolved images should expose:

```rust
struct ResolvedImage {
    name: String,
    kind: ImageKind,
    source: ImageSource,
    node: String,
    storage: String,
    content_type: String,
    file_name: String,
    volume_id: String,
}
```

Example `volume_id`:

```text
local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst
```

## Backend Terraform/OpenTofu Rendering

### URL Images

For `source = "url"`, render a provider-managed resource when using `bpg/proxmox`:

```hcl
resource "proxmox_virtual_environment_download_file" "image_debian_12_lxc_url" {
  node_name    = "mini"
  datastore_id = "local"
  content_type = "vztmpl"
  file_name    = "debian-12-rootfs.tar.zst"
  url          = "https://example.invalid/images/debian-12-rootfs.tar.zst"

  checksum_algorithm = "sha256"
  checksum           = "..."
}
```

Consumers should depend on the resource and consume its ID or generated volume ID according to provider behavior.

### pveam Images

For `source = "pveam"`, prefer a `vmctl` preflight/cache step instead of Terraform provisioners:

```bash
pveam update
pveam list local
pveam download local debian-12-standard_12.7-1_amd64.tar.zst
```

The Terraform LXC resource then references the resolved `volume_id`.

Do not use `local-exec` as the default implementation. OpenTofu cannot model provisioner side effects cleanly, and failures are harder to make idempotent.

### Existing Images

For `source = "existing"`, perform validation only:

```toml
[images.debian_12_lxc_existing]
kind = "lxc"
source = "existing"
node = "mini"
storage = "local"
content_type = "vztmpl"
file_name = "debian-12-standard_12.7-1_amd64.tar.zst"
```

If missing, fail before apply with an actionable message.

## vmctl Commands

Add image-oriented subcommands:

```bash
vmctl images list
vmctl images plan
vmctl images ensure
vmctl images ensure debian_12_lxc
vmctl images doctor
```

### `vmctl images list`

Print resolved image catalog:

- logical name
- kind
- source
- node
- storage
- content type
- volume ID
- present/missing/unknown

### `vmctl images plan`

Print planned actions without side effects:

- validate config
- check presence
- update pveam catalog if needed
- download missing `pveam` templates
- render Terraform download resources for URL images

### `vmctl images ensure`

Ensure required images are available:

- For `pveam`: query Proxmox node and download missing templates.
- For `url`: either run the generated image-only Terraform target or let normal `up` apply handle provider-managed downloads.
- For `existing`: validate presence and fail if missing.

### `vmctl up`

Default flow:

1. Load and validate `vmctl.toml`.
2. Resolve image references from VM/LXC definitions.
3. Run image preflight.
4. Ensure `pveam` and `existing` images are present.
5. Render Terraform/OpenTofu from source config.
6. Apply image download resources before dependent resources, or rely on generated `depends_on`.
7. Apply VM/LXC resources.

Add a flag for explicit operator control:

```bash
vmctl up --no-image-ensure
```

This should be reserved for CI or advanced workflows and should print a warning if required images are not known-present.

## Validation Rules

- Every VM/LXC `image` reference must resolve to an `[images.<name>]` entry.
- `kind = "lxc"` images can be used only by containers.
- `kind = "vm"` images can be used only by VMs.
- `source = "pveam"` requires:
  - `kind = "lxc"`
  - `storage`
  - `template`
  - `content_type = "vztmpl"`
- `source = "url"` requires:
  - `url`
  - `storage`
  - `node`
  - `content_type`
  - `file_name`
- `source = "existing"` requires:
  - `storage`
  - `content_type`
  - `file_name`
- If a checksum algorithm is supplied, checksum must also be supplied.
- If checksum is supplied, checksum algorithm must also be supplied.
- Generated provider resources must not contain deprecated fields such as `network_device.enabled`.

## Caching Semantics

The Proxmox storage is the cache of record.

An image is considered present when the node/storage reports a matching file under the expected content type and filename.

For `pveam` images:

- Run `pveam list <storage>` or equivalent Proxmox API query.
- Download only when missing.
- Do not redownload by default.

For `url` images:

- Terraform/OpenTofu provider state tracks the download resource.
- Checksums should be required for pinned production images.
- `overwrite` behavior should be explicit in source config if the provider supports it.

## Diagnostics

Missing image errors should happen before container/VM creation starts.

Example:

```text
Missing LXC template for containers.tailscale_gateway

Image: debian_12_lxc
Expected: local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst
Node: mini
Storage: local

vmctl can download it with:
  vmctl images ensure debian_12_lxc

Or configure a different image in vmctl.toml:
  [containers.tailscale_gateway]
  image = "..."
```

## Testing Strategy

### Unit Tests

- Parse `[images]` entries for all source types.
- Reject invalid image definitions.
- Resolve VM/LXC image references.
- Generate correct Proxmox volume IDs.
- Ensure `pveam` images are restricted to LXC templates.
- Ensure deprecated `network_device.enabled` is not emitted.

### Backend Rendering Tests

- `source = "url"` renders `proxmox_virtual_environment_download_file`.
- LXC consumers depend on or reference the rendered image resource.
- VM consumers use provider-supported image import fields.
- `source = "existing"` emits no download resource but emits validation metadata.
- Generated JSON snapshot excludes deprecated provider arguments.

### CLI Tests

- `vmctl images plan` reports missing/present images.
- `vmctl images ensure --dry-run` prints `pveam download` actions without running them.
- `vmctl up` invokes image preflight before Terraform/OpenTofu apply.
- `vmctl up --no-image-ensure` skips preflight but warns.

### Integration Tests

Use a Proxmox test node or mocked Proxmox API/SSH boundary:

- Missing `pveam` LXC template is downloaded before LXC creation.
- Existing template is not downloaded again.
- Missing `existing` image fails before Terraform/OpenTofu creates disks or containers.
- URL image resource is applied before VM/LXC consumers.

## Migration Plan

1. Add `[images]` support while preserving current direct template fields.
2. Emit deprecation warning when VM/LXC definitions use direct template volume strings.
3. Update examples to use logical image references.
4. Migrate generated Terraform backend to consume resolved images.
5. Remove deprecated direct template fields in a later breaking release.

Compatibility bridge:

```toml
[containers.tailscale_gateway]
ostemplate = "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst"
```

can be internally converted to:

```toml
[images.generated_tailscale_gateway]
kind = "lxc"
source = "existing"
storage = "local"
content_type = "vztmpl"
file_name = "debian-12-standard_12.7-1_amd64.tar.zst"
```

but new configs should use explicit `[images]`.

## Implementation Phases

### Phase 1: Source Schema

- Add image config structs.
- Parse `[images]`.
- Add validation and resolution.
- Add tests for valid/invalid configs.

Definition of done:

- `vmctl config validate` catches invalid image definitions.
- VM/LXC image references resolve to typed `ResolvedImage` values.

### Phase 2: CLI Image Commands

- Add `vmctl images list`.
- Add `vmctl images plan`.
- Add `vmctl images ensure --dry-run`.
- Add Proxmox image presence abstraction.

Definition of done:

- Operators can see which images are required and whether they are present before running `up`.

### Phase 3: pveam Ensure Path

- Implement `pveam` catalog update/list/download boundary.
- Make it node/storage aware.
- Add idempotent missing/present behavior.

Definition of done:

- Missing LXC templates can be downloaded by `vmctl images ensure`.
- Existing templates are skipped.

### Phase 4: Terraform Backend Rendering

- Render `proxmox_virtual_environment_download_file` for URL images.
- Add generated dependencies from consumers to image resources.
- Remove deprecated `network_device.enabled` from the source renderer.

Definition of done:

- Generated Terraform/OpenTofu can create required image resources before VM/LXC consumers.
- Provider deprecation warning for `network_device.enabled` is gone.

### Phase 5: `vmctl up` Integration

- Run image preflight before apply.
- Default to ensuring images.
- Add `--no-image-ensure`.
- Improve failure diagnostics.

Definition of done:

- `vmctl up` on a fresh Proxmox node downloads/caches required images or fails before partial VM/LXC creation.

### Phase 6: Docs and Examples

- Update `vmctl.toml` reference.
- Add LXC and VM image examples.
- Add troubleshooting section for missing Proxmox templates.
- Document required Proxmox permissions:
  - storage audit/allocation
  - template allocation
  - any node/system permissions required by `pveam` or provider downloads

Definition of done:

- A new operator can configure a Debian LXC and VM cloud image without manual Proxmox UI steps.

## Open Questions

- Does current `backend-terraform` already generate `proxmox_virtual_environment_download_file` anywhere?
- Does `vmctl` already have a Proxmox SSH/API command boundary suitable for `pveam`?
- Should `pveam` operations use SSH to the node, the Proxmox API, or provider-managed resources where possible?
- Should `url` image checksums be mandatory for non-local environments?
- How should multi-node clusters cache images: per-node, shared storage, or both?
- Should image ensure run automatically for all images or only images referenced by selected targets?

## Recommended Decision

Implement image support as a `vmctl` source-level feature with backend-specific execution:

- Use provider-managed `proxmox_virtual_environment_download_file` for URL/import images.
- Use `vmctl images ensure` with `pveam` for Proxmox appliance catalog LXC templates.
- Treat direct Proxmox volume strings in VM/LXC definitions as legacy escape hatches.
- Fail before Terraform/OpenTofu apply when required images are missing and cannot be ensured.
