# Working on Droidloom

## Moto Edge 70 test device (.154)

- Device workspace: `/mnt/development/moto70edge`.
- Wi-Fi SSH: `root@192.168.1.154` (Moto Edge 70, `roadstr`, serial `ZY22MMG59D`).
- Use `/mnt/development/droidloom/.work/moto-ssh` from this checkout; the helper
  selects the workspace key and verifies the known host key.
- Host command: `.work/moto-ssh uname -a`.
- Android command: `.work/moto-ssh --android /system/bin/getprop sys.boot_completed`.
  Arguments are passed literally; Android commands need absolute paths.
- Before device work, read `/mnt/development/AGENTS.md`,
  `/mnt/development/denial-ace3/procedures.md`, and the device workspace's
  `WORKFLOW.md`. Follow their device identity and activation rules. Android
  image-only updates require a Droidloom restart, not a phone reboot.
- This is an ARM64 device; the x86_64 desktop pacman packages do not apply.

## Commands

- Build pacman packages: `cargo run --locked -j 1 -p droidloom-package -- build`
- Check packages: `cargo run --locked -j 1 -p droidloom-package -- check --packages dist/arch/<version-release>`
- Check Rust code: `cargo check --locked --workspace -j 1`
- Test packaging: `cargo test --locked -j 1 -p droidloom-update -p droidloom-package -p droidloom-package-support`
- Test runtime contracts: `cargo test --locked -j 1 -p droidloom-contracts`

Use focused tests for changed components. Keep two logical CPUs free across all
compiler processes; the Rust builder enforces this for nested Android builds.
Use Rust for new tooling; shell is reserved for required upstream interfaces and
PKGBUILD functions. Explain why administrator access is needed before requesting
it. Package builds use rootless Podman and need no host sudo.

## Useful files

- `README.md`: overview and starting points.
- `packaging/arch/README.md`: installation and source builds.
- `docs/README.md`: architecture, integration and contracts.
- `tools/README.md`: maintained entry points and specialist helpers.
- `tools/droidloom-package/src/main.rs`: container build and pacman packaging.
- `tools/droidloom-update/src/`: Android build, assembly and source updater.
- `runtime/`: lifecycle, CLI, catalog and package setup.
- `graphics/`: Wayland presentation and private Android transport.
- `android/manifest/`: pinned upstream inputs.
- `LICENSE` and `THIRD_PARTY.md`: licensing and upstream exceptions.

Keep packages, APKs, build outputs, caches and local test journals out of Git.
Update existing guides instead of adding dated plans. Documentation and ordinary
development tasks do not authorize installation, hardware operations or restarting
a user's graphical session.
