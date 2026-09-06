package com.android.droidloom.ime;

public final class ByteOffsetsTest {
    private static void check(int expected, CharSequence text, int bytes, boolean backwards) {
        int result = ByteOffsets.utf16Length(text, bytes, backwards);
        if (expected != result) throw new AssertionError(expected + " != " + result);
    }
    public static void main(String[] args) {
        check(1, "hello", 1, true);
        check(1, "caffè", 2, true);
        check(2, "fox🦊", 4, true);
        check(2, "🦊fox", 4, false);
        check(-1, "caffè", 1, true);
        check(-1, "fox🦊", 3, true);
        check(-1, "x", 2, false);
        check(-1, null, 1, true);
        check(0, null, 0, true);
        System.out.println("IME Unicode offset tests passed");
    }
}
