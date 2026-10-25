/*
 * Portions derived from PhoneWindowManager:
 * Copyright (C) 2006 The Android Open Source Project
 * SPDX-License-Identifier: Apache-2.0 AND GPL-3.0-or-later
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use the upstream portions except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * Original Droidloom additions are licensed under GPL-3.0-or-later; see LICENSE.
 */
package com.android.server.policy;

import static android.Manifest.permission.CREATE_VIRTUAL_DEVICE;
import static android.Manifest.permission.INTERNAL_SYSTEM_WINDOW;
import static android.Manifest.permission.SYSTEM_ALERT_WINDOW;
import static android.Manifest.permission.SYSTEM_APPLICATION_OVERLAY;
import static android.content.pm.PackageManager.PERMISSION_GRANTED;
import static android.view.Display.DEFAULT_DISPLAY;
import static android.view.Display.DEFAULT_DISPLAY_GROUP;
import static android.view.Display.INVALID_DISPLAY;
import static android.view.WindowManager.LayoutParams.*;
import static android.view.WindowManagerPolicyConstants.*;
import static android.view.contentprotection.flags.Flags.createAccessibilityOverlayAppOpEnabled;
import static com.android.internal.policy.IKeyguardService.SCREEN_TURNING_ON_REASON_DISPLAY_SWITCH;
import static com.android.internal.policy.IKeyguardService.SCREEN_TURNING_ON_REASON_UNKNOWN;
import static com.android.server.policy.DroidloomPolicyNative.*;
import static com.android.server.policy.WindowManagerPolicy.WindowManagerFuncs.LID_CLOSED;
import static com.android.server.policy.WindowManagerPolicy.WindowManagerFuncs.LID_OPEN;

import android.app.AppOpsManager;
import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.media.session.MediaSessionLegacyHelper;
import android.os.Binder;
import android.os.Bundle;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.Message;
import android.os.PowerManager;
import android.os.Process;
import android.os.RemoteException;
import android.os.UserHandle;
import android.os.UserManager;
import android.provider.Settings;
import android.util.Slog;
import android.util.SparseArray;
import android.util.proto.ProtoOutputStream;
import android.view.Display;
import android.view.KeyEvent;
import android.view.KeyboardShortcutGroup;
import android.view.WindowManager;
import android.view.WindowManagerGlobal;

import com.android.internal.policy.IKeyguardDismissCallback;
import com.android.internal.policy.IShortcutService;
import com.android.server.LocalServices;
import com.android.server.input.InputManagerInternal;
import com.android.server.policy.keyguard.KeyguardServiceDelegate;
import com.android.server.pm.UserManagerInternal;
import com.android.server.statusbar.StatusBarManagerInternal;
import com.android.server.wm.DisplayPolicy;
import com.android.server.wm.DisplayRotation;
import com.android.server.wm.WindowManagerInternal;

import java.io.PrintWriter;
import java.util.Collections;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;

/**
 * Android framework adapter for Droidloom's in-process Rust window policy.
 *
 * This implements WindowManagerPolicy directly. It does not construct or delegate
 * to PhoneWindowManager. Android owns permissions, credentials and framework
 * objects; Rust decides authorization, key routing and lifecycle readiness. No
 * host transport or process invocation is allowed on WindowManager's threads.
 */
public final class DroidloomWindowManagerPolicy implements WindowManagerPolicy {
    private static final String TAG = "DroidloomWindowPolicy";
    private static final int MSG_WINDOWS_DRAWN = 1;
    private static final long WINDOWS_DRAWN_TIMEOUT_MS = 1000;
    private static final long KEYGUARD_DRAWN_TIMEOUT_MS = 1000;
    private static final long KEYGUARD_BOOT_TIMEOUT_MS = 5000;

    // DisplayPowerController screen events, our Handler, and keyguard Binder
    // draw callbacks acquire this monitor. WMS callbacks never do: DisplayPolicy
    // takes the WMS global lock,
    // and screenTurningOn deliberately calls AM outside that global lock.
    // JNI never calls back into Java while holding the native state mutex.
    private final Object mScreenLock = new Object();
    private final SparseArray<PendingScreen> mPendingScreens = new SparseArray<>();
    private Context mContext;
    private WindowManagerFuncs mFuncs;
    private WindowManagerInternal mWindowManagerInternal;
    private InputManagerInternal mInputManagerInternal;
    private Handler mHandler;
    private KeyguardServiceDelegate mKeyguard;
    private DisplayPolicy mDisplayPolicy;
    private Display mDefaultDisplay;
    private DisplayRotation mDisplayRotation;
    private final AtomicBoolean mKeyguardBound = new AtomicBoolean();
    private volatile boolean mPendingOccluded;
    private volatile boolean mDismissImeOnBack;
    private volatile boolean mGoingToSleep;
    private volatile int mCurrentUser = UserHandle.USER_SYSTEM;
    private volatile int mTopFocusedDisplay = DEFAULT_DISPLAY;
    private volatile boolean mSafeMode;
    // Accessed only on the policy Handler.
    private boolean mLockTimerActive;
    private final Runnable mLockTimeout = () -> {
        mLockTimerActive = false;
        mKeyguard.doKeyguardTimeout(null);
    };

    private static final class PendingScreen {
        final int display;
        final int generation;
        final ScreenOnListener listener;
        Runnable timeout;

        PendingScreen(int display, int generation, ScreenOnListener listener) {
            this.display = display;
            this.generation = generation;
            this.listener = listener;
        }
    }

    @Override
    public void init(Context context, WindowManagerFuncs funcs) {
        android.os.SystemProperties.set("sys.droidloom.framework_ready", "0");
        mContext = context;
        mFuncs = funcs;
        mWindowManagerInternal = LocalServices.getService(WindowManagerInternal.class);
        mInputManagerInternal = LocalServices.getService(InputManagerInternal.class);
        mHandler = new Handler(Looper.myLooper(), null, true) {
            @Override public void handleMessage(Message message) {
                if (message.what == MSG_WINDOWS_DRAWN) {
                    finishWindowsDrawn((PendingScreen) message.obj);
                }
            }
        };
        mKeyguard = new KeyguardServiceDelegate(context, new KeyguardServiceDelegate.StateCallback() {
            @Override public void onTrustedChanged() { mFuncs.notifyKeyguardTrustedChanged(); }
            @Override public void onShowingChanged() {
                mFuncs.onKeyguardShowingAndNotOccludedChanged();
            }
            @Override public void onKeyguardServiceConnected() {
                mFuncs.resetFirstKeyguardLockedStateDispatched();
            }
            @Override public void onDrawn() {
                // Binder callbacks never acquire DisplayPolicy's lock directly.
                // Capture the generation at receipt, before the Handler can be
                // delayed behind another screen-off/on transition.
                final PendingScreen pending;
                synchronized (mScreenLock) { pending = mPendingScreens.get(DEFAULT_DISPLAY); }
                if (pending != null) mHandler.post(() -> finishKeyguardDrawn(pending));
            }
        });
        // Resolve the JNI library and check the paired ABI before WMS starts.
        query(READY, DEFAULT_DISPLAY, false);
        Slog.i(TAG, "Using in-process Rust policy ABI 1");
    }

    @Override
    public void setDefaultDisplay(DisplayContentInfo info) {
        mDefaultDisplay = info.getDisplay();
        mDisplayRotation = info.getDisplayRotation();
        mDisplayPolicy = mDisplayRotation.getDisplayPolicy();
    }

    /** Classify framework constants; authorization itself belongs to Rust. */
    private static int windowKind(int type) {
        if ((type >= FIRST_APPLICATION_WINDOW && type <= LAST_APPLICATION_WINDOW)
                || (type >= FIRST_SUB_WINDOW && type <= LAST_SUB_WINDOW)) return APPLICATION;
        if (type < FIRST_SYSTEM_WINDOW || type > LAST_SYSTEM_WINDOW) return INVALID;
        if (isSystemAlertWindowType(type)) {
            return type == TYPE_APPLICATION_OVERLAY ? APPLICATION_OVERLAY : ALERT;
        }
        switch (type) {
            case TYPE_TOAST: return TOAST;
            case TYPE_ACCESSIBILITY_OVERLAY:
                if (createAccessibilityOverlayAppOpEnabled()) {
                    return TOKEN_CHECKED;
                }
                // With the flag disabled, WMS verifies the accessibility token as upstream does.
            case TYPE_INPUT_METHOD:
            case TYPE_WALLPAPER:
            case TYPE_PRESENTATION:
            case TYPE_PRIVATE_PRESENTATION:
            case TYPE_VOICE_INTERACTION:
            case TYPE_QS_DIALOG:
            case TYPE_NAVIGATION_BAR_PANEL:
            case TYPE_STATUS_BAR:
            case TYPE_NOTIFICATION_SHADE:
            case TYPE_NAVIGATION_BAR:
            case TYPE_STATUS_BAR_ADDITIONAL:
            case TYPE_STATUS_BAR_SUB_PANEL:
            case TYPE_VOICE_INTERACTION_STARTING: return TOKEN_CHECKED;
            default: return INTERNAL;
        }
    }

    private boolean permitted(String permission) {
        return mContext.checkCallingOrSelfPermission(permission) == PERMISSION_GRANTED;
    }

    @Override
    public int checkAddPermission(int type, boolean rounded, String packageName,
            int[] outAppOp, int displayId) {
        final int kind = windowKind(type);
        final int uid = Binder.getCallingUid();
        final boolean needsInternalPermission = rounded || kind == INTERNAL
                || kind == ALERT || kind == APPLICATION_OVERLAY;
        int facts = (rounded ? ROUNDED_OVERLAY : 0)
                | (needsInternalPermission && permitted(INTERNAL_SYSTEM_WINDOW) ? INTERNAL_WINDOW : 0)
                | (UserHandle.getAppId(uid) == Process.SYSTEM_UID ? SYSTEM_UID : 0);
        outAppOp[0] = AppOpsManager.OP_NONE;
        if (kind == TOAST) outAppOp[0] = AppOpsManager.OP_TOAST_WINDOW;
        if (type == TYPE_ACCESSIBILITY_OVERLAY
                && createAccessibilityOverlayAppOpEnabled()) {
            outAppOp[0] = AppOpsManager.OP_CREATE_ACCESSIBILITY_OVERLAY;
        }
        if (kind == ALERT || kind == APPLICATION_OVERLAY) {
            outAppOp[0] = AppOpsManager.OP_SYSTEM_ALERT_WINDOW;
        }
        int targetSdk = -1;
        int result = authorizeWindow(kind, facts, targetSdk, -1);
        if (result == NEED_PACKAGE) {
            ApplicationInfo app = null;
            try {
                app = mContext.getPackageManager().getApplicationInfoAsUser(
                        packageName, 0, UserHandle.getUserId(uid));
            } catch (PackageManager.NameNotFoundException e) {
                // Rust treats a missing package as requiring INTERNAL_SYSTEM_WINDOW.
            }
            facts |= PACKAGE_CHECKED;
            if (app != null) {
                facts |= PACKAGE_EXISTS;
                targetSdk = app.targetSdkVersion;
            }
            if (permitted(SYSTEM_APPLICATION_OVERLAY)) facts |= SYSTEM_OVERLAY;
            if (mFuncs.isCallerVirtualDeviceOwner(displayId, uid)
                    && mFuncs.isDisplayTrusted(displayId) && permitted(CREATE_VIRTUAL_DEVICE)) {
                facts |= TRUSTED_VIRTUAL_OWNER;
            }
            result = authorizeWindow(kind, facts, targetSdk, -1);
        }
        if (result == NEED_APP_OP) {
            // Keep the Binder caller identity: noteOp logs usage and validates the package.
            final int mode = mContext.getSystemService(AppOpsManager.class).noteOpNoThrow(
                    outAppOp[0], uid, packageName, null, "check-add");
            if (permitted(SYSTEM_ALERT_WINDOW)) facts |= ALERT_WINDOW;
            result = authorizeWindow(kind, facts, targetSdk, mode);
        }
        if (result == ALLOW) return WindowManagerGlobal.ADD_OKAY;
        if (result == INVALID_TYPE) return WindowManagerGlobal.ADD_INVALID_TYPE;
        return WindowManagerGlobal.ADD_PERMISSION_DENIED;
    }

    @Override
    public void adjustConfigurationLw(Configuration config, int keyboards, int navigation) {
        // Host input devices are authoritative; no Android lid or phone keyboard policy.
        if (config.keyboard == Configuration.KEYBOARD_NOKEYS) {
            config.hardKeyboardHidden = Configuration.HARDKEYBOARDHIDDEN_YES;
        }
        if (config.navigation == Configuration.NAVIGATION_NONAV) {
            config.navigationHidden = Configuration.NAVIGATIONHIDDEN_YES;
        }
    }

    @Override public boolean isKeyguardHostWindow(WindowManager.LayoutParams attrs) {
        return attrs.type == TYPE_NOTIFICATION_SHADE;
    }

    private void dispatchSystemKey(KeyEvent event, int route) {
        final KeyEvent copy = new KeyEvent(event);
        mHandler.post(() -> {
            MediaSessionLegacyHelper helper = MediaSessionLegacyHelper.getHelper(mContext);
            if (route == MEDIA) helper.sendMediaButtonEvent(copy, true);
            if (route == VOLUME) helper.sendVolumeKeyEvent(copy,
                    android.media.AudioManager.USE_DEFAULT_STREAM_TYPE, false);
        });
    }

    @Override public int interceptKeyBeforeQueueing(KeyEvent event, int policyFlags) {
        int route = routeKey(event.getKeyCode(), (policyFlags & FLAG_INTERACTIVE) != 0,
                (policyFlags & FLAG_INJECTED) != 0, false);
        if (route == MEDIA || route == VOLUME) dispatchSystemKey(event, route);
        return route == PASS ? ACTION_PASS_TO_USER : 0;
    }
    @Override public boolean interceptKeyBeforeDispatching(IBinder focusedToken, KeyEvent event) {
        return routeKey(event.getKeyCode(), true, true, false) != PASS;
    }
    @Override public boolean interceptUnhandledKey(KeyEvent event, IBinder focusedToken) {
        int code = event.getKeyCode();
        if ((code >= KeyEvent.KEYCODE_BUTTON_7 && code <= KeyEvent.KEYCODE_BUTTON_16)
                || code == KeyEvent.KEYCODE_STYLUS_BUTTON_PRIMARY
                || code == KeyEvent.KEYCODE_STYLUS_BUTTON_SECONDARY
                || code == KeyEvent.KEYCODE_STYLUS_BUTTON_TERTIARY
                || code == KeyEvent.KEYCODE_STYLUS_BUTTON_TAIL) {
            return true;
        }
        int route = routeKey(code, true, false, true);
        if (route == MEDIA || route == VOLUME) {
            dispatchSystemKey(event, route);
            return true;
        }
        return false;
    }
    @Override public int interceptMotionBeforeQueueingNonInteractive(int displayId, int source,
            int action, long whenNanos, int policyFlags) {
        // The host sends output wake state separately; input cannot toggle phone power.
        return 0;
    }
    @Override public KeyboardShortcutGroup getApplicationLaunchKeyboardShortcuts(int deviceId) {
        return new KeyboardShortcutGroup("Droidloom", Collections.emptyList());
    }
    @Override public void registerShortcutKey(long shortcut, IShortcutService receiver)
            throws RemoteException {
        mInputManagerInternal.registerShortcutKey(shortcut, receiver);
    }
    @Override public void setTopFocusedDisplay(int displayId) { mTopFocusedDisplay = displayId; }
    @Override public boolean isGlobalKey(int keyCode) { return false; }
    @Override public void addSingleKeyRule(SingleKeyGestureDetector.SingleKeyRule rule) {
        // Physical power and shell gestures are handled by the host compositor.
    }

    @Override public void screenTurningOn(int displayId, ScreenOnListener listener) {
        final PendingScreen pending;
        synchronized (mScreenLock) {
            int generation = command(SCREEN_START, displayId, 0, false);
            if (generation <= 0) throw new IllegalStateException("Invalid screen generation");
            PendingScreen previous = mPendingScreens.get(displayId);
            if (previous != null && previous.timeout != null) {
                mHandler.removeCallbacks(previous.timeout);
            }
            pending = new PendingScreen(displayId, generation, listener);
            if (displayId == DEFAULT_DISPLAY) {
                pending.timeout = () -> finishKeyguardDrawn(pending);
            }
            mPendingScreens.put(displayId, pending);
            if (displayId == DEFAULT_DISPLAY) mDisplayPolicy.screenTurningOn(listener);
        }
        if (displayId != DEFAULT_DISPLAY) {
            waitForWindows(pending);
        } else {
            mHandler.postDelayed(pending.timeout, query(BOOTED, 0, false)
                    ? KEYGUARD_DRAWN_TIMEOUT_MS : KEYGUARD_BOOT_TIMEOUT_MS);
            if (mKeyguard.hasKeyguard()) {
                mKeyguard.onScreenTurningOn(mDisplayPolicy.isDisplaySwitching()
                        ? SCREEN_TURNING_ON_REASON_DISPLAY_SWITCH
                        : SCREEN_TURNING_ON_REASON_UNKNOWN);
            } else {
                mHandler.post(pending.timeout);
            }
        }
    }

    private void finishKeyguardDrawn(PendingScreen pending) {
        synchronized (mScreenLock) {
            if (mPendingScreens.get(pending.display) != pending) return;
            if (command(KEYGUARD_DRAWN, pending.display, pending.generation, false)
                    != WAIT_FOR_WINDOWS) return;
            mHandler.removeCallbacks(pending.timeout);
            mDisplayPolicy.finishKeyguardDrawn();
        }
        waitForWindows(pending);
    }

    private void waitForWindows(PendingScreen pending) {
        mWindowManagerInternal.waitForAllWindowsDrawn(
                mHandler.obtainMessage(MSG_WINDOWS_DRAWN, pending), WINDOWS_DRAWN_TIMEOUT_MS,
                pending.display == DEFAULT_DISPLAY ? INVALID_DISPLAY : pending.display);
    }

    private void finishWindowsDrawn(PendingScreen pending) {
        synchronized (mScreenLock) {
            if (mPendingScreens.get(pending.display) != pending) return;
            if (pending.display == DEFAULT_DISPLAY) {
                if (!mDisplayPolicy.finishWindowsDrawn()
                        || !mDisplayPolicy.finishScreenTurningOn()) return;
            }
            if (command(WINDOWS_DRAWN, pending.display, pending.generation, false)
                    != COMPLETE_SCREEN) return;
            mPendingScreens.remove(pending.display);
        }
        if (pending.display == DEFAULT_DISPLAY) mDisplayRotation.updateOrientationListener();
        if (pending.listener != null) pending.listener.onScreenOn();
        mFuncs.enableScreenIfNeeded();
    }

    @Override public void screenTurnedOn(int displayId) {
        if (displayId == DEFAULT_DISPLAY) {
            mKeyguard.onScreenTurnedOn();
            mDisplayPolicy.screenTurnedOn();
        }
    }
    @Override public void screenTurningOff(int displayId, ScreenOffListener listener) {
        mFuncs.screenTurningOff(displayId, listener);
        if (displayId == DEFAULT_DISPLAY) mKeyguard.onScreenTurningOff();
    }
    @Override public void screenTurnedOff(int displayId, boolean swappingDisplay) {
        synchronized (mScreenLock) {
            command(SCREEN_OFF, displayId, 0, false);
            PendingScreen pending = mPendingScreens.get(displayId);
            if (pending != null && pending.timeout != null) mHandler.removeCallbacks(pending.timeout);
            mPendingScreens.remove(displayId);
            if (displayId == DEFAULT_DISPLAY) {
                mDisplayPolicy.screenTurnedOff(!swappingDisplay || mGoingToSleep);
            }
        }
        if (displayId == DEFAULT_DISPLAY) {
            mKeyguard.onScreenTurnedOff();
            mDisplayRotation.updateOrientationListener();
        }
    }
    @Override public void onDisplaySwitchStart(int displayId) {
        if (displayId == DEFAULT_DISPLAY) mDisplayPolicy.onDisplaySwitchStart();
    }
    @Override public boolean isScreenOn(int displayId) { return query(SCREEN_ON, displayId, false); }
    @Override public boolean okToAnimate(boolean ignoreScreenOn) {
        return query(CAN_ANIMATE, 0, ignoreScreenOn);
    }
    @Override public boolean canDismissBootAnimation() {
        return query(BOOT_ANIMATION_DISMISSIBLE, 0, false);
    }
    @Override public boolean isKeyguardDrawnLw() { return query(KEYGUARD_DRAWN_ONCE, 0, false); }

    @Override public void startedWakingUpGlobal(int reason) {
        command(GOING_TO_SLEEP, 0, 0, false);
    }
    @Override public void finishedWakingUpGlobal(int reason) {}
    @Override public void startedGoingToSleepGlobal(int reason) {
        command(GOING_TO_SLEEP, 0, 0, true);
    }
    @Override public void finishedGoingToSleepGlobal(int reason) {
        command(GOING_TO_SLEEP, 0, 0, false);
    }
    @Override public void startedWakingUp(int group, int reason, boolean anyInteractive) {
        if (group != DEFAULT_DISPLAY_GROUP) return;
        mGoingToSleep = false;
        mDisplayPolicy.setAwake(true);
        mDisplayRotation.updateOrientationListener();
        applyKeyguardOcclusionChange();
        mKeyguard.onStartedWakingUp(reason, false);
        mHandler.post(() -> updateLockscreenTimer(false));
    }
    @Override public void finishedWakingUp(int group, int reason) {
        if (group == DEFAULT_DISPLAY_GROUP) mKeyguard.onFinishedWakingUp();
    }
    @Override public void startedGoingToSleep(int group, int reason, boolean anyInteractive) {
        if (group != DEFAULT_DISPLAY_GROUP) return;
        mGoingToSleep = true;
        mKeyguard.onStartedGoingToSleep(reason);
    }
    @Override public void finishedGoingToSleep(int group, int reason) {
        if (group != DEFAULT_DISPLAY_GROUP) return;
        mGoingToSleep = false;
        mDisplayPolicy.setAwake(false);
        mDisplayRotation.updateOrientationListener();
        mKeyguard.onFinishedGoingToSleep(reason, false);
        mHandler.post(() -> updateLockscreenTimer(false));
    }
    @Override public void notifyLidSwitchChanged(long whenNanos, boolean lidOpen) {
        mDisplayPolicy.setLidState(lidOpen ? LID_OPEN : LID_CLOSED);
    }
    @Override public void notifyCameraLensCoverSwitchChanged(long whenNanos, boolean covered) {}

    private void bindKeyguard() {
        if (!mKeyguardBound.compareAndSet(false, true)) return;
        mKeyguard.bindService(mContext, mHandler);
    }
    @Override public void onSystemUiStarted() { bindKeyguard(); }
    @Override public void systemReady() {
        DroidloomHostChannels.start(mContext, mHandler);
        mKeyguard.onSystemReady();
        if (command(SYSTEM_READY, 0, 0, false) == NOTIFY_BOOT_COMPLETED) {
            mKeyguard.onBootCompleted();
        }
    }
    @Override public void systemBooted() {
        bindKeyguard();
        if (command(SYSTEM_BOOTED, 0, 0, false) == NOTIFY_BOOT_COMPLETED) {
            mKeyguard.onBootCompleted();
        }
        startedWakingUp(DEFAULT_DISPLAY_GROUP, PowerManager.WAKE_REASON_UNKNOWN, true);
        finishedWakingUp(DEFAULT_DISPLAY_GROUP, PowerManager.WAKE_REASON_UNKNOWN);
        if (mDefaultDisplay.getState() == Display.STATE_ON || isScreenOn(DEFAULT_DISPLAY)) {
            screenTurningOn(DEFAULT_DISPLAY, mDisplayPolicy.getScreenOnListener());
            screenTurnedOn(DEFAULT_DISPLAY);
        } else {
            command(BOOT_WITH_OUTPUT_OFF, 0, 0, false);
            mFuncs.enableScreenIfNeeded();
        }
    }
    @Override public void enableScreenAfterBoot() { mFuncs.updateRotation(true, false); }
    @Override public void showBootMessage(CharSequence message, boolean always) {
        Slog.i(TAG, "Android boot: " + message);
    }
    @Override public void hideBootMessages() {}

    // Credential and trust decisions remain with Android's actual keyguard.
    @Override public KeyguardServiceDelegate getKeyguardServiceDelegate() { return mKeyguard; }
    @Override public void onKeyguardOccludedChangedLw(boolean occluded) { mPendingOccluded = occluded; }
    @Override public void applyKeyguardOcclusionChange() { mKeyguard.setOccluded(mPendingOccluded); }
    @Override public void showDismissibleKeyguard() { mKeyguard.showDismissibleKeyguard(); }
    @Override public void enableKeyguard(boolean enabled) { mKeyguard.setKeyguardEnabled(enabled); }
    @Override public void exitKeyguardSecurely(Consumer<Boolean> callback) {
        if (!mKeyguard.hasKeyguard()) {
            callback.accept(false);
            return;
        }
        // A disconnected keyguard may never answer. Complete the request once,
        // without treating timeout/disconnection as successful authentication.
        AtomicBoolean completed = new AtomicBoolean();
        Runnable timeout = () -> {
            if (completed.compareAndSet(false, true)) callback.accept(false);
        };
        mHandler.postDelayed(timeout, KEYGUARD_BOOT_TIMEOUT_MS);
        mKeyguard.verifyUnlock(success -> {
            if (completed.compareAndSet(false, true)) {
                mHandler.removeCallbacks(timeout);
                callback.accept(success);
            }
        });
    }
    @Override public boolean isKeyguardLocked() {
        return isKeyguardShowingAndNotOccluded() || inKeyguardRestrictedKeyInputMode();
    }
    @Override public boolean isKeyguardSecure(int userId) { return mKeyguard.isSecure(userId); }
    @Override public boolean isKeyguardOccluded() { return mKeyguard.isOccluded(); }
    private boolean missingSecureKeyguard() {
        // The upstream delegate disables its reported state if bindService fails.
        // A configured credential must still keep this no-credential-UI cell locked.
        return !mKeyguard.hasKeyguard() && mKeyguard.isSecure(mCurrentUser);
    }
    @Override public boolean isKeyguardShowing() {
        return mKeyguard.isShowing() || missingSecureKeyguard();
    }
    @Override public boolean isKeyguardShowingAndNotOccluded() {
        return isKeyguardShowing() && !isKeyguardOccluded();
    }
    @Override public boolean isKeyguardTrustedLw() { return mKeyguard.isTrusted(); }
    @Override public boolean inKeyguardRestrictedKeyInputMode() {
        return mKeyguard.isInputRestricted() || missingSecureKeyguard();
    }
    @Override public void dismissKeyguardLw(IKeyguardDismissCallback callback, CharSequence message) {
        if (mKeyguard.isShowing()) {
            mKeyguard.dismiss(callback, message);
        } else if (callback != null) {
            try { callback.onDismissError(); }
            catch (RemoteException e) { Slog.w(TAG, "Keyguard dismiss callback failed", e); }
        }
    }
    @Override public void startKeyguardExitAnimation(long startTime) {
        mKeyguard.startKeyguardExitAnimation(startTime);
    }
    @Override public void lockNow(Bundle options) {
        mContext.enforceCallingOrSelfPermission(android.Manifest.permission.DEVICE_POWER, null);
        mHandler.post(() -> {
            mHandler.removeCallbacks(mLockTimeout);
            mLockTimerActive = false;
            mKeyguard.doKeyguardTimeout(options);
        });
    }
    @Override public void setCurrentUserLw(int userId) {
        mCurrentUser = userId;
        mKeyguard.setCurrentUser(userId);
        command(CLEAR_LOCKSCREEN_DISPLAYS, 0, 0, false);
        mHandler.post(() -> updateLockscreenTimer(false));
        StatusBarManagerInternal statusBar = LocalServices.getService(StatusBarManagerInternal.class);
        if (statusBar != null) statusBar.setCurrentUser(userId);
    }
    @Override public void setSwitchingUser(boolean switching) { mKeyguard.setSwitchingUser(switching); }
    @Override public boolean isUserSetupComplete() {
        return Settings.Secure.getIntForUser(mContext.getContentResolver(),
                Settings.Secure.USER_SETUP_COMPLETE, 0, mCurrentUser) != 0;
    }
    @Override public int getUiMode() {
        return mContext.getResources().getConfiguration().uiMode & Configuration.UI_MODE_TYPE_MASK;
    }
    @Override public void setSafeMode(boolean safeMode) { mSafeMode = safeMode; }
    @Override public void setDismissImeOnBackKeyPressed(boolean dismiss) { mDismissImeOnBack = dismiss; }
    @Override public boolean getDismissImeOnBackKeyPressed() { return mDismissImeOnBack; }

    private void updateLockscreenTimer(boolean restart) {
        boolean enable = query(LOCKSCREEN_TIMER_REQUIRED, 0,
                mDisplayPolicy.isAwake() && mKeyguard.isSecure(mCurrentUser));
        if (enable && (!mLockTimerActive || restart)) {
            mHandler.removeCallbacks(mLockTimeout);
            long timeout = Math.max(0, Settings.System.getLongForUser(mContext.getContentResolver(),
                    Settings.System.SCREEN_OFF_TIMEOUT, 30000, mCurrentUser));
            mHandler.postDelayed(mLockTimeout, timeout);
        } else if (!enable) {
            mHandler.removeCallbacks(mLockTimeout);
        }
        mLockTimerActive = enable;
    }

    // Host shell owns bars, recents, global actions and physical-key haptics.
    // Preserve Android credential locking for apps allowing lock while screen-on.
    @Override public boolean hasNavigationBar() { return false; }
    @Override public void setAllowLockscreenWhenOn(int displayId, boolean allow) {
        if (UserManager.isVisibleBackgroundUsersEnabled()
                && LocalServices.getService(UserManagerInternal.class)
                        .getUserAssignedToDisplay(displayId) != mCurrentUser) return;
        command(ALLOW_LOCKSCREEN_WHEN_ON, displayId, 0, allow);
        mHandler.post(() -> updateLockscreenTimer(false));
    }
    @Override public void userActivity(int displayGroupId, int event) {
        if (displayGroupId == DEFAULT_DISPLAY_GROUP) {
            mHandler.post(() -> {
                if (mLockTimerActive) updateLockscreenTimer(true);
            });
        }
    }
    @Override public void setRecentsVisibilityLw(boolean visible) {}
    @Override public void setPipVisibilityLw(boolean visible) {}
    @Override public void setNavBarVirtualKeyHapticFeedbackEnabledLw(boolean enabled) {}
    @Override public void showRecentApps() {}
    @Override public void showGlobalActions() {}

    @Override public void dump(String prefix, PrintWriter writer, String[] args) {
        writer.println(prefix + "DroidloomWindowManagerPolicy (Rust, ABI 1)");
        writer.println(prefix + "  ready=" + query(READY, 0, false)
                + " booted=" + query(BOOTED, 0, false) + " screenOn=" + isScreenOn()
                + " drawn=" + isKeyguardDrawnLw());
        writer.println(prefix + "  user=" + mCurrentUser + " focusedDisplay=" + mTopFocusedDisplay
                + " safeMode=" + mSafeMode);
        mKeyguard.dump(prefix + "  ", writer);
    }
    @Override public void dumpDebug(ProtoOutputStream proto, long fieldId) {
        long token = proto.start(fieldId);
        mKeyguard.dumpDebug(proto,
                android.internal.perfetto.protos.Windowmanagerservice.WindowManagerPolicyProto.KEYGUARD_DELEGATE);
        proto.end(token);
    }
}
