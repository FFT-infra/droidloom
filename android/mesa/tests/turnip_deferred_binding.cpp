// The Rust runner inserts the actual pinned/patched Android bind branch here.
// Test gralloc layout/import ordering and error propagation without a GPU.
#include <cassert>
#include <cstdint>
#include <cstdlib>

using VkResult = int;
constexpr VkResult VK_SUCCESS = 0;
constexpr VkResult VK_ERROR_INVALID_EXTERNAL_HANDLE = -1000072003;
constexpr int VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO = 14;
constexpr int NATIVE_BUFFER_ANDROID = 1000010000;
constexpr int TU_MAX_PLANE_COUNT = 3;
struct native_handle { int numFds; };
struct VkNativeBufferANDROID { int sType; const void *pNext; native_handle *handle; };
struct VkImageCreateInfo { int sType; const void *pNext; };
struct VkSubresourceLayout { uint64_t offset; };
struct VkImageDrmFormatModifierExplicitCreateInfoEXT { uint64_t drmFormatModifier; };
struct VkBindImageMemoryInfo { const void *pNext; };
struct vk_device { int alloc; };
struct tu_device { vk_device vk; };
struct tu_image { int vk; };
static int phase, imports;
static VkResult layout_result, update_result, import_result;
static const void *expected_next;
static tu_device *expected_device;
static tu_image *expected_image;

static const VkNativeBufferANDROID *vk_find_struct_const(const void *chain, int type) {
    auto buffer = static_cast<const VkNativeBufferANDROID *>(chain);
    return buffer && buffer->sType == type ? buffer : nullptr;
}
static VkResult vk_android_get_anb_layout(const VkImageCreateInfo *info,
    VkImageDrmFormatModifierExplicitCreateInfoEXT *out, VkSubresourceLayout *planes, int count) {
    assert(phase++ == 0 && info->pNext == expected_next);
    assert(info->sType == VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO && count == TU_MAX_PLANE_COUNT);
    out->drmFormatModifier = 0x0500000000000001;
    planes[0].offset = 4096;
    return layout_result;
}
static VkResult tu_image_update_layout(tu_device *device, tu_image *image,
    uint64_t modifier, const VkSubresourceLayout *planes) {
    assert(phase++ == 1 && device == expected_device && image == expected_image);
    assert(modifier == 0x0500000000000001 && planes[0].offset == 4096);
    return update_result;
}
static VkResult vk_android_import_anb(vk_device *device, const VkImageCreateInfo *info,
    const int *allocator, int *image) {
    assert(phase++ == 2 && info->pNext == expected_next);
    assert(device == &expected_device->vk && allocator == &expected_device->vk.alloc);
    assert(image == &expected_image->vk);
    ++imports;
    return import_result;
}
#define TU_CALLX(device, function) function
#define UNREACHABLE(message) std::abort()
static VkResult bind(tu_device *device, tu_image *image, const VkBindImageMemoryInfo *bind_info) {
    VkResult result;
    /* ACTUAL_ANDROID_BIND_BRANCH */
    std::abort();
}
static void reset() {
    phase = imports = 0;
    layout_result = update_result = import_result = VK_SUCCESS;
}
int main() {
    tu_device device{};
    tu_image image{};
    native_handle handle{1};
    VkNativeBufferANDROID native{NATIVE_BUFFER_ANDROID, nullptr, &handle};
    VkBindImageMemoryInfo info{&native};
    expected_device = &device;
    expected_image = &image;
    expected_next = &native;
    reset();
    assert(bind(&device, &image, &info) == VK_SUCCESS && phase == 3 && imports == 1);
    reset(); layout_result = -2;
    assert(bind(&device, &image, &info) == -2 && phase == 1 && imports == 0);
    reset(); update_result = -3;
    assert(bind(&device, &image, &info) == -3 && phase == 2 && imports == 0);
    reset(); import_result = -4;
    assert(bind(&device, &image, &info) == -4 && phase == 3 && imports == 1);
    reset(); info.pNext = nullptr;
    assert(bind(&device, &image, &info) == VK_ERROR_INVALID_EXTERNAL_HANDLE && phase == 0);
    info.pNext = &native; native.handle = nullptr;
    assert(bind(&device, &image, &info) == VK_ERROR_INVALID_EXTERNAL_HANDLE && phase == 0);
    native.handle = &handle; handle.numFds = 0;
    assert(bind(&device, &image, &info) == VK_ERROR_INVALID_EXTERNAL_HANDLE && phase == 0);
}
