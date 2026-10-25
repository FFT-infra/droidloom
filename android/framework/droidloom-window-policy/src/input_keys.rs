// Android key values from the pinned KeyEvent API; evdev mapping retained from InputBridge.
pub(super) fn android_keycode(scan: u32) -> Option<i32> {
    Some(match scan {
        1 => 111,   // ESCAPE
        2 => 8,     // 1
        3 => 9,     // 2
        4 => 10,    // 3
        5 => 11,    // 4
        6 => 12,    // 5
        7 => 13,    // 6
        8 => 14,    // 7
        9 => 15,    // 8
        10 => 16,   // 9
        11 => 7,    // 0
        12 => 69,   // MINUS
        13 => 70,   // EQUALS
        14 => 67,   // DEL
        15 => 61,   // TAB
        16 => 45,   // Q
        17 => 51,   // W
        18 => 33,   // E
        19 => 46,   // R
        20 => 48,   // T
        21 => 53,   // Y
        22 => 49,   // U
        23 => 37,   // I
        24 => 43,   // O
        25 => 44,   // P
        26 => 71,   // LEFT_BRACKET
        27 => 72,   // RIGHT_BRACKET
        28 => 66,   // ENTER
        29 => 113,  // CTRL_LEFT
        30 => 29,   // A
        31 => 47,   // S
        32 => 32,   // D
        33 => 34,   // F
        34 => 35,   // G
        35 => 36,   // H
        36 => 38,   // J
        37 => 39,   // K
        38 => 40,   // L
        39 => 74,   // SEMICOLON
        40 => 75,   // APOSTROPHE
        41 => 68,   // GRAVE
        42 => 59,   // SHIFT_LEFT
        43 => 73,   // BACKSLASH
        44 => 54,   // Z
        45 => 52,   // X
        46 => 31,   // C
        47 => 50,   // V
        48 => 30,   // B
        49 => 42,   // N
        50 => 41,   // M
        51 => 55,   // COMMA
        52 => 56,   // PERIOD
        53 => 76,   // SLASH
        54 => 60,   // SHIFT_RIGHT
        55 => 155,  // NUMPAD_MULTIPLY
        56 => 57,   // ALT_LEFT
        57 => 62,   // SPACE
        58 => 115,  // CAPS_LOCK
        59 => 131,  // F1
        60 => 132,  // F2
        61 => 133,  // F3
        62 => 134,  // F4
        63 => 135,  // F5
        64 => 136,  // F6
        65 => 137,  // F7
        66 => 138,  // F8
        67 => 139,  // F9
        68 => 140,  // F10
        69 => 143,  // NUM_LOCK
        70 => 116,  // SCROLL_LOCK
        71 => 151,  // NUMPAD_7
        72 => 152,  // NUMPAD_8
        73 => 153,  // NUMPAD_9
        74 => 156,  // NUMPAD_SUBTRACT
        75 => 148,  // NUMPAD_4
        76 => 149,  // NUMPAD_5
        77 => 150,  // NUMPAD_6
        78 => 157,  // NUMPAD_ADD
        79 => 145,  // NUMPAD_1
        80 => 146,  // NUMPAD_2
        81 => 147,  // NUMPAD_3
        82 => 144,  // NUMPAD_0
        83 => 158,  // NUMPAD_DOT
        87 => 141,  // F11
        88 => 142,  // F12
        96 => 160,  // NUMPAD_ENTER
        97 => 114,  // CTRL_RIGHT
        98 => 154,  // NUMPAD_DIVIDE
        99 => 120,  // SYSRQ
        100 => 58,  // ALT_RIGHT
        102 => 122, // MOVE_HOME
        103 => 19,  // DPAD_UP
        104 => 92,  // PAGE_UP
        105 => 21,  // DPAD_LEFT
        106 => 22,  // DPAD_RIGHT
        107 => 123, // MOVE_END
        108 => 20,  // DPAD_DOWN
        109 => 93,  // PAGE_DOWN
        110 => 124, // INSERT
        111 => 112, // FORWARD_DEL
        113 => 164, // VOLUME_MUTE
        114 => 25,  // VOLUME_DOWN
        115 => 24,  // VOLUME_UP
        117 => 161, // NUMPAD_EQUALS
        119 => 121, // BREAK
        121 => 159, // NUMPAD_COMMA
        125 => 117, // META_LEFT
        126 => 118, // META_RIGHT
        139 => 82,  // MENU
        158 => 4,   // BACK
        159 => 125, // FORWARD
        262 => 194, // BUTTON_7 (Pinch / 轻捏)
        263 => 195, // BUTTON_8 (Double-tap / 双击)
        264 => 196, // BUTTON_9 (Slide Up / 上滑)
        265 => 197, // BUTTON_10 (Slide Down / 下滑)
        331 => 194, // BUTTON_7 (Stylus Primary / 书写键)
        332 => 195, // BUTTON_8 (Stylus Secondary / 批注键)
        0x113 | 0x116 => 4, // Mouse side/back button navigation
        0x114 | 0x115 => 125, // Mouse extra/forward button navigation
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_stylus_and_gesture_keys() {
        assert_eq!(android_keycode(262), Some(194));
        assert_eq!(android_keycode(263), Some(195));
        assert_eq!(android_keycode(264), Some(196));
        assert_eq!(android_keycode(265), Some(197));
        assert_eq!(android_keycode(331), Some(194));
        assert_eq!(android_keycode(332), Some(195));
    }
}
