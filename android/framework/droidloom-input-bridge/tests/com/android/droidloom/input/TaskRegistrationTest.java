/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.input;

public final class TaskRegistrationTest {
    private static TaskRegistration.Task app(int id, String owner) {
        return new TaskRegistration.Task(id, 0, 0, true, true, true, owner);
    }
    private static void check(boolean value) {
        if (!value) throw new AssertionError();
    }
    public static void main(String[] args) {
        TaskRegistration state = new TaskRegistration();
        TaskRegistration.Task store = app(25, "com.android.vending");
        TaskRegistration.Task tiktok = app(26, "com.zhiliaoapp.musically");
        check(state.needsRegistration(store));
        state.registered(store);
        check(!state.needsRegistration(store));
        check(state.needsRegistration(tiktok)); // Play Store Open creates a new host window.
        check(state.needsRegistration(tiktok)); // Failed backend operation remains retryable.
        state.registered(tiktok);
        check(!state.needsRegistration(app(26, tiktok.owner))); // Stack/layout noise is inert.
        check(state.needsRegistration(store));
        state.registered(store);
        check(state.needsRegistration(tiktok)); // Open an already running app again.
        state.registered(tiktok);
        check(state.needsRegistration(app(26, "org.example.changed"))); // ID reuse/owner change.
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
        check(state.needsRegistration(tiktok)); // Returning from HOME must activate again.
        state.registered(tiktok);
        state.clear();
        check(state.needsRegistration(tiktok)); // System-server reconnection rebuilds bindings.
        // Foreign activities inside an existing app task retain its base owner;
        // deep links and chooser-only apps need no launcher entry or package allowlist.
        check(state.needsRegistration(app(30, "org.example.deep_link_only")));
        System.out.println("TaskRegistrationTest: passed");
    }
}
