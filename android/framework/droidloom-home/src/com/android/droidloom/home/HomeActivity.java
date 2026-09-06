/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.home;

import android.app.Activity;

/**
 * Idle HOME destination for the Android cell. The theme supplies the only pixels.
 *
 * Android owns this activity's lifecycle; there are no services, callbacks, views,
 * or wake locks to keep running. In particular, do not finish on creation: Android
 * must retain a home task when the last ordinary app closes. The native desktop
 * handles app selection, and this task is never registered for native export.
 */
public final class HomeActivity extends Activity {
}
