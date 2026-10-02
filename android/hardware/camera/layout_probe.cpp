#include <android/hardware_buffer.h>

#include <poll.h>
#include <signal.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>

namespace {

constexpr uint32_t kWidth = 640;
constexpr uint32_t kHeight = 480;
constexpr uint32_t kMaxRowStride = kWidth * 16;

void timeout(int) {
    constexpr char message[] = "layout_probe FAIL timeout\n";
    static_cast<void>(write(STDERR_FILENO, message, sizeof(message) - 1));
    _exit(EXIT_FAILURE);
}

uint8_t grey(uint32_t x, uint32_t y) {
    return static_cast<uint8_t>(16 + (x * 7 + y * 13) % 224);
}

bool waitFence(int fd) {
    if (fd < 0) return true;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(1);
    bool ready = false;
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd fence{fd, POLLIN, 0};
        const int result = poll(&fence, 1, 20);
        if (result < 0 && errno == EINTR) continue;
        if (result < 0) break;
        if (result > 0) {
            ready = (fence.revents & POLLIN) != 0 && (fence.revents & (POLLERR | POLLNVAL)) == 0;
            break;
        }
    }
    close(fd);
    return ready;
}

class Buffer {
  public:
    explicit Buffer(const char* name) : mName(name) {}
    ~Buffer() {
        if (mLocked) {
            int fence = -1;
            AHardwareBuffer_unlock(mBuffer, &fence);
            if (fence >= 0) close(fence);
        }
        if (mBuffer != nullptr) AHardwareBuffer_release(mBuffer);
    }

    bool allocate(uint32_t format, uint64_t usage) {
        AHardwareBuffer_Desc description{};
        description.width = kWidth;
        description.height = kHeight;
        description.layers = 1;
        description.format = format;
        description.usage = usage;
        const int result = AHardwareBuffer_allocate(&description, &mBuffer);
        if (result != 0 || mBuffer == nullptr) return fail("allocate", result);
        AHardwareBuffer_describe(mBuffer, &description);
        if (description.width != kWidth || description.height != kHeight ||
            description.layers != 1 || description.format != format) {
            return fail("description", -1);
        }
        std::printf("%s format=0x%x stride=%u allocate PASS\n", mName, format, description.stride);
        return true;
    }

    bool lock(uint64_t usage, AHardwareBuffer_Planes& planes) {
        planes = {};
        const int result = AHardwareBuffer_lockPlanes(mBuffer, usage, -1, nullptr, &planes);
        if (result != 0) return fail("lockPlanes", result);
        mLocked = true;
        return true;
    }

    bool unlock() {
        int fence = -1;
        const int result = AHardwareBuffer_unlock(mBuffer, &fence);
        mLocked = false;
        if (result != 0) {
            if (fence >= 0) close(fence);
            return fail("unlock", result);
        }
        if (!waitFence(fence)) return fail("unlock_fence", -1);
        return true;
    }

    bool fail(const char* stage, int result) const {
        std::printf("%s %s FAIL code=%d\n", mName, stage, result);
        return false;
    }

  private:
    const char* mName;
    AHardwareBuffer* mBuffer = nullptr;
    bool mLocked = false;
};

bool layout(const char* name, const AHardwareBuffer_Planes& planes, bool rgba) {
    const uint32_t count = rgba ? 1 : 3;
    std::printf("%s planeCount=%u\n", name, planes.planeCount);
    if (planes.planeCount != count) return false;
    for (uint32_t index = 0; index < count; ++index) {
        const auto& plane = planes.planes[index];
        const uint32_t width = index == 0 ? kWidth : kWidth / 2;
        const bool pixelStrideValid = rgba ? plane.pixelStride == 4 :
            index == 0 ? plane.pixelStride == 1 : (plane.pixelStride == 1 || plane.pixelStride == 2);
        std::printf("%s plane=%u rowStride=%u pixelStride=%u\n", name, index,
                    plane.rowStride, plane.pixelStride);
        if (plane.data == nullptr || !pixelStrideValid || plane.rowStride > kMaxRowStride ||
            plane.rowStride < width * plane.pixelStride) {
            return false;
        }
    }
    return rgba || (planes.planes[1].rowStride == planes.planes[2].rowStride &&
                    planes.planes[1].pixelStride == planes.planes[2].pixelStride);
}

bool yuvReadWrite() {
    Buffer buffer("YUV420");
    if (!buffer.allocate(AHARDWAREBUFFER_FORMAT_Y8Cb8Cr8_420,
                          AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN |
                          AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN)) {
        return false;
    }
    AHardwareBuffer_Planes planes{};
    if (!buffer.lock(AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN, planes)) return false;
    if (!layout("YUV420 write", planes, false)) return buffer.fail("write_layout", -1);
    for (uint32_t index = 0; index < 3; ++index) {
        const auto& plane = planes.planes[index];
        const uint32_t width = index == 0 ? kWidth : kWidth / 2;
        const uint32_t height = index == 0 ? kHeight : kHeight / 2;
        auto* data = static_cast<uint8_t*>(plane.data);
        for (uint32_t y = 0; y < height; ++y) {
            for (uint32_t x = 0; x < width; ++x) {
                data[y * plane.rowStride + x * plane.pixelStride] = index == 0 ? grey(x, y) : 128;
            }
        }
    }
    if (!buffer.unlock() || !buffer.lock(AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN, planes)) return false;
    if (!layout("YUV420 read", planes, false)) return buffer.fail("read_layout", -1);
    for (uint32_t index = 0; index < 3; ++index) {
        const auto& plane = planes.planes[index];
        const uint32_t width = index == 0 ? kWidth : kWidth / 2;
        const uint32_t height = index == 0 ? kHeight : kHeight / 2;
        const auto* data = static_cast<const uint8_t*>(plane.data);
        for (uint32_t y = 0; y < height; ++y) {
            for (uint32_t x = 0; x < width; ++x) {
                const uint8_t expected = index == 0 ? grey(x, y) : 128;
                if (data[y * plane.rowStride + x * plane.pixelStride] != expected) {
                    return buffer.fail("readback", -1);
                }
            }
        }
    }
    if (!buffer.unlock()) return false;
    std::puts("YUV420 CPU_READ/WRITE readback PASS");
    return true;
}

bool rgbaWrite() {
    Buffer buffer("RGBA8888");
    if (!buffer.allocate(AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM,
                          AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN |
                          AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE)) {
        return false;
    }
    AHardwareBuffer_Planes planes{};
    if (!buffer.lock(AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN, planes)) return false;
    if (!layout("RGBA8888 write", planes, true)) return buffer.fail("write_layout", -1);
    const auto& plane = planes.planes[0];
    auto* data = static_cast<uint8_t*>(plane.data);
    for (uint32_t y = 0; y < kHeight; ++y) {
        for (uint32_t x = 0; x < kWidth; ++x) {
            auto* pixel = data + y * plane.rowStride + x * plane.pixelStride;
            pixel[0] = pixel[1] = pixel[2] = grey(x, y);
            pixel[3] = 255;
        }
    }
    if (!buffer.unlock()) return false;
    std::puts("RGBA8888 CPU_WRITE|GPU_SAMPLED allocation/write PASS");
    return true;
}

}  // namespace

int main() {
    std::setvbuf(stdout, nullptr, _IONBF, 0);
    struct sigaction action{};
    action.sa_handler = timeout;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, nullptr) != 0) {
        std::puts("layout_probe deadline FAIL");
        return EXIT_FAILURE;
    }
    alarm(10);
    const bool yuv = yuvReadWrite();
    const bool rgba = rgbaWrite();
    alarm(0);
    std::printf("layout_probe %s\n", yuv && rgba ? "PASS" : "FAIL");
    return yuv && rgba ? EXIT_SUCCESS : EXIT_FAILURE;
}
