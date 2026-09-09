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
target pool for composition fallback. The default task path first resolves
SurfaceFlinger's ordinary layer occlusion and output geometry, then exports
eligible original Android buffers as synchronized Wayland subsurfaces. Denial
samples those buffers in its desktop composition, removing the intermediate
SurfaceFlinger task render. This policy applies to every task and supports
multiple layers; it never checks application names.

The authenticated Composer broker delegates a pair of task-scoped sequenced
sockets. Image planes are registered once, imports are confirmed asynchronously,
and a complete staged layer list commits atomically under the current configure.
The root keeps the XDG role and task-relative input. A constant black XRGB SHM
buffer supplies an opaque backing; its reduced dimensions preserve the task's
aspect without allocating a full-window image. This matches the black fill in
SurfaceFlinger's composed task output, including gaps between producer layers.
Switching back to a composed target damages the whole root: Android's damage
rectangles describe differences between app frames, not replacement of the
presenter's constant backing. Already-hidden children are not detached again
on subsequent composed frames.
Child input regions are empty. Source rectangles, orthogonal transforms, stacking,
opaque coverage, per-layer alpha and constant-color layers are preserved. An
unchanged scene retains its existing attachments instead of generating new work.
Opaque ARGB/ABGR Android layers use an XRGB/XBGR import view of the same planes
and modifier, preserving Android's ignored-alpha semantics without a copy. A
blend-mode change retires the old view only after its reads complete; rejected
format reinterpretations select normal composition fallback.

Each source allocation owns an independent Wayland syncobj timeline. The original
producer fence gates the host read. Only a signalled compositor release point
completes the corresponding Android read ticket. As soon as the compositor
materializes a release point, the presenter exports its real `sync_file` without
waiting for it to signal. SurfaceFlinger merges that fence with Android's release
dependencies for the exact GraphicBuffer ID and producer frame number, including
layer destruction. Until a fence exists, only dependent callbacks are deferred;
no host-read wait occupies the shared transaction callback worker. Completion
callbacks retain order within each listener while other apps continue. Commit
callbacks always retain surface/frame metadata and never depend on release.
Socket acknowledgements and presentation timestamps do not release a source.
Disconnects leave missing fences unresolved; delivered kernel fences continue
protecting producer reuse independently of the socket.

Scenes fall back as a whole for unsupported formats, failed imports, protected
or secure content, HDR/LUT/color operations, effects, front buffers, duplicate
source attachments, or geometry that cannot be represented exactly. Fractional
host scales must preserve every layer edge in integer Wayland surface coordinates.
The fallback renders into a host target and detaches the direct children in the
same root commit. Receiving a direct commit first drains older Composer records
to preserve ordering across both sockets. The stream bounds scenes to 64 layers
and caches to 128 source allocations per task.

The duplicate headless bootstrap draw is also disabled by default; its HWC
scheduling remains active. `persist.vendor.droidloom.elide_bootstrap=false`
restores that draw for comparison.

`persist.vendor.droidloom.direct_layers=false` selects composition fallback for
all tasks. Missing host alpha-modifier support also selects fallback. Existing
host targets remain subject to the original configure and release rules.

Opaque coverage belongs to each rendered frame, independently of its ARGB
allocation format. SurfaceFlinger's normal task output now includes an opaque
black fill below the app layers. The private
`PresentWithContent` request carries that opacity beside the buffer ID and
original ready fence. Legacy `Present` requests conservatively mean unknown
opacity. Unknown content bits and nonzero reserved fields are rejected.

Composer sends the existing `SetContentState::OPAQUE` metadata before the
corresponding host `Present` on the ordered task transport, updating it when
opacity changes. The Wayland presenter applies an opaque region in logical
surface coordinates to that commit and clears it for transparent successors;
resizing invalidates the cached region. An ARGB buffer with opaque pixels
therefore need not trigger unnecessary compositor backdrop work. Transparency
within a normal app blends over its other layers and black backing; legacy
targets still honor their supplied opacity metadata. This requires no application-specific override,
pixel readback, extra composition pass or compositor modification.

Each target owns its own Wayland syncobj timeline. Acquire and release points
alternate only across reuses of that same buffer, after its preceding release
has completed. A newer buffer's acquire fence or out-of-order release must not
advance another buffer's release state. This follows linux-drm-syncobj's
per-buffer timeline recommendation and preserves GPU fencing without a CPU copy.
On kernels with syncobj eventfd support, completion of the buffer's release
point wakes the presenter directly. Older kernels retain a bounded polling
fallback. Waking alone never releases a buffer: the actual release timeline
point must have completed. Presentation feedback is independent and never
holds a reusable target. At most 64 outstanding feedback requests per task
retain frame identities and timing metadata, without retaining buffers or
fences. When full, new frames omit the optional timing request until capacity
returns. Late `Presented` events remain valid after `BufferReleased`.

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
   feedback was outstanding when release was observed; it no longer gates
   reuse. Observation times are upper bounds on the
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

Fallback SurfaceFlinger presents carry bounded buffer-coordinate damage through
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

## Render/input fast-path work

The Moto two-app stall capture found 112,239 target-acquisition failures in a
roughly 15-minute host log, amplified into over 900,000 log lines. Sampled
buffers remained unavailable for up to 1.73 seconds after observed compositor
release while waiting for presentation feedback. The presenter now separates
those lifetimes as described above. A task's `dequeueBuffer` returns
`WOULD_BLOCK` on pool exhaustion; SurfaceFlinger preserves the previous image,
invalidates that task for a full retry, schedules the next composition and
continues processing other tasks. Normal backpressure no longer logs an error
on every attempt. The bootstrap window retains its blocking contract. These
changes require user-owned device validation; they do not establish how much
of first-image loading is app decoding, allocation or GPU upload work.

Render and input receive loops reuse caller-owned packet storage. Direct-layer
requests use a 128-byte buffer matching their wire limit; general task and
SurfaceFlinger connections retain maximum-sized storage across requests.
The transport still rejects truncation and closes rejected descriptors.

Direct-layer views cache their applied geometry independently of buffer IDs and
read tickets. Buffer-only frames retain stacking, position, viewport, opacity,
alpha and transform state, avoiding repeated opaque-region creation. Geometry
changes still update that state; root state is reapplied after composition
fallback. Layer staging retains its allocation across commits. Idle cached
source imports no longer incur an eventfd read on every presenter wakeup.
Acquire/release dependencies and per-buffer attachment rules remain unchanged.

The timeline bridge now retains one private binary transfer syncobj per timeline,
created lazily and reused under exclusive mutable access. After the first use,
each fence import/export avoids a syncobj create/destroy pair (four fewer DRM
ioctls for one import plus one export). The scratch handle is never shared with
the compositor. Linux exports a reference to the current fence into a sync_file;
replacing the scratch object's fence does not change previously exported files.
This follows the kernel's [syncobj import/export semantics](https://raw.githubusercontent.com/torvalds/linux/master/drivers/gpu/drm/drm_syncobj.c).
The opt-in DRM regression also checks scratch-handle stability across 128 buffer
cycles; it remains hardware-dependent and was not run in the host test suite.

Each direct-layer source retains its availability eventfd across reads. Rearming
drains the previous notification before registering the new timeline point, so
an immediate new notification cannot be consumed accidentally. An unsupported
kernel is remembered for that source rather than retried on every frame. Host
tests use real eventfds to cover reuse, stale/immediate notifications, unsupported
registration and error propagation. Availability still never proves completion.

The presenter retains its poll descriptor and layer-task vectors between wakeups.
Layer and text-input descriptor enumeration uses iterators rather than temporary
vectors, retaining the existing poll readiness and idle-sleep behavior.

The layer stream advertises optional `ASYNC_STAGE` capability bit 2. Updated
SurfaceFlinger sends opcode 7 for each layer without waiting for individual
acknowledgements, then waits for the existing complete-scene Commit result.
For a changed scene with cached imports, staging alone reduces synchronous
replies from N+2 to two (Config and Commit); allocation registration remains
separately confirmed.
Peers lacking the capability keep opcode 4 and its per-layer replies. Scene
overflow is sticky through Commit and releases all rejected staging resources;
Abort and disconnect discard staged metadata without releasing existing reads.
A lost Commit reply still leaves submitted source dependencies unresolved.

Optional `CONFIG_EVENTS` capability bit 3 removes the remaining per-frame Config
query after initial subscription. The host publishes revisioned configuration
changes on the existing completion socket; the sender caches them and schedules
composition when they change, including when an app is idle. The revision orders
the initial reply against events on the other socket and covers visibility
changes without a new configure serial. The host publishes after draining all
layer barriers, including configuration changes consumed from the Composer
socket during that drain. The repaint callback is created once with the stream.
Commit still checks the current host
configuration. With both extensions and cached imports, a changed scene requires
one synchronous reply (Commit); an unchanged scene sends no layer requests.
Legacy hosts retain per-frame Config queries. These are protocol operation
counts, not measured end-to-end latency improvements.

The Android sender's packet helper uses a stack header plus payload iovecs,
avoiding its former packet vector allocation and payload copy. A host C++ test
applies the actual 0007/0008 patch chain and exercises the resulting transport:
64 queued layers with descriptors before any reply, Commit rejection, the
legacy acknowledgement path, and send failure. It also checks configuration
subscription, a newer resize event racing the initial reply, visibility changes,
duplicate/invalid revisions, callbacks outside the cache lock, and 1,000 cached
configuration reads that send no requests. Run it with:

```console
cargo test --locked -j 1 -p droidloom-update --test release_queue
```

Rust regressions cover both stage opcodes, descriptor and flag validation,
sticky overflow, resource cleanup and staging-capacity reuse. These tests do
not replace a full Android build or on-device rendering acceptance.

Configuration-event validation passed 15 surface-bridge tests, 39 Wayland tests
(including integration tests), and two C++ regressions. The complete nine-patch
SurfaceFlinger sequence also applies to the local upstream checkout in an
isolated temporary directory. `cargo check` passes for the presenter; Clippy
completes with existing warnings. The full x86_64 package build for 0.1.0-14
compiled the modified Android targets, including SurfaceFlinger and Composer.
That build caught and corrected explicit AOSP weak-pointer construction and
Rust 2021 syntax compatibility in the optional Composer key tracing. Device
latency measurements remain pending.

The Moto ARM64 build was deployed as `fastpath1` at
`/var/lib/moto70-droidloom/fastpath1`. Activation verified Android boot completion,
the running input bridge, a 14-app catalog, all staged Android files as mounted
inside the cell, and running presenter/daemon/catalog executable hashes. The
phone boot ID and Denial PID remained unchanged. GApps, userdata and the existing
private host Mesa configuration were retained. The prior cell configuration and
service definitions remain available for rollback. Local deployment evidence is
under `.work/moto-fastpath-deploy/`; this readiness check does not establish
touch-to-display latency or visual acceptance.

Key-event logs are off by default. `DROIDLOOM_INPUT_TRACE=1` enables them in each
process where the environment is set: presenter, Composer, or Java input bridge.
The flag is read once per process, not from the environment on every event.

Host regression tests cover reusable record boundaries, truncated-FD cleanup,
geometry stability across buffer rotation, and resize/opacity/solid transitions.
The explicit local benchmark is:

```console
cargo test --locked --release -j 1 -p droidloom-denial-ipc benchmark_reused_record_storage -- --ignored --nocapture
```

Six alternating local trials of 50,000 112-byte socket records measured median
send/receive cost of 2.56 microseconds with the allocating receiver and 2.04 with
reused storage (about 20% lower). This isolates local socket/buffer work; it is
not a frame-rate or touch-to-display measurement. The x86_64 Android module build
has passed; visual acceptance and latency measurements on the target remain
necessary to validate the deployed behavior.

## CPU placement and Android task profiles

The host presenter and Android graphics services are separate scheduling
boundaries. A placement policy inside Denial affects neither the user-managed
`droidloom.service` nor the cell owned by the system `droidloomd.service`.
Restricting the entire cell to one CPU class would also restrict Android init,
Zygote, application processes and unrelated services.

### Implemented policy

`runtime/droidloom-cpu-placement` separates worker roles from Linux topology
discovery and Android controller placement. Roles are declared at process or
worker creation; there is no thread-name scanner or polling daemon.

| Role | CPU domain | Owned work |
| --- | --- | --- |
| Graphics | Every capacity tier above the smallest | Presenter event loop and render/submission dependencies; Composer; SurfaceFlinger, RenderEngine and timing workers; input bridge main path |
| Normal | Original inherited CPU domain | Lifecycle helpers before exec; ordinary Android foreground work |
| Background | Smallest capacity tier | Lifecycle daemon; application catalog; clipboard/notification transfer workers; frame-log writers; explicitly minimum-priority Mesa queues |

The host backend reads `cpu_capacity` for the CPUs allowed at startup. It keeps
prime cores in the graphics class, respects the inherited domain, and does not
guess topology from CPU numbers or frequency. Homogeneous domains need no
partition. Missing topology produces a diagnostic and preserves inherited
masks; a platform can supply the paired `DROIDLOOM_BIG_CPUS` and
`DROIDLOOM_LITTLE_CPUS` overrides. `DROIDLOOM_CPU_PLACEMENT=off` disables the
capacity partition. No Motorola model, serial, or CPU-number condition is part
of the runtime policy.

The lifecycle daemon captures its original domain before moving to background.
Its child-command wrapper restores that domain with one affinity syscall before
exec, so Android init and command helpers cannot inherit little-only placement
accidentally. The presenter initializes its background notification reactor
before restoring graphics placement and initializing GBM/EGL. Other owned
workers declare their role at entry, before spawning dependencies or doing work.

The Android backend in `runtime/droidloom-supervisor/src/cpu_placement.rs`
prepares private legacy cpuset and blkio subtrees before entering the cgroup
namespace. Creating a new legacy hierarchy after `CLONE_NEWCGROUP` failed with
`EPERM` on the tested kernel. The backend mounts each controller in the private
mount namespace, creates a cell subtree below the caller's existing group,
joins it, and retains only that subtree at a private holding path. The temporary
full-hierarchy discovery mount is detached before Android starts. Declared
`/dev/cpuset` and `/dev/blkio` mount points remain unmounted until Android's
`CgroupSetup` runs: an already-mounted v1 descriptor otherwise makes AOSP return
early and skip its v2 subtrees. Android mounts its own group root normally. The separate v2 memory/freezer domain is retained. Cleanup removes only
empty, cell-owned legacy groups.

Android retains its native task-profile API. Generated init policy assigns
top-app to graphics, foreground to normal, and background/system-background/
restricted to little. Existing CPU `JoinCgroup` actions are translated to these
cpuset groups; non-placement actions and aggregate profiles are preserved.
Blkio is prepared because Android's scheduling aggregates include I/O actions.
This supplies their existing private policy, without adding frequency boosts or
utilization-clamp tuning. A host which reserves these controllers for cgroup v2
or disables them receives an explicit backend diagnostic; the implementation
does not take controllers away from host services. Such platforms need another
Android backend. This is not a claim of compatibility with every Linux kernel.

Composer and the input bridge request `MaxPerformance`. SurfaceFlinger's main,
RenderEngine and timing workers use the graphics group. Runtime generation
merges the existing SurfaceFlinger service stanza, preserving other options and
bootstrap imports. Owned Java notification and clipboard workers enter Android's
background service group and use background priority. Arbitrary app internals,
ART/GC/JIT workers, and unrelated services retain Android's process-state policy.

Mesa patch `android/aosp-patches/0014-mesa-background-cpu-placement.patch`
applies a platform's optional background policy at queue entry, after Mesa's
affinity reset and before the first job. It affects only queues explicitly
marked minimum priority. Zink cache-put is marked accordingly; cache lookup,
compilation and submission remain on their creator's graphics domain. The hook
also removes inherited FIFO/RR scheduling from these background queues.
`MESA_BACKGROUND_CPUS` selects the host mask; `MESA_BACKGROUND_CPUSET` selects an
Android tasks file. The Android builder applies this patch automatically, and
SurfaceFlinger's generated service supplies the cell path. Host platforms must
provide the same hook in their Mesa build to obtain the Mesa worker exception;
an unpatched host Mesa does not consume these variables. No global loader
interposition or thread-name inference is used.

Android application process-state transitions can override per-thread cpuset
requests, including those of the replacement SystemUI's bridges. Their explicit
background priority remains useful, but startup role assignment alone is not a
persistent little-only guarantee for AMS-managed app processes. This is an
observed limit; no thread-name sweep or repeated affinity repair hides it. The
hard graphics/background separation is verified for the host presenter and
init-managed Composer, SurfaceFlinger and input bridge.

Focused native tests cover topology partitioning, invalid masks, worker and
exec inheritance, profile translation, and service-stanza preservation. The
non-UI `cpu-placement-probe` example checks real controller remount isolation,
big/little/normal transitions, inherited masks, and unchanged v2 membership.
Run it as root only inside a disposable private mount namespace; it is not a
visual test. The Mesa worker test is documented in `android/mesa/README.md`.

### Implementation validation on Moto

The final `cpu-placement2` bundle booted Android successfully with all three
Droidloom host services active. Running executable/APK/JAR hashes matched the
staged manifest. Denial's PID and the phone's boot ID stayed unchanged. A final
passive snapshot found presenter 3 graphics / 10 background threads,
SurfaceFlinger 23 / 4, and Composer 9 / 1. Graphics allowed CPUs 3-7 and
background 0-2; Android init retained 0-7. Both host Gallium and Turnip mapped
from Droidloom's private prefix. Minimum-priority Mesa workers were SCHED_BATCH,
nice 19. SystemUI's four bridge workers had nice 10 but retained the observed
Android-managed 0-7 cpuset limitation described above. Existing timer-slack
writes still report read-only procfs errors; this does not imply CPU placement
failed or establish complete support for every task-profile action.

The focused native tests, Mesa worker test, ARM64/Android builds, APK signer
verification and disposable controller integration probes passed. The raw logs,
exact artifact manifest and rollback instructions are retained outside Git at
`/mnt/development/moto70edge/droidloom-prep/cpu-placement-implementation/`.
No UI test event was generated, and no frame-time gain is claimed from these
health and affinity checks.

### Initial inspection (before implementation)

A passive Moto Edge 70 inspection on 2026-09-08 found a concrete integration
gap. The kernel exposes `cpu` and `cpuset`, but Android's `/dev/cpuctl` and
`/dev/cpuset` are ordinary tmpfs directories. Their foreground/top-app/background
subdirectories have no controller files. The private cgroup2 mount is rooted at
the host's `/system.slice/droidloomd.service`; its available controllers were
only `memory pids`, with no CPU or cpuset controller enabled for the cell.

`runtime/droidloom-supervisor/assets/development-cgroups.json` marks the legacy
CPU, cpuset and blkio controllers optional, and declares only freezer and memory
in its v2 descriptor. `bind_development_cgroups` in `development.rs` installs
this descriptor over Android's `system/etc/cgroups.json`. This lets Android
start on the unified host, but does not supply the missing CPU policy backend.
The live journal contains failed `HighPerformance`, `ProcessCapacityHigh` and
`MaxPerformance` profile applications. Other actions such as thread priority
can still work; this is not evidence that every Android scheduling mechanism
is inactive.

The installed Android `task_profiles.json` still maps capacity profiles through
`JoinCgroup` to the missing cpuset controller: `ProcessCapacityLow` to
`background`, `ProcessCapacityHigh` to `foreground`, `ProcessCapacityMax` to
`top-app`, and `ServiceCapacityLow` to `system-background`. CPU performance
profiles similarly refer to the absent `cpu` controller. Android already
provides the profile abstraction and foreground/background transitions; its
backend needs to be connected to the actual delegated host hierarchy. See
[AOSP's cgroup abstraction contract](https://source.android.com/docs/core/perf/cgroups).
Profile actions must be audited individually so a placement repair does not
silently introduce frequency boosts or unrelated resource changes.

The installed Composer init service and repository
`android/device/droidloom_arm64/droidloom-composer.rc` declare
`task_profiles ServiceCapacityLow`. That currently does not enforce little
placement because the controller is missing. Once capacity groups work, blindly
mapping `system-background` to little would classify this graphics-critical
service incorrectly. Give Composer an explicit graphics placement role and
separate its housekeeping work.

### Initial proposed allocation boundaries

| Work | Initial placement and implementation boundary |
| --- | --- |
| `droidloom-wayland` event loop, input delivery, DMA-BUF/fence transport, Mesa submission `zfq` and mixed compilation `zcfq` | Big default before GBM/library initialization, with explicit worker exceptions |
| Presenter notification bridge, clipboard transfer workers, `dl-frame-log`, Mesa cache-put `zcq` and minimum-priority disk/trace queues | Little at owned worker entry; bounded compatibility handling for Mesa |
| Presenter async/D-Bus helpers and unidentified threads | Retain big until their full responsibilities are established |
| Android Composer Binder work, `droidloom-vsync`, `droidloom-denial-events`, SurfaceFlinger listener and `droidloom-sf-task` handlers | Big; they carry timing, buffer and release dependencies, including direct-layer transport |
| Composer task-control listener/clients | Keep big initially: configure/registration/activation are visible launch dependencies |
| Composer `dl-frame-log` | Little |
| SurfaceFlinger, RenderEngine, scheduler/timing lanes and dependent Mesa rendering/submission workers | Big graphics domain; classify housekeeping separately |
| Android app UI/RenderThread, input dispatcher and Droidloom input/text bridge | Use Android state/explicit roles with a functioning backend; do not restrict all of system_server or Zygote |
| `droidloom-applications` launcher/icon catalog | Little; this is periodic desktop catalog reconciliation |
| `droidloomd` management work | Medium initially; little is possible only after restoring the original domain before cell/helper launch |
| Other Android apps/services, mixed ART worker pools and unknown workers | Keep Android's state-aware policy; no blanket little classification from daemon/JIT/GC names |

The host clipboard code has four unnamed `std::thread::spawn` sites in
`graphics/droidloom-wayland/src/clipboard/mod.rs` (native receive, Android
receive, Android send and offer writing). Add explicit role/name registration
there before treating the identically named live threads as proven clipboard
workers. Notification and frame-log workers are already named. Java bridge
workers are created by `TextInputBridge`, `ClipboardRelay`, `HostInputMethod`,
the SystemUI bridges and the input bridge's scheduled executor; not all are
part of the same process or have the same latency requirements.

There is also a priority-inheritance concern distinct from placement:
SurfaceFlinger's live `surfacefl:zcq0` cache-put worker had `SCHED_FIFO` priority
2, as did rendering workers. The disk and trace queues had `SCHED_BATCH`, nice
19. Thus neither ordinary nice 0 nor an inherited RT policy alone identifies
work that deserves a big core. Do not extend Denial's priority demotion to
arbitrary Android threads; retain known graphics/real-time roles and explicitly
handle any proven background inheritance.

`start_development_cell` launches `ip` -> `unshare` -> supervisor -> Android init
without resetting affinity. The `nsenter` task-launcher/catalog paths similarly
inherit their caller's CPU mask. A future little-only lifecycle daemon must
restore the original application domain before those launches, otherwise it
can inadvertently confine the entire Android workload to little. Zygote-spawned
apps have their own Android process-state policy; process ancestry is not a
sufficient role declaration.

### Inspection evidence and limits

Boot `ecbda8eb-af2e-49ae-952c-437c8efbae34`, Moto serial `ZY22MMG59D`.
The initial process listing contained 89 processes and 1301 threads across the
three Droidloom roots and their descendants. These counts include the Android
system and apps, not just Droidloom-owned workers. A later affinity inspection
of 427 tasks across 12 selected processes found every sampled mask at `0-7`.
Snapshots are not atomic: workers appear/disappear, so counts differ between
the initial listing and the per-process affinity read.

Key host PIDs and initial thread counts were presenter 1135 (12), lifecycle
daemon 1133 (1), catalog 1345 (1), Android init 1346 (3), Composer 2254 (11),
input bridge 2288 (18), SurfaceFlinger 6590 (32), and system_server 6675 (208).
Android namespace PIDs differ: init is 1, Composer 650 and SurfaceFlinger 4059.
SurfaceFlinger main, RenderEngine and Mesa submission/GL/driver workers were
last observed on little CPUs despite FIFO priority 2. Last CPU is a snapshot,
not a residence-time or frame-cost measurement.

Presenter SHA-256
`57efaf0da2e57891ec3c087992360426e7ed043d13a1d81bf62627d1f480e68b`
matches the retained `root-aspect/stage/droidloom-wayland` artifact.
Composer SHA-256 is
`ed29841ad0a65bdaa4bf82ceaa84d81adbb317341785a14cf8d7010842e96fed`;
SurfaceFlinger SHA-256 is
`ce495505acb8a0057b73033c4513cb6a95bd729effc783a1883c2a1ddeb4e1ac`.
The current checkout has existing development edits; this inspection does not
assert that its complete source tree matches every installed binary.

Raw process, affinity, installed profile/mount and journal evidence is retained
outside Git at `/mnt/development/moto70edge/droidloom-prep/cpu-placement-audit/`.
No runtime code, affinity, controller configuration, service, Android image or
frequency policy was changed during this inspection. No UI event was generated.
No performance gain is claimed until the user exercises the Android frame path
with comparable before/after timing data.

The implementation should first establish the host presenter's owned worker
roles and reuse the native affinity backend, then repair the Android cell's
controller/profile integration and add explicit graphics roles. The current
kernel already exposes the relevant controllers; this finding does not justify
a kernel upgrade. Verify effective masks, profile success, child inheritance,
foreground/background transitions and background exceptions before treating the
Android integration as complete.
