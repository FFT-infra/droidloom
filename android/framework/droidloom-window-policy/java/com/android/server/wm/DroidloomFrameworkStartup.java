/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

/** WMS policy and ATMS become ready separately; accept either callback order. */
final class DroidloomFrameworkStartup {
    private Runnable action;
    private boolean policyReady, started;
    synchronized void attach(Runnable action) {
        if (this.action != null) throw new IllegalStateException("Framework backend already attached");
        this.action = action;
        startIfReady();
    }
    synchronized void policyReady() {
        policyReady = true;
        startIfReady();
    }
    private void startIfReady() {
        if (policyReady && action != null && !started) {
            action.run();
            started = true;
        }
    }
}
