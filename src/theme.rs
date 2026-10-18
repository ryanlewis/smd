//! Whether the terminal has a dark or a light background, so images drawn for
//! it can match. The terminal is asked once at start-up; see [`crate::kitty`].

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Theme {
    Dark,
    Light,
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Set the theme for the rest of the run. Only the first call counts.
pub fn set(theme: Theme) {
    let _ = THEME.set(theme);
}

/// The theme set at start-up, or dark if none was.
pub fn current() -> Theme {
    THEME.get().copied().unwrap_or(Theme::Dark)
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

/// Find the reply to `OSC 11 ; ?`, `OSC 11 ; rgb:R/G/B` ended by BEL or ST,
/// in what the terminal sent back, and say whether that colour is light.
/// Each part is one to four hex digits.
pub fn parse_background_reply(buf: &[u8]) -> Option<Theme> {
    let text = String::from_utf8_lossy(buf);
    for reply in text.split("\x1b]11;").skip(1) {
        let Some(end) = reply.find(['\x07', '\x1b']) else {
            continue;
        };
        let Some(rgb) = reply[..end].strip_prefix("rgb:") else {
            continue;
        };
        let parts: Vec<f64> = rgb.split('/').filter_map(fraction).collect();
        let [r, g, b] = parts[..] else {
            continue;
        };
        // Relative luminance, near enough for telling dark from light.
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        return Some(if luma > 0.5 {
            Theme::Light
        } else {
            Theme::Dark
        });
    }
    None
}

/// A hex colour part as a fraction of its largest value.
fn fraction(hex: &str) -> Option<f64> {
    if hex.is_empty() || hex.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(f64::from(value) / f64::from((1u32 << (4 * hex.len())) - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_reply_is_read_with_either_ending() {
        let dark = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[6;34;17t\x1b[0n";
        assert_eq!(parse_background_reply(dark), Some(Theme::Dark));
        let light = b"x\x1b]11;rgb:ffff/fafa/f0f0\x07\x1b[0n";
        assert_eq!(parse_background_reply(light), Some(Theme::Light));
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:f/f/f\x07"),
            Some(Theme::Light)
        );
    }

    #[test]
    fn missing_or_broken_background_reply_is_none() {
        assert_eq!(parse_background_reply(b"\x1b[0n"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:ff/ff\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:zz/00/00\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:ffff/ffff/ffff"), None);
    }
}
