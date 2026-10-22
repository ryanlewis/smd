//! Spike copy of `src/table.rs` with cell wrapping: the text table wraps its
//! cells to fit a width, and the image wraps them at a fixed cap.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::doc::InlineStyle;
use crate::raster::{self, Canvas, Rgba};
use crate::theme::Theme;

/// Text size of the image in pixels.
pub const SIZE: f32 = 32.0;

/// The most pixels a table image may have.
const MAX_PIXELS: u64 = 40_000_000;

/// Characters a long word may be broken after, before falling back to a
/// break at the column edge.
const BREAK_AFTER: &str = "/-_.,:=";

/// Longest word a column is widened for; a longer one, such as a URL,
/// breaks.
pub const WORD_CAP: usize = 30;

/// The text of one cell, in runs of one style each.
pub type Cell = Vec<(String, InlineStyle)>;

#[derive(Debug, PartialEq)]
pub struct Table {
    pub aligns: Vec<Alignment>,
    /// The header row first. Every row has one cell per column.
    pub rows: Vec<Vec<Cell>>,
}

/// A cell wrapped to a width.
pub struct Wrapped {
    pub lines: Vec<Cell>,
    /// Words broken after punctuation such as `/`.
    pub soft: usize,
    /// Words broken at the column edge, mid-word.
    pub hard: usize,
}

/// How a wrapped text table came out.
#[derive(Default, Debug, Clone)]
pub struct Stats {
    /// Lines each body row takes.
    pub heights: Vec<usize>,
    pub soft: usize,
    pub hard: usize,
}

impl Stats {
    pub fn mean(&self) -> f32 {
        let n = self.heights.len().max(1);
        self.heights.iter().sum::<usize>() as f32 / n as f32
    }

    pub fn max(&self) -> usize {
        self.heights.iter().copied().max().unwrap_or(1)
    }
}

impl Table {
    pub fn parse(source: &str) -> Table {
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
        let mut aligns = Vec::new();
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        let mut style = InlineStyle::default();
        let mut depth = [0u32; 3];
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
                    style.emphasis = nest(&mut depth[0], &event);
                    continue;
                }
                Event::Start(Tag::Strong) | Event::End(TagEnd::Strong) => {
                    style.strong = nest(&mut depth[1], &event);
                    continue;
                }
                Event::Start(Tag::Strikethrough) | Event::End(TagEnd::Strikethrough) => {
                    style.strikethrough = nest(&mut depth[2], &event);
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

    pub fn columns(&self) -> usize {
        self.aligns.len()
    }

    /// The width of each column with nothing wrapped.
    pub fn natural(&self) -> Vec<usize> {
        (0..self.columns())
            .map(|c| {
                let widest = self.rows.iter().map(|row| text_width(&row[c])).max();
                widest.unwrap_or(0).max(1)
            })
            .collect()
    }

    /// Columns the table takes with nothing wrapped.
    pub fn width(&self) -> usize {
        frame(&self.natural())
    }

    /// The widest word in each column, at most `cap`.
    pub fn words(&self, cap: usize) -> Vec<usize> {
        (0..self.columns())
            .map(|c| {
                let cells = self.rows.iter().map(|row| &row[c]);
                let words = cells.flat_map(|cell| {
                    let text: String = cell.iter().map(|(t, _)| t.as_str()).collect();
                    text.split(' ')
                        .map(UnicodeWidthStr::width)
                        .collect::<Vec<_>>()
                });
                words.max().unwrap_or(1).clamp(1, cap)
            })
            .collect()
    }

    /// Column widths that fit in `avail` columns. Each column first gets
    /// room for its longest word, up to `word_cap`, so words stay whole; if
    /// that does not fit, each gets its natural width or `floor`, whichever
    /// is less. The space left over goes to each column in proportion to how
    /// much it wants beyond that. `None` when even the floors do not fit.
    pub fn fit_widths(&self, avail: usize, floor: usize) -> Option<Vec<usize>> {
        let natural = self.natural();
        let budget = avail.checked_sub(frame(&vec![0; natural.len()]))?;
        if natural.iter().sum::<usize>() <= budget {
            return Some(natural);
        }
        let words = self.words(WORD_CAP);
        let min: Vec<usize> = if words.iter().sum::<usize>() <= budget {
            words
        } else {
            natural.iter().map(|&w| w.min(floor.max(1))).collect()
        };
        let spare = budget.checked_sub(min.iter().sum())?;
        let want: Vec<usize> = natural.iter().zip(&min).map(|(n, m)| n - m).collect();
        let total = want.iter().sum::<usize>().max(1);
        let mut widths: Vec<usize> = min
            .iter()
            .zip(&want)
            .map(|(m, w)| m + w * spare / total)
            .collect();
        // Rounding leaves a few columns over: give them to whoever is
        // furthest short of its natural width.
        let mut left = budget - widths.iter().sum::<usize>();
        while left > 0 {
            let Some(c) = (0..widths.len())
                .filter(|&c| widths[c] < natural[c])
                .max_by_key(|&c| natural[c] - widths[c])
            else {
                break;
            };
            widths[c] += 1;
            left -= 1;
        }
        Some(widths)
    }

    /// The table as lines of text inside box rules, with each cell wrapped
    /// to its column's width.
    pub fn text(
        &self,
        widths: &[usize],
        styled: impl Fn(Style, &InlineStyle) -> Style,
    ) -> (Vec<Vec<Span<'static>>>, Stats) {
        let border = Style::new().fg(Color::DarkGray);
        let rule = |left: &str, mid: &str, right: &str| {
            let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            vec![Span::styled(
                format!("{left}{}{right}", parts.join(mid)),
                border,
            )]
        };
        let mut stats = Stats::default();
        let mut lines = vec![rule("┌", "┬", "┐")];
        for (r, row) in self.rows.iter().enumerate() {
            let base = if r == 0 {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let cells: Vec<Wrapped> = row
                .iter()
                .enumerate()
                .map(|(c, cell)| wrap(cell, widths[c]))
                .collect();
            let height = cells.iter().map(|w| w.lines.len()).max().unwrap_or(1);
            if r > 0 {
                stats.heights.push(height);
                stats.soft += cells.iter().map(|w| w.soft).sum::<usize>();
                stats.hard += cells.iter().map(|w| w.hard).sum::<usize>();
            }
            for k in 0..height {
                let mut line = Vec::new();
                for (c, cell) in cells.iter().enumerate() {
                    let edge = if c == 0 { "│ " } else { " │ " };
                    line.push(Span::styled(edge, border));
                    let empty = Vec::new();
                    let part = cell.lines.get(k).unwrap_or(&empty);
                    let extra = widths[c].saturating_sub(text_width(part));
                    let (before, after) = padding(self.aligns[c], extra);
                    line.push(Span::raw(" ".repeat(before)));
                    for (text, style) in part {
                        line.push(Span::styled(text.clone(), styled(base, style)));
                    }
                    line.push(Span::raw(" ".repeat(after)));
                }
                line.push(Span::styled(" │", border));
                lines.push(line);
            }
            if r == 0 && self.rows.len() > 1 {
                lines.push(rule("├", "┼", "┤"));
            }
        }
        lines.push(rule("└", "┴", "┘"));
        (lines, stats)
    }
}

impl Table {
    /// Each body row as a card: a rule, then one `Header: value` line per
    /// cell, the value wrapped under itself. Empty cells are left out.
    pub fn cards(
        &self,
        width: usize,
        styled: impl Fn(Style, &InlineStyle) -> Style,
    ) -> (Vec<Vec<Span<'static>>>, Stats) {
        let border = Style::new().fg(Color::DarkGray);
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let heads: Vec<String> = self.rows[0]
            .iter()
            .map(|cell| cell.iter().map(|(t, _)| t.as_str()).collect())
            .collect();
        // Headers line up when they are short enough to.
        let label = heads
            .iter()
            .map(|h| h.width())
            .max()
            .unwrap_or(0)
            .min(width / 3);
        let mut stats = Stats::default();
        let mut lines = Vec::new();
        for row in &self.rows[1..] {
            lines.push(vec![Span::styled("─".repeat(width), border)]);
            let mut height = 0;
            for (c, cell) in row.iter().enumerate() {
                if cell.is_empty() {
                    continue;
                }
                let head = &heads[c];
                let indent = label.max(head.width().min(label)) + 2;
                let wrapped = wrap(cell, width.saturating_sub(indent).max(1));
                stats.soft += wrapped.soft;
                stats.hard += wrapped.hard;
                for (k, part) in wrapped.lines.iter().enumerate() {
                    let mut line = if k == 0 {
                        let pad = label.saturating_sub(head.width());
                        vec![Span::styled(
                            format!("{head}:{}", " ".repeat(pad + 1)),
                            bold,
                        )]
                    } else {
                        vec![Span::raw(" ".repeat(indent))]
                    };
                    for (text, style) in part {
                        line.push(Span::styled(text.clone(), styled(Style::default(), style)));
                    }
                    lines.push(line);
                    height += 1;
                }
            }
            stats.heights.push(height);
        }
        lines.push(vec![Span::styled("─".repeat(width), border)]);
        (lines, stats)
    }
}

/// Columns taken by borders and padding around columns of these widths.
fn frame(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + 3 * widths.len() + 1
}

/// Break a cell into lines no wider than `width`, at spaces where it can.
/// A word longer than the width breaks after punctuation such as `/` when
/// that keeps at least half the line, or else at the width.
pub fn wrap(cell: &Cell, width: usize) -> Wrapped {
    let width = width.max(1);
    // Each character with the index of the run it came from.
    let chars = cell
        .iter()
        .enumerate()
        .flat_map(|(i, (text, _))| text.chars().map(move |c| (c, i)));
    // Words, each with the run of the space before it.
    type Word = (Option<usize>, Vec<(char, usize)>);
    let mut words: Vec<Word> = Vec::new();
    let mut sep = None;
    let mut word = Vec::new();
    for (c, i) in chars {
        if c == ' ' {
            if !word.is_empty() {
                words.push((sep, std::mem::take(&mut word)));
            }
            sep = Some(i);
        } else {
            word.push((c, i));
        }
    }
    if !word.is_empty() {
        words.push((sep, word));
    }

    let mut lines: Vec<Vec<(char, usize)>> = vec![Vec::new()];
    let mut used = 0;
    let (mut soft, mut hard) = (0, 0);
    for (sep, word) in words {
        let w = chars_width(&word);
        if used > 0 && used + 1 + w <= width {
            let line = lines.last_mut().unwrap();
            line.push((' ', sep.unwrap_or(word[0].1)));
            line.extend(word);
            used += 1 + w;
            continue;
        }
        if used > 0 {
            lines.push(Vec::new());
        }
        let mut rest = &word[..];
        while chars_width(rest) > width {
            let (cut, at_punct) = break_point(rest, width);
            if at_punct {
                soft += 1;
            } else {
                hard += 1;
            }
            lines.last_mut().unwrap().extend_from_slice(&rest[..cut]);
            lines.push(Vec::new());
            rest = &rest[cut..];
        }
        lines.last_mut().unwrap().extend_from_slice(rest);
        used = chars_width(rest);
    }

    let lines = lines
        .into_iter()
        .map(|line| {
            let mut runs: Cell = Vec::new();
            for (c, i) in line {
                match runs.last_mut() {
                    Some((s, style)) if *style == cell[i].1 => s.push(c),
                    _ => runs.push((c.to_string(), cell[i].1.clone())),
                }
            }
            runs
        })
        .collect();
    Wrapped { lines, soft, hard }
}

/// Where to cut a word too wide for `width`: how many characters go on this
/// line, and whether the cut is after punctuation.
fn break_point(word: &[(char, usize)], width: usize) -> (usize, bool) {
    let mut fit = 0;
    let mut used = 0;
    for (k, (c, _)) in word.iter().enumerate() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        fit = k + 1;
    }
    let fit = fit.max(1);
    let punct = word[..fit]
        .iter()
        .rposition(|(c, _)| BREAK_AFTER.contains(*c))
        .map(|p| p + 1);
    match punct {
        Some(p) if p * 2 >= fit && p < word.len() => (p, true),
        _ => (fit, false),
    }
}

fn chars_width(chars: &[(char, usize)]) -> usize {
    chars.iter().map(|(c, _)| c.width().unwrap_or(0)).sum()
}

/// Go in or out of a span that nests. Returns whether still inside one.
fn nest(depth: &mut u32, event: &Event) -> bool {
    if matches!(event, Event::Start(_)) {
        *depth += 1;
    } else {
        *depth = depth.saturating_sub(1);
    }
    *depth > 0
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

/// `Canvas::fill`, which smd no longer needs since tables stopped being
/// images.
trait Fill {
    fn fill(&mut self, x: i64, y: i64, w: i64, h: i64, colour: Rgba);
}

impl Fill for Canvas {
    fn fill(&mut self, x: i64, y: i64, w: i64, h: i64, colour: Rgba) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, colour, 1.0);
            }
        }
    }
}

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

/// Draw the table, wrapping each cell at `cap` characters when given.
pub fn draw(table: &Table, theme: Theme, cap: Option<usize>) -> Result<Canvas, String> {
    let fonts = raster::fonts()?;
    let chars = table.rows.iter().flatten().flatten();
    let mut chars = chars.flat_map(|(text, _)| text.chars());
    if let Some(c) = chars.find(|&c| !c.is_whitespace() && fonts.regular.glyph_id(c).0 == 0) {
        return Err(format!("no glyph for {c:?}"));
    }
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

    // Every cell as its lines.
    let cells: Vec<Vec<Vec<Cell>>> = table
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| match cap {
                    Some(cap) => wrap(cell, cap).lines,
                    None => vec![cell.clone()],
                })
                .collect()
        })
        .collect();

    let pad = (SIZE * 0.5).round();
    let line = 2.0;
    let line_height = (SIZE * 1.3).round();
    let inset = (SIZE * 0.15).round();
    let heights: Vec<f32> = cells
        .iter()
        .map(|row| {
            let n = row.iter().map(Vec::len).max().unwrap_or(1).max(1);
            n as f32 * line_height + 2.0 * inset
        })
        .collect();
    let columns = table.aligns.len();
    let widths: Vec<f32> = (0..columns)
        .map(|c| {
            let lines = cells
                .iter()
                .enumerate()
                .flat_map(|(r, row)| row[c].iter().map(move |l| (r, l)));
            let widest = lines.map(|(r, l)| measure(r, l).ceil());
            widest.fold(SIZE * 0.5, f32::max) + 2.0 * pad
        })
        .collect();
    let width = line + widths.iter().map(|w| w + line).sum::<f32>();
    let height = line + heights.iter().map(|h| h + line).sum::<f32>();
    let (width, height) = (width as u32, height as u32);
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!("table too large: {width}x{height} px"));
    }

    let mut canvas = Canvas::new(width, height);
    let scaled = fonts.regular.as_scaled(PxScale::from(SIZE));
    let (ascent, descent) = (scaled.ascent(), scaled.descent());
    let mut top = line;
    for (r, row) in cells.iter().enumerate() {
        if r == 0 {
            let w = i64::from(width) - 2;
            canvas.fill(1, 1, w, (heights[0] + line) as i64, palette.header);
        }
        let mut left = line;
        for (c, cell) in row.iter().enumerate() {
            for (k, part) in cell.iter().enumerate() {
                let line_top = top + inset + k as f32 * line_height;
                let baseline =
                    line_top + ((line_height - (ascent - descent)) / 2.0 + ascent).round();
                let extra = widths[c] - 2.0 * pad - measure(r, part);
                let before = match table.aligns[c] {
                    Alignment::Right => extra,
                    Alignment::Center => (extra / 2.0).floor(),
                    Alignment::Left | Alignment::None => 0.0,
                };
                let mut x = left + pad + before;
                for (text, style) in part {
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
                        let (x0, w) = ((x - 4.0) as i64, (run + 8.0) as i64);
                        let h = line_height as i64;
                        canvas.fill(x0, line_top as i64, w, h, palette.code_back);
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
            }
            left += widths[c] + line;
        }
        top += heights[r] + line;
    }

    let l = line as i64;
    let mut y = 0.0;
    for r in 0..=cells.len() {
        canvas.fill(0, y as i64, i64::from(width), l, palette.rule);
        y += heights.get(r).copied().unwrap_or(0.0) + line;
    }
    let mut x = 0.0;
    for c in 0..=columns {
        canvas.fill(x as i64, 0, l, i64::from(height), palette.rule);
        x += widths.get(c).copied().unwrap_or(0.0) + line;
    }
    Ok(canvas)
}
