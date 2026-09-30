/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.input;

/** Binder-independent foreground transition policy. */
final class TaskRegistration {
    static final class Task {
        final int id, user, display;
        final boolean standard, visible, hasActivity;
        final String owner;
        Task(int id, int user, int display, boolean standard, boolean visible,
                boolean hasActivity, String owner) {
            this.id = id; this.user = user; this.display = display;
            this.standard = standard; this.visible = visible;
            this.hasActivity = hasActivity; this.owner = owner;
        }
        // Last reason eligibility() refused this task, or null when eligible.
        // Set on every call so the value matches the current fields.
        private String rejection;
        boolean eligible() {
            String reason;
            if (id <= 0) {
                reason = "invalid task ID " + id;
            } else if (user != 0) {
                reason = "user " + user + " is not the primary user";
            } else if (display != 0) {
                reason = "display " + display + " is not the default display";
            } else if (!standard) {
                reason = "activity type is not standard";
            } else if (!visible) {
                reason = "task is not visible";
            } else if (!hasActivity) {
                reason = "task has no activity";
            } else if (owner == null || owner.isEmpty()) {
                reason = "task has no owning package";
            } else {
                reason = null;
            }
            rejection = reason;
            return reason == null;
        }
        /** Why {@link #eligible()} refused this task, or null when it did not. */
        String rejection() { return rejection; }
    }
    /** Identity of a registered binding; a task that changes shape is new. */
    private static String key(Task task) {
        return task.id + ":" + task.user + ":" + task.display + ":" + task.owner;
    }
    /**
     * Bindings the host currently holds, by identity.
     *
     * Several tasks are on screen at once — a game and the package installer
     * behind it — so this is a set, not the single "last" task it used to be.
     * A single slot made every reconcile re-bind every other task, and the
     * launcher refuses a task it has already bound, which took the whole pass
     * down with it and left the installer's task unregistered.
     */
    private final java.util.Set<String> bound = new java.util.HashSet<>();
    private String rejection;
    void clear() { bound.clear(); rejection = null; }
    boolean needsRegistration(Task task) {
        // An ineligible or absent task cannot be registered, but it must not
        // erase the binding of the task that is registered: a task briefly
        // hidden behind another window is still the task the host holds.
        if (task == null || !task.eligible()) {
            if (task != null) rejection = task.rejection();
            return false;
        }
        rejection = null;
        task.rejection = null;
        return !bound.contains(key(task));
    }
    void registered(Task task) {
        task.rejection = null;
        bound.add(key(task));
    }
    /**
     * Drop bindings for tasks Android no longer lists, so a task that comes
     * back is registered again instead of being assumed still bound.
     */
    void retain(java.util.Collection<Task> present) {
        java.util.Set<String> live = new java.util.HashSet<>();
        for (Task task : present) live.add(key(task));
        bound.retainAll(live);
    }
    /**
     * Why the most recently seen ineligible task was refused, or null when
     * none was seen. Retained for the journal; it is not proof that the
     * observed task differing from {@link #last} is the refused one.
     */
    String lastRejection() { return rejection; }
}
