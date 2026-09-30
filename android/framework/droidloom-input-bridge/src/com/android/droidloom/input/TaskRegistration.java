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
        boolean same(Task other) {
            return other != null && id == other.id && user == other.user
                    && display == other.display && owner.equals(other.owner);
        }
    }
    private Task last;
    private String rejection;
    void clear() { last = null; rejection = null; }
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
        return !task.same(last);
    }
    void registered(Task task) { last = task; }
    /**
     * Why the most recently seen ineligible task was refused, or null when
     * none was seen. Retained for the journal; it is not proof that the
     * observed task differing from {@link #last} is the refused one.
     */
    String lastRejection() { return rejection; }
}
