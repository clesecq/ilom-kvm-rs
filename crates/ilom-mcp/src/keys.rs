//! Key and modifier names to USB HID keyboard usages.

/// Modifier bit of the keyboard report for a name such as `ctrl` or `alt`.
pub fn modifier(name: &str) -> Option<u8> {
    Some(match normalize(name).as_str() {
        "ctrl" | "control" | "lctrl" | "leftctrl" => 0x01,
        "shift" | "lshift" | "leftshift" => 0x02,
        "alt" | "lalt" | "leftalt" => 0x04,
        "gui" | "super" | "meta" | "win" | "windows" | "cmd" | "lgui" => 0x08,
        "rctrl" | "rightctrl" => 0x10,
        "rshift" | "rightshift" => 0x20,
        "altgr" | "ralt" | "rightalt" => 0x40,
        "rgui" | "rsuper" | "rmeta" => 0x80,
        _ => return None,
    })
}

/// HID usage of a key name such as `a`, `7`, `enter`, `f5` or `pagedown`.
pub fn usage(name: &str) -> Option<u8> {
    let name = normalize(name);
    let mut chars = name.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        match ch {
            'a'..='z' => return Some(0x04 + (ch as u8 - b'a')),
            '1'..='9' => return Some(0x1e + (ch as u8 - b'1')),
            '0' => return Some(0x27),
            _ => {}
        }
    }
    if let Some(number) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return match number {
            1..=12 => Some(0x3a + number - 1),
            13..=24 => Some(0x68 + number - 13),
            _ => None,
        };
    }
    Some(match name.as_str() {
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" => 0x2a,
        "tab" => 0x2b,
        "space" | "spacebar" => 0x2c,
        "minus" | "dash" => 0x2d,
        "equal" | "equals" => 0x2e,
        "leftbracket" | "lbracket" => 0x2f,
        "rightbracket" | "rbracket" => 0x30,
        "backslash" => 0x31,
        "semicolon" => 0x33,
        "quote" | "apostrophe" => 0x34,
        "grave" | "backtick" => 0x35,
        "comma" => 0x36,
        "period" | "dot" => 0x37,
        "slash" => 0x38,
        "capslock" => 0x39,
        "printscreen" | "prtsc" | "sysrq" => 0x46,
        "scrolllock" => 0x47,
        "pause" | "break" => 0x48,
        "insert" | "ins" => 0x49,
        "home" => 0x4a,
        "pageup" | "pgup" => 0x4b,
        "delete" | "del" => 0x4c,
        "end" => 0x4d,
        "pagedown" | "pgdn" => 0x4e,
        "right" | "rightarrow" => 0x4f,
        "left" | "leftarrow" => 0x50,
        "down" | "downarrow" => 0x51,
        "up" | "uparrow" => 0x52,
        "numlock" => 0x53,
        "menu" | "application" => 0x65,
        _ => return None,
    })
}

/// Case, spaces, dashes and underscores do not matter: `Page_Down`,
/// `page down` and `pagedown` are the same key.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|ch| !matches!(ch, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_function_keys() {
        assert_eq!(usage("a"), Some(0x04));
        assert_eq!(usage("Z"), Some(0x1d));
        assert_eq!(usage("1"), Some(0x1e));
        assert_eq!(usage("0"), Some(0x27));
        assert_eq!(usage("F1"), Some(0x3a));
        assert_eq!(usage("f12"), Some(0x45));
        assert_eq!(usage("F13"), Some(0x68));
        assert_eq!(usage("f25"), None);
    }

    #[test]
    fn named_keys_ignore_case_and_separators() {
        assert_eq!(usage("Enter"), Some(0x28));
        assert_eq!(usage("Page_Down"), Some(0x4e));
        assert_eq!(usage("page down"), Some(0x4e));
        assert_eq!(usage("Delete"), Some(0x4c));
        assert_eq!(usage("esc"), Some(0x29));
        assert_eq!(usage("nonsense"), None);
        assert_eq!(usage(""), None);
    }

    #[test]
    fn modifiers() {
        assert_eq!(modifier("Ctrl"), Some(0x01));
        assert_eq!(modifier("alt"), Some(0x04));
        assert_eq!(modifier("super"), Some(0x08));
        assert_eq!(modifier("AltGr"), Some(0x40));
        assert_eq!(modifier("x"), None);
    }
}
