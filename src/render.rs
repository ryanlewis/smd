//! Lays a document out as styled lines for a given width.

use std::collections::HashMap;
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::doc::{Block, BlockKind, Document, FigureKind, Inline, InlineStyle};
use crate::heading;
use crate::images::{self, Images, Placement};
use crate::kitty;
use crate::table::Table;

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
    /// The markdown the blocks' spans point into.
    source: &'a str,
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
        source: &doc.source,
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
    let key = images::key(block)?;
    let prefix = spans_width(first).max(spans_width(rest));
    let avail = width.saturating_sub(prefix).clamp(1, u16::MAX as usize) as u16;
    // A table that fits stays as text.
    if let BlockKind::Figure {
        kind: FigureKind::Table,
        source,
    } = &block.kind
        && Table::parse(source).width() <= usize::from(avail)
    {
        return None;
    }
    let ready = ctx.images.ready(&key)?;
    let fitted = match block.kind {
        BlockKind::Heading { .. } => heading::fit(ready.px, ctx.images.cell(), avail)?,
        _ => images::fit(ready.px, ctx.images.cell(), avail),
    };
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
            preformatted(code, CODE, width, first, rest, out);
        }
        // Without graphics an image block is never drawn, so it shows its
        // source as code. Tables are text already, cut when too wide.
        BlockKind::Figure { kind, source }
            if !ctx.images.graphics() && *kind != FigureKind::Table =>
        {
            preformatted(source, CODE, width, first, rest, out);
        }
        BlockKind::Image { src, alt } if !ctx.images.graphics() => {
            // As written, unless it spans lines, which in a quote or list
            // would bring their markers along.
            let text = match ctx.source.get(block.span.clone()).map(str::trim) {
                Some(text) if !text.contains('\n') => text.to_string(),
                _ => format!("![{alt}]({src})"),
            };
            preformatted(&text, CODE, width, first, rest, out);
        }
        BlockKind::Figure { kind, source } => {
            let table = (*kind == FigureKind::Table).then(|| Table::parse(source));
            let prefix = spans_width(first).max(spans_width(rest));
            let fits = |t: &Table| t.width() <= width.saturating_sub(prefix).max(1);
            // A figure that could not be drawn says why above its source. A
            // table that fits is meant to be text, so it has nothing to say.
            let error = match &table {
                Some(t) if fits(t) => None,
                _ => images::key(block).and_then(|key| ctx.images.error(&key)),
            };
            let first = match error {
                Some(error) => {
                    let note = Inline::Text {
                        text: format!("{error}; showing the source"),
                        style: InlineStyle::default(),
                    };
                    let dim = Style::new().add_modifier(Modifier::DIM);
                    wrap(&[note], dim, width, first, rest, out);
                    rest
                }
                None => first,
            };
            let Some(table) = table else {
                let style = Style::new().fg(Color::Cyan);
                preformatted(source, style, width, first, rest, out);
                return;
            };
            for (i, line) in table.text(inline_style).into_iter().enumerate() {
                let prefix = if i == 0 { first } else { rest };
                let avail = width.saturating_sub(spans_width(prefix)).max(1);
                emit(prefix, cut(line, avail), out);
            }
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

/// `spans` cut to `avail` columns, ending in `…` when anything was cut. Wide
/// tables are cut rather than wrapped, so their columns stay lined up.
fn cut(spans: Vec<Span<'static>>, avail: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= avail {
        return spans;
    }
    let mut out = Vec::new();
    let mut used = 0;
    'spans: for span in spans {
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w + 1 > avail {
                out.push(Span::styled(text, span.style));
                break 'spans;
            }
            text.push(c);
            used += w;
        }
        out.push(Span::styled(text, span.style));
    }
    out.push(Span::styled("…", Style::new().fg(Color::DarkGray)));
    out
}

const CODE: Style = Style::new().fg(Color::Yellow);

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
    }

    #[test]
    fn a_table_that_fits_is_drawn_as_text() {
        assert_eq!(
            plain("> | a | b |\n> |---|---|\n> | 1 | 2 |\n", 40),
            vec![
                "│ ┌───┬───┐",
                "│ │ a │ b │",
                "│ ├───┼───┤",
                "│ │ 1 │ 2 │",
                "│ └───┴───┘",
            ]
        );
    }

    #[test]
    fn a_wide_table_without_its_image_is_cut_not_wrapped() {
        assert_eq!(
            plain("| alpha | beta |\n|---|---|\n| `x` | 2 |\n", 10),
            vec![
                "┌───────┬…",
                "│ alpha │…",
                "├───────┼…",
                "│ x     │…",
                "└───────┴…",
            ]
        );
    }

    /// A store with the image of the table in `md` ready, at 10x20 cells.
    fn table_ready(md: &str) -> Images {
        let mut store = Images::new((10, 20));
        let doc = parse(md);
        let key = images::key(&doc.blocks[0]).unwrap();
        let job = crate::images::tests::job(crate::images::tests::png(400, 60));
        store.request(key, job);
        crate::images::tests::settle(&mut store);
        store
    }

    #[test]
    fn a_failed_table_image_is_noted_only_when_the_table_does_not_fit() {
        let md = "| alpha | beta |\n|---|---|\n| 1 | 2 |\n";
        let mut images = Images::new((10, 20));
        let key = images::key(&parse(md).blocks[0]).unwrap();
        images.request(key, Box::new(|| Err("table: no glyph".into())));
        crate::images::tests::settle(&mut images);
        let lines = |width| -> Vec<String> {
            let layout = render(&parse(md), width, &images);
            layout.lines.iter().map(|l| l.to_string()).collect()
        };
        assert_eq!(lines(40)[0], "┌───────┬──────┐");
        let narrow = lines(15);
        assert_eq!(narrow[0], "table: no");
        assert_eq!(narrow.last().unwrap(), "└───────┴─────…");
    }

    #[test]
    fn a_table_becomes_its_image_only_when_it_does_not_fit() {
        let md = "| alpha | beta |\n|---|---|\n| 1 | 2 |\n";
        let images = table_ready(md);
        let wide = render(&parse(md), 40, &images);
        assert_eq!(wide.lines.len(), 5);
        assert_eq!(wide.placements(), vec![]);
        // 16 columns do not fit in 15: the image is fitted to the width.
        let narrow = render(&parse(md), 15, &images);
        let p = narrow.placements();
        assert_eq!((p[0].cols, p[0].rows), (15, 2));
        assert_eq!(narrow.lines.len(), 2);
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
    fn text_mode_shows_image_blocks_as_their_source_in_code_style() {
        let md = "![a cat](cat.png)\n\n```mermaid\ngraph TD\n```\n\n| a |\n|---|\n\n# Title\n";
        let layout = render(&parse(md), 40, &Images::text());
        let text: Vec<String> = layout.lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(
            text,
            vec![
                "![a cat](cat.png)",
                "",
                "graph TD",
                "",
                "┌───┐",
                "│ a │",
                "└───┘",
                "",
                "Title"
            ]
        );
        for row in [0, 2] {
            assert_eq!(layout.lines[row].spans[0].style, CODE, "row {row}");
        }
        // Tables stay text, cut with … when too wide.
        for row in [4, 8] {
            assert_ne!(layout.lines[row].spans[0].style, CODE, "row {row}");
        }
        let wide = render(&parse("| aaaa | bbbb |\n|---|---|\n"), 8, &Images::text());
        let text: Vec<String> = wide.lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().all(|l| l.ends_with('…')), "{text:?}");
        assert!(layout.placements().is_empty());
        // The image is shown as written, not rebuilt from its parts.
        let md = "> ![a \\] b](<my diagram.png> \"Title\")\n";
        let layout = render(&parse(md), 60, &Images::text());
        assert_eq!(
            layout.lines[0].to_string(),
            "│ ![a \\] b](<my diagram.png> \"Title\")"
        );
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
    fn a_heading_image_shrinks_by_whole_rows_then_falls_back_to_text() {
        use crate::images::tests::{job, png, settle};
        let doc = parse("# Title\n");
        let mut store = Images::new((10, 20));
        // A level-one heading drawn 40 by 3 cells.
        // No heading image without a system font.
        let Some(key) = images::key(&doc.blocks[0]) else {
            return;
        };
        store.request(key, job(png(400, 60)));
        settle(&mut store);
        let size = |width| {
            let layout = render(&doc, width, &store);
            layout.placements().first().map(|p| (p.cols, p.rows))
        };
        assert_eq!(size(80), Some((40, 3)));
        assert_eq!(size(30), Some((27, 2)));
        assert_eq!(size(13), None);
        assert_eq!(plain("# Title\n", 13), vec!["Title"]);
    }

    #[test]
    fn no_line_is_wider_than_the_view() {
        let md = "# A heading that is long\n\n> - quoted list item with words\n\n```\na long line of code here\n```\n\n---\n\n- | a | 日本語 |\n  |---|---|\n  | b | c |\n";
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
        // A narrow pane wraps the note rather than cutting it off.
        let narrow: Vec<String> = render(&doc, 20, &images)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            narrow,
            vec!["│ mermaid: boom;", "│ showing the source", "│ graph LR"]
        );
    }
}
