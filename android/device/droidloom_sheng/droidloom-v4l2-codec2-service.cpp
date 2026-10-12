// AIDL Codec2 service adapter for Droidloom's pinned AOSP V4L2 components.
#include <C2Component.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>
#include <codec2/aidl/ComponentStore.h>
#include <log/log.h>
#include <minijail.h>
#include <v4l2_codec2/v4l2/V4L2ComponentStore.h>

#include <csignal>
#include <memory>
#include <string>

static constexpr char kBaseSeccompPolicyPath[] =
        "/vendor/etc/seccomp_policy/android.hardware.media.c2-default-seccomp_policy";
static constexpr char kExtSeccompPolicyPath[] =
        "/vendor/etc/seccomp_policy/android.hardware.media.c2-extended-seccomp_policy";

int main() {
    signal(SIGPIPE, SIG_IGN);
    android::SetUpMinijail(kBaseSeccompPolicyPath, kExtSeccompPolicyPath);
    ABinderProcess_setThreadPoolMaxThreadCount(16);
    ABinderProcess_startThreadPool();

    auto store = ::ndk::SharedRefBase::make<
            aidl::android::hardware::media::c2::utils::ComponentStore>(
                    android::V4L2ComponentStore::Create());
    const std::string serviceName =
            std::string(aidl::android::hardware::media::c2::IComponentStore::descriptor)
            + "/default";
    const binder_exception_t result = AServiceManager_addService(
            store->asBinder().get(), serviceName.c_str());
    if (result != EX_NONE) {
        ALOGE("Could not register V4L2 Codec2 store: %d", result);
        return 1;
    }
    ABinderProcess_joinThreadPool();
    return 0;
}
