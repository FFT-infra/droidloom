# Pinned Mesa Android build

Droidloom builds Mesa 26.1.7 from the archive and checksums in the source lock.
It uses Mesa's Android Meson wrapper with the pinned AOSP/Bionic compiler and
Android API 37, rather than AOSP's incomplete generated Mesa Soong modules.

`droidloom-mesa prepare` verifies the archive, applies the ordered `patches/`,
and records the resulting tree's identity. The maintained Rust Android builder
in `tools/droidloom-update/src/android.rs` mirrors that tree into its temporary
AOSP projection, applies the runtime patches below, and restores projected
sources after building. The cached ARM64 build uses the same projection.

The ARM64 product builds Gallium Freedreno and Zink, Vulkan Freedreno (Turnip),
and both MSM and KGSL kernel interfaces. Native DRM remains the cell default.
Explicit `graphics_backend: "kgsl_dma_heap"` selects Zink over Turnip and the
portable minigbm allocator; see `docs/architecture.md` for the device boundary.
No software renderer is included.

Runtime patches under `android/aosp-patches/`:

- `0010-minigbm-dma-heap-images.patch`: opt-in bounded linear image allocation,
  import validation and matching allocation/capability-query checks.
- `0011-mesa-zink-kgsl.patch`: when ordinary DRM matching fails on `msm_drm`,
  select a unique Turnip device without DRM identity only if external DMA-BUF
  memory and image modifier extensions are present. Other DRM drivers keep
  their existing matching behavior.
- `0012-mesa-adreno722.patch`: upstream device entry from Mesa commit
  `803731f7ed4aa8e8265fe0aff08b85efb3f3c7e9`, also used by the validated host
  Turnip build. It retains upstream's initial A730-derived register settings.

- `0013-mesa-texture-upload-span.patch`: preserve the threaded staging layout
  while copying only the valid source span, excluding trailing row/layer
  padding. Prevents guard-page overreads during Skia glyph-atlas uploads on
  the Zink render-pass path; keeps threading and GPU buffer-to-image copies.

Validate a new host using real Android allocations, raw DMA-BUF and native
Android EGL imports, GPU rendering/readback, native fences and synchronized CPU
visibility. EGL initialization alone does not prove the complete graphics path.
Full Android boot and application presentation remain separate checks; linear
buffer bandwidth and frame rate must be measured under the intended workload.
