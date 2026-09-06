# Pinned Mesa Android build

Droidloom builds Mesa 26.1.7 from the exact archive and SHA-256/SHA-512 identity
in `android/manifest/source-lock.json`. It does not use the incomplete generated
Soong driver modules in AOSP's `external/mesa3d` project.

`droidloom-mesa prepare` verifies the downloaded archive, rejects unsafe archive
paths, applies the ordered patches in `patches/`, hashes the resulting tree, and
atomically publishes it below `.work/mesa-prepared/`. An existing prepared tree
is accepted only when its manifest, patch hashes, and complete tree hash still
match.

`tools/droidloom-vendor-image-smoke` then projects that verified tree into the
sparse AOSP materialization and invokes Mesa's Android Meson wrapper with:

- Gallium `freedreno` for EGL/GLES;
- Vulkan `freedreno` (Turnip);
- only the MSM DRM render-node KMD;
- Android API 37 and the pinned AOSP/Bionic compiler/linker inputs.

The two local patches make host code generation use AOSP's pinned Mako and add
the standard `<type_traits>` include required by Android's libc++. Neither patch
changes driver behavior.

Download the already-pinned archive when it is not present:

    curl --fail --location \
      --output .work/mesa-26.1.7.tar.xz \
      https://archive.mesa3d.org/mesa-26.1.7.tar.xz

The build wrapper refuses a missing or hash-mismatched archive.
