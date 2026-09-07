# Architecture

Droidloom runs one Android userspace per configured Linux user. Android supplies
Bionic, ART, Binder, Zygote and system_server. The privileged supervisor creates
namespaces, image mounts and networking; the desktop presenter and app catalog
run as the desktop user. This is shared-kernel isolation, not a VM.

Each top-level Android task has a host-managed logical display and one ordinary
Wayland `xdg_toplevel`. Android Composer and the Rust graphics bridge exchange
task buffers, lifecycle and input over private Unix sockets. The presenter uses
DMA-BUFs and explicit synchronization. Compositor geometry is sent back to Android;
there is no combined Android desktop window or compositor plugin.

Minimal Android SystemUI supplies system UI support and clipboard/notification
adapters. The catalog exports Android activities as XDG desktop launchers.
Launching uses an already-running cell. See [desktop integration](desktop-integration.md).

## Build and source boundaries

`droidloom-package` runs the Rust builder in rootless Arch Podman and produces
runtime and image packages. `droidloom-update` owns source preparation, modified
Android builds, host compilation and assembly, plus a separate developer installer.

The build reuses pinned Android base partitions and rebuilds modified components.
Source locks live in `android/manifest/`; modules are declared in `Android.bp`
and Android product files. Unified packaging currently targets x86_64; specialist
ARM64 workflows remain in `tools/`.

- `runtime/droidloom-supervisor`: privileged lifecycle, control protocol and CLI.
- `runtime/droidloom-applications`: desktop launcher catalog.
- `runtime/droidloom-package-support`: first-user setup and package lifecycle.
- `graphics/droidloom-wayland`: windows, input and desktop integration.
- `graphics/`: Composer, private transport and synchronization libraries.
- `android/framework/`: launcher, input bridge, IME and minimal system apps.
- `android/device/`: product configuration, overlays and image contents.

The development supervisor constructs private device nodes from the host's
actual device geometry. Loop-device minors account for `loop.max_part` rather
than assuming consecutive whole-device minors. The selected DRM render node
uses a private inode with Android-accessible permissions and the same device
number; host node permissions remain unchanged, and no card/KMS node is exposed.

The ARM64 product retains native MSM rendering and also builds Zink/Turnip
for split display/KGSL hosts. The cell's optional `graphics_backend` defaults
to `drm`. Explicit `kgsl_dma_heap` selection exposes private character nodes
for `/dev/kgsl-3d0` and `/dev/dma_heap/system`, verified against their kernel
identities. It selects Zink GLES and minigbm's linear DMA-heap image allocator
for Android children. It does not rename the display driver or grant KMS access.

The image backend preserves minigbm's native-handle ABI, AIDL allocator and
stable-C mapper. RGB, HDR, YUV and blob layouts share DMA-BUFs directly with
the GPU; CPU locks retain DMA-BUF cache synchronization. Imports require one
shared allocation, linear modifiers, valid strides and non-overlapping planes.
Images are bounded to 16384 pixels per dimension and 1 GiB per allocation; R8
blobs may exceed the image width limit. Protected allocations are rejected.
The host presenter still allocates final Wayland targets. Linear images add no
required CPU copy, but their bandwidth cost needs workload-specific measurement.
Passing host feedback/syncobj checks alone does not establish guest GPU support.

## Wayland and Denial

The presenter requires `xdg-shell`, `linux-dmabuf` v4 feedback and
`linux-drm-syncobj` synchronization. Each task has a bounded, generation-scoped
target pool. Android Composer composes task layers into an available target;
the presenter attaches it to the task's Wayland surface and releases it back to
Android only after the compositor signals release.

Opaque coverage belongs to each rendered frame, independently of its ARGB
allocation format. After composing a task, SurfaceFlinger uses
CompositionEngine's transformed and clipped opaque coverage: an empty
`undefinedRegion` proves that the whole target is opaque. The private
`PresentWithContent` request carries that proof beside the buffer ID and
original ready fence. Legacy `Present` requests conservatively mean unknown
opacity. Unknown content bits and nonzero reserved fields are rejected.

Composer sends the existing `SetContentState::OPAQUE` metadata before the
corresponding host `Present` on the ordered task transport, updating it when
opacity changes. The Wayland presenter applies an opaque region in logical
surface coordinates to that commit and clears it for transparent successors;
resizing invalidates the cached region. An ARGB buffer with opaque pixels
therefore need not trigger unnecessary compositor backdrop work, while actual
transparency remains intact. This requires no application-specific override,
pixel readback, extra composition pass or compositor modification.

Each target owns its own Wayland syncobj timeline. Acquire and release points
alternate only across reuses of that same buffer, after its preceding release
has completed. A newer buffer's acquire fence or out-of-order release must not
advance another buffer's release state. This follows linux-drm-syncobj's
per-buffer timeline recommendation and preserves GPU fencing without a CPU copy.
On kernels with syncobj eventfd support, completion of the buffer's release
point wakes the presenter directly. Older kernels retain a bounded polling
fallback. Waking alone never releases a buffer: timeline completion and
resolution of its presentation feedback remain necessary.

The presenter sleeps until a Wayland, Android transport, text-input, clipboard,
or release-eventfd event arrives. It has no periodic idle poll. A 4 ms timeout
is used only while a busy target lacks kernel release notifications; queued
input waiting for clipboard synchronization retains its existing ten-second
deadline. Pending Wayland callbacks are dispatched before sleeping, and output
backpressure waits for socket writability. These waits preserve frame and
clipboard ordering without checking sockets and GPU timelines 250 times per
second on a still screen.

For diagnosis, `DROIDLOOM_PRESENT_AUDIT=1` enables aggregate commit-to-reported-
presentation, feedback-delivery and buffer-release timings. These exclude
Android's earlier input/render work, and buffer-release ages can include idle
time while a compositor retains the current image. The diagnostic logs no
input or application content and is disabled by default.

`DROIDLOOM_FRAME_TRACE=1` enables correlated per-frame records in both the
Android Composer service and `droidloom-wayland`. Every eighth task-local frame
is sampled with its existing object/frame/buffer IDs; no wire protocol or
release policy changes. The SurfaceFlinger broker additionally records its
display ID, which the `android_submit` record maps to the same host object.
All event times use `CLOCK_MONOTONIC` nanoseconds. Android and the host must
share that clock (as in the standard cell); journal delivery time is not a
rendering timestamp.

The recorded path is:

1. `sf_bridge`: first output-target acquisition request, successful target
   reply, retry count, receipt of SurfaceFlinger's rendered target, and ACK.
2. `android_submit`: Composer submission entry, acquire-fence import/export,
   and socket-send interval.
3. `host_receive`, `native_present_begin`, `wayland_commit`, `wayland_flushed`:
   host receipt, fence export, surface commit and successful connection flush.
4. `android_gpu_complete`: original RenderEngine fence's kernel signal time,
   queried without waiting when the buffer returns. `gpu_complete` is the
   host-side fence snapshot when a real timestamp survives DRM transfer.
   Placeholder/stub fences do not establish a GPU completion timestamp.
5. `presented`: compositor-reported display timestamp and local callback time,
   refresh period and display sequence; `discarded` identifies skipped frames.
6. `release_observed`, `buffer_return_sent`, `android_reusable`: independent
   observation of the release point, return-message completion, and Android's
   target becoming reusable. `feedback_pending` records whether presentation
   feedback still held up release. Observation times are upper bounds on the
   corresponding kernel readiness/delivery events, not invented fence times.

The earliest event is the broker receiving SurfaceFlinger's output-target
request. App input, Java/UI work and app RenderThread work before that request
are outside this trace; do not call the total input-to-display latency. The
target-reply-to-present-receipt interval includes SurfaceFlinger submission
and IPC, and is not exclusively GPU execution. Buffer reuse may include idle
time while a compositor retains the current image.

Log writing uses a bounded 256-record channel and a separate thread; producers
use nonblocking sends. Per-process output is capped at 65,536 records, and
sequence numbers plus `dropped_total` expose loss. Original sampled fences
are retained only within the existing bounded in-flight target pool. Query
errors produce unavailable-timestamp records, never zero-duration work.
Tracing is disabled by default and does not log input contents or app pixels.

Denial handles Droidloom windows through ordinary Wayland focus, input, tiling
and resize paths. No Droidloom compositor plugin or alternate window role is
required. Mobile integrations use the same surfaces and seat; the optional
[keyboard-dismissal extension](contracts/text-input-v1.md) provides feedback to
Android without changing ordinary text-input semantics.

The user service starts Android after presenter readiness and stops the cell
when the presenter stops. See [security requirements](threat-model-v1.md) for
the shared-kernel trust boundary.

Droidloom retains each direct task's CompositionEngine, capture output, and
LayerFE identities across buffer-only frames. A target rotation swaps the
render surface's buffer; changes to size, crop, layer stack or pacesetter
recreate the output. Layer membership changes and interrupted submissions
invalidate its geometry history. Snapshots are refreshed in place and retired
layer identities are removed.

Direct SurfaceFlinger presents carry bounded buffer-coordinate damage through
Composer to Wayland. Unscaled, uncropped buffer updates preserve producer
damage; other content changes use the changed layer's visible coverage.
Geometry changes, recovery, and effects that sample outside that coverage
retain full-target damage. Every rotated host target is still completely
rendered, so damage is relative to the preceding submitted frame and never
assumes a buffer age. Legacy private presents retain full damage; explicit
empty damage means unchanged content.

Each native task's refresh follows its entered Wayland outputs, with the
presentation-feedback output taking precedence when a window spans monitors.
Before feedback, the most recently entered known output supplies cadence;
unmapped tasks default to 60 Hz. Enter, leave, hotplug, and mode changes update
cadence without replacing the configure's target allocation generation.
Android's headless bootstrap uses the fastest active task's cadence to support
the shared scheduler without selecting an unrelated faster monitor.
The Composer refresh path also updates an enabled synthetic vsync timer and
reannounces the existing Android display configuration, so SurfaceFlinger
reloads its cached mode period. Disabled clocks remain disabled.

Task discovery first uses Android's completed activity or resolved alias. If
that activity has already handed off, only a visible successor in the requested
package's owned task, Android user, and display is eligible. New tasks take
precedence over preexisting ones; ambiguous candidates are rejected.
SurfaceFlinger retries unregistered tasks every 250 ms initially, then every
five seconds after 24 attempts. Delayed composition callbacks allow an idle
task to recover after late registration. The broker's explicit export allowlist
continues to exclude unsolicited HOME and system tasks.
