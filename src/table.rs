//! Tables, drawn as text. A table wider than the view wraps its cells, and
//! one with too many columns to wrap well is drawn as cards: a block of
//! `Header: value` lines per row.

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::doc::{Embed, InlineStyle, Link, embed, is_wiki, link_dest};
use crate::render::{LinkSpot, breaks_at};

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

/// A span, and the link its text is part of, by its index in the table's
/// links.
type Piece = (Span<'static>, Option<usize>);

/// Lines of styled text.
type Lines = Vec<Vec<Piece>>;

#[derive(Debug, PartialEq)]
pub struct Table {
    aligns: Vec<Alignment>,
    /// The header row first. Every row has one cell per column.
    rows: Vec<Vec<Cell>>,
    /// The links in the cells, in order. Runs next to each other in a cell
    /// with the same link are one link. A link with nothing to see is left
    /// out, so the links are the same however the table is drawn.
    links: Vec<Link>,
    run_links: RunLinks,
}

/// For each row, cell and run, its link in the table's links.
type RunLinks = Vec<Vec<Vec<Option<usize>>>>;

/// A table drawn as lines of text.
pub struct Drawn {
    pub lines: Vec<Vec<Span<'static>>>,
    /// Each link, and the line and columns of each piece of its text.
    pub links: Vec<LinkSpot>,
}

impl Table {
    pub fn parse(source: &str) -> Table {
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_MATH
            | Options::ENABLE_WIKILINKS;
        let mut builder = Builder::default();
        for event in Parser::new_ext(source, options) {
            builder.event(event);
        }
        builder.finish()
    }

    /// The table drawn in `avail` columns: as it is when it fits, with its
    /// cells wrapped when that breaks few words and takes no more lines than
    /// cards, and as cards otherwise. `styled` turns a cell's inline style into a
    /// terminal style on top of a base style.
    pub fn lines(
        &self,
        avail: usize,
        styled: impl Fn(Style, &InlineStyle) -> Style + Copy,
    ) -> Drawn {
        self.drawn(self.layout(avail, styled))
    }

    /// The lines `lines` draws, as pieces.
    fn layout(&self, avail: usize, styled: impl Fn(Style, &InlineStyle) -> Style + Copy) -> Lines {
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
            let widest = text.split(breaks_at).map(UnicodeWidthStr::width).max();
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
            vec![(
                Span::styled(format!("{left}{}{right}", parts.join(mid)), border),
                None,
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
            let pieces: Vec<Lines> = cells
                .iter()
                .enumerate()
                .map(|(c, cell)| pieces(cell, &self.run_links[r][c], base, &styled))
                .collect();
            for k in 0..height(cells) {
                let mut line = Vec::new();
                for (c, cell) in cells.iter().enumerate() {
                    let edge = if c == 0 { "│ " } else { " │ " };
                    line.push((Span::styled(edge, border), None));
                    let part = cell.lines.get(k).map_or(&[][..], Vec::as_slice);
                    let extra = widths[c].saturating_sub(text_width(part));
                    let (before, after) = padding(self.aligns[c], extra);
                    line.push((Span::raw(" ".repeat(before)), None));
                    line.extend(pieces[c].get(k).cloned().unwrap_or_default());
                    line.push((Span::raw(" ".repeat(after)), None));
                }
                line.push((Span::styled(" │", border), None));
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
        let rule = vec![(
            Span::styled("─".repeat(width), Style::new().fg(Color::DarkGray)),
            None,
        )];
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let heads = &self.rows[0];
        if self.rows.len() == 1 {
            let mut lines = vec![rule.clone()];
            for (head, ids) in heads.iter().zip(&self.run_links[0]) {
                if !head.is_empty() {
                    lines.extend(pieces(&wrap(head, width), ids, bold, styled));
                }
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
        for (r, row) in self.rows.iter().enumerate().skip(1) {
            lines.push(rule.clone());
            for (c, (head, cell)) in heads.iter().zip(row).enumerate() {
                if cell.is_empty() {
                    continue;
                }
                let head_ids = &self.run_links[0][c];
                let value = wrap(cell, width.saturating_sub(indent));
                let value = pieces(&value, &self.run_links[r][c], Style::default(), styled);
                let mut values = value.into_iter();
                let head_width = text_width(head) + 1;
                if head_width <= label + 1 {
                    let whole = Wrapped::whole(head);
                    let mut line = pieces(&whole, head_ids, bold, styled).remove(0);
                    let pad = " ".repeat(indent - head_width);
                    line.push((Span::styled(format!(":{pad}"), bold), None));
                    line.extend(values.next().unwrap_or_default());
                    lines.push(line);
                } else {
                    // The header on lines of its own, the value under it.
                    let head = wrap(head, width.saturating_sub(1));
                    let mut heading = pieces(&head, head_ids, bold, styled);
                    if let Some(last) = heading.last_mut() {
                        last.push((Span::styled(":", bold), None));
                    }
                    lines.extend(heading);
                }
                for value in values {
                    let mut line = vec![(Span::raw(" ".repeat(indent)), None)];
                    line.extend(value);
                    lines.push(line);
                }
            }
        }
        lines.push(rule);
        lines
    }
}

impl Table {
    /// The spans of `lines`, and the columns each link's pieces take.
    fn drawn(&self, lines: Lines) -> Drawn {
        let mut links: Vec<LinkSpot> = self
            .links
            .iter()
            .map(|link| LinkSpot {
                link: link.clone(),
                cells: Vec::new(),
            })
            .collect();
        let lines = lines
            .into_iter()
            .enumerate()
            .map(|(row, line)| {
                let mut col = 0;
                let mut spans = Vec::with_capacity(line.len());
                for (span, link) in line {
                    let start = col;
                    col += span.width() as u16;
                    if let Some(i) = link {
                        let cells = &mut links[i].cells;
                        match cells.last_mut() {
                            Some((r, cols)) if *r == row && cols.end == start => cols.end = col,
                            _ => cells.push((row, start..col)),
                        }
                    }
                    spans.push(span);
                }
                spans
            })
            .collect();
        Drawn { lines, links }
    }
}

/// A wrapped cell's lines as pieces on top of `base`, given each of the
/// cell's runs' links.
fn pieces(
    cell: &Wrapped,
    run_links: &[Option<usize>],
    base: Style,
    styled: impl Fn(Style, &InlineStyle) -> Style,
) -> Lines {
    let line = |(runs, sources): (&Cell, &Vec<usize>)| {
        runs.iter()
            .zip(sources)
            .map(|((text, style), &i)| {
                let span = Span::styled(text.clone(), styled(base, style));
                (span, run_links.get(i).copied().flatten())
            })
            .collect()
    };
    cell.lines.iter().zip(&cell.sources).map(line).collect()
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
    /// For each run in `lines`, the run in the cell it came from.
    sources: Vec<Vec<usize>>,
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
            if breaks_at(c) {
                if !word.is_empty() {
                    words.push((std::mem::take(&mut gap), std::mem::take(&mut word)));
                }
                // As in `plain_spaces`, for a cell not read by `parse`.
                gap.push((' ', i));
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
        let space = chars_width(&gap);
        if used > 0 && used + space + w <= width {
            let line = lines.last_mut().unwrap();
            line.extend(gap.iter().chain(&word));
            used += space + w;
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

    let mut sources = Vec::with_capacity(lines.len());
    let lines = lines
        .into_iter()
        .map(|line| {
            let mut runs: Cell = Vec::new();
            let mut from = Vec::new();
            for (c, i) in line {
                match runs.last_mut() {
                    Some((s, _)) if from.last() == Some(&i) => s.push(c),
                    _ => {
                        runs.push((c.to_string(), cell[i].1.clone()));
                        from.push(i);
                    }
                }
            }
            sources.push(from);
            runs
        })
        .collect();
    Wrapped {
        lines,
        sources,
        hard,
    }
}

impl Wrapped {
    /// `cell` on one line as it is.
    fn whole(cell: &Cell) -> Wrapped {
        Wrapped {
            lines: vec![cell.clone()],
            sources: vec![(0..cell.len()).collect()],
            hard: 0,
        }
    }
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

/// A table read from parser events, one at a time.
#[derive(Default)]
struct Builder {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    /// The style of the text read next.
    style: InlineStyle,
    /// How deep in emphasis, strong and strikethrough spans, which nest.
    depth: [u32; 3],
    /// For each image started and not yet ended, whether it embeds a note.
    embeds: Vec<bool>,
}

impl Builder {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            event => {
                if let Some(run) = self.run(event) {
                    self.push(run);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Table(aligns) => self.aligns = aligns,
            Tag::TableHead | Tag::TableRow => self.rows.push(Vec::new()),
            Tag::TableCell => {
                if let Some(row) = self.rows.last_mut() {
                    row.push(Vec::new());
                }
            }
            Tag::Emphasis => self.style.emphasis = nest(&mut self.depth[0], true),
            Tag::Strong => self.style.strong = nest(&mut self.depth[1], true),
            Tag::Strikethrough => self.style.strikethrough = nest(&mut self.depth[2], true),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                let wiki = is_wiki(link_type);
                self.style.link = Some(Link {
                    dest: link_dest(&dest_url, wiki).into_owned(),
                    wiki,
                });
            }
            Tag::Image {
                link_type,
                dest_url,
                ..
            } => {
                // An embedded note is shown as a link to it, and an image as
                // its alt text, as in a paragraph.
                let wiki = is_wiki(link_type);
                match embed(&dest_url, wiki) {
                    Embed::Note(link) => {
                        self.embeds.push(true);
                        self.style.link = Some(link);
                    }
                    Embed::Image => {
                        self.embeds.push(false);
                        self.push(("[image: ".to_string(), self.style.clone()));
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Emphasis => self.style.emphasis = nest(&mut self.depth[0], false),
            TagEnd::Strong => self.style.strong = nest(&mut self.depth[1], false),
            TagEnd::Strikethrough => self.style.strikethrough = nest(&mut self.depth[2], false),
            TagEnd::Link => self.style.link = None,
            TagEnd::Image => {
                if self.embeds.pop() == Some(true) {
                    self.style.link = None;
                } else {
                    self.push(("]".to_string(), self.style.clone()));
                }
            }
            _ => {}
        }
    }

    /// The text an event adds to a cell, and its style.
    fn run(&self, event: Event) -> Option<(String, InlineStyle)> {
        let run = match event {
            Event::Text(t) | Event::InlineHtml(t) | Event::Html(t) => {
                (t.to_string(), self.style.clone())
            }
            Event::Code(t) => (
                t.to_string(),
                InlineStyle {
                    code: true,
                    ..self.style.clone()
                },
            ),
            Event::InlineMath(t) | Event::DisplayMath(t) => (
                t.to_string(),
                InlineStyle {
                    math: true,
                    ..self.style.clone()
                },
            ),
            Event::SoftBreak | Event::HardBreak => (" ".to_string(), self.style.clone()),
            _ => return None,
        };
        Some((plain_spaces(&run.0), run.1))
    }

    /// Add a run to the last cell, joined to its last run when their styles
    /// match. Text outside a cell is dropped.
    fn push(&mut self, (text, style): (String, InlineStyle)) {
        let Some(cell) = self.rows.last_mut().and_then(|row| row.last_mut()) else {
            return;
        };
        match cell.last_mut() {
            Some((s, last)) if *last == style => s.push_str(&text),
            _ => cell.push((text, style)),
        }
    }

    /// The table, with an alignment and a cell in every row for each column.
    fn finish(mut self) -> Table {
        let columns = self
            .aligns
            .len()
            .max(self.rows.iter().map(Vec::len).max().unwrap_or(0))
            .max(1);
        self.aligns.resize(columns, Alignment::None);
        for row in &mut self.rows {
            row.resize_with(columns, Vec::new);
        }
        let (links, run_links) = cell_links(&self.rows);
        Table {
            aligns: self.aligns,
            rows: self.rows,
            links,
            run_links,
        }
    }
}

/// The links in `rows`, and each run's link among them. Runs next to each
/// other in a cell with the same link are one link. A link with nothing
/// drawn whether the table is a grid or cards is left out: one whose text is
/// only spaces, where a cell's lines break, or one in the header of a column
/// with nothing in it, which no card shows.
fn cell_links(rows: &[Vec<Cell>]) -> (Vec<Link>, RunLinks) {
    // Each link found, and whether any of its text shows.
    let mut found: Vec<(Link, bool)> = Vec::new();
    let mut run_links: RunLinks = Vec::new();
    let unseen = |c: usize| rows.len() > 1 && rows[1..].iter().all(|row| row[c].is_empty());
    for (r, row) in rows.iter().enumerate() {
        let mut row_links = Vec::new();
        for (c, cell) in row.iter().enumerate() {
            let hidden = r == 0 && unseen(c);
            let mut ids: Vec<Option<usize>> = Vec::with_capacity(cell.len());
            for (text, style) in cell {
                let open = ids.last().copied().flatten();
                let id = style.link.as_ref().map(|link| match open {
                    Some(i) if found[i].0 == *link => i,
                    _ => {
                        found.push((link.clone(), false));
                        found.len() - 1
                    }
                });
                if let Some(i) = id {
                    let drawn = |c: char| !breaks_at(c) && c.width().unwrap_or(0) > 0;
                    found[i].1 |= !hidden && text.chars().any(drawn);
                }
                ids.push(id);
            }
            row_links.push(ids);
        }
        run_links.push(row_links);
    }
    // Each link's new index, if it is kept.
    let mut index = Vec::with_capacity(found.len());
    let mut links = Vec::new();
    for (link, shows) in found {
        index.push(shows.then_some(links.len()));
        if shows {
            links.push(link);
        }
    }
    for id in run_links.iter_mut().flatten().flatten() {
        *id = id.and_then(|i| index[i]);
    }
    (links, run_links)
}

/// Go in or out of a span that nests. Returns whether still inside one.
fn nest(depth: &mut u32, start: bool) -> bool {
    if start {
        *depth += 1;
    } else {
        *depth = depth.saturating_sub(1);
    }
    *depth > 0
}

fn text_width(cell: &[(String, InlineStyle)]) -> usize {
    cell.iter().map(|(text, _)| text.width()).sum()
}

/// `text` with each space a line may break at made an ordinary space, so a
/// tab is not drawn as nothing and a wide space not wider. Runs of spaces
/// are kept.
fn plain_spaces(text: &str) -> String {
    text.chars()
        .map(|c| if breaks_at(c) { ' ' } else { c })
        .collect()
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
        let lines = table.lines(avail, |base, _| base).lines;
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
    fn a_wiki_link_in_a_cell_drops_the_escape_before_its_alias() {
        let table = Table::parse("| [[Note\\|alias]] |\n|-|");
        let note = InlineStyle {
            link: Some(Link {
                dest: "Note".into(),
                wiki: true,
            }),
            ..InlineStyle::default()
        };
        assert_eq!(table.rows, vec![vec![vec![("alias".into(), note)]]]);
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
        let lines = table.lines(80, |base, _| base).lines;
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
    fn cells_wrap_at_any_space_but_a_no_break_one() {
        assert_eq!(wrapped("a\u{3000}b", 1), (vec!["a".into(), "b".into()], 0));
        assert_eq!(wrapped("a\tb", 1), (vec!["a".into(), "b".into()], 0));
        // Any breakable space is drawn as one ordinary space.
        assert_eq!(wrapped("a\u{3000}b", 3), (vec!["a b".into()], 0));
        assert_eq!(wrapped("a\tb", 3), (vec!["a b".into()], 0));
        assert_eq!(
            wrapped("10\u{a0}km x", 5),
            (vec!["10\u{a0}km".into(), "x".into()], 0)
        );
        let table = Table::parse("| aaaa\u{3000}bbbb | 10\u{a0}km |\n|-|-|");
        assert_eq!(table.words(), vec![4, 5]);
        let table = Table::parse("| a\tb | 10\u{a0}km |\n|-|-|");
        assert_eq!(
            plain(&table, 80),
            vec!["┌─────┬───────┐", "│ a b │ 10\u{a0}km │", "└─────┴───────┘",]
        );
    }

    #[test]
    fn a_card_header_with_a_tab_or_wide_space_lines_up() {
        for header in ["h\tx", "h\u{3000}x"] {
            let table = Table::parse(&format!("| {header} | bb | cc |\n|-|-|-|\n| 1 | 2 | 3 |"));
            let lines = plain(&table, 12);
            assert_eq!(lines[1..4], ["h x: 1", "bb:  2", "cc:  3"], "{header:?}");
        }
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
    fn images_and_note_embeds_in_a_cell_read_as_in_a_paragraph() {
        let table = Table::parse(
            "| ![a *b*](a.png) | ![[d.png]] | ![[Note]] | [![a](a.png)](u) |\n|-|-|-|-|",
        );
        let plain = InlineStyle::default();
        let emphasis = InlineStyle {
            emphasis: true,
            ..InlineStyle::default()
        };
        let note = InlineStyle {
            link: Some(Link {
                dest: "Note".into(),
                wiki: true,
            }),
            ..InlineStyle::default()
        };
        let outer = InlineStyle {
            link: Some("u".into()),
            ..InlineStyle::default()
        };
        assert_eq!(
            table.rows,
            vec![vec![
                vec![
                    ("[image: a ".into(), plain.clone()),
                    ("b".into(), emphasis),
                    ("]".into(), plain.clone()),
                ],
                vec![("[image: d.png]".into(), plain)],
                vec![("Note".into(), note)],
                vec![("[image: a]".into(), outer)],
            ]]
        );
    }

    #[test]
    fn an_embed_in_a_cell_drops_the_escape_before_its_alias() {
        let table = Table::parse("| ![[pic.png\\|200]] | ![[Note\\|alias]] |\n|-|-|");
        let note = InlineStyle {
            link: Some(Link {
                dest: "Note".into(),
                wiki: true,
            }),
            ..InlineStyle::default()
        };
        assert_eq!(
            table.rows,
            vec![vec![
                vec![("[image: 200]".into(), InlineStyle::default())],
                vec![("alias".into(), note)],
            ]]
        );
    }

    #[test]
    fn an_image_embed_with_a_fragment_in_a_cell_is_still_an_image() {
        let table = Table::parse("| ![[pic.png#frag]] |\n|-|");
        let image = vec![("[image: pic.png#frag]".into(), InlineStyle::default())];
        assert_eq!(table.rows, vec![vec![image]]);
    }

    #[test]
    fn an_embed_in_a_cell_drops_spaces_around_its_name() {
        let table = Table::parse("| ![[pic.png \\|300]] |\n|-|");
        let image = vec![("[image: 300]".into(), InlineStyle::default())];
        assert_eq!(table.rows, vec![vec![image]]);
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
