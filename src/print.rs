//! Print mode: when stdout is not a terminal, as with `smd note.md | less -R`
//! or `pbpaste | smd > note.txt`, the note is laid out once and written out as
//! text, in colour or not, instead of being shown in the browser.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::{self, Write};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use crate::doc::Document;
use crate::images::Images;
use crate::render;

/// The width to lay a note out at when none is given and no terminal says.
pub const DEFAULT_WIDTH: u16 = 80;

/// When printed text carries colours and styles as escape codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Only on a terminal, which printed text never goes to: as never
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    /// Whether printed text carries colours.
    pub fn colors(self) -> bool {
        self == ColorChoice::Always
    }
}

/// The width to lay a note out at: the terminal's, held to `max_width`, or
/// [`DEFAULT_WIDTH`] when there is no terminal to ask.
pub fn width(terminal: Option<u16>, max_width: Option<u16>) -> u16 {
    let width = terminal.filter(|&w| w > 0).unwrap_or(DEFAULT_WIDTH);
    max_width.map_or(width, |m| m.min(width))
}

/// The terminal's width in columns, from `$COLUMNS` or the terminal smd was
/// started from, when either says.
pub fn terminal_width() -> Option<u16> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .or_else(|| ratatui::crossterm::terminal::size().ok().map(|(w, _)| w))
}

/// `doc` laid out `width` columns wide, as text, with escape codes for its
/// colours and styles when `colors` is on.
pub fn text(doc: &Document, width: u16, colors: bool) -> String {
    let layout = render::render_folds(doc, width, &Images::text(), &HashSet::new());
    let mut out = String::new();
    for line in &layout.lines {
        push_line(&mut out, line, colors);
        out.push('\n');
    }
    // Blank lines at the end are only the layout's margin.
    let kept = out.trim_end_matches('\n').len();
    out.truncate(kept);
    out.push('\n');
    out
}

/// Write `text` to stdout. A reader that stops early, as `head` does, is not
/// a failure.
pub fn write(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn push_line(out: &mut String, line: &Line, colors: bool) {
    if !colors {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        out.push_str(text.trim_end());
        return;
    }
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let codes = sgr(style);
        if codes.is_empty() {
            out.push_str(&span.content);
        } else {
            let _ = write!(out, "\x1b[{codes}m{}\x1b[0m", span.content);
        }
    }
}

/// The SGR parameters that draw `style`, joined by `;`.
fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    let modifiers = style.add_modifier - style.sub_modifier;
    for (modifier, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::RAPID_BLINK, "6"),
        (Modifier::REVERSED, "7"),
        (Modifier::HIDDEN, "8"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifiers.contains(modifier) {
            codes.push(code.into());
        }
    }
    codes.extend(style.fg.and_then(|c| color(c, 30)));
    codes.extend(style.bg.and_then(|c| color(c, 40)));
    codes.join(";")
}

/// The SGR parameter for `color`, `base` being 30 for the foreground and 40
/// for the background. None for the terminal's own colour.
fn color(color: Color, base: u8) -> Option<String> {
    let bright = base + 60;
    Some(match color {
        Color::Reset => return None,
        Color::Black => base.to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray => (base + 7).to_string(),
        Color::DarkGray => bright.to_string(),
        Color::LightRed => (bright + 1).to_string(),
        Color::LightGreen => (bright + 2).to_string(),
        Color::LightYellow => (bright + 3).to_string(),
        Color::LightBlue => (bright + 4).to_string(),
        Color::LightMagenta => (bright + 5).to_string(),
        Color::LightCyan => (bright + 6).to_string(),
        Color::White => (bright + 7).to_string(),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        Color::Indexed(i) => format!("{};5;{i}", base + 8),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    #[test]
    fn only_always_colours_printed_text() {
        assert!(ColorChoice::Always.colors());
        assert!(!ColorChoice::Auto.colors());
        assert!(!ColorChoice::Never.colors());
    }

    #[test]
    fn the_width_is_the_terminals_held_to_max_width() {
        assert_eq!(width(None, None), DEFAULT_WIDTH);
        assert_eq!(width(Some(0), None), DEFAULT_WIDTH);
        assert_eq!(width(Some(120), None), 120);
        assert_eq!(width(Some(120), Some(60)), 60);
        assert_eq!(width(Some(40), Some(60)), 40);
    }

    #[test]
    fn styles_become_sgr_codes() {
        let style = Style::new()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Indexed(7))
            .add_modifier(Modifier::BOLD | Modifier::ITALIC);
        assert_eq!(sgr(style), "1;3;38;2;1;2;3;48;5;7");
        assert_eq!(sgr(Style::new().fg(Color::Red).bg(Color::White)), "31;107");
        assert_eq!(sgr(Style::new().fg(Color::Reset)), "");
    }

    #[test]
    fn plain_text_has_no_escapes_and_no_trailing_blanks() {
        let doc = doc::parse("# Title\n\nSome **bold** text.\n\n```\ncode\n```\n");
        let out = text(&doc, 40, false);
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(out.contains("Some bold text."), "{out:?}");
        assert!(out.contains("code"), "{out:?}");
        assert!(!out.ends_with("\n\n"), "{out:?}");
        assert!(out.lines().all(|l| l == l.trim_end()), "{out:?}");
    }

    #[test]
    fn coloured_text_styles_bold() {
        let doc = doc::parse("Some **bold** text.\n");
        let out = text(&doc, 40, true);
        assert!(out.contains("\x1b[1"), "{out:?}");
        assert!(out.contains("bold\x1b[0m"), "{out:?}");
    }

    #[test]
    fn long_text_wraps_at_the_width() {
        let doc = doc::parse(&"word ".repeat(40));
        let out = text(&doc, 30, false);
        assert!(out.lines().count() > 1, "{out:?}");
        assert!(out.lines().all(|l| l.chars().count() <= 30), "{out:?}");
    }
}
