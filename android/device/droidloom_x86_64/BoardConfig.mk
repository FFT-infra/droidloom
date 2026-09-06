# Native x86_64 userspace sharing the host kernel and its render node.
TARGET_ARCH := x86_64
TARGET_ARCH_VARIANT := x86_64
TARGET_CPU_VARIANT := generic
TARGET_CPU_ABI := x86_64

TARGET_SUPPORTS_32_BIT_APPS := false
TARGET_SUPPORTS_64_BIT_APPS := true
TARGET_SUPPORTS_OMX_SERVICE := false

TARGET_NO_KERNEL := true
TARGET_COPY_OUT_VENDOR := vendor

# These fragments are architecture-independent even though their historical
# source location carries the first ARM64 product name.
DEVICE_MANIFEST_FILE += vendor/droidloom/android/device/droidloom_arm64/manifest.xml
DEVICE_MATRIX_FILE += vendor/droidloom/android/device/droidloom_arm64/compatibility_matrix.xml

BOARD_VENDORIMAGE_FILE_SYSTEM_TYPE := ext4
BOARD_VENDORIMAGE_PARTITION_SIZE := 1073741824

BOARD_AVB_ENABLE := false

# Minigbm allocates directly through the host DRM driver and hands native
# DMA-BUFs to the Droidloom Composer. The x86_64 development product supports
# both AMD and Intel render nodes; neither Android component receives a DRM
# card/KMS node.
SOONG_CONFIG_NAMESPACES += minigbm
SOONG_CONFIG_minigbm += platform
SOONG_CONFIG_minigbm_platform := droidloom_x86

# Produce Bionic RadeonSI and Iris EGL/GLES drivers for the two supported host
# GPU families. This remains fully native: there is no gfxstream, virgl, or
# software renderer between the Android app and the kernel DRM driver.
BOARD_MESA3D_USES_MESON_BUILD := true
BOARD_MESA3D_GALLIUM_DRIVERS := radeonsi iris
BOARD_MESA3D_VULKAN_DRIVERS := amd
BOARD_MESA3D_BUILD_LIBGBM := true
# Mesa 26 can compile RadeonSI shaders with ACO. Keep the Android vendor image
# independent from an otherwise unused target LLVM runtime; RADV already uses
# ACO by default.
BOARD_MESA3D_MESON_ARGS := -Damd-use-llvm=false
