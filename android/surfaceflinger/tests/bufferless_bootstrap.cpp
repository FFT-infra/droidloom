/* SPDX-License-Identifier: GPL-3.0-or-later */
#include <cassert>
#include <cerrno>
#include <cstdint>
#include <dirent.h>
#include <memory>
#include <sys/eventfd.h>
#include <unistd.h>
#include <utility>
#include <vector>

// Framework substitutes only. The methods/selection guard below are extracted
// from the actual patch; the Android build validates their real dependencies.
#define ALOGE(...) ((void)0)
#define ALOGI(...) ((void)0)
using status_t = int;
constexpr int NO_ERROR = 0;
namespace ui { enum class Dataspace { UNKNOWN, DISPLAY_P3 }; }
template<class T> struct sp : std::shared_ptr<T> {
    using std::shared_ptr<T>::shared_ptr;
    template<class... Args> static sp make(Args&&... args) {
        return sp(new T(std::forward<Args>(args)...));
    }
};
struct Fence {
    int fd;
    explicit Fence(int fd = -1) : fd(fd) {}
    ~Fence() { if (fd >= 0) close(fd); }
    int dup() const { return fd >= 0 ? ::dup(fd) : -1; }
    int get() const { return fd; }
};
struct FenceTime {
    sp<Fence> fence;
    explicit FenceTime(sp<Fence> fence) : fence(std::move(fence)) {}
};
enum class DroidloomTimingStage { BootstrapQueue, BootstrapRequests };
struct DroidloomTimingSpan {
    DroidloomTimingSpan(DroidloomTimingStage, uint64_t, uint64_t = 0, uint64_t = 0) {}
    void outcome(bool) {}
    void stop() {}
};
struct DisplaySurface {
    int advances = 0, result = NO_ERROR;
    float ratio = 0;
    int advanceFrame(float value) { ++advances; ratio = value; return result; }
};
struct SurfaceBase {
    virtual ~SurfaceBase() = default;
    virtual bool prepareDroidloomHeadlessFrame(const sp<Fence>&) { return false; }
    virtual bool queueDroidloomHeadlessFrame(float) { return false; }
};
struct DroidloomNativeWindow { static constexpr int kCompleteHeadlessFrame = 0x444c0001; };
struct NativeWindow {
    int requests = 0, lastFence = -2, result = NO_ERROR;
    int perform(NativeWindow* window, int operation, int fence) {
        assert(window == this && operation == DroidloomNativeWindow::kCompleteHeadlessFrame);
        ++requests; lastFence = fence;
        return result;
    }
};
struct RenderSurface : SurfaceBase {
    std::shared_ptr<int> mTexture;
    DisplaySurface display;
    DisplaySurface* mDisplaySurface = &display;
    sp<NativeWindow> mNativeWindow = sp<NativeWindow>::make();
    bool mDroidloomBufferlessReported = false;
    bool prepareDroidloomHeadlessFrame(const sp<Fence>&) override;
    bool queueDroidloomHeadlessFrame(float) override;
};
struct LayerFE {
    sp<Fence> last;
    void setWasClientComposed(const sp<Fence>& fence) { last = fence; }
};
struct ColorProfile {
    bool wide = false;
    bool hasWideColorGamut() const { return wide; }
};
struct RenderEngine {
    bool protectedSupport = true;
    bool supportsProtectedContent() const { return protectedSupport; }
};
struct CompositionEngine {
    RenderEngine engine;
    RenderEngine& getRenderEngine() { return engine; }
};
enum class CompositionStrategyPredictionState { DISABLED, SUCCESS, FAIL };
struct GpuCompositionResult {
    bool buffer = false;
    bool bufferAvailable() const { return buffer; }
};
struct Output {
    struct State {
        bool usesClientComposition = true, isEnabled = true;
        ui::Dataspace dataspace = ui::Dataspace::DISPLAY_P3;
        CompositionStrategyPredictionState strategyPrediction =
                CompositionStrategyPredictionState::DISABLED;
    } state;
    bool mDroidloomSkipBootstrapTarget = true;
    sp<Fence> mDroidloomHeadlessFence;
    uint64_t mDroidloomTimingFrame = 1;
    SurfaceBase* mRenderSurface;
    ColorProfile profile;
    ColorProfile* mDisplayColorProfile = &profile;
    CompositionEngine engine;
    std::vector<LayerFE*> selected;
    bool expensive = true, powerHints = false, gpuReporting = false;
    bool generatedProtected = false;
    ui::Dataspace generatedDataspace = ui::Dataspace::UNKNOWN;
    int requests = 0, hints = 0, fallbackFrames = 0;
    std::unique_ptr<FenceTime> gpuFence;
    explicit Output(SurfaceBase& surface) : mRenderSurface(&surface) {}
    const State& getState() const { return state; }
    CompositionEngine& getCompositionEngine() { return engine; }
    uint64_t getOutputLayerCount() const { return selected.size(); }
    std::vector<int> generateClientCompositionRequests(bool support, ui::Dataspace dataspace,
                                                      std::vector<LayerFE*>& layers) {
        ++requests; generatedProtected = support; generatedDataspace = dataspace;
        layers = selected;
        return {};
    }
    void setExpensiveRenderingExpected(bool value) { expensive = value; }
    bool isPowerHintSessionEnabled() const { return powerHints; }
    bool isPowerHintSessionGpuReportingEnabled() const { return gpuReporting; }
    void setHintSessionGpuFence(std::unique_ptr<FenceTime> fence) {
        ++hints; gpuFence = std::move(fence);
    }
    bool finishDroidloomHeadlessFrame();
    void prepareDroidloomHeadlessFrame();
    void finishFrame(GpuCompositionResult&&);
};
#include "DroidloomHeadlessMethods.inc"

void unsignaled(int fd) {
    uint64_t value = 0;
    assert(read(fd, &value, sizeof(value)) == -1 && errno == EAGAIN);
}
int descriptors() {
    DIR* dir = opendir("/proc/self/fd");
    assert(dir);
    int count = 0;
    while (readdir(dir)) ++count;
    closedir(dir);
    return count;
}
int main() {
    const int initialDescriptors = descriptors();
    {
        const auto fence = sp<Fence>::make(eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK));
        assert(fence->fd >= 0);
        RenderSurface surface;
        Output output(surface);
        LayerFE first, second, excluded;
        output.selected = {&first, &second};
        output.mDroidloomHeadlessFence = fence;
        output.powerHints = true;
        output.profile.wide = true;
        for (int i = 0; i < 128; ++i) {
            output.prepareDroidloomHeadlessFrame();
            // HWC may consume the borrowed pending fence during preparation.
            assert(surface.mNativeWindow->requests == i + 1);
            unsignaled(surface.mNativeWindow->lastFence);
            output.finishFrame({});
        }
        assert(output.fallbackFrames == 0 && output.requests == 128);
        assert(surface.display.advances == 128 && surface.display.ratio == 1.0f);
        assert(surface.mNativeWindow->requests == 128);
        assert(surface.mNativeWindow->lastFence == fence->get());
        assert(surface.mTexture == nullptr && surface.mDroidloomBufferlessReported);
        assert(first.last.get() == fence.get() && second.last.get() == fence.get());
        assert(!excluded.last && !output.expensive);
        assert(output.generatedProtected && output.generatedDataspace == ui::Dataspace::DISPLAY_P3);
        assert(output.hints == 128 && output.gpuFence->fence->fd >= 0);
        unsignaled(fence->fd);
        unsignaled(output.gpuFence->fence->fd);
    }
    assert(descriptors() == initialDescriptors); // Includes advisory GPU-fence duplicates.
    for (int scenario = 0; scenario < 9; ++scenario) {
        RenderSurface surface;
        SurfaceBase unsupported;
        Output output(scenario == 7 ? unsupported : static_cast<SurfaceBase&>(surface));
        output.mDroidloomHeadlessFence = sp<Fence>::make();
        GpuCompositionResult result;
        if (scenario == 0) output.mDroidloomSkipBootstrapTarget = false;
        if (scenario == 1) output.mDroidloomHeadlessFence = {};
        if (scenario == 2) output.state.usesClientComposition = false;
        if (scenario == 3) surface.mTexture = std::make_shared<int>(42);
        if (scenario == 4) output.state.strategyPrediction = CompositionStrategyPredictionState::SUCCESS;
        if (scenario == 5) output.state.strategyPrediction = CompositionStrategyPredictionState::FAIL;
        if (scenario == 6) result.buffer = true;
        if (scenario == 8) surface.mNativeWindow->result = -5;
        output.prepareDroidloomHeadlessFrame();
        output.finishFrame(std::move(result));
        assert(output.fallbackFrames == 1 && output.requests == 0);
        assert(surface.display.advances == 0 && !surface.mDroidloomBufferlessReported);
        assert(surface.mNativeWindow->requests ==
                ((scenario == 2 || scenario == 4 || scenario == 5 || scenario == 6 || scenario == 8) ? 1 : 0));
        if (scenario == 3) assert(surface.mTexture && *surface.mTexture == 42);
    }
    {
        RenderSurface surface;
        Output output(surface);
        output.state.isEnabled = false;
        output.mDroidloomHeadlessFence = sp<Fence>::make();
        output.prepareDroidloomHeadlessFrame();
        output.finishFrame({});
        assert(output.fallbackFrames == 0 && surface.display.advances == 0);
        output.state.isEnabled = true;
        output.mDroidloomSkipBootstrapTarget = true; // Next refresh selects the path again.
        output.engine.engine.protectedSupport = false;
        output.powerHints = output.gpuReporting = true;
        surface.display.result = -5;
        output.prepareDroidloomHeadlessFrame();
        output.finishFrame({});
        assert(output.fallbackFrames == 0 && surface.display.advances == 1);
        assert(surface.mNativeWindow->requests == 1 && surface.mNativeWindow->lastFence == -1);
        assert(output.requests == 1 && output.hints == 0 && !output.generatedProtected);
        assert(output.generatedDataspace == ui::Dataspace::UNKNOWN);
    }
    assert(descriptors() == initialDescriptors);
}
