/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.miui.penengine.impl.algorithm.shape;

import android.content.Context;
import android.graphics.Path;
import android.view.MotionEvent;

/** Shape snapping. Droidloom has none, so the feature reports itself off. */
public final class ShapeRecognizeFacade {
    private static final ShapeRecognizeFacade INSTANCE = new ShapeRecognizeFacade();

    private ShapeRecognizeFacade() {}

    public static ShapeRecognizeFacade getInstance() {
        return INSTANCE;
    }

    public boolean init(Context context) {
        return false;
    }

    public int destroy() {
        return 0;
    }

    public boolean isFeatureEnable() {
        return false;
    }

    public void processTouchEvent(MotionEvent event) {}

    public Path getShapePath() {
        return null;
    }

    public void setStopTime(int milliseconds) {}
}
