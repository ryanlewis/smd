//! Finding a term in the text as it is drawn: what the reader sees, code
//! blocks and table cells included, not the markdown source.

use std::ops::Range;

use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::doc::{Block, BlockKind, Document, Inline};
use crate::heading::SizedLine;
use crate::render::Layout;

/// One place the term is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The first line it is on.
    pub row: usize,
    /// Each line it is on, more than one where the text wrapped, with the
    /// columns it takes there. For a heading drawn as an image, each line
    /// of the image with no columns, as it has no text cells to mark.
    pub cells: Vec<(usize, Range<u16>)>,
}

impl Match {
    /// The lines it is on.
    pub fn rows(&self) -> Range<usize> {
        let last = self.cells.last().map_or(self.row, |(row, _)| *row);
        self.row..last + 1
    }

    /// Whether any of it is on the lines of `view`.
    pub fn seen(&self, view: &Range<usize>) -> bool {
        let rows = self.rows();
        rows.start < view.end && view.start < rows.end
    }
}

/// A search: the term, where it is drawn, and which match the reader is on.
#[derive(Debug, Default)]
pub struct Search {
    pub term: String,
    /// Whether the term is still being typed on the bottom row.
    pub typing: bool,
    pub matches: Vec<Match>,
    /// The match the reader last went to, by its index in `matches`.
    pub current: Option<usize>,
}

impl Search {
    /// Whether there is a term, so its matches are marked.
    pub fn active(&self) -> bool {
        !self.term.is_empty()
    }

    /// Find the term again, as in a new layout. The matches may have moved
    /// or changed in number, so the current one is the one `kept` names:
    /// the nth of the matches on the given rows, those of the block it was
    /// in, when it is still there.
    pub fn refresh(
        &mut self,
        layout: &Layout,
        doc: &Document,
        kept: Option<(Range<usize>, usize)>,
    ) {
        self.matches = find(&self.term, layout, doc);
        self.current = kept.and_then(|(rows, nth)| {
            let i = self.matches.partition_point(|m| m.row < rows.start) + nth;
            self.matches
                .get(i)
                .filter(|m| rows.contains(&m.row))
                .map(|_| i)
        });
    }

    /// The first match on or below row `top`, going round to the first of
    /// all when there is none. Gives its index and whether it went round.
    pub fn first_from(&self, top: usize) -> Option<(usize, bool)> {
        let row = |i: usize| self.matches[i].row;
        step_round(self.matches.len(), None, true, top..top, row)
    }

    /// The next or previous match, going round at the ends. From a match
    /// that is not in `view` at all, it is the first match from the top of
    /// the view or the last one above its bottom. Gives its index and whether
    /// it went round.
    pub fn step(&self, forward: bool, view: Range<usize>) -> Option<(usize, bool)> {
        let current = self.current.filter(|&i| self.matches[i].seen(&view));
        let row = |i: usize| self.matches[i].row;
        step_round(self.matches.len(), current, forward, view, row)
    }

    /// `step` taken `times` times: the first step is from the view, the rest
    /// from that match. Gives its index and whether it went round.
    pub fn step_by(
        &self,
        forward: bool,
        view: Range<usize>,
        times: usize,
    ) -> Option<(usize, bool)> {
        let (i, round) = self.step(forward, view)?;
        let (len, more) = (self.matches.len(), times.saturating_sub(1));
        Some(if forward {
            ((i + more % len) % len, round || more >= len - i)
        } else {
            ((i + len - more % len) % len, round || more > i)
        })
    }

    /// What the bottom row says about where the reader is, such as
    /// "match 3/17".
    pub fn status(&self, wrapped: Option<bool>) -> String {
        let (Some(i), false) = (self.current, self.matches.is_empty()) else {
            return format!("no matches for {}", self.term);
        };
        let at = format!("match {}/{}", i + 1, self.matches.len());
        match wrapped {
            Some(true) => format!("{at}, wrapped to the top"),
            Some(false) => format!("{at}, wrapped to the bottom"),
            None => at,
        }
    }

    /// The prompt shown while the term is typed, with how many matches it
    /// has so far.
    pub fn prompt(&self) -> String {
        match self.matches.len() {
            _ if self.term.is_empty() => "/".to_string(),
            0 => format!("/{}  no matches", self.term),
            1 => format!("/{}  1 match", self.term),
            n => format!("/{}  {n} matches", self.term),
        }
    }
}

/// The next or previous of `len` things in reading order, where `row` gives
/// the line each starts on, going round at the ends. From `current`, which
/// must be in `view`, it is the one after or before it. Without it, it is the
/// first on or below the top of the view or the last above its bottom. Gives
/// its index and whether it went round.
pub fn step_round(
    len: usize,
    current: Option<usize>,
    forward: bool,
    view: Range<usize>,
    row: impl Fn(usize) -> usize,
) -> Option<(usize, bool)> {
    if len == 0 {
        return None;
    }
    Some(match (current, forward) {
        (Some(i), true) => ((i + 1) % len, i + 1 == len),
        (Some(i), false) => ((i + len - 1) % len, i == 0),
        (None, true) => match (0..len).find(|&i| row(i) >= view.start) {
            Some(i) => (i, false),
            None => (0, true),
        },
        (None, false) => match (0..len).rev().find(|&i| row(i) < view.end) {
            Some(i) => (i, false),
            None => (len - 1, true),
        },
    })
}

/// Whether to ignore case for `term`: smartcase, so a capital in the term
/// makes the search match case.
fn ignore_case(term: &str) -> bool {
    !term.chars().any(char::is_uppercase)
}

/// The character compared for `c`: lower case when case is ignored. A
/// character that lowers to several keeps its first, so columns stay one to
/// one with what is drawn.
fn fold(c: char, ignore: bool) -> char {
    if ignore {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// Every place `term` is drawn in `layout`, in reading order. Lines of a
/// block drawn as an image are passed over, but a heading drawn as one
/// matches on its first line by its text. Where the text wrapped, the lines
/// are searched as one, with a space where the break dropped one.
pub fn find(term: &str, layout: &Layout, doc: &Document) -> Vec<Match> {
    if term.is_empty() {
        return Vec::new();
    }
    let ignore = ignore_case(term);
    let wanted: Vec<char> = term.chars().map(|c| fold(c, ignore)).collect();
    let images: Vec<_> = layout.blocks.iter().filter(|b| b.image.is_some()).collect();
    let mut out = Vec::new();
    let mut image = images.iter().peekable();
    let mut sized = layout.sized.iter().peekable();
    let mut joins = layout.joins.iter().peekable();
    // The text of the lines joined so far, and the rows drawn under the
    // last line of a heading drawn larger.
    let mut run: Vec<Drawn> = Vec::new();
    let mut under = 0..0;
    for (row, line) in layout.lines.iter().enumerate() {
        // Image blocks, sized lines and joins are in document order, so
        // their rows only go up.
        while image.next_if(|b| b.rows.end <= row).is_some() {}
        while sized.next_if(|l| l.row < row).is_some() {}
        while joins.next_if(|j| j.row < row).is_some() {}
        if let Some(b) = image.peek()
            && b.rows.contains(&row)
        {
            in_run(&std::mem::take(&mut run), &wanted, &mut out);
            if row == b.rows.start
                && heading_text(&doc.blocks, &b.span).is_some_and(|text| {
                    let text: Vec<char> = text.chars().map(|c| fold(c, ignore)).collect();
                    text.windows(wanted.len()).any(|w| w == wanted)
                })
            {
                let cells = b.rows.clone().map(|r| (r, 0..0)).collect();
                out.push(Match { row, cells });
            }
            continue;
        }
        // The rows under a larger line hold only its prefix, and the next
        // line may still carry on from it.
        if under.contains(&row) {
            in_run(&in_line(line, ignore, row), &wanted, &mut out);
            continue;
        }
        let drawn = match sized.peek().filter(|l| l.row == row) {
            Some(sized) => {
                under = row + 1..row + usize::from(sized.scale);
                in_sized(sized, ignore)
            }
            None => in_line(line, ignore, row),
        };
        match joins.peek().filter(|j| j.row == row) {
            Some(join) => {
                if let (Some(end), Some(last)) = (join.end, run.last().map(|d| d.row)) {
                    while run
                        .pop_if(|d| d.row == last && d.cols.start >= end)
                        .is_some()
                    {}
                }
                if join.space
                    && let Some(last) = run.last()
                {
                    let at = last.cols.end;
                    let (row, cols) = (last.row, at..at);
                    run.push(Drawn { c: ' ', row, cols });
                }
                run.extend(drawn.into_iter().filter(|d| d.cols.start >= join.col));
            }
            None => in_run(&std::mem::replace(&mut run, drawn), &wanted, &mut out),
        }
    }
    in_run(&run, &wanted, &mut out);
    // A run's matches go in when it ends, after those of rows under it.
    out.sort_by_key(|m| m.row);
    out
}

/// A character as drawn, compared as `fold` gives it.
struct Drawn {
    c: char,
    /// The line it is on, in 32 bits to keep it small.
    row: u32,
    /// The columns it covers.
    cols: Range<u16>,
}

/// The characters of a heading line drawn larger, whose characters the
/// layout spaced out, by the columns they cover.
fn in_sized(line: &SizedLine, ignore: bool) -> Vec<Drawn> {
    let mut out = Vec::new();
    for cell in &line.cells {
        let end = cell.col + u16::from(line.scale) * u16::from(cell.width);
        out.extend(cell.text.chars().map(|c| Drawn {
            c: fold(c, ignore),
            row: line.row as u32,
            cols: cell.col..end,
        }));
    }
    out
}

/// The characters of one drawn line, by column.
fn in_line(line: &Line, ignore: bool, row: usize) -> Vec<Drawn> {
    let mut out = Vec::new();
    let mut col: u16 = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            let end = col.saturating_add(c.width().unwrap_or(0) as u16);
            out.push(Drawn {
                c: fold(c, ignore),
                row: row as u32,
                cols: col..end,
            });
            col = end;
        }
    }
    out
}

/// The matches of `wanted` in `run`, the text of one line or of lines the
/// text wrapped over, each in a piece per line. Matches do not overlap.
fn in_run(run: &[Drawn], wanted: &[char], out: &mut Vec<Match>) {
    let mut i = 0;
    while i + wanted.len() <= run.len() {
        let found = &run[i..i + wanted.len()];
        if !found.iter().map(|d| d.c).eq(wanted.iter().copied()) {
            i += 1;
            continue;
        }
        let mut cells: Vec<(usize, Range<u16>)> = Vec::new();
        for d in found {
            let at = d.row as usize;
            match cells.last_mut() {
                Some((row, cols)) if *row == at => cols.end = cols.end.max(d.cols.end),
                _ => cells.push((at, d.cols.clone())),
            }
        }
        // Such as the space put in at a break, which is not drawn.
        cells.retain(|(_, cols)| !cols.is_empty());
        if let Some(&(row, _)) = cells.first() {
            out.push(Match { row, cells });
        }
        i += wanted.len();
    }
}

/// The text of the heading whose span is `span`, looking inside quotes,
/// callouts, lists and the like.
fn heading_text(blocks: &[Block], span: &Range<usize>) -> Option<String> {
    blocks.iter().find_map(|b| match &b.kind {
        BlockKind::Heading { content, .. } if b.span == *span => Some(
            content
                .iter()
                .map(|i| match i {
                    Inline::Text { text, .. } => text.as_str(),
                    Inline::Break => " ",
                })
                .collect(),
        ),
        BlockKind::Quote(inner)
        | BlockKind::Callout { blocks: inner, .. }
        | BlockKind::Details { blocks: inner, .. } => heading_text(inner, span),
        BlockKind::List { items, .. } => items
            .iter()
            .find_map(|item| heading_text(&item.blocks, span)),
        BlockKind::Definition { definitions, .. } => definitions
            .iter()
            .find_map(|details| heading_text(details, span)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;
    use crate::images::Images;
    use crate::render::render;

    fn found(md: &str, term: &str) -> Vec<Match> {
        let doc = doc::parse(md);
        let layout = render(&doc, 40, &Images::new((8, 16)));
        find(term, &layout, &doc)
    }

    fn at(row: usize, cols: Range<u16>) -> Match {
        Match {
            row,
            cells: vec![(row, cols)],
        }
    }

    #[test]
    fn a_sized_heading_matches_by_its_characters() {
        use crate::heading::SizedCell;
        use ratatui::text::Line;
        // "Go 日" twice the size, spaced out, after a quote bar.
        let cell = |col, width, text: &str| SizedCell {
            col,
            width,
            text: text.into(),
        };
        let layout = Layout {
            lines: vec![Line::raw("> G o   日  "), Line::raw(">")],
            sized: vec![SizedLine {
                row: 0,
                scale: 2,
                cells: vec![
                    cell(2, 1, "G"),
                    cell(4, 1, "o"),
                    cell(6, 1, " "),
                    cell(8, 2, "日"),
                ],
            }],
            ..Layout::default()
        };
        let doc = doc::parse("> ## Go 日\n");
        assert_eq!(find("go", &layout, &doc), vec![at(0, 2..6)]);
        assert_eq!(find("o 日", &layout, &doc), vec![at(0, 4..12)]);
        assert_eq!(find("g o", &layout, &doc), vec![]);
    }

    /// The matches of `term` when the block that starts at the first `from`
    /// in `md` is drawn as an image, on the lines of its text and `more`.
    fn found_in_image(md: &str, from: &str, more: usize, term: &str) -> Vec<Match> {
        use crate::images::Placement;
        let doc = doc::parse(md);
        let mut layout = render(&doc, 40, &Images::new((8, 16)));
        let at = md.find(from).unwrap();
        let block = layout.blocks.iter_mut().rfind(|b| b.span.start == at);
        let block = block.expect("a block starts there");
        block.rows.end += more;
        let rows = block.rows.len() as u16;
        block.image = Some(Placement {
            id: 1,
            cols: 20,
            rows,
        });
        find(term, &layout, &doc)
    }

    #[test]
    fn a_heading_drawn_as_an_image_matches_by_its_text() {
        // Each line of the image, with no columns to mark.
        let on = |rows: Range<usize>| {
            vec![Match {
                row: rows.start,
                cells: rows.map(|r| (r, 0..0)).collect(),
            }]
        };
        for (md, row) in [
            ("# Big Title\n\nafter\n", 0),
            ("> # Big Title\n", 0),
            ("> [!NOTE]\n> # Big Title\n", 1),
            (
                "<details open>\n<summary>more</summary>\n\n# Big Title\n\n</details>\n",
                1,
            ),
            ("- # Big Title\n", 0),
            ("Term\n: # Big Title\n", 1),
        ] {
            let on = on(row..row + 1);
            assert_eq!(found_in_image(md, "#", 0, "big title"), on, "{md:?}");
            assert_eq!(found_in_image(md, "#", 0, "Big Title"), on, "{md:?}");
            assert_eq!(found_in_image(md, "#", 0, "Big title"), [], "{md:?}");
            assert_eq!(found_in_image(md, "#", 0, "big titles"), [], "{md:?}");
        }
        // Only its first line matches: the lines under it are the image.
        let md = "# Big Title\n\nbig title\n";
        assert_eq!(found_in_image(md, "#", 2, "big title"), on(0..3));
        // The text is that of the heading drawn, not of another one.
        let md = "# Other\n\n# Big Title\n";
        assert_eq!(found_in_image(md, "# Big", 0, "big title"), on(2..3));
        assert_eq!(found_in_image(md, "# Big", 0, "other"), [at(0, 0..5)]);
        // A hard break in the heading reads as a space.
        assert_eq!(
            found_in_image("Big\\\nTitle\n===\n", "Big", 0, "big title"),
            on(0..2)
        );
    }

    #[test]
    fn the_lines_of_a_picture_drawn_as_an_image_are_passed_over() {
        let md = "![big title](big.png)\n\nbig title\n";
        let text = found(md, "big title");
        assert_eq!(text.len(), 2, "{text:?}");
        assert_eq!(found_in_image(md, "!", 0, "big title"), text[1..]);
    }

    #[test]
    fn lower_case_ignores_case_and_a_capital_matches_it() {
        let md = "Apple apple APPLE\n";
        assert_eq!(
            found(md, "apple"),
            vec![at(0, 0..5), at(0, 6..11), at(0, 12..17)]
        );
        assert_eq!(found(md, "Apple"), vec![at(0, 0..5)]);
        assert_eq!(found(md, ""), vec![]);
    }

    #[test]
    fn the_drawn_text_is_searched_not_the_source() {
        // The link's address and the emphasis marks are not drawn.
        let md = "see [the docs](http://x.org) and *this*\n";
        assert_eq!(found(md, "x.org"), vec![]);
        assert_eq!(found(md, "docs"), vec![at(0, 8..12)]);
        assert_eq!(found(md, "*"), vec![]);
    }

    #[test]
    fn code_blocks_and_table_cells_are_searched() {
        let md = "```\nlet needle = 1;\n```\n\n| a | b |\n| - | - |\n| hay | needle |\n";
        let rows: Vec<usize> = found(md, "needle").iter().map(|m| m.row).collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
    }

    #[test]
    fn a_term_wrapped_over_two_lines_is_found_in_two_pieces() {
        // At 40 columns "nine" goes to the second line.
        let md = "one two three four five six seven eight nine ten\n";
        let wrapped = Match {
            row: 0,
            cells: vec![(0, 34..39), (1, 0..4)],
        };
        assert_eq!(found(md, "eight nine"), std::slice::from_ref(&wrapped));
        assert_eq!(wrapped.rows(), 0..2);
        // The space dropped at the break is put back, once.
        assert_eq!(found(md, "eightnine"), []);
        assert_eq!(found(md, "eight  nine"), []);
        // In a quote the second line's bar is not part of the text.
        let quoted = Match {
            row: 0,
            cells: vec![(0, 30..35), (1, 2..7)],
        };
        assert_eq!(found(&format!("> {md}"), "seven eight"), [quoted]);
        assert_eq!(found(&format!("> {md}"), "seven │"), []);
        // A word too long for a line is broken with no space between.
        let long = format!("{}yz\n", "x".repeat(39));
        let broken = Match {
            row: 0,
            cells: vec![(0, 39..40), (1, 0..1)],
        };
        assert_eq!(found(&long, "yz"), [broken]);
    }

    #[test]
    fn a_frontmatter_value_is_searched_across_its_wrap() {
        let words = "one two three four five six seven eight nine ten";
        let md = format!("---\ndescription: {words}\n---\n");
        let words: Vec<&str> = words.split(' ').collect();
        for pair in words.windows(2) {
            assert_eq!(found(&md, &pair.join(" ")).len(), 1, "{pair:?}");
        }
    }

    #[test]
    fn a_code_line_broken_at_the_edge_is_searched_across_the_break() {
        // The edge splits "needle", with or without the panel's indent.
        let md = format!("```\n{}needle\nfoo\nbar\n```\n", "x".repeat(36));
        let m = found(&md, "needle");
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!(m[0].cells.len(), 2);
        assert_eq!(m[0].rows().len(), 2);
        // No space is put back at the break.
        assert_eq!(found(&md, "ne edle"), []);
        assert_eq!(found(&md, "nee dle"), []);
        // Separate lines of the code are still not joined.
        assert_eq!(found(&md, "needlefoo"), []);
        assert_eq!(found(&md, "needle foo"), []);
        assert_eq!(found(&md, "foo bar"), []);
    }

    #[test]
    fn the_first_match_from_a_row_starts_on_or_below_it() {
        let search = Search {
            term: "x".into(),
            matches: vec![
                Match {
                    row: 0,
                    cells: vec![(0, 5..6), (1, 0..1)],
                },
                Match {
                    row: 3,
                    cells: vec![(3, 0..1)],
                },
            ],
            ..Search::default()
        };
        assert_eq!(search.first_from(1), Some((1, false)));
    }

    #[test]
    fn a_counted_step_goes_on_from_the_first_and_round() {
        let on = |row| Match {
            row,
            cells: vec![(row, 0..1)],
        };
        let mut search = Search {
            term: "x".into(),
            matches: vec![on(0), on(5), on(10)],
            ..Search::default()
        };
        // The first step is from the view, the rest from that match.
        let (top, bottom) = (0..3, 9..12);
        assert_eq!(search.step_by(true, top.clone(), 1), Some((0, false)));
        assert_eq!(search.step_by(true, top.clone(), 3), Some((2, false)));
        assert_eq!(search.step_by(true, top.clone(), 4), Some((0, true)));
        assert_eq!(search.step_by(true, top.clone(), 7), Some((0, true)));
        assert_eq!(search.step_by(false, bottom.clone(), 1), Some((2, false)));
        assert_eq!(search.step_by(false, bottom.clone(), 3), Some((0, false)));
        assert_eq!(search.step_by(false, bottom, 4), Some((2, true)));
        // From the current match while it is in view.
        search.current = Some(1);
        assert_eq!(search.step_by(true, 4..7, 2), Some((0, true)));
        assert_eq!(search.step_by(false, 4..7, 2), Some((2, true)));
        assert_eq!(Search::default().step_by(true, top, 2), None);
    }

    #[test]
    fn a_match_is_seen_when_any_of_its_lines_is_in_view() {
        let m = Match {
            row: 2,
            cells: vec![(2, 5..6), (3, 0..1)],
        };
        for (view, seen) in [(0..2, false), (1..3, true), (3..4, true), (4..6, false)] {
            assert_eq!(m.seen(&view), seen, "{view:?}");
        }
    }

    #[test]
    fn a_step_with_no_match_ahead_goes_round() {
        let on = |row| Match {
            row,
            cells: vec![(row, 0..1)],
        };
        let search = Search {
            term: "x".into(),
            matches: vec![on(5), on(10)],
            ..Search::default()
        };
        // Below the last match, forward goes round to the first.
        assert_eq!(search.step(true, 11..14), Some((0, true)));
        assert_eq!(search.first_from(11), Some((0, true)));
        // Above the first match, back goes round to the last.
        assert_eq!(search.step(false, 0..3), Some((1, true)));
        // A match on the line under the view is not above its bottom.
        assert_eq!(search.step(false, 2..5), Some((1, true)));
    }

    #[test]
    fn the_prompt_counts_the_matches_so_far() {
        let one = Match {
            row: 0,
            cells: vec![(0, 0..1)],
        };
        let mut search = Search::default();
        assert!(!search.active());
        assert_eq!(search.prompt(), "/");
        search.term = "x".into();
        assert!(search.active());
        assert_eq!(search.prompt(), "/x  no matches");
        search.matches = vec![one.clone()];
        assert_eq!(search.prompt(), "/x  1 match");
        search.matches = vec![one.clone(), one];
        assert_eq!(search.prompt(), "/x  2 matches");
    }

    #[test]
    fn a_drawn_character_is_small() {
        // One is held for each character of a run of wrapped lines.
        assert_eq!(std::mem::size_of::<Drawn>(), 12);
    }

    #[test]
    fn lines_that_did_not_wrap_are_not_joined() {
        for md in [
            "```\nfoo\nbar\n```\n",
            "| a |\n| - |\n| foo |\n| bar |\n",
            "foo\n\nbar\n",
            "foo  \nbar\n",
            "- foo\n- bar\n",
        ] {
            assert_eq!(found(md, "foo bar"), [], "{md:?}");
            assert_eq!(found(md, "foo").len(), 1, "{md:?}");
        }
    }

    #[test]
    fn columns_count_wide_characters_twice() {
        assert_eq!(found("日本 cat\n", "cat"), vec![at(0, 5..8)]);
    }
}
