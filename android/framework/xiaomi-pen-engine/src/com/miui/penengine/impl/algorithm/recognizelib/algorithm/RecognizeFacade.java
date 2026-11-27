/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.miui.penengine.impl.algorithm.recognizelib.algorithm;

import android.content.Context;
import java.util.List;

/** Handwriting recognition. Droidloom has none, so nothing is recognized. */
public final class RecognizeFacade {
    public RecognizeFacade(Context context) {}

    public String recognizeStrokes(List<?> strokes) {
        return "";
    }

    public void onDestroy() {}
}
