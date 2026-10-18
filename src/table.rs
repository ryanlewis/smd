//! Tables: drawn as text with box rules when they fit the view, and as an
//! image when they do not.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::doc::InlineStyle;
use crate::images::Png;
use crate::raster::{self, Canvas, Rgba};
use crate::theme::Theme;

/// Text size of the image in pixels. The image is made at this one size and
/// shrunk to fit the view, so a resize never makes a new one.
const SIZE: f32 = 32.0;

/// The most pixels a table image may have; a bigger table stays as text.
const MAX_PIXELS: u64 = 40_000_000;

/// The text of one cell, in runs of one style each.
type Cell = Vec<(String, InlineStyle)>;

#[derive(Debug, PartialEq)]
pub struct Table {
    aligns: Vec<Alignment>,
    /// The header row first. Every row has one cell per column.
    rows: Vec<Vec<Cell>>,
}

impl Table {
    pub fn parse(source: &str) -> Table {
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
        let mut aligns = Vec::new();
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        let mut style = InlineStyle::default();
        for event in Parser::new_ext(source, options) {
            let (text, style) = match event {
                Event::Start(Tag::Table(a)) => {
                    aligns = a;
                    continue;
                }
                Event::Start(Tag::TableHead | Tag::TableRow) => {
                    rows.push(Vec::new());
                    continue;
                }
                Event::Start(Tag::TableCell) => {
                    if let Some(row) = rows.last_mut() {
                        row.push(Vec::new());
                    }
                    continue;
                }
                Event::Start(Tag::Emphasis) | Event::End(TagEnd::Emphasis) => {
                    style.emphasis = matches!(event, Event::Start(_));
                    continue;
                }
                Event::Start(Tag::Strong) | Event::End(TagEnd::Strong) => {
                    style.strong = matches!(event, Event::Start(_));
                    continue;
                }
                Event::Start(Tag::Strikethrough) | Event::End(TagEnd::Strikethrough) => {
                    style.strikethrough = matches!(event, Event::Start(_));
                    continue;
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    style.link = Some(dest_url.to_string());
                    continue;
                }
                Event::End(TagEnd::Link) => {
                    style.link = None;
                    continue;
                }
                Event::Text(t) | Event::InlineHtml(t) | Event::Html(t) => {
                    (t.to_string(), style.clone())
                }
                Event::Code(t) => (
                    t.to_string(),
                    InlineStyle {
                        code: true,
                        ..style.clone()
                    },
                ),
                Event::InlineMath(t) | Event::DisplayMath(t) => (
                    t.to_string(),
                    InlineStyle {
                        math: true,
                        ..style.clone()
                    },
                ),
                Event::SoftBreak | Event::HardBreak => (" ".to_string(), style.clone()),
                _ => continue,
            };
            let Some(cell) = rows.last_mut().and_then(|row| row.last_mut()) else {
                continue;
            };
            match cell.last_mut() {
                Some((s, last)) if *last == style => s.push_str(&text),
                _ => cell.push((text, style)),
            }
        }
        let columns = aligns
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0))
            .max(1);
        aligns.resize(columns, Alignment::None);
        for row in &mut rows {
            row.resize_with(columns, Vec::new);
        }
        Table { aligns, rows }
    }

    /// The width of each column drawn as text.
    fn widths(&self) -> Vec<usize> {
        (0..self.aligns.len())
            .map(|c| {
                let widest = self.rows.iter().map(|row| text_width(&row[c])).max();
                widest.unwrap_or(0).max(1)
            })
            .collect()
    }

    /// Columns the table takes drawn as text.
    pub fn width(&self) -> usize {
        let widths = self.widths();
        widths.iter().sum::<usize>() + 3 * widths.len() + 1
    }

    /// The table as lines of text inside box rules. `styled` turns a cell's
    /// inline style into a terminal style on top of a base style.
    pub fn text(&self, styled: impl Fn(Style, &InlineStyle) -> Style) -> Vec<Vec<Span<'static>>> {
        let widths = self.widths();
        let border = Style::new().fg(Color::DarkGray);
        let rule = |left: &str, mid: &str, right: &str| {
            let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            vec![Span::styled(
                format!("{left}{}{right}", parts.join(mid)),
                border,
            )]
        };
        let mut lines = vec![rule("┌", "┬", "┐")];
        for (r, row) in self.rows.iter().enumerate() {
            let base = if r == 0 {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut line = Vec::new();
            for (c, cell) in row.iter().enumerate() {
                let edge = if c == 0 { "│ " } else { " │ " };
                line.push(Span::styled(edge, border));
                let (before, after) = padding(self.aligns[c], widths[c] - text_width(cell));
                line.push(Span::raw(" ".repeat(before)));
                for (text, style) in cell {
                    line.push(Span::styled(text.clone(), styled(base, style)));
                }
                line.push(Span::raw(" ".repeat(after)));
            }
            line.push(Span::styled(" │", border));
            lines.push(line);
            if r == 0 && self.rows.len() > 1 {
                lines.push(rule("├", "┼", "┤"));
            }
        }
        lines.push(rule("└", "┴", "┘"));
        lines
    }
}

fn text_width(cell: &Cell) -> usize {
    cell.iter().map(|(text, _)| text.width()).sum()
}

/// Space before and after a cell's text to fill `extra` columns.
fn padding(align: Alignment, extra: usize) -> (usize, usize) {
    match align {
        Alignment::Right => (extra, 0),
        Alignment::Center => (extra / 2, extra - extra / 2),
        Alignment::Left | Alignment::None => (0, extra),
    }
}

/// Tints are faint on dark: terminals blend in linear light, which makes a
/// light tint over a dark background look stronger than its alpha.
struct Palette {
    text: Rgba,
    rule: Rgba,
    header: Rgba,
    code: Rgba,
    code_back: Rgba,
    link: Rgba,
    math: Rgba,
}

const DARK: Palette = Palette {
    text: [0xd8, 0xde, 0xe9, 0xff],
    rule: [0x6b, 0x72, 0x80, 0xff],
    header: [0xff, 0xff, 0xff, 0x0a],
    code: [0xe5, 0xc0, 0x7b, 0xff],
    code_back: [0xff, 0xff, 0xff, 0x0c],
    link: [0x61, 0xaf, 0xef, 0xff],
    math: [0x56, 0xb6, 0xc2, 0xff],
};

const LIGHT: Palette = Palette {
    text: [0x24, 0x29, 0x2f, 0xff],
    rule: [0xa0, 0xa7, 0xb0, 0xff],
    header: [0x00, 0x00, 0x00, 0x0e],
    code: [0x95, 0x5f, 0x00, 0xff],
    code_back: [0x00, 0x00, 0x00, 0x12],
    link: [0x05, 0x50, 0xae, 0xff],
    math: [0x0e, 0x74, 0x90, 0xff],
};

/// Draw the table in `source` as a PNG on a transparent background, in
/// colours for the terminal's theme.
pub fn image(source: &str, theme: Theme) -> Result<Png, String> {
    draw(source, theme)
        .and_then(|canvas| canvas.png())
        .map_err(|e| format!("table: {e}"))
}

fn draw(source: &str, theme: Theme) -> Result<Canvas, String> {
    let table = Table::parse(source);
    let fonts = raster::fonts()?;
    let palette = match theme {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    };
    let font = |header: bool, style: &InlineStyle| -> &FontVec {
        if header || style.strong {
            fonts.bold()
        } else {
            &fonts.regular
        }
    };
    let measure = |r: usize, cell: &Cell| -> f32 {
        let runs = cell.iter();
        runs.map(|(text, style)| raster::measure(font(r == 0, style), SIZE, text))
            .sum()
    };

    let pad = (SIZE * 0.5).round();
    let line = 2.0;
    let row_height = (SIZE * 1.6).round();
    let columns = table.aligns.len();
    let widths: Vec<f32> = (0..columns)
        .map(|c| {
            let cells = table.rows.iter().enumerate();
            let widest = cells.map(|(r, row)| measure(r, &row[c]).ceil());
            widest.fold(SIZE * 0.5, f32::max) + 2.0 * pad
        })
        .collect();
    let width = line + widths.iter().map(|w| w + line).sum::<f32>();
    let height = line + table.rows.len() as f32 * (row_height + line);
    let (width, height) = (width as u32, height as u32);
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!("table too large: {width}x{height} px"));
    }

    let mut canvas = Canvas::new(width, height);
    let scaled = fonts.regular.as_scaled(PxScale::from(SIZE));
    let (ascent, descent) = (scaled.ascent(), scaled.descent());
    let mut top = line;
    for (r, row) in table.rows.iter().enumerate() {
        if r == 0 {
            let w = i64::from(width) - 2;
            canvas.fill(1, 1, w, (row_height + line) as i64, palette.header);
        }
        let baseline = top + ((row_height - (ascent - descent)) / 2.0 + ascent).round();
        let mut left = line;
        for (c, cell) in row.iter().enumerate() {
            let extra = widths[c] - 2.0 * pad - measure(r, cell);
            let (before, _) = match table.aligns[c] {
                Alignment::Right => (extra, 0.0),
                Alignment::Center => ((extra / 2.0).floor(), 0.0),
                Alignment::Left | Alignment::None => (0.0, 0.0),
            };
            let mut x = left + pad + before;
            for (text, style) in cell {
                let face = font(r == 0, style);
                let colour = if style.code {
                    palette.code
                } else if style.math {
                    palette.math
                } else if style.link.is_some() {
                    palette.link
                } else {
                    palette.text
                };
                let run = raster::measure(face, SIZE, text);
                if style.code {
                    let back = SIZE * 1.3;
                    let y = top + ((row_height - back) / 2.0).round();
                    let (x0, w) = ((x - 4.0) as i64, (run + 8.0) as i64);
                    canvas.fill(x0, y as i64, w, back as i64, palette.code_back);
                }
                let end = canvas.text(face, SIZE, x, baseline, text, colour);
                if style.link.is_some() {
                    let y = (baseline + 3.0) as i64;
                    canvas.fill(x as i64, y, run as i64, 2, colour);
                }
                if style.strikethrough {
                    let y = (baseline - SIZE * 0.3) as i64;
                    canvas.fill(x as i64, y, run as i64, 2, colour);
                }
                x = end;
            }
            left += widths[c] + line;
        }
        top += row_height + line;
    }

    // Rules between rows and columns, and round the edge.
    let l = line as i64;
    let mut y = 0.0;
    for _ in 0..=table.rows.len() {
        canvas.fill(0, y as i64, i64::from(width), l, palette.rule);
        y += row_height + line;
    }
    let mut x = 0.0;
    for c in 0..=columns {
        canvas.fill(x as i64, 0, l, i64::from(height), palette.rule);
        x += widths.get(c).copied().unwrap_or(0.0) + line;
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(table: &Table) -> Vec<String> {
        let lines = table.text(|base, _| base);
        let text = |line: &Vec<Span>| line.iter().map(|s| s.content.as_ref()).collect();
        lines.iter().map(text).collect()
    }

    #[test]
    fn cells_keep_their_inline_styles() {
        let table = Table::parse("| a | `b c` |\n|---|:-:|\n| **x** y | [l](u) |");
        assert_eq!(table.aligns, vec![Alignment::None, Alignment::Center]);
        let code = InlineStyle {
            code: true,
            ..InlineStyle::default()
        };
        let strong = InlineStyle {
            strong: true,
            ..InlineStyle::default()
        };
        let link = InlineStyle {
            link: Some("u".into()),
            ..InlineStyle::default()
        };
        let plain = InlineStyle::default();
        assert_eq!(
            table.rows,
            vec![
                vec![
                    vec![("a".into(), plain.clone())],
                    vec![("b c".into(), code)]
                ],
                vec![
                    vec![("x".into(), strong), (" y".into(), plain)],
                    vec![("l".into(), link)],
                ],
            ]
        );
    }

    #[test]
    fn short_rows_are_filled_and_escaped_pipes_kept() {
        let table = Table::parse("| a | b |\n|---|---|\n| 1 |\n| x \\| y | 2 |");
        assert_eq!(
            plain(&table),
            vec![
                "┌───────┬───┐",
                "│ a     │ b │",
                "├───────┼───┤",
                "│ 1     │   │",
                "│ x | y │ 2 │",
                "└───────┴───┘",
            ]
        );
        assert_eq!(table.width(), 13);
    }

    #[test]
    fn columns_follow_their_alignment() {
        let table =
            Table::parse("| left | mid | right |\n|:--|:-:|--:|\n| a | b | c |\n| é | 日本 | dd |");
        assert_eq!(
            plain(&table),
            vec![
                "┌──────┬──────┬───────┐",
                "│ left │ mid  │ right │",
                "├──────┼──────┼───────┤",
                "│ a    │  b   │     c │",
                "│ é    │ 日本 │    dd │",
                "└──────┴──────┴───────┘",
            ]
        );
    }

    #[test]
    fn the_header_is_bold() {
        let table = Table::parse("| h |\n|---|\n| b |");
        let lines = table.text(|base, _| base);
        assert!(lines[1][2].style.add_modifier.contains(Modifier::BOLD));
        assert!(!lines[3][2].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_image_grows_with_the_table() {
        if raster::fonts().is_err() {
            eprintln!("no system font; skipped");
            return;
        }
        let size = |md: &str| draw(md, Theme::Dark).unwrap().size();
        let one = size("| a |\n|---|\n| 1 |");
        let three = size("| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |");
        let taller = size("| a |\n|---|\n| 1 |\n| 2 |");
        assert!(three.0 > one.0 && three.1 == one.1, "{one:?} {three:?}");
        assert!(taller.1 > one.1 && taller.0 == one.0, "{one:?} {taller:?}");
        assert!(image("| a |\n|---|\n| 1 |", Theme::Light).is_ok());
    }
}
