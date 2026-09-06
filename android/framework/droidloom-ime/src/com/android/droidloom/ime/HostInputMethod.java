package com.android.droidloom.ime;

import android.inputmethodservice.InputMethodService;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import org.json.JSONObject;
import java.io.InputStream;
import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/** No Android keyboard window: the host owns the input panel. */
public final class HostInputMethod extends InputMethodService {
    private final Handler main = new Handler(Looper.getMainLooper());
    private final ExecutorService writes = Executors.newSingleThreadExecutor();
    private volatile LocalSocket socket;
    private volatile boolean stopped;
    private Thread reader;
    private long session;
    private long showRequest;
    private boolean composing;
    private JSONObject state;
    private final EditorVisibility visibility = new EditorVisibility();

    @Override public void onCreate() {
        super.onCreate();
        reader = new Thread(this::connect, "droidloom-ime");
        reader.start();
    }

    @Override public boolean onEvaluateFullscreenMode() { return false; }
    @Override public boolean onEvaluateInputViewShown() { return false; }
    @Override public boolean onShowInputRequested(int flags, boolean configChange) {
        android.util.Log.i("DroidloomIME", "show request configChange=" + configChange);
        showRequest++;
        visibility.show(getCurrentInputEditorInfo() != null
                && getCurrentInputEditorInfo().inputType != InputType.TYPE_NULL);
        publish(visibility.isVisible());
        return false;
    }

    @Override public void hideWindow() {
        // onWindowHidden is conditional on an Android window having been
        // visible. Our headless method has none, but receives hideWindow for
        // the framework's hideSoftInput request just like a graphical IME.
        visibility.hide();
        publish(false);
        android.util.Log.i("DroidloomIME", "hide input panel");
        super.hideWindow();
    }

    @Override public void onStartInput(EditorInfo editor, boolean restarting) {
        android.util.Log.i("DroidloomIME", "start input restarting=" + restarting
                + " editable=" + (editor.inputType != InputType.TYPE_NULL));
        super.onStartInput(editor, restarting);
        session++;
        composing = false;
        visibility.start(editor.inputType != InputType.TYPE_NULL, restarting);
        publish(visibility.isVisible());
    }

    @Override public void onFinishInput() {
        android.util.Log.i("DroidloomIME", "finish input");
        visibility.finish();
        publish(false);
        composing = false;
        super.onFinishInput();
    }

    private void publish(boolean active) {
        try {
            EditorInfo editor = getCurrentInputEditorInfo();
            state = new JSONObject().put("session", session).put("active", active)
                    .put("show_request", showRequest)
                    .put("package", editor == null ? "" : editor.packageName)
                    .put("input_type", editor == null ? 0 : editor.inputType);
            sendState();
        } catch (Exception error) {
            android.util.Log.w("DroidloomIME", "Unable to publish editor lifecycle", error);
        }
    }

    private void sendState() {
        if (state == null || stopped) return;
        final byte[] bytes = (state.toString() + "\n").getBytes(StandardCharsets.UTF_8);
        final LocalSocket target = socket;
        if (target == null) return;
        writes.execute(() -> {
            try { target.getOutputStream().write(bytes); }
            catch (Exception error) { close(target); }
        });
    }

    private void connect() {
        while (!stopped) {
            try (LocalSocket client = new LocalSocket()) {
                client.connect(new LocalSocketAddress("droidloom-ime"));
                socket = client;
                main.post(this::sendState);
                InputStream input = client.getInputStream();
                ByteArrayOutputStream line = new ByteArrayOutputStream();
                int value;
                while (!stopped && (value = input.read()) >= 0) {
                    if (value == '\n') {
                        JSONObject edit = new JSONObject(line.toString("UTF-8"));
                        line.reset();
                        main.post(() -> apply(edit));
                    } else {
                        if (line.size() >= 16384) throw new java.io.IOException("oversized edit");
                        line.write(value);
                    }
                }
            } catch (Exception error) {
                // Reconnect only after transport loss, never poll editor state.
            } finally { socket = null; }
            if (!stopped) {
                try { Thread.sleep(1000); }
                catch (InterruptedException error) { return; }
            }
        }
    }

    private void apply(JSONObject edit) {
        if (state == null || !state.optBoolean("active")
                || edit.optLong("session", -1) != session) return;
        if (edit.optBoolean("dismiss")) {
            // Host dismissal is not Back, Done, or forced focus loss. Tell
            // Android's IMMS so the app's requested IME visibility is reset;
            // tapping the same focused editor can then request it again.
            if (!visibility.dismissFromHost(edit.optLong("show_request", -1), showRequest)) return;
            composing = false;
            InputConnection current = getCurrentInputConnection();
            if (current != null) current.finishComposingText();
            publish(false);
            requestHideSelf(0);
            android.util.Log.i("DroidloomIME", "host dismissed input panel session=" + session);
            return;
        }
        InputConnection connection = getCurrentInputConnection();
        if (connection == null) return;
        connection.beginBatchEdit();
        try {
            int beforeBytes = edit.optInt("before");
            int afterBytes = edit.optInt("after");
            if (beforeBytes != 0 || afterBytes != 0) {
                if (composing) connection.setComposingText("", 1);
                connection.finishComposingText();
                composing = false;
            }
            int before = beforeBytes == 0 ? 0 : ByteOffsets.utf16Length(
                    connection.getTextBeforeCursor(4096, 0), beforeBytes, true);
            int after = afterBytes == 0 ? 0 : ByteOffsets.utf16Length(
                    connection.getTextAfterCursor(4096, 0), afterBytes, false);
            if (before < 0 || after < 0) return;
            if (before != 0 || after != 0) connection.deleteSurroundingText(before, after);
            if (edit.has("commit")) {
                connection.commitText(edit.optString("commit"), 1);
                composing = false;
            }
            if (edit.has("preedit")) {
                String preedit = edit.optString("preedit");
                if (!preedit.isEmpty() || composing) connection.setComposingText(preedit, 1);
                composing = !preedit.isEmpty();
                if (!composing) connection.finishComposingText();
            }
        } finally { connection.endBatchEdit(); }
    }

    private static void close(LocalSocket client) {
        if (client != null) try { client.close(); } catch (Exception ignored) { }
    }

    @Override public void onDestroy() {
        stopped = true;
        close(socket);
        reader.interrupt();
        writes.shutdownNow();
        super.onDestroy();
    }
}
