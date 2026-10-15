/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server;

public final class GraphicsCapabilitiesTest {
    public static void main(String[] args) {
        for (int level : new int[] {0, 1}) {
            int[] detected = DroidloomGraphicsCapabilities.detectVulkan(() -> new int[] {0x404000, level});
            if (detected == null || detected[0] != 0x404000 || detected[1] != level) {
                throw new AssertionError("Valid hardware capabilities lost");
            }
        }
        for (int[] invalid : new int[][] {null, {}, {0x404000}, {0, 0}, {0x404000, -1},
                {0x404000, 2}, {0x20404000, 1}}) {
            if (DroidloomGraphicsCapabilities.detectVulkan(() -> invalid) != null) {
                throw new AssertionError("Invalid capabilities published");
            }
        }
        if (DroidloomGraphicsCapabilities.detectVulkan(() -> { throw new UnsatisfiedLinkError(); }) != null
                || DroidloomGraphicsCapabilities.detectVulkan(() -> { throw new IllegalStateException(); }) != null) {
            throw new AssertionError("Probe failure published features");
        }
        System.out.println("PASS: Java Vulkan feature boundary and failed probe handling");
    }
}
