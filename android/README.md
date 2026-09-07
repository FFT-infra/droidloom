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
`persist.vendor.droidloom.elide_bootstrap=true` (default false). It forwards the
merged completion fences from actual task rendering through the existing HWC
path, including across idle transitions. Only the explicit bootstrap display
is selected; task and capture outputs keep rendering. Fence merge/export errors
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

- [Manifest](manifest/README.md): source locks and sparse materialization.
- [Device products](device/README.md): architecture configuration and image boundary.
- [Framework](framework/README.md): Android adapters and minimal system apps.
- [Hardware](hardware/README.md): Composer and allocator integration.
- [Mesa](mesa/README.md): graphics source preparation.
