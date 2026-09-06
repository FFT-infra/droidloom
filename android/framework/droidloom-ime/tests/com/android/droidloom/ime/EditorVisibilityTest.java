package com.android.droidloom.ime;

/** Host-JVM regression tests; no Android process or UI required. */
public final class EditorVisibilityTest {
    private static void check(boolean value) {
        if (!value) throw new AssertionError();
    }

    public static void main(String[] args) {
        EditorVisibility state = new EditorVisibility();
        state.start(true, false);
        check(state.isVisible());
        state.hide();
        check(!state.isVisible());
        // Phone hides its keyboard, then restarts a still-editable connection.
        state.start(true, true);
        check(!state.isVisible());
        state.start(true, true);
        check(!state.isVisible());
        // Explicitly refocusing/showing the same editor is allowed.
        state.show(true);
        check(state.isVisible());
        state.start(true, true);
        check(state.isVisible());
        state.hide();
        state.finish();
        check(!state.isVisible());
        // A genuinely new editor has a new visibility lifecycle.
        state.start(true, false);
        check(state.isVisible());
        state.start(false, true);
        check(!state.isVisible());
        state.show(false);
        check(!state.isVisible());
        state.start(true, false);
        state.finish();
        check(!state.isVisible());
        state.show(true);
        check(!state.dismissFromHost(4, 5));
        check(state.isVisible());
        check(state.dismissFromHost(5, 5));
        check(!state.isVisible());
        check(!state.dismissFromHost(5, 5));
        state.start(true, true);
        check(!state.isVisible());
        // A second tap on the same focused editor asks to show again.
        state.show(true);
        check(state.isVisible());
        check(!state.dismissFromHost(5, 6));
        check(state.isVisible());
        System.out.println("EditorVisibility: 20 lifecycle assertions passed");
    }
}
