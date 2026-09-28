package com.xiaomi.touchservice.pencilengine;

import android.os.Binder;
import android.os.IBinder;
import android.os.RemoteException;
import android.util.Log;
import android.util.SparseArray;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;

public final class PencilEngineHost {
    private static final String TAG = "PencilEngine_Host";
    public static final int FEATURE_COLOR_PICK = 0;
    public static final int FEATURE_POSTURE = 1;
    public static final int FEATURE_TOUCH_FILM = 2;

    private static final Set<String> TOUCH_FILM_PACKAGES = Collections.synchronizedSet(new HashSet<String>());
    private static final Set<String> POSTURE_PACKAGES = Collections.synchronizedSet(new HashSet<String>());
    private static final PencilEngineHost INSTANCE = new PencilEngineHost();

    private final SparseArray<ClientRecord> clients = new SparseArray<>();
    private final Object lock = new Object();

    private final IPencilEngine.Stub binder = new IPencilEngine.Stub() {
        @Override
        public void registerListener(IPencilEngineCallback callback) {
            registerClient(callback, null);
        }

        @Override
        public void registerListenerByPackageName(IPencilEngineCallback callback, String packageName) {
            registerClient(callback, packageName);
        }

        @Override
        public void unregisterListener() {
            int pid = Binder.getCallingPid();
            synchronized (lock) {
                clients.remove(pid);
                Log.i(TAG, "Client unregistered pid=" + pid + " remaining=" + clients.size());
            }
        }

        @Override
        public int triggerFunction(int feature, int enable) {
            int pid = Binder.getCallingPid();
            ClientRecord record;
            synchronized (lock) {
                record = clients.get(pid);
            }
            if (record == null) {
                Log.w(TAG, "triggerFunction without registered listener pid=" + pid);
                return -1;
            }
            boolean on = enable != 0;
            String packageName = record.packageName;
            if (feature == FEATURE_TOUCH_FILM && packageName != null) {
                if (on) TOUCH_FILM_PACKAGES.add(packageName);
                else TOUCH_FILM_PACKAGES.remove(packageName);
                Log.i(TAG, (on ? "Add " : "Remove ") + packageName + " to touch film packages");
            } else if (feature == FEATURE_POSTURE && packageName != null) {
                if (on) POSTURE_PACKAGES.add(packageName);
                else POSTURE_PACKAGES.remove(packageName);
                Log.i(TAG, (on ? "Add " : "Remove ") + packageName + " to posture packages");
            }
            return 0;
        }
    };

    public static PencilEngineHost get() {
        return INSTANCE;
    }

    public IBinder asBinder() {
        return binder;
    }

    private void registerClient(IPencilEngineCallback callback, String packageName) {
        if (callback == null) return;
        int pid = Binder.getCallingPid();
        int uid = Binder.getCallingUid();
        synchronized (lock) {
            ClientRecord record = new ClientRecord(pid, uid, packageName, callback);
            try {
                callback.asBinder().linkToDeath(record, 0);
            } catch (RemoteException e) {
                Log.w(TAG, "Failed linkToDeath: " + e);
            }
            clients.put(pid, record);
            Log.i(TAG, "Registered client pid=" + pid + " pkg=" + packageName);
        }
    }

    private final class ClientRecord implements IBinder.DeathRecipient {
        final int pid;
        final int uid;
        final String packageName;
        final IPencilEngineCallback callback;

        ClientRecord(int pid, int uid, String packageName, IPencilEngineCallback callback) {
            this.pid = pid;
            this.uid = uid;
            this.packageName = packageName;
            this.callback = callback;
        }

        @Override
        public void binderDied() {
            Log.w(TAG, "Client died pid=" + pid);
            synchronized (lock) {
                clients.remove(pid);
            }
            if (packageName != null) {
                TOUCH_FILM_PACKAGES.remove(packageName);
                POSTURE_PACKAGES.remove(packageName);
            }
        }
    }
}
