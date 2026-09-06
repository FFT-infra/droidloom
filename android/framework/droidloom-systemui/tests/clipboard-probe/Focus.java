package com.android.droidloom.clipboardprobe;
/** Focus target for semantic Wayland clipboard tests; never sends data to another app. */
public final class Focus extends android.app.Activity {
    @Override public void onCreate(android.os.Bundle b){super.onCreate(b);android.widget.EditText field=new android.widget.EditText(this);field.setHint("Droidloom clipboard test");setContentView(field);}
}
