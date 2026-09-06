package com.android.droidloom.input;

import android.content.ComponentName;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.IBinder;
import org.json.JSONObject;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;

/** Authenticated, event-driven relay between the Android IME and host text input. */
final class TextInputBridge {
    static void start() {
        Thread worker = new Thread(TextInputBridge::serve, "droidloom-text-input");
        worker.setDaemon(true);
        worker.start();
    }

    private static void serve() {
        try (LocalServerSocket listener = new LocalServerSocket("droidloom-ime")) {
            while (true) {
                try (LocalSocket ime = listener.accept(); LocalSocket host = new LocalSocket()) {
                    IBinder packageBinder = (IBinder) Class.forName("android.os.ServiceManager")
                            .getMethod("getService", String.class).invoke(null, "package");
                    Object packages = Class.forName("android.content.pm.IPackageManager$Stub")
                            .getMethod("asInterface", IBinder.class).invoke(null, packageBinder);
                    int expectedUid = (Integer) Class.forName("android.content.pm.IPackageManager")
                            .getMethod("getPackageUid", String.class, long.class, int.class)
                            .invoke(packages, "com.android.droidloom.ime", 0L, 0);
                    if (ime.getPeerCredentials().getUid() != expectedUid) continue;
                    host.connect(new LocalSocketAddress("/dev/socket/droidloom/text-input",
                            LocalSocketAddress.Namespace.FILESYSTEM));
                    Thread edits = new Thread(() -> {
                        try {
                            String line;
                            while ((line = readLine(host.getInputStream())) != null) {
                                ime.getOutputStream().write((line + "\n").getBytes(StandardCharsets.UTF_8));
                            }
                        } catch (Exception ignored) { }
                        finally { close(ime); close(host); }
                    }, "droidloom-text-edits");
                    edits.setDaemon(true);
                    edits.start();
                    String line;
                    while ((line = readLine(ime.getInputStream())) != null) {
                        JSONObject state = new JSONObject(line);
                        state.put("task", state.getBoolean("active")
                                ? focusedTask(state.getString("package")) : 0);
                        state.remove("package");
                        host.getOutputStream().write((state.toString() + "\n").getBytes(StandardCharsets.UTF_8));
                    }
                } catch (Exception error) {
                    android.util.Log.w("DroidloomInput", "Text-input relay disconnected: "
                            + error.getClass().getSimpleName());
                }
            }
        } catch (Exception error) {
            android.util.Log.e("DroidloomInput", "Text-input listener failed", error);
        }
    }

    private static int focusedTask(String packageName) throws Exception {
        IBinder binder = (IBinder) Class.forName("android.os.ServiceManager")
                .getMethod("getService", String.class).invoke(null, "activity_task");
        Object manager = Class.forName("android.app.IActivityTaskManager$Stub")
                .getMethod("asInterface", IBinder.class).invoke(null, binder);
        List<?> tasks = (List<?>) Class.forName("android.app.IActivityTaskManager")
                .getMethod("getTasks", int.class, boolean.class, boolean.class, int.class)
                .invoke(manager, 100, false, false, 0);
        Class<?> info = Class.forName("android.app.TaskInfo");
        for (Object task : tasks) {
            ComponentName top = (ComponentName) info.getField("topActivity").get(task);
            if (info.getField("isFocused").getBoolean(task) && top != null
                    && packageName.equals(top.getPackageName())) {
                return info.getField("taskId").getInt(task);
            }
        }
        return 0;
    }

    private static String readLine(InputStream input) throws Exception {
        ByteArrayOutputStream line = new ByteArrayOutputStream();
        int value;
        while ((value = input.read()) >= 0) {
            if (value == '\n') return line.toString("UTF-8");
            if (line.size() >= 16384) throw new java.io.IOException("oversized text record");
            line.write(value);
        }
        return null;
    }

    private static void close(LocalSocket socket) {
        try { socket.close(); } catch (Exception ignored) { }
    }
}
