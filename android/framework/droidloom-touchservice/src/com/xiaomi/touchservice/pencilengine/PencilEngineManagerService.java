package com.xiaomi.touchservice.pencilengine;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

public final class PencilEngineManagerService extends Service {
    private static final String TAG = "PencilEngine_Service";

    @Override
    public void onCreate() {
        super.onCreate();
        Log.i(TAG, "PencilEngineManagerService created");
    }

    @Override
    public IBinder onBind(Intent intent) {
        Log.i(TAG, "Binding request accepted: " + intent);
        return PencilEngineHost.get().asBinder();
    }
}
