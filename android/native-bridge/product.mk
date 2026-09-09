# Teto uses Android's native API proxies; the selected Mesa drivers remain native.
include frameworks/libs/binary_translation/berberis_config.mk

# RenderScript is outside the initial ARM64 APK milestone. Use the common NDK
# closure rather than pulling the legacy RenderScript compiler into the sparse build.
DROIDLOOM_NATIVE_BRIDGE_PACKAGES := \
    $(filter-out $(NATIVE_BRIDGE_PRODUCT_PACKAGES),$(BERBERIS_PRODUCT_PACKAGES_ARM64_TO_X86_64)) \
    $(NATIVE_BRIDGE_PRODUCT_PACKAGES_RISCV64_READY)
PRODUCT_PACKAGES += $(DROIDLOOM_NATIVE_BRIDGE_PACKAGES)
PRODUCT_SOONG_NAMESPACES += frameworks/libs/native_bridge_support/android_api/libc
BUILD_BERBERIS := true
BUILD_BERBERIS_ARM64_TO_X86_64 := true
$(call soong_config_set,berberis,translation_arch,arm64_to_x86_64)

# The assembler also updates these properties in the reused system partition.
# Executable binfmt registration is disabled for the library-loading MVP: the
# shared host kernel must not receive Android's global binfmt handlers.
PRODUCT_SYSTEM_PROPERTIES += \
    ro.dalvik.vm.native.bridge=libberberis_arm64.so \
    ro.dalvik.vm.isa.arm64=x86_64 \
    ro.enable.native.bridge.exec=0 \
    ro.berberis.flags=android-mmap-noreserve
