package com.android.droidloom.notificationprobe;
public class ClickReceiver extends android.content.BroadcastReceiver {
    @Override public void onReceive(android.content.Context context,android.content.Intent intent) {
        context.getSharedPreferences("result",0).edit().putString("action",("default".equals(intent.getStringExtra("kind"))?"default:":"button:")+intent.getIntExtra("id",0)).commit();
    }
}
