//! Global shortcuts as text ("Shift+F3", "Ctrl+Alt+C") <-> RegisterHotKey's
//! modifier flags + virtual key. The text is what clipper.ini stores and the
//! window shows.

/// RegisterHotKey modifier flags (MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN).
pub const ALT: u32 = 0x1;
pub const CTRL: u32 = 0x2;
pub const SHIFT: u32 = 0x4;
pub const WIN: u32 = 0x8;

pub const DEFAULT_SAVE: &str = "Shift+F3";
pub const DEFAULT_WINDOW: &str = "Ctrl+Alt+C";

const NAMED: &[(&str, u32)] = &[
    ("Space", 0x20),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("End", 0x23),
    ("Home", 0x24),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("PrintScreen", 0x2C),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
    ("Tab", 0x09),
    (";", 0xBA),
    ("=", 0xBB),
    (",", 0xBC),
    ("-", 0xBD),
    (".", 0xBE),
    ("/", 0xBF),
    ("`", 0xC0),
    ("[", 0xDB),
    ("\\", 0xDC),
    ("]", 0xDD),
    ("'", 0xDE),
];

/// Name of a key that can be a shortcut's main key (not a modifier).
pub fn key_name(vk: u32) -> Option<String> {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => Some(char::from(vk as u8).to_string()),
        0x60..=0x69 => Some(format!("Num{}", vk - 0x60)),
        0x70..=0x87 => Some(format!("F{}", vk - 0x6F)),
        _ => NAMED.iter().find(|(_, v)| *v == vk).map(|(n, _)| n.to_string()),
    }
}

fn key_code(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    if up.len() == 1 {
        let c = up.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Some(n) = up.strip_prefix("NUM").and_then(|n| n.parse::<u32>().ok()).filter(|n| *n <= 9) {
        return Some(0x60 + n);
    }
    if let Some(n) = up.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)) {
        return Some(0x6F + n);
    }
    NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
}

/// A bare key is only allowed when typing can't hit it by accident.
pub fn bare_ok(vk: u32) -> bool {
    matches!(vk, 0x70..=0x87 | 0x13 | 0x91 | 0x2C)
}

/// "Ctrl+Alt+C" -> (CTRL | ALT, 'C'). None for "off" or anything unreadable.
pub fn parse(s: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    // "Ctrl++" style isn't supported; '=' covers the plus key
    let (key, mods) = parts.split_last()?;
    let mut m = 0;
    for p in mods {
        m |= match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => CTRL,
            "alt" => ALT,
            "shift" => SHIFT,
            "win" => WIN,
            _ => return None,
        };
    }
    let vk = key_code(key)?;
    (m != 0 || bare_ok(vk)).then_some((m, vk))
}

/// (CTRL | ALT, 'C') -> "Ctrl+Alt+C".
pub fn format(mods: u32, vk: u32) -> Option<String> {
    let mut s = String::new();
    for (bit, name) in [(CTRL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")] {
        if mods & bit != 0 {
            s.push_str(name);
            s.push('+');
        }
    }
    s.push_str(&key_name(vk)?);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for s in ["Shift+F3", "Ctrl+Alt+C", "Ctrl+Shift+9", "Alt+Num5", "F9", "Shift+Win+S", "Ctrl+-", "Pause"] {
            let (m, vk) = parse(s).unwrap();
            assert_eq!(format(m, vk).unwrap(), s);
        }
    }

    #[test]
    fn rejects() {
        assert_eq!(parse("off"), None);
        assert_eq!(parse("C"), None); // bare letter would eat typing
        assert_eq!(parse("Hyper+C"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("ctrl+alt+c"), Some((CTRL | ALT, 0x43)));
    }
}
