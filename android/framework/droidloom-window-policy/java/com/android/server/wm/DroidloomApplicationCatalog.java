/*
 * Copyright 2026 Droidloom
 * SPDX-License-Identifier: GPL-3.0-or-later
 * See LICENSE and LICENSES/GPL-3.0-or-later.txt for the license terms.
 */

package com.android.server.wm;

import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ActivityInfo;
import android.content.pm.PackageInfo;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.PorterDuff;
import android.graphics.drawable.Drawable;
import android.util.Base64;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * In-process package manager/resource adapter for Droidloom's host catalog.
 *
 * <p>Android remains authoritative for intent resolution, localized labels,
 * adaptive icons, and per-user package state. The host receives only a bounded
 * JSON snapshot or one rendered PNG and never parses APK resources itself.</p>
 */
final class DroidloomApplicationCatalog {
    private static final int MIN_ICON_SIZE = 16;
    private static final int MAX_ICON_SIZE = 512;
    private static final Set<String> HIDDEN_PACKAGES = new HashSet<>(Arrays.asList(
            "android.droidloom.bootstrap",
            "com.android.launcher3",
            "com.android.systemui"
    ));

    private DroidloomApplicationCatalog() {}

    static JSONArray listApplications(Context context, int userId) throws Exception {
        PackageManager packageManager = context.getPackageManager();
        List<ResolveInfo> activities = new ArrayList<>(
                queryLauncherActivities(packageManager, userId, null));
        activities.removeIf(activity -> HIDDEN_PACKAGES.contains(
                activity.activityInfo.packageName));
        activities.sort(Comparator.comparing(
                activity -> component(activity).flattenToString()));

        JSONArray result = new JSONArray();
        for (ResolveInfo activity : activities) {
            ComponentName component = component(activity);
            CharSequence loadedLabel = activity.loadLabel(packageManager);
            String label = loadedLabel == null ? component.getPackageName()
                    : loadedLabel.toString();
            if (label.isEmpty()) {
                label = component.getPackageName();
            }
            JSONObject entry = new JSONObject();
            entry.put("name", label);
            entry.put("package", component.getPackageName());
            entry.put("component", component.flattenToString());
            entry.put("icon_key", iconKey(packageManager, component) + ":"
                    + Integer.toHexString(context.getResources().getConfiguration().getLocales().toLanguageTags().hashCode()));
            result.put(entry);
        }
        return result;
    }

    static String renderIcon(Context context, int userId, ComponentName requested, int size)
            throws Exception {
        if (size < MIN_ICON_SIZE || size > MAX_ICON_SIZE || requested == null)
            throw new IllegalArgumentException("Invalid launcher icon request");
        if (HIDDEN_PACKAGES.contains(requested.getPackageName())) {
            throw new IllegalArgumentException("component is hidden from the host launcher");
        }
        PackageManager packageManager = context.getPackageManager();
        ResolveInfo selected = null;
        for (ResolveInfo activity
                : queryLauncherActivities(packageManager, userId, requested.getPackageName())) {
            if (requested.equals(component(activity))) {
                selected = activity;
                break;
            }
        }
        if (selected == null) {
            throw new IllegalArgumentException("component is not an enabled launcher activity");
        }

        Drawable drawable = selected.loadIcon(packageManager);
        if (drawable == null) {
            drawable = packageManager.getDefaultActivityIcon();
        }
        Bitmap bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888);
        Canvas canvas = new Canvas(bitmap);
        canvas.drawColor(Color.TRANSPARENT, PorterDuff.Mode.CLEAR);
        setCenteredBounds(drawable, size);
        drawable.draw(canvas);
        ByteArrayOutputStream encoded = new ByteArrayOutputStream();
        if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, encoded)) {
            bitmap.recycle();
            throw new IllegalStateException("failed to encode launcher icon");
        }
        bitmap.recycle();
        return Base64.encodeToString(encoded.toByteArray(), Base64.NO_WRAP);
    }

    @SuppressWarnings("deprecation")
    private static String iconKey(PackageManager packageManager, ComponentName component)
            throws PackageManager.NameNotFoundException {
        PackageInfo packageInfo = packageManager.getPackageInfo(component.getPackageName(), 0);
        ActivityInfo activityInfo = packageManager.getActivityInfo(component, 0);
        return Long.toUnsignedString(packageInfo.getLongVersionCode()) + ":"
                + Long.toUnsignedString(packageInfo.lastUpdateTime) + ":"
                + Integer.toUnsignedString(activityInfo.getIconResource());
    }

    private static void setCenteredBounds(Drawable drawable, int size) {
        int width = drawable.getIntrinsicWidth();
        int height = drawable.getIntrinsicHeight();
        if (width <= 0 || height <= 0) {
            drawable.setBounds(0, 0, size, size);
            return;
        }
        float scale = Math.min((float) size / width, (float) size / height);
        int renderedWidth = Math.max(1, Math.round(width * scale));
        int renderedHeight = Math.max(1, Math.round(height * scale));
        int left = (size - renderedWidth) / 2;
        int top = (size - renderedHeight) / 2;
        drawable.setBounds(left, top, left + renderedWidth, top + renderedHeight);
    }

    private static ComponentName component(ResolveInfo activity) {
        ActivityInfo info = activity.activityInfo;
        if (info == null || info.packageName == null || info.name == null) {
            throw new IllegalArgumentException("package manager returned an incomplete activity");
        }
        return new ComponentName(info.packageName, info.name);
    }

    @SuppressWarnings("unchecked")
    private static List<ResolveInfo> queryLauncherActivities(
            PackageManager packageManager, int userId, String packageName) throws Exception {
        Intent launcher = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
        if (packageName != null) {
            launcher.setPackage(packageName);
        }
        // Droidloom's current cell runs its framework tools in Android user 0.
        // Keep that hot path entirely on the public SDK; the reflected system
        // API is reserved for future explicitly requested secondary users.
        if (userId == 0) {
            return packageManager.queryIntentActivities(launcher, 0);
        }
        Method query = packageManager.getClass().getMethod(
                "queryIntentActivitiesAsUser", Intent.class, int.class, int.class);
        query.setAccessible(true);
        return (List<ResolveInfo>) query.invoke(packageManager, launcher, 0, userId);
    }

}
