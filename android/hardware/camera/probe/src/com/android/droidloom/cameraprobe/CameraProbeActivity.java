package com.android.droidloom.cameraprobe;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Color;
import android.graphics.ImageFormat;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCaptureSession;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraDevice;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.CaptureFailure;
import android.hardware.camera2.CaptureRequest;
import android.hardware.camera2.TotalCaptureResult;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.Image;
import android.media.ImageReader;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.os.Process;
import android.os.SystemClock;
import android.util.AtomicFile;
import android.util.Log;
import android.util.Size;
import android.view.WindowManager;
import android.widget.TextView;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Collections;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/** An ordinary-UID, memory-only Camera2 acceptance test. */
public final class CameraProbeActivity extends Activity {
    private static final int WIDTH = 640;
    private static final int HEIGHT = 480;
    private static final long DEADLINE_MS = 15_000;
    private static final long CAPTURE_DEADLINE_MS = 14_000;
    private static final Object RESULT_LOCK = new Object();

    private final Handler main = new Handler(Looper.getMainLooper());
    private TextView text;
    private volatile Run active;
    private Intent pending;
    private boolean resumed;
    private boolean destroyed;

    @Override
    public void onCreate(Bundle state) {
        super.onCreate(state);
        text = new TextView(this);
        text.setTextSize(18);
        text.setTextColor(Color.BLACK);
        text.setBackgroundColor(Color.WHITE);
        int padding = Math.round(20 * getResources().getDisplayMetrics().density);
        text.setPadding(padding, padding, padding, padding);
        setContentView(text);
        pending = getIntent();
    }

    @Override
    public void onResume() {
        super.onResume();
        resumed = true;
        startPending();
    }

    @Override
    public void onPause() {
        resumed = false;
        super.onPause();
    }

    @Override
    public void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        pending = intent;
        if (active != null && !active.published.get()) {
            active.finish(false, "superseded");
        } else {
            startPending();
        }
    }

    @Override
    public void onStop() {
        super.onStop();
        if (active != null && !active.published.get()) active.finish(false, "activity_stopped");
    }

    @Override
    public void onDestroy() {
        destroyed = true;
        pending = null;
        if (active != null && !active.published.get()) active.finish(false, "activity_destroyed");
        super.onDestroy();
    }

    private void startPending() {
        if (!resumed || destroyed || pending == null ||
                (active != null && !active.published.get())) return;
        Intent intent = pending;
        pending = null;
        active = new Run(intent);
        active.start();
    }

    private void show(Run run, String status) {
        if (!destroyed && active == run) {
            text.setText("Droidloom Camera Check\n" + run.mode + " / " + run.lens +
                    "\nToken: " + run.token + "\n\n" + status +
                    "\n\n15-second limit. Images stay in memory.");
        }
    }

    private final class Run {
        final String token;
        final String mode;
        final String lens;
        final long started = SystemClock.elapsedRealtime();
        final AtomicBoolean published = new AtomicBoolean();
        final AtomicBoolean stopping = new AtomicBoolean();
        final ScheduledExecutorService watchdog = Executors.newSingleThreadScheduledExecutor();
        final JSONArray images = new JSONArray();
        final Runnable softTimeout = () -> finish(false, "capture_timeout");
        final Runnable pollCleanup = this::completeIfClosed;
        ScheduledFuture<?> hardTimeout;
        volatile HandlerThread imageThread;
        Handler imageHandler;
        volatile CameraDevice device;
        volatile CameraCaptureSession session;
        volatile ImageReader reader;
        volatile String cameraId = "";
        volatile String reason = "";
        volatile String cleanupError = "";
        volatile boolean permissionGranted;
        volatile boolean openAttempted;
        volatile boolean securityException;
        volatile boolean opening;
        volatile boolean deviceCloseRequested;
        volatile boolean sessionCloseRequested;
        volatile boolean readerCloseRequested;
        volatile boolean deviceClosed;
        volatile boolean sessionClosed;
        volatile boolean readerClosed = true;
        boolean jpegCaptureCompleted;
        boolean success;
        volatile boolean finishing;
        volatile int readImages;
        long lastTimestamp;
        volatile int recordedFrames;
        volatile int capturesCompleted;
        volatile String originalReason = "";
        volatile String lastPhase = "created";
        boolean firstFrameLogged;
        boolean fifthFrameLogged;
        boolean threadStopLogged;

        Run(Intent intent) {
            token = stringExtra(intent, "token");
            mode = stringExtra(intent, "mode");
            lens = stringExtra(intent, "lens");
        }

        void phase(String value) {
            lastPhase = value;
            Log.i("DroidloomCameraProbe", "token=" + token + " phase=" + value +
                    " elapsed_ms=" + (SystemClock.elapsedRealtime() - started) + " reason=" + reason);
        }

        void start() {
            phase("start");
            getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            show(this, "Starting…");
            hardTimeout = watchdog.schedule(this::deadlineExceeded, DEADLINE_MS, TimeUnit.MILLISECONDS);
            main.postDelayed(softTimeout, CAPTURE_DEADLINE_MS);
            try {
                if (!writeResult(base("running", "starting"))) {
                    finish(false, "result_write_failed");
                    return;
                }
                if (token.isEmpty() || token.length() > 128 ||
                        !(mode.equals("deny") || mode.equals("yuv") || mode.equals("jpeg")) ||
                        !(lens.equals("back") || lens.equals("front"))) {
                    finish(false, "invalid_token_mode_or_lens");
                    return;
                }
                permissionGranted = checkSelfPermission(Manifest.permission.CAMERA) ==
                        PackageManager.PERMISSION_GRANTED;
                imageThread = new HandlerThread("camera-probe-images");
                imageThread.start();
                imageHandler = new Handler(imageThread.getLooper());
                CameraManager manager = getSystemService(CameraManager.class);
                if (manager == null) {
                    finish(false, "camera_manager_missing");
                    return;
                }
                int facing = lens.equals("back") ? CameraCharacteristics.LENS_FACING_BACK :
                        CameraCharacteristics.LENS_FACING_FRONT;
                CameraCharacteristics characteristics = null;
                for (String candidate : manager.getCameraIdList()) {
                    CameraCharacteristics current = manager.getCameraCharacteristics(candidate);
                    Integer direction = current.get(CameraCharacteristics.LENS_FACING);
                    if (direction != null && direction == facing) {
                        cameraId = candidate;
                        characteristics = current;
                        break;
                    }
                }
                if (characteristics == null) {
                    finish(false, "no_camera_for_lens");
                    return;
                }
                if (!mode.equals("deny")) {
                    int format = mode.equals("yuv") ? ImageFormat.YUV_420_888 : ImageFormat.JPEG;
                    StreamConfigurationMap streams = characteristics.get(
                            CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
                    Size[] sizes = streams == null ? null : streams.getOutputSizes(format);
                    if (sizes == null || !Arrays.asList(sizes).contains(new Size(WIDTH, HEIGHT))) {
                        finish(false, "640x480_format_unsupported");
                        return;
                    }
                }
                show(this, "Opening " + cameraId + "…");
                openAttempted = true;
                opening = true;
                try {
                    phase("open_call_before");
                    manager.openCamera(cameraId, deviceState, main);
                    phase("open_call_after");
                } catch (SecurityException denied) {
                    opening = false;
                    securityException = true;
                    phase("open_security_exception");
                    finish(mode.equals("deny") && !permissionGranted,
                            mode.equals("deny") && !permissionGranted ? "permission_denied" :
                                    "unexpected_security_exception");
                } catch (CameraAccessException error) {
                    opening = false;
                    finish(false, "open_camera_access_" + error.getReason());
                } catch (RuntimeException error) {
                    opening = false;
                    finish(false, "open_" + error.getClass().getSimpleName());
                }
            } catch (CameraAccessException error) {
                finish(false, "enumeration_camera_access_" + error.getReason());
            } catch (Exception error) {
                finish(false, "setup_" + error.getClass().getSimpleName());
            }
        }

        final CameraDevice.StateCallback deviceState = new CameraDevice.StateCallback() {
            @Override
            public void onOpened(CameraDevice opened) {
                opening = false;
                device = opened;
                phase("onOpened");
                if (finishing || stopping.get()) {
                    closeDevice();
                } else if (mode.equals("deny")) {
                    finish(false, "deny_open_succeeded");
                } else {
                    configure();
                }
            }

            @Override
            public void onDisconnected(CameraDevice disconnected) {
                opening = false;
                device = disconnected;
                finish(false, "camera_disconnected");
                closeDevice();
            }

            @Override
            public void onError(CameraDevice failed, int error) {
                opening = false;
                device = failed;
                finish(false, "camera_error_" + error);
                closeDevice();
            }

            @Override
            public void onClosed(CameraDevice closed) {
                if (device == closed) {
                    deviceClosed = true;
                    phase("device_onClosed");
                    if (!finishing) finish(false, "camera_closed_before_result");
                }
                if (finishing) completeIfClosed();
            }
        };

        void configure() {
            try {
                int format = mode.equals("yuv") ? ImageFormat.YUV_420_888 : ImageFormat.JPEG;
                reader = ImageReader.newInstance(WIDTH, HEIGHT, format, 2);
                readerClosed = false;
                reader.setOnImageAvailableListener(this::readImage, imageHandler);
                device.createCaptureSession(Collections.singletonList(reader.getSurface()),
                        sessionState, main);
                show(this, "Configuring " + mode.toUpperCase(java.util.Locale.ROOT) + "…");
            } catch (CameraAccessException error) {
                finish(false, "configure_camera_access_" + error.getReason());
            } catch (RuntimeException error) {
                finish(false, "configure_" + error.getClass().getSimpleName());
            }
        }

        final CameraCaptureSession.StateCallback sessionState = new CameraCaptureSession.StateCallback() {
            @Override
            public void onConfigured(CameraCaptureSession configured) {
                session = configured;
                phase("onConfigured");
                if (finishing || stopping.get()) {
                    closeSession();
                    return;
                }
                try {
                    int template = mode.equals("yuv") ? CameraDevice.TEMPLATE_PREVIEW :
                            CameraDevice.TEMPLATE_STILL_CAPTURE;
                    CaptureRequest.Builder request = device.createCaptureRequest(template);
                    request.addTarget(reader.getSurface());
                    if (mode.equals("yuv")) {
                        session.setRepeatingRequest(request.build(), captures, main);
                    } else {
                        request.set(CaptureRequest.JPEG_QUALITY, (byte) 90);
                        session.capture(request.build(), captures, main);
                    }
                    show(Run.this, "Receiving frames…");
                } catch (CameraAccessException error) {
                    finish(false, "capture_camera_access_" + error.getReason());
                } catch (RuntimeException error) {
                    finish(false, "capture_" + error.getClass().getSimpleName());
                }
            }

            @Override
            public void onConfigureFailed(CameraCaptureSession failed) {
                session = failed;
                // A failed session is already closed and will not receive onClosed.
                sessionClosed = true;
                finish(false, "session_configuration_failed");
                closeSession();
            }

            @Override
            public void onClosed(CameraCaptureSession closed) {
                if (session == closed) {
                    sessionClosed = true;
                    phase("session_onClosed");
                    if (!finishing) finish(false, "session_closed_before_result");
                }
                if (finishing) completeIfClosed();
            }
        };

        final CameraCaptureSession.CaptureCallback captures = new CameraCaptureSession.CaptureCallback() {
            @Override
            public void onCaptureFailed(CameraCaptureSession captureSession, CaptureRequest request,
                    CaptureFailure failure) {
                if (!finishing) finish(false, "capture_failed_" + failure.getReason());
            }

            @Override
            public void onCaptureSequenceAborted(CameraCaptureSession captureSession, int sequenceId) {
                if (!finishing) finish(false, "capture_sequence_aborted");
            }

            @Override
            public void onCaptureCompleted(CameraCaptureSession captureSession, CaptureRequest request,
                    TotalCaptureResult result) {
                capturesCompleted++;
                if (capturesCompleted == 1 || capturesCompleted == 5) {
                    phase("onCaptureCompleted_" + capturesCompleted);
                }
                if (!finishing && mode.equals("jpeg")) {
                    jpegCaptureCompleted = true;
                    acceptCompleteCapture();
                }
            }
        };

        void readImage(ImageReader source) {
            Image image = null;
            JSONObject statistics = null;
            String error = null;
            try {
                image = source.acquireNextImage();
                if (image == null || stopping.get()) return;
                int target = mode.equals("yuv") ? 5 : 1;
                if (readImages >= target) return;
                if (!firstFrameLogged) {
                    firstFrameLogged = true;
                    phase("first_frame");
                }
                if (readImages == 4 && !fifthFrameLogged) {
                    fifthFrameLogged = true;
                    phase("fifth_frame");
                }
                statistics = imageStatistics(image);
                readImages++;
            } catch (ProbeFailure failure) {
                error = failure.code;
            } catch (OutOfMemoryError exhausted) {
                error = "image_memory_exhausted";
            } catch (Exception failure) {
                error = "image_" + failure.getClass().getSimpleName();
            } finally {
                if (image != null) {
                    try {
                        image.close();
                    } catch (RuntimeException failure) {
                        error = "image_close_" + failure.getClass().getSimpleName();
                    }
                }
            }
            final String failure = error;
            final JSONObject received = statistics;
            main.post(() -> {
                if (active != this || finishing) return;
                if (failure != null) {
                    finish(false, failure);
                } else if (received != null) {
                    images.put(received);
                    recordedFrames = images.length();
                    if (recordedFrames == 1 || recordedFrames == 5) {
                        phase("frame_recorded_" + recordedFrames);
                    }
                    show(this, mode.equals("yuv") ? "YUV frames: " + images.length() + " / 5" :
                            "JPEG decoded in memory");
                    acceptCompleteCapture();
                }
            });
        }

        JSONObject imageStatistics(Image image) throws Exception {
            require(image.getWidth() == WIDTH && image.getHeight() == HEIGHT, "image_size_mismatch");
            int expectedFormat = mode.equals("yuv") ? ImageFormat.YUV_420_888 : ImageFormat.JPEG;
            require(image.getFormat() == expectedFormat, "image_format_mismatch");
            long timestamp = image.getTimestamp();
            require(timestamp > 0 && timestamp > lastTimestamp, "image_timestamp_not_increasing");
            lastTimestamp = timestamp;
            JSONObject result = new JSONObject();
            result.put("format", image.getFormat());
            result.put("width", image.getWidth());
            result.put("height", image.getHeight());
            result.put("timestamp_ns", timestamp);
            Image.Plane[] planes = image.getPlanes();
            require(planes.length == (mode.equals("yuv") ? 3 : 1), "image_plane_count_mismatch");
            JSONArray layout = new JSONArray();
            long totalBytes = 0;
            long totalNonzero = 0;
            byte[] scratch = new byte[4096];
            for (int index = 0; index < planes.length; index++) {
                Image.Plane plane = planes[index];
                ByteBuffer bytes = plane.getBuffer().duplicate();
                int size = bytes.remaining();
                require(size > 0 && size <= 8 * 1024 * 1024, "plane_size_invalid_" + index);
                if (mode.equals("yuv")) {
                    int width = index == 0 ? WIDTH : WIDTH / 2;
                    int height = index == 0 ? HEIGHT : HEIGHT / 2;
                    long rowBytes = (long) (width - 1) * plane.getPixelStride() + 1;
                    long required = (long) (height - 1) * plane.getRowStride() + rowBytes;
                    require(plane.getPixelStride() > 0 && plane.getRowStride() >= rowBytes &&
                            required <= size, "plane_layout_invalid_" + index);
                }
                long nonzero = 0;
                while (bytes.hasRemaining()) {
                    int count = Math.min(bytes.remaining(), scratch.length);
                    bytes.get(scratch, 0, count);
                    for (int i = 0; i < count; i++) if (scratch[i] != 0) nonzero++;
                }
                JSONObject item = new JSONObject();
                item.put("bytes", size);
                item.put("nonzero_bytes", nonzero);
                if (mode.equals("yuv")) {
                    item.put("row_stride", plane.getRowStride());
                    item.put("pixel_stride", plane.getPixelStride());
                }
                layout.put(item);
                totalBytes += size;
                totalNonzero += nonzero;
            }
            result.put("bytes", totalBytes);
            result.put("nonzero_bytes", totalNonzero);
            result.put("planes", layout);
            if (mode.equals("jpeg")) {
                ByteBuffer bytes = planes[0].getBuffer().duplicate();
                require(bytes.remaining() <= 4 * 1024 * 1024, "jpeg_too_large");
                byte[] encoded = new byte[bytes.remaining()];
                Bitmap bitmap = null;
                try {
                    bytes.get(encoded);
                    BitmapFactory.Options bounds = new BitmapFactory.Options();
                    bounds.inJustDecodeBounds = true;
                    BitmapFactory.decodeByteArray(encoded, 0, encoded.length, bounds);
                    require(bounds.outWidth == WIDTH && bounds.outHeight == HEIGHT,
                            "jpeg_bounds_mismatch");
                    BitmapFactory.Options options = new BitmapFactory.Options();
                    options.inScaled = false;
                    bitmap = BitmapFactory.decodeByteArray(encoded, 0, encoded.length, options);
                    require(bitmap != null && bitmap.getWidth() == WIDTH && bitmap.getHeight() == HEIGHT,
                            "jpeg_decode_failed");
                    result.put("decoded_width", bitmap.getWidth());
                    result.put("decoded_height", bitmap.getHeight());
                } finally {
                    if (bitmap != null) bitmap.recycle();
                    Arrays.fill(encoded, (byte) 0);
                }
            }
            return result;
        }

        void acceptCompleteCapture() {
            if (mode.equals("yuv") && images.length() == 5) {
                finish(true, "five_yuv_frames");
            } else if (mode.equals("jpeg") && images.length() == 1 && jpegCaptureCompleted) {
                finish(true, "jpeg_decoded");
            }
        }

        void finish(boolean passed, String detail) {
            if (finishing || published.get()) return;
            finishing = true;
            stopping.set(true);
            success = passed;
            reason = detail;
            originalReason = detail;
            phase("finish");
            main.removeCallbacks(softTimeout);
            show(this, "Closing camera resources…");
            closeSession();
            closeDevice();
            if (reader != null && !readerClosed) {
                try {
                    reader.setOnImageAvailableListener(null, null);
                } catch (RuntimeException error) {
                    cleanupError = "reader_listener_" + error.getClass().getSimpleName();
                }
            }
            if (imageThread != null) {
                imageThread.quitSafely();
                phase("thread_quit_requested");
            }
            completeIfClosed();
        }

        void closeSession() {
            if (session == null || sessionCloseRequested) return;
            sessionCloseRequested = true;
            try {
                phase("session_close_call");
                session.close();
                phase("session_close_returned");
            } catch (RuntimeException error) {
                cleanupError = "session_close_" + error.getClass().getSimpleName();
                phase("session_close_error");
            }
        }

        void closeDevice() {
            if (device == null || deviceCloseRequested) return;
            deviceCloseRequested = true;
            try {
                phase("device_close_call");
                device.close();
                phase("device_close_returned");
            } catch (RuntimeException error) {
                cleanupError = "device_close_" + error.getClass().getSimpleName();
                phase("device_close_error");
            }
        }

        void completeIfClosed() {
            if (!finishing || published.get()) return;
            main.removeCallbacks(pollCleanup);
            boolean threadStopped = imageThread == null || !imageThread.isAlive();
            if (imageThread != null && threadStopped && !threadStopLogged) {
                threadStopLogged = true;
                phase("thread_stopped");
            }
            // Closing a reader invalidates acquired plane buffers; let the worker close its Image first.
            if (threadStopped && reader != null && !readerCloseRequested) {
                readerCloseRequested = true;
                try {
                    phase("reader_close_call");
                    reader.close();
                    readerClosed = true;
                    phase("reader_closed");
                } catch (RuntimeException error) {
                    cleanupError = "reader_close_" + error.getClass().getSimpleName();
                    phase("reader_close_error");
                }
            }
            if (opening || (device != null && !deviceClosed) ||
                    (session != null && !sessionClosed) || !readerClosed || !threadStopped) {
                main.postDelayed(pollCleanup, 25);
                return;
            }
            if (!cleanupError.isEmpty()) {
                success = false;
                reason = cleanupError;
            }
            if (SystemClock.elapsedRealtime() - started >= DEADLINE_MS) {
                success = false;
                reason = "cleanup_deadline_exceeded";
            }
            try {
                JSONObject result = base(success ? "pass" : "fail", reason);
                result.put("camera_id", cameraId);
                result.put("permission_granted", permissionGranted);
                result.put("open_attempted", openAttempted);
                result.put("security_exception", securityException);
                result.put("frames", images.length());
                result.put("images", images);
                JSONObject cleanup = new JSONObject();
                cleanup.put("session_closed", session == null || sessionClosed);
                cleanup.put("device_closed", device == null || deviceClosed);
                cleanup.put("reader_closed", readerClosed);
                cleanup.put("handler_thread_stopped", threadStopped);
                result.put("cleanup", cleanup);
                result.put("cleanup_complete", true);
                if (!published.compareAndSet(false, true)) return;
                boolean saved = writeResult(result);
                if (!saved) {
                    success = false;
                    reason = "result_write_failed";
                }
                hardTimeout.cancel(false);
                watchdog.shutdown();
                if (!destroyed) getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                show(this, (success ? "PASS" : "FAIL") + "\n" + reason);
                if (active == this) startPending();
            } catch (JSONException error) {
                reason = "result_json_failed";
                deadlineExceeded();
            }
        }

        JSONObject base(String status, String detail) throws JSONException {
            JSONObject result = new JSONObject();
            result.put("token", token);
            result.put("mode", mode);
            result.put("lens", lens);
            result.put("status", status);
            result.put("reason", detail);
            result.put("elapsed_ms", SystemClock.elapsedRealtime() - started);
            result.put("uid", Process.myUid());
            result.put("frames", recordedFrames);
            result.put("frames_read", readImages);
            result.put("capture_completed", capturesCompleted);
            result.put("original_reason", originalReason);
            result.put("last_phase", lastPhase);
            JSONObject state = new JSONObject();
            state.put("camera_id", cameraId);
            state.put("permission_granted", permissionGranted);
            state.put("open_attempted", openAttempted);
            state.put("opening", opening);
            state.put("security_exception", securityException);
            state.put("session_present", session != null);
            state.put("session_close_requested", sessionCloseRequested);
            state.put("session_closed", sessionClosed);
            state.put("device_present", device != null);
            state.put("device_close_requested", deviceCloseRequested);
            state.put("device_closed", deviceClosed);
            state.put("reader_present", reader != null);
            state.put("reader_close_requested", readerCloseRequested);
            state.put("reader_closed", readerClosed);
            HandlerThread thread = imageThread;
            state.put("thread_present", thread != null);
            state.put("thread_alive", thread != null && thread.isAlive());
            state.put("thread_state", thread == null ? "not_started" : thread.getState().name());
            state.put("finishing", finishing);
            state.put("cleanup_error", cleanupError);
            result.put("state", state);
            return result;
        }

        void deadlineExceeded() {
            if (active != this || !published.compareAndSet(false, true)) return;
            String timeoutPhase = lastPhase;
            Log.i("DroidloomCameraProbe", "token=" + token + " phase=hard_timeout last_phase=" +
                    timeoutPhase + " original_reason=" + originalReason + " frames=" + recordedFrames +
                    " frames_read=" + readImages);
            stopping.set(true);
            try {
                JSONObject result = base("fail", "hard_deadline_cleanup_incomplete");
                result.put("last_phase", timeoutPhase);
                result.put("cleanup_complete", false);
                writeResult(result);
            } catch (JSONException ignored) {
                // A missing final result is never a successful probe.
            }
            // Do not leave a stuck Binder operation, image thread, or camera lease behind.
            Process.killProcess(Process.myPid());
        }
    }

    private boolean writeResult(JSONObject result) {
        synchronized (RESULT_LOCK) {
            AtomicFile file = new AtomicFile(new File(getFilesDir(), "result.json"));
            FileOutputStream output = null;
            try {
                output = file.startWrite();
                output.write(result.toString().getBytes(StandardCharsets.UTF_8));
                file.finishWrite(output);
                return true;
            } catch (Exception error) {
                if (output != null) file.failWrite(output);
                return false;
            }
        }
    }

    private static String stringExtra(Intent intent, String key) {
        String value = intent == null ? null : intent.getStringExtra(key);
        return value == null ? "" : value;
    }

    private static void require(boolean condition, String code) throws ProbeFailure {
        if (!condition) throw new ProbeFailure(code);
    }

    private static final class ProbeFailure extends Exception {
        final String code;
        ProbeFailure(String code) {
            super(code);
            this.code = code;
        }
    }
}
