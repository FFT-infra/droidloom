package com.miui.penengine.impl.algorithm.estimate;

import android.content.Context;
import java.util.List;

public class EstimateFacade {
    private static final EstimateFacade INSTANCE = new EstimateFacade();
    public static EstimateFacade getInstance() {
        return INSTANCE;
    }
    public void init(Context context) {
    }
    public boolean isFeatureEnable() {
        return false;
    }
    public void setRefreshRate(float rate) {
    }
    public Object getEstimateEvent(List list1, List list2) {
        return null;
    }
}
