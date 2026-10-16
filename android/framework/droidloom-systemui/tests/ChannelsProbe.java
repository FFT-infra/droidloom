/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui;

import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import com.android.droidloom.runtime.HostChannel;
import java.io.File;
import java.io.IOException;

/** Nonvisual negative authorization/FD-lifetime checks; never reads host payloads. */
public final class ChannelsProbe {
    private static void denied(int channel) throws Exception {
        try (HostChannel unexpected = HostChannel.open(channel)) {
            throw new AssertionError("Unauthorized host descriptor for channel " + channel);
        } catch (IOException expected) { }
    }
    public static void main(String[] args) throws Exception {
        if (android.os.Process.myUid() != 2000)
            throw new AssertionError("Run as Android shell UID 2000");
        for (int channel : new int[] {1, 2, 3, 0, 255}) denied(channel);
        int before = new File("/proc/self/fd").list().length;
        for (int i = 0; i < 32; i++) denied(1 + i % 3);
        int after = new File("/proc/self/fd").list().length;
        if (after > before) throw new AssertionError("Denied delegation leaked descriptors");
        for (String name : new String[] {"text-input", "clipboard", "notifications"}) {
            try (LocalSocket direct = new LocalSocket()) {
                direct.connect(new LocalSocketAddress("/dev/socket/droidloom-framework-host/" + name,
                        LocalSocketAddress.Namespace.FILESYSTEM));
                throw new AssertionError("Unprivileged direct host path access: " + name);
            } catch (IOException expected) { }
        }
        System.out.println("PASS: unauthorized channels rejected, no descriptor leaks, private host paths inaccessible");
    }
}
