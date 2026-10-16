/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server;

import android.util.Slog;

import java.util.function.Supplier;

/** One boot-time query of the Android Vulkan loader, before PackageManager publishes features. */
public final class DroidloomGraphicsCapabilities {
    private static final String TAG = "DroidloomGraphicsCapabilities";
    private static final String ABI = "DROIDLOOM_VULKAN_CAPABILITIES_ABI=1;";

    /** Returns {API version, hardware level}, or null when support cannot be established. */
    public static int[] detectVulkan() {
        return detectVulkan(() -> {
            System.loadLibrary("inputflinger");
            return probeVulkan();
        });
    }

    static int[] detectVulkan(Supplier<int[]> probe) {
        try {
            int[] capabilities = probe.get();
            if (capabilities == null || capabilities.length != 2
                    || capabilities[0] < 0x00400000 || (capabilities[0] >>> 29) != 0
                    || capabilities[1] < 0 || capabilities[1] > 1) {
                Slog.i(TAG, "No hardware Vulkan capabilities detected");
                return null;
            }
            Slog.i(TAG, ABI + " version=0x" + Integer.toHexString(capabilities[0])
                    + " level=" + capabilities[1]);
            return capabilities;
        } catch (LinkageError | RuntimeException failure) {
            Slog.w(TAG, "Vulkan capability probe failed; leaving features absent", failure);
            return null;
        }
    }

    private static native int[] probeVulkan();

    private DroidloomGraphicsCapabilities() {}
}
