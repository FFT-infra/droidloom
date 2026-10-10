/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;
public final class FrameworkStartupTest {
    public static void main(String[] args) {
        for (boolean policyFirst : new boolean[] {true, false}) {
            DroidloomFrameworkStartup startup = new DroidloomFrameworkStartup();
            int[] calls = {0};
            if (policyFirst) startup.policyReady();
            startup.attach(() -> calls[0]++);
            if (calls[0] != (policyFirst ? 1 : 0)) throw new AssertionError("Premature/missing startup");
            startup.policyReady(); startup.policyReady();
            if (calls[0] != 1) throw new AssertionError("Startup must run exactly once");
        }
        DroidloomFrameworkStartup startup = new DroidloomFrameworkStartup();
        int[] attempts = {0}; startup.policyReady();
        try {
            startup.attach(() -> {if (attempts[0]++ == 0) throw new IllegalStateException("fixture failure");});
            throw new AssertionError("Failure hidden");
        } catch (IllegalStateException expected) { }
        startup.policyReady(); startup.policyReady();
        if (attempts[0] != 2) throw new AssertionError("Failed start was marked ready");
        System.out.println("PASS: both framework callback orders, exactly-once startup and failed-start retry");
    }
}
