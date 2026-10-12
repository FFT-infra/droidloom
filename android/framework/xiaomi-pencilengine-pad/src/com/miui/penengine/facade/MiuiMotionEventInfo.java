package com.miui.penengine.facade;

public class MiuiMotionEventInfo {
    public MiuiMotionEventInfo(float x, float y, long time, float tilt, float pressure, float orientation, int action, boolean isHistory) {
    }
    public float getX() { return 0f; }
    public float getY() { return 0f; }
    public long getTime() { return 0L; }
    public float getTilt() { return 0f; }
    public float getPressure() { return 0f; }
    public float getOrientation() { return 0f; }
    public boolean isHistoryPoint() { return false; }
    public int getMotionAction() { return 0; }
}
