package com.android.droidloom.ime;

/** Convert Wayland UTF-8 deletion lengths without splitting Unicode characters. */
final class ByteOffsets {
    private ByteOffsets() { }
    static int utf16Length(CharSequence text, int bytes, boolean backwards) {
        if (bytes == 0) return 0;
        if (text == null || bytes < 0 || bytes > 16384) return -1;
        String value = text.toString();
        int position = backwards ? value.length() : 0;
        int start = position;
        while (bytes > 0 && (backwards ? position > 0 : position < value.length())) {
            int codepoint = backwards ? value.codePointBefore(position) : value.codePointAt(position);
            bytes -= codepoint <= 0x7f ? 1 : codepoint <= 0x7ff ? 2 : codepoint <= 0xffff ? 3 : 4;
            position += (backwards ? -1 : 1) * Character.charCount(codepoint);
        }
        return bytes == 0 ? Math.abs(position - start) : -1;
    }
}
