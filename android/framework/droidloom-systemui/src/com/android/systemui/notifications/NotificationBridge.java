/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.notifications;

import android.app.ActivityOptions;
import android.app.Notification;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.ComponentName;
import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.Bundle;
import android.os.Handler;
import android.os.UserHandle;
import android.service.notification.NotificationListenerService;
import android.service.notification.StatusBarNotification;
import android.util.Log;
import android.util.LruCache;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.UUID;
import org.json.JSONArray;
import org.json.JSONObject;

/** Event-driven listener; PendingIntents stay inside Android and never cross the socket. */
public final class NotificationBridge extends NotificationListenerService implements AutoCloseable {
    private static final String TAG = "DroidloomNotifications";
    private static final int MAX_ACTIVE = 256;
    private static final int MAX_FRAME = 131072;
    private final Context context;
    private final Handler main;
    private final Object lock = new Object();
    private final LinkedHashMap<String, Entry> active = new LinkedHashMap<>();
    private final LruCache<String, JSONArray> icons = new LruCache<>(32);
    private final String session = UUID.randomUUID().toString();
    private long generation;
    private boolean dirty;
    private volatile boolean stopped, connected, listening;
    private volatile LocalSocket socket;
    private static final class Entry {
        final StatusBarNotification sbn;
        final String version;
        final int importance;
        Entry(StatusBarNotification sbn, String version, int importance) {
            this.sbn = sbn; this.version = version; this.importance = importance;
        }
    }
    public NotificationBridge(Context context) { this.context=context; main=new Handler(context.getMainLooper()); }
    public void start() {
        try {
            registerAsSystemService(context, new ComponentName(context, NotificationBridge.class), UserHandle.USER_SYSTEM);
            Log.i(TAG, "DROIDLOOM_NOTIFICATIONS_ABI=1;");
            Thread worker = new Thread(this::connect, "droidloom-notifications");
            worker.setDaemon(true); worker.start();
        } catch (Exception e) { Log.e(TAG, "Notification listener registration failed: " + e.getClass().getSimpleName()); }
    }
    @Override public void onListenerConnected() {
        synchronized (lock) {
            active.clear(); listening=true;
            StatusBarNotification[] all=getActiveNotifications();
            if (all!=null) for (StatusBarNotification sbn:all) put(sbn, getCurrentRanking());
            dirty=true; lock.notifyAll();
        }
    }
    @Override public void onListenerDisconnected() {
        listening=false;
        disconnect();
    }
    @Override public void onNotificationPosted(StatusBarNotification sbn, RankingMap ranking) {
        synchronized(lock) { put(sbn,ranking); dirty=true; lock.notifyAll(); }
    }
    @Override public void onNotificationRemoved(StatusBarNotification sbn) {
        synchronized(lock) { active.remove(sbn.getKey()); dirty=true; lock.notifyAll(); }
    }
    private void put(StatusBarNotification sbn, RankingMap map) {
        Ranking rank = new Ranking();
        int importance=NotificationManager.IMPORTANCE_DEFAULT;
        if (map!=null && map.getRanking(sbn.getKey(),rank)) {
            importance=rank.getImportance();
            if (rank.isSuspended() || importance==NotificationManager.IMPORTANCE_NONE) { active.remove(sbn.getKey()); return; }
        }
        // Group summaries duplicate their children on desktops without Android grouping.
        if ((sbn.getNotification().flags & Notification.FLAG_GROUP_SUMMARY)!=0) { active.remove(sbn.getKey()); return; }
        if (!active.containsKey(sbn.getKey()) && active.size()>=MAX_ACTIVE) active.remove(active.keySet().iterator().next());
        active.put(sbn.getKey(),new Entry(sbn,session+"-"+(++generation),importance));
    }
    private void connect() {
        com.android.droidloom.runtime.CpuPlacement.background();
        while (!stopped) {
            try (LocalSocket current = new LocalSocket()) {
                socket=current;
                current.connect(new LocalSocketAddress("droidloom-notifications", LocalSocketAddress.Namespace.ABSTRACT));
                if (current.getPeerCredentials().getUid()!=0) throw new SecurityException();
                DataOutputStream out=new DataOutputStream(current.getOutputStream());
                send(out,new JSONObject().put("type","hello").put("abi",1));
                current.setSoTimeout(5000);
                DataInputStream input=new DataInputStream(current.getInputStream());
                int helloLength=input.readInt();
                if(helloLength<=0 || helloLength>4096) throw new IllegalArgumentException();
                byte[] helloBytes=new byte[helloLength]; input.readFully(helloBytes);
                JSONObject hello=new JSONObject(new String(helloBytes,StandardCharsets.UTF_8));
                if(!"hello".equals(hello.optString("type")) || hello.optInt("abi")!=1) throw new IllegalArgumentException();
                current.setSoTimeout(0);
                connected=true;
                Thread writer=new Thread(() -> writeSnapshots(current,out),"droidloom-notifications-out");
                writer.setDaemon(true); writer.start();
                synchronized(lock) { dirty=true; lock.notifyAll(); }
                try {
                    while(!stopped) {
                        int length=input.readInt();
                        if(length<=0 || length>4096) throw new IllegalArgumentException();
                        byte[] bytes=new byte[length]; input.readFully(bytes);
                        JSONObject message=new JSONObject(new String(bytes,StandardCharsets.UTF_8));
                        main.post(() -> command(message));
                    }
                } finally {
                    connected=false; closeSocket(current);
                    synchronized(lock) { lock.notifyAll(); }
                    writer.join();
                }
            } catch(Exception e) {
                if(!stopped) Log.w(TAG,"Notification transport disconnected: "+e.getClass().getSimpleName());
            } finally { connected=false; socket=null; }
            if(!stopped) try { Thread.sleep(2000); } catch(InterruptedException ignored) { }
        }
    }
    private void writeSnapshots(LocalSocket current, DataOutputStream out) {
        com.android.droidloom.runtime.CpuPlacement.background();
        try {
            while(!stopped && connected) {
                ArrayList<Entry> entries;
                synchronized(lock) {
                    while(!stopped && connected && (!dirty || !listening)) lock.wait();
                    if(stopped || !connected) return;
                    entries=new ArrayList<>(active.values()); dirty=false;
                }
                send(out,new JSONObject().put("type","begin"));
                for(Entry entry:entries) {
                    try { send(out,new JSONObject().put("type","post").put("notification",encode(entry))); }
                    catch(org.json.JSONException | android.content.pm.PackageManager.NameNotFoundException e) { /* An uninstalled app can race a snapshot. */ }
                }
                send(out,new JSONObject().put("type","end"));
            }
        } catch(Exception e) { closeSocket(current); }
    }
    private JSONObject encode(Entry entry) throws Exception {
        StatusBarNotification sbn=entry.sbn; Notification n=sbn.getNotification(); Bundle extras=n.extras;
        String title=text(extras.getCharSequence(Notification.EXTRA_TITLE),2048);
        CharSequence big=extras.getCharSequence(Notification.EXTRA_BIG_TEXT);
        String body=text(big!=null?big:extras.getCharSequence(Notification.EXTRA_TEXT),8192);
        if(body.isEmpty()) {
            CharSequence[] lines=extras.getCharSequenceArray(Notification.EXTRA_TEXT_LINES);
            if(lines!=null) body=text(android.text.TextUtils.join("\n",lines),8192);
        }
        JSONArray actions=new JSONArray();
        if(n.contentIntent!=null) actions.put(new JSONObject().put("key","default").put("label","Open"));
        if(n.actions!=null) for(int i=0;i<Math.min(n.actions.length,16);i++) {
            Notification.Action action=n.actions[i];
            // The standard D-Bus interface has no portable inline-reply field.
            if(action.actionIntent!=null && action.getRemoteInputs()==null)
                actions.put(new JSONObject().put("key","action-"+i).put("label",text(action.title,256)));
        }
        String label=context.getPackageManager().getApplicationLabel(context.getPackageManager().getApplicationInfo(sbn.getPackageName(),0)).toString();
        return new JSONObject().put("key",sbn.getKey()).put("version",entry.version).put("package",sbn.getPackageName())
                .put("app",text(label,256)).put("title",title).put("body",body).put("actions",actions)
                // Android HIGH requests a heads-up alert; it does not mean a critical
                // desktop event that must remain visible until explicitly dismissed.
                .put("urgency",entry.importance<=NotificationManager.IMPORTANCE_LOW?0:1)
                .put("ongoing",sbn.isOngoing()).put("clearable",sbn.isClearable())
                .put("icon",icon(sbn.getPackageName()));
    }
    private JSONArray icon(String pkg) {
        JSONArray cached=icons.get(pkg); if(cached!=null) return cached;
        JSONArray bytes=new JSONArray();
        try {
            Drawable drawable=context.getPackageManager().getApplicationIcon(pkg);
            Bitmap bitmap=Bitmap.createBitmap(48,48,Bitmap.Config.ARGB_8888);
            drawable.setBounds(0,0,48,48); drawable.draw(new Canvas(bitmap));
            int[] pixels=new int[48*48]; bitmap.getPixels(pixels,0,48,0,0,48,48); bitmap.recycle();
            for(int pixel:pixels) { bytes.put((pixel>>16)&255); bytes.put((pixel>>8)&255); bytes.put(pixel&255); bytes.put((pixel>>>24)&255); }
        } catch(Exception ignored) { }
        icons.put(pkg,bytes); return bytes;
    }
    private static String text(CharSequence source,int max) {
        if(source==null) return "";
        String value=source.toString().replace("\u0000","");
        if(value.length()<=max) return value;
        int end=max; if(Character.isHighSurrogate(value.charAt(end-1))) end--;
        return value.substring(0,end);
    }
    private void command(JSONObject message) {
        try {
            Entry entry;
            synchronized(lock) { entry=active.get(message.getString("key")); }
            if(entry==null || !entry.version.equals(message.getString("version"))) return;
            Notification n=entry.sbn.getNotification();
            String type=message.getString("type");
            if(type.equals("dismiss")) { if(entry.sbn.isClearable()) cancelNotification(entry.sbn.getKey()); return; }
            if(!type.equals("action")) return;
            String action=message.getString("action"); PendingIntent intent=null;
            if(action.equals("default")) intent=n.contentIntent;
            else if(action.startsWith("action-") && n.actions!=null) {
                int index=Integer.parseInt(action.substring(7));
                if(index>=0 && index<n.actions.length && n.actions[index].getRemoteInputs()==null) intent=n.actions[index].actionIntent;
            }
            if(intent==null) return;
            ActivityOptions options=ActivityOptions.makeBasic().setPendingIntentBackgroundActivityStartMode(ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED);
            intent.send(context,0,null,null,null,null,options.toBundle());
            if(action.equals("default") && (n.flags & Notification.FLAG_AUTO_CANCEL)!=0 && entry.sbn.isClearable()) cancelNotification(entry.sbn.getKey());
        } catch(Exception e) { Log.w(TAG,"Notification action unavailable: "+e.getClass().getSimpleName()); }
    }
    private static void send(DataOutputStream out,JSONObject message) throws Exception {
        byte[] bytes=message.toString().getBytes(StandardCharsets.UTF_8);
        if(bytes.length>MAX_FRAME) throw new IllegalArgumentException("Notification frame too large");
        out.writeInt(bytes.length); out.write(bytes); out.flush();
    }
    private static void closeSocket(LocalSocket socket) {
        try { socket.shutdownInput(); } catch(Exception ignored) { }
        try { socket.shutdownOutput(); } catch(Exception ignored) { }
        try { socket.close(); } catch(Exception ignored) { }
    }
    private void disconnect() { connected=false; LocalSocket current=socket; if(current!=null) closeSocket(current); synchronized(lock) { lock.notifyAll(); } }
    @Override public void close() { stopped=true; disconnect(); try { unregisterAsSystemService(); } catch(Exception ignored) { } }
    public void dump(PrintWriter out) { synchronized(lock) { out.println("DROIDLOOM_NOTIFICATIONS_ABI=1; notificationsListening="+listening+" notificationsConnected="+connected+" activeNotifications="+active.size()); } }
}
