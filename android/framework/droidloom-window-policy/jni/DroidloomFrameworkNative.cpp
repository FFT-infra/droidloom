// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
// JNI array conversion only; framing, credentials, bounds and sockets live in Rust.
#include <jni.h>
#include <array>
#include <cstdint>
#include <cstddef>
extern "C" bool droidloom_framework_start();
extern "C" void droidloom_framework_catalog_changed();
extern "C" int32_t droidloom_framework_next(uint8_t*, size_t);
extern "C" bool droidloom_framework_reply(const uint8_t*, size_t, bool);
extern "C" JNIEXPORT jboolean JNICALL
Java_com_android_server_wm_DroidloomFrameworkNative_start(JNIEnv*, jclass) { return droidloom_framework_start(); }
extern "C" JNIEXPORT void JNICALL
Java_com_android_server_wm_DroidloomFrameworkNative_catalogChanged(JNIEnv*, jclass) { droidloom_framework_catalog_changed(); }
extern "C" JNIEXPORT jbyteArray JNICALL
Java_com_android_server_wm_DroidloomFrameworkNative_next(JNIEnv* env, jclass) {
    std::array<uint8_t, 16386> bytes{};
    int length = droidloom_framework_next(bytes.data(), bytes.size());
    if (length < 0) return nullptr;
    jbyteArray result = env->NewByteArray(length);
    if (result != nullptr) env->SetByteArrayRegion(result, 0, length, reinterpret_cast<const jbyte*>(bytes.data()));
    return result;
}
extern "C" JNIEXPORT jboolean JNICALL
Java_com_android_server_wm_DroidloomFrameworkNative_reply(JNIEnv* env, jclass, jbyteArray bytes, jboolean success) {
    if (bytes == nullptr || env->GetArrayLength(bytes) > 8 * 1024 * 1024) return JNI_FALSE;
    jbyte* data = env->GetByteArrayElements(bytes, nullptr);
    if (data == nullptr) return JNI_FALSE;
    bool okay = droidloom_framework_reply(reinterpret_cast<const uint8_t*>(data), env->GetArrayLength(bytes), success);
    env->ReleaseByteArrayElements(bytes, data, JNI_ABORT); return okay;
}
