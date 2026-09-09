/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.input;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.IBinder;
import java.io.File;

/** Byte-stream relay. Only the installed SystemUI UID may reach the host socket. */
final class ClipboardRelay {
    static void start() {
        start("clipboard");
        start("notifications");
    }
    private static void start(String endpoint) {
        if (!new File("/dev/socket/droidloom/" + endpoint).exists()) return;
        android.util.Log.i("DroidloomClipboard", "DROIDLOOM_CLIPBOARD_RELAY_ABI=1;");
        if (endpoint.equals("notifications")) android.util.Log.i("DroidloomNotifications", "DROIDLOOM_NOTIFICATIONS_RELAY_ABI=1;");
        Thread thread = new Thread(() -> serve(endpoint), "droidloom-" + endpoint + "-relay");
        thread.setDaemon(true); thread.start();
    }
    private static void serve(String endpoint) {
        com.android.droidloom.runtime.CpuPlacement.background();
        try (LocalServerSocket listener = new LocalServerSocket("droidloom-" + endpoint)) {
            while (true) {
                try (LocalSocket cell = listener.accept(); LocalSocket host = new LocalSocket()) {
                    IBinder binder = (IBinder) Class.forName("android.os.ServiceManager")
                            .getMethod("getService", String.class).invoke(null, "package");
                    Object packages = Class.forName("android.content.pm.IPackageManager$Stub")
                            .getMethod("asInterface", IBinder.class).invoke(null, binder);
                    int uid = (Integer) Class.forName("android.content.pm.IPackageManager")
                            .getMethod("getPackageUid", String.class, long.class, int.class)
                            .invoke(packages, "com.android.systemui", 0L, 0);
                    if (uid < 0 || cell.getPeerCredentials().getUid() != uid) continue;
                    host.connect(new LocalSocketAddress("/dev/socket/droidloom/" + endpoint, LocalSocketAddress.Namespace.FILESYSTEM));
                    int owner = android.system.Os.stat("/dev/socket/droidloom/" + endpoint).st_uid;
                    if (host.getPeerCredentials().getUid() != owner) continue;
                    Thread incoming = new Thread(() -> copy(host, cell), "droidloom-" + endpoint + "-in");
                    incoming.setDaemon(true); incoming.start();
                    copy(cell, host); incoming.join();
                } catch (Exception e) {
                    android.util.Log.w("DroidloomClipboard", "Relay disconnected: " + e.getClass().getSimpleName());
                }
            }
        } catch (Exception e) { android.util.Log.e("DroidloomClipboard", "Relay unavailable"); }
    }
    private static void copy(LocalSocket from, LocalSocket to) {
        com.android.droidloom.runtime.CpuPlacement.background();
        try {
            byte[] buffer = new byte[65536]; int n;
            while ((n = from.getInputStream().read(buffer)) >= 0) to.getOutputStream().write(buffer, 0, n);
        } catch (Exception ignored) { }
        finally { close(from); close(to); }
    }
    private static void close(LocalSocket socket) {
        // close alone need not interrupt another thread's read; shutdown wakes both pumps.
        try { socket.shutdownInput(); } catch (Exception ignored) { }
        try { socket.shutdownOutput(); } catch (Exception ignored) { }
        try { socket.close(); } catch (Exception ignored) { }
    }
}
