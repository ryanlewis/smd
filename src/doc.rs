//! The document model: a markdown file parsed into a list of blocks.
//!
//! Every block keeps the byte range it came from, so a later layer can swap a
//! block (a figure, or a heading) for an image and still reach its source.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};

use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub source: String,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    /// Byte range of the block in `Document::source`.
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Code {
        lang: Option<String>,
        code: String,
    },
    Quote(Vec<Block>),
    List {
        /// The number of the first item for an ordered list.
        start: Option<u64>,
        /// Tight lists have no blank lines between or inside their items.
        tight: bool,
        items: Vec<ListItem>,
    },
    Rule,
    Html(String),
    /// A paragraph holding nothing but one image. Until the image is ready,
    /// the block is shown as its alt text.
    Image {
        src: String,
        alt: String,
        /// An Obsidian embed, `![[name.png]]`, whose `src` is a name to look
        /// up in the vault rather than a path.
        wiki: bool,
    },
    /// A block the graphics layer will draw as an image. Until it does, the
    /// block is shown as its source text.
    Figure {
        kind: FigureKind,
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    /// `Some(done)` for a task list item.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FigureKind {
    Mermaid,
    Math,
    Table,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text {
        text: String,
        style: InlineStyle,
    },
    /// A hard line break.
    Break,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub emphasis: bool,
    pub strong: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub math: bool,
    pub link: Option<Link>,
}

/// Where a link goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The URL or path as written, or for a wiki link the note's name, which
    /// may have a `#heading` after it.
    pub dest: String,
    /// An Obsidian wiki link, `[[name]]` or `[[name|text]]`.
    pub wiki: bool,
}

impl From<&str> for Link {
    fn from(dest: &str) -> Link {
        Link {
            dest: dest.to_string(),
            wiki: false,
        }
    }
}

/// Whether `name` is an image file, by its extension. A `#` fragment after
/// it is ignored.
pub fn is_image(name: &str) -> bool {
    let name = without_fragment(name);
    let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    ["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "avif"]
        .iter()
        .any(|e| ext.eq_ignore_ascii_case(e))
}

/// The end of an image's address that says a page shows it only on the
/// background of the theme other than `theme`.
fn other_theme(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "#gh-light-mode-only",
        Theme::Light => "#gh-dark-mode-only",
    }
}

/// What an image tag stands for.
#[derive(Debug, PartialEq)]
pub enum Embed {
    /// An embedded note, shown as a link to it.
    Note(Link),
    /// An image, loaded from [`image_src`].
    Image,
}

/// What an image tag to `dest` stands for. A wiki embed of anything but an
/// image file embeds a note.
pub fn embed(dest: &str, wiki: bool) -> Embed {
    if !wiki {
        return Embed::Image;
    }
    let dest = link_dest(dest, true);
    if is_image(&dest) {
        Embed::Image
    } else {
        Embed::Note(Link {
            dest: dest.into_owned(),
            wiki: true,
        })
    }
}

/// The file an image tag loads: its path or, for a wiki embed, its name in
/// the vault without a fragment.
pub fn image_src(dest: &str, wiki: bool) -> String {
    if wiki {
        without_fragment(&link_dest(dest, true)).to_string()
    } else {
        dest.to_string()
    }
}

/// Where a link or embed goes, as the parser gives it. A wiki link's name
/// loses a backslash at its end, which is left there when the pipe before an
/// alias is escaped, `\|`, as it must be in a table. The rest is
/// [`tidy_dest`].
pub fn link_dest(dest: &str, wiki: bool) -> Cow<'_, str> {
    let dest = if wiki {
        dest.strip_suffix('\\').unwrap_or(dest)
    } else {
        dest
    };
    tidy_dest(dest, wiki)
}

/// A link's destination without spaces around it, and for a wiki link
/// without the space before a `#` fragment, `[[Note #Heading]]`. Tidying it
/// again changes nothing, so a link can be tidied where it is read and where
/// it is followed.
pub fn tidy_dest(dest: &str, wiki: bool) -> Cow<'_, str> {
    let dest = dest.trim();
    if !wiki {
        return Cow::Borrowed(dest);
    }
    match dest.split_once('#') {
        Some((name, fragment)) if name.ends_with(char::is_whitespace) => {
            Cow::Owned(format!("{}#{fragment}", name.trim_end()))
        }
        _ => Cow::Borrowed(dest),
    }
}

/// `name` without a `#heading` or `#^block` after it.
fn without_fragment(name: &str) -> &str {
    name.split_once('#').map_or(name, |(name, _)| name)
}

/// `source` parsed with no theme known.
#[cfg(test)]
pub fn parse(source: &str) -> Document {
    parse_for(source, None)
}

/// `source` parsed for a background of `theme`, when the terminal reported
/// it. An image a page on the web shows only on the other theme's
/// background, such as one of a README's two logos, is left out, with a
/// link, paragraph, quote, list item or list it leaves empty.
pub fn parse_for(source: &str, theme: Option<Theme>) -> Document {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_WIKILINKS;
    let mut builder = Builder::new(source);
    builder.hide = theme.map(other_theme);
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        builder.event(event, range);
    }
    builder.finish()
}

struct Builder<'a> {
    source: &'a str,
    stack: Vec<Frame>,
    emphasis: u32,
    strong: u32,
    strikethrough: u32,
    links: Vec<Link>,
    in_table: bool,
    /// The image being read: its source, alt text so far, and whether it is a
    /// wiki embed.
    image: Option<(String, String, bool)>,
    image_depth: u32,
    /// Images ending with this are left out.
    hide: Option<&'static str>,
    /// How deep in an image being left out.
    hidden: u32,
    /// For each image started and not yet ended, whether it was a wiki embed
    /// of a note, read as a link instead.
    embeds: Vec<bool>,
}

struct Frame {
    kind: FrameKind,
    span: Range<usize>,
    blocks: Vec<Block>,
    /// Inline content of a paragraph or heading, or the text of a tight list
    /// item, which pulldown-cmark does not wrap in a paragraph.
    inlines: Vec<Inline>,
    inline_span: Option<Range<usize>>,
    /// Display maths and images (source, alt text) seen in `inlines`, and
    /// whether anything else was.
    display_math: Vec<String>,
    images: Vec<(String, String, bool)>,
    other_inline: bool,
    /// Whether an image or block in it was left out for the theme.
    left_out: bool,
}

enum FrameKind {
    Root,
    Quote,
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<ListItem>,
    },
    Item {
        task: Option<bool>,
    },
    Paragraph,
    Heading(u8),
    Code {
        lang: Option<String>,
        code: String,
    },
    Html(String),
}

impl Frame {
    fn new(kind: FrameKind, span: Range<usize>) -> Self {
        Frame {
            kind,
            span,
            blocks: Vec::new(),
            inlines: Vec::new(),
            inline_span: None,
            display_math: Vec::new(),
            images: Vec::new(),
            other_inline: false,
            left_out: false,
        }
    }

    /// Turn loose inline content (a tight list item's text) into a paragraph.
    fn flush_inlines(&mut self) {
        if self.inlines.is_empty() {
            return;
        }
        let span = self.inline_span.take().unwrap_or(self.span.clone());
        let blocks = paragraph_blocks(self, span);
        self.blocks.extend(blocks);
    }
}

/// A paragraph that holds nothing but display maths becomes maths figures,
/// and one that holds nothing but one image becomes an image block.
fn paragraph_blocks(frame: &mut Frame, span: Range<usize>) -> Vec<Block> {
    let inlines = std::mem::take(&mut frame.inlines);
    let maths = std::mem::take(&mut frame.display_math);
    let mut images = std::mem::take(&mut frame.images);
    let other = std::mem::replace(&mut frame.other_inline, false);
    if frame.left_out && maths.is_empty() && images.is_empty() && !other {
        return Vec::new();
    }
    if maths.is_empty() && images.len() == 1 && !other {
        let (src, alt, wiki) = images.remove(0);
        return vec![Block {
            kind: BlockKind::Image { src, alt, wiki },
            span,
        }];
    }
    if !maths.is_empty() && images.is_empty() && !other {
        return maths
            .into_iter()
            .map(|source| Block {
                kind: BlockKind::Figure {
                    kind: FigureKind::Math,
                    source,
                },
                span: span.clone(),
            })
            .collect();
    }
    vec![Block {
        kind: BlockKind::Paragraph(inlines),
        span,
    }]
}

impl<'a> Builder<'a> {
    fn new(source: &'a str) -> Self {
        Builder {
            source,
            stack: vec![Frame::new(FrameKind::Root, 0..source.len())],
            emphasis: 0,
            strong: 0,
            strikethrough: 0,
            links: Vec::new(),
            in_table: false,
            image: None,
            image_depth: 0,
            hide: None,
            hidden: 0,
            embeds: Vec::new(),
        }
    }

    fn finish(mut self) -> Document {
        while self.stack.len() > 1 {
            self.close();
        }
        let mut root = self.stack.pop().expect("root frame");
        root.flush_inlines();
        Document {
            source: self.source.to_string(),
            blocks: root.blocks,
        }
    }

    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("root frame")
    }

    fn style(&self) -> InlineStyle {
        InlineStyle {
            emphasis: self.emphasis > 0,
            strong: self.strong > 0,
            strikethrough: self.strikethrough > 0,
            link: self.links.last().cloned(),
            ..InlineStyle::default()
        }
    }

    /// The current style with `code` or `math` added.
    fn style_with(&self, code: bool, math: bool) -> InlineStyle {
        InlineStyle {
            code,
            math,
            ..self.style()
        }
    }

    fn event(&mut self, event: Event, range: Range<usize>) {
        // A table is kept whole as source text, so skip its contents.
        if self.in_table {
            if matches!(event, Event::End(TagEnd::Table)) {
                self.in_table = false;
            }
            return;
        }
        // An image left out takes its alt text with it.
        if self.hidden > 0 {
            match event {
                Event::Start(Tag::Image { .. }) => self.hidden += 1,
                Event::End(TagEnd::Image) => self.hidden -= 1,
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.top().kind {
                FrameKind::Code { code, .. } => code.push_str(&text),
                _ => self.text(&text, self.style(), range),
            },
            Event::Html(html) => match &mut self.top().kind {
                FrameKind::Html(buf) => buf.push_str(&html),
                _ => self.text(&html, self.style(), range),
            },
            Event::InlineHtml(html) => self.text(&html, self.style(), range),
            Event::Code(code) => self.text(&code, self.style_with(true, false), range),
            Event::InlineMath(math) => {
                self.text(&format!("${math}$"), self.style_with(false, true), range);
            }
            Event::DisplayMath(math) => self.display_math(&math, range),
            Event::FootnoteReference(label) => {
                self.text(&format!("[^{label}]"), self.style(), range);
            }
            Event::SoftBreak => self.text(" ", self.style(), range),
            Event::HardBreak => self.inline(Inline::Break, range),
            Event::Rule => self.push_block(BlockKind::Rule, range),
            Event::TaskListMarker(done) => self.mark_task(done),
        }
    }

    fn display_math(&mut self, math: &str, range: Range<usize>) {
        // Display maths alone does not make a paragraph prose.
        let other = self.top().other_inline;
        self.text(&format!("$${math}$$"), self.style_with(false, true), range);
        let top = self.top();
        top.other_inline = other;
        top.display_math.push(math.trim().to_string());
    }

    fn start(&mut self, tag: Tag, range: Range<usize>) {
        match tag {
            Tag::Table(_) => self.start_table(range),
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strikethrough += 1,
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                let wiki = is_wiki(link_type);
                self.links.push(Link {
                    dest: link_dest(&dest_url, wiki).into_owned(),
                    wiki,
                });
            }
            Tag::Image {
                link_type,
                dest_url,
                ..
            } => self.start_image(is_wiki(link_type), &dest_url, range),
            tag => {
                if let Some(kind) = self.block_frame(tag) {
                    self.top().flush_inlines();
                    self.stack.push(Frame::new(kind, range));
                }
            }
        }
    }

    /// The frame a block-level tag opens, or `None` for a tag that is not
    /// enabled or is kept as plain inline text.
    fn block_frame(&mut self, tag: Tag) -> Option<FrameKind> {
        Some(match tag {
            Tag::Paragraph => {
                if let FrameKind::Item { .. } = self.top().kind {
                    self.mark_list_loose();
                }
                FrameKind::Paragraph
            }
            Tag::Heading { level, .. } => FrameKind::Heading(level as u8),
            Tag::BlockQuote(_) => FrameKind::Quote,
            Tag::CodeBlock(kind) => FrameKind::Code {
                lang: code_lang(kind),
                code: String::new(),
            },
            Tag::HtmlBlock => FrameKind::Html(String::new()),
            Tag::List(start) => FrameKind::List {
                start,
                tight: true,
                items: Vec::new(),
            },
            Tag::Item => FrameKind::Item { task: None },
            _ => return None,
        })
    }

    fn start_table(&mut self, range: Range<usize>) {
        let source = table_source(self.source, range.clone());
        let kind = BlockKind::Figure {
            kind: FigureKind::Table,
            source,
        };
        self.push_block(kind, range);
        self.in_table = true;
    }

    fn start_image(&mut self, wiki: bool, dest: &str, range: Range<usize>) {
        if let Some(end) = self.hide
            && link_dest(dest, wiki).ends_with(end)
            && embed(dest, wiki) == Embed::Image
        {
            self.hidden = 1;
            self.top().left_out = true;
            return;
        }
        if let Embed::Note(link) = embed(dest, wiki) {
            self.embeds.push(true);
            self.links.push(link);
            return;
        }
        self.embeds.push(false);
        // A linked image stays inline so it keeps its link.
        if !self.links.is_empty() {
            self.top().other_inline = true;
        }
        self.push_text("[image: ", self.style(), range);
        self.image_depth += 1;
        if self.image_depth == 1 {
            self.image = Some((image_src(dest, wiki), String::new(), wiki));
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item => self.close(),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strikethrough = self.strikethrough.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => self.end_image(),
            _ => {}
        }
    }

    fn end_image(&mut self) {
        if self.embeds.pop() == Some(true) {
            self.links.pop();
            return;
        }
        self.image_depth = self.image_depth.saturating_sub(1);
        if self.image_depth == 0
            && let Some(image) = self.image.take()
        {
            self.top().images.push(image);
        }
        let end = self.top().inline_span.as_ref().map_or(0, |s| s.end);
        self.push_text("]", self.style(), end..end);
    }

    /// Pop the top frame and add what it built to its parent.
    fn close(&mut self) {
        let mut frame = self.stack.pop().expect("frame to close");
        let span = frame.span.clone();
        // A block left empty by an image left out for the theme goes too.
        let emptied = |frame: &Frame| frame.left_out && frame.blocks.is_empty();
        if frame.left_out {
            self.top().left_out = true;
        }
        let kind = match std::mem::replace(&mut frame.kind, FrameKind::Root) {
            FrameKind::Root => unreachable!("root frame is never closed"),
            FrameKind::Paragraph => {
                let blocks = paragraph_blocks(&mut frame, span);
                self.top().blocks.extend(blocks);
                return;
            }
            FrameKind::Item { task } => {
                frame.flush_inlines();
                if emptied(&frame) {
                    return;
                }
                if let FrameKind::List { items, .. } = &mut self.top().kind {
                    items.push(ListItem {
                        task,
                        blocks: frame.blocks,
                    });
                }
                return;
            }
            FrameKind::Heading(_) if frame.left_out && !frame.other_inline => return,
            FrameKind::Heading(level) => BlockKind::Heading {
                level,
                content: frame.inlines,
            },
            FrameKind::Quote => {
                frame.flush_inlines();
                if emptied(&frame) {
                    return;
                }
                BlockKind::Quote(frame.blocks)
            }
            FrameKind::Code { lang, code } => code_block(lang, code),
            FrameKind::Html(html) => BlockKind::Html(html),
            FrameKind::List { items, .. } if frame.left_out && items.is_empty() => return,
            FrameKind::List {
                start,
                tight,
                items,
            } => BlockKind::List {
                start,
                tight,
                items,
            },
        };
        self.push_block(kind, span);
    }

    fn push_block(&mut self, kind: BlockKind, span: Range<usize>) {
        let top = self.top();
        top.flush_inlines();
        top.blocks.push(Block { kind, span });
    }

    /// Mark the open list item as a task. In a loose list the marker comes
    /// inside the item's paragraph, so the item is the frame below it.
    fn mark_task(&mut self, done: bool) {
        let item = match self.top().kind {
            FrameKind::Item { .. } => self.stack.last_mut(),
            _ => self.under_top(),
        };
        if let Some(Frame {
            kind: FrameKind::Item { task },
            ..
        }) = item
        {
            *task = Some(done);
        }
    }

    /// Mark the list holding the item on top of the stack as loose.
    fn mark_list_loose(&mut self) {
        if let Some(Frame {
            kind: FrameKind::List { tight, .. },
            ..
        }) = self.under_top()
        {
            *tight = false;
        }
    }

    /// The frame below the top one: the item holding a paragraph, or the
    /// list holding an item.
    fn under_top(&mut self) -> Option<&mut Frame> {
        let len = self.stack.len();
        len.checked_sub(2).map(|i| &mut self.stack[i])
    }

    fn text(&mut self, text: &str, style: InlineStyle, range: Range<usize>) {
        // An image's alt text does not make its paragraph prose.
        if let Some((_, alt, _)) = &mut self.image {
            alt.push_str(text);
        } else if !text.trim().is_empty() {
            self.top().other_inline = true;
        }
        self.push_text(text, style, range);
    }

    /// Add inline text without counting it as prose.
    fn push_text(&mut self, text: &str, style: InlineStyle, range: Range<usize>) {
        if text.is_empty() {
            return;
        }
        let top = self.top();
        if let Some(Inline::Text {
            text: last,
            style: s,
        }) = top.inlines.last_mut()
            && *s == style
        {
            last.push_str(text);
        } else {
            top.inlines.push(Inline::Text {
                text: text.to_string(),
                style,
            });
        }
        extend_span(&mut top.inline_span, range);
    }

    fn inline(&mut self, inline: Inline, range: Range<usize>) {
        let top = self.top();
        top.inlines.push(inline);
        extend_span(&mut top.inline_span, range);
    }
}

pub fn is_wiki(link_type: LinkType) -> bool {
    matches!(link_type, LinkType::WikiLink { .. })
}

/// The language of a code block: the first word of a fence's info string,
/// without attributes after a comma such as `rust,no_run`.
fn code_lang(kind: CodeBlockKind) -> Option<String> {
    match kind {
        CodeBlockKind::Fenced(info) => info
            .split_whitespace()
            .next()
            .and_then(|word| word.split(',').next())
            .map(str::to_string),
        CodeBlockKind::Indented => None,
    }
}

/// A fenced block in `mermaid` or `math`, in any case, is a figure; any
/// other is code.
fn code_block(lang: Option<String>, code: String) -> BlockKind {
    let figure = match lang.as_deref() {
        Some(n) if n.eq_ignore_ascii_case("mermaid") => FigureKind::Mermaid,
        Some(n) if n.eq_ignore_ascii_case("math") => FigureKind::Math,
        _ => return BlockKind::Code { lang, code },
    };
    BlockKind::Figure {
        kind: figure,
        source: code,
    }
}

/// A table's markdown without the quote markers or list indent that sit in
/// front of every line after the first, and with plain line endings.
fn table_source(source: &str, range: Range<usize>) -> String {
    let line_start = source[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let column = source[line_start..range.start].chars().count();
    let mut lines = source[range].trim_end().lines();
    let mut out = lines.next().unwrap_or("").to_string();
    for line in lines {
        let mut rest = line;
        for _ in 0..column {
            match rest.strip_prefix([' ', '\t', '>']) {
                Some(r) => rest = r,
                None => break,
            }
        }
        out.push('\n');
        out.push_str(rest);
    }
    out
}

fn extend_span(span: &mut Option<Range<usize>>, range: Range<usize>) {
    *span = Some(match span.take() {
        Some(s) => s.start.min(range.start)..s.end.max(range.end),
        None => range,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> Inline {
        Inline::Text {
            text: t.to_string(),
            style: InlineStyle::default(),
        }
    }

    fn styled(t: &str, style: InlineStyle) -> Inline {
        Inline::Text {
            text: t.to_string(),
            style,
        }
    }

    fn kinds(md: &str) -> Vec<BlockKind> {
        parse(md).blocks.into_iter().map(|b| b.kind).collect()
    }

    #[test]
    fn headings_and_paragraphs() {
        let doc = parse("# Title\n\nSome text\nwrapped here.\n\n### Small\n");
        assert_eq!(
            doc.blocks
                .iter()
                .map(|b| b.kind.clone())
                .collect::<Vec<_>>(),
            vec![
                BlockKind::Heading {
                    level: 1,
                    content: vec![text("Title")]
                },
                BlockKind::Paragraph(vec![text("Some text wrapped here.")]),
                BlockKind::Heading {
                    level: 3,
                    content: vec![text("Small")]
                },
            ]
        );
        assert_eq!(&doc.source[doc.blocks[0].span.clone()], "# Title\n");
        assert_eq!(&doc.source[doc.blocks[2].span.clone()], "### Small\n");
    }

    #[test]
    fn inline_styles() {
        let blocks = kinds("a *em* **strong** ~~gone~~ `code` [link](https://x.org)  \nnext");
        let BlockKind::Paragraph(inlines) = &blocks[0] else {
            panic!("expected a paragraph, got {blocks:?}");
        };
        assert_eq!(
            inlines,
            &vec![
                text("a "),
                styled(
                    "em",
                    InlineStyle {
                        emphasis: true,
                        ..Default::default()
                    }
                ),
                text(" "),
                styled(
                    "strong",
                    InlineStyle {
                        strong: true,
                        ..Default::default()
                    }
                ),
                text(" "),
                styled(
                    "gone",
                    InlineStyle {
                        strikethrough: true,
                        ..Default::default()
                    }
                ),
                text(" "),
                styled(
                    "code",
                    InlineStyle {
                        code: true,
                        ..Default::default()
                    }
                ),
                text(" "),
                styled(
                    "link",
                    InlineStyle {
                        link: Some("https://x.org".into()),
                        ..Default::default()
                    }
                ),
                Inline::Break,
                text("next"),
            ]
        );
    }

    #[test]
    fn tight_list_with_tasks_and_nesting() {
        let blocks = kinds("- [x] done\n- [ ] todo\n  - inner\n");
        let BlockKind::List {
            start: None,
            tight: true,
            items,
        } = &blocks[0]
        else {
            panic!("expected a tight bullet list, got {blocks:?}");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].task, Some(true));
        assert_eq!(items[1].task, Some(false));
        assert_eq!(
            items[0].blocks[0].kind,
            BlockKind::Paragraph(vec![text("done")])
        );
        assert!(matches!(
            &items[1].blocks[1].kind,
            BlockKind::List { items, .. } if items.len() == 1
        ));
    }

    #[test]
    fn a_loose_list_keeps_its_task_markers() {
        let blocks = kinds("- loose\n\n- [x] done\n");
        let BlockKind::List {
            tight: false,
            items,
            ..
        } = &blocks[0]
        else {
            panic!("expected a loose list, got {blocks:?}");
        };
        assert_eq!(items[0].task, None);
        assert_eq!(items[1].task, Some(true));
        assert_eq!(
            items[1].blocks[0].kind,
            BlockKind::Paragraph(vec![text("done")])
        );
    }

    #[test]
    fn loose_task_markers_unchecked_first_and_nested() {
        let tasks = |items: &[ListItem]| items.iter().map(|i| i.task).collect::<Vec<_>>();
        let blocks = kinds("- [ ] a\n\n- b\n");
        let BlockKind::List {
            tight: false,
            items,
            ..
        } = &blocks[0]
        else {
            panic!("expected a loose list, got {blocks:?}");
        };
        assert_eq!(tasks(items), [Some(false), None]);
        let blocks = kinds("- x\n  - [ ] a\n\n  - [x] b\n");
        let BlockKind::List { items, .. } = &blocks[0] else {
            panic!("expected a list, got {blocks:?}");
        };
        assert_eq!(tasks(items), [None]);
        let BlockKind::List {
            tight: false,
            items,
            ..
        } = &items[0].blocks[1].kind
        else {
            panic!("expected a nested loose list, got {:?}", items[0].blocks);
        };
        assert_eq!(tasks(items), [Some(false), Some(true)]);
    }

    #[test]
    fn loose_ordered_list() {
        let blocks = kinds("3. one\n\n4. two\n");
        assert!(matches!(
            &blocks[0],
            BlockKind::List { start: Some(3), tight: false, items } if items.len() == 2
        ));
    }

    #[test]
    fn block_quote_holds_blocks() {
        let blocks = kinds("> quoted\n>\n> more\n");
        assert_eq!(
            blocks,
            vec![BlockKind::Quote(vec![
                Block {
                    kind: BlockKind::Paragraph(vec![text("quoted")]),
                    span: 2..9,
                },
                Block {
                    kind: BlockKind::Paragraph(vec![text("more")]),
                    span: 13..18,
                },
            ])]
        );
    }

    #[test]
    fn code_block_keeps_language_and_text() {
        assert_eq!(
            kinds("```rust\nfn main() {}\n```\n"),
            vec![BlockKind::Code {
                lang: Some("rust".into()),
                code: "fn main() {}\n".into()
            }]
        );
    }

    #[test]
    fn figure_fences_ignore_case_and_attributes() {
        for (md, kind) in [
            ("```Mermaid\ngraph TD\n```\n", FigureKind::Mermaid),
            ("```mermaid,foo\ngraph TD\n```\n", FigureKind::Mermaid),
            ("```MATH\ngraph TD\n```\n", FigureKind::Math),
        ] {
            assert_eq!(
                kinds(md),
                vec![BlockKind::Figure {
                    kind,
                    source: "graph TD\n".into()
                }],
                "{md:?}"
            );
        }
    }

    #[test]
    fn math_fence_with_attributes_is_a_figure_and_near_misses_stay_code() {
        assert_eq!(
            kinds("```math,display\nx^2\n```\n"),
            vec![BlockKind::Figure {
                kind: FigureKind::Math,
                source: "x^2\n".into()
            }]
        );
        for lang in ["mermaidx", "mermaid-js"] {
            let blocks = kinds(&format!("```{lang}\ngraph TD\n```\n"));
            assert!(matches!(&blocks[0], BlockKind::Code { .. }), "{blocks:?}");
        }
    }

    #[test]
    fn mermaid_block_is_a_figure_with_its_source() {
        assert_eq!(
            kinds("```mermaid\ngraph TD\n  A --> B\n```\n"),
            vec![BlockKind::Figure {
                kind: FigureKind::Mermaid,
                source: "graph TD\n  A --> B\n".into()
            }]
        );
    }

    #[test]
    fn display_maths_is_a_figure_and_inline_maths_stays_inline() {
        let blocks = kinds("$$\nE = mc^2\n$$\n\nWhere $c$ is light.\n");
        assert_eq!(
            blocks[0],
            BlockKind::Figure {
                kind: FigureKind::Math,
                source: "E = mc^2".into()
            }
        );
        let BlockKind::Paragraph(inlines) = &blocks[1] else {
            panic!("expected a paragraph, got {blocks:?}");
        };
        assert_eq!(
            inlines[1],
            styled(
                "$c$",
                InlineStyle {
                    math: true,
                    ..Default::default()
                }
            )
        );
    }

    #[test]
    fn display_maths_inside_prose_stays_inline() {
        let blocks = kinds("so $$x$$ holds\n");
        assert!(matches!(&blocks[0], BlockKind::Paragraph(_)));
    }

    #[test]
    fn table_is_a_figure_with_its_markdown() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n\nafter\n";
        let doc = parse(md);
        assert_eq!(
            doc.blocks[0].kind,
            BlockKind::Figure {
                kind: FigureKind::Table,
                source: "| a | b |\n|---|---|\n| 1 | 2 |".into()
            }
        );
        assert_eq!(
            doc.blocks[1].kind,
            BlockKind::Paragraph(vec![text("after")])
        );
    }

    #[test]
    fn table_in_a_quote_or_list_loses_its_markers() {
        let figure = |md: &str| {
            let doc = parse(md);
            let blocks = match &doc.blocks[0].kind {
                BlockKind::Quote(blocks) => blocks.clone(),
                BlockKind::List { items, .. } => items[0].blocks.clone(),
                other => panic!("unexpected {other:?}"),
            };
            blocks[0].kind.clone()
        };
        let table = BlockKind::Figure {
            kind: FigureKind::Table,
            source: "| a |\n|---|\n| 1 |".into(),
        };
        assert_eq!(figure("> | a |\n> |---|\n> | 1 |\n"), table);
        assert_eq!(figure("- | a |\n  |---|\n  | 1 |\n"), table);
    }

    #[test]
    fn table_source_drops_carriage_returns() {
        assert_eq!(
            kinds("| a |\r\n|---|\r\n| 1 |\r\n"),
            vec![BlockKind::Figure {
                kind: FigureKind::Table,
                source: "| a |\n|---|\n| 1 |".into()
            }]
        );
    }

    #[test]
    fn display_maths_with_inline_maths_stays_a_paragraph() {
        let blocks = kinds("$$y$$ $x$\n");
        assert!(matches!(&blocks[0], BlockKind::Paragraph(_)), "{blocks:?}");
    }

    #[test]
    fn a_paragraph_of_one_image_is_an_image_block() {
        let doc = parse("![a *big* cat](cat.png \"title\")\n\n- ![b](b.png)\n");
        assert_eq!(
            doc.blocks[0].kind,
            BlockKind::Image {
                src: "cat.png".into(),
                alt: "a big cat".into(),
                wiki: false,
            }
        );
        assert_eq!(
            &doc.source[doc.blocks[0].span.clone()],
            "![a *big* cat](cat.png \"title\")\n"
        );
        let BlockKind::List { items, .. } = &doc.blocks[1].kind else {
            panic!("expected a list, got {:?}", doc.blocks[1]);
        };
        assert_eq!(
            items[0].blocks[0].kind,
            BlockKind::Image {
                src: "b.png".into(),
                alt: "b".into(),
                wiki: false,
            }
        );
    }

    #[test]
    fn images_with_other_content_stay_inline() {
        for md in [
            "see ![a](a.png)\n",
            "![a](a.png) ![b](b.png)\n",
            "$$x$$ ![a](a.png)\n",
        ] {
            let blocks = kinds(md);
            assert!(
                matches!(&blocks[0], BlockKind::Paragraph(_)),
                "{md}: {blocks:?}"
            );
        }
        assert_eq!(
            kinds("see ![a](a.png)\n"),
            vec![BlockKind::Paragraph(vec![text("see [image: a]")])]
        );
    }

    #[test]
    fn a_linked_image_stays_a_link() {
        let blocks = kinds("[![CI](ci.png)](https://x.org/ci)\n");
        let BlockKind::Paragraph(inlines) = &blocks[0] else {
            panic!("expected a paragraph, got {blocks:?}");
        };
        assert_eq!(
            inlines,
            &vec![styled(
                "[image: CI]",
                InlineStyle {
                    link: Some("https://x.org/ci".into()),
                    ..Default::default()
                }
            )]
        );
    }

    #[test]
    fn rule_and_html() {
        assert_eq!(
            kinds("---\n\n<div>hi</div>\n"),
            vec![BlockKind::Rule, BlockKind::Html("<div>hi</div>\n".into())]
        );
    }

    /// Every field of the parsed blocks as indented lines, one block or
    /// inline per line, so a test can pin the whole of the builder's output.
    fn outline(md: &str) -> String {
        let mut out = String::new();
        outline_blocks(&parse(md).blocks, 0, &mut out);
        out
    }

    fn outline_blocks(blocks: &[Block], depth: usize, out: &mut String) {
        for Block { kind, span } in blocks {
            let pad = "  ".repeat(depth);
            let head = match kind {
                BlockKind::Heading { level, .. } => format!("heading {level}"),
                BlockKind::Paragraph(_) => "paragraph".to_string(),
                BlockKind::Code { lang, code } => format!("code {lang:?} {code:?}"),
                BlockKind::Quote(_) => "quote".to_string(),
                BlockKind::List { start, tight, .. } => format!("list {start:?} tight={tight}"),
                BlockKind::Rule => "rule".to_string(),
                BlockKind::Html(html) => format!("html {html:?}"),
                BlockKind::Image { src, alt, wiki } => {
                    format!("image {src:?} alt={alt:?} wiki={wiki}")
                }
                BlockKind::Figure { kind, source } => format!("figure {kind:?} {source:?}"),
            };
            out.push_str(&format!("{pad}{head} @{span:?}\n"));
            match kind {
                BlockKind::Heading { content, .. } | BlockKind::Paragraph(content) => {
                    outline_inlines(content, depth + 1, out);
                }
                BlockKind::Quote(blocks) => outline_blocks(blocks, depth + 1, out),
                BlockKind::List { items, .. } => {
                    for ListItem { task, blocks } in items {
                        out.push_str(&format!("{pad}  item task={task:?}\n"));
                        outline_blocks(blocks, depth + 2, out);
                    }
                }
                _ => {}
            }
        }
    }

    fn outline_inlines(inlines: &[Inline], depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        for inline in inlines {
            let line = match inline {
                Inline::Break => "break".to_string(),
                Inline::Text { text, style } => {
                    let InlineStyle {
                        emphasis,
                        strong,
                        strikethrough,
                        code,
                        math,
                        link,
                    } = style;
                    let mut line = format!("{text:?}");
                    for (on, name) in [
                        (emphasis, "em"),
                        (strong, "strong"),
                        (strikethrough, "strike"),
                        (code, "code"),
                        (math, "math"),
                    ] {
                        if *on {
                            line.push_str(&format!(" {name}"));
                        }
                    }
                    if let Some(Link { dest, wiki }) = link {
                        line.push_str(&format!(" link={dest:?} wiki={wiki}"));
                    }
                    line
                }
            };
            out.push_str(&format!("{pad}{line}\n"));
        }
    }

    #[track_caller]
    fn assert_outline(md: &str, expected: &str) {
        assert_eq!(outline(md), expected, "markdown: {md:?}");
    }

    fn themed(md: &str, theme: Theme) -> String {
        let mut out = String::new();
        outline_blocks(&parse_for(md, Some(theme)).blocks, 0, &mut out);
        out
    }

    #[test]
    fn an_inline_logo_for_the_other_background_is_left_out() {
        // Two logos on a line, each in a link: the one left out takes its
        // link with it.
        let md = "[![d](d.png#gh-dark-mode-only)](u) [![l](l.png#gh-light-mode-only)](v)\n";
        assert_eq!(
            themed(md, Theme::Dark),
            "paragraph @0..71\n  \"[image: d]\" link=\"u\" wiki=false\n  \" \"\n"
        );
        assert_eq!(
            themed(md, Theme::Light),
            "paragraph @0..71\n  \" \"\n  \"[image: l]\" link=\"v\" wiki=false\n"
        );
        // A wiki embed, whose fragment is not part of its name.
        let md = "![[logo.png#gh-dark-mode-only]]\n\nafter\n";
        assert_eq!(themed(md, Theme::Light), "paragraph @33..39\n  \"after\"\n");
        // Two logos on a line with no links.
        let md = "![d](d.png#gh-dark-mode-only) ![l](l.png#gh-light-mode-only)\n";
        assert_eq!(
            themed(md, Theme::Light),
            "image \"l.png#gh-light-mode-only\" alt=\"l\" wiki=false @0..61\n"
        );
    }

    #[test]
    fn a_logo_for_the_other_background_leaves_no_empty_block() {
        let md = "![logo](dark.png#gh-dark-mode-only)\n\n![logo](light.png#gh-light-mode-only)\n\n\
                  > ![logo](light.png#gh-light-mode-only)\n\n![shot](shot.png)\n";
        assert_eq!(
            themed(md, Theme::Dark),
            "image \"dark.png#gh-dark-mode-only\" alt=\"logo\" wiki=false @0..36\n\
             image \"shot.png\" alt=\"shot\" wiki=false @117..135\n"
        );
        // A quote, list item or list left with nothing goes too.
        let md = "- ![logo](light.png#gh-light-mode-only)\n- one\n\n\
                  > ![logo](light.png#gh-light-mode-only)\n\n\
                  - ![logo](light.png#gh-light-mode-only)\n";
        let blocks = parse_for(md, Some(Theme::Dark)).blocks;
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        let BlockKind::List { items, .. } = &blocks[0].kind else {
            panic!("{blocks:?}");
        };
        assert_eq!(items.len(), 1);
        // A heading or paragraph left with nothing to show goes too.
        let md = "# ![l](l.png#gh-light-mode-only)\n\n\
                  ![l](l.png#gh-light-mode-only)\\\n![l](l2.png#gh-light-mode-only)\n\ntext\n";
        assert_eq!(themed(md, Theme::Dark), "paragraph @99..104\n  \"text\"\n");
        // And on a light background, the other logo.
        let md = "> ![logo](dark.png#gh-dark-mode-only)\n\n![logo](light.png#gh-light-mode-only)\n";
        assert_eq!(
            themed(md, Theme::Light),
            "image \"light.png#gh-light-mode-only\" alt=\"logo\" wiki=false @39..77\n"
        );
        // With no theme known, both logos stay.
        assert_eq!(parse_for(md, None).blocks, parse(md).blocks);
    }

    #[test]
    fn pin_headings_atx_setext_and_inline_code() {
        assert_outline(
            "## Heading `code`\nSetext\n===\n",
            r#"heading 2 @0..18
  "Heading "
  "code" code
heading 1 @18..29
  "Setext"
"#,
        );
    }

    #[test]
    fn pin_nested_inline_styles() {
        assert_outline(
            "***strong em*** ~~*x*~~ [a **b** `c`](u)  \nend\n",
            r#"paragraph @0..47
  "strong em" em strong
  " "
  "x" em strike
  " "
  "a " link="u" wiki=false
  "b" strong link="u" wiki=false
  " " link="u" wiki=false
  "c" code link="u" wiki=false
  break
  "end"
"#,
        );
    }

    #[test]
    fn pin_wiki_links() {
        assert_outline(
            "# Wiki *a* [[Note]]\n\nsee [[Note#Part|the part]] and [[Other]]\n",
            r#"heading 1 @0..20
  "Wiki "
  "a" em
  " "
  "Note" link="Note" wiki=true
paragraph @21..62
  "see "
  "the part" link="Note#Part" wiki=true
  " and "
  "Other" link="Other" wiki=true
"#,
        );
    }

    #[test]
    fn pin_wiki_embeds_of_images_and_notes() {
        assert_outline(
            "![[pic.png]]\n",
            "image \"pic.png\" alt=\"pic.png\" wiki=true @0..13\n",
        );
        assert_outline(
            "![[Note]]\n",
            r#"paragraph @0..10
  "Note" link="Note" wiki=true
"#,
        );
        assert_outline(
            "text ![[Note|alias]] more\n",
            r#"paragraph @0..26
  "text "
  "alias" link="Note" wiki=true
  " more"
"#,
        );
    }

    #[test]
    fn an_image_embed_with_a_fragment_is_still_an_image() {
        assert_outline(
            "![[pic.png#frag]]\n",
            "image \"pic.png\" alt=\"pic.png#frag\" wiki=true @0..18\n",
        );
        assert_outline(
            "text ![[pic.png#x|200]] more\n",
            r#"paragraph @0..29
  "text [image: 200] more"
"#,
        );
    }

    #[test]
    fn a_wiki_name_drops_spaces_and_the_escape_before_its_alias() {
        assert_outline(
            "![[pic.png |300]]\n",
            "image \"pic.png\" alt=\"300\" wiki=true @0..18\n",
        );
        // Only a backslash right before the alias's pipe is an escape.
        assert_outline(
            "[[a\\ |t]]\n",
            "paragraph @0..10\n  \"t\" link=\"a\\\\\" wiki=true\n",
        );
        assert_outline(
            "![[pic.png #small]]\n",
            "image \"pic.png\" alt=\"pic.png #small\" wiki=true @0..20\n",
        );
        assert_outline(
            "a [[Note\\|x]] ![[Note#H\\|y]] [[ Note |z]]\n",
            r#"paragraph @0..42
  "a "
  "x" link="Note" wiki=true
  " "
  "y" link="Note#H" wiki=true
  " "
  "z" link="Note" wiki=true
"#,
        );
    }

    #[test]
    fn stray_html_and_footnote_references_are_text() {
        // The parser sends neither today: HTML comes inside an HTML block,
        // and footnotes are not turned on.
        use pulldown_cmark::CowStr;
        let mut builder = Builder::new("x<b>[^1]");
        builder.event(Event::Start(Tag::Paragraph), 0..8);
        builder.event(Event::Text(CowStr::from("x")), 0..1);
        builder.event(Event::Html(CowStr::from("<b>")), 1..4);
        builder.event(Event::FootnoteReference(CowStr::from("1")), 4..8);
        builder.event(Event::End(TagEnd::Paragraph), 0..8);
        let kinds: Vec<BlockKind> = builder
            .finish()
            .blocks
            .into_iter()
            .map(|b| b.kind)
            .collect();
        assert_eq!(kinds, vec![BlockKind::Paragraph(vec![text("x<b>[^1]")])]);
    }

    #[test]
    fn pin_images() {
        assert_outline(
            "![outer ![inner](i.png) alt](o.png)\n",
            "image \"o.png\" alt=\"outer inner alt\" wiki=false @0..36\n",
        );
        assert_outline(
            "- item\n\n  ![x](x.png)\n",
            r#"list None tight=false @0..22
  item task=None
    paragraph @2..7
      "item"
    image "x.png" alt="x" wiki=false @10..22
"#,
        );
        assert_outline(
            "![a](a.png) ![b](b.png)\n",
            r#"paragraph @0..24
  "[image: a] [image: b]"
"#,
        );
    }

    #[test]
    fn pin_nested_quotes_and_lists() {
        assert_outline(
            "> outer\n> > inner\n> > - item\n",
            r#"quote @0..29
  paragraph @2..8
    "outer"
  quote @10..29
    paragraph @12..18
      "inner"
    list None tight=true @22..29
      item task=None
        paragraph @24..28
          "item"
"#,
        );
        assert_outline(
            "1. a\n   - [ ] b\n     > c\n2. d\n",
            r#"list Some(1) tight=true @0..30
  item task=None
    paragraph @3..4
      "a"
    list None tight=true @8..25
      item task=Some(false)
        paragraph @14..15
          "b"
        quote @21..25
          paragraph @23..25
            "c"
  item task=None
    paragraph @28..29
      "d"
"#,
        );
        assert_outline(
            "- one\n\n  two\n- three\n",
            r#"list None tight=false @0..21
  item task=None
    paragraph @2..6
      "one"
    paragraph @9..13
      "two"
  item task=None
    paragraph @15..21
      "three"
"#,
        );
    }

    #[test]
    fn pin_code_fences() {
        assert_outline(
            "```\nplain\n```\n\n    indented\n\n``` rust  extra\nlet x;\n```\n",
            r#"code None "plain\n" @0..13
code None "indented\n" @19..28
code Some("rust") "let x;\n" @29..55
"#,
        );
    }

    #[test]
    fn pin_maths_and_mermaid_figures() {
        assert_outline(
            "```math\nx^2\n```\n\n```mermaid\ngraph TD\n```\n\n$$a$$\n$$b$$\n",
            r#"figure Math "x^2\n" @0..15
figure Mermaid "graph TD\n" @17..40
figure Math "a" @42..54
figure Math "b" @42..54
"#,
        );
    }

    #[test]
    fn pin_tables() {
        assert_outline(
            "> | a |\n> |---|\n\nafter *x*\n",
            r#"quote @0..16
  figure Table "| a |\n|---|" @2..16
paragraph @17..27
  "after "
  "x" em
"#,
        );
    }

    #[test]
    fn pin_footnotes_html_and_rules() {
        assert_outline(
            "Text[^1].\n\n[^1]: The note.\n\n<!-- c -->\n\na <b>bold</b> c\n\n***\n___\n",
            r#"paragraph @0..10
  "Text[^1]."
paragraph @11..27
  "[^1]: The note."
html "<!-- c -->\n" @28..39
paragraph @40..56
  "a <b>bold</b> c"
rule @57..61
rule @61..65
"#,
        );
    }
}
