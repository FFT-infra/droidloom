package com.android.droidloom.ime;

/** An editable InputConnection may remain alive while its keyboard is hidden. */
final class EditorVisibility {
    private boolean dismissed;
    private boolean visible;

    void start(boolean editable, boolean restarting) {
        if (!restarting) dismissed = false;
        visible = editable && !dismissed;
    }

    void show(boolean editable) {
        dismissed = false;
        visible = editable;
    }

    void hide() {
        dismissed = true;
        visible = false;
    }

    void finish() { visible = false; }

    boolean dismissFromHost(long requestedShow, long currentShow) {
        if (!visible || requestedShow != currentShow) return false;
        hide();
        return true;
    }

    boolean isVisible() { return visible; }
}
