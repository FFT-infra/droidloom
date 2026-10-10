/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.graphics.Rect;
import android.os.IBinder;
import android.os.Parcel;
import android.os.RemoteException;
import android.os.ServiceManager;
import android.util.Slog;

/** In-process native Binder adapter for infrequent WMS route publication. */
final class DroidloomInputNative {
    static final String BUILD_ABI = "DROIDLOOM_NATIVE_INPUT_ABI=2;";
    private static IBinder sInput;

    static boolean publish(int task, long incarnation, int display, Rect bounds, IBinder token) {
        return update(0x00444c04, task, incarnation, display, bounds, token);
    }
    static boolean retire(int task, long incarnation) {
        return update(0x00444c05, task, incarnation, 0, null, null);
    }
    private static boolean update(int code, int task, long incarnation, int display,
            Rect bounds, IBinder token) {
        // Called only on the task worker, outside WMS's lock. The native
        // InputManager Binder lives in this same system_server process.
        if (sInput == null) sInput = ServiceManager.getService("inputflinger");
        if (sInput == null) return false;
        Parcel data = Parcel.obtain();
        Parcel reply = Parcel.obtain();
        try {
            data.writeInterfaceToken("android.os.IInputFlinger");
            data.writeInt(1);
            data.writeInt(task);
            data.writeLong(incarnation);
            if (bounds != null) {
                data.writeInt(display);
                data.writeInt(bounds.left);
                data.writeInt(bounds.top);
                data.writeStrongBinder(token);
            }
            if (!sInput.transact(code, data, reply, 0)) return false;
            reply.readException();
            return reply.readBoolean();
        } catch (RemoteException | RuntimeException error) {
            sInput = null;
            Slog.w("DroidloomTasks", BUILD_ABI + " route publication failed", error);
            return false;
        } finally { reply.recycle(); data.recycle(); }
    }
    private DroidloomInputNative() {}
}
