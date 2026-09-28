# Known issues and preview limits

These limitations apply to the current pacman preview. Installation and APK
commands are in [INSTALL.md](INSTALL.md); source builds are in
[BUILDING.md](BUILDING.md).

## Video decode

- **Sheng Iris hardware decode is opt-in, and preferred once enabled.** The path
  is Android `MediaCodec` → Codec2 AIDL → AOSP `v4l2_codec2` → the host's
  Qualcomm Iris stateful V4L2 decoder, with DMA-BUF buffers. Only builds made
  with `DROIDLOOM_ENABLE_EXPERIMENTAL_IRIS_CODEC2=true` include the Codec2
  service and capability XML, and a cell exposes the decoder only when its
  `cell.json` sets `"video_decoder": "iris"`. Codec2 sizes input buffers from
  the OUTPUT `sizeimage` that Iris reports when the service starts: 7,077,888
  bytes for H.264, HEVC and AV1 and 14,155,776 bytes for VP9 on sheng. That
  size is a floor. A client `max-input-size` can raise the buffers but never
  shrink them below it, which the driver rejects with `EFAULT` at the first
  `VIDIOC_QBUF` instead of decoding anything. A qualification run on sheng
  decoded a 60-frame clip with H.264, HEVC, VP9 and AV1 at 1080p and 4K, with
  both ByteBuffer and Surface output. Longer playback, seeking, mid-stream
  resolution changes and concurrent decoders are untested. The capability XML
  now advertises 4K30 or 1080p60, bitrate up to 120 Mbps and four concurrent
  decoder instances, and all four `c2.v4l2` decoders rank 256, ahead of
  Android's software codecs at 512. An app that selects a decoder by format
  therefore gets hardware, and falls back to software only outside those limits
  or once the vendor service has admitted four instances. The 120 Mbps ceiling
  is an advertised bound, not a measured throughput, and decoding more than one
  stream at a time has not been measured.

## Sheng daily-use integration

- **Android playback reaches the host sink, with underruns; capture does not
  exist yet.** The cell's primary output is an `AUDIO_DEVICE_OUT_BUS` port whose
  stream Droidloom's patched audio HAL writes to `/dev/socket/droidloom/audio`,
  the endpoint the supervisor projects into the cell; the `droidloom-audio` user
  service forwards one stream at a time to the session's PipeWire sink. On sheng
  the in-cell HAL logged `AHAL_DriverSocket: connect: sending 48000 Hz`, Douyin
  video playback produced a 56.8 s host stream and a notification sound a 4.8 s
  one, so application audio leaves the cell. The longer stream also logged
  `AudioFlinger: prepareTracks_l BUFFER TIMEOUT` underruns, so sustained
  playback still needs measuring. Nothing captures in the other direction:
  recording returns silence, and microphone support must not be described as
  available.
- **Pen and window acceptance is still partial.** The Android bridge maps Linux
  `BTN_STYLUS` and `BTN_STYLUS2` to Android's standard primary and secondary
  stylus-button key codes; Droidloom contains no StarNote-specific action. The
  user has confirmed pen input and finger dragging work on sheng, but button
  press/release traces, pressure, tilt, palm rejection, reconnect, edge resize,
  and saved window size still need repeatable device tests.
- **UU remote-play acceptance is pending external test inputs.** UU Remote
  (`com.netease.uuremote` 4.42.0) is installed on sheng, but a signed-in account
  and a test peer are still required before its video and audio paths can be
  judged.
- **Douyin keeps its phone layout, and product identity is not the gate.**
  The installed Douyin renders the phone layout, with its bottom navigation bar,
  in a freeform task. A complete Xiaomi identity measured through a file
  override (`ro.product.model=24018RPACC`, `brand` and `manufacturer` `Xiaomi`,
  device and name `sheng`, Xiaomi fingerprint) changed nothing, runtime MIUI
  properties (`ro.miui.ui.version.name`, notch height) changed nothing, and a
  fullscreen attempt was letterboxed by the application's own portrait lock, so
  the landscape fullscreen condition could not be constructed at all. The APKs
  reference no `Lmiui/` class, so the decision does not read MIUI framework
  state; every `com.ss.android.ugc.aweme.pad_api` interface ships a
  `*Downgrade` fallback and the predicates are an `isPadABon` A/B flag and
  `isPadLandscapeMode`. `isInMultiWindowMode` appears in roughly twenty dex
  files, and Droidloom fixes the display to freeform, so a multi-window test can
  suppress a tablet layout before any device check runs. The identity override
  was removed: it bought nothing and made the device report a tablet it is not.
  Confirming either cause requires changing the window model, a display per task
  or no freeform, which affects every application.
- **Cell internet depends on the host's forwarding chains.** Droidloom adds
  exact-match accept rules for the cell to `ufw-user-forward` and
  `DOCKER-USER`, the two chains that forward traffic ahead of a host drop
  policy. A host that drops forwarded traffic from some other chain leaves the
  cell without internet until an equivalent rule exists.

## Application compatibility

- **Clash of Clans on the x86_64 desktop.** Version 18.600.5 rejected inherited
  Cuttlefish product identities (`02`), then rejected the ARM64 phone-specific
  `readlink` wrapper exported by Droidloom's compatibility library (`16`).
  The x86_64 library now leaves `readlink` to bionic; ARM64 behavior and SELinux
  compatibility operations are unchanged. With local truthful Droidloom
  system/product/system_ext identity overlays and the corrected library, the
  original Play Store APKs reached the rendered age-entry screen without either
  exception. The identity overlays are not yet part of general image assembly;
  this is local startup validation, not a released package fix or verified gameplay.
  Launch through Droidloom: direct diagnostic Android activity starts can render
  internally without registering a native desktop window.
- **Brawl Stars on the x86_64 desktop: local startup fix validated.** The
  original Play Store ARM64 build 69.252 previously exited during native startup.
  Its own helper uses ptrace around an ARM64 `BRK`. Local tracing identified
  missing `PTRACE_GETSIGINFO`/`PTRACE_GETEVENTMSG` support and software-signal
  metadata (`SI_QUEUE`) where the breakpoint requires `TRAP_BRKPT` and its guest
  PC. The local Digitalis development checkout fixes these query/metadata paths
  while preserving user-sent SIGTRAP payloads. The next missing operation was
  `PTRACE_GETREGSET`. Development support now reads/writes actual ARM64 GPRs
  and NZCV through the native-bridge state header at verified, fully materialized
  BRK stops; it rejects arbitrary JIT/asynchronous stops and unsupported regsets.
  Live tracing now confirms successful 272-byte get/set transfers and resume
  past both BRK stops. The first live regset build exposed an Android-only
  argument-width bug in its new caller-buffer copy; this has been corrected.
  A later core dump identifies a second architecture crossing: Brawl replaces
  the shared JNI `FindClass` table slot with an ARM64 callback, which x86 ART
  then calls directly. The local fix isolates the guest function table from the
  host table, keeping guest hooks guest-side. Ten focused regressions pass;
  the full host suite has 3743 passing tests, three skipped and zero failures
  (two additional disabled). After activation, the unchanged APK completed its
  activity launch, stayed alive, and the user confirmed it works. The mounted
  translator SHA-256 is
  `02b00904b92a7c3188189debc5883ca23a5073732508c65fbe159367e3ed67f8`;
  rollback configuration is retained under
  `/var/lib/droidloom/rollback/breakpoint-ptrace-02b00904/`.
  The existing Clash compatibility and identity overrides remain intact.
  These are local Digitalis working-tree changes and development overrides,
  not a released package fix, exhaustive gameplay validation, or proof that
  Waydroid/Houdini has the same underlying defect.
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
  exited with status 0. The Play Store licensing service was absent in that run.
  The optional x86_64 Google-app add-on now registers that service, and the user
  completed Play Store sign-in. NTE initially still failed because the migrated
  package lacked its requested normal `com.android.vending.CHECK_LICENSE`
  permission. Reinstalling its unchanged base APK with inherited splits restored
  the Google permissions without deleting data. It then connected to the
  licensing service and redirected to Play Store, which marked NTE incompatible.
  The APK requires GLES 3.2 and implies landscape, Wi-Fi and Bluetooth features;
  the desktop lacked their declarations. GLES and orientation reporting are
  corrected in the product and survived a desktop-cell restart with the license
  permission still granted. With local Wi-Fi/Bluetooth catalog declarations,
  a successful Google device check-in and Play cache refresh changed the listing
  to offer **Update from Play**. Completing that update registered
  `com.android.vending` as NTE's installer and added it to the Play library.
  NTE then stayed open, its licensing requests reached the reporting service,
  and the user confirmed it works without the previous visual error.
  Wireless hardware integration is not implemented by those local declarations;
  they remain active on the development desktop but are excluded from product
  defaults. This test does not isolate the individual catalog flags or establish
  sustained gameplay performance or general Play Integrity compatibility.
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
  archives with raw ext4 or EROFS base images; normal first activation requires
  fresh Android data. Account sign-in and opening Play Store have been verified
  on the Moto ARM64 developer cell. On the x86_64 development workstation, the
  user completed sign-in with Android 17 Services Framework/configuration and
  native Play Services 24.23.37 and Play Store 41.3.25 APKs. An offline migration
  used a separate copy of existing Android data, retaining the original for
  rollback; this does not relax the normal fresh-data activation guard.
  After sign-in, Google updated Play Services to 26.33.32 and Play Store to
  53.0.27, both targeting API 37. Existing apps may need an ordinary reinstall
  preserving their data and splits to acquire newly introduced normal Google
  permissions; NTE's licensing connection required this repair.
  That older Play Services build's `magictether.host.TetherListenerService`
  crashed because the desktop lacks a Wi-Fi hotspot service. It was disabled
  locally for Android user 0. The updated Google core services survived the
  subsequent restart without a matching crash; YouTube's separate boot-receiver
  crash remains present.
  Catalog availability, app installation, push delivery
  and integrity checks require separate validation on each target. Older vendor
  images omit touch features and report no GLES version, which can hide apps;
  see the capability checks in the optional Google-app build guide.
  The development desktop also omitted both multitouch declarations despite
  the input bridge supporting independent contacts. The product now declares
  `android.hardware.touchscreen.multitouch` and
  `android.hardware.touchscreen.multitouch.distinct`. A matching local override
  is active on that workstation, with `input.xml.before-multitouch` alongside
  it for rollback. Restarting Droidloom activated the flags but left Brawl Stars
  incompatible in Play Store. After clearing only Play Store's cache, stopping
  the Store process and reopening the listing, the user confirmed compatibility
  was resolved. This does not establish gameplay or integrity-check support.
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
- **An app that dereferences absent hardware crashed, and Bilibili was the
  case.** The device products declared only the features Droidloom implements:
  there was no `android.hardware.wifi`, camera, Bluetooth, GNSS or telephony
  declaration, and the Wi-Fi module in the images ships no `wificond`.
  `getSystemService("wifi")` therefore returned null, and an app that used the
  result without a null check died. Bilibili 9.13.0, the Xiaomi channel build,
  read the Wi-Fi MAC address for its device-id library that way and crashed
  with `NullPointerException: null receiver` in
  `com.bilibili.lib.biliid.api.internal`. Its earlier two-second `SIGKILL` is
  fixed: that came from a thread pool sized by `availableProcessors()`, which
  the cell's synthetic CPU topology now answers correctly, and a launch creates
  a task whose process now lives until this crash. Dexopt works as well:
  `cmd package compile -m speed` produced an 883 MB `base.odex` through
  `dex2oat64` on eight threads with no `dex2oat32` attempt.
  Declaring `android.hardware.wifi` in the vendor permissions is the fix, and
  the device products now carry it. SystemServer starts `WifiService` behind the
  declaration and the AOSP default implementation answers without a radio:
  `service list` reports `wifi: [android.net.wifi.IWifiManager]` and
  `wifiscanner`, `pm list features` reports the feature, and Bilibili renders
  its video pages with nothing in the crash buffer. An earlier note here claimed
  the declaration would only trade the null for a missing binder service and
  that app parity needs a real Wi-Fi backend; sheng measured the opposite, and
  the same declaration is what Waydroid ships in its vendor image
  (`android.hardware.wifi.xml` with the identical `<feature>` element) although
  its guest has no radio either. Scanning and connecting still fail, so this
  does not make Wi-Fi work for apps that need a connection, and the experimental
  location and Bluetooth declarations above remain device-local tests that are
  not enabled in the release product.

## ARM64 translation accuracy findings

These correctness defects must be addressed independently of whether they
explain NTE's rendering. Source fixes have passed host validation in the separate
Digitalis checkout and are active on the development desktop through the
Android file-override mechanism. The audited
Digitalis source revision is `3ba3e75` in the local translation checkout; source
paths below are relative to that checkout, not to Droidloom. Preserve these
findings when updating the translation source.

Validation: the source-built `berberis_arm64_host_tests` completed 3,736 tests:
**3,733 passed, 3 skipped, 0 failed** (two additional tests are disabled upstream).
All ten new `FpRoundingRegression*` tests passed. They execute decoded guest
instructions with FPCR set by guest MSR, compare against literal expected
encodings, and restore the host floating-point environment after each test.
The heavy tier's existing unsupported vector FP64 FRINT and half FCVTN forms
are checked for explicit fallback; the lite tier is tested for actual JIT
execution of those forms. Scalar FP64-to-FP16 intentionally falls back from
both JITs to avoid double rounding. The skips are two no-F16C-only checks on
this F16C-capable host and an opt-in large-address-space reservation test.

The implementation and tests are working-tree changes in
`.work/translation/digitalis`, based on `3ba3e75`; they are not part of the
unchanged upstream source pin. Build with `--native-bridge-source` as documented
in [BUILDING.md](BUILDING.md). The Android NativeBridge target also built
successfully. Installation uses the existing 0.1.0-17 package pair and GApps
images, with two entries in `/etc/droidloom/cell.json` overriding
`/system/lib64/libberberis_arm64.so` and `/system/etc/droidloom-native-bridge.json`
from `/usr/lib/droidloom/local-overrides/fp-rounding-025fcee7/`.
All other inventoried NativeBridge build outputs matched the prior installation.
The activated library SHA-256 is
`025fcee736709ff2d62f45225fe41148414a44859043716fdcda2a7306ec629f`.
Droidloom was restarted and NTE launched at 2560x1440; its process mapped the
installed override's inode and its namespace-visible library matched that hash.
The user subsequently reported substantially improved graphics. This is user
acceptance evidence for the combined change, not attribution to any one fix.
The existing 64 GiB data disk, game assets and Google account were retained.
The pre-install cell configuration is backed up at
`/var/lib/droidloom/rollback/fp-rounding-025fcee7/cell.json`.
To roll back this activation, stop Droidloom, remove these two override entries
(or restore that backup if no later configuration changes must be retained),
then start Droidloom. The packaged translator remains available underneath.

### TR-001: half-precision conversion truncates in interpreter paths

**Fixed; host regressions passed; installed and active via local override.**
The working fix replaces truncation with integer-based binary64-to-binary16
rounding using all four FPCR modes. FP32 inputs widen exactly to that helper;
FP64 inputs convert directly, avoiding a second rounding through FP32. The
scalar integer and vector FCVTN/FCVTN2 paths now use the same helper. Both JITs
decline scalar FP64-to-FP16 conversion so the exact interpreter handles it.

Original defect:
`interpreter/arm64/interpreter.h`, `FpSingleToHalf`, drops low mantissa bits
instead of rounding to nearest-even. The same file explicitly distinguishes
this helper from `FpSingleToHalfRN`. Scalar single/double-to-half conversions
and several FP16 arithmetic paths call the truncating helper. For example,
`AdvSimdThreeSame` uses it for half-precision FADD, FSUB, FMUL and FDIV.
The corresponding FP16 JIT arithmetic in
`lite_translator/arm64_to_x86_64/lite_translator_simd_three_same.inc` narrows
with `VCVTPS2PH` immediate zero, which selects nearest-even. The two paths can
therefore produce different finite results under the default rounding mode.

The original helper was extracted verbatim and compiled in an isolated host
diagnostic, using hardware F16C nearest-even conversion as an independent
reference. No translator source or installed binary was modified:

| FP32 input | Interpreter helper, FP16 bits | Nearest-even reference |
| --- | --- | --- |
| `1.000732421875` | `0x3c00` (1.0) | `0x3c01` (1.0009765625) |
| `-1.000732421875` | `0xbc00` (-1.0) | `0xbc01` (-1.0009765625) |
| `65520` | `0x7bff` (65504) | `0x7c00` (+infinity) |

The first input is the exact sum of two representable FP16 inputs, `1.0` and
`0.000732421875`, so this is relevant to arithmetic as well as conversion.
Follow-up must cover scalar/vector conversions and arithmetic, rounding ties,
subnormal and overflow boundaries, and non-default FPCR modes against an
independent architectural reference. Passing interpreter-versus-JIT tests alone
is insufficient where both paths can share an error.

### TR-002: interpreter FRINTN follows the ambient rounding mode

**Fixed; host regressions passed; installed and active via local override.**
A fixed nearest-even helper now serves scalar and vector FRINTN. The audit
also found scalar FRINTI/FRINTX hard-coded to nearest-even in both JITs; these
now use MXCSR's guest rounding mode. FRINTI suppresses inexact while FRINTX
retains its inexact behavior. The tests distinguish all three instructions
across FP16, FP32 and FP64, signed zero, ties and all four FPCR modes.

Original defect: `interpreter/arm64/interpreter.h` implemented scalar FRINTN with
`std::nearbyint` and the vector `kFrintnV` path with `nearbyintf`/`nearbyint`.
Those functions follow the host rounding mode, while FRINTN requires a fixed
nearest-even result independently of FPCR's rounding selection. The same
interpreter programs the host MXCSR rounding mode when the guest writes FPCR,
so assuming that nearbyint always operates in its default mode is invalid.

Reproducer: select round-down (`FE_DOWNWARD`, corresponding to ARM FPCR RMode
`10`) and evaluate the interpreter expression for `2.75`. It returns `2`;
fixed nearest-even returns `3`. An isolated host check reproduced both results
using `std::nearbyint` and SSE `ROUNDSS` immediate zero. The vector JIT in
`lite_translator/arm64_to_x86_64/lite_translator_simd_two_reg_misc.inc` explicitly
selects immediate zero for FRINTN, so interpreter and JIT also disagree here.
Audit scalar and vector widths, positive/negative inputs and ties under every
FPCR rounding mode; distinguish FRINTN from FRINTI/FRINTX, which intentionally
use the current rounding mode. This reproducer tests the source operation in
isolation, not an end-to-end decoded guest instruction.

### TR-003: FP16 JIT arithmetic narrowing ignores guest rounding mode

**Fixed; host regressions passed; installed and active via local override.**
FP16 narrowing in the lite JIT and the heavy JIT's shared narrowing helpers now
uses `VCVTPS2PH` immediate four, selecting MXCSR's synchronized guest rounding
mode. This includes scalar/vector arithmetic and FP32-to-FP16 conversion.
The conversion rounding selection follows the
[Arm instruction specification](https://documentation-service.arm.com/static/67e40f3398aa3c3b6eea6a85)
and the immediate-bit behavior in the
[Intel instruction reference](https://www.intel.com/content/dam/www/public/us/en/documents/manuals/64-ia-32-architectures-software-developer-vol-2c-manual.pdf).
Tests use literal expected encodings rather than interpreter/JIT agreement as
their oracle. Broader composite-operation accuracy remains a separate audit
item below; fixing the final rounding mode does not prove every FP16 operation.

Original defect: FP16 vector FADD/FSUB/FMUL/FDIV in
`lite_translator/arm64_to_x86_64/lite_translator_simd_three_same.inc` widen to
FP32, perform SSE arithmetic, then use `VCVTPS2PH` with immediate zero to narrow
to FP16. The narrowing therefore always uses nearest-even, even when the guest
FPCR selects a different rounding direction. Unlike FRINTN, these arithmetic
instructions must honor that rounding selection. Fixing only TR-001's
interpreter helper would not resolve this separate JIT defect.

Reproducer: under guest round-up, the exact FP32 result of adding the
representable FP16 values `1.0` and `0.000244140625` is `1.000244140625`.
Narrowing with the JIT's immediate zero yields `0x3c00` (1.0); rounding upward
yields `0x3c01` (1.0009765625). An isolated F16C check reproduced this with
immediate zero versus immediate four (use MXCSR) after selecting `FE_UPWARD`.
Audit all FP16 narrowing sites and FPCR modes, including conversions and
double-rounding boundaries; do not treat switching one immediate as a complete
architectural fix. This is an operation-level reproducer, not an executed NTE
trace or full translator regression test.

### Remaining floating-point audit items

The FP16 fused multiply-add paths still narrow an FP64 result through FP32 in
several places, including `FpDataProc3` in the interpreter and both JITs.
That intermediate can hide which side of an FP16 midpoint contains the exact
result. A concrete candidate to execute is `FMADD H0,H1,H2,H3` with input bits
`H1=0x3e00` (1.5), `H2=0x3c01` (1.0009765625), `H3=0x8001` (-2^-24): the exact
result lies just below the midpoint between `0x3e01` and `0x3e02`, but FP32
nearest-even rounds it onto the midpoint. This is a source/arithmetic finding;
it has not yet been validated through a decoded instruction. Keep it open
independently of TR-001 through TR-003 and NTE.

The new value-rounding regressions do not establish full FPCR/FPSR conformance:
DN, AHP, FZ16, signaling NaN behavior and cumulative exception flags still need
dedicated coverage. The existing FPCR-to-MXCSR mapping explicitly leaves some
of those controls unimplemented. Do not describe these fixes as complete ARM
floating-point emulation.

### NTE investigation status

After activation of TR-001 through TR-003, the user reported that NTE looks
substantially better, with sharp UI and otherwise good graphics. Three remaining
visual observations are tracked below. Their causes remain **unidentified**;
no instruction trace links an individual translator defect to these pixels.

**NTE-GFX-001 — stepped face shading (unconfirmed defect).** The user-provided
face crop shows discrete shading boundaries around the cheek/jaw and describes
reduced color accuracy despite otherwise sharp rendering. Preserve the
distinction between observed banding and its cause: material/shader precision,
lighting or color-buffer quantization, a shading ramp, and intentional cel
shading remain candidates. A screenshot cannot establish texture compression
or the precision of the underlying arithmetic. Epic documents that
[mobile material precision can cause rendering artifacts](https://dev.epicgames.com/documentation/unreal-engine/materials-for-mobile-platforms?application_version=4.27),
but this does not establish which material variant NTE uses.

**NTE-GFX-002 — stationary noise in backgrounds/3D environment (unconfirmed defect).** The second
user-provided image shows a grain-like texture across the dark and magenta
Info-menu background while text and icons stay sharp. The user subsequently
confirmed that the noise is stationary and also part of the 3D environment;
do not restrict the investigation to a menu overlay or ask again whether it
animates. This may be an authored texture or static grain/dither pattern;
texture sampling/decoding and material
arithmetic remain alternatives. Do not assume it shares GFX-001's cause or
identify it as CPU translation failure from this image alone. Useful follow-up
evidence is a matching known-good Android scene or a trace identifying the background texture/shader
and render-target formats. The reference images remain user-owned screenshots
outside Git (`Screenshot-1788966896-889.png` and
`Screenshot-1788967333-627.png`).

**NTE-GFX-003 — blurred foreground character (unconfirmed cause).** In the
user's `Screenshot-1788967512-437.png`, the character holding a phone has soft
face, hair and clothing detail while the adjacent phone-menu UI stays crisp.
The user suspects incorrect depth calculation causing depth-of-field blur.
Track that as a hypothesis, not an established depth-buffer defect. The
selective softness makes the game's 3D rendering/post-processing a useful
first place to investigate. Distinguish wrong depth reconstruction or sampling
from a wrong focus-distance/camera parameter, intentional camera behavior,
temporal filtering, motion blur, or reduced resolution in the 3D render pass.
The screenshot alone does not select between them. A future controlled test
should isolate the DOF pass before changing depth/projection code, and compare
the same pose/camera with a known-good Android rendering where possible.
Do not merge this finding with face banding or stationary noise without
evidence of a common cause. No DOF setting or depth calculation has been
modified as part of documenting this report.

Initial follow-up read 218 retained per-app log lines, consisting of translator
dispatch/translation counters without a shader, format or precision diagnostic.
The inspected Droidloom compositor shader uses `mediump` texture sampling and
alpha multiplication; its declaration alone neither demonstrates reduced
precision on this GPU nor attributes either observation to that compositing
path. No rendering setting, shader, translator mode or runtime state was
changed during this follow-up, and no screenshots were taken by the agent.

Working differential for these residual effects: keep CPU translation, the
NativeBridge graphics-call boundary, GPU shader behavior, and game-selected
device profiles distinct. In the inspected GLES proxy,
`glShaderSource`, `glGetShaderPrecisionFormat` and `glUniform1f` use typed
host-call trampolines. ARM64 CPU instruction translation does not itself
execute the GPU shader; however, incorrect CPU-computed matrices, focus
parameters, material constants or argument transfer could feed wrong inputs
to an otherwise correct GPU implementation.

The inspected Mesa 26.1.7 RadeonSI source advertises FP16 shader capabilities
in `si_get.c` and has explicit mediump-I/O lowering in `si_nir_mediump.c`.
That proves support, not that an affected NTE shader actually takes that path.
The [GLES shading specification](https://registry.khronos.org/OpenGL/specs/es/3.2/GLSL_ES_Specification_3.20.html#precision-and-precision-qualifiers)
permits implementation differences within defined precision requirements.
Unreal also supports [GPU/driver-specific Android device profiles](https://dev.epicgames.com/documentation/unreal-engine/customizing-device-profiles-and-scalability-for-android?application_version=4.27);
NTE's selected profile and material variants have not been observed. Thus a
GPU-dependent result need not establish a driver defect. The user's improvement
after the isolated translator-library update keeps CPU translation a credible
candidate; it does not assign the remaining symptoms to that layer.

Discriminating tests, not yet performed: inspect the active profile and affected
shaders/target formats; compare guest execution tiers with the same scene and
GPU settings; or capture the graphics API inputs after NativeBridge and replay
the same supported workload on another backend. Differences between execution
tiers would implicate translation; differences between compatible GPU replays
with identical inputs would implicate shader/driver behavior or portability.
Agreement alone would not prove either implementation correct. A full-precision
shader experiment can test precision sensitivity but cannot by itself prove a
GPU bug, and disabling DOF can localize blur without proving bad depth values.

Historical baseline before activation: NTE was closed during the original
read-only investigation. Its latest retained exit was `EXIT_SELF`, status zero, rather
than a recorded native crash. The installed `libUnreal.so` matches the inspected
local copy by SHA-256:
`ce4486e04dca7978cbdda8584944c061494926f7425246b3bdbb682213997850`.
A static disassembly search found 12 FP16 arithmetic-looking instruction
encodings in that library, including `fadd v14.4h, v27.4h, v17.4h` at virtual
address `0xc1777a4`. Such encodings do not establish that those bytes are
reachable executable code rather than embedded data, or that NTE executes a
faulty translator path. The inspected surrounding bytes contain many unknown
instructions and unrelated instruction families, making embedded data a
plausible explanation for this match. No dynamic instruction trace was collected.

Android's compositor reports Radeon `gfx1201`, radeonsi/ACO, GLES 3.2 and Mesa
26.1.7. Earlier NTE memory maps contain native Mesa EGL/GLES libraries and ARM64
graphics API proxies; loaded Vulkan proxy libraries do not prove that Vulkan
was selected for game rendering. No GPU reset/fault was found in the retained
kernel log interval inspected. This does not rule out shader compiler, graphics
API bridge, or silent CPU translation errors. The recent per-app log retained
translation counters but no diagnostic identifying a rendering failure.

During the original read-only investigation, an existing host test binary ran
a focused floating-point selection: 159 passed,
one skipped because the tested host supports F16C. Those results do not cover
all conversion paths and do not invalidate the independent TR-001 reproducer.
Local reproduction sources and raw logs remain ignored under `.work/`; the
substantive findings and reproduction inputs are recorded here so they survive
workspace cleanup. No renderer switch, settings change or deployment was
performed during that investigation or the subsequent source-fix validation.
The subsequent source fixes and full-suite results are recorded above.

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
- **Android task profiles lose CPU and I/O placement on cgroup-v2-only hosts.**
  Android expresses CPU sets and I/O priorities by joining the legacy `cpuset`,
  `cpu` and `blkio` hierarchies. A host that already owns those controllers in
  cgroup v2 cannot provide the v1 hierarchies, and the sheng target is such a
  host: Droidloom reports both backends unavailable (`blkio` busy, `cpuset`
  invalid) before Android starts. The cell now removes those joins from the platform's
  task profiles instead of letting each one fail, so services start without
  `failed to set task profiles` and keep their remaining actions (memory,
  freezer, scheduler policy, timer slack). What is missing is the placement
  itself: SurfaceFlinger and applications are not pinned to capacity classes and
  I/O priorities do not apply, so the host scheduler places every cell thread.
  A cgroup v2 backend that declares `cpuset` next to the existing `memory` and
  `freezer` controllers, creates Android's `apps`/`system` sub-hierarchies and
  expresses the same groups through the v2 controller is the follow-up.
- **The tablet cell's density comes from the product, because the panel reports
  no physical size.** SurfaceFlinger takes the density of an internal display
  from `ro.sf.lcd_density` and falls back to its TV density (213 dpi) when the
  property is unset, which is what sheng ran with until the tablet product
  declared `ro.sf.lcd_density=320`. The panel is a 12.4-inch 3048x2032 display
  (about 295 ppi) with no EDID, so nothing else in the stack can derive the
  right scale; 320 is the density Android's own rule picks for that pixel
  density. A per-user `droidloomctl dpi` override still outranks the product
  default and persists in the Android data image.
- **The cell's synthetic sysfs exposes CPU topology but no frequency metadata.**
  `/sys/devices/system/cpu/{possible,present,online}` report the CPUs the cell
  actually inherits, which is what `get_nprocs()` and Java's
  `availableProcessors()` read. Per-CPU directories, `cpufreq`, `cpu_capacity`,
  idle states and cache topology remain absent, so applications that classify
  cores by frequency or capacity still see a flat, unclassified machine.

## Performance review targets

These findings describe the current source, including working-tree changes;
they are not measurements of an installed build. Priorities indicate where to
investigate first, not measured shares of CPU time or latency.

1. **High: sustained framework input overload can still lose input transitions.**
   Source now uses a bounded 16,384-record input lane and an independent
   256-command control lane. Window bounds/focus/close commands are serviced
   ahead of queued motion; repeated bounds updates coalesce, and pointer/pen
   motion coalesces under pressure while preserving contact and button edges.
   Framework socket sends time out after 100 ms; a failed send drops the socket
   and clears host-side pointer routes. Ten isolated production-module tests
   pass, including a stalled socket, priority controls, move coalescing, and
   send timeout. This has not yet been exercised in a sheng interaction soak.
   If the input lane fills with 16,384 critical transitions, the producer now
   returns an error instead of blocking, and that transition can be lost.
   Sources: [receive loop](../graphics/droidloom-composer-service/src/main.rs),
   [input sender](../graphics/droidloom-composer-service/src/input.rs),
   [socket transport](../graphics/droidloom-denial-ipc/src/lib.rs).

2. **Medium: app catalog repeatedly scans unchanged state.**
   The catalog queries Android and reconciles launcher files every 30 seconds
   by default. Icon keys and write-if-changed avoid some work, but the query and
   filesystem comparisons still run. Prefer package/user/locale change events
   with a full snapshot at connection or recovery. Measure idle wakeups and
   refresh cost across catalog sizes. Sources:
   [polling loop](../runtime/droidloom-applications/src/main.rs),
   [reconciliation](../runtime/droidloom-applications/src/lib.rs).

3. **Medium: notification changes resend the full active snapshot.**
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
