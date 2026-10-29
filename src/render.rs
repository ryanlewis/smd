//! Lays a document out as styled lines for a given width.

use std::collections::HashMap;
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::doc::{Block, BlockKind, Document, FigureKind, Inline, InlineStyle, Link, ListItem};
use crate::highlight::{self, Segments};
use crate::images::{self, Images, Placement};
use crate::kitty;
use crate::table::Table;
use crate::theme;

const TAB_WIDTH: usize = 4;

/// A document laid out for one width.
#[derive(Debug, Default)]
pub struct Layout {
    pub lines: Vec<Line<'static>>,
    /// Where each block landed, in document order. A block that holds others
    /// comes before them.
    pub blocks: Vec<BlockRows>,
    /// Every link drawn as text, in document order.
    pub links: Vec<LinkSpot>,
}

/// A link and the cells its text takes, which may be on several lines.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSpot {
    pub link: Link,
    /// The line and the columns on it, for each line the link is on.
    pub cells: Vec<(usize, Range<u16>)>,
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

    /// The block to keep in place when `row` is the top line of the view, as
    /// its index in `blocks` and the row's offset from the block's first line.
    /// That is the innermost block holding the row, or the next block when the
    /// row falls between blocks, even inside a quote or list, giving a
    /// negative offset.
    pub fn anchor(&self, row: usize) -> Option<(usize, isize)> {
        let holder = self.blocks.iter().rposition(|b| b.rows.contains(&row));
        let next = self.blocks.iter().position(|b| b.rows.start > row);
        let index = match (holder, next) {
            (Some(h), Some(n)) if self.blocks[n].rows.end <= self.blocks[h].rows.end => n,
            (Some(h), _) => h,
            (None, n) => n?,
        };
        Some((index, row as isize - self.blocks[index].rows.start as isize))
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
    /// The colour code blocks are drawn on, when the background is known.
    panel: Option<Color>,
    /// The syntaxes code is coloured with, once they have loaded.
    syntaxes: Option<&'a SyntaxSet>,
}

/// Lay out the whole document for a view `width` columns wide. Blocks whose
/// image is ready in `images` are drawn as placeholder cells.
pub fn render(doc: &Document, width: u16, images: &Images) -> Layout {
    layout(doc, width, images, theme::code_panel(), highlight::ready())
}

fn layout(
    doc: &Document,
    width: u16,
    images: &Images,
    panel: Option<Color>,
    syntaxes: Option<&SyntaxSet>,
) -> Layout {
    let mut ctx = Ctx {
        source: &doc.source,
        images,
        layout: Layout::default(),
        sizes: HashMap::new(),
        panel,
        syntaxes,
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
    let ready = ctx.images.ready(&key)?;
    let fitted = images::fit(ready.px, ctx.images.cell(), avail)?;
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
    let graphics = ctx.images.graphics();
    let out = &mut ctx.layout.lines;
    match &block.kind {
        BlockKind::Paragraph(content) => {
            let links = wrap(content, Style::default(), width, first, rest, out);
            ctx.layout.links.extend(links);
        }
        BlockKind::Heading { level, content } => {
            let links = wrap(content, heading_style(*level), width, first, rest, out);
            ctx.layout.links.extend(links);
        }
        BlockKind::Code { lang, code } => {
            let colours = lang
                .as_deref()
                .and_then(|lang| highlight::highlight(ctx.syntaxes, lang, code));
            match ctx.panel {
                Some(bg) => code_panel(code, colours.as_deref(), bg, width, first, rest, out),
                None => code_indented(code, colours.as_deref(), width, first, rest, out),
            }
        }
        BlockKind::Figure {
            kind: FigureKind::Table,
            source,
        } => table(source, width, first, rest, out),
        // Without graphics an image block is never drawn, so it shows its
        // source as code.
        BlockKind::Figure { source, .. } if !graphics => {
            preformatted(source, CODE, width, first, rest, out);
        }
        BlockKind::Image { src, alt, .. } if !graphics => {
            let text = image_source(ctx.source, block, src, alt);
            preformatted(&text, CODE, width, first, rest, out);
        }
        BlockKind::Figure { source, .. } => {
            let error = images::key(block).and_then(|key| ctx.images.error(&key));
            figure(source, error, width, first, rest, out);
        }
        BlockKind::Html(html) => {
            let dim = Style::new().add_modifier(Modifier::DIM);
            preformatted(html, dim, width, first, rest, out);
        }
        BlockKind::Image { alt, .. } => {
            let text = note(format!("[image: {alt}]"));
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
        } => list(*start, *tight, items, width, first, rest, ctx),
    }
}

/// Plain text as inline content.
fn note(text: String) -> Inline {
    Inline::Text {
        text,
        style: InlineStyle::default(),
    }
}

/// Code with no panel, where indenting sets it apart instead.
fn code_indented(
    code: &str,
    colours: Option<&[Segments]>,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let indent = Span::raw("  ");
    let first = with(first, indent.clone());
    let rest = with(rest, indent);
    let mut lines = Lines::new(width, &first, &rest, out);
    lines.code(code, colours, Style::new());
    lines.finish_last();
}

/// A table drawn as text in the width its prefix leaves.
fn table(
    source: &str,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let prefix = spans_width(first).max(spans_width(rest));
    let avail = width.saturating_sub(prefix).max(1);
    for (i, line) in Table::parse(source)
        .lines(avail, inline_style)
        .into_iter()
        .enumerate()
    {
        emit(if i == 0 { first } else { rest }, line, out);
    }
}

/// An image block's markdown as written, unless it spans lines, which in a
/// quote or list would bring their markers along.
fn image_source(source: &str, block: &Block, src: &str, alt: &str) -> String {
    match source.get(block.span.clone()).map(str::trim) {
        Some(text) if !text.contains('\n') => text.to_string(),
        _ => format!("![{alt}]({src})"),
    }
}

/// A figure not drawn as an image shows its source, and when drawing it
/// failed, says why above it.
fn figure(
    source: &str,
    error: Option<&str>,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let first = match error {
        Some(error) => {
            let dim = Style::new().add_modifier(Modifier::DIM);
            wrap(
                &[note(format!("{error}; showing the source"))],
                dim,
                width,
                first,
                rest,
                out,
            );
            rest
        }
        None => first,
    };
    preformatted(
        source,
        Style::new().fg(Color::Cyan),
        width,
        first,
        rest,
        out,
    );
}

/// List items, each under a marker: a number counting from `start` for an
/// ordered list, a bullet otherwise, and a checkbox for a task.
fn list(
    start: Option<u64>,
    tight: bool,
    items: &[ListItem],
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    ctx: &mut Ctx,
) {
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
        blocks(&item.blocks, width, &item_first, &item_rest, tight, ctx);
    }
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

/// Text in one style, and the link it is part of as an index into the
/// links [`units`] found.
type Segment = (String, Style, Option<usize>);

/// A unit of inline content for line breaking.
enum Unit {
    /// A run of text with no space to break at, possibly in several styles.
    Word(Vec<Segment>),
    Space(Style, Option<usize>),
    Break,
}

/// The content as units, and the links in it. Text next to text with the
/// same link is one link, even when its style changes inside it.
fn units(content: &[Inline], base: Style) -> (Vec<Unit>, Vec<Link>) {
    let mut units = Units::default();
    for inline in content {
        match inline {
            Inline::Text { text, style } => units.text(text, style, base),
            Inline::Break => units.hard_break(),
        }
    }
    units.finish()
}

/// Inline content turned into units so far.
#[derive(Default)]
struct Units {
    units: Vec<Unit>,
    links: Vec<Link>,
    /// The word being built.
    word: Vec<Segment>,
    /// The link the last inline was part of.
    open: Option<usize>,
}

impl Units {
    fn text(&mut self, text: &str, inline: &InlineStyle, base: Style) {
        let style = inline_style(base, inline);
        self.open = self.link(inline.link.as_ref());
        for c in text.chars() {
            // Code keeps its spaces and is not broken at them, and nor is
            // text at a no-break space.
            if breaks_at(c) && !inline.code {
                self.space(style);
            } else {
                self.push(c, style);
            }
        }
    }

    /// The index of `link` in the links: the open one when it is the same,
    /// otherwise a new one.
    fn link(&mut self, link: Option<&Link>) -> Option<usize> {
        let link = link?;
        if self.open.is_some_and(|i| self.links[i] == *link) {
            return self.open;
        }
        self.links.push(link.clone());
        Some(self.links.len() - 1)
    }

    /// End the word, and add a space unless one is already there.
    fn space(&mut self, style: Style) {
        self.end_word();
        if !matches!(self.units.last(), Some(Unit::Space(..))) {
            self.units.push(Unit::Space(style, self.open));
        }
    }

    /// Add `c` to the word, in the last segment when its style and link match.
    fn push(&mut self, c: char, style: Style) {
        match self.word.last_mut() {
            Some((s, st, l)) if *st == style && *l == self.open => s.push(c),
            _ => self.word.push((c.to_string(), style, self.open)),
        }
    }

    fn hard_break(&mut self) {
        self.end_word();
        self.units.push(Unit::Break);
        self.open = None;
    }

    fn end_word(&mut self) {
        if !self.word.is_empty() {
            self.units.push(Unit::Word(std::mem::take(&mut self.word)));
        }
    }

    /// The units and links. A link with no text drawn but spaces is left
    /// out, as its spaces may merge away or fall at a line break, and is left
    /// out at every width so the links keep their indices across a resize.
    fn finish(mut self) -> (Vec<Unit>, Vec<Link>) {
        self.end_word();
        let mut drawn = vec![false; self.links.len()];
        for unit in &self.units {
            if let Unit::Word(segments) = unit {
                for (s, _, link) in segments {
                    if let Some(i) = link.filter(|_| s.width() > 0) {
                        drawn[i] = true;
                    }
                }
            }
        }
        // Each link's new index, if it is kept.
        let mut index = Vec::with_capacity(drawn.len());
        let mut links = Vec::new();
        for (link, drawn) in self.links.into_iter().zip(drawn) {
            index.push(drawn.then_some(links.len()));
            if drawn {
                links.push(link);
            }
        }
        let renumber = |link: &mut Option<usize>| *link = link.and_then(|i| index[i]);
        for unit in &mut self.units {
            match unit {
                Unit::Word(segments) => segments.iter_mut().for_each(|(.., l)| renumber(l)),
                Unit::Space(_, l) => renumber(l),
                Unit::Break => {}
            }
        }
        (self.units, links)
    }
}

/// Whether a line may break at `c`: any space but a no-break one.
fn breaks_at(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{2007}' | '\u{202f}')
}

/// Fill lines greedily with inline content, breaking at spaces. Returns
/// where the links in it landed.
fn wrap(
    content: &[Inline],
    base: Style,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) -> Vec<LinkSpot> {
    let (units, links) = units(content, base);
    let mut spots: Vec<LinkSpot> = links
        .into_iter()
        .map(|link| LinkSpot {
            link,
            cells: Vec::new(),
        })
        .collect();
    let mut lines = Lines::new(width, first, rest, out);
    // The space before the next word, held back until it is known whether
    // the word fits on the line after it.
    let mut space: Option<(Style, Option<usize>)> = None;
    for unit in units {
        match unit {
            Unit::Break => {
                lines.finish();
                space = None;
            }
            Unit::Space(style, link) => {
                if lines.used > 0 {
                    space = Some((style, link));
                }
            }
            Unit::Word(segments) => lines.word(&segments, space.take(), &mut spots),
        }
    }
    lines.finish_last();
    spots
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
    lines.preformatted(text, style);
    lines.finish_last();
}

/// Code on a panel of colour `bg` reaching to the right edge, with a blank
/// panel row above and below. There is no space inside the panel's left edge,
/// so copied code has no added indent.
fn code_panel(
    code: &str,
    colours: Option<&[Segments]>,
    bg: Color,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let mut lines = Lines::new(width, first, rest, out);
    lines.pad = Some(Style::new().bg(bg));
    lines.finish();
    lines.code(code, colours, Style::new().bg(bg));
    lines.finish();
    lines.finish();
}

/// `text` with its tabs turned to spaces, where it starts at column `col`,
/// which is moved on past it.
fn expand_tabs(text: &str, col: &mut usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c == '\t' {
            let n = TAB_WIDTH - *col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', n));
            *col += n;
        } else {
            out.push(c);
            *col += c.width().unwrap_or(0);
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
    /// When set, each line is filled to the edge with spaces in this style.
    pad: Option<Style>,
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
            pad: None,
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

    /// Push `s`, and when it is part of link `link`, note its cells there.
    fn push_link(&mut self, s: &str, style: Style, link: Option<usize>, spots: &mut [LinkSpot]) {
        let start = spans_width(self.prefix()) + self.used;
        self.push(s, style);
        let Some(spot) = link.and_then(|i| spots.get_mut(i)) else {
            return;
        };
        let row = self.out.len();
        let end = spans_width(self.prefix()) + self.used;
        let (start, end) = (start as u16, end as u16);
        match spot.cells.last_mut() {
            Some((r, cols)) if *r == row && cols.end == start => cols.end = end,
            _ => spot.cells.push((row, start..end)),
        }
    }

    /// Push a word, after `space` when it fits on the line after it and on a
    /// new line when not. A word longer than a whole line is broken anywhere.
    fn word(
        &mut self,
        segments: &[Segment],
        space: Option<(Style, Option<usize>)>,
        spots: &mut [LinkSpot],
    ) {
        let width: usize = segments.iter().map(|(s, ..)| s.width()).sum();
        if let Some((style, link)) = space {
            if self.used + 1 + width > self.avail() {
                self.finish();
            } else {
                self.push_link(" ", style, link, spots);
            }
        }
        let breaking = width > self.avail();
        for (s, style, link) in segments {
            if breaking {
                let mut buf = [0; 4];
                for c in s.chars() {
                    self.break_before(c.width().unwrap_or(0));
                    self.push_link(c.encode_utf8(&mut buf), *style, *link, spots);
                }
            } else {
                self.push_link(s, *style, *link, spots);
            }
        }
    }

    /// Start a new line when one `w` columns wide would not fit.
    fn break_before(&mut self, w: usize) {
        if self.used > 0 && self.used + w > self.avail() {
            self.finish();
        }
    }

    fn push_breaking(&mut self, s: &str, style: Style) {
        let mut buf = [0; 4];
        for c in s.chars() {
            self.break_before(c.width().unwrap_or(0));
            self.push(c.encode_utf8(&mut buf), style);
        }
    }

    /// Text as is, one line per line and broken at the edge. The last line is
    /// left open.
    fn preformatted(&mut self, text: &str, style: Style) {
        let text = text.trim_end_matches('\n');
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                self.finish();
            }
            self.push_breaking(&expand_tabs(line, &mut 0), style);
        }
    }

    /// Code on a `bg` style, in its `colours` when it has them and yellow
    /// otherwise. The last line is left open.
    fn code(&mut self, code: &str, colours: Option<&[Segments]>, bg: Style) {
        let Some(colours) = colours else {
            return self.preformatted(code, CODE.patch(bg));
        };
        for (i, line) in colours.iter().enumerate() {
            if i > 0 {
                self.finish();
            }
            let mut col = 0;
            for (text, style) in line {
                self.push_breaking(&expand_tabs(text, &mut col), bg.patch(*style));
            }
        }
    }

    fn finish(&mut self) {
        if let Some(pad) = self.pad {
            let gap = self.avail().saturating_sub(self.used);
            if gap > 0 {
                self.current.push(Span::styled(" ".repeat(gap), pad));
            }
        }
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
    fn anchor_is_the_innermost_block_or_the_next_one() {
        // A quote (block 0) holding two paragraphs (1 and 2), then one more.
        let layout = render(&parse("> one\n>\n> two\n\nthree\n"), 20, &no_images());
        let rows: Vec<_> = layout.blocks.iter().map(|b| b.rows.clone()).collect();
        assert_eq!(rows, vec![0..3, 0..1, 2..3, 4..5]);
        assert_eq!(layout.anchor(0), Some((1, 0)));
        // Inside the quote but between its paragraphs.
        assert_eq!(layout.anchor(1), Some((2, -1)));
        assert_eq!(layout.anchor(2), Some((2, 0)));
        // Between blocks.
        assert_eq!(layout.anchor(3), Some((3, -1)));
        assert_eq!(layout.anchor(5), None);
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

    const BG: Color = Color::Rgb(48, 48, 63);

    fn panel(md: &str, width: u16) -> Vec<Line<'static>> {
        layout(&parse(md), width, &no_images(), Some(BG), None).lines
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn code_keeps_lines_and_breaks_at_the_edge() {
        assert_eq!(
            text(&panel("```\nlet x = 1;\n\tindented\n```\n", 8)),
            vec![
                "        ", "let x = ", "1;      ", "    inde", "nted    ", "        ",
            ]
        );
    }

    #[test]
    fn code_panel_colours_every_cell_to_the_edge() {
        let lines = panel("text\n\n```\nab\n\ncd\n```\n\nmore\n", 6);
        assert_eq!(
            text(&lines),
            vec![
                "text", "", "      ", "ab    ", "      ", "cd    ", "      ", "", "more"
            ]
        );
        for line in &lines[2..7] {
            assert_eq!(line.width(), 6);
            assert!(
                line.spans.iter().all(|s| s.style.bg == Some(BG)),
                "{line:?}"
            );
        }
        assert_eq!(lines[3].spans[0].style, CODE.bg(BG));
        assert!(lines[0].spans.iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn code_panel_starts_after_list_and_quote_prefixes() {
        assert_eq!(
            text(&panel("- ```\n  ab\n  ```\n", 8)),
            vec!["•       ", "  ab    ", "        "]
        );
        let lines = panel("> ```\n> ab\n> ```\n", 8);
        assert_eq!(text(&lines), vec!["│       ", "│ ab    ", "│       "]);
        for line in &lines {
            assert_eq!(line.spans[0].content, "│ ");
            assert_eq!(line.spans[0].style.bg, None);
            assert!(line.spans[1..].iter().all(|s| s.style.bg == Some(BG)));
        }
    }

    #[test]
    fn code_is_indented_when_the_background_is_unknown() {
        let lines = layout(
            &parse("```\nlet x = 1;\n\nz\n```\n"),
            8,
            &no_images(),
            None,
            None,
        )
        .lines;
        assert_eq!(text(&lines), vec!["  let x ", "  = 1;", "", "  z"]);
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .all(|s| s.style.bg.is_none())
        );
    }

    #[test]
    fn only_code_blocks_get_the_panel() {
        let lines = layout(
            &parse("```mermaid\ngraph TD\n```\n\n<div>x</div>\n"),
            40,
            &Images::text(),
            Some(BG),
            None,
        )
        .lines;
        assert_eq!(text(&lines), vec!["graph TD", "", "<div>x</div>"]);
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .all(|s| s.style.bg.is_none())
        );
    }

    #[test]
    fn inline_code_keeps_its_spaces() {
        assert_eq!(plain("run `a  b c` now\n", 8), vec!["run", "a  b c", "now"]);
    }

    type Cells = Vec<(usize, Range<u16>)>;

    /// Each link in the layout as its destination and the cells it takes.
    fn spots(md: &str, width: u16) -> Vec<(String, Cells)> {
        render(&parse(md), width, &no_images())
            .links
            .into_iter()
            .map(|l| (l.link.dest, l.cells))
            .collect()
    }

    fn styled_md(md: &str, width: u16) -> Vec<String> {
        styled(&render(&parse(md), width, &no_images()).lines)
    }

    #[test]
    fn a_hard_break_ends_the_line_and_drops_the_space_before_it() {
        assert_eq!(plain("one  \ntwo three\n", 20), vec!["one", "two three"]);
        assert_eq!(plain("ab\\\ncd\n", 20), vec!["ab", "cd"]);
        assert_eq!(plain("a  \n  \nb\n", 20), vec!["a", "", "b"]);
    }

    #[test]
    fn a_hard_break_inside_a_link_splits_it_in_two() {
        assert_eq!(
            spots("[one  \ntwo](x)\n", 20),
            vec![
                ("x".to_string(), vec![(0, 0..3)]),
                ("x".to_string(), vec![(1, 0..3)])
            ]
        );
    }

    #[test]
    fn a_link_is_one_link_across_style_changes_and_spaces() {
        assert_eq!(
            spots("go [a **b** c](x) now\n", 20),
            vec![("x".to_string(), vec![(0, 3..8)])]
        );
        assert_eq!(
            styled_md("go [a **b** c](x) now\n", 20),
            vec![
                "[go |][a |fg=Blue,UNDERLINED][b|fg=Blue,BOLD | UNDERLINED][ c|fg=Blue,UNDERLINED][ now|]"
            ]
        );
    }

    #[test]
    fn neighbouring_links_merge_only_when_they_are_the_same() {
        assert_eq!(
            spots("[a](x)[b](x) [c](y)[d](z)\n", 20),
            vec![
                ("x".to_string(), vec![(0, 0..2)]),
                ("y".to_string(), vec![(0, 3..4)]),
                ("z".to_string(), vec![(0, 4..5)])
            ]
        );
        // The same link again after other text is a second link.
        assert_eq!(
            spots("[a](x) b [c](x)\n", 20),
            vec![
                ("x".to_string(), vec![(0, 0..1)]),
                ("x".to_string(), vec![(0, 4..5)])
            ]
        );
    }

    #[test]
    fn a_wrapped_link_takes_cells_on_each_line_without_the_broken_space() {
        assert_eq!(
            spots("see [one two three](x)\n", 9),
            vec![("x".to_string(), vec![(0, 4..7), (1, 0..9)])]
        );
        assert_eq!(
            spots("- [abcdefgh](x)\n", 6),
            vec![("x".to_string(), vec![(0, 2..6), (1, 2..6)])]
        );
    }

    #[test]
    fn no_break_spaces_keep_words_together() {
        assert_eq!(plain("go 10&nbsp;km\n", 6), vec!["go", "10\u{a0}km"]);
        assert_eq!(plain("a\u{a0}\u{a0}b\n", 20), vec!["a\u{a0}\u{a0}b"]);
    }

    #[test]
    fn a_link_with_no_cells_is_left_out() {
        // Its space merges into the one before it, so nothing is drawn for it.
        assert_eq!(
            spots("a [ ](x) [b](y)\n", 20),
            vec![("y".to_string(), vec![(0, 2..3)])]
        );
        assert_eq!(spots("[ ](x) b\n", 20), vec![]);
    }

    #[test]
    fn the_links_are_the_same_at_every_width() {
        let md = "[c](y) aaaa[ ](x)bbbb [d](z) [\u{200b}](w)\n";
        for width in [6, 20] {
            let dests: Vec<String> = spots(md, width).into_iter().map(|(d, _)| d).collect();
            assert_eq!(dests, vec!["y", "z"], "width {width}");
        }
    }

    #[test]
    fn a_style_change_inside_a_word_does_not_break_it() {
        assert_eq!(plain("xx a**b**c\n", 4), vec!["xx", "abc"]);
        assert_eq!(styled_md("a**b**c\n", 4), vec!["[a|][b|BOLD][c|]"]);
    }

    #[test]
    fn runs_of_space_become_one_space_in_the_first_style() {
        assert_eq!(
            styled_md("a\n*b* **c** d\n", 20),
            vec!["[a |][b|ITALIC][ |][c|BOLD][ d|]"]
        );
        // A space ending a link and the one after it: the link keeps it.
        assert_eq!(
            styled_md("[a ](x)  b\n", 20),
            vec!["[a |fg=Blue,UNDERLINED][b|]"]
        );
        assert_eq!(
            spots("[a ](x)  b\n", 20),
            vec![("x".to_string(), vec![(0, 0..2)])]
        );
    }

    #[test]
    fn code_spans_keep_their_spaces_in_links_too() {
        assert_eq!(
            spots("x [`a  b`](y) z\n", 20),
            vec![("y".to_string(), vec![(0, 2..6)])]
        );
        assert_eq!(plain("x `a  b` z\n", 5), vec!["x", "a  b", "z"]);
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
    fn a_wide_table_wraps_to_the_width_left_by_its_prefix() {
        assert_eq!(
            plain(
                "> | name | note |\n> |---|---|\n> | a | one two three four |\n",
                24
            ),
            vec![
                "│ ┌──────┬─────────────┐",
                "│ │ name │ note        │",
                "│ ├──────┼─────────────┤",
                "│ │ a    │ one two     │",
                "│ │      │ three four  │",
                "│ └──────┴─────────────┘",
            ]
        );
    }

    #[test]
    fn a_table_is_text_even_with_graphics() {
        let md = "| alpha | beta |\n|---|---|\n| 1 | 2 |\n";
        let doc = parse(md);
        assert_eq!(images::key(&doc.blocks[0]), None);
        for width in [40, 15, 8] {
            let graphics = render(&doc, width, &Images::new((10, 20)));
            let text = render(&doc, width, &Images::text());
            assert!(graphics.placements().is_empty());
            assert_eq!(graphics.lines, text.lines, "width {width}");
        }
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
        // Tables are drawn, not shown as code, and fit the width.
        for row in [4, 8] {
            assert_ne!(layout.lines[row].spans[0].style, CODE, "row {row}");
        }
        let wide = render(&parse("| aaaa | bbbb |\n|---|---|\n"), 8, &Images::text());
        let text: Vec<String> = wide.lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().all(|l| l.width() <= 8), "{text:?}");
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
        let layout = render(&parse("![w](wide.png)\n\n> ![w](wide.png)\n"), 41, &images);
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
        for panel in [None, Some(BG)] {
            for width in 8..30u16 {
                for line in layout(&parse(md), width, &no_images(), panel, None).lines {
                    assert!(line.width() <= width as usize, "width {width}: {line:?}");
                }
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

    fn highlighted(md: &str, width: u16, ready: bool) -> Vec<Line<'static>> {
        let set = ready.then(highlight::syntax_set);
        layout(&parse(md), width, &no_images(), Some(BG), set).lines
    }

    /// The colour of the first piece of a line holding `text`.
    fn fg_of(lines: &[Line], text: &str) -> Option<Color> {
        let span = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains(text));
        span.unwrap().style.fg
    }

    #[test]
    fn code_is_coloured_by_its_language_on_the_panel() {
        let md = "```rust\n// note\nfn add(a: Vec<u8>) { \"hi\"; 1; add(2) }\n```\n";
        let lines = highlighted(md, 60, true);
        let dim = lines[1].spans[0].style;
        assert!(dim.add_modifier.contains(Modifier::DIM), "{dim:?}");
        let code = &lines[2..3];
        assert_eq!(fg_of(code, "fn"), Some(Color::Magenta));
        assert_eq!(fg_of(code, "add"), Some(Color::Blue));
        assert_eq!(fg_of(code, "Vec"), Some(Color::Yellow));
        assert_eq!(fg_of(code, "hi"), Some(Color::Green));
        assert_eq!(fg_of(code, "1"), Some(Color::Cyan));
        for line in &lines {
            assert_eq!(line.width(), 60, "{line:?}");
            assert!(
                line.spans.iter().all(|s| s.style.bg == Some(BG)),
                "{line:?}"
            );
        }
    }

    #[test]
    fn diff_lines_are_coloured_by_their_first_character() {
        let md =
            "```diff\ndiff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n same\n-old\n+new\n```\n";
        let lines = highlighted(md, 30, true);
        let style = |row: usize| lines[row].spans[0].style;
        for row in 1..=3 {
            assert!(
                style(row).add_modifier.contains(Modifier::BOLD),
                "row {row}"
            );
        }
        assert_eq!(style(4).fg, Some(Color::Cyan));
        assert_eq!(style(5).fg, None);
        assert_eq!(style(6).fg, Some(Color::Red));
        assert_eq!(style(7).fg, Some(Color::Green));
        assert!(
            lines
                .iter()
                .all(|l| l.spans.iter().all(|s| s.style.bg == Some(BG)))
        );
        let patch = highlighted("```patch\n+new\n```\n", 30, false);
        assert_eq!(patch[1].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn unknown_or_missing_languages_stay_yellow() {
        for md in ["```nosuchlang\nlet x = 1;\n```\n", "```\nlet x = 1;\n```\n"] {
            let lines = highlighted(md, 20, true);
            assert_eq!(lines[1].spans[0].style, CODE.bg(BG), "{md}");
        }
    }

    #[test]
    fn code_is_yellow_until_the_syntaxes_are_ready() {
        let lines = highlighted("```rust\nfn main() {}\n```\n", 20, false);
        assert_eq!(lines[1].spans[0].style, CODE.bg(BG));
    }

    #[test]
    fn highlighted_lines_break_at_the_edge_in_their_colours() {
        let md = "```rust\nlet s = \"abcdefghijkl\";\n\tlet t = 1;\n```\n";
        let lines = highlighted(md, 10, true);
        assert_eq!(
            text(&lines),
            vec![
                "          ",
                "let s = \"a",
                "bcdefghijk",
                "l\";       ",
                "    let t ",
                "= 1;      ",
                "          ",
            ]
        );
        for line in &lines {
            assert_eq!(line.width(), 10, "{line:?}");
        }
        assert_eq!(fg_of(&lines[2..3], "bcd"), Some(Color::Green));
        assert_eq!(fg_of(&lines[3..4], "l"), Some(Color::Green));
        assert_eq!(fg_of(&lines[4..5], "let"), Some(Color::Magenta));
    }

    /// Each line as its text, then each span's text and style in brackets,
    /// so a test pins both what is drawn and how.
    fn styled(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let spans: Vec<String> = l
                    .spans
                    .iter()
                    .map(|s| {
                        let st = s.style;
                        let mut tags = Vec::new();
                        if let Some(fg) = st.fg {
                            tags.push(format!("fg={fg}"));
                        }
                        if let Some(bg) = st.bg {
                            tags.push(format!("bg={bg}"));
                        }
                        if !st.add_modifier.is_empty() {
                            tags.push(format!("{:?}", st.add_modifier));
                        }
                        format!("[{}|{}]", s.content, tags.join(","))
                    })
                    .collect();
                spans.concat()
            })
            .collect()
    }

    /// A document with every kind of block `contents` handles.
    const EVERY_KIND: &str = "\
# One [a](x)

## Two

### Three

#### Four

text with [a link](y) in it

---

<div>x</div>

3. three
4. [ ] open
5.

- loose

- [x] done

> - in a quote
>
>   ---

```mermaid
graph TD
```

- ![two
  lines](cat.png)

> ```
> code
> ```
";

    /// In text mode every block is text: headings, links, rules, HTML,
    /// ordered, task, empty and loose list items, quotes, figures and images
    /// as their source, and code indented inside a quote with no panel.
    #[test]
    fn every_block_kind_without_graphics_is_pinned() {
        let layout = layout(&parse(EVERY_KIND), 16, &Images::text(), None, None);
        let got = styled(&layout.lines);
        let want = vec![
            "[One |fg=Magenta,BOLD | UNDERLINED][a|fg=Blue,BOLD | UNDERLINED]",
            "",
            "[Two|fg=Cyan,BOLD]",
            "",
            "[Three|fg=Green,BOLD]",
            "",
            "[Four|BOLD]",
            "",
            "[text with |][a link|fg=Blue,UNDERLINED]",
            "[in it|]",
            "",
            "[────────────────|DIM]",
            "",
            "[<div>x</div>|DIM]",
            "",
            "[3. |fg=Blue][three|]",
            "[4. [ ] |fg=Blue][open|]",
            "[5.|fg=Blue]",
            "",
            "[• |fg=Blue][loose|]",
            "",
            "[• [x] |fg=Blue][done|]",
            "",
            "[│ |fg=DarkGray][• |fg=Blue][in a quote|]",
            "[│|fg=DarkGray]",
            "[│ |fg=DarkGray][  |][────────────|DIM]",
            "",
            "[graph TD|fg=Yellow]",
            "",
            "[• |fg=Blue][![two lines](c|fg=Yellow]",
            "[  |][at.png)|fg=Yellow]",
            "",
            "[│ |fg=DarkGray][  |][code|fg=Yellow]",
        ];
        assert_eq!(got, want);
        let links: Vec<_> = layout
            .links
            .iter()
            .map(|l| (l.link.dest.clone(), l.cells.clone()))
            .collect();
        assert_eq!(
            links,
            vec![
                ("x".to_string(), vec![(0, 4..5)]),
                ("y".to_string(), vec![(8, 10..16)])
            ]
        );
        let rows: Vec<_> = layout.blocks.iter().map(|b| b.rows.clone()).collect();
        assert_eq!(
            rows,
            vec![
                0..1,
                2..3,
                4..5,
                6..7,
                8..10,
                11..12,
                13..14,
                15..18,
                15..16,
                16..17,
                19..22,
                19..20,
                21..22,
                23..26,
                23..26,
                23..24,
                25..26,
                27..28,
                29..31,
                29..31,
                32..33,
                32..33,
            ]
        );
    }

    /// With graphics but nothing ready, figures show their source in cyan and
    /// images their alt text.
    #[test]
    fn every_block_kind_with_graphics_not_ready_is_pinned() {
        let layout = layout(&parse(EVERY_KIND), 16, &no_images(), None, None);
        let got = styled(&layout.lines);
        let want = vec![
            "[One |fg=Magenta,BOLD | UNDERLINED][a|fg=Blue,BOLD | UNDERLINED]",
            "",
            "[Two|fg=Cyan,BOLD]",
            "",
            "[Three|fg=Green,BOLD]",
            "",
            "[Four|BOLD]",
            "",
            "[text with |][a link|fg=Blue,UNDERLINED]",
            "[in it|]",
            "",
            "[────────────────|DIM]",
            "",
            "[<div>x</div>|DIM]",
            "",
            "[3. |fg=Blue][three|]",
            "[4. [ ] |fg=Blue][open|]",
            "[5.|fg=Blue]",
            "",
            "[• |fg=Blue][loose|]",
            "",
            "[• [x] |fg=Blue][done|]",
            "",
            "[│ |fg=DarkGray][• |fg=Blue][in a quote|]",
            "[│|fg=DarkGray]",
            "[│ |fg=DarkGray][  |][────────────|DIM]",
            "",
            "[graph TD|fg=Cyan]",
            "",
            "[• |fg=Blue][[image: two|]",
            "[  |][lines]|]",
            "",
            "[│ |fg=DarkGray][  |][code|fg=Yellow]",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn code_is_coloured_without_a_panel_too() {
        let md = "```rust\nfn main() {}\n```\n";
        let set = Some(highlight::syntax_set());
        let lines = layout(&parse(md), 20, &no_images(), None, set).lines;
        assert_eq!(fg_of(&lines, "fn"), Some(Color::Magenta));
        assert!(
            lines
                .iter()
                .all(|l| l.spans.iter().all(|s| s.style.bg.is_none()))
        );
    }
}
