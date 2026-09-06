/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.screenshot;

import android.app.Service;
import android.content.Intent;
import android.os.*;
import com.android.internal.util.ScreenshotHelper;
import com.android.systemui.Unsupported;

/** Acknowledge unsupported screenshots so callers do not wait for a timeout. */
public final class TakeScreenshotService extends Service {
    private final Messenger messenger = new Messenger(new Handler(Looper.getMainLooper(), message -> {
        Unsupported.report("screenshot portal");
        if (message.replyTo != null) try {
            message.replyTo.send(Message.obtain(null, ScreenshotHelper.SCREENSHOT_MSG_URI, null));
            message.replyTo.send(Message.obtain(null, ScreenshotHelper.SCREENSHOT_MSG_PROCESS_COMPLETE));
        } catch (RemoteException ignored) {}
        return true;
    }));
    @Override public IBinder onBind(Intent intent) { return messenger.getBinder(); }
}
