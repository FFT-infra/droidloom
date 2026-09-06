# Tools

## Main workflows

| Tool | Purpose |
| --- | --- |
| `droidloom-package` | Build binaries and pacman packages in rootless Podman; test package lifecycle in a disposable container. See the [package guide](../packaging/arch/README.md). |
| `droidloom-update` | Rust source builder and transactional developer installer. Supplies the package builder's Android build and assembly implementation. See its [guide](droidloom-update/README.md). |
| `droidloom-source` | Materialize pinned sparse AOSP sources; a library dependency of the updater. |
| `droidloom-image` | Prepare the pinned Android base; a library dependency of the updater. |
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

The Rust updater and package workflow own x86_64 installation, init projection
and Mesa builds. New orchestration should be Rust; legacy scripts are not a
template for new tooling.
