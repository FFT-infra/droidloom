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
        boolean eligible() {
            return id > 0 && user == 0 && display == 0 && standard && visible
                    && hasActivity && owner != null && !owner.isEmpty();
        }
        boolean same(Task other) {
            return other != null && id == other.id && user == other.user
                    && display == other.display && owner.equals(other.owner);
        }
    }
    private Task last;
    void clear() { last = null; }
    boolean needsRegistration(Task task) {
        if (task == null || !task.eligible()) { last = null; return false; }
        return !task.same(last);
    }
    void registered(Task task) { last = task; }
}
