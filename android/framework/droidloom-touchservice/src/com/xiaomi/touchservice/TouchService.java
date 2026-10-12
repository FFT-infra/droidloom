package com.xiaomi.touchservice;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;
import com.xiaomi.touchservice.pencilengine.PencilEngineHost;

public final class TouchService extends Service {
    private static final String TAG = "TouchService";

    @Override
    public void onCreate() {
        super.onCreate();
        Log.i(TAG, "TouchService created");
    }

    @Override
    public IBinder onBind(Intent intent) {
        Log.i(TAG, "TouchService binding request accepted: " + intent);
        return PencilEngineHost.get().asBinder();
    }
}
