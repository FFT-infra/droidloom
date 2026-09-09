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

The builder reserves two logical CPUs. To limit compilation further:

```console
cargo run --locked -j 1 -p droidloom-package -- build --jobs 4
```

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
The first implementation accepts raw ext4 base partitions, matching the ARM64
developer runtime. The standard desktop package uses EROFS and is not yet an
input to this builder. An ARM64 archive cannot be used with an x86_64 image.

Host tools are `bsdtar`, `xz`, `e2fsprogs`, Android SDK `aapt2` and `apksigner`,
and a Java runtime. Pacman archives use the existing rootless Podman Arch builder
and its `makepkg`/`fakeroot` tools. Build as an ordinary user; assembly uses `debugfs` on private image
copies and does not mount images, install software or start services.

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

The default selects Google Services Framework, Play Services and Play Store.
`--sync-adapters` also selects Google Contacts and Calendar sync. Configuration
is filtered to selected applications and their requested permissions, with
privileged allowlists on the apps' own partitions. Pixel feature declarations,
phone setup wizard configuration and unrelated applications are excluded.
`import-report.json` lists selected files and excluded upstream files. Original
license comments and the archive's license notice are retained.

Only `product` and `system_ext` are derived. Assembly checks every retained
file's contents, symlink target, UID/GID, mode, mtime and xattrs and checks ext4
integrity. The manifest binds the outputs to all three exact base image hashes,
the source archive, SDK, architecture and verified APK signers. A failed build
does not publish the destination. Keep images, APKs and packages outside Git.

For a separately maintained ARM developer runtime:

```console
cargo run --locked -j 1 -p droidloom-gapps -- package \
  --addon .work/gapps-arm64 --version 4.9.20260513 --standalone \
  --output dist/droidloom-gapps-4.9.20260513-1-aarch64.pkg.tar.zst
```

For pacman-managed raw-ext4 deployments, replace `--standalone` with
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

Offline checks do not prove account sign-in, Play Store installation, push
delivery or Play Integrity behavior. Validate those on the intended device,
including repeat boots and package changes, before considering the integration
ready for use. Google certification and redistribution rights are separate from
successful image assembly; see [third-party scope](../THIRD_PARTY.md).

Play Store also filters its catalog using Android's reported capabilities.
The vendor product declares the basic touch interface provided by Droidloom's
input bridge. The framework includes that routed input when computing display
configuration, since physical input devices remain private to Denial. The ARM64
Mesa product advertises OpenGL ES 3.2 (`ro.opengles.version=196610`), matching the
verified Moto rendering path. When validating another graphics backend, compare
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
