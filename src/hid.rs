//! USB HID report descriptors, packet builders, and Android shortcut maps.

/// USB HID modifier bitmasks matching boot keyboard reports.
pub const MOD_CTRL: u8 = 0x01;
pub const MOD_SHIFT: u8 = 0x02;
pub const MOD_ALT: u8 = 0x04;
pub const MOD_META: u8 = 0x08;

/// 67-byte scrcpy-compatible 5-button relative mouse + vertical/horizontal wheel descriptor.
pub const MOUSE_REPORT_DESC: [u8; 67] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Buttons)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x05, //     Usage Maximum (5)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x05, //     Report Count (5)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x03, //     Report Size (3)
    0x81, 0x01, //     Input (Constant)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x03, //     Report Count (3)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0x05, 0x0C, //     Usage Page (Consumer)
    0x0A, 0x38, 0x02, // Usage (AC Pan)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0xC0, //         End Collection
    0xC0, //       End Collection
];

/// 45-byte standard 8-byte USB boot keyboard descriptor.
pub const KEYB_REPORT_DESC: [u8; 45] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0xE0, //   Usage Minimum (224 - Left Control)
    0x29, 0xE7, //   Usage Maximum (231 - Right GUI)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data, Variable, Absolute)
    0x95, 0x01, //   Report Count (1)
    0x75, 0x08, //   Report Size (8)
    0x81, 0x01, //   Input (Constant)
    0x95, 0x06, //   Report Count (6)
    0x75, 0x08, //   Report Size (8)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x65, //   Logical Maximum (101)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0x00, //   Usage Minimum (0)
    0x29, 0x65, //   Usage Maximum (101)
    0x81, 0x00, //   Input (Data, Array)
    0xC0, //       End Collection
];

/// Clamp an integer into signed HID relative motion bounds (`-127..=127`).
#[must_use]
pub fn clamp_i8(val: i32) -> i8 {
    val.clamp(-127, 127) as i8
}

/// Pack a 5-byte relative mouse + scroll HID input report.
#[must_use]
pub fn pack_mouse_report(buttons: u8, dx: i32, dy: i32, vscroll: i32, hscroll: i32) -> [u8; 5] {
    [
        buttons & 0x1F,
        clamp_i8(dx) as u8,
        clamp_i8(dy) as u8,
        clamp_i8(vscroll) as u8,
        clamp_i8(hscroll) as u8,
    ]
}

/// Pack an 8-byte boot keyboard HID input report.
#[must_use]
pub fn pack_keyboard_report(mod_mask: u8, keycode: u8) -> [u8; 8] {
    [mod_mask, 0, keycode.min(0x65), 0, 0, 0, 0, 0]
}

/// Map named Android and TalkBack accessibility shortcuts into `(modifier, hid_keycode)`.
#[must_use]
pub fn shortcut_to_hid(name: &str) -> Option<(u8, u8)> {
    match name {
        "home" => Some((MOD_META, 0x28)),
        "back" | "escape" => Some((0, 0x29)),
        "settings" => Some((MOD_META, 0x0C)),
        "notifications" => Some((MOD_META, 0x11)),
        "assistant" => Some((MOD_META, 0x04)),
        "wake" => Some((0, 0x2C)),
        "tab" => Some((0, 0x2B)),
        "enter" => Some((0, 0x28)),
        "backspace" => Some((0, 0x2A)),
        "up" => Some((0, 0x52)),
        "down" => Some((0, 0x51)),
        "left" => Some((0, 0x50)),
        "right" => Some((0, 0x4F)),
        "talkback_next" => Some((MOD_ALT, 0x4F)),
        "talkback_prev" => Some((MOD_ALT, 0x50)),
        "talkback_activate" => Some((MOD_ALT, 0x28)),
        _ => None,
    }
}

/// Map a printable ASCII character into a US-layout `(modifier, hid_keycode)` pair.
#[must_use]
pub fn ascii_to_hid(ch: char) -> Option<(u8, u8)> {
    let pair = match ch {
        'a'..='z' => (0, 0x04 + (ch as u8 - b'a')),
        'A'..='Z' => (MOD_SHIFT, 0x04 + (ch as u8 - b'A')),
        '1' => (0, 0x1E),
        '!' => (MOD_SHIFT, 0x1E),
        '2' => (0, 0x1F),
        '@' => (MOD_SHIFT, 0x1F),
        '3' => (0, 0x20),
        '#' => (MOD_SHIFT, 0x20),
        '4' => (0, 0x21),
        '$' => (MOD_SHIFT, 0x21),
        '5' => (0, 0x22),
        '%' => (MOD_SHIFT, 0x22),
        '6' => (0, 0x23),
        '^' => (MOD_SHIFT, 0x23),
        '7' => (0, 0x24),
        '&' => (MOD_SHIFT, 0x24),
        '8' => (0, 0x25),
        '*' => (MOD_SHIFT, 0x25),
        '9' => (0, 0x26),
        '(' => (MOD_SHIFT, 0x26),
        '0' => (0, 0x27),
        ')' => (MOD_SHIFT, 0x27),
        '\n' | '\r' => (0, 0x28),
        '\t' => (0, 0x2B),
        ' ' => (0, 0x2C),
        '-' => (0, 0x2D),
        '_' => (MOD_SHIFT, 0x2D),
        '=' => (0, 0x2E),
        '+' => (MOD_SHIFT, 0x2E),
        '[' => (0, 0x2F),
        '{' => (MOD_SHIFT, 0x2F),
        ']' => (0, 0x30),
        '}' => (MOD_SHIFT, 0x30),
        '\\' => (0, 0x31),
        '|' => (MOD_SHIFT, 0x31),
        ';' => (0, 0x33),
        ':' => (MOD_SHIFT, 0x33),
        '\'' => (0, 0x34),
        '"' => (MOD_SHIFT, 0x34),
        '`' => (0, 0x35),
        '~' => (MOD_SHIFT, 0x35),
        ',' => (0, 0x36),
        '<' => (MOD_SHIFT, 0x36),
        '.' => (0, 0x37),
        '>' => (MOD_SHIFT, 0x37),
        '/' => (0, 0x38),
        '?' => (MOD_SHIFT, 0x38),
        _ => return None,
    };
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_match_expected_hid_sizes() {
        assert_eq!(MOUSE_REPORT_DESC.len(), 67);
        assert_eq!(KEYB_REPORT_DESC.len(), 45);
    }

    #[test]
    fn mouse_report_clamps_and_packs() {
        assert_eq!(pack_mouse_report(0xFF, 500, -500, 128, -999), [0x1F, 127, 0x81, 127, 0x81]);
        assert_eq!(pack_mouse_report(1, 5, -4, 0, 0), [1, 5, 0xFC, 0, 0]);
    }

    #[test]
    fn keyboard_report_clamps_and_packs() {
        assert_eq!(pack_keyboard_report(MOD_ALT, 0x4F), [MOD_ALT, 0, 0x4F, 0, 0, 0, 0, 0]);
        assert_eq!(pack_keyboard_report(0, 0xFF), [0, 0, 0x65, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn ascii_and_shortcuts_map_cleanly() {
        assert_eq!(ascii_to_hid('a'), Some((0, 0x04)));
        assert_eq!(ascii_to_hid('Z'), Some((MOD_SHIFT, 0x1D)));
        assert_eq!(ascii_to_hid('!'), Some((MOD_SHIFT, 0x1E)));
        assert_eq!(ascii_to_hid('é'), None);
        assert_eq!(shortcut_to_hid("talkback_next"), Some((MOD_ALT, 0x4F)));
        assert_eq!(shortcut_to_hid("unknown_action"), None);
    }
}
