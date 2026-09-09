# Known issues and preview limits

These limitations apply to the current pacman preview. Installation and APK
commands are in [INSTALL.md](INSTALL.md); source builds are in
[BUILDING.md](BUILDING.md).

## Application compatibility

- **Moto overlay transparency and video import.** System-popup transitions
  showed intermittent transparency, and the TikTok birthday picker could
  leave a transparent window. A captured TikTok failure revealed SurfaceFlinger
  aborting on a 576x1024 YV12 video import: the DMA-heap allocator's chroma
  pitches violated Turnip's linear-layout alignment. The allocator now aligns
  planar luma pitches to 128 bytes, giving 64-byte-aligned chroma pitches while
  preserving Android's YV12 rules. The Moto `overlay1`
  deployment passed runtime readiness and artifact verification. An offscreen
  Android native EGL import of the failing size failed before the change and
  passed afterward. The user confirmed both reported visual issues work after
  deployment; the crash evidence alone does not isolate the popup mechanism.
- **Moto media playback clock.** The primary audio HAL's legacy BUS detection
  missed current AIDL BUS device types, attempted ALSA, and left the output
  stream in `ERROR` with zero written frames and invalid timestamps. TikTok
  requested speed 1.0 but played videos too fast. The audio-service patch now
  routes both BUS representations to the timed silent stub. Moto `audio-clock1`
  passed boot and running-audio-artifact verification. The user confirmed
  playback works well; host sound integration is still proposed.
- **Moto image-loading stalls and transparent gaps.** A user-prepared TikTok
  and Telegram capture showed target-pool backpressure and sampled buffers
  held for up to 1.73 seconds after compositor release while waiting for
  presentation feedback. Local changes separate release from bounded timing
  metadata, yield a busy task to the next scheduled composition, suppress
  routine acquire-failure log floods, and give both composition modes an
  opaque black backing. Host Rust checks and both ARM64 builds pass. Moto
  `stall1` passed startup and mounted/running-artifact verification; visual and
  responsiveness validation remains with the user. Window disappearance and
  app decoding/upload costs are not yet fully isolated. The capture had about
  3.55 GiB of available host memory and
  zero recent memory-pressure averages; this does not rule out earlier spikes.
  The presenter reported zero restarts, and SurfaceFlinger/SystemUI retained
  their original process start times. Earlier logs separately showed Google
  Play Services repeatedly crashing with `wifiManager cannot be null`; its
  relationship to the reported notification replay is unproven.
- **Experimental ARM64 translation.** Revision 15 includes source-built
  Digitalis/Berberis. TikTok 46.8.2's ARM64 split bundle installed and opened on
  the x86_64 development workstation, with JIT and optimizing-tier execution
  recorded in Android logs. Broader compatibility, sustained playback and
  performance remain under test. ARM32, RenderScript and standalone ARM
  executables are unsupported. `droidloomctl install` still accepts only a
  standalone APK; the TikTok bundle was installed through an Android package
  manager split session. TikTok needed its explicit catalog launcher component
  (`com.zhiliaoapp.musically/com.ss.android.ugc.aweme.splash.SplashActivity`)
  because launching by package name alone did not resolve the activity.
  Installation also recorded ART `dex2oat32` execution failures (`ENOENT`) for
  its dex splits, although installation completed and TikTok opened. Compiler
  selection and ahead-of-time optimization need separate investigation.
  NTE 1.3.1.84649's ARM64 XAPK installs, but revision 15 fails when Unreal
  reserves a 76 GiB arena without `MAP_NORESERVE` on a desktop using heuristic
  overcommit. Revision 16, built from the Digitalis development checkout, fixes
  this container mismatch with an opt-in mapping policy. A debugger-free run
  opened a visible window showing PairIP's Google Play error dialog and then
  exited with status 0. The Play Store licensing service was absent. NTE testing
  is paused pending Google Play support on x86_64; gameplay remains unverified.
- **Fruit Ninja first launch on revisions through 10.** A launcher-to-game or
  age-screen handoff could defeat task discovery; a later successful launch
  could still have no window because SurfaceFlinger permanently abandoned the
  task after six seconds. Revision 11 follows an unambiguous visible same-app
  successor and retries late registration with bounded backoff. User-owned
  visual validation remains required; a CLI response alone does not prove a
  visible desktop window.
- **Legacy app layouts.** Revision 7 enables Android's force-resizable setting
  so non-resizable apps can enter desktop windowing mode. This does not guarantee
  that every game handles arbitrary window sizes or desktop input correctly.
- **App services.** Google Play Store / Google Play Services are not supplied as
  an installation step by this preview. An experimental [optional Google-app
  builder](BUILDING.md#optional-google-apps) supports locally supplied Android 17
  archives and raw ext4 developer images; first activation requires fresh Android
  data. Account sign-in and opening Play Store have been verified on the Moto
  ARM64 developer cell. Catalog availability, app installation, push delivery
  and integrity checks require separate validation on each target. Older vendor
  images omit touch features and report no GLES version, which can hide apps;
  see the capability checks in the optional Google-app build guide.
  On the Moto cell, Play reports an uncertified device. TikTok 46.8.3 became
  available after a device-local test added generic location and Bluetooth
  declarations to the corrected touch/GLES profile. Play completed the base
  installation and six additional feature installs, and the user verified the
  app runs from HOME. Those declarations do not implement location or Bluetooth
  integration and are not enabled in the release product. The APK analyzer
  infers generic location from location permissions even though GPS is optional;
  it also infers Bluetooth from a legacy permission capped at API 30. Testing
  both together does not isolate which declaration affected the catalog.
  Play Store's Open button exposed a separate missing intent-to-host task path;
  the new task observer and host activation path still require device validation.
  Runtime DRM/integrity behavior remains untested.
  Installing an APK does not establish that
  its account, billing, integrity checks or other service dependencies will work.
- **Standalone APKs only.** `droidloomctl install` accepts one standalone APK.
  Split bundles, drag-and-drop installation and separate game assets are not
  supported by the command. Older preview packages need an update to provide it.

## Desktop and runtime

- **Startup is manual.** Run `droidloomctl start` before launching an app. App
  launch does not start a stopped runtime. First boot may take longer than later
  starts while Android initializes its data.
- **Direct layer composition needs host support.** The global layer-export path
  requires Denial's surface alpha and buffer-orientation handling. Unsupported
  effects, secure/protected content, HDR, formats, and inexact fractional geometry
  retain SurfaceFlinger composition. Package and unit checks do not establish
  visual correctness or a measured performance gain; those need acceptance in
  the updated session.
- **Graphics support is bounded.** The package targets AMD and Intel. A dual-GPU
  AMD/NVIDIA laptop was tested using AMD; NVIDIA rendering is not validated.
- **First-start authentication needs a desktop agent.** If the polkit prompt
  is unavailable, use the explicit sudo setup helper in the installation guide.
  The exact normal-user graphical authentication path still needs a complete
  recipient-style rehearsal; the fresh installation test used explicit setup.
- **Clipboard and input depend on the desktop and app.** Background clipboard
  sharing depends on compositor protocol support. Surrounding text and cursor
  geometry are not exported through the text-input integration. Inline
  notification replies and custom Android notification layouts are unsupported.
- **Shared host kernel.** This is an Android container runtime, not a virtual
  machine. See [security requirements](threat-model-v1.md) for isolation boundaries.

## Performance review targets

These findings describe the current source, including working-tree changes;
they are not measurements of an installed build. Priorities indicate where to
investigate first, not measured shares of CPU time or latency.

1. **High: clipboard synchronization stalls unrelated input.**
   The presenter's `send_input` queues touch and keys while clipboard import is
   pending, with a ten-second blocking timeout. Once the 512-event queue fills,
   newer events bypass it, potentially arriving before older DOWN/MOVE records.
   Timestamps are assigned on send, so queued time is also absent from those
   timestamps. Restrict clipboard ordering to operations that require it,
   preserve gesture/key ordering under overload, and timestamp at receipt.
   Validate with a stalled clipboard peer while delivering touch and keys.
   Sources: [presenter](../graphics/droidloom-wayland/src/main.rs),
   [clipboard state](../graphics/droidloom-wayland/src/clipboard/mod.rs).

2. **High: slow input consumption can block graphics control reception.**
   Composer dispatches input inline in the same receive loop that handles
   configure, presentation and buffer-release records. Its framework sender
   uses a blocking sequenced-packet socket while holding the input-state mutex.
   If the Java reader stalls and the socket fills, that receive loop waits too.
   Decouple input writes from graphics control reception with bounded,
   order-preserving backpressure; do not indiscriminately drop input events.
   Validate by pausing a synthetic input peer and checking continued control
   message processing. Sources: [receive loop](../graphics/droidloom-composer-service/src/main.rs),
   [input sender](../graphics/droidloom-composer-service/src/input.rs),
   [socket transport](../graphics/droidloom-denial-ipc/src/lib.rs).

3. **Medium: app catalog repeatedly scans unchanged state.**
   The catalog queries Android and reconciles launcher files every 30 seconds
   by default. Icon keys and write-if-changed avoid some work, but the query and
   filesystem comparisons still run. Prefer package/user/locale change events
   with a full snapshot at connection or recovery. Measure idle wakeups and
   refresh cost across catalog sizes. Sources:
   [polling loop](../runtime/droidloom-applications/src/main.rs),
   [reconciliation](../runtime/droidloom-applications/src/lib.rs).

4. **Medium: notification changes resend the full active snapshot.**
   After coalescing callbacks, the Java writer re-encodes every active entry,
   including each 48-by-48 RGBA icon as 9,216 JSON numbers. Icon rendering is
   cached and the host avoids unchanged desktop Notify calls, but serialization,
   transport and parsing still repeat. Use versioned deltas and reusable icon
   identities, retaining full snapshots for reconnects. Measure traffic and CPU
   while one notification updates among many unchanged entries. Sources:
   [Android writer](../android/framework/droidloom-systemui/src/com/android/systemui/notifications/NotificationBridge.java),
   [host reconciliation](../graphics/droidloom-wayland/src/notifications/mod.rs).

The extra Composer/Java/Binder input hops are also a candidate for simplification,
but their latency has not been isolated. Existing frame tracing starts after app
input and rendering work; it cannot establish touch-to-display latency. Preserve
real GPU release dependencies and the existing event-driven idle waits when
optimizing the graphics path.

Updated direct-layer peers pipeline Stage records and subscribe to revisioned
configuration changes, leaving one synchronous Commit reply per changed scene
with cached imports and no layer requests for unchanged scenes. Older peers keep
Config queries and individual Stage acknowledgements according to capabilities.
Commit still validates current configure identity and geometry. New allocation
registration remains synchronous; avoiding that wait needs import readiness and
failure handling before a scene can reference the allocation.
Source: [SurfaceFlinger layer sender](../android/surfaceflinger/0007-droidloom-wayland-layers.patch).

The current source now reuses render/input receive storage, suppresses unchanged
direct-layer geometry requests, retains layer staging capacity, skips eventfd
reads for idle source imports, retains syncobj/eventfd resources and poll storage,
and disables key tracing by default. See
[render/input fast-path work](architecture.md#renderinput-fast-path-work) for
validation and the limits of the local transport measurement.

## Build and distribution

- The rootless package workflow has passed package lifecycle checks and fresh
  Omarchy installation tests, but downloading all sources with completely empty
  caches has not yet been rehearsed.
- Package staging needs additional disk space beyond retained compiler outputs.
  A build can compile successfully and still fail during image copying or archive
  creation if the filesystem is full.
- Uninstalling the packages retains Android data and personal configuration.
  Reinstalling or downgrading the software does not reset or roll back app data.
- Older developer-installed runtimes can conflict with package-owned paths.
  Back up and retire such an installation before migration; do not use blanket
  pacman overwrite options.

## Reporting a problem

Include the package revision, compositor, GPU, APK version and architecture, the
command used, and what appeared on screen. Collect logs before restarting Android:

```console
pacman -Q droidloom-runtime droidloom-image
droidloomctl logs -n 500
droidloomctl crashes com.example.app
journalctl --user -u droidloom.service -n 100 --no-pager
```

Replace `com.example.app` with the affected package. Review logs for personal
information before sharing them. A package check passing establishes installation
behavior, not compatibility with every Android app.
