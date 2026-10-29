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

use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

use crate::doc::Inline;
use crate::images::Job;
use crate::kitty;
use crate::raster::{self, Canvas, Emoji, Rgba};
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
/// outside code become one.
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
                let space = c.is_whitespace();
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
    let has = |font: &FontVec, c: char| c.is_whitespace() || font.glyph_id(c).0 != 0;
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

/// What a heading image is made from, or `None` for a heading that stays
/// text: an empty one, or one the fonts cannot draw. Checking here rather
/// than in the job keeps a heading that cannot be drawn from being tried
/// again on every reload.
pub fn name(level: u8, content: &[Inline], theme: Theme) -> Option<String> {
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
    fn levels_outside_one_to_six_get_the_body_colour() {
        for theme in [Theme::Dark, Theme::Light] {
            assert_eq!(colours(0, theme).0, colours(4, theme).0);
            assert_eq!(colours(7, theme).0, colours(4, theme).0);
        }
    }
}
