//! Finding a term in the text as it is drawn: what the reader sees, code
//! blocks and table cells included, not the markdown source.

use std::ops::Range;

use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::doc::{Block, BlockKind, Document, Inline};
use crate::render::Layout;

/// One place the term is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The line it is on.
    pub row: usize,
    /// The columns it takes, or none for a heading drawn as an image, which
    /// has no text cells to mark.
    pub cols: Option<Range<u16>>,
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

    /// Find the term again, as in a new layout.
    pub fn refresh(&mut self, layout: &Layout, doc: &Document) {
        self.matches = find(&self.term, layout, doc);
        self.current = self.current.filter(|&i| i < self.matches.len());
    }

    /// The first match on or below row `top`, going round to the first of
    /// all when there is none. Gives its index and whether it went round.
    pub fn first_from(&self, top: usize) -> Option<(usize, bool)> {
        match self.matches.iter().position(|m| m.row >= top) {
            Some(i) => Some((i, false)),
            None => (!self.matches.is_empty()).then_some((0, true)),
        }
    }

    /// The next or previous match, going round at the ends. From a match
    /// that is not in `view`, it is the first match from the top of the view
    /// or the last one above its bottom. Gives its index and whether it went
    /// round.
    pub fn step(&self, forward: bool, view: Range<usize>) -> Option<(usize, bool)> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        let row = |i: usize| self.matches[i].row;
        let current = self.current.filter(|&i| view.contains(&row(i)));
        Some(match (current, forward) {
            (Some(i), true) => ((i + 1) % len, i + 1 == len),
            (Some(i), false) => ((i + len - 1) % len, i == 0),
            (None, true) => return self.first_from(view.start),
            (None, false) => match (0..len).rev().find(|&i| row(i) < view.end) {
                Some(i) => (i, false),
                None => (len - 1, true),
            },
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
/// matches on its first line by its text.
pub fn find(term: &str, layout: &Layout, doc: &Document) -> Vec<Match> {
    if term.is_empty() {
        return Vec::new();
    }
    let ignore = ignore_case(term);
    let wanted: Vec<char> = term.chars().map(|c| fold(c, ignore)).collect();
    let images: Vec<_> = layout.blocks.iter().filter(|b| b.image.is_some()).collect();
    let mut out = Vec::new();
    let mut image = images.iter().peekable();
    for (row, line) in layout.lines.iter().enumerate() {
        // Image blocks are in document order, so their rows only go up.
        while image.next_if(|b| b.rows.end <= row).is_some() {}
        if let Some(b) = image.peek()
            && b.rows.contains(&row)
        {
            if row == b.rows.start
                && heading_text(&doc.blocks, &b.span).is_some_and(|text| {
                    let text: Vec<char> = text.chars().map(|c| fold(c, ignore)).collect();
                    text.windows(wanted.len()).any(|w| w == wanted)
                })
            {
                out.push(Match { row, cols: None });
            }
            continue;
        }
        in_line(line, &wanted, ignore, row, &mut out);
    }
    out
}

/// The matches of `wanted` on one drawn line, by column. Matches do not
/// overlap.
fn in_line(line: &Line, wanted: &[char], ignore: bool, row: usize, out: &mut Vec<Match>) {
    // Each character with the column it starts at.
    let mut chars: Vec<(char, u16)> = Vec::new();
    let mut col: u16 = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            chars.push((fold(c, ignore), col));
            col = col.saturating_add(c.width().unwrap_or(0) as u16);
        }
    }
    let mut i = 0;
    while i + wanted.len() <= chars.len() {
        if chars[i..i + wanted.len()]
            .iter()
            .map(|(c, _)| *c)
            .eq(wanted.iter().copied())
        {
            let end = chars.get(i + wanted.len()).map_or(col, |(_, c)| *c);
            out.push(Match {
                row,
                cols: Some(chars[i].1..end),
            });
            i += wanted.len();
        } else {
            i += 1;
        }
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
            cols: Some(cols),
        }
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
    fn columns_count_wide_characters_twice() {
        assert_eq!(found("日本 cat\n", "cat"), vec![at(0, 5..8)]);
    }
}
