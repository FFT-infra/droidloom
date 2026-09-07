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
