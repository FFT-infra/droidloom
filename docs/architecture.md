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

## Wayland and Denial

The presenter requires `xdg-shell`, `linux-dmabuf` v4 feedback and
`linux-drm-syncobj` synchronization. Each task has a bounded, generation-scoped
target pool. Android Composer composes task layers into an available target;
the presenter attaches it to the task's Wayland surface and releases it back to
Android only after the compositor signals release.

Denial handles Droidloom windows through ordinary Wayland focus, input, tiling
and resize paths. No Droidloom compositor plugin or alternate window role is
required. Mobile integrations use the same surfaces and seat; the optional
[keyboard-dismissal extension](contracts/text-input-v1.md) provides feedback to
Android without changing ordinary text-input semantics.

The user service starts Android after presenter readiness and stops the cell
when the presenter stops. See [security requirements](threat-model-v1.md) for
the shared-kernel trust boundary.
