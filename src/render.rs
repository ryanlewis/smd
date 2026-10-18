//! Lays a document out as styled lines for a given width.

use std::collections::HashMap;
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::doc::{Block, BlockKind, Document, FigureKind, Inline, InlineStyle};
use crate::images::{self, Images, Placement};
use crate::kitty;

const TAB_WIDTH: usize = 4;

/// A document laid out for one width.
#[derive(Debug, Default)]
pub struct Layout {
    pub lines: Vec<Line<'static>>,
    /// Where each block landed, in document order. A block that holds others
    /// comes before them.
    pub blocks: Vec<BlockRows>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockRows {
    /// The block's byte range in the source.
    pub span: Range<usize>,
    /// The lines the block takes.
    pub rows: Range<usize>,
    /// The image drawn on those lines, when the block is drawn as one.
    pub image: Option<Placement>,
}

impl Layout {
    /// Each image in the layout, once, with the size it was laid out at.
    pub fn placements(&self) -> Vec<Placement> {
        let mut out: Vec<Placement> = Vec::new();
        for p in self.blocks.iter().filter_map(|b| b.image) {
            if !out.iter().any(|q| q.id == p.id) {
                out.push(p);
            }
        }
        out
    }
}

struct Ctx<'a> {
    images: &'a Images,
    layout: Layout,
    /// The size each image was given where it first appeared. One image has
    /// one placement, so a later block showing it must use the same size.
    sizes: HashMap<u32, (u16, u16)>,
}

/// Lay out the whole document for a view `width` columns wide. Blocks whose
/// image is ready in `images` are drawn as placeholder cells.
pub fn render(doc: &Document, width: u16, images: &Images) -> Layout {
    let mut ctx = Ctx {
        images,
        layout: Layout::default(),
        sizes: HashMap::new(),
    };
    blocks(&doc.blocks, width as usize, &[], &[], false, &mut ctx);
    ctx.layout
}

/// Render blocks one after another. `first` prefixes the first line and
/// `rest` every other line, which is how list markers and quote bars nest.
fn blocks(
    blocks: &[Block],
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    tight: bool,
    ctx: &mut Ctx,
) {
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 && !tight {
            emit(rest, Vec::new(), &mut ctx.layout.lines);
        }
        block(b, width, if i == 0 { first } else { rest }, rest, ctx);
    }
}

fn block(
    block: &Block,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    ctx: &mut Ctx,
) {
    let index = ctx.layout.blocks.len();
    let start = ctx.layout.lines.len();
    ctx.layout.blocks.push(BlockRows {
        span: block.span.clone(),
        rows: start..start,
        image: None,
    });
    let image = placement(block, width, first, rest, ctx);
    match image {
        Some(p) => image_rows(p, first, rest, &mut ctx.layout.lines),
        None => contents(block, width, first, rest, ctx),
    }
    let entry = &mut ctx.layout.blocks[index];
    entry.rows = start..ctx.layout.lines.len();
    entry.image = image;
}

/// The size to draw a block's image at, if it has one ready.
fn placement(
    block: &Block,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    ctx: &mut Ctx,
) -> Option<Placement> {
    let ready = ctx.images.ready(&images::key(block)?)?;
    let prefix = spans_width(first).max(spans_width(rest));
    let avail = width.saturating_sub(prefix).clamp(1, u16::MAX as usize) as u16;
    let fitted = images::fit(ready.px, ctx.images.cell(), avail);
    let size = *ctx.sizes.entry(ready.id).or_insert(fitted);
    // Shown at a different size elsewhere: keep this one as text.
    if size != fitted {
        return None;
    }
    Some(Placement {
        id: ready.id,
        cols: size.0,
        rows: size.1,
    })
}

/// One line of placeholder cells per image row.
fn image_rows(
    p: Placement,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let style = Style::new().fg(kitty::colour(p.id));
    for row in 0..p.rows {
        let cells: String = (0..p.cols).map(|col| kitty::cell(row, col)).collect();
        let prefix = if row == 0 { first } else { rest };
        emit(prefix, vec![Span::styled(cells, style)], out);
    }
}

fn contents(
    block: &Block,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    ctx: &mut Ctx,
) {
    let out = &mut ctx.layout.lines;
    match &block.kind {
        BlockKind::Paragraph(content) => {
            wrap(content, Style::default(), width, first, rest, out);
        }
        BlockKind::Heading { level, content } => {
            wrap(content, heading_style(*level), width, first, rest, out);
        }
        BlockKind::Code { code, .. } => {
            preformatted(
                code,
                Style::new().fg(Color::Yellow),
                width,
                first,
                rest,
                out,
            );
        }
        BlockKind::Figure { kind, source } => {
            let style = match kind {
                FigureKind::Table => Style::default(),
                FigureKind::Mermaid | FigureKind::Math => Style::new().fg(Color::Cyan),
            };
            // A figure that could not be drawn says why above its source.
            let error = images::key(block).and_then(|key| ctx.images.error(&key));
            let first = match error {
                Some(error) => {
                    let note = format!("{error}; showing the source");
                    let dim = Style::new().add_modifier(Modifier::DIM);
                    emit(first, vec![Span::styled(note, dim)], out);
                    rest
                }
                None => first,
            };
            preformatted(source, style, width, first, rest, out);
        }
        BlockKind::Html(html) => {
            preformatted(
                html,
                Style::new().add_modifier(Modifier::DIM),
                width,
                first,
                rest,
                out,
            );
        }
        BlockKind::Image { alt, .. } => {
            let text = Inline::Text {
                text: format!("[image: {alt}]"),
                style: InlineStyle::default(),
            };
            wrap(&[text], Style::default(), width, first, rest, out);
        }
        BlockKind::Rule => {
            let avail = width.saturating_sub(spans_width(first)).max(1);
            let rule = Span::styled("─".repeat(avail), Style::new().add_modifier(Modifier::DIM));
            emit(first, vec![rule], out);
        }
        BlockKind::Quote(inner) => {
            let bar = Span::styled("│ ", Style::new().fg(Color::DarkGray));
            let first = with(first, bar.clone());
            let rest = with(rest, bar);
            blocks(inner, width, &first, &rest, false, ctx);
        }
        BlockKind::List {
            start,
            tight,
            items,
        } => {
            let markers: Vec<String> = (0..items.len() as u64)
                .map(|n| match start {
                    Some(s) => format!("{}. ", s + n),
                    None => "• ".to_string(),
                })
                .collect();
            let marker_width = markers.iter().map(|m| m.width()).max().unwrap_or(0);
            for (i, (item, marker)) in items.iter().zip(markers).enumerate() {
                if i > 0 && !tight {
                    emit(rest, Vec::new(), &mut ctx.layout.lines);
                }
                let mut marker = format!("{marker:>marker_width$}");
                let mut indent = marker_width;
                if let Some(done) = item.task {
                    marker.push_str(if done { "[x] " } else { "[ ] " });
                    indent += 4;
                }
                let item_first = with(
                    if i == 0 { first } else { rest },
                    Span::styled(marker, Style::new().fg(Color::Blue)),
                );
                let item_rest = with(rest, Span::raw(" ".repeat(indent)));
                if item.blocks.is_empty() {
                    emit(&item_first, Vec::new(), &mut ctx.layout.lines);
                }
                blocks(&item.blocks, width, &item_first, &item_rest, *tight, ctx);
            }
        }
    }
}

fn heading_style(level: u8) -> Style {
    let style = Style::new().add_modifier(Modifier::BOLD);
    match level {
        1 => style.fg(Color::Magenta).add_modifier(Modifier::UNDERLINED),
        2 => style.fg(Color::Cyan),
        3 => style.fg(Color::Green),
        _ => style,
    }
}

fn inline_style(base: Style, style: &InlineStyle) -> Style {
    let mut s = base;
    if style.emphasis {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if style.strong {
        s = s.add_modifier(Modifier::BOLD);
    }
    if style.strikethrough {
        s = s.add_modifier(Modifier::CROSSED_OUT);
    }
    if style.code {
        s = s.fg(Color::Yellow);
    }
    if style.math {
        s = s.fg(Color::Cyan);
    }
    if style.link.is_some() {
        s = s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
    }
    s
}

/// A unit of inline content for line breaking.
enum Unit {
    /// A run of non-space text, possibly in several styles.
    Word(Vec<(String, Style)>),
    Space(Style),
    Break,
}

fn units(content: &[Inline], base: Style) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut word: Vec<(String, Style)> = Vec::new();
    for inline in content {
        let (text, code, style) = match inline {
            Inline::Text { text, style } => (text, style.code, inline_style(base, style)),
            Inline::Break => {
                if !word.is_empty() {
                    units.push(Unit::Word(std::mem::take(&mut word)));
                }
                units.push(Unit::Break);
                continue;
            }
        };
        for c in text.chars() {
            // Code keeps its spaces and is not broken at them.
            if c.is_whitespace() && !code {
                if !word.is_empty() {
                    units.push(Unit::Word(std::mem::take(&mut word)));
                }
                if !matches!(units.last(), Some(Unit::Space(_))) {
                    units.push(Unit::Space(style));
                }
            } else {
                match word.last_mut() {
                    Some((s, st)) if *st == style => s.push(c),
                    _ => word.push((c.to_string(), style)),
                }
            }
        }
    }
    if !word.is_empty() {
        units.push(Unit::Word(word));
    }
    units
}

/// Fill lines greedily with inline content, breaking at spaces.
fn wrap(
    content: &[Inline],
    base: Style,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let mut lines = Lines::new(width, first, rest, out);
    let mut space: Option<Style> = None;
    for unit in units(content, base) {
        match unit {
            Unit::Break => {
                lines.finish();
                space = None;
            }
            Unit::Space(style) => {
                if lines.used > 0 {
                    space = Some(style);
                }
            }
            Unit::Word(segments) => {
                let word_width: usize = segments.iter().map(|(s, _)| s.width()).sum();
                if let Some(style) = space.take() {
                    if lines.used + 1 + word_width > lines.avail() {
                        lines.finish();
                    } else {
                        lines.push(" ", style);
                    }
                }
                if word_width <= lines.avail() {
                    for (s, style) in &segments {
                        lines.push(s, *style);
                    }
                } else {
                    // Longer than a whole line: break it anywhere.
                    for (s, style) in &segments {
                        lines.push_breaking(s, *style);
                    }
                }
            }
        }
    }
    lines.finish_last();
}

/// Text shown as is, broken at the edge of the view rather than reflowed.
fn preformatted(
    text: &str,
    style: Style,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let mut lines = Lines::new(width, first, rest, out);
    let text = text.trim_end_matches('\n');
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            lines.finish();
        }
        lines.push_breaking(&expand_tabs(line), style);
    }
    lines.finish_last();
}

fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::new();
    let mut col = 0;
    for c in line.chars() {
        if c == '\t' {
            let n = TAB_WIDTH - col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += c.width().unwrap_or(0);
        }
    }
    out
}

/// Builds prefixed lines, tracking how much of the current one is used.
struct Lines<'a> {
    width: usize,
    first: &'a [Span<'static>],
    rest: &'a [Span<'static>],
    out: &'a mut Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    used: usize,
    emitted: usize,
}

impl<'a> Lines<'a> {
    fn new(
        width: usize,
        first: &'a [Span<'static>],
        rest: &'a [Span<'static>],
        out: &'a mut Vec<Line<'static>>,
    ) -> Self {
        Lines {
            width,
            first,
            rest,
            out,
            current: Vec::new(),
            used: 0,
            emitted: 0,
        }
    }

    fn prefix(&self) -> &'a [Span<'static>] {
        if self.emitted == 0 {
            self.first
        } else {
            self.rest
        }
    }

    /// Columns available for content on the current line.
    fn avail(&self) -> usize {
        self.width.saturating_sub(spans_width(self.prefix())).max(1)
    }

    fn push(&mut self, s: &str, style: Style) {
        self.used += s.width();
        match self.current.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(s),
            _ => self.current.push(Span::styled(s.to_string(), style)),
        }
    }

    fn push_breaking(&mut self, s: &str, style: Style) {
        let mut buf = [0; 4];
        for c in s.chars() {
            let w = c.width().unwrap_or(0);
            if self.used > 0 && self.used + w > self.avail() {
                self.finish();
            }
            self.push(c.encode_utf8(&mut buf), style);
        }
    }

    fn finish(&mut self) {
        let content = std::mem::take(&mut self.current);
        emit(self.prefix(), content, self.out);
        self.used = 0;
        self.emitted += 1;
    }

    /// Emit the last line, and at least one line for the block.
    fn finish_last(&mut self) {
        if !self.current.is_empty() || self.emitted == 0 {
            self.finish();
        }
    }
}

fn emit(prefix: &[Span<'static>], content: Vec<Span<'static>>, out: &mut Vec<Line<'static>>) {
    let mut spans = prefix.to_vec();
    if content.is_empty() {
        // No trailing spaces on blank lines, so copied text stays clean.
        while let Some(last) = spans.last_mut() {
            let trimmed = last.content.trim_end().to_string();
            if trimmed.is_empty() {
                spans.pop();
            } else {
                last.content = trimmed.into();
                break;
            }
        }
    }
    spans.extend(content);
    out.push(Line::from(spans));
}

fn with(prefix: &[Span<'static>], span: Span<'static>) -> Vec<Span<'static>> {
    let mut v = prefix.to_vec();
    v.push(span);
    v
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::parse;

    fn no_images() -> Images {
        Images::new((8, 16))
    }

    fn plain(md: &str, width: u16) -> Vec<String> {
        render(&parse(md), width, &no_images())
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn paragraphs_reflow_to_width() {
        assert_eq!(
            plain("one two three four five\n\nsix\n", 10),
            vec!["one two", "three four", "five", "", "six"]
        );
    }

    #[test]
    fn long_words_break_anywhere() {
        assert_eq!(plain("abcdefghij\n", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wide_characters_count_two_columns() {
        assert_eq!(plain("日本語 日本\n", 6), vec!["日本語", "日本"]);
    }

    #[test]
    fn list_items_wrap_under_their_text() {
        assert_eq!(
            plain("- alpha beta gamma\n- [x] done\n", 12),
            vec!["• alpha beta", "  gamma", "• [x] done"]
        );
    }

    #[test]
    fn ordered_markers_align() {
        let md: String = (1..=10).map(|n| format!("{n}. item\n")).collect();
        let lines = plain(&md, 20);
        assert_eq!(lines[0], " 1. item");
        assert_eq!(lines[9], "10. item");
    }

    #[test]
    fn nested_tight_list_has_no_blank_lines() {
        assert_eq!(plain("- a\n  - b\n- c\n", 20), vec!["• a", "  • b", "• c"]);
    }

    #[test]
    fn quotes_prefix_every_line() {
        assert_eq!(
            plain("> one two\n>\n> three\n", 9),
            vec!["│ one two", "│", "│ three"]
        );
    }

    #[test]
    fn code_keeps_lines_and_breaks_at_the_edge() {
        assert_eq!(
            plain("```\nlet x = 1;\n\tindented\n```\n", 8),
            vec!["let x = ", "1;", "    inde", "nted"]
        );
    }

    #[test]
    fn inline_code_keeps_its_spaces() {
        assert_eq!(plain("run `a  b c` now\n", 8), vec!["run", "a  b c", "now"]);
    }

    #[test]
    fn figures_show_their_source() {
        assert_eq!(
            plain("```mermaid\ngraph TD\n  A --> B\n```\n", 40),
            vec!["graph TD", "  A --> B"]
        );
        assert_eq!(
            plain("| a | b |\n|---|---|\n| 1 | 2 |\n", 40),
            vec!["| a | b |", "|---|---|", "| 1 | 2 |"]
        );
    }

    /// Images with `src` ready, each `px` pixels, in a store with 10x20 cells.
    fn ready(images: &[(&str, (u32, u32))]) -> Images {
        use crate::images::tests::{file_key, job, png, settle};
        let mut store = Images::new((10, 20));
        for (src, (w, h)) in images {
            store.request(file_key(src), job(png(*w, *h)));
        }
        settle(&mut store);
        store
    }

    fn id(images: &Images, src: &str) -> u32 {
        images
            .ready(&crate::images::tests::file_key(src))
            .unwrap()
            .id
    }

    #[test]
    fn an_image_not_ready_shows_its_alt_text() {
        assert_eq!(plain("![a cat](cat.png)\n", 40), vec!["[image: a cat]"]);
    }

    #[test]
    fn a_ready_image_reserves_rows_of_placeholder_cells() {
        let images = ready(&[("cat.png", (30, 40))]);
        let id = id(&images, "cat.png");
        let layout = render(&parse("one\n\n![cat](cat.png)\n\ntwo\n"), 40, &images);
        let text: Vec<String> = layout.lines.iter().map(|l| l.to_string()).collect();
        // 30x40 px is 3x2 cells; each row is one span coloured with the id.
        assert_eq!(
            text,
            vec![
                "one".to_string(),
                String::new(),
                kitty::cell(0, 0) + &kitty::cell(0, 1) + &kitty::cell(0, 2),
                kitty::cell(1, 0) + &kitty::cell(1, 1) + &kitty::cell(1, 2),
                String::new(),
                "two".to_string(),
            ]
        );
        assert_eq!(layout.lines[2].spans[0].style.fg, Some(kitty::colour(id)));
        let image = Placement {
            id,
            cols: 3,
            rows: 2,
        };
        assert_eq!(
            layout.blocks,
            vec![
                BlockRows {
                    span: 0..4,
                    rows: 0..1,
                    image: None
                },
                BlockRows {
                    span: 5..21,
                    rows: 2..4,
                    image: Some(image)
                },
                BlockRows {
                    span: 22..26,
                    rows: 5..6,
                    image: None
                },
            ]
        );
        assert_eq!(layout.placements(), vec![image]);
    }

    #[test]
    fn a_wide_image_fits_inside_its_prefix() {
        let images = ready(&[("wide.png", (1000, 100))]);
        let layout = render(&parse("> - ![w](wide.png)\n"), 24, &images);
        // The quote bar and list marker take 4 of the 24 columns.
        let image = layout.placements()[0];
        assert_eq!((image.cols, image.rows), (20, 1));
        assert!(layout.lines[0].to_string().starts_with("│ • \u{10EEEE}"));
        // Quote, list, then the image inside them.
        assert_eq!(layout.blocks.len(), 3);
        assert_eq!(layout.blocks[2].image, Some(image));
        for line in &layout.lines {
            assert!(line.width() <= 24, "{line:?}");
        }
    }

    #[test]
    fn one_image_shown_at_two_sizes_keeps_the_second_as_text() {
        let images = ready(&[("wide.png", (1000, 100))]);
        let layout = render(&parse("![w](wide.png)\n\n> ![w](wide.png)\n"), 30, &images);
        assert_eq!(layout.placements().len(), 1);
        assert_eq!(layout.lines.last().unwrap().to_string(), "│ [image: w]");
    }

    #[test]
    fn no_line_is_wider_than_the_view() {
        let md = "# A heading that is long\n\n> - quoted list item with words\n\n```\na long line of code here\n```\n\n---\n";
        for width in 8..30u16 {
            for line in render(&parse(md), width, &no_images()).lines {
                assert!(line.width() <= width as usize, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn a_figure_that_failed_shows_why_above_its_source() {
        let doc = parse("> ```mermaid\n> graph LR\n> ```\n");
        let BlockKind::Quote(inner) = &doc.blocks[0].kind else {
            panic!("not a quote");
        };
        let mut images = no_images();
        let key = images::key(&inner[0]).unwrap();
        images.request(key, Box::new(|| Err("mermaid: boom".into())));
        crate::images::tests::settle(&mut images);
        let lines: Vec<String> = render(&doc, 40, &images)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            lines,
            vec!["│ mermaid: boom; showing the source", "│ graph LR"]
        );
    }
}
