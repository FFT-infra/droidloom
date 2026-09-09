/*
 * Copyright 2026 Droidloom
 * SPDX-License-Identifier: GPL-3.0-or-later
 * See LICENSE and LICENSES/GPL-3.0-or-later.txt for the license terms.
 */

package com.android.droidloom.input;

import android.graphics.Rect;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.IBinder;
import android.os.Parcel;
import android.os.ParcelFileDescriptor;
import android.os.RemoteException;
import android.util.Log;
import android.view.InputDevice;
import android.view.InputEvent;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;

/**
 * Narrow platform adapter from Droidloom's routed input records to InputManager.
 *
 * <p>The physical touchscreen remains private to Denial. This process accepts
 * only fixed-size records from the root/system-only init socket and targets the
 * Android logical display already bound to the selected native window.</p>
 */
public final class InputBridge {
    private static final String TAG = "DroidloomInput";
    private static final boolean INPUT_TRACE = "1".equals(System.getenv("DROIDLOOM_INPUT_TRACE"));
    private static final String SOCKET_ENV = "ANDROID_SOCKET_droidloom_input";
    private static final int RECORD_BYTES = 40;
    private static final int PROTOCOL_MAJOR = 5;
    private static final String BUILD_COMPATIBILITY =
            "DROIDLOOM_INPUT_ABI=" + PROTOCOL_MAJOR + ";";
    private static final int KIND_TOUCH = 1;
    private static final int KIND_KEY = 2;
    private static final int KIND_TASK_BOUNDS = 3;
    private static final int KIND_TASK_FOCUS = 4;
    private static final int KIND_TASK_CLOSE = 5;
    private static final int TOUCH_ACTION_DOWN = 0;
    private static final int TOUCH_ACTION_MOTION = 1;
    private static final int TOUCH_ACTION_UP = 2;
    private static final int TOUCH_ACTION_CANCEL = 3;
    private static final int KEY_ACTION_DOWN = 0;
    private static final int KEY_ACTION_UP = 1;
    private static final int MAX_EVDEV_KEYCODE = 0x2ff;
    private static final float FIXED_SCALE = 65536.0f;
    private static final float PRESSURE_SCALE = 65535.0f;
    private static final int INJECT_ASYNC = 0;
    private static final int RESIZE_MODE_RESIZEABLE = 2;
    private static final int RESIZE_MODE_SYSTEM = 0;
    private static final long TASK_RESIZE_DEBOUNCE_MILLIS = 80;
    private static final int GET_TASK_INPUT_TOKEN_TRANSACTION = 0x00444c01;
    private static final int INJECT_TO_APPLICATION_TRANSACTION = 0x00444c02;
    private static final String SURFACE_FLINGER_SERVICE = "SurfaceFlinger";
    private static final String SURFACE_FLINGER_DESCRIPTOR = "android.ui.ISurfaceComposer";
    private static final String ACTIVITY_TASK_SERVICE = "activity_task";
    private static final String INPUT_SERVICE = "inputflinger";
    private static final String INPUT_DESCRIPTOR = "android.os.IInputFlinger";

    private final Map<Long, Gesture> mGestures = new HashMap<>();
    private final Map<KeyIdentity, KeyState> mKeys = new HashMap<>();
    private final Map<Long, Integer> mMetaStates = new HashMap<>();
    private final Map<Integer, ScheduledFuture<?>> mPendingTaskBounds = new HashMap<>();
    private final ScheduledExecutorService mTaskResizeExecutor =
            Executors.newSingleThreadScheduledExecutor();
    private final Method mSetDisplayId;
    private final Method mGetSystemService;
    private final Method mAsActivityTaskManagerInterface;
    private final Method mSetTaskResizeable;
    private final Method mGetTaskBounds;
    private final Method mSetFocusedTask;
    private final Method mRemoveTask;
    private final Method mGetTasks;
    private final Method mResizeTask;
    private final Field mTaskId;
    private final Field mTaskDisplayId;

    private InputBridge() throws ReflectiveOperationException {
        // ServiceManager and ActivityTaskManager are hidden framework APIs.
        // Keep their use at this tiny runtime boundary. The observer also
        // uses the pinned platform TaskStackListener. The two private transactions are
        // implemented natively by Droidloom's SurfaceFlinger and InputFlinger,
        // leaving the boot framework and its Binder ABI untouched.
        final Class<?> serviceManagerClass = Class.forName("android.os.ServiceManager");
        mGetSystemService = serviceManagerClass.getMethod("getService", String.class);
        mSetDisplayId = InputEvent.class.getDeclaredMethod("setDisplayId", int.class);
        mSetDisplayId.setAccessible(true);

        final Class<?> activityTaskManagerInterface =
                Class.forName("android.app.IActivityTaskManager");
        final Class<?> activityTaskManagerStub =
                Class.forName("android.app.IActivityTaskManager$Stub");
        mAsActivityTaskManagerInterface =
                activityTaskManagerStub.getMethod("asInterface", IBinder.class);
        mSetTaskResizeable = activityTaskManagerInterface.getMethod(
                "setTaskResizeable", int.class, int.class);
        mGetTaskBounds = activityTaskManagerInterface.getMethod("getTaskBounds", int.class);
        mSetFocusedTask = activityTaskManagerInterface.getMethod("setFocusedTask", int.class);
        mRemoveTask = activityTaskManagerInterface.getMethod("removeTask", int.class);
        mGetTasks = activityTaskManagerInterface.getMethod(
                "getTasks", int.class, boolean.class, boolean.class, int.class);
        mResizeTask = activityTaskManagerInterface.getMethod(
                "resizeTask", int.class, Rect.class, int.class);

        final Class<?> taskInfoClass = Class.forName("android.app.TaskInfo");
        mTaskId = taskInfoClass.getField("taskId");
        mTaskDisplayId = taskInfoClass.getField("displayId");
    }

    public static void main(String[] args) {
        try {
            TextInputBridge.start();
            ClipboardRelay.start();
            TaskObserver.start();
            new InputBridge().run();
        } catch (Throwable error) {
            Log.e(TAG, "Input bridge terminated", error);
            System.exit(1);
        }
    }

    private void run() throws IOException {
        final String descriptor = System.getenv(SOCKET_ENV);
        if (descriptor == null) {
            throw new IOException("Android init did not provide " + SOCKET_ENV);
        }
        final int rawDescriptor;
        try {
            rawDescriptor = Integer.parseInt(descriptor);
        } catch (NumberFormatException error) {
            throw new IOException("Invalid Android init socket descriptor", error);
        }

        try (ParcelFileDescriptor owner = ParcelFileDescriptor.adoptFd(rawDescriptor);
                LocalServerSocket listener = new LocalServerSocket(owner.getFileDescriptor())) {
            Log.i(TAG, "Android input injection bridge is ready; " + BUILD_COMPATIBILITY);
            while (true) {
                try (LocalSocket client = listener.accept()) {
                    resetInputState();
                    serve(client.getInputStream());
                } catch (IOException error) {
                    Log.w(TAG, "Input client disconnected", error);
                } finally {
                    resetInputState();
                }
            }
        }
    }

    private void resetInputState() {
        resetTransientInputState();
        synchronized (mPendingTaskBounds) {
            for (ScheduledFuture<?> pending : mPendingTaskBounds.values()) {
                pending.cancel(false);
            }
            mPendingTaskBounds.clear();
        }
    }

    private void serve(InputStream input) throws IOException {
        final byte[] record = new byte[RECORD_BYTES];
        while (true) {
            final int count = input.read(record);
            if (count < 0) {
                return;
            }
            if (count != RECORD_BYTES) {
                Log.w(TAG, "Rejected input record of " + count + " bytes");
                continue;
            }
            try {
                inject(decode(record));
            } catch (SecurityException error) {
                Log.e(TAG, "InputManager rejected Droidloom's privileged injector", error);
                resetTransientInputState();
            } catch (RuntimeException error) {
                Log.w(TAG, "Rejected routed input record", error);
                resetTransientInputState();
            }
        }
    }

    private void resetTransientInputState() {
        mGestures.clear();
        mKeys.clear();
        mMetaStates.clear();
    }

    private static RoutedRecord decode(byte[] bytes) {
        if (bytes[0] != 'D' || bytes[1] != 'L' || bytes[2] != 'I' || bytes[3] != 'N') {
            throw new IllegalArgumentException("invalid input record marker");
        }
        final ByteBuffer record = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
        record.position(4);
        final int receivedProtocol = Short.toUnsignedInt(record.getShort());
        if (receivedProtocol != PROTOCOL_MAJOR) {
            throw new IllegalArgumentException("Droidloom component mismatch: input sender v"
                    + receivedProtocol + ", bridge v" + PROTOCOL_MAJOR
                    + "; reinstall a matching Droidloom bundle");
        }
        final int kind = Byte.toUnsignedInt(record.get());
        final int action = Byte.toUnsignedInt(record.get());
        if (kind == KIND_TASK_BOUNDS) {
            final int taskId = record.getInt();
            final int width = record.getInt();
            final int height = record.getInt();
            final int displayId = record.getInt();
            final int scaleNumerator = record.getInt();
            final int scaleDenominator = record.getInt();
            if (action != 0 || taskId <= 0 || width <= 0 || height <= 0
                    || displayId < 0 || scaleNumerator <= 0 || scaleDenominator <= 0) {
                throw new IllegalArgumentException("invalid task-bounds record");
            }
            while (record.hasRemaining()) {
                if (record.get() != 0) {
                    throw new IllegalArgumentException("invalid task-bounds reserved field");
                }
            }
            return new TaskBoundsRecord(taskId, displayId, width, height);
        }
        if (kind == KIND_TASK_FOCUS) {
            final int taskId = record.getInt();
            if (action > 1 || taskId <= 0) {
                throw new IllegalArgumentException("invalid task-focus record");
            }
            while (record.hasRemaining()) {
                if (record.get() != 0) {
                    throw new IllegalArgumentException("invalid task-focus reserved field");
                }
            }
            return new TaskFocusRecord(taskId, action != 0);
        }
        if (kind == KIND_TASK_CLOSE) {
            final int taskId = record.getInt();
            if (action != 0 || taskId <= 0) {
                throw new IllegalArgumentException("invalid task-close record");
            }
            while (record.hasRemaining()) {
                if (record.get() != 0) {
                    throw new IllegalArgumentException("invalid task-close reserved field");
                }
            }
            return new TaskCloseRecord(taskId);
        }
        final int displayId = record.getInt();
        final int code = record.getInt();
        final long timestampNanos = record.getLong();
        if (displayId < 0) {
            throw new IllegalArgumentException("invalid display field");
        }
        if (kind == KIND_TOUCH) {
            if (action > TOUCH_ACTION_CANCEL || code < 0 || code > 31) {
                throw new IllegalArgumentException("invalid touch action or pointer field");
            }
            final int xFixed = record.getInt();
            final int yFixed = record.getInt();
            final int pressure = Short.toUnsignedInt(record.getShort());
            if (record.getShort() != 0) {
                throw new IllegalArgumentException("invalid touch reserved field");
            }
            final int taskId = record.getInt();
            if (taskId <= 0) {
                throw new IllegalArgumentException("invalid touch task field");
            }
            return new TouchRecord(
                    displayId,
                    taskId,
                    action,
                    code,
                    timestampNanos,
                    xFixed / FIXED_SCALE,
                    yFixed / FIXED_SCALE,
                    pressure / PRESSURE_SCALE);
        }
        if (kind == KIND_KEY) {
            if (action > KEY_ACTION_UP || code < 0 || code > MAX_EVDEV_KEYCODE) {
                throw new IllegalArgumentException("invalid key action or scan-code field");
            }
            final int repeat = Short.toUnsignedInt(record.getShort());
            if (record.getShort() != 0) {
                throw new IllegalArgumentException("invalid key reserved field");
            }
            final long routeSerial = record.getLong();
            final int taskId = record.getInt();
            if (taskId <= 0) {
                throw new IllegalArgumentException("invalid key task field");
            }
            return new KeyRecord(
                    displayId, taskId, action, code, timestampNanos, repeat, routeSerial);
        }
        throw new IllegalArgumentException("unsupported input record kind");
    }

    private void inject(RoutedRecord record) {
        if (record instanceof TouchRecord) {
            inject((TouchRecord) record);
        } else if (record instanceof KeyRecord) {
            inject((KeyRecord) record);
        } else if (record instanceof TaskBoundsRecord) {
            scheduleTaskBounds((TaskBoundsRecord) record);
        } else if (record instanceof TaskFocusRecord) {
            focusTask((TaskFocusRecord) record);
        } else if (record instanceof TaskCloseRecord) {
            closeTask((TaskCloseRecord) record);
        } else {
            throw new IllegalArgumentException("unsupported decoded input record");
        }
    }

    private void focusTask(TaskFocusRecord record) {
        // Android has no useful "unfocus this one task" operation. The next
        // positive wl_keyboard.enter selects its replacement, so only mirror
        // positive focus transitions.
        if (!record.focused) {
            return;
        }
        try {
            final Object activityTaskManager = activityTaskManager();
            mSetFocusedTask.invoke(activityTaskManager, record.taskId);
            Log.i(TAG, "Task " + record.taskId + " focused from host keyboard focus");
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Android task focus API failed", error);
        }
    }

    private void closeTask(TaskCloseRecord record) {
        try {
            final Object activityTaskManager = activityTaskManager();
            final boolean removed = (Boolean) mRemoveTask.invoke(
                    activityTaskManager, record.taskId);
            if (removed) {
                Log.i(TAG, "Removed task " + record.taskId + " after host close");
            } else {
                Log.i(TAG, "Host-closed task " + record.taskId + " was already absent");
            }
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Android task removal API failed", error);
        }
    }

    private void scheduleTaskBounds(TaskBoundsRecord record) {
        synchronized (mPendingTaskBounds) {
            final ScheduledFuture<?> previous = mPendingTaskBounds.remove(record.taskId);
            if (previous != null) {
                previous.cancel(false);
            }
            final ScheduledFuture<?> pending = mTaskResizeExecutor.schedule(() -> {
                try {
                    resizeTask(record);
                } catch (RuntimeException error) {
                    Log.w(TAG, "Rejected coalesced task bounds for " + record.taskId, error);
                } finally {
                    synchronized (mPendingTaskBounds) {
                        final ScheduledFuture<?> current = mPendingTaskBounds.get(record.taskId);
                        if (current != null && current.isDone()) {
                            mPendingTaskBounds.remove(record.taskId);
                        }
                    }
                }
            }, TASK_RESIZE_DEBOUNCE_MILLIS, TimeUnit.MILLISECONDS);
            mPendingTaskBounds.put(record.taskId, pending);
        }
    }

    private void resizeTask(TaskBoundsRecord record) {
        try {
            final Object activityTaskManager = activityTaskManager();
            final Rect requestedBounds = new Rect(0, 0, record.width, record.height);
            final Rect previousBounds = (Rect) mGetTaskBounds.invoke(
                    activityTaskManager, record.taskId);

            requireTaskDisplay(activityTaskManager, record);
            mSetTaskResizeable.invoke(
                    activityTaskManager, record.taskId, RESIZE_MODE_RESIZEABLE);
            // WCT drops bounds changes for tasks without a TaskOrganizer, as
            // used by Droidloom's minimal SystemUI. ActivityTaskManager owns
            // this privileged resize path and collects the task into a sync
            // transition when Shell transitions are enabled, updating its
            // configuration and surface crop together for organized tasks too.
            mResizeTask.invoke(activityTaskManager, record.taskId, requestedBounds,
                    RESIZE_MODE_SYSTEM);
            final Rect appliedBounds = (Rect) mGetTaskBounds.invoke(
                    activityTaskManager, record.taskId);
            Log.i(TAG, "Task " + record.taskId + " bounds " + previousBounds
                    + " -> " + appliedBounds + " (requested " + requestedBounds + ")");
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Android task resize API failed", error);
        }
    }

    private void requireTaskDisplay(Object activityTaskManager, TaskBoundsRecord record)
            throws ReflectiveOperationException {
        final Object result = mGetTasks.invoke(
                activityTaskManager, 1000, false, false, record.displayId);
        if (!(result instanceof List<?>)) {
            throw new IllegalStateException("Android task query returned no task list");
        }
        for (Object task : (List<?>) result) {
            if (mTaskId.getInt(task) == record.taskId) {
                if (mTaskDisplayId.getInt(task) != record.displayId) {
                    throw new IllegalStateException("Android task moved to a different display");
                }
                return;
            }
        }
        throw new IllegalStateException("Android task is no longer running");
    }

    private void inject(TouchRecord record) {
        final long gestureIdentity = taskIdentity(record.displayId, record.taskId);
        Gesture gesture = mGestures.get(gestureIdentity);
        if (record.action == TOUCH_ACTION_DOWN) {
            if (gesture == null) {
                gesture = new Gesture(
                        record.timestampNanos / 1_000_000L,
                        taskInputApplicationToken(record.taskId));
                mGestures.put(gestureIdentity, gesture);
            }
            if (gesture.pointers.containsKey(record.pointerId)) {
                throw new IllegalStateException("duplicate touch down");
            }
            gesture.pointers.put(record.pointerId, Pointer.from(record));
            final int pointerIndex = gesture.indexOf(record.pointerId);
            final int action = gesture.pointers.size() == 1
                    ? MotionEvent.ACTION_DOWN
                    : MotionEvent.ACTION_POINTER_DOWN
                            | (pointerIndex << MotionEvent.ACTION_POINTER_INDEX_SHIFT);
            injectMotionEvent(record, gesture, action);
            return;
        }

        if (gesture == null || !gesture.pointers.containsKey(record.pointerId)) {
            throw new IllegalStateException("touch update has no matching down");
        }
        gesture.pointers.put(record.pointerId, Pointer.from(record));

        if (record.action == TOUCH_ACTION_MOTION) {
            injectMotionEvent(record, gesture, MotionEvent.ACTION_MOVE);
        } else if (record.action == TOUCH_ACTION_UP) {
            final int pointerIndex = gesture.indexOf(record.pointerId);
            final int action = gesture.pointers.size() == 1
                    ? MotionEvent.ACTION_UP
                    : MotionEvent.ACTION_POINTER_UP
                            | (pointerIndex << MotionEvent.ACTION_POINTER_INDEX_SHIFT);
            injectMotionEvent(record, gesture, action);
            gesture.pointers.remove(record.pointerId);
            if (gesture.pointers.isEmpty()) {
                mGestures.remove(gestureIdentity);
            }
        } else if (record.action == TOUCH_ACTION_CANCEL) {
            injectMotionEvent(record, gesture, MotionEvent.ACTION_CANCEL);
            mGestures.remove(gestureIdentity);
        }
    }

    private void injectMotionEvent(TouchRecord record, Gesture gesture, int action) {
        final Rect taskBounds = taskBounds(record.taskId);
        final int pointerCount = gesture.pointers.size();
        final MotionEvent.PointerProperties[] properties =
                new MotionEvent.PointerProperties[pointerCount];
        final MotionEvent.PointerCoords[] coordinates =
                new MotionEvent.PointerCoords[pointerCount];
        int index = 0;
        for (Map.Entry<Integer, Pointer> entry : gesture.pointers.entrySet()) {
            final MotionEvent.PointerProperties pointerProperties =
                    new MotionEvent.PointerProperties();
            pointerProperties.id = entry.getKey();
            pointerProperties.toolType = MotionEvent.TOOL_TYPE_FINGER;
            properties[index] = pointerProperties;

            final MotionEvent.PointerCoords pointerCoordinates = new MotionEvent.PointerCoords();
            pointerCoordinates.x = entry.getValue().x + taskBounds.left;
            pointerCoordinates.y = entry.getValue().y + taskBounds.top;
            pointerCoordinates.pressure = action == MotionEvent.ACTION_UP ? 0.0f
                    : entry.getValue().pressure;
            pointerCoordinates.size = 1.0f;
            coordinates[index] = pointerCoordinates;
            index++;
        }

        final long eventTime = Math.max(gesture.downTimeMillis, record.timestampNanos / 1_000_000L);
        final MotionEvent event = MotionEvent.obtain(
                gesture.downTimeMillis,
                eventTime,
                action,
                pointerCount,
                properties,
                coordinates,
                0,
                0,
                1.0f,
                1.0f,
                0,
                0,
                InputDevice.SOURCE_TOUCHSCREEN,
                0);
        try {
            mSetDisplayId.invoke(event, record.displayId);
            if (!injectInputEventToApplication(event, gesture.applicationToken)) {
                Log.w(TAG, "InputManager rejected touch for display " + record.displayId);
            }
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Android input injection API failed", error);
        } finally {
            event.recycle();
        }
    }

    private Rect taskBounds(int taskId) {
        try {
            final Object activityTaskManager = activityTaskManager();
            final Rect bounds = (Rect) mGetTaskBounds.invoke(activityTaskManager, taskId);
            if (bounds == null || bounds.isEmpty()) {
                throw new IllegalStateException("Android task has no input bounds");
            }
            return bounds;
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Android task-bounds query failed", error);
        }
    }

    private void inject(KeyRecord record) {
        final int keyCode = androidKeyCode(record.scanCode);
        if (keyCode == KeyEvent.KEYCODE_UNKNOWN) {
            throw new IllegalArgumentException("unsupported evdev key code " + record.scanCode);
        }
        final KeyIdentity identity = new KeyIdentity(
                record.displayId, record.taskId, record.scanCode);
        final long eventTimeMillis = record.timestampNanos / 1_000_000L;
        final boolean pressed = record.action == KEY_ACTION_DOWN;
        final KeyState existing = mKeys.get(identity);
        final long downTimeMillis;
        final IBinder applicationToken;
        if (pressed) {
            if (record.repeat == 0 && existing != null) {
                throw new IllegalStateException("duplicate key down");
            }
            downTimeMillis = existing == null ? eventTimeMillis : existing.downTimeMillis;
            applicationToken = existing == null
                    ? taskInputApplicationToken(record.taskId)
                    : existing.applicationToken;
            mKeys.put(identity, new KeyState(downTimeMillis, applicationToken));
        } else {
            if (existing == null) {
                throw new IllegalStateException("key up has no matching down");
            }
            downTimeMillis = existing.downTimeMillis;
            applicationToken = existing.applicationToken;
            mKeys.remove(identity);
        }

        final int metaState = updateMetaState(
                record.displayId, record.taskId, keyCode, pressed);
        final KeyEvent event = new KeyEvent(
                downTimeMillis,
                Math.max(downTimeMillis, eventTimeMillis),
                pressed ? KeyEvent.ACTION_DOWN : KeyEvent.ACTION_UP,
                keyCode,
                record.repeat,
                metaState,
                KeyCharacterMap.VIRTUAL_KEYBOARD,
                record.scanCode,
                KeyEvent.FLAG_FROM_SYSTEM | KeyEvent.FLAG_VIRTUAL_HARD_KEY,
                InputDevice.SOURCE_KEYBOARD);
        try {
            mSetDisplayId.invoke(event, record.displayId);
            if (!injectInputEventToApplication(event, applicationToken)) {
                Log.w(TAG, "InputManager rejected key for display " + record.displayId);
            } else if (INPUT_TRACE) {
                Log.i(TAG, "Key trace stage=android serial=" + record.routeSerial
                        + " task=" + record.taskId + " display=" + record.displayId
                        + " action=" + record.action + " scanCode=" + record.scanCode
                        + " repeat=" + record.repeat);
            }
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Android key injection API failed", error);
        }
    }

    private int updateMetaState(int displayId, int taskId, int keyCode, boolean pressed) {
        final int bits;
        switch (keyCode) {
            case KeyEvent.KEYCODE_SHIFT_LEFT:
                bits = KeyEvent.META_SHIFT_ON | KeyEvent.META_SHIFT_LEFT_ON;
                break;
            case KeyEvent.KEYCODE_SHIFT_RIGHT:
                bits = KeyEvent.META_SHIFT_ON | KeyEvent.META_SHIFT_RIGHT_ON;
                break;
            case KeyEvent.KEYCODE_ALT_LEFT:
                bits = KeyEvent.META_ALT_ON | KeyEvent.META_ALT_LEFT_ON;
                break;
            case KeyEvent.KEYCODE_ALT_RIGHT:
                bits = KeyEvent.META_ALT_ON | KeyEvent.META_ALT_RIGHT_ON;
                break;
            case KeyEvent.KEYCODE_CTRL_LEFT:
                bits = KeyEvent.META_CTRL_ON | KeyEvent.META_CTRL_LEFT_ON;
                break;
            case KeyEvent.KEYCODE_CTRL_RIGHT:
                bits = KeyEvent.META_CTRL_ON | KeyEvent.META_CTRL_RIGHT_ON;
                break;
            case KeyEvent.KEYCODE_META_LEFT:
                bits = KeyEvent.META_META_ON | KeyEvent.META_META_LEFT_ON;
                break;
            case KeyEvent.KEYCODE_META_RIGHT:
                bits = KeyEvent.META_META_ON | KeyEvent.META_META_RIGHT_ON;
                break;
            default:
                bits = 0;
                break;
        }
        final long target = taskIdentity(displayId, taskId);
        final int previous = mMetaStates.containsKey(target) ? mMetaStates.get(target) : 0;
        final int updated = pressed ? previous | bits : previous & ~bits;
        if (updated == 0) {
            mMetaStates.remove(target);
        } else {
            mMetaStates.put(target, updated);
        }
        return KeyEvent.normalizeMetaState(updated);
    }

    private IBinder taskInputApplicationToken(int taskId) {
        final Parcel data = Parcel.obtain();
        final Parcel reply = Parcel.obtain();
        try {
            final IBinder surfaceFlinger = systemService(SURFACE_FLINGER_SERVICE);
            if (surfaceFlinger == null) {
                throw new IllegalStateException("SurfaceFlinger is not ready");
            }
            data.writeInterfaceToken(SURFACE_FLINGER_DESCRIPTOR);
            data.writeInt(taskId);
            if (!surfaceFlinger.transact(
                    GET_TASK_INPUT_TOKEN_TRANSACTION, data, reply, 0)) {
                throw new IllegalStateException("Android rejected the task input-token query");
            }
            reply.readException();
            final IBinder token = reply.readStrongBinder();
            if (token == null) {
                throw new IllegalStateException(
                        "Android task " + taskId + " has no input application token");
            }
            return token;
        } catch (ReflectiveOperationException | RemoteException error) {
            throw new IllegalStateException("Android task input-token query failed", error);
        } finally {
            reply.recycle();
            data.recycle();
        }
    }

    private boolean injectInputEventToApplication(InputEvent event, IBinder applicationToken)
            throws ReflectiveOperationException {
        final IBinder inputManager = systemService(INPUT_SERVICE);
        if (inputManager == null) {
            throw new IllegalStateException("InputManager is not ready");
        }
        final Parcel data = Parcel.obtain();
        final Parcel reply = Parcel.obtain();
        try {
            data.writeInterfaceToken(INPUT_DESCRIPTOR);
            data.writeStrongBinder(applicationToken);
            data.writeInt(INJECT_ASYNC);
            event.writeToParcel(data, 0);
            if (!inputManager.transact(INJECT_TO_APPLICATION_TRANSACTION, data, reply, 0)) {
                throw new IllegalStateException("Android rejected targeted input injection");
            }
            reply.readException();
            return reply.readBoolean();
        } catch (RemoteException error) {
            throw new IllegalStateException("Android targeted input injection failed", error);
        } finally {
            reply.recycle();
            data.recycle();
        }
    }

    private IBinder systemService(String name) throws ReflectiveOperationException {
        return (IBinder) mGetSystemService.invoke(null, name);
    }

    private Object activityTaskManager() throws ReflectiveOperationException {
        final IBinder binder = systemService(ACTIVITY_TASK_SERVICE);
        if (binder == null) {
            throw new IllegalStateException("ActivityTaskManager is not ready");
        }
        final Object activityTaskManager = mAsActivityTaskManagerInterface.invoke(null, binder);
        if (activityTaskManager == null) {
            throw new IllegalStateException("ActivityTaskManager is not ready");
        }
        return activityTaskManager;
    }

    private static long taskIdentity(int displayId, int taskId) {
        return ((long) displayId << 32) | Integer.toUnsignedLong(taskId);
    }

    private static int androidKeyCode(int scanCode) {
        switch (scanCode) {
            case 1: return KeyEvent.KEYCODE_ESCAPE;
            case 2: return KeyEvent.KEYCODE_1;
            case 3: return KeyEvent.KEYCODE_2;
            case 4: return KeyEvent.KEYCODE_3;
            case 5: return KeyEvent.KEYCODE_4;
            case 6: return KeyEvent.KEYCODE_5;
            case 7: return KeyEvent.KEYCODE_6;
            case 8: return KeyEvent.KEYCODE_7;
            case 9: return KeyEvent.KEYCODE_8;
            case 10: return KeyEvent.KEYCODE_9;
            case 11: return KeyEvent.KEYCODE_0;
            case 12: return KeyEvent.KEYCODE_MINUS;
            case 13: return KeyEvent.KEYCODE_EQUALS;
            case 14: return KeyEvent.KEYCODE_DEL;
            case 15: return KeyEvent.KEYCODE_TAB;
            case 16: return KeyEvent.KEYCODE_Q;
            case 17: return KeyEvent.KEYCODE_W;
            case 18: return KeyEvent.KEYCODE_E;
            case 19: return KeyEvent.KEYCODE_R;
            case 20: return KeyEvent.KEYCODE_T;
            case 21: return KeyEvent.KEYCODE_Y;
            case 22: return KeyEvent.KEYCODE_U;
            case 23: return KeyEvent.KEYCODE_I;
            case 24: return KeyEvent.KEYCODE_O;
            case 25: return KeyEvent.KEYCODE_P;
            case 26: return KeyEvent.KEYCODE_LEFT_BRACKET;
            case 27: return KeyEvent.KEYCODE_RIGHT_BRACKET;
            case 28: return KeyEvent.KEYCODE_ENTER;
            case 29: return KeyEvent.KEYCODE_CTRL_LEFT;
            case 30: return KeyEvent.KEYCODE_A;
            case 31: return KeyEvent.KEYCODE_S;
            case 32: return KeyEvent.KEYCODE_D;
            case 33: return KeyEvent.KEYCODE_F;
            case 34: return KeyEvent.KEYCODE_G;
            case 35: return KeyEvent.KEYCODE_H;
            case 36: return KeyEvent.KEYCODE_J;
            case 37: return KeyEvent.KEYCODE_K;
            case 38: return KeyEvent.KEYCODE_L;
            case 39: return KeyEvent.KEYCODE_SEMICOLON;
            case 40: return KeyEvent.KEYCODE_APOSTROPHE;
            case 41: return KeyEvent.KEYCODE_GRAVE;
            case 42: return KeyEvent.KEYCODE_SHIFT_LEFT;
            case 43: return KeyEvent.KEYCODE_BACKSLASH;
            case 44: return KeyEvent.KEYCODE_Z;
            case 45: return KeyEvent.KEYCODE_X;
            case 46: return KeyEvent.KEYCODE_C;
            case 47: return KeyEvent.KEYCODE_V;
            case 48: return KeyEvent.KEYCODE_B;
            case 49: return KeyEvent.KEYCODE_N;
            case 50: return KeyEvent.KEYCODE_M;
            case 51: return KeyEvent.KEYCODE_COMMA;
            case 52: return KeyEvent.KEYCODE_PERIOD;
            case 53: return KeyEvent.KEYCODE_SLASH;
            case 54: return KeyEvent.KEYCODE_SHIFT_RIGHT;
            case 56: return KeyEvent.KEYCODE_ALT_LEFT;
            case 57: return KeyEvent.KEYCODE_SPACE;
            case 58: return KeyEvent.KEYCODE_CAPS_LOCK;
            case 59: return KeyEvent.KEYCODE_F1;
            case 60: return KeyEvent.KEYCODE_F2;
            case 61: return KeyEvent.KEYCODE_F3;
            case 62: return KeyEvent.KEYCODE_F4;
            case 63: return KeyEvent.KEYCODE_F5;
            case 64: return KeyEvent.KEYCODE_F6;
            case 65: return KeyEvent.KEYCODE_F7;
            case 66: return KeyEvent.KEYCODE_F8;
            case 67: return KeyEvent.KEYCODE_F9;
            case 68: return KeyEvent.KEYCODE_F10;
            case 87: return KeyEvent.KEYCODE_F11;
            case 88: return KeyEvent.KEYCODE_F12;
            case 96: return KeyEvent.KEYCODE_NUMPAD_ENTER;
            case 97: return KeyEvent.KEYCODE_CTRL_RIGHT;
            case 100: return KeyEvent.KEYCODE_ALT_RIGHT;
            case 102: return KeyEvent.KEYCODE_MOVE_HOME;
            case 103: return KeyEvent.KEYCODE_DPAD_UP;
            case 104: return KeyEvent.KEYCODE_PAGE_UP;
            case 105: return KeyEvent.KEYCODE_DPAD_LEFT;
            case 106: return KeyEvent.KEYCODE_DPAD_RIGHT;
            case 107: return KeyEvent.KEYCODE_MOVE_END;
            case 108: return KeyEvent.KEYCODE_DPAD_DOWN;
            case 109: return KeyEvent.KEYCODE_PAGE_DOWN;
            case 110: return KeyEvent.KEYCODE_INSERT;
            case 111: return KeyEvent.KEYCODE_FORWARD_DEL;
            case 125: return KeyEvent.KEYCODE_META_LEFT;
            case 126: return KeyEvent.KEYCODE_META_RIGHT;
            case 139: return KeyEvent.KEYCODE_MENU;
            case 158: return KeyEvent.KEYCODE_BACK;
            case 159: return KeyEvent.KEYCODE_FORWARD;
            default: return KeyEvent.KEYCODE_UNKNOWN;
        }
    }

    private interface RoutedRecord {}

    private static final class Gesture {
        final long downTimeMillis;
        final IBinder applicationToken;
        final TreeMap<Integer, Pointer> pointers = new TreeMap<>();

        Gesture(long downTimeMillis, IBinder applicationToken) {
            this.downTimeMillis = downTimeMillis;
            this.applicationToken = applicationToken;
        }

        int indexOf(int pointerId) {
            int index = 0;
            for (int current : pointers.keySet()) {
                if (current == pointerId) {
                    return index;
                }
                index++;
            }
            throw new IllegalStateException("pointer disappeared during event construction");
        }
    }

    private static final class Pointer {
        final float x;
        final float y;
        final float pressure;

        Pointer(float x, float y, float pressure) {
            this.x = x;
            this.y = y;
            this.pressure = pressure;
        }

        static Pointer from(TouchRecord record) {
            return new Pointer(record.x, record.y, record.pressure);
        }
    }

    private static final class TouchRecord implements RoutedRecord {
        final int displayId;
        final int taskId;
        final int action;
        final int pointerId;
        final long timestampNanos;
        final float x;
        final float y;
        final float pressure;

        TouchRecord(
                int displayId,
                int taskId,
                int action,
                int pointerId,
                long timestampNanos,
                float x,
                float y,
                float pressure) {
            this.displayId = displayId;
            this.taskId = taskId;
            this.action = action;
            this.pointerId = pointerId;
            this.timestampNanos = timestampNanos;
            this.x = x;
            this.y = y;
            this.pressure = pressure;
        }
    }

    private static final class KeyRecord implements RoutedRecord {
        final int displayId;
        final int taskId;
        final int action;
        final int scanCode;
        final long timestampNanos;
        final int repeat;
        final long routeSerial;

        KeyRecord(
                int displayId,
                int taskId,
                int action,
                int scanCode,
                long timestampNanos,
                int repeat,
                long routeSerial) {
            this.displayId = displayId;
            this.taskId = taskId;
            this.action = action;
            this.scanCode = scanCode;
            this.timestampNanos = timestampNanos;
            this.repeat = repeat;
            this.routeSerial = routeSerial;
        }
    }

    private static final class TaskBoundsRecord implements RoutedRecord {
        final int taskId;
        final int displayId;
        final int width;
        final int height;

        TaskBoundsRecord(
                int taskId,
                int displayId,
                int width,
                int height) {
            this.taskId = taskId;
            this.displayId = displayId;
            this.width = width;
            this.height = height;
        }
    }

    private static final class TaskFocusRecord implements RoutedRecord {
        final int taskId;
        final boolean focused;

        TaskFocusRecord(int taskId, boolean focused) {
            this.taskId = taskId;
            this.focused = focused;
        }
    }

    private static final class KeyIdentity {
        final int displayId;
        final int taskId;
        final int scanCode;

        KeyIdentity(int displayId, int taskId, int scanCode) {
            this.displayId = displayId;
            this.taskId = taskId;
            this.scanCode = scanCode;
        }

        @Override
        public boolean equals(Object other) {
            if (this == other) return true;
            if (!(other instanceof KeyIdentity)) return false;
            final KeyIdentity identity = (KeyIdentity) other;
            return displayId == identity.displayId
                    && taskId == identity.taskId
                    && scanCode == identity.scanCode;
        }

        @Override
        public int hashCode() {
            int result = displayId;
            result = 31 * result + taskId;
            return 31 * result + scanCode;
        }
    }

    private static final class KeyState {
        final long downTimeMillis;
        final IBinder applicationToken;

        KeyState(long downTimeMillis, IBinder applicationToken) {
            this.downTimeMillis = downTimeMillis;
            this.applicationToken = applicationToken;
        }
    }

    private static final class TaskCloseRecord implements RoutedRecord {
        final int taskId;

        TaskCloseRecord(int taskId) {
            this.taskId = taskId;
        }
    }
}
