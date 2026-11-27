/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.miui.penengine.impl.manager;

import android.content.Context;
import java.util.Observer;

/** Xiaomi's licence check for the pen features. Droidloom has none, so it never reports. */
public final class PencilEngineManager {
    public PencilEngineManager() {}

    public void pencilEngineInit(Context context, String packageName, long flags,
            Observer observer, boolean debug) {}

    public void pencilEngineDestroy() {}

    public void runAuthConnect() {}

    public void setLogLevel(int level) {}
}
