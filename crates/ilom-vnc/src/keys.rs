//! VNC key events to USB HID keyboard reports.
//!
//! Plain RFB key events carry X11 keysyms: the character the client's own
//! layout produced. Characters are mapped back to keys through the **host**
//! layout, so the host types what the client typed whatever both layouts are.
//! QEMU extended key events add the XT scancode of the physical key, which
//! maps to a USB usage directly, like the egui viewer does.

use std::collections::BTreeSet;

use ilom_kvm_core::{
    keymap::{ALTGR, Layout, SHIFT, Stroke},
    session::ViewerCommand,
};

const LEFT_CTRL: u8 = 0x01;
const RIGHT_SHIFT: u8 = 0x20;
const SHIFTS: u8 = SHIFT | RIGHT_SHIFT;

/// What one key event means for the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// Modifier bit of the keyboard report.
    Modifier(u8),
    /// Physical key; the modifiers held on the client apply.
    Usage(u8),
    /// Character typed through the host layout. Shift and AltGr come from
    /// the strokes, not from the client.
    Char(Vec<Stroke>),
}

/// Maps an X11 keysym through the host `layout`.
pub fn from_keysym(keysym: u32, layout: Layout) -> Option<Key> {
    if let Some(key) = special_keysym(keysym) {
        return Some(key);
    }
    if let Some(accent) = dead_keysym(keysym) {
        return layout
            .dead_key(accent)
            .map(|stroke| Key::Char(vec![stroke]));
    }
    let ch = match keysym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(keysym)?,
        0x20ac => '€',
        0x0100_0000..=0x0110_ffff => char::from_u32(keysym - 0x0100_0000)?,
        _ => return None,
    };
    layout.strokes(ch).map(Key::Char)
}

fn dead_keysym(keysym: u32) -> Option<char> {
    Some(match keysym {
        0xfe50 => '`',
        0xfe51 => '´',
        0xfe52 => '^',
        0xfe53 => '~',
        0xfe57 => '¨',
        _ => return None,
    })
}

fn special_keysym(keysym: u32) -> Option<Key> {
    let modifier = match keysym {
        0xffe1 => Some(0x02),                   // Shift_L
        0xffe2 => Some(0x20),                   // Shift_R
        0xffe3 => Some(0x01),                   // Control_L
        0xffe4 => Some(0x10),                   // Control_R
        0xffe7 | 0xffeb => Some(0x08),          // Meta_L, Super_L
        0xffe8 | 0xffec => Some(0x80),          // Meta_R, Super_R
        0xffe9 => Some(0x04),                   // Alt_L
        0xffea | 0xfe03 | 0xff7e => Some(0x40), // Alt_R, ISO_Level3_Shift, Mode_switch
        _ => None,
    };
    if let Some(bit) = modifier {
        return Some(Key::Modifier(bit));
    }
    let usage = match keysym {
        0xff08 => 0x2a,          // BackSpace
        0xff09 | 0xfe20 => 0x2b, // Tab, ISO_Left_Tab
        0xff0d => 0x28,          // Return
        0xff13 => 0x48,          // Pause
        0xff14 => 0x47,          // Scroll_Lock
        0xff15 | 0xff61 => 0x46, // Sys_Req, Print
        0xff1b => 0x29,          // Escape
        0xffff => 0x4c,          // Delete
        0xff50 => 0x4a,          // Home
        0xff51 => 0x50,          // Left
        0xff52 => 0x52,          // Up
        0xff53 => 0x4f,          // Right
        0xff54 => 0x51,          // Down
        0xff55 => 0x4b,          // Prior
        0xff56 => 0x4e,          // Next
        0xff57 => 0x4d,          // End
        0xff63 => 0x49,          // Insert
        0xff67 => 0x65,          // Menu
        0xff7f => 0x53,          // Num_Lock
        0xffe5 => 0x39,          // Caps_Lock
        0xff8d => 0x58,          // KP_Enter
        // Keypad keys by position, with NumLock on (digits) or off.
        0xffb0 | 0xff9e => 0x62, // KP_0, KP_Insert
        0xffb1 | 0xff9c => 0x59, // KP_1, KP_End
        0xffb2 | 0xff99 => 0x5a, // KP_2, KP_Down
        0xffb3 | 0xff9b => 0x5b, // KP_3, KP_Next
        0xffb4 | 0xff96 => 0x5c, // KP_4, KP_Left
        0xffb5 | 0xff9d => 0x5d, // KP_5, KP_Begin
        0xffb6 | 0xff98 => 0x5e, // KP_6, KP_Right
        0xffb7 | 0xff95 => 0x5f, // KP_7, KP_Home
        0xffb8 | 0xff97 => 0x60, // KP_8, KP_Up
        0xffb9 | 0xff9a => 0x61, // KP_9, KP_Prior
        0xffae | 0xff9f => 0x63, // KP_Decimal, KP_Delete
        0xffac => 0x85,          // KP_Separator
        0xffaa => 0x55,          // KP_Multiply
        0xffab => 0x57,          // KP_Add
        0xffad => 0x56,          // KP_Subtract
        0xffaf => 0x54,          // KP_Divide
        0xffbd => 0x67,          // KP_Equal
        // F1–F12, then F13–F24.
        0xffbe..=0xffc9 => 0x3a + (keysym - 0xffbe) as u8,
        0xffca..=0xffd5 => 0x68 + (keysym - 0xffca) as u8,
        _ => return None,
    };
    Some(Key::Usage(usage))
}

/// Maps the XT scancode of a QEMU extended key event. Keys with an `E0`
/// prefix arrive with the high bit set (`E0 48`, Up, is `0xc8`).
pub fn from_xt(code: u32) -> Option<Key> {
    let modifier = match code {
        0x1d => Some(0x01), // Left Ctrl
        0x2a => Some(0x02), // Left Shift
        0x38 => Some(0x04), // Left Alt
        0xdb => Some(0x08), // Left GUI
        0x9d => Some(0x10), // Right Ctrl
        0x36 => Some(0x20), // Right Shift
        0xb8 => Some(0x40), // Right Alt (AltGr)
        0xdc => Some(0x80), // Right GUI
        _ => None,
    };
    if let Some(bit) = modifier {
        return Some(Key::Modifier(bit));
    }
    const LETTERS: &[u8; 26] = b"\x1e\x30\x2e\x20\x12\x21\x22\x23\x17\x24\x25\x26\x32\x31\x18\x19\x10\x13\x1f\x14\x16\x2f\x11\x2d\x15\x2c";
    if let Some(index) = LETTERS.iter().position(|&xt| u32::from(xt) == code) {
        return Some(Key::Usage(0x04 + index as u8));
    }
    let usage = match code {
        0x02..=0x0b => 0x1e + (code - 0x02) as u8, // 1–9, 0
        0x01 => 0x29,                              // Escape
        0x0c => 0x2d,                              // -
        0x0d => 0x2e,                              // =
        0x0e => 0x2a,                              // Backspace
        0x0f => 0x2b,                              // Tab
        0x1a => 0x2f,                              // [
        0x1b => 0x30,                              // ]
        0x1c => 0x28,                              // Enter
        0x27 => 0x33,                              // ;
        0x28 => 0x34,                              // '
        0x29 => 0x35,                              // `
        0x2b => 0x31,                              // \ (ISO #)
        0x33 => 0x36,                              // ,
        0x34 => 0x37,                              // .
        0x35 => 0x38,                              // /
        0x37 => 0x55,                              // Keypad *
        0x39 => 0x2c,                              // Space
        0x3a => 0x39,                              // CapsLock
        0x3b..=0x44 => 0x3a + (code - 0x3b) as u8, // F1–F10
        0x45 => 0x53,                              // NumLock
        0x46 => 0x47,                              // ScrollLock
        0x47 => 0x5f,                              // Keypad 7
        0x48 => 0x60,                              // Keypad 8
        0x49 => 0x61,                              // Keypad 9
        0x4a => 0x56,                              // Keypad -
        0x4b => 0x5c,                              // Keypad 4
        0x4c => 0x5d,                              // Keypad 5
        0x4d => 0x5e,                              // Keypad 6
        0x4e => 0x57,                              // Keypad +
        0x4f => 0x59,                              // Keypad 1
        0x50 => 0x5a,                              // Keypad 2
        0x51 => 0x5b,                              // Keypad 3
        0x52 => 0x62,                              // Keypad 0
        0x53 => 0x63,                              // Keypad .
        0x54 => 0x46,                              // SysRq
        0x56 => 0x64,                              // ISO key left of Z
        0x57 => 0x44,                              // F11
        0x58 => 0x45,                              // F12
        0x59 => 0x67,                              // Keypad =
        0x64..=0x6e => 0x68 + (code - 0x64) as u8, // F13–F23
        0x70 => 0x88,                              // Katakana/Hiragana
        0x73 => 0x87,                              // Ro
        0x76 => 0x73,                              // F24
        0x79 => 0x8a,                              // Henkan
        0x7b => 0x8b,                              // Muhenkan
        0x7d => 0x89,                              // Yen
        0x7e => 0x85,                              // Keypad ,
        0x9c => 0x58,                              // Keypad Enter
        0xb5 => 0x54,                              // Keypad /
        0xb7 => 0x46,                              // PrintScreen
        0xc6 => 0x48,                              // Pause
        0xc7 => 0x4a,                              // Home
        0xc8 => 0x52,                              // Up
        0xc9 => 0x4b,                              // PageUp
        0xcb => 0x50,                              // Left
        0xcd => 0x4f,                              // Right
        0xcf => 0x4d,                              // End
        0xd0 => 0x51,                              // Down
        0xd1 => 0x4e,                              // PageDown
        0xd2 => 0x49,                              // Insert
        0xd3 => 0x4c,                              // Delete
        0xdd => 0x65,                              // Menu
        0xde => 0x66,                              // Power
        _ => return None,
    };
    Some(Key::Usage(usage))
}

/// Keyboard state of one VNC client, turned into full HID reports.
pub struct Keyboard {
    layout: Layout,
    /// Modifiers held on the client.
    modifiers: u8,
    pressed: BTreeSet<u8>,
    /// Shift/AltGr forced by the character key pressed last: (usage,
    /// modifier bits cleared, modifier bits set).
    forced: Option<(u8, u8, u8)>,
}

impl Keyboard {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            modifiers: 0,
            pressed: BTreeSet::new(),
            forced: None,
        }
    }

    /// Plain RFB key event.
    pub fn keysym(&mut self, keysym: u32, down: bool) -> Option<ViewerCommand> {
        let key = from_keysym(keysym, self.layout)?;
        self.apply(key, down)
    }

    /// QEMU extended key event; falls back to the keysym for unknown codes.
    pub fn extended(&mut self, keysym: u32, code: u32, down: bool) -> Option<ViewerCommand> {
        match from_xt(code) {
            Some(key) => self.apply(key, down),
            None => self.keysym(keysym, down),
        }
    }

    /// Releases every key, e.g. when the client disconnects.
    pub fn release_all(&mut self) -> Option<ViewerCommand> {
        let held = self.modifiers != 0 || !self.pressed.is_empty();
        self.modifiers = 0;
        self.pressed.clear();
        self.forced = None;
        held.then(|| self.report())
    }

    fn apply(&mut self, key: Key, down: bool) -> Option<ViewerCommand> {
        match key {
            Key::Modifier(bit) => {
                let before = self.modifiers;
                if down {
                    self.modifiers |= bit;
                } else {
                    self.modifiers &= !bit;
                }
                (self.modifiers != before).then(|| self.report())
            }
            Key::Usage(usage) => self.press(usage, down, None),
            Key::Char(strokes) => match strokes.as_slice() {
                &[(modifiers, usage)] => {
                    let mut clear = SHIFTS | ALTGR;
                    // Windows clients send AltGr as Left Ctrl + Right Alt.
                    if modifiers & ALTGR != 0 {
                        clear |= LEFT_CTRL;
                    }
                    self.press(usage, down, Some((clear, modifiers)))
                }
                // Dead-key sequences are typed in one go on press.
                _ => down.then_some(ViewerCommand::TypeStrokes(strokes)),
            },
        }
    }

    fn press(&mut self, usage: u8, down: bool, force: Option<(u8, u8)>) -> Option<ViewerCommand> {
        if down {
            let new = self.pressed.insert(usage);
            let forced = force.map(|(clear, set)| (usage, clear, set));
            let changed = new || forced != self.forced;
            self.forced = forced;
            changed.then(|| self.report())
        } else {
            if self.forced.is_some_and(|(held, ..)| held == usage) {
                self.forced = None;
            }
            self.pressed.remove(&usage).then(|| self.report())
        }
    }

    fn report(&self) -> ViewerCommand {
        let modifiers = match self.forced {
            Some((_, clear, set)) => (self.modifiers & !clear) | set,
            None => self.modifiers,
        };
        ViewerCommand::Keyboard {
            modifiers,
            usages: self.pressed.iter().copied().take(6).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(command: Option<ViewerCommand>) -> (u8, Vec<u8>) {
        match command {
            Some(ViewerCommand::Keyboard { modifiers, usages }) => (modifiers, usages),
            other => panic!("expected a keyboard report, got {other:?}"),
        }
    }

    #[test]
    fn xt_codes_map_to_usb_usages() {
        assert_eq!(from_xt(0x1e), Some(Key::Usage(0x04))); // a
        assert_eq!(from_xt(0x2c), Some(Key::Usage(0x1d))); // z
        assert_eq!(from_xt(0x10), Some(Key::Usage(0x14))); // q
        assert_eq!(from_xt(0x0b), Some(Key::Usage(0x27))); // 0
        assert_eq!(from_xt(0xc8), Some(Key::Usage(0x52))); // Up
        assert_eq!(from_xt(0x56), Some(Key::Usage(0x64))); // ISO <>
        assert_eq!(from_xt(0xb8), Some(Key::Modifier(0x40))); // AltGr
    }

    #[test]
    fn keysyms_go_through_the_host_layout() {
        // "a" is on the Q key of an AZERTY host.
        assert_eq!(
            from_keysym(0x61, Layout::French),
            Some(Key::Char(vec![(0, 0x14)]))
        );
        assert_eq!(
            from_keysym(0x61, Layout::Us),
            Some(Key::Char(vec![(0, 0x04)]))
        );
        assert_eq!(from_keysym(0xff0d, Layout::Us), Some(Key::Usage(0x28)));
        assert_eq!(
            from_keysym(0xfe52, Layout::French),
            Some(Key::Char(vec![(0, 0x2f)]))
        );
        assert_eq!(
            from_keysym(0x0100_20ac, Layout::French),
            Some(Key::Char(vec![(ALTGR, 0x08)]))
        );
    }

    #[test]
    fn characters_force_shift_but_keep_ctrl() {
        let mut keyboard = Keyboard::new(Layout::Us);
        keyboard.keysym(0xffe3, true); // Control_L
        assert_eq!(report(keyboard.keysym(0x63, true)), (0x01, vec![0x06]));
        assert_eq!(report(keyboard.keysym(0x63, false)), (0x01, vec![]));
        // "1" needs Shift on an AZERTY host, whatever the client layout.
        let mut keyboard = Keyboard::new(Layout::French);
        assert_eq!(report(keyboard.keysym(0x31, true)), (SHIFT, vec![0x1e]));
        assert_eq!(report(keyboard.keysym(0x31, false)), (0, vec![]));
    }

    #[test]
    fn shift_held_on_the_client_is_dropped_for_unshifted_characters() {
        // US client: Shift+1 gives "!", which is unshifted on AZERTY.
        let mut keyboard = Keyboard::new(Layout::French);
        keyboard.keysym(0xffe1, true);
        assert_eq!(report(keyboard.keysym(0x21, true)), (0, vec![0x38]));
        assert_eq!(report(keyboard.keysym(0x21, false)), (SHIFT, vec![]));
    }

    #[test]
    fn extended_events_use_physical_keys_and_client_modifiers() {
        let mut keyboard = Keyboard::new(Layout::French);
        keyboard.extended(0xffe1, 0x2a, true); // Shift
        assert_eq!(
            report(keyboard.extended(0x21, 0x02, true)),
            (SHIFT, vec![0x1e])
        );
        assert_eq!(report(keyboard.release_all()), (0, vec![]));
        assert!(keyboard.release_all().is_none());
    }

    #[test]
    fn composed_characters_are_typed_on_press_only() {
        let mut keyboard = Keyboard::new(Layout::French);
        assert!(matches!(
            keyboard.keysym(0xea, true), // ê
            Some(ViewerCommand::TypeStrokes(strokes)) if strokes.len() == 2
        ));
        assert!(keyboard.keysym(0xea, false).is_none());
    }
}
