//! Whether the terminal has a dark or a light background, so images drawn for
//! it can match, and its colour, which code blocks are shaded from. The terminal is asked once at start-up; see [`crate::kitty`].

use std::sync::OnceLock;

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Theme {
    Dark,
    Light,
}

static THEME: OnceLock<Theme> = OnceLock::new();

static BACKGROUND: OnceLock<Rgb> = OnceLock::new();

/// A colour as red, green and blue, each 0 to 255.
pub type Rgb = (u8, u8, u8);

/// How far the code panel moves from the background towards white or black.
const PANEL_SHADE: f64 = 0.08;

/// Set the theme for the rest of the run. Only the first call counts.
pub fn set(theme: Theme) {
    let _ = THEME.set(theme);
}

/// The theme set at start-up, or dark if none was.
pub fn current() -> Theme {
    THEME.get().copied().unwrap_or(Theme::Dark)
}

/// Keep the background colour the terminal reported. Only the first call counts.
pub fn set_background(rgb: Rgb) {
    let _ = BACKGROUND.set(rgb);
}

/// The colour to draw code blocks on, if the terminal said what its
/// background is.
pub fn code_panel() -> Option<Color> {
    BACKGROUND.get().map(|&rgb| panel(rgb))
}

/// The background moved a little towards white when it is dark, or towards
/// black when it is light.
fn panel((r, g, b): Rgb) -> Color {
    let light = luma(fraction_of(r), fraction_of(g), fraction_of(b)) > 0.5;
    let shade = |c: u8| {
        let c = f64::from(c);
        let target = if light { 0.0 } else { 255.0 };
        (c + (target - c) * PANEL_SHADE).round() as u8
    };
    Color::Rgb(shade(r), shade(g), shade(b))
}

fn fraction_of(c: u8) -> f64 {
    f64::from(c) / 255.0
}

/// Relative luminance, near enough for telling dark from light.
fn luma(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
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
/// Each part is one to four hex digits. Some terminals answer `rgba:R/G/B/A`.
pub fn parse_background_reply(buf: &[u8]) -> Option<Theme> {
    let (r, g, b) = background_fractions(buf)?;
    Some(if luma(r, g, b) > 0.5 {
        Theme::Light
    } else {
        Theme::Dark
    })
}

/// The colour in the reply to `OSC 11 ; ?`, as [`parse_background_reply`]
/// finds it.
pub fn parse_background_colour(buf: &[u8]) -> Option<Rgb> {
    let (r, g, b) = background_fractions(buf)?;
    let byte = |c: f64| (c * 255.0).round() as u8;
    Some((byte(r), byte(g), byte(b)))
}

fn background_fractions(buf: &[u8]) -> Option<(f64, f64, f64)> {
    let text = String::from_utf8_lossy(buf);
    for reply in text.split("\x1b]11;").skip(1) {
        let Some(end) = reply.find(['\x07', '\x1b']) else {
            continue;
        };
        let reply = &reply[..end];
        let (rgb, count) = match reply.strip_prefix("rgba:") {
            Some(rgba) => (rgba, 4),
            None => match reply.strip_prefix("rgb:") {
                Some(rgb) => (rgb, 3),
                None => continue,
            },
        };
        let parts: Vec<f64> = rgb.split('/').filter_map(fraction).collect();
        if parts.len() != count {
            continue;
        }
        return Some((parts[0], parts[1], parts[2]));
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
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgba:ffff/ffff/ffff/ffff\x1b\\"),
            Some(Theme::Light)
        );
    }

    #[test]
    fn missing_or_broken_background_reply_is_none() {
        assert_eq!(parse_background_reply(b"\x1b[0n"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:ff/ff\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgba:ff/ff/ff\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:zz/00/00\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:ffff/ffff/ffff"), None);
        assert_eq!(parse_background_colour(b"\x1b[0n"), None);
        assert_eq!(parse_background_colour(b"\x1b]11;rgb:ff/ff\x07"), None);
    }

    #[test]
    fn background_reply_keeps_the_colour() {
        let dark = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[6;34;17t\x1b[0n";
        assert_eq!(parse_background_colour(dark), Some((0x1e, 0x1e, 0x2e)));
        assert_eq!(
            parse_background_colour(b"x\x1b]11;rgb:ffff/fafa/f0f0\x07"),
            Some((0xff, 0xfa, 0xf0))
        );
        assert_eq!(
            parse_background_colour(b"\x1b]11;rgb:f/8/0\x07"),
            Some((0xff, 0x88, 0x00))
        );
        assert_eq!(
            parse_background_colour(b"\x1b]11;rgba:0000/0000/0000/ffff\x1b\\"),
            Some((0, 0, 0))
        );
    }

    #[test]
    fn panel_is_lighter_on_dark_and_darker_on_light() {
        assert_eq!(panel((0, 0, 0)), Color::Rgb(20, 20, 20));
        assert_eq!(panel((0x1e, 0x1e, 0x2e)), Color::Rgb(48, 48, 63));
        assert_eq!(panel((255, 255, 255)), Color::Rgb(235, 235, 235));
    }
}
