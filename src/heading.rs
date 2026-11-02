//! Headings drawn as images in a real font, larger for higher levels.
//!
//! The font is read from the machine rather than bundled (see
//! [`raster::sans_bold`]), with the monospace bold face for inline code and
//! the colour emoji font for emoji. A heading with a character none of them
//! has, an emoji sequence that needs shaping (a flag, a skin tone, a joined
//! family), or with no font found at all, stays as text.
//!
//! Each level is drawn at a fixed height in whole cell rows, so the image does
//! not change when the pane is resized. A heading wider than the pane is
//! shrunk to fit by whole rows (see [`images::fit`](crate::images::fit)); one that cannot shrink to a row
//! is shown as text instead.
//!
//! In a terminal that has kitty's text sizing protocol (OSC 66), headings are
//! not images: they are text the terminal draws larger (see [`draw_sized`]),
//! so they can be selected and their links followed.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::Style;
use serde::Deserialize;

use crate::doc::Inline;
use crate::images::Job;
use crate::kitty;
use crate::raster::{self, Canvas, Emoji, Rgba};
use crate::render::breaks_at;
use crate::theme::Theme;

/// Cell size used by heading jobs, set once at start-up.
static CELL: OnceLock<(u16, u16)> = OnceLock::new();

const DEFAULT_CELL: (u16, u16) = (8, 16);

/// Draw headings for cells of `cell` pixels. Call once, before any are made.
pub fn set_cell(cell: (u16, u16)) {
    let _ = CELL.set((cell.0.max(1), cell.1.max(1)));
}

fn cell() -> (u16, u16) {
    CELL.get().copied().unwrap_or(DEFAULT_CELL)
}

/// How the config asks for headings to be drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Larger text when the terminal can draw it, images when not.
    #[default]
    Auto,
    /// Always images.
    Image,
    /// Always text: larger when the terminal can draw it, plain when not.
    Text,
}

/// How headings are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawn {
    /// As images, or as plain text without graphics.
    Image,
    /// As text the terminal draws larger, with OSC 66.
    Sized,
    /// As plain text.
    Plain,
}

/// How headings are drawn, set once at start-up.
static DRAWN: OnceLock<Drawn> = OnceLock::new();

/// How headings are drawn for `mode` in a terminal that can or cannot
/// (`text_sizing`) draw larger text.
pub fn choose(mode: Mode, text_sizing: bool) -> Drawn {
    match (mode, text_sizing) {
        (Mode::Image, _) => Drawn::Image,
        (Mode::Auto | Mode::Text, true) => Drawn::Sized,
        (Mode::Auto, false) => Drawn::Image,
        (Mode::Text, false) => Drawn::Plain,
    }
}

/// Draw headings as `drawn` says. Call once, before any are laid out.
pub fn set_drawn(drawn: Drawn) {
    let _ = DRAWN.set(drawn);
}

fn drawn() -> Drawn {
    DRAWN.get().copied().unwrap_or(Drawn::Image)
}

/// How many times larger than text each level is drawn with OSC 66, for
/// levels 1 to 6. They take as many rows as the images do.
const SCALES: [u8; 6] = [3, 2, 2, 1, 1, 1];

/// The fewest columns of text a larger heading keeps on a line. Where that
/// many would not fit, it is drawn smaller.
const MIN_COLUMNS: usize = 20;

/// How many times larger a heading of `level` is drawn in `avail` columns:
/// 1, the size of text, unless headings are drawn with OSC 66.
pub fn scale(level: u8, avail: usize) -> u8 {
    scale_for(drawn(), level, avail)
}

fn scale_for(drawn: Drawn, level: u8, avail: usize) -> u8 {
    if drawn != Drawn::Sized {
        return 1;
    }
    let mut scale = SCALES[usize::from(level.clamp(1, 6)) - 1];
    while scale > 1 && avail / usize::from(scale) < MIN_COLUMNS {
        scale -= 1;
    }
    scale
}

/// One line of a heading drawn larger. Each character is `scale` times its
/// columns wide and `scale` rows tall, from row `row` of the layout down.
#[derive(Debug, Clone, PartialEq)]
pub struct SizedLine {
    pub row: usize,
    pub scale: u8,
    pub cells: Vec<SizedCell>,
}

/// A character of a heading drawn larger: its column in the layout, the
/// columns it takes at the size of text, and the character.
#[derive(Debug, Clone, PartialEq)]
pub struct SizedCell {
    pub col: u16,
    pub width: u8,
    pub text: String,
}

/// The OSC 66 sequence that draws `text`, `width` columns wide at the size
/// of text, `scale` times larger. Control characters would end it early.
fn osc66(scale: u8, width: u8, text: &str) -> String {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]66;s={scale}:w={width};{text}\x1b\\")
}

/// Erase the `rows` rows of `cols` cells from the cursor down, the lowest
/// first, leaving the cursor where it was. kitty keeps the lower rows of a
/// larger character when only its top row is written over, and draws a
/// larger character that starts on one of those rows to the right of it, as
/// when a heading moves down a row. Erasing a lower row removes all of it.
fn erase(rows: u16, cols: u16) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if rows > 1 {
        let _ = write!(out, "\x1b[{}B", rows - 1);
    }
    for row in (0..rows).rev() {
        let _ = write!(out, "\x1b[{cols}X");
        if row > 0 {
            out.push_str("\x1b[A");
        }
    }
    out
}

/// Draw the heading lines in view as larger text: the first cell of each
/// character holds its OSC 66 sequence, after an erase of the cells it
/// covers (see [`erase`]), and the other cells are left alone, as the
/// terminal draws over them. `scroll` is the layout row at the top of
/// `area`. The terminal cannot draw part of a character, so a line cut off
/// by the bottom of `area` is drawn at the size of text, and one cut off by
/// the top leaves only its blank rows in view.
pub fn draw_sized(buf: &mut Buffer, area: Rect, scroll: usize, lines: &[SizedLine]) {
    for line in lines {
        let s = u16::from(line.scale);
        let Some(top) = line.row.checked_sub(scroll) else {
            continue;
        };
        if top >= usize::from(area.height) {
            continue;
        }
        let y = area.y + top as u16;
        if top + usize::from(s) > usize::from(area.height) {
            text_size(buf, area, y, line);
            continue;
        }
        for cell in &line.cells {
            let cols = s * u16::from(cell.width);
            let Some(width) = NonZeroU16::new(cols) else {
                continue;
            };
            if cell.col + cols > area.width {
                continue;
            }
            let x = area.x + cell.col;
            for dy in 0..s {
                for dx in 0..cols {
                    buf[(x + dx, y + dy)].set_diff_option(CellDiffOption::Skip);
                }
            }
            let symbol = erase(s, cols) + &osc66(line.scale, cell.width, &cell.text);
            buf[(x, y)]
                .set_symbol(&symbol)
                .set_diff_option(CellDiffOption::ForcedWidth(width));
        }
    }
}

/// Rows of the image and font height, in cell heights, for levels 1 to 6.
const SIZES: [(u32, f32); 6] = [
    (3, 2.4),
    (2, 1.8),
    (2, 1.45),
    (1, 1.0),
    (1, 0.92),
    (1, 0.85),
];

/// A run of heading text, and whether it is inline code.
type Run = (String, bool);

/// The heading's text as runs. Line breaks become spaces, and runs of spaces
/// outside code become one, except no-break spaces, which are kept as written
/// but for any at the ends, which draw nothing in an image.
fn runs(content: &[Inline]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for inline in content {
        let (text, code) = match inline {
            Inline::Text { text, style } => (text.as_str(), style.code),
            Inline::Break => (" ", false),
        };
        match out.last_mut() {
            Some((s, c)) if *c == code => s.push_str(text),
            _ => out.push((text.to_string(), code)),
        }
    }
    for (s, code) in &mut out {
        if !*code {
            let mut collapsed = String::with_capacity(s.len());
            for c in s.chars() {
                let space = breaks_at(c);
                if !(space && collapsed.ends_with(' ')) {
                    collapsed.push(if space { ' ' } else { c });
                }
            }
            *s = collapsed;
        }
    }
    if let Some((s, _)) = out.first_mut() {
        *s = s.trim_start().to_string();
    }
    if let Some((s, _)) = out.last_mut() {
        *s = s.trim_end().to_string();
    }
    out.retain(|(s, _)| !s.is_empty());
    out
}

/// Where a piece's glyphs come from.
#[derive(Clone, Copy)]
enum Face<'a> {
    Outline(&'a FontVec),
    Emoji(&'a Emoji),
}

impl Face<'_> {
    fn same(self, other: Face) -> bool {
        match (self, other) {
            (Face::Outline(a), Face::Outline(b)) => std::ptr::eq(a, b),
            (Face::Emoji(_), Face::Emoji(_)) => true,
            _ => false,
        }
    }
}

/// Text drawn in one face and colour.
struct Piece<'a> {
    face: Face<'a>,
    text: String,
    colour: Rgba,
}

/// Characters that change the emoji before them. Drawing them one by one
/// would be wrong, so a heading with any of them stays text: the zero-width
/// joiner, the keycap mark, skin tones, flag letters and tag characters.
fn needs_shaping(c: char) -> bool {
    matches!(
        c,
        '\u{200D}'
            | '\u{20E3}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{1F1E6}'..='\u{1F1FF}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// The runs split by face: text in `sans`, code in `mono` where it has the
/// glyph and in `sans` where only that does, and emoji in `emoji`. A
/// character with U+FE0F after it asks to be an emoji, so it is drawn in
/// `emoji` when that has it, even if the text font has it too, as many have
/// ❤. Variation selectors are dropped. `None` when a character is in none of
/// them.
fn pieces<'a>(
    runs: &[Run],
    sans: &'a FontVec,
    mono: &'a FontVec,
    emoji: Option<&'a Emoji>,
    (text, code): (Rgba, Rgba),
) -> Option<Vec<Piece<'a>>> {
    // A no-break space is drawn from the font, so it needs a glyph there.
    let has = |font: &FontVec, c: char| breaks_at(c) || font.glyph_id(c).0 != 0;
    let mut out: Vec<Piece> = Vec::new();
    for (s, is_code) in runs {
        let colour = if *is_code { code } else { text };
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if matches!(c, '\u{FE0E}' | '\u{FE0F}') {
                continue;
            }
            if needs_shaping(c) {
                return None;
            }
            let as_emoji = chars.peek() == Some(&'\u{FE0F}');
            let face = if let Some(e) = emoji.filter(|e| as_emoji && e.has(c)) {
                Face::Emoji(e)
            } else if *is_code && has(mono, c) {
                Face::Outline(mono)
            } else if has(sans, c) {
                Face::Outline(sans)
            } else {
                Face::Emoji(emoji.filter(|e| e.has(c))?)
            };
            match out.last_mut() {
                Some(p) if p.face.same(face) && p.colour == colour => p.text.push(c),
                _ => out.push(Piece {
                    face,
                    text: c.to_string(),
                    colour,
                }),
            }
        }
    }
    Some(out)
}

/// The text and code fonts, or `None` when there is no text font.
fn fonts() -> Option<(&'static FontVec, &'static FontVec)> {
    let sans = raster::sans_bold().ok()?;
    let mono = raster::fonts().map_or(sans, |f| f.bold());
    Some((sans, mono))
}

/// Draw `line`'s characters on row `y` of `area` side by side at the size
/// of text, where the layout spaced them out, in the styles they were given.
/// A character laid out past the right edge of `area` takes the style of the
/// one before it.
fn text_size(buf: &mut Buffer, area: Rect, y: u16, line: &SizedLine) {
    let (Some(first), Some(last)) = (line.cells.first(), line.cells.last()) else {
        return;
    };
    let (x, right) = (area.x, area.right());
    let s = u16::from(line.scale);
    let end = (last.col + s * u16::from(last.width)).min(right - x);
    let mut style = Style::default();
    let chars: Vec<_> = line
        .cells
        .iter()
        .map(|c| {
            if x + c.col < right {
                style = buf[(x + c.col, y)].style();
            }
            (c, style)
        })
        .collect();
    for col in first.col..end {
        buf[(x + col, y)].reset();
    }
    let mut col = x + first.col;
    for (cell, style) in chars {
        if col + u16::from(cell.width) > right {
            break;
        }
        buf[(col, y)].set_symbol(&cell.text).set_style(style);
        col += u16::from(cell.width);
    }
}

/// What a heading image is made from, or `None` for a heading that stays
/// text: an empty one, or one the fonts cannot draw. Checking here rather
/// than in the job keeps a heading that cannot be drawn from being tried
/// again on every reload. Headings drawn as text have no image.
pub fn name(level: u8, content: &[Inline], theme: Theme) -> Option<String> {
    if drawn() != Drawn::Image {
        return None;
    }
    let runs = runs(content);
    let (sans, mono) = fonts()?;
    let emoji = raster::emoji();
    if runs.is_empty() || pieces(&runs, sans, mono, emoji, colours(level, theme)).is_none() {
        return None;
    }
    let (w, h) = cell();
    Some(format!("heading:{level}:{}:{w}x{h}:{runs:?}", theme.name()))
}

/// Make the image for a heading.
pub fn job(level: u8, content: &[Inline], theme: Theme) -> Job {
    let runs = runs(content);
    let cell = cell();
    Box::new(move || {
        draw(level, &runs, cell, theme)
            .and_then(|canvas| canvas.png())
            .map_err(|e| format!("heading: {e}"))
    })
}

/// Text and code colours for a level, close to the text-mode styles. Code
/// matches inline code in tables.
fn colours(level: u8, theme: Theme) -> (Rgba, Rgba) {
    match theme {
        Theme::Dark => {
            let text = match level {
                1 => [0xd7, 0x8b, 0xf0, 0xff],
                2 => [0x6f, 0xd3, 0xe8, 0xff],
                3 => [0x8f, 0xd8, 0x8f, 0xff],
                6 => [0xa8, 0xa8, 0xa8, 0xff],
                _ => [0xe8, 0xe8, 0xe8, 0xff],
            };
            (text, [0xe5, 0xc0, 0x7b, 0xff])
        }
        Theme::Light => {
            let text = match level {
                1 => [0x8b, 0x3f, 0xa8, 0xff],
                2 => [0x1f, 0x6f, 0x80, 0xff],
                3 => [0x2e, 0x7d, 0x32, 0xff],
                6 => [0x66, 0x66, 0x66, 0xff],
                _ => [0x22, 0x22, 0x22, 0xff],
            };
            (text, [0x95, 0x5f, 0x00, 0xff])
        }
    }
}

/// Where a heading's text goes in its image.
struct Plan<'a> {
    /// The text font, which emoji are centred against.
    sans: &'a FontVec,
    pieces: Vec<Piece<'a>>,
    /// Font size in pixels.
    size: f32,
    /// Where the text starts, and its baseline.
    left: f32,
    baseline: f32,
    width: u32,
    height: u32,
}

fn plan<'a>(
    (sans, mono): (&'a FontVec, &'a FontVec),
    level: u8,
    runs: &[Run],
    cell: (u16, u16),
    theme: Theme,
) -> Result<Plan<'a>, String> {
    let emoji = raster::emoji();
    let pieces = pieces(runs, sans, mono, emoji, colours(level, theme)).ok_or("missing glyph")?;
    let (cw, ch) = (u32::from(cell.0.max(1)), u32::from(cell.1.max(1)));
    let (rows, size) = SIZES[usize::from(level.clamp(1, 6)) - 1];
    let height = rows * ch;
    let mut size = size * ch as f32;
    let (mut advance, mut ink) = measure(sans, &pieces, size);
    // Accented capitals rise above the font's ascent: make the type smaller
    // rather than cut them off.
    // The ink is rounded out to whole pixels, so one step may not be enough.
    for _ in 0..4 {
        let tall = ink[3] - ink[1];
        if tall <= height as f32 {
            break;
        }
        size *= (height as f32 - 1.0) / tall;
        (advance, ink) = measure(sans, &pieces, size);
    }
    // Centre the line in the rows, then move it to keep all the ink inside.
    // The ink was measured on a whole-pixel baseline, so keep it whole.
    let scaled = sans.as_scaled(PxScale::from(size));
    let centred = ((height as f32 - scaled.height()) / 2.0 + scaled.ascent()).round();
    let baseline = centred.min(height as f32 - ink[3]).max(-ink[1]);
    let left = (1.0 - ink[0]).max(1.0);
    let right = left + advance.max(ink[2]);
    let width = (right.ceil() as u32 + 1).div_ceil(cw) * cw;
    if width > u32::from(kitty::MAX_CELLS) * cw {
        return Err("too wide".into());
    }
    Ok(Plan {
        sans,
        pieces,
        size,
        left,
        baseline,
        width,
        height,
    })
}

/// How far the pieces advance, and the pixels their ink covers on the
/// baseline 0 from x 0: left, top, right, bottom.
fn measure(sans: &FontVec, pieces: &[Piece], size: f32) -> (f32, [f32; 4]) {
    let mut x = 0.0;
    let mut ink = [0.0f32; 4];
    let mut add = |[l, t, r, b]: [f32; 4]| {
        ink = [ink[0].min(l), ink[1].min(t), ink[2].max(r), ink[3].max(b)];
    };
    for p in pieces {
        match p.face {
            Face::Outline(font) => {
                if let Some(bounds) = raster::ink(font, size, x, &p.text) {
                    add(bounds);
                }
                x += raster::measure(font, size, &p.text);
            }
            Face::Emoji(emoji) => {
                for c in p.text.chars() {
                    let [l, t, r, b] = emoji_box(sans, emoji, c, size, x);
                    add([l.floor(), t.floor(), r.ceil(), b.ceil()]);
                    x = r;
                }
            }
        }
    }
    (x, ink)
}

/// Where emoji `c` goes when drawn from `x` on the baseline 0, at its own
/// shape and as tall as the type: left, top, right, bottom. It is centred on
/// the capitals, as emoji sit in running text.
fn emoji_box(sans: &FontVec, emoji: &Emoji, c: char, size: f32, x: f32) -> [f32; 4] {
    let (w, h) = emoji
        .bitmap(c, size)
        .map_or((size, size), |(bitmap, ppem)| {
            let scale = size / ppem;
            (bitmap.width as f32 * scale, bitmap.height as f32 * scale)
        });
    let middle =
        raster::ink(sans, size, 0.0, "H").map_or(-0.35 * size, |[_, t, _, b]| (t + b) / 2.0);
    [x, middle - h / 2.0, x + w, middle + h / 2.0]
}

/// Draw `runs` on a clear background, a whole number of cells wide and tall.
fn draw(level: u8, runs: &[Run], cell: (u16, u16), theme: Theme) -> Result<Canvas, String> {
    let plan = plan(fonts().ok_or("no font")?, level, runs, cell, theme)?;
    let mut canvas = Canvas::new(plan.width, plan.height);
    let mut x = plan.left;
    for p in &plan.pieces {
        match p.face {
            Face::Outline(font) => {
                x = canvas.text(font, plan.size, x, plan.baseline, &p.text, p.colour);
            }
            Face::Emoji(emoji) => {
                for c in p.text.chars() {
                    let [l, t, r, b] = emoji_box(plan.sans, emoji, c, plan.size, x);
                    if let Some((bitmap, _)) = emoji.bitmap(c, plan.size) {
                        let top = t + plan.baseline;
                        canvas.image(&bitmap, l, top, r - l, b - t);
                    }
                    x = r;
                }
            }
        }
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{BlockKind, parse};

    fn heading(md: &str) -> (u8, Vec<Inline>) {
        match parse(md).blocks.remove(0).kind {
            BlockKind::Heading { level, content } => (level, content),
            other => panic!("not a heading: {other:?}"),
        }
    }

    #[test]
    fn runs_mark_inline_code_and_trim() {
        let (_, content) = heading("## Use `smd --watch`  now\n");
        assert_eq!(
            runs(&content),
            vec![
                ("Use ".to_string(), false),
                ("smd --watch".to_string(), true),
                (" now".to_string(), false),
            ]
        );
    }

    #[test]
    fn runs_keep_no_break_spaces() {
        let (_, content) = heading("# 10&nbsp;km a&nbsp;&nbsp;b&nbsp;\n");
        assert_eq!(
            runs(&content),
            vec![("10\u{a0}km a\u{a0}\u{a0}b".to_string(), false)]
        );
        // Only at the ends are they trimmed, so a heading of them is empty.
        let (_, content) = heading("# &nbsp;&nbsp;\n");
        assert_eq!(runs(&content), vec![]);
    }

    #[test]
    fn the_name_depends_on_level_text_and_theme() {
        if fonts().is_none() {
            return;
        }
        let named = |md, theme| {
            let (level, content) = heading(md);
            name(level, &content, theme)
        };
        assert_ne!(
            named("# Title\n", Theme::Dark),
            named("## Title\n", Theme::Dark)
        );
        assert_ne!(
            named("# Title\n", Theme::Dark),
            named("# Title\n", Theme::Light)
        );
        // Emphasis does not change how the heading is drawn.
        assert_eq!(
            named("# Title\n", Theme::Dark),
            named("# *Title*\n", Theme::Dark)
        );
        assert_eq!(name(1, &[], Theme::Dark), None);
    }

    fn drawn(md: &str, cell: (u16, u16), theme: Theme) -> Canvas {
        let (level, content) = heading(md);
        draw(level, &runs(&content), cell, theme).unwrap()
    }

    #[test]
    fn levels_get_whole_rows_and_smaller_type() {
        // Every font the code could pick here, as their widths differ.
        let fonts = raster::sans_bold_all();
        let mono = raster::fonts().ok().map(|f| f.bold());
        for sans in &fonts {
            let plans: Vec<Plan> = (1..=6)
                .map(|level| {
                    let runs = [("Heading".to_string(), false)];
                    let fonts = (sans, mono.unwrap_or(sans));
                    plan(fonts, level, &runs, (10, 20), Theme::Dark).unwrap()
                })
                .collect();
            let heights: Vec<u32> = plans.iter().map(|p| p.height).collect();
            assert_eq!(heights, vec![60, 40, 40, 20, 20, 20]);
            // The type gets smaller every level. The image is whole cells
            // wide, so two levels can round up to the same width.
            for pair in plans.windows(2) {
                assert!(pair[0].size > pair[1].size);
                assert!(pair[0].width >= pair[1].width);
            }
            for p in &plans {
                assert_eq!(p.width % 10, 0);
            }
        }
    }

    #[test]
    fn accents_and_overhangs_stay_inside_the_image() {
        if fonts().is_none() {
            return;
        }
        for level in 1..=6 {
            let emoji = raster::emoji().is_some();
            let texts = ["Évolution", "Å jÄ fÉ", "j`ÉÅ`f", "🏠 Home ✅"];
            for text in texts.into_iter().filter(|t| emoji || !t.contains('🏠')) {
                let (_, content) = heading(&format!("# {text}\n"));
                let plan = plan(
                    fonts().unwrap(),
                    level,
                    &runs(&content),
                    (17, 34),
                    Theme::Dark,
                )
                .unwrap();
                let (_, [l, t, r, b]) = measure(plan.sans, &plan.pieces, plan.size);
                let case = format!("h{level} {text}");
                assert!(l + plan.left >= 0.0, "{case}");
                assert!(t + plan.baseline >= 0.0, "{case}");
                assert!(r + plan.left <= plan.width as f32, "{case}");
                assert!(b + plan.baseline <= plan.height as f32, "{case}");
            }
        }
    }

    #[test]
    fn missing_glyphs_and_huge_headings_stay_text() {
        if fonts().is_none() {
            return;
        }
        let (level, content) = heading("# 日本\n");
        assert_eq!(name(level, &content, Theme::Dark), None);
        // Emoji sequences that need shaping: a flag, a skin tone, a family.
        for text in ["🇬🇧 Home", "👋🏽 Hi", "👨\u{200D}👩\u{200D}👧 Us"] {
            let (level, content) = heading(&format!("# {text}\n"));
            assert_eq!(name(level, &content, Theme::Dark), None, "{text}");
        }
        let runs = vec![("word ".repeat(400), false)];
        assert!(draw(1, &runs, (10, 20), Theme::Dark).is_err());
    }

    #[test]
    fn emoji_are_drawn_in_colour() {
        if fonts().is_none() || raster::emoji().is_none() {
            return;
        }
        let (level, content) = heading("# 🏠 Dashboard ❤️\n");
        assert!(name(level, &content, Theme::Dark).is_some());
        let plan = plan(
            fonts().unwrap(),
            level,
            &runs(&content),
            (10, 20),
            Theme::Dark,
        )
        .unwrap();
        // The variation selector is dropped, and the emoji are their own pieces.
        let emoji: Vec<&str> = plan
            .pieces
            .iter()
            .filter(|p| matches!(p.face, Face::Emoji(_)))
            .map(|p| p.text.as_str())
            .collect();
        assert_eq!(emoji, ["🏠", "❤"]);
        // The house has colours the heading text does not.
        let canvas = drawn("# 🏠\n", (10, 20), Theme::Dark);
        let text = colours(1, Theme::Dark).0;
        assert!(canvas.pixels().any(|p| p[3] == 255 && p[..3] != text[..3]));
    }

    #[test]
    fn the_theme_changes_the_colours_not_the_size() {
        if fonts().is_none() {
            return;
        }
        let dark = drawn("# Theme\n", (10, 20), Theme::Dark);
        let light = drawn("# Theme\n", (10, 20), Theme::Light);
        assert_eq!(dark.size(), light.size());
        assert_ne!(dark.png().unwrap(), light.png().unwrap());
    }

    #[test]
    fn each_level_has_its_colour_in_both_themes() {
        let dark = |t: [u8; 3]| [t[0], t[1], t[2], 0xff];
        let want_dark = [
            (1, dark([0xd7, 0x8b, 0xf0])),
            (2, dark([0x6f, 0xd3, 0xe8])),
            (3, dark([0x8f, 0xd8, 0x8f])),
            (4, dark([0xe8, 0xe8, 0xe8])),
            (5, dark([0xe8, 0xe8, 0xe8])),
            (6, dark([0xa8, 0xa8, 0xa8])),
        ];
        let want_light = [
            (1, dark([0x8b, 0x3f, 0xa8])),
            (2, dark([0x1f, 0x6f, 0x80])),
            (3, dark([0x2e, 0x7d, 0x32])),
            (4, dark([0x22, 0x22, 0x22])),
            (5, dark([0x22, 0x22, 0x22])),
            (6, dark([0x66, 0x66, 0x66])),
        ];
        for (level, text) in want_dark {
            assert_eq!(
                colours(level, Theme::Dark),
                (text, [0xe5, 0xc0, 0x7b, 0xff]),
                "dark level {level}"
            );
        }
        for (level, text) in want_light {
            assert_eq!(
                colours(level, Theme::Light),
                (text, [0x95, 0x5f, 0x00, 0xff]),
                "light level {level}"
            );
        }
    }

    #[test]
    fn headings_are_sized_only_when_the_terminal_can_draw_it() {
        assert_eq!(choose(Mode::Auto, true), Drawn::Sized);
        assert_eq!(choose(Mode::Auto, false), Drawn::Image);
        assert_eq!(choose(Mode::Image, true), Drawn::Image);
        assert_eq!(choose(Mode::Image, false), Drawn::Image);
        assert_eq!(choose(Mode::Text, true), Drawn::Sized);
        assert_eq!(choose(Mode::Text, false), Drawn::Plain);
    }

    #[test]
    fn sizes_go_down_by_level_and_in_narrow_panes() {
        let scales: Vec<u8> = (1..=6).map(|l| scale_for(Drawn::Sized, l, 80)).collect();
        assert_eq!(scales, [3, 2, 2, 1, 1, 1]);
        // 50 columns hold 16 at three times the size, 25 at twice.
        assert_eq!(scale_for(Drawn::Sized, 1, 50), 2);
        assert_eq!(scale_for(Drawn::Sized, 1, 30), 1);
        assert_eq!(scale_for(Drawn::Sized, 0, 80), 3);
        assert_eq!(scale_for(Drawn::Sized, 9, 80), 1);
        // Images and plain text are never sized.
        for drawn in [Drawn::Image, Drawn::Plain] {
            assert_eq!(scale_for(drawn, 1, 200), 1);
        }
        // Nothing is set in tests, so headings are images, as before.
        assert_eq!(scale(1, 200), 1);
    }

    #[test]
    fn osc66_names_the_scale_and_width() {
        assert_eq!(osc66(2, 1, "H"), "\x1b]66;s=2:w=1;H\x1b\\");
        assert_eq!(osc66(3, 2, "日"), "\x1b]66;s=3:w=2;日\x1b\\");
        assert_eq!(osc66(2, 2, "❤\u{FE0F}"), "\x1b]66;s=2:w=2;❤\u{FE0F}\x1b\\");
        assert_eq!(osc66(2, 1, "a\x1b\x07"), "\x1b]66;s=2:w=1;a\x1b\\");
    }

    fn sized(row: usize, scale: u8, cells: &[(u16, u8, &str)]) -> SizedLine {
        SizedLine {
            row,
            scale,
            cells: cells
                .iter()
                .map(|&(col, width, text)| SizedCell {
                    col,
                    width,
                    text: text.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn sized_characters_take_their_whole_block() {
        // A narrow letter, a wide one and an emoji, twice the size.
        let line = sized(1, 2, &[(1, 1, "A"), (3, 2, "日"), (7, 2, "🏠")]);
        let area = Rect::new(0, 0, 12, 4);
        let mut buf = Buffer::empty(area);
        draw_sized(&mut buf, area, 0, &[line]);
        let forced = |n| CellDiffOption::ForcedWidth(NonZeroU16::new(n).unwrap());
        let erase = "\x1b[1B\x1b[2X\x1b[A\x1b[2X";
        assert_eq!(
            buf[(1, 1)].symbol(),
            format!("{erase}\x1b]66;s=2:w=1;A\x1b\\")
        );
        assert_eq!(buf[(1, 1)].diff_option, forced(2));
        let erase = "\x1b[1B\x1b[4X\x1b[A\x1b[4X";
        assert_eq!(
            buf[(3, 1)].symbol(),
            format!("{erase}\x1b]66;s=2:w=2;日\x1b\\")
        );
        assert_eq!(buf[(3, 1)].diff_option, forced(4));
        assert_eq!(buf[(7, 1)].diff_option, forced(4));
        // Every other cell of the blocks is left to the terminal.
        let skip = |x, y| buf[(x, y)].diff_option == CellDiffOption::Skip;
        for (x, y) in [(2, 1), (1, 2), (2, 2), (4, 1), (6, 1), (6, 2), (10, 2)] {
            assert!(skip(x, y), "({x}, {y})");
        }
        for (x, y) in [(0, 1), (0, 2), (11, 1), (1, 0), (1, 3)] {
            assert_eq!(buf[(x, y)].diff_option, CellDiffOption::None, "({x}, {y})");
        }
    }

    #[test]
    fn a_sized_character_clears_its_block_before_it_is_drawn() {
        use ratatui::backend::{Backend, CrosstermBackend};
        // "An" twice the size moves down a row, as a heading does when an
        // image above it is ready, so "Ma" starts on the lower row of "An".
        let area = Rect::new(0, 0, 10, 4);
        let frame = |line: SizedLine| {
            let mut buf = Buffer::empty(area);
            draw_sized(&mut buf, area, 0, &[line]);
            buf
        };
        let before = frame(sized(0, 2, &[(0, 1, "A"), (2, 1, "n")]));
        let after = frame(sized(1, 2, &[(0, 1, "M"), (2, 1, "a")]));
        let mut bytes = Vec::new();
        CrosstermBackend::new(&mut bytes)
            .draw(before.diff(&after).into_iter())
            .unwrap();
        let bytes = String::from_utf8(bytes).unwrap();
        // kitty keeps the lower row of "A" when only its top row is written
        // over, and would draw "M" to the right of it. Each character
        // first erases the rows it covers, the lowest first.
        let m = "\x1b[2;1H\x1b[1B\x1b[2X\x1b[A\x1b[2X\x1b]66;s=2:w=1;M\x1b\\";
        assert!(bytes.contains(m), "{bytes:?}");
        let a = "\x1b[2;3H\x1b[1B\x1b[2X\x1b[A\x1b[2X\x1b]66;s=2:w=1;a\x1b\\";
        assert!(bytes.contains(a), "{bytes:?}");
    }

    #[test]
    fn sized_lines_partly_out_of_view_stay_as_laid_out() {
        let lines = [sized(0, 2, &[(0, 1, "A")]), sized(3, 3, &[(0, 1, "B")])];
        let area = Rect::new(0, 0, 10, 4);
        // Scrolled one row down, the first is cut at the top: nothing is
        // drawn on its blank row.
        let mut buf = Buffer::empty(area);
        draw_sized(&mut buf, area, 1, &lines[..1]);
        assert_eq!(buf, Buffer::empty(area));
        // In view inside an area lower down the screen.
        let area = Rect::new(2, 1, 10, 6);
        let mut buf = Buffer::empty(area);
        draw_sized(&mut buf, area, 0, &lines);
        let a = "\x1b[1B\x1b[2X\x1b[A\x1b[2X\x1b]66;s=2:w=1;A\x1b\\";
        assert_eq!(buf[(2, 1)].symbol(), a);
        let b = "\x1b[2B\x1b[3X\x1b[A\x1b[3X\x1b[A\x1b[3X\x1b]66;s=3:w=1;B\x1b\\";
        assert_eq!(buf[(2, 4)].symbol(), b);
        // A character past the right edge is left out.
        let mut buf = Buffer::empty(area);
        draw_sized(&mut buf, area, 0, &[sized(0, 2, &[(9, 1, "C")])]);
        assert_eq!(buf, Buffer::empty(area));
    }

    #[test]
    fn sized_lines_cut_at_the_bottom_are_drawn_at_the_size_of_text() {
        use ratatui::style::{Color, Style};
        // As laid out: "A", a space and "日" spaced out three times.
        let line = sized(1, 3, &[(1, 1, "A"), (4, 1, " "), (7, 2, "日")]);
        let area = Rect::new(0, 0, 14, 3);
        let mut buf = Buffer::empty(area);
        let red = Style::new().fg(Color::Red);
        buf.set_string(1, 1, "A     日    ", red);
        draw_sized(&mut buf, area, 0, &[line]);
        let mut want = Buffer::empty(area);
        want.set_string(1, 1, "A 日", red);
        assert_eq!(buf, want);
    }

    #[test]
    fn levels_outside_one_to_six_get_the_body_colour() {
        for theme in [Theme::Dark, Theme::Light] {
            assert_eq!(colours(0, theme).0, colours(4, theme).0);
            assert_eq!(colours(7, theme).0, colours(4, theme).0);
        }
    }

    use crate::raster::tests::{
        OPEN_STAR, STAR, emoji_font, solid_png, square_font, square_font_from,
    };

    const TEXT: Rgba = [1, 1, 1, 255];
    const CODE: Rgba = [2, 2, 2, 255];

    #[test]
    fn plan_centres_the_line_in_its_rows_on_a_whole_pixel() {
        let (sans, mono) = (square_font(), square_font_from('b'));
        // At 24 pixels "ab" advances 19.2 and inks 17 up. The line is 24
        // tall in 30, so it is centred 3 down, on the baseline 22.
        let runs = [("ab".to_string(), false)];
        let p = plan((&sans, &mono), 1, &runs, (1, 10), Theme::Dark).unwrap();
        assert_eq!(p.size, 24.0);
        assert_eq!((p.left, p.baseline), (1.0, 22.0));
        assert_eq!((p.width, p.height), (22, 30));
    }

    #[test]
    fn plan_lifts_a_descender_off_the_bottom_of_the_image() {
        let (sans, mono) = (square_font(), square_font_from('b'));
        // 'c' goes 8 pixels below the baseline at 20, so the baseline 16
        // the line would be centred on moves up to 12.
        let runs = [("c".to_string(), false)];
        let p = plan((&sans, &mono), 4, &runs, (1, 20), Theme::Dark).unwrap();
        assert_eq!(p.size, 20.0);
        assert_eq!((p.left, p.baseline), (1.0, 12.0));
        assert_eq!((p.width, p.height), (8, 20));
    }

    #[test]
    fn plan_shrinks_type_whose_ink_is_taller_than_the_image() {
        let (sans, mono) = (square_font(), square_font_from('b'));
        // "bc" inks 14 up and 8 down at 20: 22 in 20 rows, so it shrinks
        // to fit in 19, after which it inks 13 up and 7 down.
        let runs = [("bc".to_string(), false)];
        let p = plan((&sans, &mono), 4, &runs, (1, 20), Theme::Dark).unwrap();
        assert_eq!(p.size, 20.0 * (19.0 / 22.0));
        assert_eq!((p.baseline, p.width, p.height), (13.0, 15, 20));
    }

    #[test]
    fn plan_takes_up_to_the_most_cells_kitty_can_place() {
        let (sans, mono) = (square_font(), square_font_from('b'));
        // Each 'a' is one 6 pixel cell, and the margins take one more.
        let max = usize::from(kitty::MAX_CELLS);
        let runs = [("a".repeat(max - 1), false)];
        let p = plan((&sans, &mono), 4, &runs, (6, 10), Theme::Dark).unwrap();
        assert_eq!(p.width, u32::from(kitty::MAX_CELLS) * 6);
        let runs = [("a".repeat(max), false)];
        assert!(plan((&sans, &mono), 4, &runs, (6, 10), Theme::Dark).is_err());
    }

    /// Each piece as which of `faces` it is drawn in, its text and colour.
    fn faces_of<'a>(
        pieces: &[Piece],
        faces: &[(&'a str, &FontVec)],
    ) -> Vec<(&'a str, String, Rgba)> {
        pieces
            .iter()
            .map(|p| {
                let face = match p.face {
                    Face::Emoji(_) => "emoji",
                    Face::Outline(f) => {
                        faces
                            .iter()
                            .find(|(_, font)| std::ptr::eq(*font, f))
                            .unwrap()
                            .0
                    }
                };
                (face, p.text.clone(), p.colour)
            })
            .collect()
    }

    #[test]
    fn pieces_split_where_the_face_or_colour_changes() {
        // Code is drawn in the code font where it has the glyph, and in the
        // text font where only that does. Text is never in the code font.
        let (sans, mono) = (square_font(), square_font_from('b'));
        let runs = [("ab".to_string(), false), ("bdab".to_string(), true)];
        let got = pieces(&runs, &sans, &mono, None, (TEXT, CODE)).unwrap();
        let faces = [("sans", &sans), ("mono", &mono)];
        assert_eq!(
            faces_of(&got, &faces),
            [
                ("sans", "ab".to_string(), TEXT),
                ("mono", "bd".to_string(), CODE),
                ("sans", "a".to_string(), CODE),
                ("mono", "b".to_string(), CODE),
            ]
        );
        // A character in neither font, with no emoji font or one without it.
        let missing = [("ad".to_string(), false)];
        assert!(pieces(&missing, &sans, &mono, None, (TEXT, CODE)).is_none());
        let png = solid_png(1, 1, [9, 9, 9, 255]);
        let emoji = emoji_font(&[(20, [None, Some(&png), None])]);
        assert!(pieces(&missing, &sans, &mono, Some(&emoji), (TEXT, CODE)).is_none());
    }

    #[test]
    fn pieces_draw_a_character_asking_to_be_an_emoji_from_the_emoji_font() {
        let png = solid_png(1, 1, [9, 9, 9, 255]);
        let emoji = emoji_font(&[(20, [None, Some(&png), None])]);
        let (sans, mono) = (square_font_from(STAR), square_font());
        // With U+FE0F the star is an emoji, as the emoji font has it;
        // without, it is text. The open star has no emoji image, so it is
        // text even when asked.
        let text = format!("{STAR}\u{FE0F}{STAR}\u{FE0F}{STAR}{OPEN_STAR}\u{FE0F}");
        let runs = [(text, false)];
        let got = pieces(&runs, &sans, &mono, Some(&emoji), (TEXT, CODE)).unwrap();
        let faces = [("sans", &sans), ("mono", &mono)];
        assert_eq!(
            faces_of(&got, &faces),
            [
                ("emoji", format!("{STAR}{STAR}"), TEXT),
                ("sans", format!("{STAR}{OPEN_STAR}"), TEXT),
            ]
        );
    }

    #[test]
    fn shaping_marks_are_the_joiner_keycap_skin_tones_flags_and_tags() {
        for c in [
            '\u{200D}',
            '\u{20E3}',
            '\u{1F3FB}',
            '\u{1F3FF}',
            '\u{1F1E6}',
            '\u{1F1FF}',
            '\u{E0020}',
            '\u{E007F}',
        ] {
            assert!(needs_shaping(c), "{:04X}", c as u32);
        }
        for c in ['a', '\u{FE0F}', '\u{1F3FA}', '\u{1F200}', '\u{E0080}', '🏠'] {
            assert!(!needs_shaping(c), "{:04X}", c as u32);
        }
    }

    #[test]
    fn emoji_are_as_tall_as_their_image_and_centred_on_the_capitals() {
        // A 4 by 2 image from a 20 pixel strike is 2 by 1 at 10 pixels.
        let png = solid_png(4, 2, [9, 9, 9, 255]);
        let emoji = emoji_font(&[(20, [None, Some(&png), None])]);
        // 'H' inks 4 up, so emoji centre 2 up.
        let with_h = square_font_from('H');
        assert_eq!(
            emoji_box(&with_h, &emoji, STAR, 10.0, 3.0),
            [3.0, -2.5, 5.0, -1.5]
        );
        // An 'H' that inks 3 up and 4 down puts the middle half a pixel down.
        let low_h = square_font_from('F');
        assert_eq!(
            emoji_box(&low_h, &emoji, STAR, 10.0, 3.0),
            [3.0, 0.0, 5.0, 1.0]
        );
        // With no image, an emoji is a square as tall as the type.
        assert_eq!(
            emoji_box(&with_h, &emoji, OPEN_STAR, 10.0, 3.0),
            [3.0, -7.0, 13.0, 3.0]
        );
        // With no 'H', the middle is guessed at 0.35 of the type up.
        let without_h = square_font();
        assert_eq!(
            emoji_box(&without_h, &emoji, STAR, 10.0, 3.0),
            [3.0, -4.0, 5.0, -3.0]
        );
    }

    #[test]
    fn measure_follows_each_piece_on_from_the_last() {
        let png = solid_png(4, 2, [9, 9, 9, 255]);
        let emoji = emoji_font(&[(20, [None, Some(&png), None])]);
        let (sans, other) = (square_font(), square_font());
        let piece = |face, text: &str| Piece {
            face,
            text: text.to_string(),
            colour: TEXT,
        };
        // "a" advances 6, then "b" from there 4, then the emoji 2.
        let pieces = [
            piece(Face::Outline(&sans), "a"),
            piece(Face::Outline(&other), "b"),
            piece(Face::Emoji(&emoji), &STAR.to_string()),
        ];
        assert_eq!(
            measure(&sans, &pieces, 10.0),
            (12.0, [0.0, -7.0, 12.0, 0.0])
        );
    }

    #[test]
    fn runs_join_text_of_the_same_kind_across_styles() {
        let (_, content) = heading("# a *b* c\n");
        assert_eq!(runs(&content), vec![("a b c".to_string(), false)]);
    }

    #[test]
    fn the_name_has_the_default_cell_size() {
        assert_eq!(fonts().is_some(), raster::sans_bold().is_ok());
        if fonts().is_none() {
            return;
        }
        let (level, content) = heading("# Title\n");
        assert_eq!(
            name(level, &content, Theme::Dark).as_deref(),
            Some("heading:1:dark:8x16:[(\"Title\", false)]")
        );
    }

    #[test]
    fn one_row_is_erased_without_moving() {
        assert_eq!(erase(1, 3), "\x1b[3X");
    }

    #[test]
    fn scale_stays_at_one_and_keeps_twenty_columns() {
        // A level drawn at the size of text stays there in a narrow pane.
        assert_eq!(scale_for(Drawn::Sized, 4, 10), 1);
        // 60 columns hold exactly 20 at three times the size; 59 do not.
        assert_eq!(scale_for(Drawn::Sized, 1, 60), 3);
        assert_eq!(scale_for(Drawn::Sized, 1, 59), 2);
    }

    #[test]
    fn a_sized_line_ending_on_the_last_row_or_column_is_drawn_sized() {
        let area = Rect::new(0, 0, 6, 3);
        let mut buf = Buffer::empty(area);
        draw_sized(&mut buf, area, 0, &[sized(1, 2, &[(0, 1, "A")])]);
        let want = "\x1b[1B\x1b[2X\x1b[A\x1b[2X\x1b]66;s=2:w=1;A\x1b\\";
        assert_eq!(buf[(0, 1)].symbol(), want);
        // And one ending on the last column.
        let mut buf = Buffer::empty(area);
        draw_sized(&mut buf, area, 0, &[sized(0, 2, &[(4, 1, "B")])]);
        let want = "\x1b[1B\x1b[2X\x1b[A\x1b[2X\x1b]66;s=2:w=1;B\x1b\\";
        assert_eq!(buf[(4, 0)].symbol(), want);
    }

    #[test]
    fn a_cut_line_at_the_size_of_text_stops_at_the_right_edge() {
        use ratatui::style::{Color, Style};
        let red = Style::new().fg(Color::Red);
        // An area from column 2 to 12, its second row full of "X", with a
        // line three times the size cut at the bottom.
        let area = Rect::new(2, 0, 10, 2);
        let cut = |cells: &[(u16, u8, &str)]| {
            let mut buf = Buffer::empty(area);
            buf.set_string(2, 1, "X".repeat(10), red);
            draw_sized(&mut buf, area, 0, &[sized(1, 3, cells)]);
            buf
        };
        let row = |parts: &[(u16, &str)]| {
            let mut want = Buffer::empty(area);
            for &(x, text) in parts {
                want.set_string(x, 1, text, red);
            }
            want
        };
        // "A" clears the three columns it was laid out on, from 5.
        assert_eq!(cut(&[(3, 1, "A")]), row(&[(2, "XXXA"), (8, "XXXX")]));
        // A wide character that just fits on the last two columns.
        assert_eq!(cut(&[(8, 2, "日")]), row(&[(2, "XXXXXXXX"), (10, "日")]));
        // A character laid out from the edge itself, after one that fits.
        assert_eq!(
            cut(&[(8, 2, "日"), (10, 1, "B")]),
            row(&[(2, "XXXXXXXX"), (10, "日")])
        );
        // One that would hang over the edge is left out.
        assert_eq!(cut(&[(9, 2, "日")]), row(&[(2, "XXXXXXXXX")]));
    }
}
