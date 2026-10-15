// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
#include "DroidloomVulkan.h"

#include <dlfcn.h>
#include <jni.h>

extern "C" __attribute__((used, visibility("default")))
const char DROIDLOOM_VULKAN_CAPABILITIES_ABI[] = "DROIDLOOM_VULKAN_CAPABILITIES_ABI=1;";

extern "C" JNIEXPORT jintArray JNICALL
Java_com_android_server_DroidloomGraphicsCapabilities_probeVulkan(JNIEnv* env, jclass) {
    void* loader = dlopen("libvulkan.so", RTLD_NOW | RTLD_LOCAL);
    if (!loader) return nullptr;
    auto getProc = reinterpret_cast<PFN_vkGetInstanceProcAddr>(
            dlsym(loader, "vkGetInstanceProcAddr"));
    const auto capabilities = droidloom::detectVulkan(getProc);
    dlclose(loader);
    if (!capabilities.version) return nullptr;
    const jint values[] = {static_cast<jint>(capabilities.version), capabilities.level};
    jintArray result = env->NewIntArray(2);
    if (result) env->SetIntArrayRegion(result, 0, 2, values);
    return result;
}
