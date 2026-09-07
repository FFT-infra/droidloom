# Droidloom builds userspace only and shares the host kernel. The runtime mounts
# this vendor image beside checksum-pinned AOSP system/system_ext/product images.
TARGET_ARCH := arm64
TARGET_ARCH_VARIANT := armv8-a
TARGET_CPU_VARIANT := generic
TARGET_CPU_ABI := arm64-v8a

TARGET_SUPPORTS_32_BIT_APPS := false
TARGET_SUPPORTS_64_BIT_APPS := true
TARGET_SUPPORTS_OMX_SERVICE := false

TARGET_NO_KERNEL := true
TARGET_COPY_OUT_VENDOR := vendor

# These files anchor the HAL fragments in a complete Android 17 device VINTF
# contract. The cell reuses the matching 202604 framework matrix from the
# pinned CI base rather than inheriting Cuttlefish's device manifest.
DEVICE_MANIFEST_FILE += vendor/droidloom/android/device/droidloom_arm64/manifest.xml
DEVICE_MATRIX_FILE += vendor/droidloom/android/device/droidloom_arm64/compatibility_matrix.xml

BOARD_VENDORIMAGE_FILE_SYSTEM_TYPE := ext4
BOARD_VENDORIMAGE_PARTITION_SIZE := 1073741824

BOARD_AVB_ENABLE := false

# Keep the native MSM allocator. Split display/render hosts explicitly select
# the portable linear DMA-heap image backend at cell construction time.
SOONG_CONFIG_NAMESPACES += minigbm
SOONG_CONFIG_minigbm += platform
SOONG_CONFIG_minigbm_platform := msm

# Bionic hardware rendering supports both mainline MSM and downstream KGSL.
# Native Freedreno remains the default; KGSL cells explicitly select Zink
# over Turnip. No software renderer is included.
BOARD_MESA3D_USES_MESON_BUILD := true
BOARD_MESA3D_GALLIUM_DRIVERS := freedreno zink
BOARD_MESA3D_VULKAN_DRIVERS := freedreno
BOARD_MESA3D_BUILD_LIBGBM := false
BOARD_MESA3D_MESON_ARGS := -Dfreedreno-kmds=msm,kgsl
