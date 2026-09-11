# Tools

## Main workflows

| Tool | Purpose |
| --- | --- |
| `droidloom-package` | Build binaries and pacman packages in rootless Podman; test package lifecycle in a disposable container. See the [package guide](../packaging/arch/README.md). |
| `droidloom-update` | Rust source builder and transactional developer installer. Supplies the package builder's Android build and assembly implementation. See its [guide](droidloom-update/README.md). |
| `droidloom-source` | Materialize pinned sparse AOSP sources; a library dependency of the updater. |
| `droidloom-image` | Prepare the pinned Android base; a library dependency of the updater. |
| `droidloom-gapps` | Import a local LiteGapps archive, derive optional raw ext4 images and build a pacman data package. See [optional Google apps](../docs/BUILDING.md#optional-google-apps). |
| `droidloom-mesa` | Prepare pinned Mesa sources and patches; a library dependency of the updater. |

The package builder is the distribution entry point. The source updater is an
alternative developer installation; it cannot overwrite a pacman installation.
Do not assemble a preview from unrelated component builds.

## Specialist helpers

These are not needed to install or build the x86_64 pacman preview. They remain
because the Rust package workflow does not replace their ARM64, diagnostic or
source-generation roles.

| Tool | Retained purpose |
| --- | --- |
| `droidloom-soong-core-smoke` | Focused Android builds, including ARM64 and individual framework components. |
| `droidloom-vendor-image-smoke` | Standalone vendor-image build, including ARM64. |
| `droidloom-navigation-services` | Services-JAR provenance used by focused Soong and ARM64 deployment scripts. |
| `droidloom-systemui-stubs` | Regenerate SystemUI Binder adapters when pinned AOSP interfaces change. The Rust builder checks these adapters. |
| `droidloom-dmabuf-probe` | GLES/DMA-BUF/explicit-sync diagnostic; does not validate a complete Android app. |
| `droidloom-translation-bench` | Repeat Digitalis host correctness tests, time seven guest workloads across three translation modes, and compare saved runs. See [translation benchmarking](../docs/BUILDING.md#translation-stress-and-performance-benchmarks). |
| `droidloom-inspect.rs` | Optional read-only diagnostics for the development workstation's u1000 cell; fixed `features`, `properties`, and `configuration` modes. |

The Rust updater and package workflow own x86_64 installation, init projection
and Mesa builds. New orchestration should be Rust; legacy scripts are not a
template for new tooling.

The workstation diagnostic helper can be built with
`rustc --edition 2024 -O tools/droidloom-inspect.rs -o /tmp/droidloom-inspect`.
Its optional administrator installation is `/usr/local/sbin/droidloom-inspect`,
owned by root with mode 0755. A sudoers rule may grant UID 1000's user the three
exact mode arguments above; validate it with `visudo -cf` before installation.
Use `sudo -n /usr/local/sbin/droidloom-inspect features` to inspect the live
feature list. The helper takes no PID, path, shell or extra command arguments,
selects init only from `/run/netns/droidloom-u1000`, pins its process resources,
clears the execution environment and bounds commands to 15 seconds.
It is deliberately workstation-specific and is not installed by the packages.
Remove `/etc/sudoers.d/droidloom-inspect` to revoke the local passwordless grant.

The package builder also provides `ship prepare`, `ship publish`, and
`runner setup|start|stop|status` for the [GitHub shipping workflow](../docs/BUILDING.md#shipping-with-github-actions).
