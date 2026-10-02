#define LOG_TAG "DroidloomCameraProducer"

#include "protocol.h"

#include <aidl/android/companion/virtualcamera/BnVirtualCameraCallback.h>
#include <aidl/android/companion/virtualcamera/IVirtualCameraService.h>
#include <aidl/android/companion/virtualcamera/VirtualCameraConfiguration.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>
#include <android/hardware_buffer.h>
#include <android/native_window.h>
#include <apex/window.h>
#include <log/log.h>
#include <system/window.h>
#include <vndk/window.h>

#include <sys/un.h>
#include <unistd.h>

#include <condition_variable>
#include <cstdlib>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

namespace {

using namespace droidloom::camera;
namespace camera = aidl::android::companion::virtualcamera;
using ndk::ScopedAStatus;
constexpr char kEndpoint[] = "/dev/socket/droidloom/camera";
constexpr auto kDequeueTimeout = std::chrono::milliseconds(500);

int connectHost(const std::atomic_bool& stopped) {
    const int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (fd < 0) return -1;
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    std::memcpy(address.sun_path, kEndpoint, sizeof(kEndpoint));
    if (connect(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0) {
        if (errno != EINPROGRESS ||
            waitSocket(fd, POLLOUT, std::chrono::steady_clock::now() + kIoTimeout, stopped) !=
                IoResult::Ok) {
            close(fd);
            return -1;
        }
        int error = 0;
        socklen_t size = sizeof(error);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &size) != 0 || error != 0) {
            close(fd);
            return -1;
        }
    }
    return fd;
}

std::optional<Response> readResponse(int fd, const std::atomic_bool& stopped) {
    std::array<uint8_t, 24> bytes{};
    if (readExact(fd, bytes.data(), bytes.size(), stopped) != IoResult::Ok) return std::nullopt;
    auto header = response(bytes);
    if (header && header->kind == Kind::Error) ALOGE("Host camera error %u", header->status);
    return header;
}

std::optional<uint32_t> catalogue() {
    const std::atomic_bool stopped{false};
    const int fd = connectHost(stopped);
    if (fd < 0) return std::nullopt;
    const auto query = request(Operation::Catalogue, 0);
    std::optional<uint32_t> result;
    if (writeAll(fd, query.data(), query.size(), stopped) == IoResult::Ok) {
        const auto header = readResponse(fd, stopped);
        std::array<uint8_t, 4> bytes{};
        if (header && header->kind == Kind::Catalogue &&
            readExact(fd, bytes.data(), bytes.size(), stopped) == IoResult::Ok) {
            const auto mask = static_cast<uint32_t>(littleEndian(bytes.data(), bytes.size()));
            if ((mask & ~3U) == 0) result = mask;
        }
    }
    close(fd);
    return result;
}

// Own the dequeued buffer until queue succeeds; every other path returns it without posting pixels.
class WindowBuffer {
  public:
    explicit WindowBuffer(ANativeWindow* window) : mWindow(window) {}
    ~WindowBuffer() {
        if (mLocked) AHardwareBuffer_unlock(mHardware, &mFence);
        if (mBuffer != nullptr) ANativeWindow_cancelBuffer(mWindow, mBuffer, mFence);
    }

    bool write(const std::vector<uint8_t>& frame, uint64_t timestamp,
               const std::atomic_bool& stopped) {
        if (ANativeWindow_dequeueBuffer(mWindow, &mBuffer, &mFence) != 0) return false;
        if (mFence >= 0) {
            const auto ready = waitSocket(mFence, POLLIN,
                                         std::chrono::steady_clock::now() + kDequeueTimeout, stopped);
            if (ready != IoResult::Ok) return false;
            close(mFence);
            mFence = -1;
        }
        mHardware = ANativeWindowBuffer_getHardwareBuffer(mBuffer);
        if (mHardware == nullptr || stopped.load()) return false;
        AHardwareBuffer_Desc description{};
        AHardwareBuffer_describe(mHardware, &description);
        if (description.format != AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM ||
            description.width != kWidth || description.height != kHeight ||
            description.stride > static_cast<uint32_t>(std::numeric_limits<int>::max())) {
            return false;
        }
        void* pixels = nullptr;
        if (AHardwareBuffer_lock(mHardware, AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
                                 -1, nullptr, &pixels) != 0) {
            return false;
        }
        mLocked = true;
        if (!copyFrame(frame.data(), frame.size(), pixels, description.width,
                       description.height, description.stride)) {
            return false;
        }
        const int unlocked = AHardwareBuffer_unlock(mHardware, &mFence);
        mLocked = false;
        if (unlocked != 0) return false;
        if (stopped.load() ||
            ANativeWindow_setBuffersTimestamp(mWindow, static_cast<int64_t>(timestamp)) != 0) {
            return false;
        }
        const int result = ANativeWindow_queueBuffer(mWindow, mBuffer, mFence);
        mFence = -1;  // queueBuffer consumes the fence even when it fails.
        if (result != 0) return false;
        mBuffer = nullptr;
        return true;
    }

  private:
    ANativeWindow* mWindow;
    ANativeWindowBuffer* mBuffer = nullptr;
    AHardwareBuffer* mHardware = nullptr;
    int mFence = -1;
    bool mLocked = false;
};

class Capture {
  public:
    Capture(uint32_t cameraId, ANativeWindow* window, std::function<void()> failed)
        : mCameraId(cameraId), mWindow(window), mFailed(std::move(failed)) {
        ANativeWindow_acquire(mWindow);
    }
    ~Capture() {
        stop();
        ANativeWindow_release(mWindow);
    }
    void start() { mThread = std::thread(&Capture::run, this); }
    void stop() {
        mStopped.store(true);
        {
            std::lock_guard lock(mSocketLock);
            if (mSocket >= 0) shutdown(mSocket, SHUT_RDWR);
        }
        std::lock_guard lock(mJoinLock);
        if (mThread.joinable()) mThread.join();
    }

  private:
    bool forward(int fd) {
        const auto query = request(Operation::Open, mCameraId);
        if (writeAll(fd, query.data(), query.size(), mStopped) != IoResult::Ok) return false;
        const auto opened = readResponse(fd, mStopped);
        if (!opened || opened->kind != Kind::Opened) return false;
        std::vector<uint8_t> frame(kFrameBytes);
        uint64_t previousTimestamp = 0;
        while (!mStopped.load()) {
            const auto header = readResponse(fd, mStopped);
            if (!header || header->kind != Kind::Frame || header->timestamp <= previousTimestamp ||
                readExact(fd, frame.data(), frame.size(), mStopped) != IoResult::Ok) {
                return false;
            }
            WindowBuffer buffer(mWindow);
            if (!buffer.write(frame, header->timestamp, mStopped)) return false;
            previousTimestamp = header->timestamp;
        }
        return true;
    }

    void run() {
        bool connected = native_window_api_connect(mWindow, NATIVE_WINDOW_API_CPU) == 0;
        bool ready = connected &&
            ANativeWindow_setBuffersGeometry(mWindow, kWidth, kHeight, WINDOW_FORMAT_RGBA_8888) == 0 &&
            ANativeWindow_setUsage(mWindow, AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN |
                                           AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE) == 0 &&
            ANativeWindow_setDequeueTimeout(mWindow,
                std::chrono::duration_cast<std::chrono::nanoseconds>(kDequeueTimeout).count()) == 0;
        bool success = false;
        if (ready && !mStopped.load()) {
            const int fd = connectHost(mStopped);
            if (fd >= 0) {
                {
                    std::lock_guard lock(mSocketLock);
                    mSocket = fd;
                    if (mStopped.load()) shutdown(fd, SHUT_RDWR);
                }
                success = forward(fd);
                {
                    std::lock_guard lock(mSocketLock);
                    mSocket = -1;
                    close(fd);
                }
            }
        }
        if (connected) native_window_api_disconnect(mWindow, NATIVE_WINDOW_API_CPU);
        if (!success && !mStopped.load()) {
            ALOGE("Camera %u stream failed; closing its HAL session", mCameraId);
            mFailed();
        }
    }

    const uint32_t mCameraId;
    ANativeWindow* const mWindow;
    std::function<void()> mFailed;
    std::atomic_bool mStopped{false};
    std::mutex mSocketLock;
    int mSocket = -1;
    std::mutex mJoinLock;
    std::thread mThread;
};

struct Failure {
    uint32_t cameraId;
    uint64_t generation;
};

struct Events {
    std::mutex lock;
    std::condition_variable changed;
    std::deque<Failure> failures;
    bool serviceDied = false;
};

class Producer final : public camera::BnVirtualCameraCallback {
  public:
    Producer(uint32_t cameraId, std::shared_ptr<camera::IVirtualCameraService> service, Events& events)
        : mCameraId(cameraId), mService(std::move(service)), mEvents(events) {}
    ~Producer() override { shutdown(); }

    ScopedAStatus onOpenCamera() override { return ScopedAStatus::ok(); }
    ScopedAStatus onConfigureSession(const camera::VirtualCameraMetadata&,
            const std::shared_ptr<camera::ICaptureResultConsumer>&) override {
        return ScopedAStatus::ok();
    }
    ScopedAStatus onProcessCaptureRequest(int32_t, int32_t,
            const std::optional<camera::VirtualCameraMetadata>&) override {
        return ScopedAStatus::ok();
    }
    ScopedAStatus onStreamConfigured(int32_t streamId, const aidl::android::view::Surface& surface,
            int32_t width, int32_t height, camera::Format format) override {
        std::lock_guard lock(mLock);
        if (mClosing) {
            reportFailureLocked(mGeneration);
            return ScopedAStatus::ok();
        }
        if (mCapture || surface.get() == nullptr || width != kWidth || height != kHeight ||
            format != camera::Format::RGBA_8888) {
            reportFailureLocked(mGeneration);
            return ScopedAStatus::ok();
        }
        const uint64_t generation = ++mGeneration;
        mFailurePending = false;
        mStreamId = streamId;
        mCapture = std::make_shared<Capture>(mCameraId, surface.get(), [this, generation] {
            std::lock_guard lock(mLock);
            reportFailureLocked(generation);
        });
        mCapture->start();
        return ScopedAStatus::ok();
    }
    ScopedAStatus onStreamClosed(int32_t streamId) override {
        std::shared_ptr<Capture> capture;
        {
            std::lock_guard lock(mLock);
            if (streamId != mStreamId || !mCapture) return ScopedAStatus::ok();
            capture = std::move(mCapture);
            ++mGeneration;
            mFailurePending = false;
        }
        if (capture) capture->stop();
        return ScopedAStatus::ok();
    }

    void closeFailedSession(uint64_t generation) {
        std::shared_ptr<Capture> capture;
        {
            std::lock_guard lock(mLock);
            if (generation != mGeneration) return;
            capture = std::move(mCapture);
            ++mGeneration;
            mFailurePending = false;
            mClosing = true;
        }
        if (capture) capture->stop();
        const auto result = mService->closeSession(asBinder());
        if (!result.isOk()) ALOGE("Cannot close camera %u: %s", mCameraId, result.getDescription().c_str());
        std::lock_guard lock(mLock);
        mClosing = false;
    }

    void shutdown() {
        std::shared_ptr<Capture> capture;
        {
            std::lock_guard lock(mLock);
            mClosing = true;
            capture = std::move(mCapture);
            ++mGeneration;
        }
        if (capture) capture->stop();
    }

  private:
    void reportFailureLocked(uint64_t generation) {
        if (generation != mGeneration || mFailurePending) return;
        mFailurePending = true;
        {
            std::lock_guard lock(mEvents.lock);
            const auto queued = std::find_if(mEvents.failures.begin(), mEvents.failures.end(),
                [&](const Failure& failure) { return failure.cameraId == mCameraId; });
            if (queued == mEvents.failures.end()) {
                mEvents.failures.push_back({mCameraId, generation});
            } else {
                queued->generation = generation;
            }
        }
        mEvents.changed.notify_one();
    }

    const uint32_t mCameraId;
    std::shared_ptr<camera::IVirtualCameraService> mService;
    Events& mEvents;
    std::mutex mLock;
    std::shared_ptr<Capture> mCapture;
    int32_t mStreamId = -1;
    uint64_t mGeneration = 0;
    bool mFailurePending = false;
    bool mClosing = false;
};

std::shared_ptr<camera::IVirtualCameraService> waitForService() {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(30);
    do {
        ndk::SpAIBinder binder(AServiceManager_checkService("virtual_camera"));
        if (binder.get() != nullptr) return camera::IVirtualCameraService::fromBinder(binder);
        std::this_thread::sleep_for(std::chrono::milliseconds(200));
    } while (std::chrono::steady_clock::now() < deadline);
    return nullptr;
}

}  // namespace

int main() {
    ABinderProcess_setThreadPoolMaxThreadCount(6);
    ABinderProcess_startThreadPool();
    const auto service = waitForService();
    const auto cameras = catalogue();
    if (!service || !cameras || *cameras == 0) {
        ALOGE("Camera provider or host catalogue unavailable");
        return EXIT_FAILURE;
    }
    Events events;
    std::unique_ptr<AIBinder_DeathRecipient, decltype(&AIBinder_DeathRecipient_delete)> death(
        AIBinder_DeathRecipient_new([](void* cookie) {
            auto& events = *static_cast<Events*>(cookie);
            {
                std::lock_guard lock(events.lock);
                events.serviceDied = true;
            }
            events.changed.notify_one();
        }), AIBinder_DeathRecipient_delete);
    if (AIBinder_linkToDeath(service->asBinder().get(), death.get(), &events) != STATUS_OK) {
        ALOGE("Cannot monitor camera provider lifetime");
        return EXIT_FAILURE;
    }
    std::array<std::shared_ptr<Producer>, 2> producers;
    bool registered = true;
    for (uint32_t id = 0; id < producers.size(); ++id) {
        if ((*cameras & (1U << id)) == 0) continue;
        auto producer = ndk::SharedRefBase::make<Producer>(id, service, events);
        camera::VirtualCameraConfiguration configuration;
        camera::SupportedStreamConfiguration input;
        input.width = kWidth;
        input.height = kHeight;
        input.imageFormat = camera::Format::RGBA_8888;
        input.maxFps = 30;
        input.index = 0;
        configuration.supportedStreamConfigs = {input};
        configuration.virtualCameraCallback = producer;
        configuration.lensFacing = id == 0 ? camera::LensFacing::BACK : camera::LensFacing::FRONT;
        configuration.sensorOrientation = camera::SensorOrientation::ORIENTATION_0;
        configuration.perFrameCameraMetadataEnabled = false;
        configuration.isMultiInputStreamEnabled = false;
        bool accepted = false;
        const auto result = service->registerCamera(producer->asBinder(), configuration, 0, &accepted);
        if (!result.isOk() || !accepted) {
            ALOGE("Cannot register camera %u: %s", id, result.getDescription().c_str());
            registered = false;
            break;
        }
        producers[id] = std::move(producer);
        ALOGI("Host %s camera registered on default Android device", id == 0 ? "back" : "front");
    }
    while (registered) {
        Failure failure{};
        {
            std::unique_lock lock(events.lock);
            events.changed.wait(lock, [&] { return events.serviceDied || !events.failures.empty(); });
            if (events.serviceDied) break;
            failure = events.failures.front();
            events.failures.pop_front();
        }
        if (producers[failure.cameraId]) producers[failure.cameraId]->closeFailedSession(failure.generation);
    }
    for (const auto& producer : producers) {
        if (producer) {
            producer->shutdown();
            service->unregisterCamera(producer->asBinder());
        }
    }
    AIBinder_unlinkToDeath(service->asBinder().get(), death.get(), &events);
    // Binder oneway callbacks can still be queued; keep their process-owned state alive until exit.
    std::_Exit(EXIT_FAILURE);
}
