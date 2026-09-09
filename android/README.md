# Android source boundary

Droidloom uses a thin AOSP product. The full upstream checkout and generated
images are not stored in this repository.

`manifest/source-lock.json` pins ARM64 base inputs;
`manifest/source-lock-x86_64.json` pins x86_64 inputs. The sparse AOSP project
closure is recorded separately in `manifest/m2-sparse-source-lock.json`.

The [Rust package workflow](../packaging/arch/README.md) prepares the pinned
upstream base partitions, materializes required sources, applies Droidloom patches,
builds modified platform components and the vendor image, and assembles packages.
Mesa is prepared from its pinned upstream release. It does not rebuild all of AOSP.

Cuttlefish vendor, ODM, boot and kernel images are excluded: their virtual-device
graphics and KMS assumptions do not match Droidloom. The host retains its kernel.

The SurfaceFlinger task path retains one headless bootstrap display for Android
scheduling and layer-release accounting. Patch
`surfaceflinger/0003-droidloom-headless-bootstrap.patch` can suppress that
display's duplicate GPU composition with
`persist.vendor.droidloom.elide_bootstrap=true` (enabled by default by patch 0007). It forwards the
merged completion fences from actual task rendering through the existing HWC
path, including across idle transitions. Only the explicit bootstrap display
is selected; capture outputs keep rendering and task outputs retain their
independent composition policy. Fence merge/export errors
fall back to ordinary composition. The property is read each frame, allowing
same-process performance comparisons and rollback without restarting Android.

`surfaceflinger/0004-droidloom-opaque-content.patch` carries each task frame's
opaque-coverage result through the private render-surface protocol. It uses
CompositionEngine's existing `undefinedRegion` after composition, including
its transform, clipping and layer-alpha rules. Composer forwards the result
using the host protocol's existing `OPAQUE` content-state bit; the presenter
sets or clears the ordinary Wayland opaque region on the matching commit.
The ARGB buffer format and pixel contents are unchanged. Package IDs do not
determine opacity. Deploy the patched SurfaceFlinger with the matching Composer
service, Rust dependency closure and presenter; older private requests remain
conservatively non-opaque.

`surfaceflinger/0007-droidloom-wayland-layers.patch` exports supported task
layer lists through synchronized Wayland subsurfaces. SurfaceFlinger still
resolves transactions, visibility, output geometry and input; it skips the task
GPU draw when the host accepts the complete scene. Original buffer/frame identities
remain protected until the host confirms actual read completion. Droidloom uses
asynchronous Android release callbacks even on source branches where their
upstream feature flag defaults off. Unsupported scenes retain the task renderer.
`surfaceflinger/0008-droidloom-release-fences.patch` forwards and merges real
compositor release fences, defers only callbacks whose fences are unavailable,
and preserves first-frame commit metadata. It avoids blocking SurfaceFlinger's
shared callback worker on desktop buffer reads. The host release-queue regression
runs with `cargo test --locked -j 1 -p droidloom-update --test release_queue`.
The application-neutral policy defaults on and can be disabled with
`persist.vendor.droidloom.direct_layers=false`. It requires the matching Composer
broker, presenter and Denial surface-tree alpha/orientation support. Live visual
and performance acceptance belongs to the user after activation.

- [Manifest](manifest/README.md): source locks and sparse materialization.
- [Device products](device/README.md): architecture configuration and image boundary.
- [Framework](framework/README.md): Android adapters and minimal system apps.
- [Hardware](hardware/README.md): Composer and allocator integration.
- [Mesa](mesa/README.md): graphics source preparation.
