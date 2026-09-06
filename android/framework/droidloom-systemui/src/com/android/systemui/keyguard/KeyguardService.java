/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.keyguard;

import android.app.Service;
import android.content.Intent;
import android.os.Bundle;
import android.os.IBinder;
import android.os.RemoteException;
import com.android.internal.policy.*;
import com.android.internal.widget.LockPatternUtils;
import com.android.systemui.Unsupported;
import com.android.systemui.compat.EmptyKeyguard;
import java.io.FileDescriptor;
import java.io.PrintWriter;

/** Native desktop owns locking. Never claim an Android credential was verified. */
public final class KeyguardService extends Service {
    private volatile int userId;
    private LockPatternUtils locks;
    private boolean secure() { return locks.isSecure(userId); }
    private void state(IKeyguardStateCallback callback) throws RemoteException {
        if (callback == null) return;
        boolean secure = secure();
        if (secure) Unsupported.report("Android credential lock");
        callback.onShowingStateChanged(secure, userId);
        callback.onInputRestrictedStateChanged(secure);
        callback.onTrustedChanged(false);
        callback.onSimSecureStateChanged(false);
    }
    private final EmptyKeyguard binder = new EmptyKeyguard() {
        @Override public void addStateMonitorCallback(IKeyguardStateCallback callback)
                throws RemoteException { state(callback); }
        @Override public void verifyUnlock(IKeyguardExitCallback callback) throws RemoteException {
            if (callback != null) callback.onKeyguardExitResult(!secure());
        }
        @Override public void dismiss(IKeyguardDismissCallback callback, CharSequence message)
                throws RemoteException {
            if (callback != null) {
                if (secure()) callback.onDismissError(); else callback.onDismissSucceeded();
            }
        }
        @Override public void setCurrentUser(int currentUser) { userId = currentUser; }
        @Override public void onScreenTurningOn(int reason, IKeyguardDrawnCallback callback)
                throws RemoteException { if (callback != null) callback.onDrawn(); }
        @Override public void restoreKeyguardState(KeyguardState restored,
                IKeyguardStateCallback callback, IKeyguardDrawnCallback drawn,
                boolean timeoutRequested, Bundle options) throws RemoteException {
            userId = restored.userId;
            state(callback);
            if (drawn != null) drawn.onDrawn();
        }
    };
    @Override public void onCreate() {
        super.onCreate();
        locks = new LockPatternUtils(this);
    }
    @Override public IBinder onBind(Intent intent) { return binder; }
    @Override protected void dump(FileDescriptor fd, PrintWriter out, String[] args) {
        out.println("Droidloom keyguard: user=" + userId + " androidCredentialConfigured=" + secure());
    }
}
