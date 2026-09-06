/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui;

import android.util.Log;
import java.util.HashSet;
import java.util.Set;

/** Bounded diagnostics: log feature names once, never notification or app content. */
public final class Unsupported {
    private static final Set<String> SEEN = new HashSet<>();
    public static synchronized void report(String feature) {
        if (SEEN.add(feature)) Log.w("DroidloomSystemUI", "Not implemented: " + feature);
    }
    private Unsupported() {}
}
