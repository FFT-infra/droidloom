/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.input;

public final class TaskRegistrationTest {
    private static TaskRegistration.Task app(int id, String owner) {
        return new TaskRegistration.Task(id, 0, 0, true, true, true, owner);
    }
    private static void check(boolean value) {
        if (!value) throw new AssertionError();
    }
    private static void checkReason(String value) {
        if (value == null || value.isEmpty()) throw new AssertionError("missing reason");
    }
    public static void main(String[] args) {
        TaskRegistration state = new TaskRegistration();
        TaskRegistration.Task store = app(25, "com.android.vending");
        TaskRegistration.Task tiktok = app(26, "com.zhiliaoapp.musically");
        check(state.needsRegistration(store));
        state.registered(store);
        check(!state.needsRegistration(store));
        // Several tasks are on screen at once — a game and the package
        // installer behind it — so a second task must not displace the first,
        // and a task already bound must never be bound a second time.
        check(state.needsRegistration(tiktok)); // Play Store Open creates a new host window.
        check(state.needsRegistration(tiktok)); // Failed backend operation remains retryable.
        state.registered(tiktok);
        check(!state.needsRegistration(app(26, tiktok.owner))); // Stack/layout noise is inert.
        check(!state.needsRegistration(store)); // The first binding still holds.
        // A task Android stops listing is bound again when it comes back.
        state.retain(java.util.List.of(tiktok));
        check(state.needsRegistration(store));
        state.registered(store);
        check(!state.needsRegistration(store));
        // A focused task that is momentarily ineligible — a window briefly
        // hidden behind another — reports why but leaves the binding intact;
        // the retry budget is not spent and no window is torn down.
        TaskRegistration.Task hidden =
                new TaskRegistration.Task(26, 0, 0, true, false, true, tiktok.owner);
        check(!state.needsRegistration(hidden));
        checkReason(hidden.rejection());
        checkReason(state.lastRejection());
        for (TaskRegistration.Task rejected : new TaskRegistration.Task[] {
                null,
                new TaskRegistration.Task(1, 0, 0, false, true, true, "com.android.droidloom.home"),
                new TaskRegistration.Task(1, 0, 0, true, false, true, "org.example.hidden"),
                new TaskRegistration.Task(1, 10, 0, true, true, true, "org.example.profile"),
                new TaskRegistration.Task(1, 0, 2, true, true, true, "org.example.display"),
                new TaskRegistration.Task(1, 0, 0, true, true, false, "org.example.organizer"),
                new TaskRegistration.Task(1, 0, 0, true, true, true, null),
                app(0, "org.example.invalid"), app(-1, "org.example.invalid") }) {
            check(!state.needsRegistration(rejected));
        }
        check(!state.needsRegistration(tiktok)); // Still hidden: the binding survived.
        // The owning package is a label, not identity. Android recomputes a
        // task's base activity as its bottom-most non-finishing activity, so
        // when that one finishes the package changes while the task does not:
        // same id, same binding, no re-registration.
        check(!state.needsRegistration(app(26, "org.example.changed")));
        check(!state.needsRegistration(tiktok)); // Still the same task either way.
        // Only when Android stops listing the task does the binding go.
        state.retain(java.util.List.of(tiktok));
        check(state.needsRegistration(app(27, "org.example.other"))); // A different task is new.
        state.clear();
        check(state.needsRegistration(tiktok)); // System-server reconnection rebuilds bindings.
        // Foreign activities inside an existing app task retain its base owner;
        // deep links and chooser-only apps need no launcher entry or package allowlist.
        check(state.needsRegistration(app(30, "org.example.deep_link_only")));
        System.out.println("TaskRegistrationTest: passed");
    }
}
