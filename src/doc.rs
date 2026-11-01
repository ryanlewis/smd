//! The document model: a markdown file parsed into a list of blocks.
//!
//! Every block keeps the byte range it came from, so a later layer can swap a
//! block (a figure, or a heading) for an image and still reach its source.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{
    CodeBlockKind, Event, LinkType, MetadataBlockKind, Options, Parser, Tag, TagEnd,
};

use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub source: String,
    pub blocks: Vec<Block>,
    /// Places a link can jump to that are not headings, with the byte in
    /// the source each is at: footnote `n`, `fn:n`, and its first
    /// reference, `fnref:n`.
    pub anchors: Vec<(String, usize)>,
}

impl Document {
    /// Where the anchor `name` is in the source, if there is one.
    pub fn anchor(&self, name: &str) -> Option<usize> {
        self.anchors
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, at)| at)
    }
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
    /// An Obsidian callout or GitHub alert: a quote starting `[!type]`.
    Callout {
        callout: Callout,
        blocks: Vec<Block>,
    },
    List {
        /// The number of the first item for an ordered list.
        start: Option<u64>,
        /// Tight lists have no blank lines between or inside their items.
        tight: bool,
        items: Vec<ListItem>,
    },
    Rule,
    /// The YAML block at the very start of a note, between `---` lines, or
    /// a TOML one between `+++` lines.
    Frontmatter(Frontmatter),
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
    /// Another note shown in place of an `![[note]]` alone in a paragraph:
    /// its name, a link to it, and the part of it embedded, whose spans
    /// point into its own source.
    Embed {
        title: String,
        link: Link,
        doc: Document,
    },
}

/// A frontmatter block's text between its delimiters, and its top-level
/// properties when they could be read.
#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    pub raw: String,
    /// Each top-level key and its value, or `None` when the block is not the
    /// simple YAML read here, and is shown as its raw text instead.
    pub properties: Option<Vec<(String, String)>>,
}

/// The head of a callout: its type, title and whether it folds.
#[derive(Debug, Clone, PartialEq)]
pub struct Callout {
    pub kind: CalloutKind,
    /// The text after the marker, or the type's name when there is none.
    pub title: Vec<Inline>,
    /// `Some(open)` when the callout can fold: `+` starts it open, `-` shut.
    pub fold: Option<bool>,
}

/// GitHub's five alert types and Obsidian's callout types. Each alias is
/// read as the type it stands for, and an unknown type as a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutKind {
    Note,
    Abstract,
    Info,
    Todo,
    Tip,
    Important,
    Success,
    Question,
    Warning,
    Caution,
    Failure,
    Danger,
    Bug,
    Example,
    Quote,
}

impl CalloutKind {
    fn from_name(name: &str) -> CalloutKind {
        use CalloutKind::*;
        match name.to_ascii_lowercase().as_str() {
            "abstract" | "summary" | "tldr" => Abstract,
            "info" => Info,
            "todo" => Todo,
            "tip" | "hint" => Tip,
            "important" => Important,
            "success" | "check" | "done" => Success,
            "question" | "help" | "faq" => Question,
            "warning" | "attention" => Warning,
            "caution" => Caution,
            "failure" | "fail" | "missing" => Failure,
            "danger" | "error" => Danger,
            "bug" => Bug,
            "example" => Example,
            "quote" | "cite" => Quote,
            _ => Note,
        }
    }
}

/// The `[!type]` marker at the start of `line`, with its `+` or `-`: the
/// type's name, the fold, and the marker's length in bytes.
fn callout_marker(line: &str) -> Option<(&str, Option<bool>, usize)> {
    let rest = line.strip_prefix("[!")?;
    let end = rest.find(']')?;
    let name = &rest[..end];
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let len = 2 + end + 1;
    let fold = match line[len..].chars().next() {
        Some('+') => Some(true),
        Some('-') => Some(false),
        _ => None,
    };
    Some((name, fold, len + usize::from(fold.is_some())))
}

/// A callout's type name with its first letter in capitals and the rest
/// not, for a callout with no title.
fn default_title(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut chars = lower.chars();
    chars
        .next()
        .map_or(String::new(), |c| c.to_uppercase().chain(chars).collect())
}

/// `inlines` with the `skip` bytes of the marker taken off the front, when
/// it was read as text, and the spaces at either end trimmed.
fn trimmed_title(mut inlines: Vec<Inline>, skip: usize) -> Vec<Inline> {
    if let Some(Inline::Text { text, .. }) = inlines.first_mut()
        && text.starts_with("[!")
    {
        *text = text.get(skip..).unwrap_or("").trim_start().to_string();
    }
    if let Some(Inline::Text { text, .. }) = inlines.last_mut() {
        *text = text.trim_end().to_string();
    }
    inlines.retain(|i| !matches!(i, Inline::Text { text, .. } if text.is_empty()));
    while matches!(inlines.last(), Some(Inline::Break)) {
        inlines.pop();
    }
    inlines
}

/// A quote that starts with a callout marker, while it is read.
struct CalloutHead {
    name: String,
    fold: Option<bool>,
    /// The title, once its line has been read.
    title: Option<Vec<Inline>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    /// `Some(done)` for a task list item.
    pub task: Option<bool>,
    /// Where a task's mark is in the source: the byte between its brackets,
    /// a space or an `x`.
    pub mark: Option<usize>,
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
    /// Obsidian's `==highlight==`.
    pub highlight: bool,
    /// An Obsidian tag, `#tag` or `#nested/tag`.
    pub tag: bool,
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
        | Options::ENABLE_WIKILINKS
        | Options::ENABLE_FOOTNOTES;
    let mut builder = Builder::new(source);
    builder.hide = theme.map(other_theme);
    // The body is parsed without metadata blocks, which pulldown-cmark would
    // find anywhere, not only at the start.
    let start = match frontmatter(source) {
        Some((block, span)) => {
            builder.push_block(BlockKind::Frontmatter(block), span.clone());
            span.end
        }
        None => 0,
    };
    let body = &source[start..];
    builder.comments = comments(body, options)
        .into_iter()
        .map(|c| c.start + start..c.end + start)
        .collect();
    for (event, range) in Parser::new_ext(body, options).into_offset_iter() {
        builder.event(event, range.start + start..range.end + start);
    }
    builder.finish()
}

/// The frontmatter block at the very start of `source`, after any byte
/// order mark, and its span.
fn frontmatter(source: &str) -> Option<(Frontmatter, Range<usize>)> {
    let bom = if source.starts_with('\u{feff}') { 3 } else { 0 };
    let text = &source[bom..];
    if !text.starts_with("---") && !text.starts_with("+++") {
        return None;
    }
    let options = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS;
    let mut events = Parser::new_ext(text, options).into_offset_iter();
    let (Event::Start(Tag::MetadataBlock(kind)), span) = events.next()? else {
        return None;
    };
    if span.start != 0 {
        return None;
    }
    let span = 0..span.end + bom;
    let mut raw = String::new();
    for (event, _) in events {
        match event {
            Event::Text(text) => raw.push_str(&text),
            _ => break,
        }
    }
    let properties = match kind {
        MetadataBlockKind::YamlStyle => properties(&raw),
        MetadataBlockKind::PlusesStyle => None,
    };
    Some((Frontmatter { raw, properties }, span))
}

/// The top-level `key: value` lines of simple YAML. A list under a key,
/// as `- item` lines or `[a, b]`, is joined with commas, and any other
/// value under a key is kept as its text. `None` for anything else.
fn properties(yaml: &str) -> Option<Vec<(String, String)>> {
    // Each key, the text after it, and the lines under it.
    let mut props: Vec<(String, String, Vec<&str>)> = Vec::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        let indented = line.starts_with([' ', '\t']);
        // An indented `#` line may be the text of a block scalar.
        if trimmed.is_empty() || (!indented && trimmed.starts_with('#')) {
            continue;
        }
        if indented || is_item(trimmed) {
            let (_, after, under) = props.last_mut()?;
            // A list may start at the key's column, but not after a value.
            if !indented && !after.is_empty() {
                return None;
            }
            under.push(line);
            continue;
        }
        let (key, after) = split_key(trimmed)?;
        if !after.is_empty() && !after.starts_with(' ') {
            return None;
        }
        props.push((key.to_string(), after.trim().to_string(), Vec::new()));
    }
    let props = props
        .into_iter()
        .map(|(k, after, under)| (k, value(&after, &under)));
    Some(props.collect())
}

fn is_item(line: &str) -> bool {
    line == "-" || line.starts_with("- ")
}

/// A line's key, without quotes, and the text after its colon.
fn split_key(line: &str) -> Option<(&str, &str)> {
    for q in ['"', '\''] {
        if let Some(rest) = line.strip_prefix(q) {
            let (key, after) = rest.split_once(q)?;
            return Some((key, after.strip_prefix(':')?));
        }
    }
    let (key, after) = line.split_once(':')?;
    Some((key.trim(), after))
}

/// A property's value from the text after its key and the lines under it.
/// A block scalar, `|` or `>`, keeps or folds its lines, and a plain value
/// going on under its key is joined with spaces.
fn value(after: &str, under: &[&str]) -> String {
    let after = strip_comment(after);
    if under.is_empty() {
        let flow = after.strip_prefix('[').and_then(|v| v.strip_suffix(']'));
        return match flow.and_then(flow_items) {
            Some(items) => list(items.into_iter()),
            None => unquote(after).to_string(),
        };
    }
    if after.is_empty() && under.iter().all(|l| is_item(l.trim())) {
        return list(under.iter().map(|l| &l.trim()[1..]));
    }
    let lines = dedent(under);
    match after {
        "" | "|" | "|-" | "|+" => lines.join("\n"),
        ">" | ">-" | ">+" => lines.join(" "),
        _ => {
            let words = std::iter::once(after).chain(lines.iter().map(|l| l.trim()));
            unquote(&words.collect::<Vec<_>>().join(" ")).to_string()
        }
    }
}

/// A value without a `# comment` after it, outside quotes.
fn strip_comment(value: &str) -> &str {
    let mut quote = None;
    let mut prev = ' ';
    for (i, c) in value.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') if matches!(prev, ' ' | '[' | ',') => quote = Some(c),
            (None, '#') if prev.is_whitespace() => return value[..i].trim_end(),
            _ => {}
        }
        prev = c;
    }
    value
}

/// List items joined with commas, without their quotes or comments.
fn list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items
        .map(|i| unquote(strip_comment(i.trim())))
        .filter(|i| !i.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The items of a flow list's inside, split at commas outside quotes, or
/// `None` when it nests a list or map.
fn flow_items(flow: &str) -> Option<Vec<&str>> {
    let mut items = Vec::new();
    let mut quote = None;
    let mut start = 0;
    for (i, c) in flow.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[' | '{') => return None,
            (None, ',') => {
                items.push(&flow[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&flow[start..]);
    Some(items)
}

/// Lines with the indent of spaces and tabs they share taken off.
fn dedent<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let indent = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
    let common = lines.iter().map(|l| indent(l)).min().unwrap_or(0);
    lines.iter().map(|l| l[common..].trim_end()).collect()
}

/// A scalar without the quotes around it.
fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// A property's value as inline text, with its wiki links, `[[name]]` or
/// `[[name|text]]`, and web addresses as links.
pub fn value_inlines(value: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut rest = value;
    let push = |out: &mut Vec<Inline>, text: &str, link: Option<Link>| {
        if !text.is_empty() {
            out.push(Inline::Text {
                text: text.to_string(),
                style: InlineStyle {
                    link,
                    ..InlineStyle::default()
                },
            });
        }
    };
    loop {
        let at = ["[[", "https://", "http://"]
            .iter()
            .filter_map(|m| rest.find(m))
            .min();
        let Some(at) = at else { break };
        push(&mut out, &rest[..at], None);
        rest = &rest[at..];
        if let Some(inner) = rest.strip_prefix("[[")
            && let Some(end) = inner.find("]]")
        {
            let (name, text) = inner[..end]
                .split_once('|')
                .unwrap_or((&inner[..end], &inner[..end]));
            let link = Link {
                dest: link_dest(name, true).into_owned(),
                wiki: true,
            };
            push(&mut out, text, Some(link));
            rest = &inner[end + 2..];
        } else if rest.starts_with("[[") {
            push(&mut out, "[[", None);
            rest = &rest[2..];
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            // The comma between list items is not part of the address.
            let url = rest[..end].trim_end_matches(',');
            push(&mut out, url, Some(url.into()));
            rest = &rest[url.len()..];
        }
    }
    push(&mut out, rest, None);
    out
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
    /// Obsidian comments, which are hidden.
    comments: Vec<Range<usize>>,
    /// Each footnote referenced, by its label in lower case, in the order
    /// of first reference, which numbers it from 1, and where that
    /// reference is.
    referenced: Vec<(String, usize)>,
    /// Each footnote's definition: its label in lower case, its blocks and
    /// where it is. A label defined twice keeps the first.
    definitions: Vec<(String, Vec<Block>, Range<usize>)>,
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
    /// Whether an image or block in it was left out, for the theme or as a
    /// comment.
    left_out: bool,
    /// A `==` highlight opened and not yet closed: where it starts in
    /// `inlines`, and the style the `==` had.
    highlight: Option<(usize, InlineStyle)>,
}

enum FrameKind {
    Root,
    Quote(Option<CalloutHead>),
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<ListItem>,
    },
    Item {
        task: Option<bool>,
        mark: Option<usize>,
    },
    /// A paragraph; for the first one in a callout, the length of the
    /// marker in front of the title, until the title's line has been read.
    Paragraph(Option<usize>),
    Heading(u8),
    Code {
        lang: Option<String>,
        code: String,
    },
    Html(String),
    /// A footnote's definition, by its label in lower case.
    Footnote(String),
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
            highlight: None,
        }
    }

    /// A `==` with no partner after it is shown as text after all.
    fn end_highlight(&mut self) {
        if let Some((at, style)) = self.highlight.take() {
            for inline in &mut self.inlines[at..] {
                if let Inline::Text { style, .. } = inline {
                    style.highlight = false;
                }
            }
            let text = "==".to_string();
            self.inlines.insert(at, Inline::Text { text, style });
        }
    }

    /// Turn loose inline content (a tight list item's text) into a paragraph.
    fn flush_inlines(&mut self) {
        self.end_highlight();
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
    frame.end_highlight();
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
            comments: Vec::new(),
            referenced: Vec::new(),
            definitions: Vec::new(),
        }
    }

    fn finish(mut self) -> Document {
        while self.stack.len() > 1 {
            self.close();
        }
        let mut root = self.stack.pop().expect("root frame");
        root.flush_inlines();
        let mut blocks = root.blocks;
        let anchors = self.footnotes(&mut blocks);
        Document {
            source: self.source.to_string(),
            blocks,
            anchors,
        }
    }

    /// Add the footnotes referenced to the end of `blocks`, after a rule, as
    /// a list numbered as their references are. Each ends with a link back
    /// to its first reference. Returns the anchors for both.
    fn footnotes(&mut self, blocks: &mut Vec<Block>) -> Vec<(String, usize)> {
        if self.referenced.is_empty() {
            return Vec::new();
        }
        let end = self.source.len();
        let mut anchors = Vec::new();
        let mut items = Vec::new();
        for (i, (label, at)) in self.referenced.iter().enumerate() {
            let n = i + 1;
            let Some(i) = self.definitions.iter().position(|d| d.0 == *label) else {
                continue;
            };
            let (_, mut def, range) = self.definitions.swap_remove(i);
            anchors.push((format!("fnref:{n}"), *at));
            anchors.push((
                format!("fn:{n}"),
                def.first().map_or(range.start, |b| b.span.start),
            ));
            let back = Inline::Text {
                text: "↩".to_string(),
                style: InlineStyle {
                    link: Some(Link::from(format!("#fnref:{n}").as_str())),
                    ..InlineStyle::default()
                },
            };
            match def.last_mut() {
                Some(Block {
                    kind: BlockKind::Paragraph(inlines),
                    ..
                }) => {
                    inlines.push(Inline::Text {
                        text: " ".to_string(),
                        style: InlineStyle::default(),
                    });
                    inlines.push(back);
                }
                last => {
                    let span = last.map_or(range.clone(), |b| b.span.end..b.span.end);
                    def.push(Block {
                        kind: BlockKind::Paragraph(vec![back]),
                        span,
                    });
                }
            }
            items.push(ListItem {
                task: None,
                mark: None,
                blocks: def,
            });
        }
        if items.is_empty() {
            return anchors;
        }
        let tight = items.iter().all(|item| item.blocks.len() == 1);
        blocks.push(Block {
            kind: BlockKind::Rule,
            span: end..end,
        });
        blocks.push(Block {
            kind: BlockKind::List {
                start: Some(1),
                tight,
                items,
            },
            span: end..end,
        });
        anchors
    }

    /// A reference to a footnote, shown as its number, `[1]`, linked to the
    /// footnote.
    fn footnote_reference(&mut self, label: &str, range: Range<usize>) {
        let label = label.to_lowercase();
        let n = match self.referenced.iter().position(|(l, _)| *l == label) {
            Some(i) => i + 1,
            None => {
                self.referenced.push((label, range.start));
                self.referenced.len()
            }
        };
        let style = InlineStyle {
            link: Some(Link::from(format!("#fn:{n}").as_str())),
            ..self.style()
        };
        self.text(&format!("[{n}]"), style, range);
    }

    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("root frame")
    }

    fn style(&self) -> InlineStyle {
        InlineStyle {
            emphasis: self.emphasis > 0,
            strong: self.strong > 0,
            strikethrough: self.strikethrough > 0,
            highlight: self.stack.last().is_some_and(|f| f.highlight.is_some()),
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
        // Whatever is all inside a comment is hidden.
        if self
            .comments
            .iter()
            .any(|c| c.start <= range.start && range.end <= c.end)
        {
            self.top().left_out = true;
            // A callout's title still ends at its line.
            if matches!(event, Event::SoftBreak | Event::HardBreak) {
                self.ends_title(range.end);
            }
            return;
        }
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
                _ => self.prose(&text, range),
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
            Event::FootnoteReference(label) => self.footnote_reference(&label, range),
            Event::SoftBreak | Event::HardBreak if self.ends_title(range.end) => {}
            Event::SoftBreak => self.text(" ", self.style(), range),
            Event::HardBreak => self.inline(Inline::Break, range),
            Event::Rule => self.push_block(BlockKind::Rule, range),
            Event::TaskListMarker(done) => self.mark_task(done, range),
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
                if let Some(kind) = self.block_frame(tag, range.clone()) {
                    self.top().flush_inlines();
                    self.stack.push(Frame::new(kind, range));
                }
            }
        }
    }

    /// The frame a block-level tag opens, or `None` for a tag that is not
    /// enabled or is kept as plain inline text.
    fn block_frame(&mut self, tag: Tag, range: Range<usize>) -> Option<FrameKind> {
        Some(match tag {
            Tag::Paragraph => {
                if let FrameKind::Item { .. } = self.top().kind {
                    self.mark_list_loose();
                }
                FrameKind::Paragraph(self.callout_title(range))
            }
            Tag::Heading { level, .. } => FrameKind::Heading(level as u8),
            Tag::BlockQuote(_) => FrameKind::Quote(None),
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
            Tag::Item => FrameKind::Item {
                task: None,
                mark: None,
            },
            Tag::FootnoteDefinition(label) => FrameKind::Footnote(label.to_lowercase()),
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
            // The span takes in the whole `![[note]]`, not only its text.
            extend_span(&mut self.top().inline_span, range);
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
            | TagEnd::Item
            | TagEnd::FootnoteDefinition => self.close(),
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
        frame.end_highlight();
        let span = frame.span.clone();
        // A block left empty by an image left out for the theme goes too.
        let emptied = |frame: &Frame| frame.left_out && frame.blocks.is_empty();
        if frame.left_out {
            self.top().left_out = true;
        }
        let kind = match std::mem::replace(&mut frame.kind, FrameKind::Root) {
            FrameKind::Root => unreachable!("root frame is never closed"),
            FrameKind::Paragraph(Some(marker)) => {
                // The title was the paragraph's only line.
                self.take_title(std::mem::take(&mut frame.inlines), marker);
                return;
            }
            FrameKind::Paragraph(None) => {
                let blocks = paragraph_blocks(&mut frame, span);
                self.top().blocks.extend(blocks);
                return;
            }
            FrameKind::Item { task, mark } => {
                frame.flush_inlines();
                // A task keeps its box.
                if task.is_none() && emptied(&frame) {
                    return;
                }
                if let FrameKind::List { items, .. } = &mut self.top().kind {
                    items.push(ListItem {
                        task,
                        mark,
                        blocks: frame.blocks,
                    });
                }
                return;
            }
            FrameKind::Footnote(label) => {
                // A quote or list item that held only this goes too.
                self.top().left_out = true;
                frame.flush_inlines();
                if !self.definitions.iter().any(|d| d.0 == label) {
                    self.definitions.push((label, frame.blocks, span));
                }
                return;
            }
            FrameKind::Heading(_) if frame.left_out && !frame.other_inline => return,
            FrameKind::Heading(level) => BlockKind::Heading {
                level,
                content: frame.inlines,
            },
            FrameKind::Quote(Some(head)) => {
                frame.flush_inlines();
                let title = head.title.unwrap_or_default();
                let title = if title.is_empty() {
                    vec![Inline::Text {
                        text: default_title(&head.name),
                        style: InlineStyle::default(),
                    }]
                } else {
                    title
                };
                BlockKind::Callout {
                    callout: Callout {
                        kind: CalloutKind::from_name(&head.name),
                        title,
                        fold: head.fold,
                    },
                    blocks: frame.blocks,
                }
            }
            FrameKind::Quote(None) => {
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

    /// For a paragraph opening at `range` as the first thing in a quote,
    /// the length of the callout marker it starts with, if it does. The
    /// quote becomes a callout.
    fn callout_title(&mut self, range: Range<usize>) -> Option<usize> {
        let line = self.source[range.start..].lines().next().unwrap_or("");
        let (name, fold, len) = callout_marker(line)?;
        let top = self.stack.last_mut()?;
        if !(matches!(top.kind, FrameKind::Quote(None))
            && top.blocks.is_empty()
            && top.inlines.is_empty())
        {
            return None;
        }
        top.kind = FrameKind::Quote(Some(CalloutHead {
            name: name.to_string(),
            fold,
            title: None,
        }));
        Some(len)
    }

    /// At a line break: when it ends a callout's title line, take the line
    /// so far as the title and return true. The paragraph goes on as the
    /// body from `end`.
    fn ends_title(&mut self, end: usize) -> bool {
        if self.image.is_some() {
            return false;
        }
        let top = self.top();
        let FrameKind::Paragraph(Some(marker)) = top.kind else {
            return false;
        };
        top.kind = FrameKind::Paragraph(None);
        top.inline_span = None;
        top.span.start = end;
        // What the title line held says nothing about the body.
        top.other_inline = false;
        top.images.clear();
        top.display_math.clear();
        // A highlight does not go on past the title.
        top.end_highlight();
        let inlines = std::mem::take(&mut top.inlines);
        self.take_title(inlines, marker);
        true
    }

    /// Give the callout holding the paragraph on top of the stack its title.
    fn take_title(&mut self, inlines: Vec<Inline>, marker: usize) {
        // The paragraph is on top while it is open, and gone once closed.
        let quote = match self.top().kind {
            FrameKind::Paragraph(_) => self.under_top(),
            _ => self.stack.last_mut(),
        };
        if let Some(Frame {
            kind: FrameKind::Quote(Some(head)),
            ..
        }) = quote
        {
            head.title = Some(trimmed_title(inlines, marker));
        }
    }

    /// Mark the open list item as a task. In a loose list the marker comes
    /// inside the item's paragraph, so the item is the frame below it.
    fn mark_task(&mut self, done: bool, range: Range<usize>) {
        // The marker's range can take in the spaces around `[ ]`.
        let at = self.source[range.clone()]
            .find('[')
            .map(|i| range.start + i + 1);
        let item = match self.top().kind {
            FrameKind::Item { .. } => self.stack.last_mut(),
            _ => self.under_top(),
        };
        if let Some(Frame {
            kind: FrameKind::Item { task, mark },
            ..
        }) = item
        {
            *task = Some(done);
            *mark = at;
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

    /// A text event's text less what is in a comment, with its highlights
    /// and tags. Text that is not the same as its source, such as an entity,
    /// has no comment taken out.
    fn prose(&mut self, text: &str, range: Range<usize>) {
        let source = self.source;
        if source.get(range.clone()) != Some(text) {
            return self.marks(text, range, false);
        }
        let mut shown = Vec::new();
        let mut at = range.start;
        for c in &self.comments {
            if c.start < range.end && range.start < c.end {
                if at < c.start {
                    shown.push(at..c.start);
                }
                at = at.max(c.end);
            }
        }
        if at < range.end {
            shown.push(at..range.end);
        }
        if shown != [range.clone()] {
            self.top().left_out = true;
        }
        for piece in shown {
            self.marks(&source[piece.clone()], piece, true);
        }
    }

    /// Text with each `==` that opens or closes a highlight taken out, and
    /// each tag in the tag style. When the text is `exact`, the same as the
    /// source at `range`, the source either side of it counts too: a `==`
    /// must touch the text it highlights, and a tag must start a line or
    /// follow a space, a bracket or markup such as `*`.
    fn marks(&mut self, text: &str, range: Range<usize>, exact: bool) {
        let source = self.source;
        let before = |i: usize| match text[..i].chars().next_back() {
            None if exact => source[..range.start].chars().next_back(),
            c => c,
        };
        let after = |i: usize| match text[i..].chars().next() {
            None if exact => source[range.end..].chars().next(),
            c => c,
        };
        // Whether byte `i` follows an odd number of backslashes.
        let escaped = |i: usize| {
            let head = if exact {
                &source[..range.start + i]
            } else {
                &text[..i]
            };
            head.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
        };
        let piece = |s: usize, e: usize| match exact {
            true => range.start + s..range.start + e,
            false => range.clone(),
        };
        // An image's alt text is kept as written.
        if self.image.is_some() {
            return self.text(text, self.style(), range);
        }
        let mut start = 0;
        let mut i = 0;
        while let Some(c) = text[i..].chars().next() {
            let prev = before(i);
            if text[i..].starts_with("==")
                && prev != Some('=')
                && !escaped(i)
                && after(i + 2) != Some('=')
            {
                let open = self.top().highlight.is_some();
                let touches = match open {
                    true => prev.is_some_and(|c| !c.is_whitespace()),
                    false => after(i + 2).is_some_and(|c| !c.is_whitespace()),
                };
                if touches {
                    self.text(&text[start..i], self.style(), piece(start, i));
                    let style = self.style();
                    let top = self.top();
                    top.highlight = match open {
                        true => None,
                        false => Some((top.inlines.len(), style)),
                    };
                    i += 2;
                    start = i;
                    continue;
                }
            }
            let len = match c {
                '#' => tag_len(&text[i + 1..]),
                _ => 0,
            };
            let boundary = prev.is_none_or(|c| c.is_whitespace() || "([*_~=\"'".contains(c));
            if len > 0 && self.links.is_empty() && boundary && !escaped(i) {
                self.text(&text[start..i], self.style(), piece(start, i));
                let end = i + 1 + len;
                let style = InlineStyle {
                    tag: true,
                    ..self.style()
                };
                self.text(&text[i..end], style, piece(i, end));
                i = end;
                start = i;
                continue;
            }
            i += c.len_utf8();
        }
        self.text(&text[start..], self.style(), piece(start, text.len()));
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

/// The length in bytes of the tag name at the start of `text`, the part
/// after its `#`: letters, digits, `_`, `-` and `/`, not only digits. Zero
/// when there is no tag.
fn tag_len(text: &str) -> usize {
    let len = text
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '/')))
        .unwrap_or(text.len());
    let name = &text[..len];
    match name.chars().all(|c| c.is_ascii_digit() || c == '/') {
        true => 0,
        false => len,
    }
}

/// Obsidian comments, `%%hidden%%`, inline or across lines, as byte ranges
/// of `source` taking in both markers. A `%%` in code or maths does not
/// count, and a last one with no partner is shown as it is.
fn comments(source: &str, options: Options) -> Vec<Range<usize>> {
    if !source.contains("%%") {
        return Vec::new();
    }
    let code: Vec<Range<usize>> = Parser::new_ext(source, options)
        .into_offset_iter()
        .filter_map(|(event, range)| match event {
            Event::Start(Tag::CodeBlock(_))
            | Event::Code(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => Some(range),
            _ => None,
        })
        .collect();
    let mut marks = Vec::new();
    let mut i = 0;
    while let Some(at) = source[i..].find("%%") {
        let at = i + at;
        if !code.iter().any(|c| c.contains(&at)) {
            marks.push(at);
        }
        i = at + 2;
    }
    let (pairs, _) = marks.as_chunks::<2>();
    pairs.iter().map(|&[open, close]| open..close + 2).collect()
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
    fn task_marks_point_at_the_byte_between_the_brackets() {
        let source = "- [ ] one\n- [x] two\n> - [X] quoted\n\n1.  [ ]  loose\n\n2. [x] b\n";
        let doc = parse(source);
        let mut marks = Vec::new();
        let collect = |blocks: &[Block], marks: &mut Vec<u8>| {
            for b in blocks {
                if let BlockKind::List { items, .. } = &b.kind {
                    marks.extend(
                        items
                            .iter()
                            .filter_map(|i| i.mark)
                            .map(|m| source.as_bytes()[m]),
                    );
                }
            }
        };
        collect(&doc.blocks, &mut marks);
        if let BlockKind::Quote(inner) = &doc.blocks[1].kind {
            collect(inner, &mut marks);
        }
        // The top-level lists first, then the one in the quote.
        assert_eq!(marks, b" x xX");
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

    #[test]
    fn a_quote_starting_with_a_marker_is_a_callout() {
        assert_eq!(
            outline("> [!tip]- Read *this* first\n> body\n> more\n"),
            "\
callout Tip fold=Some(false) @0..42
  title
    \"Read \"
    \"this\" em
    \" first\"
  paragraph @28..42
    \"body more\"
"
        );
    }

    #[test]
    fn a_github_alert_takes_its_type_as_its_title() {
        assert_eq!(
            outline("> [!WARNING]\n> Mind the step.\n"),
            "\
callout Warning fold=None @0..30
  title
    \"Warning\"
  paragraph @13..30
    \"Mind the step.\"
"
        );
    }

    #[test]
    fn callout_types_aliases_and_folds() {
        let head = |md: &str| match &parse(md).blocks[0].kind {
            BlockKind::Callout { callout, blocks } => (
                callout.kind,
                callout.fold,
                callout.title.clone(),
                blocks.len(),
            ),
            other => panic!("not a callout: {other:?}"),
        };
        assert_eq!(
            head("> [!FAQ]+\n"),
            (CalloutKind::Question, Some(true), vec![text("Faq")], 0)
        );
        assert_eq!(head("> [!tldr] Short\n").0, CalloutKind::Abstract);
        assert_eq!(head("> [!cite]\n").0, CalloutKind::Quote);
        assert_eq!(head("> [!error]\n").0, CalloutKind::Danger);
        assert_eq!(head("> [!Caution]\n").0, CalloutKind::Caution);
        assert_eq!(head("> [!whatever] x\n").0, CalloutKind::Note);
        assert_eq!(head("> [!whatever]\n").2, vec![text("Whatever")]);
        // The marker alone, then a blank line, then the body.
        assert_eq!(head("> [!note]\n>\n> one\n>\n> two\n").3, 2);
    }

    #[test]
    fn a_callout_body_after_its_title_can_be_an_image_or_maths() {
        assert_eq!(
            outline("> [!note] Diagram\n> ![pic](pic.png)\n\n> [!info] Sum\n> $$x$$\n"),
            "\
callout Note fold=None @0..36
  title
    \"Diagram\"
  image \"pic.png\" alt=\"pic\" wiki=false @18..36
callout Info fold=None @37..59
  title
    \"Sum\"
  figure Math \"x\" @51..59
"
        );
    }

    #[test]
    fn a_marker_read_as_a_link_keeps_its_text() {
        assert_eq!(
            outline("> [!note](x) rest\n"),
            "\
callout Note fold=None @0..18
  title
    \"!note\" link=\"x\" wiki=false
    \" rest\"
"
        );
    }

    #[test]
    fn callouts_nest_and_sit_in_lists() {
        assert_eq!(
            outline("- > [!info] Outer\n  > > [!bug] Inner\n  > > text\n"),
            "\
list None tight=true @0..48
  item task=None
    callout Info fold=None @2..48
      title
        \"Outer\"
      callout Bug fold=None @22..48
        title
          \"Inner\"
        paragraph @37..48
          \"text\"
"
        );
    }

    #[test]
    fn a_marker_not_at_the_start_of_a_quote_is_text() {
        assert_eq!(
            outline("> one\n>\n> [!note] two\n\n[!note] three\n"),
            "\
quote @0..22
  paragraph @2..6
    \"one\"
  paragraph @10..22
    \"[!note] two\"
paragraph @23..37
  \"[!note] three\"
"
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
                BlockKind::Callout { callout, .. } => {
                    format!("callout {:?} fold={:?}", callout.kind, callout.fold)
                }
                BlockKind::List { start, tight, .. } => format!("list {start:?} tight={tight}"),
                BlockKind::Rule => "rule".to_string(),
                BlockKind::Frontmatter(Frontmatter { raw, properties }) => match properties {
                    Some(props) => format!("frontmatter {props:?}"),
                    None => format!("frontmatter raw {raw:?}"),
                },
                BlockKind::Html(html) => format!("html {html:?}"),
                BlockKind::Image { src, alt, wiki } => {
                    format!("image {src:?} alt={alt:?} wiki={wiki}")
                }
                BlockKind::Figure { kind, source } => format!("figure {kind:?} {source:?}"),
                BlockKind::Embed { title, .. } => format!("embed {title:?}"),
            };
            out.push_str(&format!("{pad}{head} @{span:?}\n"));
            match kind {
                BlockKind::Heading { content, .. } | BlockKind::Paragraph(content) => {
                    outline_inlines(content, depth + 1, out);
                }
                BlockKind::Quote(blocks) => outline_blocks(blocks, depth + 1, out),
                BlockKind::Callout { callout, blocks } => {
                    out.push_str(&format!("{pad}  title\n"));
                    outline_inlines(&callout.title, depth + 2, out);
                    outline_blocks(blocks, depth + 1, out);
                }
                BlockKind::List { items, .. } => {
                    for ListItem { task, blocks, .. } in items {
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
                        highlight,
                        tag,
                        link,
                    } = style;
                    let mut line = format!("{text:?}");
                    for (on, name) in [
                        (emphasis, "em"),
                        (strong, "strong"),
                        (strikethrough, "strike"),
                        (code, "code"),
                        (math, "math"),
                        (highlight, "highlight"),
                        (tag, "tag"),
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
    fn stray_html_is_text() {
        // The parser does not send it today: HTML comes inside an HTML block.
        use pulldown_cmark::CowStr;
        let mut builder = Builder::new("x<b>");
        builder.event(Event::Start(Tag::Paragraph), 0..4);
        builder.event(Event::Text(CowStr::from("x")), 0..1);
        builder.event(Event::Html(CowStr::from("<b>")), 1..4);
        builder.event(Event::End(TagEnd::Paragraph), 0..4);
        let kinds: Vec<BlockKind> = builder
            .finish()
            .blocks
            .into_iter()
            .map(|b| b.kind)
            .collect();
        assert_eq!(kinds, vec![BlockKind::Paragraph(vec![text("x<b>")])]);
    }

    #[test]
    fn a_reference_with_no_definition_adds_no_footnotes() {
        // The parser sends none, leaving `[^1]` as text, but a reference
        // with nothing to list adds no rule or empty list.
        use pulldown_cmark::CowStr;
        let mut builder = Builder::new("[^1]");
        builder.event(Event::Start(Tag::Paragraph), 0..4);
        builder.event(Event::FootnoteReference(CowStr::from("1")), 0..4);
        builder.event(Event::End(TagEnd::Paragraph), 0..4);
        let doc = builder.finish();
        assert_eq!(doc.blocks.len(), 1);
        assert!(doc.anchors.is_empty());
        assert_outline("Text[^1].\n", "paragraph @0..10\n  \"Text[^1].\"\n");
    }

    #[test]
    fn footnotes_go_to_the_end_numbered_by_first_reference() {
        let md = "A[^Two] b[^one] c[^two].\n\n\
                  > [^one]: In a quote.\n\n\
                  [^two]: Para.\n\n    - item\n\n[^two]: Again.\n\n[^unused]: No.\n";
        assert_outline(
            md,
            r##"paragraph @0..25
  "A"
  "[1]" link="#fn:1" wiki=false
  " b"
  "[2]" link="#fn:2" wiki=false
  " c"
  "[1]" link="#fn:1" wiki=false
  "."
rule @107..107
list Some(1) tight=false @107..107
  item task=None
    paragraph @57..63
      "Para."
    list None tight=true @68..76
      item task=None
        paragraph @70..74
          "item"
    paragraph @76..76
      "↩" link="#fnref:1" wiki=false
  item task=None
    paragraph @36..48
      "In a quote."
      " "
      "↩" link="#fnref:2" wiki=false
"##,
        );
        let doc = parse(md);
        let at = |name| &doc.source[doc.anchor(name).unwrap()..][..4];
        assert_eq!(at("fnref:1"), "[^Tw");
        assert_eq!(at("fnref:2"), "[^on");
        assert_eq!(at("fn:1"), "Para");
        assert_eq!(at("fn:2"), "In a");
        assert_eq!(doc.anchor("fn:3"), None);
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
            r##"paragraph @0..10
  "Text"
  "[1]" link="#fn:1" wiki=false
  "."
html "<!-- c -->\n" @28..39
paragraph @40..56
  "a <b>bold</b> c"
rule @57..61
rule @61..65
rule @65..65
list Some(1) tight=true @65..65
  item task=None
    paragraph @17..27
      "The note."
      " "
      "↩" link="#fnref:1" wiki=false
"##,
        );
    }

    #[test]
    fn frontmatter_at_the_start_is_read_as_properties() {
        assert_outline(
            "---\ntitle: \"A note\"\ntags:\n  - one\n  - 'two'\naliases: [a, b]\n---\n# Title\n",
            r#"frontmatter [("title", "A note"), ("tags", "one, two"), ("aliases", "a, b")] @0..63
heading 1 @64..72
  "Title"
"#,
        );
    }

    #[test]
    fn frontmatter_keeps_nested_values_as_text() {
        let props = |md: &str| match &parse(md).blocks[0].kind {
            BlockKind::Frontmatter(f) => f.properties.clone(),
            other => panic!("expected frontmatter, got {other:?}"),
        };
        assert_eq!(
            props("---\nauthor:\n  name: Ann\n  url: x\n# comment\nlist:\n- a\n- b\nempty:\n---\n"),
            Some(vec![
                ("author".into(), "name: Ann\nurl: x".into()),
                ("list".into(), "a, b".into()),
                ("empty".into(), "".into()),
            ])
        );
        assert_eq!(props("---\nnot yaml\n---\n"), None);
        assert_eq!(props("---\na: 1\n- b\n---\n"), None);
        assert_eq!(props("+++\ntitle = \"x\"\n+++\n"), None);
    }

    #[test]
    fn a_later_metadata_block_is_rules_and_a_heading() {
        // Not at the very start, so not frontmatter.
        assert_outline(
            "text\n\n---\na: b\n---\n",
            r#"paragraph @0..5
  "text"
rule @6..10
heading 2 @10..19
  "a: b"
"#,
        );
        assert_outline(
            "\n---\na: b\n---\n",
            "rule @1..5\nheading 2 @5..14\n  \"a: b\"\n",
        );
        // Frontmatter, then a rule and setext heading of the body.
        assert_outline(
            "---\nk: v\n---\n---\na: b\n---\n",
            r#"frontmatter [("k", "v")] @0..12
rule @13..17
heading 2 @17..26
  "a: b"
"#,
        );
    }

    #[test]
    fn an_unclosed_block_is_not_frontmatter() {
        assert_outline("---\na: b\n", "rule @0..4\nparagraph @4..9\n  \"a: b\"\n");
    }

    #[test]
    fn frontmatter_review_cases() {
        let props = |md: &str| match &parse(md).blocks[0].kind {
            BlockKind::Frontmatter(f) => f.properties.clone(),
            other => panic!("expected frontmatter, got {other:?}"),
        };
        let one = |k: &str, v: &str| Some(vec![(k.to_string(), v.to_string())]);
        // A no-break space in an indent does not split a character.
        assert_eq!(
            props("---\nauthor:\n  name: Ann\n \u{a0}url: x\n---\n"),
            one("author", " name: Ann\n\u{a0}url: x")
        );
        // A `#` line under a key is its text, not a comment.
        assert_eq!(
            props("---\nnotes: |\n  # Heading\n  body\n---\n"),
            one("notes", "# Heading\nbody")
        );
        // Quoted flow list items keep their brackets and commas.
        assert_eq!(
            props("---\nrelated: [\"[[Note A]]\", \"[[Note B]]\"]\n---\n"),
            one("related", "[[Note A]], [[Note B]]")
        );
        assert_eq!(
            props("---\nauthors: [\"Doe, Jane\", Smith]\n---\n"),
            one("authors", "Doe, Jane, Smith")
        );
        assert_eq!(props("---\nx: [a, [b]]\n---\n"), one("x", "[a, [b]]"));
        // A byte order mark before the block.
        assert_eq!(props("\u{feff}---\nk: v\n---\n"), one("k", "v"));
    }

    #[test]
    fn frontmatter_scalars_comments_and_quoted_keys() {
        let props = |md: &str| match &parse(md).blocks[0].kind {
            BlockKind::Frontmatter(f) => f.properties.clone(),
            other => panic!("expected frontmatter, got {other:?}"),
        };
        let one = |k: &str, v: &str| Some(vec![(k.to_string(), v.to_string())]);
        assert_eq!(
            props("---\ndescription: >\n  A long\n  summary\n---\n"),
            one("description", "A long summary")
        );
        assert_eq!(
            props("---\nsummary: one two\n  three four\n---\n"),
            one("summary", "one two three four")
        );
        assert_eq!(
            props("---\ntags: [a, b] # todo\n---\n"),
            one("tags", "a, b")
        );
        assert_eq!(props("---\ntitle: x # draft\n---\n"), one("title", "x"));
        assert_eq!(props("---\ntitle: \"a # b\"\n---\n"), one("title", "a # b"));
        assert_eq!(props("---\ntag: c#\n---\n"), one("tag", "c#"));
        assert_eq!(props("---\n\"a: b\": c\n---\n"), one("a: b", "c"));
    }

    #[test]
    fn highlights_span_styles_and_need_text_touching_their_marks() {
        assert_outline(
            "a ==b *c* d== e == f ==g\n",
            r#"paragraph @0..25
  "a "
  "b " highlight
  "c" em highlight
  " d" highlight
  " e == f "
  "=="
  "g"
"#,
        );
    }

    #[test]
    fn highlights_are_not_read_in_code_escapes_or_longer_runs() {
        assert_outline(
            "`==a==` \\==b== c===d===\n\n```\n==e==\n```\n",
            r#"paragraph @0..24
  "==a==" code
  " ==b== c===d==="
code None "==e==\n" @25..38
"#,
        );
    }

    #[test]
    fn tags_start_after_a_space_and_are_not_all_digits() {
        assert_outline(
            "#top and #a/b-c_d, e#f #123 #4x `#g` [#h](u) &amp;#i\n",
            r##"paragraph @0..53
  "#top" tag
  " and "
  "#a/b-c_d" tag
  ", e#f #123 "
  "#4x" tag
  " "
  "#g" code
  " "
  "#h" link="u" wiki=false
  " &#i"
"##,
        );
    }

    #[test]
    fn a_heading_can_hold_a_tag() {
        assert_outline(
            "## Plan #todo\n",
            r##"heading 2 @0..14
  "Plan "
  "#todo" tag
"##,
        );
    }

    #[test]
    fn comments_are_hidden_inline_and_across_lines() {
        assert_outline(
            "a %%b%% c %%d\ne%% f\n\n%%\nhidden\n\n- x\n%%\n\n`%%` g\n",
            r#"paragraph @0..20
  "a  c  f"
paragraph @40..47
  "%%" code
  " g"
"#,
        );
    }

    #[test]
    fn a_block_holding_only_a_comment_goes() {
        assert_outline(
            "> %%x%%\n\n- %%y%%\n\n# %%z%%\n\nend\n",
            "paragraph @27..31\n  \"end\"\n",
        );
    }

    #[test]
    fn a_comment_does_not_move_a_tasks_mark() {
        let doc = parse("%%a%%\n- [x] b %%c%%\n");
        let BlockKind::List { items, .. } = &doc.blocks[0].kind else {
            panic!("not a list: {:?}", doc.blocks);
        };
        assert_eq!(items[0].mark, Some(9));
        assert_eq!(&doc.source[9..10], "x");
    }

    #[test]
    fn a_comment_with_no_partner_is_shown() {
        assert_outline("a %%b\n", "paragraph @0..6\n  \"a %%b\"\n");
    }

    #[test]
    fn a_task_holding_only_a_comment_keeps_its_box() {
        assert_outline(
            "- [ ] %%note%%\n",
            "list None tight=true @0..15\n  item task=Some(false)\n",
        );
    }

    #[test]
    fn an_escaped_backslash_does_not_escape_a_highlight() {
        assert_outline(
            "\\\\==a==\n",
            "paragraph @0..8\n  \"\\\\\"\n  \"a\" highlight\n",
        );
    }

    #[test]
    fn a_tag_can_follow_markup_or_a_bracket() {
        assert_outline(
            "*#a* (#b)\n",
            r##"paragraph @0..10
  "#a" em tag
  " ("
  "#b" tag
  ")"
"##,
        );
    }

    #[test]
    fn comments_after_frontmatter_are_hidden() {
        let doc = parse("---\na: b\n---\nx %%c%% y\n");
        assert_eq!(doc.blocks[1].kind, BlockKind::Paragraph(vec![text("x  y")]));
    }

    #[test]
    fn callout_titles_and_bodies_take_highlights_and_hide_comments() {
        assert_outline(
            "> [!tip] A ==big== %%secret%% deal\n> body ==b== %%c%%\n\n> [!note] T\n> %%x%%\n",
            "\
callout Tip fold=None @0..54
  title
    \"A \"
    \"big\" highlight
    \"  deal\"
  paragraph @35..54
    \"body \"
    \"b\" highlight
    \" \"
callout Note fold=None @55..75
  title
    \"T\"
",
        );
    }

    #[test]
    fn a_highlight_left_open_in_a_callout_title_ends_with_it() {
        assert_outline(
            "> [!tip] *a* b ==c\n> d\n",
            "\
callout Tip fold=None @0..23
  title
    \"a\" em
    \" b \"
    \"==\"
    \"c\"
  paragraph @19..23
    \"d\"
",
        );
    }

    #[test]
    fn a_comment_across_a_callout_title_line_keeps_the_body_apart() {
        assert_outline(
            "> [!tip] T %%c\n> d%% body\n",
            "\
callout Tip fold=None @0..26
  title
    \"T\"
  paragraph @15..26
    \" body\"
",
        );
    }
}
