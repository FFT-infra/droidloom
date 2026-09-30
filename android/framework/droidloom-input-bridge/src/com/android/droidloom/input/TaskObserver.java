/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.input;

import android.app.ActivityManager.RunningTaskInfo;
import android.app.IActivityTaskManager;
import android.app.TaskStackListener;
import android.app.WindowConfiguration;
import android.os.Binder;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.os.Parcel;
import android.os.RemoteException;
import android.os.ServiceManager;
import android.util.Log;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;

/** Publishes Android's foreground app task without starting another activity. */
final class TaskObserver extends TaskStackListener {
    private static final String TAG = "DroidloomTasks";
    private static final int MAX_TASKS = 4096;
    private final Handler handler;
    private final TaskRegistration state = new TaskRegistration();
    private final Runnable reconcile = this::reconcile;
    private int failures;
    private IActivityTaskManager manager;

    private TaskObserver(Handler handler) { this.handler = handler; }

    static void start() {
        HandlerThread thread = new HandlerThread(TAG);
        thread.start();
        TaskObserver observer = new TaskObserver(new Handler(thread.getLooper()));
        observer.handler.post(observer::connect);
    }

    private void connect() {
        try {
            if (manager != null) return;
            IBinder binder = ServiceManager.getService("activity_task");
            if (binder == null) throw new IllegalStateException("activity_task absent");
            IActivityTaskManager service = IActivityTaskManager.Stub.asInterface(binder);
            service.registerTaskStackListener(this);
            binder.linkToDeath(() -> handler.post(() -> {
                manager = null;
                state.clear();
                failures = 0;
                connect();
            }), 0);
            manager = service;
            Log.i(TAG, "Observing Android foreground task launches; ineligible focus is"
                    + " reported by reason and never clears the registered binding");
            schedule();
        } catch (Exception error) {
            Log.w(TAG, "ActivityTaskManager not ready", error);
            // Retry only while disconnected; there is no idle task polling.
            handler.postDelayed(this::connect, 1000);
        }
    }

    @Override public boolean onTransact(int code, Parcel data, Parcel reply, int flags)
            throws RemoteException {
        int uid = Binder.getCallingUid();
        if (uid != 0 && uid != android.os.Process.SYSTEM_UID) {
            throw new SecurityException("Task callbacks require system UID");
        }
        return super.onTransact(code, data, reply, flags);
    }

    // Binder callbacks only enqueue work. Re-read authoritative TaskInfo after
    // the transition instead of trusting an early or already stale callback.
    @Override public void onTaskStackChanged() { schedule(); }
    @Override public void onTaskMovedToFront(RunningTaskInfo task) { schedule(); }
    @Override public void onTaskFocusChanged(int taskId, boolean focused) { schedule(); }
    @Override public void onActivityRestartAttempt(RunningTaskInfo task,
            boolean homeVisible, boolean cleared, boolean wasVisible) { schedule(); }
    @Override public void onTaskRemoved(int taskId) { schedule(); }

    private void schedule() {
        handler.removeCallbacks(reconcile);
        handler.postDelayed(reconcile, 50);
    }

    private void reconcile() {
        try {
            List<RunningTaskInfo> tasks = manager.getTasks(MAX_TASKS, false, false, 0);
            // The focused task first: a normal app switch keeps its binding.
            // Then every other eligible task on the shared display, because a
            // dialog that opens in its own task (the package installer behind
            // "download finished, tap to install") never takes focus from the
            // game it belongs to. Registering it is idempotent and a task that
            // Android destroyed retires through the layer path.
            List<RunningTaskInfo> candidates = new ArrayList<>();
            RunningTaskInfo focused = null;
            for (RunningTaskInfo info : tasks) {
                if (info.isFocused && focused == null) focused = info;
            }
            if (focused != null) candidates.add(focused);
            for (RunningTaskInfo info : tasks) {
                if (!info.isFocused) candidates.add(info);
            }
            for (RunningTaskInfo info : candidates) {
                TaskRegistration.Task task = new TaskRegistration.Task(info.taskId, info.userId,
                        info.displayId,
                        info.getActivityType() == WindowConfiguration.ACTIVITY_TYPE_STANDARD,
                        info.isVisible, info.numActivities > 0 && info.topActivity != null,
                        info.baseActivity == null ? null : info.baseActivity.getPackageName());
                if (!task.eligible()) {
                    if (info.isFocused) {
                        Log.i(TAG, "Task " + task.id + " (" + task.owner + ") is not eligible: "
                                + task.rejection());
                    } else {
                        // The journal records why a background window was left
                        // alone; only an eligible one is registered below.
                        Log.i(TAG, "Held task " + task.id + " (" + task.owner + "): "
                                + task.rejection());
                    }
                    continue;
                }
                if (!state.needsRegistration(task)) continue;
                final TaskRegistration.Task target = task;
                Process child = new ProcessBuilder("/vendor/bin/droidloom-task-launcher",
                        "--bind-task", Integer.toString(target.id),
                        "--user", Integer.toString(target.user), target.owner)
                        .redirectErrorStream(true)
                        .redirectOutput(ProcessBuilder.Redirect.INHERIT).start();
                if (!child.waitFor(10, TimeUnit.SECONDS)) {
                    child.destroyForcibly();
                    child.waitFor();
                    throw new IllegalStateException("Task registration timed out");
                }
                if (child.exitValue() != 0) {
                    throw new IllegalStateException("Task registration exited " + child.exitValue());
                }
                state.registered(target);
                Log.i(TAG, "Published task=" + target.id + " package=" + target.owner);
            }
            failures = 0;
        } catch (Exception error) {
            Log.w(TAG, "Could not publish task", error);
            // Bound retries to the current transition; another task event can
            // try again after a backend reconnect. Never launch the app again.
            if (++failures <= 5) handler.postDelayed(reconcile, 500);
        }
    }

    private void schedule() {
        handler.removeCallbacks(reconcile);
        handler.postDelayed(reconcile, 50);
    }

    private void reconcile() {
        try {
            TaskRegistration.Task foreground = null;
            if (manager == null) return;
            for (RunningTaskInfo info : manager.getTasks(MAX_TASKS, false, false, 0)) {
                if (!info.isFocused) continue;
                foreground = new TaskRegistration.Task(info.taskId, info.userId, info.displayId,
                        info.getActivityType() == WindowConfiguration.ACTIVITY_TYPE_STANDARD,
                        info.isVisible, info.numActivities > 0 && info.topActivity != null,
                        info.baseActivity == null ? null : info.baseActivity.getPackageName());
                break;
            }
            if (!state.needsRegistration(foreground)) {
                // Distinguish "the registered binding still holds" from "the
                // focused task is refused": both return false, and without
                // this line a refused window is invisible in the journal.
                if (foreground != null && !foreground.eligible()) {
                    Log.i(TAG, "Task " + foreground.id + " (" + foreground.owner
                            + ") is not eligible: " + foreground.rejection());
                } else if (state.lastRejection() != null) {
                    Log.i(TAG, "Waiting: " + state.lastRejection()
                            + "; the current host window keeps its binding");
                }
                failures = 0;
                return;
            }
            final TaskRegistration.Task target = foreground;
            Process child = new ProcessBuilder("/vendor/bin/droidloom-task-launcher",
                    "--bind-task", Integer.toString(target.id),
                    "--user", Integer.toString(target.user), target.owner)
                    .redirectErrorStream(true)
                    .redirectOutput(ProcessBuilder.Redirect.INHERIT).start();
            if (!child.waitFor(10, TimeUnit.SECONDS)) {
                child.destroyForcibly();
                child.waitFor();
                throw new IllegalStateException("Task registration timed out");
            }
            if (child.exitValue() != 0) {
                throw new IllegalStateException("Task registration exited " + child.exitValue());
            }
            state.registered(target);
            failures = 0;
            Log.i(TAG, "Published foreground task=" + target.id + " package=" + target.owner);
        } catch (Exception error) {
            Log.w(TAG, "Could not publish foreground task", error);
            // Bound retries to the current transition; another task event can
            // try again after a backend reconnect. Never launch the app again.
            if (++failures <= 5) handler.postDelayed(reconcile, 500);
        }
    }
}
