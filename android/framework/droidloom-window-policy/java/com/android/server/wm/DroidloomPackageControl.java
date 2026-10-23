/*
 * Copyright 2026 Droidloom
 * SPDX-License-Identifier: GPL-3.0-or-later
 * See LICENSE and LICENSES/GPL-3.0-or-later.txt for the license terms.
 */

package com.android.server.wm;

import android.app.ActivityManager;
import android.app.AppGlobals;
import android.app.usage.StorageStats;
import android.app.usage.StorageStatsManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.IPackageDataObserver;
import android.content.pm.IPackageDeleteObserver2;
import android.content.pm.PackageInfo;
import android.content.pm.PackageManager;
import android.content.pm.VersionedPackage;
import android.net.Uri;
import android.os.UserHandle;
import android.provider.Settings;

import org.json.JSONObject;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * What the host's Settings shows and does for one installed package: the
 * same calls Android's own Settings makes for App info.
 */
final class DroidloomPackageControl {
    private static final int USER = 0;
    private static final long WAIT_SECONDS = 60;
    private static final int INSTALLED = PackageManager.MATCH_DIRECT_BOOT_AWARE
            | PackageManager.MATCH_DIRECT_BOOT_UNAWARE;

    private DroidloomPackageControl() {}

    static JSONObject details(Context context, String name) throws Exception {
        PackageManager packages = context.getPackageManager();
        PackageInfo info = packages.getPackageInfo(name, INSTALLED);
        ApplicationInfo application = info.applicationInfo;
        StorageStats stats = context.getSystemService(StorageStatsManager.class)
                .queryStatsForPackage(application.storageUuid, name, UserHandle.of(USER));
        JSONObject details = new JSONObject();
        details.put("name", application.loadLabel(packages).toString());
        details.put("package", name);
        details.put("version", info.versionName == null ? "" : info.versionName);
        details.put("system", isSystem(application));
        details.put("installed_ms", info.firstInstallTime);
        details.put("updated_ms", info.lastUpdateTime);
        details.put("app_bytes", stats.getAppBytes());
        // Android counts the cache inside the data.
        details.put("data_bytes", Math.max(0, stats.getDataBytes() - stats.getCacheBytes()));
        details.put("cache_bytes", stats.getCacheBytes());
        return details;
    }

    static String act(Context context, DroidloomTaskBackend backend, String name, String action)
            throws Exception {
        ApplicationInfo application = context.getPackageManager()
                .getApplicationInfo(name, INSTALLED);
        switch (action) {
            case "stop":
                ActivityManager.getService().forceStopPackage(name, USER);
                return "Stopped " + name;
            case "uninstall": {
                if (isSystem(application)) {
                    throw new IllegalArgumentException("System apps cannot be removed");
                }
                Outcome outcome = new Outcome();
                AppGlobals.getPackageManager().deletePackageVersioned(
                        new VersionedPackage(name, PackageManager.VERSION_CODE_HIGHEST),
                        new IPackageDeleteObserver2.Stub() {
                            @Override public void onUserActionRequired(Intent intent) {
                                outcome.finish(false, "Android asks for a confirmation");
                            }
                            @Override public void onPackageDeleted(String deleted, int code, String said) {
                                outcome.finish(code == PackageManager.DELETE_SUCCEEDED, said);
                            }
                        }, USER, 0);
                outcome.await("Uninstalling");
                return "Uninstalled " + name;
            }
            case "clear_cache": {
                Outcome outcome = new Outcome();
                AppGlobals.getPackageManager().deleteApplicationCacheFilesAsUser(
                        name, USER, outcome.observer());
                outcome.await("Clearing the cache");
                return "Cleared the cache of " + name;
            }
            case "clear_data": {
                Outcome outcome = new Outcome();
                if (!ActivityManager.getService().clearApplicationUserData(
                        name, false, outcome.observer(), USER)) {
                    throw new IllegalStateException("Android refused to clear " + name);
                }
                outcome.await("Clearing the storage");
                return "Cleared the storage of " + name;
            }
            case "settings": {
                Intent intent = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                        Uri.fromParts("package", name, null));
                return backend.launch(intent, "com.android.settings", USER, null);
            }
            default:
                throw new IllegalArgumentException("Unknown package action");
        }
    }

    private static boolean isSystem(ApplicationInfo application) {
        return (application.flags & ApplicationInfo.FLAG_SYSTEM) != 0;
    }

    /** One callback's answer, waited for on the RPC thread. */
    private static final class Outcome {
        private final CountDownLatch done = new CountDownLatch(1);
        private volatile boolean succeeded;
        private volatile String said;

        void finish(boolean succeeded, String said) {
            this.succeeded = succeeded;
            this.said = said;
            done.countDown();
        }

        IPackageDataObserver observer() {
            return new IPackageDataObserver.Stub() {
                @Override public void onRemoveCompleted(String name, boolean succeeded) {
                    finish(succeeded, null);
                }
            };
        }

        void await(String doing) throws InterruptedException {
            if (!done.await(WAIT_SECONDS, TimeUnit.SECONDS)) {
                throw new IllegalStateException(doing + " took too long");
            }
            if (!succeeded) {
                throw new IllegalStateException(doing + " failed" + (said == null ? "" : ": " + said));
            }
        }
    }
}
