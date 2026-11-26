# Fedora Linux Packaging & Hardware Adaptation

Packaging definitions and host adaptations for **Fedora Linux 45** (`x86_64` and `aarch64`)
with KDE Plasma Wayland (`kwin_wayland`), SELinux (`Enforcing`), and `firewalld`.

## Contents

- [Containerfile](Containerfile): Rootless Fedora 45 builder image for compiling host binaries, `droidloom-update`, SELinux policy modules, and RPMs.
- [droidloom-image.spec](droidloom-image.spec): RPM spec packaging `/usr/lib/droidloom/{runtime,storage,images,current}`.
- [droidloom-runtime.spec](droidloom-runtime.spec): RPM spec packaging host binaries, systemd units, SELinux module, `firewalld` rules, and architecture-specific Fedora overrides.
- [selinux/droidloom_fedora.te](selinux/droidloom_fedora.te): Type Enforcement policy allowing `binder` IPC, `mac_admin`/`mac_override`, container entrypoint labels, and tmpfs/pseudofs association under `unconfined_t` / `unconfined_service_t`.
- [firewalld/zones/droidloom.xml](firewalld/zones/droidloom.xml) & [firewalld/policies/droidloom-egress.xml](firewalld/policies/droidloom-egress.xml): Outbound NAT/forwarding zone and policy for Droidloom veth interfaces (`dlh+`, `10.177.0.0/30`).
- [density-desktop.json](density-desktop.json): Sample `$XDG_CONFIG_HOME/droidloom/density-v1.json` setting `dpiPerScale: 160` so `1 Android dp` matches `1 Wayland logical pixel`.

## Host Adaptations on Fedora

### 1. SELinux Policy Module
`packaging/fedora/selinux/droidloom_fedora.te` is compiled into `/usr/share/selinux/packages/droidloom_fedora.pp` during RPM staging:

```sh
checkmodule -M -m -o droidloom_fedora.mod packaging/fedora/selinux/droidloom_fedora.te
semodule_package -o droidloom_fedora.pp -m droidloom_fedora.mod
```

The `%post` scriptlet in `droidloom-runtime.spec` installs the module with `semodule -i`.

### 2. Fedora `bpffs` SELinux Context Compatibility (`ClatCoordinator`)
On Fedora with SELinux enforcing on the shared host kernel, `/sys/fs/bpf` nodes carry `u:object_r:bpf_t:s0` rather than Android's `u:object_r:fs_bpf:s0`. Patch `android/aosp-patches/0001-netbpfload-tolerate-shared-kernel-bpf-ids.patch` now relaxes the `ClatCoordinator` `verifyPerms` context-mismatch check under `DROIDLOOM_CELL` while preserving mode and ownership checks, so both `x86_64` and `aarch64` images avoid `system_server` boot crash loops without a separate `fedora-classpath-compat` override.

### 3. `aarch64` Adreno 830 (`A8XX`) `minigbm` UBWC Mitigation
On Qualcomm Snapdragon 8 Elite (`SM8750` / Adreno 830 `/dev/dri/renderD128` via `msm_dpu`), Android's prebuilt `libminigbm_gralloc.so` (`backend_msm`) computes `DRM_FORMAT_MOD_QCOM_COMPRESSED` (UBWC) buffer sizes using Adreno 6xx `64x16` macrotile alignment (`32,563,200` bytes at `3392x2400`), whereas Adreno 830 Turnip writes `96x32` UBWC (`33,177,600` bytes), causing an Adreno SMMU fault and kernel GPU recovery deadlock.

Do **not** switch to `vendor.minigbm.allocator=dma_heap_images`, because `dma_heap_images` allocates unpadded-height buffers (for example `2,244,608` bytes at `548x974`) that are 4 KiB smaller than Turnip's overrun guard-page requirement (`2,248,704` bytes), crashing `SurfaceFlinger` `RenderEngine` on multi-layer dialogs/overlays.

For `aarch64`, `droidloom-package build-rpm` keeps the default `backend_msm` allocator and stages `/usr/lib/droidloom/fedora-init.rc` (`export vendor.minigbm.debug nocompression`) and `/usr/lib/droidloom/fedora-vendor-build.prop` (`vendor.minigbm.debug=nocompression`), wiring them into `cell-template.json`. `x86_64` does not include or apply this Adreno workaround.

### 4. `x86_64` Notes (Intel / AMD / NVIDIA & NativeBridge)
- **GPU**: `droidloom_x86_64` supports AMD (`amdgpu` / RadeonSI + RADV), NVIDIA (`nvidia` via `droidloom-gpu-server`), and Intel (`i915`/`xe` / Mesa Iris GLES). Note that `droidloom_x86_64` currently builds Mesa Vulkan only for AMD (`vulkan.radeon`), so Intel GPUs run HWUI and apps via Iris OpenGL ES unless Intel ANV is added.
- **NativeBridge**: `x86_64` builds include source-built Digitalis/Berberis (`libberberis_arm64.so`) for ARM64 apps; validate SELinux `Enforcing` logs (`ausearch -m avc`) on first ARM64 app launch.

### 5. `firewalld` Outbound Forwarding
Place [droidloom.xml](firewalld/zones/droidloom.xml) under `/usr/lib/firewalld/zones/` and [droidloom-egress.xml](firewalld/policies/droidloom-egress.xml) under `/usr/lib/firewalld/policies/`, then reload `firewalld` (`firewall-cmd --reload`, handled by `%post`).

## Building Fedora RPMs (Rootless Podman)

Full build on `x86_64` (or `aarch64`) inside the Fedora 45 builder container:

```sh
cargo run --locked -j 1 -p droidloom-package -- build-rpm
```

To reuse an already-staged `droidloom-update package-stage` directory (recompiling host binaries natively inside Fedora 45 without rebuilding AOSP):

```sh
cargo run --locked -j 1 -p droidloom-package -- build-rpm --payload /path/to/package-stage
```

A payload for the other architecture is packaged as it is: nothing is compiled, so the RPMs carry the host programs the payload came with. Upstream's published pacman packages, unpacked into `runtime/`, `image/` and `base/`, make such a payload. `--fedora-release` picks the builder container's Fedora release and with it the RPMs' dist tag:

```sh
cargo run --locked -j 1 -p droidloom-package -- build-rpm --architecture aarch64 \
  --payload /path/to/payload --fedora-release 44
```

Built RPMs are written to `dist/fedora/<version>-<release>/<arch>/`. Install both packages on the host with `sudo dnf install ...`, then configure desktop-mode sizing:

```sh
mkdir -p ~/.config/droidloom
cp packaging/fedora/density-desktop.json ~/.config/droidloom/density-v1.json
droidloomctl window-mode windowed --width 960 --height 680
droidloomctl start
```
