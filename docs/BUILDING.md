# Build the pacman packages

The Rust package builder compiles Droidloom's host programs and modified Android
components, assembles the Android image, and produces a matching runtime/image
pacman package pair. It uses pinned upstream Android base and toolchain inputs;
it does not rebuild every upstream Android component from source.

## 1. Prepare an x86_64 build machine

The supported package workflow uses rootless Podman with an Arch build container.
Install Rust/Cargo with a C linker, Git, Podman and rsync. On Arch / Omarchy:

```console
sudo pacman -S --needed rustup gcc git podman rsync
```

Sudo is needed only to install those host tools and dependencies under `/usr` and
update pacman's database. If you already have a working Rust installation, keep
it; you do not need to replace it with rustup. The checkout pins Rust 1.93.0.

Allow **120 GiB free** for a fresh build, including sources, toolchains, compiler
outputs, downloads and temporary package staging. The workflow has been exercised
on a machine with 32 GiB RAM and 16 logical CPUs. Retained build files are smaller
than peak space requirements: keep free space for staging and compression even
when rebuilding from cache.

Verify rootless Podman works as your ordinary user:

```console
podman info
```

Resolve any rootless Podman setup error before building; do not switch to a sudo
build. Internet access is required for the initial container and pinned inputs.

## 2. Get the source

```console
git clone https://github.com/denialwm/droidloom.git
cd droidloom
```

For a published preview, check out its corresponding source tag or commit before
building. The `main` branch may contain changes beyond that preview. Package
version and release are defined in `packaging/arch/version.json`.

## 3. Build binaries and packages together

```console
cargo run --locked -j 1 -p droidloom-package -- build
```

Run as your ordinary user, without sudo. The Rust tool performs compilation and
invokes makepkg inside the rootless container; no manual binary build or separate
packaging script is required. Shell is used only at required upstream interfaces
and in PKGBUILD functions.

The builder limits jobs to the available logical CPU count minus two. To limit compilation further:

```console
cargo run --locked -j 1 -p droidloom-package -- build --jobs 4
```

Container CPU pinning is opt-in, so the default build works without rootless
`cpuset` delegation. To restrict the container to a CPU subset as well:

```console
cargo run --locked -j 1 -p droidloom-package -- build --cpuset
```

This requires the host to delegate the `cpuset` controller to rootless Podman.
Without it, the job limit still leaves capacity for two logical CPUs and nested
Android builds retain their own CPU affinity control.

Outputs are written to `dist/arch/<version-release>/`. For revision 0.1.0-7:

```text
dist/arch/0.1.0-7/droidloom-runtime-0.1.0-7-x86_64.pkg.tar.zst
dist/arch/0.1.0-7/droidloom-image-0.1.0-7-x86_64.pkg.tar.zst
```

The command prints the exact installation command when finished. Install both
packages by following [INSTALL.md](INSTALL.md).

Normal builds fetch [Teto](https://github.com/denialwm/teto), the Translation
Engine with Target Optimization, at exact commit
`917021cf3ecd077eb1581146a79d2265011755b8`, pinned in
`android/manifest/native-bridge-lock.json`. It includes the memory-reservation,
FPCR rounding, breakpoint/ptrace and JNI-table isolation fixes. No local
development checkout is needed for these fixes.

For further Teto development, use a separate Git checkout whose `origin`
matches the repository URL in that lock and whose history contains the pinned commit.
Commits on top of that base and uncommitted edits are both supported:

```console
cargo run --locked -j 1 -p droidloom-package -- build --jobs 8 \
  --native-bridge-source /absolute/path/to/digitalis
```

The checkout is mounted read-only and copied into the generated AOSP workspace.
The installed `/system/etc/droidloom-native-bridge.json` records its base, actual
commit and hashes of all working source files. Keep any unpublished development
edits needed to reproduce such a package. Omit the option for the locked Teto
source. Existing caches with an old origin URL, an older revision, or
development edits deliberately fail verification instead of silently discarding
them. Use a fresh build workspace when switching to the new pin. For an existing
development clone, preserve the DigitalisX64 remote as `upstream`, set `origin`
to the lock's Teto URL, and fetch the pinned commit before building.

Teto's `droidloom` branch includes `android-mmap-noreserve`,
enabled by Droidloom's `ro.berberis.flags`.
It gives private anonymous guest mappings Android's expected
overcommit behavior without modifying the desktop's global memory policy. Strict
host overcommit policy still applies. The pin adds branding, licensing documents
and change notices to the previously validated translator code (3743 host tests
passed, three skipped; Brawl Stars
startup confirmed by the user). Pinning source does not publish new packages or
replace existing installed images and local library overrides. Teto derives from
Digitalis/AOSP Berberis; upstream history and notices remain intact. Existing
`berberis_*` ABI names, build targets, source paths, and the local
`.work/translation/digitalis` directory remain unchanged for compatibility.

### Translation stress and performance benchmarks

`droidloom-translation-bench` is a Rust command-line runner for Digitalis's
existing `berberis_arm64_host_tests`. Run it on an **x86_64 Linux host** as an
ordinary user. It does not install packages, start Android or restart a session.
An ARM64 phone runs ARM64 natively and cannot measure this translation path.

Before measuring a translator edit, rebuild **the host test binary**, not just
`libberberis_arm64.so`: the host tests statically link the translator. In the
already prepared Android build environment, use the updater's focused target:

```console
DROIDLOOM_NATIVE_BRIDGE_SOURCE=/absolute/path/to/digitalis \
  cargo run --locked -j 1 -p droidloom-update -- \
  native-bridge-build --work /absolute/path/to/prepared-build --tests
```

Use the paths visible inside the rootless builder when running there (`/build`
for the prepared build and the mounted Digitalis source path). This is the
existing source build workflow and requires its prepared dependencies. A package
build alone does not request the host tests. Keep the test executable in its
Soong output tree, alongside its test data and `../../lib64` support libraries.
Each benchmark run makes and verifies a private copy of these inputs under its
output directory, so a later rebuild cannot mix executables within a run. It
also retains the benchmark source and writes `inputs.json` before testing;
only `result.json` denotes a completed run. Avoid concurrent builds anyway,
because their CPU and memory traffic can distort the measurements.

From the Droidloom checkout, save a baseline (choose an available, idle logical
CPU and keep it the same for subsequent runs):

```console
cargo run --locked -j 1 -p droidloom-translation-bench -- run \
  --cpu 6 --label before --output .work/translation/bench-before
```

The default paths match this checkout's `.work/arch/build/android-out` host
test executable and `.work/translation/digitalis` benchmark source. Override
`--binary` and `--benchmark-source` for another build. The supplied benchmark
source must be the file used to build that binary; hashing it cannot prove that
a stale executable was rebuilt. Optional `--build-manifest` records the same
build's `target/product/droidloom_x86_64/droidloom-native-bridge-source.json`.
The executable hash identifies exactly what ran; the optional manifest is
supporting provenance, not independently verified linkage to that executable.

The runner first executes the complete non-benchmark correctness suite twice
in fresh processes, in its normal `two-gear` mode. These tests directly exercise
the interpreter, lite translator, heavy optimizer, decoder, guest ABI and kernel
emulation, including upstream differential fuzz cases. `--stress-repeats 10`
increases repeated stress; it does not generate new fuzz seeds or establish a
code-coverage percentage. Disabled and skipped tests remain visible in the logs
and correctness summaries. A failed test or timeout aborts the run before it can
publish `result.json`. Some upstream seccomp/ptrace tests require an ordinary
unsandboxed host process; an agent/container sandbox can cause failures. Do not
filter those failures away to manufacture a passing baseline.

After correctness passes, the runner measures each of these workload families
in `interpret-only`, `lite-translate-or-interpret`, and `two-gear` modes:

| Workload | What it exercises | Loop iterations per timed run |
| --- | --- | --- |
| Integer | Mixed integer arithmetic, bitwise operations and shifts | 3,000,000 |
| Branch | Comparisons and conditional branches | 3,000,000 |
| Call | Guest calls, returns and dispatch | 3,000,000 |
| NEON | Vector arithmetic | 3,000,000 |
| FP | Scalar double-precision arithmetic | 3,000,000 |
| Memory | Repeated loads/stores over a small hot buffer | 3,000,000 |
| Syscall | Guest `clock_gettime` emulation | 1,000,000 |

`--mode two-gear` selects a shorter, production-mode timing sweep.
`--samples 9` (the default) collects nine fresh-process results per case; each
result is the upstream median of five internal timings. Case order rotates
between sweeps. `--timeout-seconds 120` bounds each child process and preserves
logs on failure. All test processes inherit the selected CPU affinity and a
clean environment. Set translator flags explicitly with `--flags` when needed;
ambient `BERBERIS_*`, tracing and dynamic-loader overrides are removed.
The workload uses one logical CPU, leaving at least two physical cores outside
its affinity on this eight-core development host. To pin the runner's own
snapshotting/reporting work as well, first build it with
`cargo build --locked -j 1 -p droidloom-translation-bench`, then invoke
`taskset --cpu-list 6 target/debug/droidloom-translation-bench run --cpu 6 ...`.

After changing and rebuilding Digitalis, repeat with a fresh output directory:

```console
cargo run --locked -j 1 -p droidloom-translation-bench -- run \
  --cpu 6 --label after --output .work/translation/bench-after
cargo run --locked -j 1 -p droidloom-translation-bench -- compare \
  .work/translation/bench-before/result.json \
  .work/translation/bench-after/result.json --fail-on-regression
```

Comparison reports milliseconds, percentage time change and relative
interquartile range (IQR) for each mode/case. **Positive time change means
slower.** Changes beyond 5% are flagged by default; a row with more than 10%
IQR in either run is inconclusive. `--threshold` and `--noise-limit` adjust these
screening thresholds. These are heuristics, not statistical significance tests;
IQR is calculated over the saved process medians and does not expose variation
hidden inside each upstream median. With `--fail-on-regression`, exit code 2
means at least one non-noisy regression, 3 means noisy data without a detected
regression, and 1 means invalid input or execution failure. Without that flag,
performance findings are informational.

The comparison rejects changed machines, CPUs, kernel versions, recorded power
settings, mode/flag selections, benchmark source, work counts, support libraries
or test data and correctness summaries. It allows a changed test binary, which is the subject
of the comparison. Use the same build configuration, an idle host and consistent
power/thermal conditions. Pinning a CPU does not isolate it from other tasks or
its SMT sibling. Repeat unchanged builds to establish normal noise, then repeat
A/B or ABBA runs before accepting small gains. Reports and logs belong under
`.work/`, outside Git; existing result directories are never overwritten.

Scope: this stresses many paths but does **not** cover every ARM instruction or
every translator path. The timed kernels themselves use upstream `SUCCEED()`
rather than answer checks; correctness comes from the separate test gate, not
from their speed. The upstream warmup is only one loop iteration, so optimization
tier promotion can still occur inside the timed interval. These are guest-loop
elapsed times, not isolated translation-compilation costs or proven steady-state
heavy-JIT timings. The reported instruction counts are workload labels (the
branch case counts skipped instructions); the runner deliberately compares time
rather than treating the upstream MIPS number as an exact instruction rate.
Iteration counts are fixed in upstream source, not controlled by `--samples`.
Changing those kernels requires rebuilding the host tests and starting a new
baseline. Memory bandwidth, atomics under contention, Android API proxies,
graphics, app startup and cold translation need additional dedicated workloads
and app-level measurements. An optimizing-mode result can include fallback;
selecting that mode does not prove every instruction used the heavy optimizer.

## 4. Exercise package installation

```console
cargo run --locked -j 1 -p droidloom-package -- check --packages dist/arch/0.1.0-7
```

Use your build's output directory if its version differs. This runs actual pacman
dependency installation, setup, reinstall, removal and data-retention checks in
a disposable rootless Arch container. It needs no host sudo and does not restart
your desktop. It uses a simulated GPU entry for setup checks; app rendering must
be tested separately on a real Wayland desktop.

Add `--previous dist/arch/<older-version-release>` to include upgrade and downgrade
transactions against an older pair.

## Rebuilds and troubleshooting

For a change confined to host components, increase the release in
`packaging/arch/version.json`, then select the component instead of compiling
Android again:

```console
cargo run --locked -j 1 -p droidloom-package -- build --component droidloom-supervisor
```

This compiles the selected Cargo package and its Rust dependencies. Selecting
`droidloom-supervisor` updates `droidloomctl`, `droidloomd` and the supervisor
together. Other supported components are `droidloom-wayland`,
`droidloom-applications`, `droidloom-doctor`, and `droidloom-package-support`.
Repeat `--component` to select more than one. Without it, the command builds
the complete runtime and Android components as before.

A component build reuses unchanged files from the newest completed older package
pair with the same version and architecture in `dist/arch`. It produces a new
matching pair and records its baseline and selections in `build-summary.json`.
It does not use installed binaries or rebuild Android, Mesa or SurfaceFlinger;
extracting and repackaging the image still takes some time.

Use a full build for Android changes, changes spanning the host/Android protocol,
or changes to dependencies and package integration. Component builds reject
changed Cargo lockfiles, pinned Android inputs, runtime contracts and packaging
recipes. They also reject `--clean` and `--source-cache`. A completed full build
provides the baseline; builds made before component support need a full rebuild
to record their inputs. Keep that baseline's archives, summary and
`component-inputs.json` together. Unselected source edits are not included.

Rerun the same build command to reuse caches. Downloads, sparse AOSP sources and
outputs live under `.work/arch`; build attempts save `build-*.log` there.
`build --clean` removes this workflow's compiler outputs while retaining downloads.
An existing sparse AOSP source checkout can seed a build with
`build --source-cache /path/to/checkout`; installed binaries are not build inputs.

If staging fails with `No space left on device`, free disposable outputs and rerun
the builder. Do not distribute partial archives from a failed attempt. APKs,
packages, caches and local test artifacts must stay out of Git.

The fully empty-cache source-download path has not yet been rehearsed; see
[KNOWN_ISSUES.md](KNOWN_ISSUES.md). Build logs should accompany reports of failures.

## Desktop image policy

The Android 17 x86_64 package omits legacy VNDK 31–34 APEXes from its derived
`system_ext.img`. The pinned upstream inputs remain unchanged. The policy rejects
other vendor products, SDK versions, or an explicit legacy VNDK selection. `adbd`
is retained. Image derivation runs in one fakeroot session to preserve Android
ownership and security xattrs. Before staging, it re-extracts the rebuilt image
and compares every retained file’s contents, permissions, ownership, timestamps,
symlink target and extended attributes. Missing expected VNDK files or any other
content/metadata change fails the build. A full package build is required.

## Optional Google apps

`droidloom-gapps` builds an optional add-on from a **locally supplied LiteGapps
regular lite archive for Android 17/API 37**. The guest architecture comes from
the APK payload and base build properties, independently of the build host.
Both raw ext4 developer partitions and the standard desktop EROFS partitions
are supported. Every selected APK must contain the guest's native ABI or be
Java-only; Google apps do not depend on ARM translation on x86_64.

Host tools are `bsdtar`, `xz`, `e2fsprogs`, Android SDK `aapt2` and `apksigner`,
and a Java runtime. EROFS derivation also requires `erofs-utils`, `fakeroot` and
`attr`. Pacman archives use the existing rootless Podman Arch builder
and its `makepkg`/`fakeroot` tools. Build as an ordinary user; assembly uses `debugfs` on private image
copies and does not mount images, install software or start services.
EROFS assembly runs extraction and rebuilding in one fakeroot session to retain
Android ownership and security attributes without administrator access.

Inspect the selected archive, then pass its reviewed SHA-256 to the builder:

```console
cargo run --locked -j 1 -p droidloom-gapps -- inspect /path/to/LiteGapps-arm64-17.0.zip
cargo run --locked -j 1 -p droidloom-gapps -- build \
  --archive /path/to/LiteGapps-arm64-17.0.zip --sha256 <archive-sha256> \
  --base /path/to/active-image-set --output .work/gapps-arm64 \
  --aapt2 /path/to/aapt2 --apksigner /path/to/apksigner
```

`--base` contains `images/system.img`, `images/system_ext.img` and
`images/product.img`. Use the actual activated images, including previous
Droidloom derivations. `--system-ext /path/to/system_ext.img` selects a separately
derived input without modifying the base directory. APK signatures, package IDs,
SDK and native ABI are checked. The importer does not execute upstream installer
scripts or resign APKs. A supplied archive checksum establishes input identity;
it is not an independent endorsement of its publisher.

For x86_64, an Android 17 ARM64 archive can supply the Java-only Services
Framework and Android 17 configuration when native Play APKs are provided:

```console
cargo run --locked -j 1 -p droidloom-gapps -- build \
  --archive /path/to/LiteGapps-arm64-17.0.zip --sha256 <archive-sha256> \
  --base /usr/lib/droidloom/images --output .work/gapps-x86_64 \
  --play-services-apk /path/to/x86_64/GmsCore.apk \
  --play-store-apk /path/to/x86_64/Phonesky.apk \
  --aapt2 /path/to/aapt2 --apksigner /path/to/apksigner
```

Replacement APKs must have the same package ID and signing certificate as the
archive's APK, or a verified ancestor in its signed certificate rotation lineage.
Their actual minimum SDK and native ABI are checked and their hashes are recorded
in the manifest and import report. Older archive release labels alone do not
establish APK compatibility: only the two native APKs are selected, never older
Services Framework, platform libraries or permission XML. Configuration is
filtered against the replacement APKs' requested permissions. Validate the result
on Android 17 before relying on sign-in or other Google APIs.

On the development desktop, native Play Services 24.23.37 and Play Store 41.3.25
from the Android 15 x86_64 archive completed account sign-in with the Android 17
framework and configuration. This older Play Services build also starts a
hotspot listener that crashes when Android has no Wi-Fi service. The local
workaround disables only that component for Android user 0, from inside the
Android cell:

```console
/system/bin/pm disable --user 0 com.google.android.gms/com.google.android.gms.magictether.host.TetherListenerService
```

This is a desktop workaround, not a general default for devices with Wi-Fi.
Google subsequently updated Play Services to 26.33.32 and Play Store to 53.0.27,
both targeting API 37. The user confirmed NTE launches successfully after an
official **Update from Play**, following permission repair and a local catalog
compatibility experiment; see the
[app-service limitations](KNOWN_ISSUES.md#application-compatibility).

The default selects Google Services Framework, Play Services and Play Store.
`--sync-adapters` also selects Google Contacts and Calendar sync. Configuration
is filtered to selected applications and their requested permissions, with
privileged allowlists on the apps' own partitions. Pixel feature declarations,
phone setup wizard configuration and unrelated applications are excluded.
`import-report.json` lists selected files and excluded upstream files. Original
license comments and the archive's license notice are retained.

Only `product` and `system_ext` are derived. Assembly checks every retained
file's contents, symlink target, UID/GID, mode, mtime and xattrs and checks filesystem
integrity. The manifest binds the outputs to all three exact base image hashes,
the source archive, SDK, architecture and verified APK signers. A failed build
does not publish the destination. Keep images, APKs and packages outside Git.

For a separately maintained ARM developer runtime:

```console
cargo run --locked -j 1 -p droidloom-gapps -- package \
  --addon .work/gapps-arm64 --version 4.9.20260513 --standalone \
  --output dist/droidloom-gapps-4.9.20260513-1-aarch64.pkg.tar.zst
```

For pacman-managed deployments, replace `--standalone` with
`--base-package-version <version-release>` to require the exact matching
`droidloom-runtime` and `droidloom-image` packages and include the lifecycle hook.
Standalone packages require manually stopping Droidloom before every install,
upgrade or removal. They contain images and notices, not an updated supervisor.

The runtime must include `gapps_dir` support before activation. Install the
package with pacman only when ready; administrator access is needed to write
the package-owned directory and update the package database. While Droidloom is
stopped, add `"gapps_dir": "/usr/lib/droidloom/addons/gapps"` to its root-owned
cell specification. Package installation alone does not activate Google apps.
The supervisor verifies root ownership and every base/output hash before boot.
An incompatible or missing selected add-on fails startup without falling back.

**First activation requires fresh Android data.** Configure a separately
provisioned fresh `data_dir` (with its `data.img` and `metadata.img`), or enable
GApps before the installation's first Android boot. Existing data is never wiped.
The runtime records Google-app selection and signers in the cell's data image;
updates with the same selection/signers refresh only Google parser caches.
Changing selection or signers requires fresh data or a separately validated
migration. To disable GApps, stop Droidloom, remove `gapps_dir` and select fresh
data or restore a pre-GApps backup. Merely removing the package cannot undo
Google updates and account state in `/data`; startup rejects that mixed state.

A separately validated migration must also reconcile existing applications'
permission state. An app installed before GApps may request Google-defined
normal permissions yet retain `granted=false` after those permissions appear.
On the desktop, reinstalling NTE's unchanged base APK with
`pm install -r -p com.hottagames.nte --user 0 <installed-base-apk-path>` retained
its existing splits and data and restored `CHECK_LICENSE`, allowing it to bind
to Play's licensing service. This does not establish a Play license or repair
catalog incompatibility. Do not substitute a blanket runtime-permission grant
or reset application data.

Offline checks do not prove account sign-in, Play Store installation, push
delivery or Play Integrity behavior. Validate those on the intended device,
including repeat boots and package changes, before considering the integration
ready for use. Google certification and redistribution rights are separate from
successful image assembly; see [third-party scope](../THIRD_PARTY.md).

Play Store also filters its catalog using Android's reported capabilities.
The vendor product declares touch and distinct multitouch provided by Droidloom's
input bridge, which preserves independent contact IDs in Android MotionEvents.
The framework includes that routed input when computing display
configuration, since physical input devices remain private to Denial. The Mesa
products advertise OpenGL ES 3.2 (`ro.opengles.version=196610`), matching the
verified Moto and x86_64 AMD rendering paths. Both window orientations are
declared for freeform Android tasks. When validating another graphics backend, compare
the advertised version with SurfaceFlinger's actual GLES implementation.

From inside the Android cell, `cmd package list features` should include the
touch features and a nonzero `reqGlEsVersion`; `cmd activity get-config` should
report `finger` for a touch-enabled Droidloom product. A feature XML alone does
not correct the display's input configuration. After updating these boot-time
capabilities, restart the cell and refresh Play Store's cache. Successful account
sign-in does not guarantee that Google has refreshed its device profile or that
every app is compatible. Only declare capabilities the runtime implements;
Google certification and missing camera, microphone or sensor integration are
not repaired by adding feature names.
