/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.mediaprojection.permission;

import android.app.Activity;
import android.os.Bundle;
import com.android.systemui.Unsupported;

/** Complete capture requests as cancelled until a native consent portal exists. */
public final class MediaProjectionPermissionActivity extends Activity {
    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        Unsupported.report("screen capture consent");
        setResult(RESULT_CANCELED);
        finish();
    }
}
