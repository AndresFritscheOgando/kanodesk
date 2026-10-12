//! HID usages to PC keys: set-1 scan codes (what Windows' `SendInput` takes
//! with `KEYEVENTF_SCANCODE`) or, where a scan code cannot express the key,
//! a Windows virtual key.
//!
//! Built from the public USB HID Usage Tables and Microsoft's "Keyboard Scan
//! Code Specification". Keys with an `E0` prefix are "extended". Virtual keys
//! are used for Pause (make code `E1 1D 45`), the Korean language keys, and
//! consumer-page media/browser keys, whose scan codes vary between keyboards.
//! [`from_pc`] is the reverse, for reading a PC keyboard (the viewer's
//! keyboard hook).

use crate::event::{CONSUMER_PAGE, KeyCode};

/// How to press one key on a PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PcKey {
    /// A set-1 make code, with or without the `E0` prefix.
    Scan {
        /// Make code (without prefix).
        code: u8,
        /// Whether it has the `E0` prefix.
        extended: bool,
    },
    /// A Windows virtual-key code.
    Vk(u16),
}

const VK_PAUSE: u16 = 0x13;
const VK_HANGUL: u16 = 0x15;
const VK_HANJA: u16 = 0x19;
const VK_NUMLOCK: u16 = 0x90;

/// Consumer-page usages and their Windows virtual keys.
const CONSUMER_VKS: &[(u16, u16)] = &[
    (0xE9, 0xAF),  // Volume Up
    (0xEA, 0xAE),  // Volume Down
    (0xE2, 0xAD),  // Mute
    (0xCD, 0xB3),  // Play/Pause
    (0xB7, 0xB2),  // Stop
    (0xB5, 0xB0),  // Next Track
    (0xB6, 0xB1),  // Previous Track
    (0x224, 0xA6), // Browser Back
    (0x225, 0xA7), // Browser Forward
    (0x227, 0xA8), // Browser Refresh
    (0x226, 0xA9), // Browser Stop
    (0x221, 0xAA), // Browser Search
    (0x22A, 0xAB), // Browser Favorites
    (0x223, 0xAC), // Browser Home
    (0x18A, 0xB4), // Mail
    (0x183, 0xB5), // Media Select
    (0x194, 0xB6), // Launch App 1 (My Computer)
    (0x192, 0xB7), // Launch App 2 (Calculator)
];

const fn k(code: u8) -> Option<PcKey> {
    Some(PcKey::Scan {
        code,
        extended: false,
    })
}

const fn e(code: u8) -> Option<PcKey> {
    Some(PcKey::Scan {
        code,
        extended: true,
    })
}

const fn vk(code: u16) -> Option<PcKey> {
    Some(PcKey::Vk(code))
}

/// How to press `code` on a PC, if it can be.
pub fn to_pc(code: KeyCode) -> Option<PcKey> {
    match code.page() {
        0x07 => keyboard_page(code.usage()),
        CONSUMER_PAGE => CONSUMER_VKS
            .iter()
            .find(|(usage, _)| *usage == code.usage())
            .map(|&(_, v)| PcKey::Vk(v)),
        _ => None,
    }
}

/// The PC key for a usage on the keyboard page.
fn keyboard_page(usage: u16) -> Option<PcKey> {
    match usage {
        // Letters A–Z.
        0x04 => k(0x1E),
        0x05 => k(0x30),
        0x06 => k(0x2E),
        0x07 => k(0x20),
        0x08 => k(0x12),
        0x09 => k(0x21),
        0x0A => k(0x22),
        0x0B => k(0x23),
        0x0C => k(0x17),
        0x0D => k(0x24),
        0x0E => k(0x25),
        0x0F => k(0x26),
        0x10 => k(0x32),
        0x11 => k(0x31),
        0x12 => k(0x18),
        0x13 => k(0x19),
        0x14 => k(0x10),
        0x15 => k(0x13),
        0x16 => k(0x1F),
        0x17 => k(0x14),
        0x18 => k(0x16),
        0x19 => k(0x2F),
        0x1A => k(0x11),
        0x1B => k(0x2D),
        0x1C => k(0x15),
        0x1D => k(0x2C),
        // Digits 1–9, 0.
        0x1E..=0x26 => k((usage - 0x1E) as u8 + 0x02),
        0x27 => k(0x0B),
        0x28 => k(0x1C), // Enter
        0x29 => k(0x01), // Escape
        0x2A => k(0x0E), // Backspace
        0x2B => k(0x0F), // Tab
        0x2C => k(0x39), // Space
        0x2D => k(0x0C), // - _
        0x2E => k(0x0D), // = +
        0x2F => k(0x1A), // [ {
        0x30 => k(0x1B), // ] }
        0x31 => k(0x2B), // \ |
        0x32 => k(0x2B), // non-US # ~ (same position as \ on ISO boards)
        0x33 => k(0x27), // ; :
        0x34 => k(0x28), // ' "
        0x35 => k(0x29), // ` ~
        0x36 => k(0x33), // , <
        0x37 => k(0x34), // . >
        0x38 => k(0x35), // / ?
        0x39 => k(0x3A), // Caps Lock
        // F1–F10, F11, F12.
        0x3A..=0x43 => k((usage - 0x3A) as u8 + 0x3B),
        0x44 => k(0x57),
        0x45 => k(0x58),
        0x46 => e(0x37), // Print Screen
        0x47 => k(0x46), // Scroll Lock
        0x48 => vk(VK_PAUSE),
        0x49 => e(0x52), // Insert
        0x4A => e(0x47), // Home
        0x4B => e(0x49), // Page Up
        0x4C => e(0x53), // Delete
        0x4D => e(0x4F), // End
        0x4E => e(0x51), // Page Down
        0x4F => e(0x4D), // Right
        0x50 => e(0x4B), // Left
        0x51 => e(0x50), // Down
        0x52 => e(0x48), // Up
        0x53 => k(0x45), // Num Lock
        0x54 => e(0x35), // Keypad /
        0x55 => k(0x37), // Keypad *
        0x56 => k(0x4A), // Keypad -
        0x57 => k(0x4E), // Keypad +
        0x58 => e(0x1C), // Keypad Enter
        0x59 => k(0x4F), // Keypad 1
        0x5A => k(0x50),
        0x5B => k(0x51),
        0x5C => k(0x4B),
        0x5D => k(0x4C),
        0x5E => k(0x4D),
        0x5F => k(0x47),
        0x60 => k(0x48),
        0x61 => k(0x49), // Keypad 9
        0x62 => k(0x52), // Keypad 0
        0x63 => k(0x53), // Keypad .
        0x64 => k(0x56), // non-US \ | (the extra ISO key)
        0x65 => e(0x5D), // Application (context menu)
        0x66 => e(0x5E), // Power
        0x67 => k(0x59), // Keypad =
        // F13–F23, F24.
        0x68..=0x72 => k((usage - 0x68) as u8 + 0x64),
        0x73 => k(0x76),
        0x85 => k(0x7E),       // Keypad , (Brazilian)
        0x87 => k(0x73),       // International1 (Ro)
        0x88 => k(0x70),       // International2 (Katakana/Hiragana)
        0x89 => k(0x7D),       // International3 (Yen)
        0x8A => k(0x79),       // International4 (Henkan)
        0x8B => k(0x7B),       // International5 (Muhenkan)
        0x90 => vk(VK_HANGUL), // Lang1 (Hangul/English)
        0x91 => vk(VK_HANJA),  // Lang2 (Hanja)
        0xE0 => k(0x1D),       // Left Control
        0xE1 => k(0x2A),       // Left Shift
        0xE2 => k(0x38),       // Left Alt
        0xE3 => e(0x5B),       // Left GUI (Windows)
        0xE4 => e(0x1D),       // Right Control
        0xE5 => k(0x36),       // Right Shift
        0xE6 => e(0x38),       // Right Alt (AltGr)
        0xE7 => e(0x5C),       // Right GUI
        _ => None,
    }
}

/// The key a PC keyboard event stands for: its scan code, `E0` flag and the
/// virtual key Windows assigned. `None` for keys without a HID usage here
/// and for the "fake Shift" codes keyboards send around some extended keys.
pub fn from_pc(scan: u16, extended: bool, vk: u16) -> Option<KeyCode> {
    // Keys known by their virtual key (their scan codes are ambiguous).
    match vk {
        VK_PAUSE => return Some(KeyCode(0x48)),
        VK_NUMLOCK => return Some(KeyCode::NUM_LOCK),
        VK_HANGUL => return Some(KeyCode(0x90)),
        VK_HANJA => return Some(KeyCode(0x91)),
        _ => {}
    }
    if let Some(&(usage, _)) = CONSUMER_VKS.iter().find(|(_, v)| *v == vk) {
        return Some(KeyCode::consumer(usage));
    }
    let code = u8::try_from(scan).ok()?;
    match (code, extended) {
        // Fake Shift around extended keys (E0 2A): not a key press.
        (0x2A, true) => return None,
        // Right Shift is sometimes flagged extended; it has one code either way.
        (0x36, _) => return Some(KeyCode(0xE5)),
        _ => {}
    }
    let wanted = PcKey::Scan { code, extended };
    (0..=0xFFu16)
        .find(|&usage| keyboard_page(usage) == Some(wanted))
        .map(KeyCode::keyboard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pc(usage: u16) -> Option<PcKey> {
        to_pc(KeyCode::keyboard(usage))
    }

    #[test]
    fn well_known_keys() {
        let key = |code, extended| Some(PcKey::Scan { code, extended });
        assert_eq!(pc(0x04), key(0x1E, false)); // A
        assert_eq!(pc(0x1D), key(0x2C, false)); // Z
        assert_eq!(pc(0x1E), key(0x02, false)); // 1
        assert_eq!(pc(0x26), key(0x0A, false)); // 9
        assert_eq!(pc(0x27), key(0x0B, false)); // 0
        assert_eq!(pc(0x3A), key(0x3B, false)); // F1
        assert_eq!(pc(0x43), key(0x44, false)); // F10
        assert_eq!(pc(0x45), key(0x58, false)); // F12
        assert_eq!(pc(0x68), key(0x64, false)); // F13
        assert_eq!(pc(0x72), key(0x6E, false)); // F23
        assert_eq!(pc(0x73), key(0x76, false)); // F24
        assert_eq!(pc(0x52), key(0x48, true)); // Up
        assert_eq!(pc(0x60), key(0x48, false)); // Keypad 8 shares the code, not extended
        assert_eq!(pc(0xE6), key(0x38, true)); // AltGr
        assert_eq!(pc(0x48), Some(PcKey::Vk(VK_PAUSE)));
        assert_eq!(pc(0x90), Some(PcKey::Vk(VK_HANGUL)));
        assert_eq!(pc(0x00), None);
        assert_eq!(pc(0xFF), None);
        assert_eq!(to_pc(KeyCode::consumer(0xE9)), Some(PcKey::Vk(0xAF)));
        assert_eq!(to_pc(KeyCode::consumer(0x01)), None);
        assert_eq!(
            to_pc(KeyCode(0x0001_0082)),
            None,
            "other pages are not mapped"
        );
    }

    #[test]
    fn codes_are_unique_except_the_two_backslash_positions() {
        let mut seen: HashMap<PcKey, u16> = HashMap::new();
        for usage in 0..=0xFFu16 {
            if let Some(code) = pc(usage)
                && let Some(prev) = seen.insert(code, usage)
            {
                assert_eq!((prev, usage), (0x31, 0x32), "{usage:#x} clashes");
            }
        }
        assert!(seen.len() > 110, "{} keys mapped", seen.len());
        let mut vks: Vec<u16> = CONSUMER_VKS.iter().map(|&(_, v)| v).collect();
        let n = vks.len();
        vks.sort();
        vks.dedup();
        assert_eq!(vks.len(), n);
    }

    #[test]
    fn reading_a_pc_keyboard_inverts_the_table() {
        for usage in 0..=0xFFu16 {
            let Some(key) = pc(usage) else { continue };
            let back = match key {
                PcKey::Scan { code, extended } => from_pc(code as u16, extended, 0),
                PcKey::Vk(v) => from_pc(0, false, v),
            };
            let expected = if usage == 0x32 { 0x31 } else { usage };
            assert_eq!(back, Some(KeyCode::keyboard(expected)), "{usage:#x}");
        }
        for &(usage, v) in CONSUMER_VKS {
            assert_eq!(from_pc(0, false, v), Some(KeyCode::consumer(usage)));
        }
        // Num Lock arrives as 0x45, flagged extended or not; its virtual key decides.
        assert_eq!(from_pc(0x45, true, VK_NUMLOCK), Some(KeyCode::NUM_LOCK));
        assert_eq!(from_pc(0x2A, true, 0xA0), None, "fake shift");
        assert_eq!(from_pc(0x36, true, 0xA1), Some(KeyCode(0xE5)));
        assert_eq!(from_pc(0x2A, false, 0xA0), Some(KeyCode::LEFT_SHIFT));
        assert_eq!(from_pc(0x1FF, false, 0), None);
    }
}
