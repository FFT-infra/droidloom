/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.miui.penengine.impl.algorithm.estimate;

import android.content.Context;
import java.util.List;

/** Stroke prediction. Droidloom has none, so the feature reports itself off. */
public final class EstimateFacade {
    private static final EstimateFacade INSTANCE = new EstimateFacade();

    private EstimateFacade() {}

    public static EstimateFacade getInstance() {
        return INSTANCE;
    }

    public void init(Context context) {}

    public boolean isFeatureEnable() {
        return false;
    }

    public void setRefreshRate(float refreshRate) {}

    public int getEstimateEvent(List<?> input, List<?> output) {
        return -1;
    }
}
