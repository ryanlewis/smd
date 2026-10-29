//! Tables, drawn as text. A table wider than the view wraps its cells, and
//! one with too many columns to wrap well is drawn as cards: a block of
//! `Header: value` lines per row.

use pulldown_cmark::{Alignment, Event, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::doc::{InlineStyle, Link};

/// The longest word a column is widened to hold whole. A longer one, such as
/// a URL, breaks.
const WORD_CAP: usize = 30;

/// The narrowest a column is made when the longest words do not all fit,
/// unless its text is shorter.
const FLOOR: usize = 12;

/// The most words wrapped cells may break mid-word before cards are used.
const MAX_HARD: usize = 2;

/// Characters a word too long for its column may break after. A break
/// there does not count against `MAX_HARD`.
const BREAK_AFTER: &str = "/-_.,:=";

/// The text of one cell, in runs of one style each.
type Cell = Vec<(String, InlineStyle)>;

/// Characters, each with the index of the run it came from.
type Chars = Vec<(char, usize)>;

/// Lines of styled text.
type Lines = Vec<Vec<Span<'static>>>;

#[derive(Debug, PartialEq)]
pub struct Table {
    aligns: Vec<Alignment>,
    /// The header row first. Every row has one cell per column.
    rows: Vec<Vec<Cell>>,
}

impl Table {
    pub fn parse(source: &str) -> Table {
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_MATH
            | Options::ENABLE_WIKILINKS;
        let mut aligns = Vec::new();
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        let mut style = InlineStyle::default();
        // How deep in emphasis, strong and strikethrough spans, which nest.
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
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    ..
                }) => {
                    style.link = Some(Link {
                        dest: dest_url.to_string(),
                        wiki: matches!(link_type, LinkType::WikiLink { .. }),
                    });
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

    /// The table drawn in `avail` columns: as it is when it fits, with its
    /// cells wrapped when that breaks few words and takes no more lines than
    /// cards, and as cards otherwise. `styled` turns a cell's inline style into a
    /// terminal style on top of a base style.
    pub fn lines(
        &self,
        avail: usize,
        styled: impl Fn(Style, &InlineStyle) -> Style + Copy,
    ) -> Lines {
        let avail = avail.max(1);
        let natural = self.natural();
        if frame(&natural) <= avail {
            return self.grid(&natural, styled).lines;
        }
        let cards = self.cards(avail, styled);
        if let Some(widths) = self.fit(avail) {
            let grid = self.grid(&widths, styled);
            // Compared without their rules, which would favour cards on a
            // short table: a grid has 3 or 4 and cards one per row.
            let body = self.rows.len() - 1;
            let card_rules = body.max(1) + 1;
            let grid_text = grid.lines.len() - grid.rules;
            if grid.hard <= MAX_HARD && grid_text <= cards.len().saturating_sub(card_rules) {
                return grid.lines;
            }
        }
        cards
    }

    /// The width of each column with nothing wrapped.
    fn natural(&self) -> Vec<usize> {
        self.per_column(|cell| text_width(cell))
    }

    /// The widest word in each column, at most `WORD_CAP`.
    fn words(&self) -> Vec<usize> {
        self.per_column(|cell| {
            let text: String = cell.iter().map(|(t, _)| t.as_str()).collect();
            let widest = text.split(' ').map(UnicodeWidthStr::width).max();
            widest.unwrap_or(0).min(WORD_CAP)
        })
    }

    /// The most `measure` gives for any cell in each column, at least 1.
    fn per_column(&self, measure: impl Fn(&Cell) -> usize) -> Vec<usize> {
        (0..self.aligns.len())
            .map(|c| {
                let widest = self.rows.iter().map(|row| measure(&row[c])).max();
                widest.unwrap_or(0).max(1)
            })
            .collect()
    }

    /// Column widths that fit the table in `avail` columns, or `None` when
    /// even narrow columns do not. Each column first gets room for its
    /// longest word, so words stay whole; when those do not all fit, it gets
    /// `FLOOR` or its natural width, whichever is less. The rest of the space
    /// goes to each column in proportion to how much more it wants.
    fn fit(&self, avail: usize) -> Option<Vec<usize>> {
        let natural = self.natural();
        let budget = avail.checked_sub(frame(&vec![0; natural.len()]))?;
        if natural.iter().sum::<usize>() <= budget {
            return Some(natural);
        }
        let words = self.words();
        let min = if words.iter().sum::<usize>() <= budget {
            words
        } else {
            natural.iter().map(|&w| w.min(FLOOR)).collect()
        };
        let spare = budget.checked_sub(min.iter().sum())?;
        let want: Vec<usize> = natural.iter().zip(&min).map(|(n, m)| n - m).collect();
        let total = want.iter().sum::<usize>().max(1);
        let mut widths: Vec<usize> = min
            .iter()
            .zip(&want)
            .map(|(m, w)| m + w * spare / total)
            .collect();
        // Rounding down leaves a few columns over: give them out one at a
        // time to whichever column is furthest short of its natural width.
        for _ in 0..budget - widths.iter().sum::<usize>() {
            let short = (0..widths.len()).max_by_key(|&c| natural[c] - widths[c]);
            match short {
                Some(c) if widths[c] < natural[c] => widths[c] += 1,
                _ => break,
            }
        }
        Some(widths)
    }

    /// The table inside box rules, each cell wrapped to its column's width,
    /// and how many words were broken mid-word.
    fn grid(&self, widths: &[usize], styled: impl Fn(Style, &InlineStyle) -> Style) -> Grid {
        let border = Style::new().fg(Color::DarkGray);
        let rule = |left: &str, fill: &str, mid: &str, right: &str| {
            let parts: Vec<String> = widths.iter().map(|w| fill.repeat(w + 2)).collect();
            vec![Span::styled(
                format!("{left}{}{right}", parts.join(mid)),
                border,
            )]
        };
        let rows: Vec<Vec<Wrapped>> = self
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .zip(widths)
                    .map(|(cell, &w)| wrap(cell, w))
                    .collect()
            })
            .collect();
        let height = |cells: &[Wrapped]| cells.iter().map(|w| w.lines.len()).max().unwrap_or(1);
        // Rows that take several lines run together, so a dashed rule marks
        // where each ends. A table whose rows are one line each stays compact.
        let separate = rows[1..].iter().any(|cells| height(cells) > 1);
        let mut hard = 0;
        let mut lines = vec![rule("┌", "─", "┬", "┐")];
        for (r, cells) in rows.iter().enumerate() {
            let base = if r == 0 {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            hard += cells.iter().map(|w| w.hard).sum::<usize>();
            for k in 0..height(cells) {
                let mut line = Vec::new();
                for (c, cell) in cells.iter().enumerate() {
                    let edge = if c == 0 { "│ " } else { " │ " };
                    line.push(Span::styled(edge, border));
                    let part = cell.lines.get(k).map_or(&[][..], Vec::as_slice);
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
            if r + 1 < rows.len() {
                if r == 0 {
                    lines.push(rule("├", "─", "┼", "┤"));
                } else if separate {
                    lines.push(rule("├", "╌", "┼", "┤"));
                }
            }
        }
        lines.push(rule("└", "─", "┴", "┘"));
        let rules =
            2 + if rows.len() > 1 { 1 } else { 0 } + if separate { rows.len() - 2 } else { 0 };
        Grid { lines, hard, rules }
    }

    /// Each body row as a card between rules: a `Header: value` line for
    /// each cell that has text, the value wrapped under itself. A table with
    /// no body shows its header as one card.
    fn cards(&self, width: usize, styled: impl Fn(Style, &InlineStyle) -> Style + Copy) -> Lines {
        let rule = vec![Span::styled(
            "─".repeat(width),
            Style::new().fg(Color::DarkGray),
        )];
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let heads = &self.rows[0];
        if self.rows.len() == 1 {
            let mut lines = vec![rule.clone()];
            for head in heads.iter().filter(|h| !h.is_empty()) {
                lines.extend(spans(&wrap(head, width).lines, bold, styled));
            }
            lines.push(rule);
            return lines;
        }
        // Values line up after the headers, unless a header is so long that
        // lining up would leave the values too little room.
        let label = heads
            .iter()
            .map(|h| text_width(h))
            .max()
            .unwrap_or(0)
            .min(width / 3);
        let indent = label + 2;
        let mut lines = Vec::new();
        for row in &self.rows[1..] {
            lines.push(rule.clone());
            for (head, cell) in heads.iter().zip(row) {
                if cell.is_empty() {
                    continue;
                }
                let value = wrap(cell, width.saturating_sub(indent));
                let mut values = spans(&value.lines, Style::default(), styled).into_iter();
                let head_width = text_width(head) + 1;
                if head_width <= label + 1 {
                    let mut line = spans(std::slice::from_ref(head), bold, styled).remove(0);
                    let pad = " ".repeat(indent - head_width);
                    line.push(Span::styled(format!(":{pad}"), bold));
                    line.extend(values.next().unwrap_or_default());
                    lines.push(line);
                } else {
                    // The header on lines of its own, the value under it.
                    let mut heading =
                        spans(&wrap(head, width.saturating_sub(1)).lines, bold, styled);
                    if let Some(last) = heading.last_mut() {
                        last.push(Span::styled(":", bold));
                    }
                    lines.extend(heading);
                }
                for value in values {
                    let mut line = vec![Span::raw(" ".repeat(indent))];
                    line.extend(value);
                    lines.push(line);
                }
            }
        }
        lines.push(rule);
        lines
    }
}

/// Lines of cell text as spans on top of `base`.
fn spans(lines: &[Cell], base: Style, styled: impl Fn(Style, &InlineStyle) -> Style) -> Lines {
    let line = |runs: &Cell| {
        let runs = runs.iter();
        runs.map(|(text, style)| Span::styled(text.clone(), styled(base, style)))
            .collect()
    };
    lines.iter().map(line).collect()
}

/// Columns taken by box rules and padding around columns of these widths.
fn frame(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + 3 * widths.len() + 1
}

/// A table drawn inside box rules.
struct Grid {
    lines: Lines,
    /// Words broken mid-word.
    hard: usize,
    /// How many of the lines are rules.
    rules: usize,
}

/// A cell broken into lines.
struct Wrapped {
    lines: Vec<Cell>,
    /// Words broken mid-word: not at a space or after `BREAK_AFTER`.
    hard: usize,
}

/// Break a cell into lines no wider than `width`, at spaces where it can.
/// A word wider than that breaks after punctuation from `BREAK_AFTER` when
/// that keeps at least half the line, and at the width otherwise. Spaces
/// at a break are dropped; spaces between words on a line are kept.
fn wrap(cell: &Cell, width: usize) -> Wrapped {
    let width = width.max(1);
    // Each character with the index of the run it came from, as words,
    // each with the spaces before it.
    let mut words: Vec<(Chars, Chars)> = Vec::new();
    let mut gap = Vec::new();
    let mut word = Vec::new();
    for (i, (text, _)) in cell.iter().enumerate() {
        for c in text.chars() {
            if c == ' ' {
                if !word.is_empty() {
                    words.push((std::mem::take(&mut gap), std::mem::take(&mut word)));
                }
                gap.push((c, i));
            } else {
                word.push((c, i));
            }
        }
    }
    if !word.is_empty() {
        words.push((gap, word));
    }

    let mut lines: Vec<Vec<(char, usize)>> = vec![Vec::new()];
    let mut used = 0;
    let mut hard = 0;
    for (gap, word) in words {
        let w = chars_width(&word);
        if used > 0 && used + gap.len() + w <= width {
            let line = lines.last_mut().unwrap();
            line.extend(gap.iter().chain(&word));
            used += gap.len() + w;
            continue;
        }
        if used > 0 {
            lines.push(Vec::new());
        }
        let mut rest = &word[..];
        while chars_width(rest) > width {
            let (cut, soft) = break_point(rest, width);
            hard += usize::from(!soft);
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
            let mut last = None;
            for (c, i) in line {
                match runs.last_mut() {
                    Some((s, _)) if last == Some(i) => s.push(c),
                    _ => runs.push((c.to_string(), cell[i].1.clone())),
                }
                last = Some(i);
            }
            runs
        })
        .collect();
    Wrapped { lines, hard }
}

/// Where to break a word too wide for `width`: how many characters stay on
/// this line, and whether the break is after punctuation.
fn break_point(word: &[(char, usize)], width: usize) -> (usize, bool) {
    let mut fit = 0;
    let mut used = 0;
    for (c, _) in word {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        fit += 1;
    }
    // At least one character, so a character wider than the line still moves on.
    let fit = fit.max(1);
    let after = word[..fit]
        .iter()
        .rposition(|(c, _)| BREAK_AFTER.contains(*c))
        .map(|p| p + 1);
    match after {
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

fn text_width(cell: &[(String, InlineStyle)]) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(table: &Table, avail: usize) -> Vec<String> {
        let lines = table.lines(avail, |base, _| base);
        let text = |line: &Vec<Span>| line.iter().map(|s| s.content.as_ref()).collect();
        lines.iter().map(text).collect()
    }

    fn cell(text: &str) -> Cell {
        vec![(text.to_string(), InlineStyle::default())]
    }

    fn wrapped(text: &str, width: usize) -> (Vec<String>, usize) {
        let w = wrap(&cell(text), width);
        let text = |line: &Cell| line.iter().map(|(t, _)| t.as_str()).collect();
        (w.lines.iter().map(text).collect(), w.hard)
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
            plain(&table, 80),
            vec![
                "┌───────┬───┐",
                "│ a     │ b │",
                "├───────┼───┤",
                "│ 1     │   │",
                "│ x | y │ 2 │",
                "└───────┴───┘",
            ]
        );
    }

    #[test]
    fn columns_follow_their_alignment() {
        let table =
            Table::parse("| left | mid | right |\n|:--|:-:|--:|\n| a | b | c |\n| é | 日本 | dd |");
        assert_eq!(
            plain(&table, 80),
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
    fn nested_spans_keep_the_outer_style() {
        let table = Table::parse("| *a _b_ c* |\n|---|");
        let emphasis = InlineStyle {
            emphasis: true,
            ..InlineStyle::default()
        };
        assert_eq!(table.rows[0][0], vec![("a b c".to_string(), emphasis)]);
    }

    #[test]
    fn cells_keep_strikethrough_math_html_and_wiki_links() {
        let table = Table::parse("| ~~a ~b~ c~~ d | $x$ <br> [[n]] |\n|---|---|");
        let plain = InlineStyle::default();
        let struck = InlineStyle {
            strikethrough: true,
            ..InlineStyle::default()
        };
        let math = InlineStyle {
            math: true,
            ..InlineStyle::default()
        };
        let wiki = InlineStyle {
            link: Some(Link {
                dest: "n".into(),
                wiki: true,
            }),
            ..InlineStyle::default()
        };
        assert_eq!(
            table.rows,
            vec![vec![
                vec![("a b c".into(), struck), (" d".into(), plain.clone())],
                vec![
                    ("x".into(), math),
                    (" <br> ".into(), plain),
                    ("n".into(), wiki),
                ],
            ]]
        );
    }

    #[test]
    fn text_before_a_table_is_dropped() {
        let table = Table::parse("<div>x</div>\n\n***\n\nsome\ntext $$y$$\n\n| a |\n|---|");
        assert_eq!(table.aligns, vec![Alignment::None]);
        assert_eq!(table.rows, vec![vec![cell("a")]]);
        let empty = Table::parse("just\ntext");
        assert_eq!(empty.aligns, vec![Alignment::None]);
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn the_header_is_bold() {
        let table = Table::parse("| h |\n|---|\n| b |");
        let lines = table.lines(80, |base, _| base);
        assert!(lines[1][2].style.add_modifier.contains(Modifier::BOLD));
        assert!(!lines[3][2].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_wide_table_wraps_its_cells() {
        let table = Table::parse("| name | note |\n|---|---|\n| a | one two three four |");
        assert_eq!(
            plain(&table, 22),
            vec![
                "┌──────┬─────────────┐",
                "│ name │ note        │",
                "├──────┼─────────────┤",
                "│ a    │ one two     │",
                "│      │ three four  │",
                "└──────┴─────────────┘",
            ]
        );
    }

    #[test]
    fn a_dashed_rule_separates_rows_only_when_one_wraps() {
        let md =
            "| name | note |\n|---|---|\n| a | one two three four |\n| b | five |\n| c | six |";
        assert_eq!(
            plain(&Table::parse(md), 22),
            vec![
                "┌──────┬─────────────┐",
                "│ name │ note        │",
                "├──────┼─────────────┤",
                "│ a    │ one two     │",
                "│      │ three four  │",
                "├╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌┤",
                "│ b    │ five        │",
                "├╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌┤",
                "│ c    │ six         │",
                "└──────┴─────────────┘",
            ]
        );
        // Wide enough for one line a row: no rules between rows.
        let lines = plain(&Table::parse(md), 40);
        assert_eq!(lines.len(), 7);
        assert!(lines.iter().all(|l| !l.contains('╌')), "{lines:?}");
    }

    #[test]
    fn cells_wrap_at_spaces_and_keep_spaces_inside_a_line() {
        assert_eq!(
            wrapped("one two three", 7),
            (vec!["one two".into(), "three".into()], 0)
        );
        assert_eq!(wrapped("a  b c", 4), (vec!["a  b".into(), "c".into()], 0));
        assert_eq!(wrapped("", 4), (vec!["".into()], 0));
    }

    #[test]
    fn a_long_word_breaks_after_punctuation_or_else_mid_word() {
        assert_eq!(
            wrapped("~/.gradle/caches/modules", 12),
            (
                vec!["~/.gradle/".into(), "caches/".into(), "modules".into()],
                0
            )
        );
        assert_eq!(
            wrapped("abcdefghij", 4),
            (vec!["abcd".into(), "efgh".into(), "ij".into()], 2)
        );
        // A break so early it wastes most of the line is not worth it.
        assert_eq!(
            wrapped("a/bcdefghij", 8),
            (vec!["a/bcdefg".into(), "hij".into()], 1)
        );
    }

    #[test]
    fn a_wrapped_line_keeps_its_runs_and_styles() {
        let code = InlineStyle {
            code: true,
            ..InlineStyle::default()
        };
        let cell = vec![
            ("see ".to_string(), InlineStyle::default()),
            ("a b".to_string(), code.clone()),
        ];
        let w = wrap(&cell, 5);
        assert_eq!(
            w.lines,
            vec![
                vec![
                    ("see ".to_string(), InlineStyle::default()),
                    ("a".to_string(), code.clone())
                ],
                vec![("b".to_string(), code)],
            ]
        );
    }

    #[test]
    fn every_column_gets_room_for_its_longest_word_first() {
        // Sharing by length alone would squeeze the MAC address column.
        let table = Table::parse(
            "| mac | vendor |\n|---|---|\n| 62:c3:61:e4:9c:de | a vendor with a long name here |",
        );
        let widths = table.fit(40).unwrap();
        assert_eq!(widths[0], 17);
        assert_eq!(frame(&widths), 40);
        // Natural widths when they fit.
        assert_eq!(table.fit(200).unwrap(), table.natural());
        // Not even the floors fit.
        assert_eq!(
            Table::parse("| aaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbb |\n|---|---|").fit(20),
            None
        );
    }

    #[test]
    fn too_many_columns_become_cards() {
        let table = Table::parse(
            "| tool | isolation mechanism | credentials |\n|---|---|---|\n\
             | srt | seatbelt on macOS | runs as you |\n| lima | full VM | |",
        );
        assert_eq!(
            plain(&table, 30),
            vec![
                "──────────────────────────────",
                "tool:       srt",
                "isolation mechanism:",
                "            seatbelt on macOS",
                "credentials:",
                "            runs as you",
                "──────────────────────────────",
                "tool:       lima",
                "isolation mechanism:",
                "            full VM",
                "──────────────────────────────",
            ]
        );
    }

    #[test]
    fn a_table_with_only_a_header_shows_it_as_one_card() {
        let table = Table::parse("| aaaaaaaa | bbbbbbbb | cccccccc |\n|---|---|---|");
        assert_eq!(
            plain(&table, 10),
            vec![
                "──────────",
                "aaaaaaaa",
                "bbbbbbbb",
                "cccccccc",
                "──────────"
            ]
        );
    }

    #[test]
    fn no_line_is_wider_than_the_view() {
        let md = "| a | b | c | d |\n|---|---|---|---|\n\
                  | https://example.org/a/very/long/path | x y z | `code span` | 1 |";
        let table = Table::parse(md);
        for avail in [8, 20, 30, 45, 60, 100] {
            for line in plain(&table, avail) {
                assert!(line.width() <= avail, "{avail}: {line:?}");
            }
        }
    }
}
