package com.xiaomi.touchservice.pencilengine;

import com.xiaomi.touchservice.pencilengine.IPencilEngineCallback;

interface IPencilEngine {
    void registerListener(IPencilEngineCallback callback);
    void unregisterListener();
    int triggerFunction(int feature, int enable);
    void registerListenerByPackageName(IPencilEngineCallback callback, String packageName);
}
