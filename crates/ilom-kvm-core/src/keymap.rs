//! Text to USB keystrokes, for typing clipboard contents into the host.
//!
//! USB usages name physical key positions, so the characters they produce
//! depend on the keyboard layout configured on the host. The layout chosen
//! here must match the host, not this machine.

pub const SHIFT: u8 = 0x02;
/// Right Alt, i.e. AltGr on ISO layouts.
pub const ALTGR: u8 = 0x40;

/// One key press: modifier byte and usage.
pub type Stroke = (u8, u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Us,
    French,
}

impl Layout {
    pub const ALL: [Layout; 2] = [Layout::Us, Layout::French];

    pub fn label(self) -> &'static str {
        match self {
            Self::Us => "US (QWERTY)",
            Self::French => "French (AZERTY)",
        }
    }

    /// Stable id for the settings file.
    pub fn id(self) -> &'static str {
        match self {
            Self::Us => "us",
            Self::French => "fr",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|layout| layout.id() == id)
    }

    /// Guess from the local locale; the host usually matches it.
    pub fn from_locale() -> Self {
        let lang = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default();
        if lang.starts_with("fr") {
            Self::French
        } else {
            Self::Us
        }
    }

    /// Keystrokes that type `ch`, or `None` if the layout cannot produce it.
    pub fn strokes(self, ch: char) -> Option<Vec<Stroke>> {
        // Keys that are the same on both layouts.
        let common = match ch {
            '\n' => Some((0, 0x28)),
            '\t' => Some((0, 0x2b)),
            ' ' => Some((0, 0x2c)),
            _ => None,
        };
        if let Some(stroke) = common {
            return Some(vec![stroke]);
        }
        match self {
            Self::Us => us(ch).map(|stroke| vec![stroke]),
            Self::French => french(ch),
        }
    }

    /// Key that starts `accent` as a dead key (`^`, `¨`, `` ` ``, `~`, `´`),
    /// or else the key that types it, if any. Front ends that receive dead
    /// keys as separate events use it to press the same key on the host.
    pub fn dead_key(self, accent: char) -> Option<Stroke> {
        match (self, accent) {
            (Self::French, '^') => Some((0, FR_CIRCUMFLEX_KEY)),
            (Self::French, '¨') => Some((SHIFT, FR_CIRCUMFLEX_KEY)),
            _ => self.strokes(accent)?.first().copied(),
        }
    }
}

fn letter_usage(ch: char) -> u8 {
    0x04 + (ch.to_ascii_lowercase() as u8 - b'a')
}

fn us(ch: char) -> Option<Stroke> {
    if ch.is_ascii_lowercase() {
        return Some((0, letter_usage(ch)));
    }
    if ch.is_ascii_uppercase() {
        return Some((SHIFT, letter_usage(ch)));
    }
    Some(match ch {
        '1'..='9' => (0, 0x1e + (ch as u8 - b'1')),
        '0' => (0, 0x27),
        '!' => (SHIFT, 0x1e),
        '@' => (SHIFT, 0x1f),
        '#' => (SHIFT, 0x20),
        '$' => (SHIFT, 0x21),
        '%' => (SHIFT, 0x22),
        '^' => (SHIFT, 0x23),
        '&' => (SHIFT, 0x24),
        '*' => (SHIFT, 0x25),
        '(' => (SHIFT, 0x26),
        ')' => (SHIFT, 0x27),
        '-' => (0, 0x2d),
        '_' => (SHIFT, 0x2d),
        '=' => (0, 0x2e),
        '+' => (SHIFT, 0x2e),
        '[' => (0, 0x2f),
        '{' => (SHIFT, 0x2f),
        ']' => (0, 0x30),
        '}' => (SHIFT, 0x30),
        '\\' => (0, 0x31),
        '|' => (SHIFT, 0x31),
        ';' => (0, 0x33),
        ':' => (SHIFT, 0x33),
        '\'' => (0, 0x34),
        '"' => (SHIFT, 0x34),
        '`' => (0, 0x35),
        '~' => (SHIFT, 0x35),
        ',' => (0, 0x36),
        '<' => (SHIFT, 0x36),
        '.' => (0, 0x37),
        '>' => (SHIFT, 0x37),
        '/' => (0, 0x38),
        '?' => (SHIFT, 0x38),
        _ => return None,
    })
}

/// Physical usage of an AZERTY letter key.
fn azerty_letter(lower: char) -> u8 {
    match lower {
        'a' => 0x14, // US Q position
        'q' => 0x04, // US A position
        'z' => 0x1a, // US W position
        'w' => 0x1d, // US Z position
        'm' => 0x33, // US ; position
        other => letter_usage(other),
    }
}

const FR_CIRCUMFLEX_KEY: u8 = 0x2f; // dead ^ (Shift: dead ¨)

fn french(ch: char) -> Option<Vec<Stroke>> {
    if ch.is_ascii_lowercase() {
        return Some(vec![(0, azerty_letter(ch))]);
    }
    if ch.is_ascii_uppercase() {
        return Some(vec![(SHIFT, azerty_letter(ch.to_ascii_lowercase()))]);
    }
    // Accents typed with a dead key followed by the base letter.
    let dead = |shift: u8, base: char| {
        let base = french(base)?;
        let mut strokes = vec![(shift, FR_CIRCUMFLEX_KEY)];
        strokes.extend(base);
        Some(strokes)
    };
    let stroke = match ch {
        // Top row: symbols unshifted, digits with Shift.
        '&' => (0, 0x1e),
        'é' => (0, 0x1f),
        '"' => (0, 0x20),
        '\'' => (0, 0x21),
        '(' => (0, 0x22),
        '-' => (0, 0x23),
        'è' => (0, 0x24),
        '_' => (0, 0x25),
        'ç' => (0, 0x26),
        'à' => (0, 0x27),
        '1'..='9' => (SHIFT, 0x1e + (ch as u8 - b'1')),
        '0' => (SHIFT, 0x27),
        ')' => (0, 0x2d),
        '°' => (SHIFT, 0x2d),
        '=' => (0, 0x2e),
        '+' => (SHIFT, 0x2e),
        '$' => (0, 0x30),
        '£' => (SHIFT, 0x30),
        '*' => (0, 0x32),
        'µ' => (SHIFT, 0x32),
        'ù' => (0, 0x34),
        '%' => (SHIFT, 0x34),
        '²' => (0, 0x35),
        ',' => (0, 0x10),
        '?' => (SHIFT, 0x10),
        ';' => (0, 0x36),
        '.' => (SHIFT, 0x36),
        ':' => (0, 0x37),
        '/' => (SHIFT, 0x37),
        '!' => (0, 0x38),
        '§' => (SHIFT, 0x38),
        '<' => (0, 0x64),
        '>' => (SHIFT, 0x64),
        // AltGr layer.
        '~' => (ALTGR, 0x1f),
        '#' => (ALTGR, 0x20),
        '{' => (ALTGR, 0x21),
        '[' => (ALTGR, 0x22),
        '|' => (ALTGR, 0x23),
        '`' => (ALTGR, 0x24),
        '\\' => (ALTGR, 0x25),
        '@' => (ALTGR, 0x27),
        ']' => (ALTGR, 0x2d),
        '}' => (ALTGR, 0x2e),
        '€' => (ALTGR, 0x08),
        '^' => return Some(vec![(0, FR_CIRCUMFLEX_KEY), (0, 0x2c)]),
        'â' => return dead(0, 'a'),
        'ê' => return dead(0, 'e'),
        'î' => return dead(0, 'i'),
        'ô' => return dead(0, 'o'),
        'û' => return dead(0, 'u'),
        'ä' => return dead(SHIFT, 'a'),
        'ë' => return dead(SHIFT, 'e'),
        'ï' => return dead(SHIFT, 'i'),
        'ö' => return dead(SHIFT, 'o'),
        'ü' => return dead(SHIFT, 'u'),
        'ÿ' => return dead(SHIFT, 'y'),
        _ => return None,
    };
    Some(vec![stroke])
}

/// Converts text into keystrokes. CRLF becomes a single Enter. Returns the
/// strokes and the characters that could not be typed.
pub fn text_to_strokes(layout: Layout, text: &str) -> (Vec<Stroke>, Vec<char>) {
    let mut strokes = Vec::new();
    let mut skipped = Vec::new();
    for ch in text.replace("\r\n", "\n").chars() {
        let ch = if ch == '\r' { '\n' } else { ch };
        match layout.strokes(ch) {
            Some(keys) => strokes.extend(keys),
            None => skipped.push(ch),
        }
    }
    (strokes, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_layout_basics() {
        assert_eq!(Layout::Us.strokes('a'), Some(vec![(0, 0x04)]));
        assert_eq!(Layout::Us.strokes('A'), Some(vec![(SHIFT, 0x04)]));
        assert_eq!(Layout::Us.strokes('@'), Some(vec![(SHIFT, 0x1f)]));
        assert_eq!(Layout::Us.strokes('0'), Some(vec![(0, 0x27)]));
    }

    #[test]
    fn french_layout_moves_letters_and_digits() {
        assert_eq!(Layout::French.strokes('a'), Some(vec![(0, 0x14)]));
        assert_eq!(Layout::French.strokes('m'), Some(vec![(0, 0x33)]));
        assert_eq!(Layout::French.strokes('1'), Some(vec![(SHIFT, 0x1e)]));
        assert_eq!(Layout::French.strokes('&'), Some(vec![(0, 0x1e)]));
        assert_eq!(Layout::French.strokes('@'), Some(vec![(ALTGR, 0x27)]));
        assert_eq!(Layout::French.strokes('.'), Some(vec![(SHIFT, 0x36)]));
    }

    #[test]
    fn dead_key_presses_the_accent_key_alone() {
        assert_eq!(Layout::French.dead_key('^'), Some((0, FR_CIRCUMFLEX_KEY)));
        assert_eq!(
            Layout::French.dead_key('¨'),
            Some((SHIFT, FR_CIRCUMFLEX_KEY))
        );
        assert_eq!(Layout::Us.dead_key('~'), Some((SHIFT, 0x35)));
        assert_eq!(Layout::Us.dead_key('¨'), None);
    }

    #[test]
    fn french_dead_keys() {
        assert_eq!(
            Layout::French.strokes('ê'),
            Some(vec![(0, FR_CIRCUMFLEX_KEY), (0, 0x08)])
        );
        assert_eq!(
            Layout::French.strokes('ï'),
            Some(vec![(SHIFT, FR_CIRCUMFLEX_KEY), (0, 0x0c)])
        );
    }

    #[test]
    fn text_conversion_reports_unknown_chars() {
        let (strokes, skipped) = text_to_strokes(Layout::Us, "ab\r\nc☃");
        assert_eq!(strokes.len(), 4);
        assert_eq!(strokes[2], (0, 0x28));
        assert_eq!(skipped, vec!['☃']);
    }
}
