/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.miui.penengine.facade;

/** One pen sample, as apps that bundle Xiaomi's pen SDK pass it around. */
public final class MiuiMotionEventInfo {
    private final float x;
    private final float y;
    private final long time;
    private final float pressure;
    private final float tilt;
    private final float orientation;
    private final int motionAction;
    private final boolean historyPoint;

    public MiuiMotionEventInfo(float x, float y, long time, float pressure, float tilt,
            float orientation, int motionAction, boolean historyPoint) {
        this.x = x;
        this.y = y;
        this.time = time;
        this.pressure = pressure;
        this.tilt = tilt;
        this.orientation = orientation;
        this.motionAction = motionAction;
        this.historyPoint = historyPoint;
    }

    public float getX() {
        return x;
    }

    public float getY() {
        return y;
    }

    public long getTime() {
        return time;
    }

    public float getTilt() {
        return tilt;
    }

    public float getPressure() {
        return pressure;
    }

    public float getOrientation() {
        return orientation;
    }

    public boolean isHistoryPoint() {
        return historyPoint;
    }

    public int getMotionAction() {
        return motionAction;
    }
}
