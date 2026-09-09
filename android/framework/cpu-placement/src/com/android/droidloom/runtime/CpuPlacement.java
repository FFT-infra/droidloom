/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.runtime;

import android.os.Process;
import android.util.Log;

/** Explicit roles for owned workers; Android remains responsible for app state. */
public final class CpuPlacement {
    private CpuPlacement() {}

    /** Call at worker entry, before creating sockets, executors or Binder clients. */
    public static void background() {
        try {
            // The native CPUSET_SP_SYSTEM profile describes background system
            // work and contains no unrelated I/O-controller action.
            Process.setThreadGroupAndCpuset(Process.myTid(), Process.THREAD_GROUP_SYSTEM);
            Process.setThreadPriority(Process.THREAD_PRIORITY_BACKGROUND);
        } catch (RuntimeException unavailable) {
            Log.w("DroidloomCpuPlacement", "Background placement unavailable", unavailable);
        }
    }
}
