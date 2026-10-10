/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.content.ComponentName;
import android.graphics.Rect;
import android.os.ShellCommand;

final class DroidloomTaskShellCommand extends ShellCommand {
    private final DroidloomTaskBackend mBackend;
    DroidloomTaskShellCommand(DroidloomTaskBackend backend) { mBackend = backend; }
    @Override public int onCommand(String command) {
        if (!"droidloom-launch".equals(command)) return handleDefaultCommands(command);
        if (mBackend == null) { getErrPrintWriter().println("Droidloom tasks unavailable"); return 1; }
        try {
            int user = 0;
            ComponentName component = null;
            Rect bounds = null;
            String option;
            while ((option = getNextOption()) != null) {
                switch (option) {
                    case "--user": user = Integer.parseInt(getNextArgRequired()); break;
                    case "--component":
                        component = ComponentName.unflattenFromString(getNextArgRequired());
                        if (component == null) throw new IllegalArgumentException("Invalid component");
                        break;
                    case "--resolution":
                        String value = getNextArgRequired();
                        if (!value.matches("[0-9]{1,5}x[0-9]{1,5}")) {
                            throw new IllegalArgumentException("Expected WIDTHxHEIGHT");
                        }
                        String[] dimensions = value.split("x");
                        int width = Integer.parseInt(dimensions[0]), height = Integer.parseInt(dimensions[1]);
                        if (width < 1 || width > 16384 || height < 1 || height > 16384) {
                            throw new IllegalArgumentException("Invalid resolution");
                        }
                        bounds = new Rect(0, 0, width, height);
                        break;
                    default: throw new IllegalArgumentException("Unknown option " + option);
                }
            }
            String owner = getNextArgRequired();
            if (getNextArg() != null) throw new IllegalArgumentException("Unexpected argument");
            getOutPrintWriter().println(mBackend.launch(owner, component, user, bounds));
            return 0;
        } catch (Exception error) {
            getErrPrintWriter().println("Droidloom launch failed: " + error);
            return 1;
        }
    }
    @Override public void onHelp() {
        getOutPrintWriter().println("droidloom-launch [--user 0] [--component PACKAGE/ACTIVITY]"
                + " [--resolution WIDTHxHEIGHT] PACKAGE");
    }
}
