//! The document model: a markdown file parsed into a list of blocks.
//!
//! Every block keeps the byte range it came from, so a later layer can swap a
//! block (a figure, or a heading) for an image and still reach its source.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

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
    pub link: Option<String>,
}

pub fn parse(source: &str) -> Document {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    let mut builder = Builder::new(source);
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
    links: Vec<String>,
    in_table: bool,
}

struct Frame {
    kind: FrameKind,
    span: Range<usize>,
    blocks: Vec<Block>,
    /// Inline content of a paragraph or heading, or the text of a tight list
    /// item, which pulldown-cmark does not wrap in a paragraph.
    inlines: Vec<Inline>,
    inline_span: Option<Range<usize>>,
    /// Display maths seen in `inlines`, and whether anything else was.
    display_math: Vec<String>,
    other_inline: bool,
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
            other_inline: false,
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

/// A paragraph that holds nothing but display maths becomes maths figures.
fn paragraph_blocks(frame: &mut Frame, span: Range<usize>) -> Vec<Block> {
    let inlines = std::mem::take(&mut frame.inlines);
    let maths = std::mem::take(&mut frame.display_math);
    let other = std::mem::replace(&mut frame.other_inline, false);
    if !maths.is_empty() && !other {
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

    fn event(&mut self, event: Event, range: Range<usize>) {
        // A table is kept whole as source text, so skip its contents.
        if self.in_table {
            if matches!(event, Event::End(TagEnd::Table)) {
                self.in_table = false;
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
            Event::Code(code) => {
                let style = InlineStyle {
                    code: true,
                    ..self.style()
                };
                self.text(&code, style, range);
            }
            Event::InlineMath(math) => {
                let style = InlineStyle {
                    math: true,
                    ..self.style()
                };
                self.text(&format!("${math}$"), style, range);
            }
            Event::DisplayMath(math) => {
                let style = InlineStyle {
                    math: true,
                    ..self.style()
                };
                self.text(&format!("$${math}$$"), style, range);
                self.top().display_math.push(math.trim().to_string());
            }
            Event::FootnoteReference(label) => {
                self.text(&format!("[^{label}]"), self.style(), range);
            }
            Event::SoftBreak => self.text(" ", self.style(), range),
            Event::HardBreak => self.inline(Inline::Break, range),
            Event::Rule => self.push_block(BlockKind::Rule, range),
            Event::TaskListMarker(done) => {
                if let FrameKind::Item { task } = &mut self.top().kind {
                    *task = Some(done);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag, range: Range<usize>) {
        let kind = match tag {
            Tag::Paragraph => {
                if let FrameKind::Item { .. } = self.top().kind {
                    self.mark_list_loose();
                }
                FrameKind::Paragraph
            }
            Tag::Heading { level, .. } => FrameKind::Heading(level as u8),
            Tag::BlockQuote(_) => FrameKind::Quote,
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().map(str::to_string)
                    }
                    CodeBlockKind::Indented => None,
                };
                FrameKind::Code {
                    lang,
                    code: String::new(),
                }
            }
            Tag::HtmlBlock => FrameKind::Html(String::new()),
            Tag::List(start) => FrameKind::List {
                start,
                tight: true,
                items: Vec::new(),
            },
            Tag::Item => FrameKind::Item { task: None },
            Tag::Table(_) => {
                let source = self.source[range.clone()].trim_end().to_string();
                self.push_block(
                    BlockKind::Figure {
                        kind: FigureKind::Table,
                        source,
                    },
                    range,
                );
                self.in_table = true;
                return;
            }
            Tag::Emphasis => {
                self.emphasis += 1;
                return;
            }
            Tag::Strong => {
                self.strong += 1;
                return;
            }
            Tag::Strikethrough => {
                self.strikethrough += 1;
                return;
            }
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                return;
            }
            Tag::Image { .. } => {
                self.text("[image: ", self.style(), range);
                return;
            }
            // Not enabled, or kept as plain inline text.
            _ => return,
        };
        self.top().flush_inlines();
        self.stack.push(Frame::new(kind, range));
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
            TagEnd::Image => {
                let end = self.top().inline_span.as_ref().map_or(0, |s| s.end);
                self.text("]", self.style(), end..end);
            }
            _ => {}
        }
    }

    /// Pop the top frame and add what it built to its parent.
    fn close(&mut self) {
        let mut frame = self.stack.pop().expect("frame to close");
        let span = frame.span.clone();
        let kind = std::mem::replace(&mut frame.kind, FrameKind::Root);
        match kind {
            FrameKind::Root => unreachable!("root frame is never closed"),
            FrameKind::Paragraph => {
                for block in paragraph_blocks(&mut frame, span) {
                    self.top().blocks.push(block);
                }
            }
            FrameKind::Heading(level) => {
                let content = std::mem::take(&mut frame.inlines);
                self.push_block(BlockKind::Heading { level, content }, span);
            }
            FrameKind::Quote => {
                frame.flush_inlines();
                self.push_block(BlockKind::Quote(frame.blocks), span);
            }
            FrameKind::Code { lang, code } => {
                let kind = match lang.as_deref() {
                    Some("mermaid") => BlockKind::Figure {
                        kind: FigureKind::Mermaid,
                        source: code,
                    },
                    Some("math") => BlockKind::Figure {
                        kind: FigureKind::Math,
                        source: code,
                    },
                    _ => BlockKind::Code { lang, code },
                };
                self.push_block(kind, span);
            }
            FrameKind::Html(html) => self.push_block(BlockKind::Html(html), span),
            FrameKind::List {
                start,
                tight,
                items,
            } => self.push_block(
                BlockKind::List {
                    start,
                    tight,
                    items,
                },
                span,
            ),
            FrameKind::Item { task } => {
                frame.flush_inlines();
                if let FrameKind::List { items, .. } = &mut self.top().kind {
                    items.push(ListItem {
                        task,
                        blocks: frame.blocks,
                    });
                }
            }
        }
    }

    fn push_block(&mut self, kind: BlockKind, span: Range<usize>) {
        let top = self.top();
        top.flush_inlines();
        top.blocks.push(Block { kind, span });
    }

    fn mark_list_loose(&mut self) {
        let len = self.stack.len();
        if len >= 2
            && let FrameKind::List { tight, .. } = &mut self.stack[len - 2].kind
        {
            *tight = false;
        }
    }

    fn text(&mut self, text: &str, style: InlineStyle, range: Range<usize>) {
        if text.is_empty() {
            return;
        }
        let top = self.top();
        if !text.trim().is_empty() && !style.math {
            top.other_inline = true;
        }
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
    fn rule_and_html() {
        assert_eq!(
            kinds("---\n\n<div>hi</div>\n"),
            vec![BlockKind::Rule, BlockKind::Html("<div>hi</div>\n".into())]
        );
    }
}
