/* SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.policy;

import static com.android.server.policy.DroidloomPolicyNative.*;

import java.util.concurrent.atomic.AtomicInteger;

/** Runs the real Rust JNI library under a host JVM, without Android or a display. */
public final class NativePolicyTest {
    private static void require(boolean value) {
        if (!value) throw new AssertionError("Rust/Java policy ABI or behavior mismatch");
    }
    public static void main(String[] args) throws InterruptedException {
        require(!query(READY, 0, false));
        AtomicInteger bootCallbacks = new AtomicInteger();
        Thread ready = new Thread(() -> {
            if (command(SYSTEM_READY, 0, 0, false) == NOTIFY_BOOT_COMPLETED) {
                bootCallbacks.incrementAndGet();
            }
        });
        Thread booted = new Thread(() -> {
            if (command(SYSTEM_BOOTED, 0, 0, false) == NOTIFY_BOOT_COMPLETED) {
                bootCallbacks.incrementAndGet();
            }
        });
        ready.start();
        booted.start();
        ready.join();
        booted.join();
        require(bootCallbacks.get() == 1);
        require(query(READY, 0, false));
        require(query(BOOTED, 0, false));
        require(command(SYSTEM_READY, 0, 0, false) == 0);
        require(command(SYSTEM_BOOTED, 0, 0, false) == 0);
        require(!query(BOOT_ANIMATION_DISMISSIBLE, 0, false));
        command(BOOT_WITH_OUTPUT_OFF, 0, 0, false);
        require(query(BOOT_ANIMATION_DISMISSIBLE, 0, false));
        int generation = command(SCREEN_START, 0, 0, false);
        require(generation > 0);
        require(!query(BOOT_ANIMATION_DISMISSIBLE, 0, false));
        require(command(WINDOWS_DRAWN, 0, generation, false) == 0);
        require(command(KEYGUARD_DRAWN, 0, generation, false) == WAIT_FOR_WINDOWS);
        require(command(WINDOWS_DRAWN, 0, generation, false) == COMPLETE_SCREEN);
        require(command(WINDOWS_DRAWN, 0, generation, false) == 0);
        require(query(KEYGUARD_DRAWN_ONCE, 0, false));
        command(SCREEN_OFF, 0, 0, false);
        require(!query(SCREEN_ON, 0, false));
        int next = command(SCREEN_START, 0, 0, false);
        require(next != generation);
        require(command(KEYGUARD_DRAWN, 0, generation, false) == 0);
        command(GOING_TO_SLEEP, 0, 0, true);
        require(!query(CAN_ANIMATE, 0, true));
        command(GOING_TO_SLEEP, 0, 0, false);
        require(query(CAN_ANIMATE, 0, false));
        command(ALLOW_LOCKSCREEN_WHEN_ON, 0, 0, true);
        require(query(LOCKSCREEN_TIMER_REQUIRED, 0, true));
        require(!query(LOCKSCREEN_TIMER_REQUIRED, 0, false));
        command(CLEAR_LOCKSCREEN_DISPLAYS, 0, 0, false);
        require(!query(LOCKSCREEN_TIMER_REQUIRED, 0, true));
        require(routeKey(4, true, false, false) == PASS);
        require(routeKey(26, true, true, false) == CONSUME);
        require(routeKey(85, false, false, false) == MEDIA);
        require(routeKey(24, true, false, true) == VOLUME);
        require(authorizeWindow(INTERNAL, 0, -1, -1) == DENY);
        require(authorizeWindow(APPLICATION_OVERLAY, 0, -1, -1) == NEED_PACKAGE);
        int facts = PACKAGE_CHECKED | PACKAGE_EXISTS;
        require(authorizeWindow(APPLICATION_OVERLAY, facts, 37, -1) == NEED_APP_OP);
        require(authorizeWindow(APPLICATION_OVERLAY, facts, 37, 0) == ALLOW);
        require(authorizeWindow(APPLICATION_OVERLAY, facts, 37, 1) == ALLOW);
        require(authorizeWindow(APPLICATION_OVERLAY, facts, 37, 2) == DENY);
        require(authorizeWindow(ALERT, facts, 37, 0) == DENY);
        require(authorizeWindow(INVALID, 0, 0, 0) == INVALID_TYPE);
        System.out.println("Rust WindowManager policy JNI probe passed");
    }
}
