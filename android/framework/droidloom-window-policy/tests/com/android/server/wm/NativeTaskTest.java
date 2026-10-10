/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

/** Host JVM/C++/Rust ABI check. No Android services or host devices are used. */
public final class NativeTaskTest {
    public static void main(String[] args) {
        // Static initialization checks the actual native version.
        if (DroidloomTaskNative.registerTask(42, 1, 10, 0, "org.example.app", false) != null
                || DroidloomTaskNative.registerTask(42, 1, 0, 1, "org.example.app", false) != null
                || DroidloomTaskNative.registerTask(42, 0, 0, 0, "org.example.app", false) != null
                || DroidloomTaskNative.registerTask(42, 1, 0, 0, null, false) != null
                || DroidloomTaskNative.retireTask(0, 1)
                || DroidloomTaskNative.acknowledgeCommand(0)
                || DroidloomTaskNative.acknowledgeCommand(-1)) {
            throw new AssertionError("JNI failed to preserve task scope/identity validation");
        }
        System.out.println("NativeTaskTest: passed");
    }
}
