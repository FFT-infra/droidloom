/* SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.policy;

/** Primitive-only JNI boundary; also exercised by the headless JVM ABI probe. */
final class DroidloomPolicyNative {
    static final String BUILD_ABI = "DROIDLOOM_WINDOW_POLICY_ABI=1;";
    static final int SCREEN_START = 1, KEYGUARD_DRAWN = 2, WINDOWS_DRAWN = 3,
            SCREEN_OFF = 4, SYSTEM_READY = 5, SYSTEM_BOOTED = 6, GOING_TO_SLEEP = 7,
            BOOT_WITH_OUTPUT_OFF = 8, ALLOW_LOCKSCREEN_WHEN_ON = 9, CLEAR_LOCKSCREEN_DISPLAYS = 10;
    static final int SCREEN_ON = 1, CAN_ANIMATE = 2, BOOT_ANIMATION_DISMISSIBLE = 3,
            KEYGUARD_DRAWN_ONCE = 4, READY = 5, BOOTED = 6, LOCKSCREEN_TIMER_REQUIRED = 7;
    static final int WAIT_FOR_WINDOWS = 1, COMPLETE_SCREEN = 2, NOTIFY_BOOT_COMPLETED = 3;
    static final int INVALID = 0, APPLICATION = 1, TOAST = 2, TOKEN_CHECKED = 3,
            INTERNAL = 4, ALERT = 5, APPLICATION_OVERLAY = 6;
    static final int ROUNDED_OVERLAY = 1, INTERNAL_WINDOW = 2, SYSTEM_UID = 4,
            PACKAGE_CHECKED = 8, PACKAGE_EXISTS = 16, SYSTEM_OVERLAY = 32,
            TRUSTED_VIRTUAL_OWNER = 64, ALERT_WINDOW = 128;
    static final int ALLOW = 0, DENY = 1, INVALID_TYPE = 2, NEED_PACKAGE = 3,
            NEED_APP_OP = 4;
    static final int CONSUME = 0, PASS = 1, MEDIA = 2, VOLUME = 3;

    static {
        // Rust is linked into the native input library already rebuilt for WMS. This also
        // works with immutable base images: no new /system mountpoint is needed.
        System.loadLibrary("inputflinger");
        int version = version();
        if (version != 1) {
            throw new LinkageError(BUILD_ABI + " native ABI mismatch: " + version);
        }
    }

    private DroidloomPolicyNative() {}
    private static native int version();
    static native int command(int op, int display, int generation, boolean value);
    static native boolean query(int op, int display, boolean value);
    static native int authorizeWindow(int kind, int facts, int targetSdk, int appOpMode);
    static native int routeKey(int code, boolean interactive, boolean injected, boolean unhandled);
}
