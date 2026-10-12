package com.miui.penengine.impl.algorithm.shape;

import android.content.Context;
import android.view.MotionEvent;

public class ShapeRecognizeFacade {
    private static final ShapeRecognizeFacade INSTANCE = new ShapeRecognizeFacade();
    public static ShapeRecognizeFacade getInstance() {
        return INSTANCE;
    }
    public void init(Context context) {
    }
    public void destroy() {
    }
    public boolean isFeatureEnable() {
        return false;
    }
    public void processTouchEvent(MotionEvent event) {
    }
    public Object getShapePath() {
        return null;
    }
    public void setStopTime(int time) {
    }
}
