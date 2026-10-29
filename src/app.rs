//! The full-screen view: a scrollable window onto the laid-out document, and
//! the browser that moves it from note to note along links.

use std::io::{self, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::{Cell, CellDiffOption};
use ratatui::crossterm::event::{
    self, Event, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame, Terminal};

use crate::config::{Action, Config, WebLinks};
use crate::doc::{self, BlockKind, Document, Inline, Link};
use crate::highlight;
use crate::images::{self, Images};
use crate::kitty;
use crate::render::{Layout, LinkSpot, render};
use crate::theme;
use crate::vault::{self, Target, Vault};
use crate::watch::{Change, Watcher};

use unicode_width::UnicodeWidthStr;

/// How often to look at the file on disk. A change shows after two checks.
const CHECK_EVERY: Duration = Duration::from_millis(150);

pub struct View {
    /// Shared with the place Back or Forward keeps when the reader leaves.
    doc: Rc<Document>,
    /// The directory relative image paths are read from.
    base: PathBuf,
    images: Images,
    layout: Layout,
    /// Width the lines were laid out for; zero before the first layout.
    width: u16,
    /// Whether the lines were laid out with the syntaxes for code.
    highlighted: bool,
    height: u16,
    scroll: usize,
    /// A message shown on the bottom row, such as the file being deleted.
    notice: Option<String>,
    /// The selected link, as an index into the layout's links.
    selected: Option<usize>,
    /// Other notes, with their image directories, whose images are kept so
    /// they show at once when the reader goes back to them.
    kept: Vec<(Rc<Document>, PathBuf)>,
}

/// What an action asks of the browser around the view.
#[derive(Debug, Clone, PartialEq)]
pub enum Go {
    Quit,
    Follow(Link),
    Back,
    Forward,
}

/// The block at the top of the view, found again after the document is laid
/// out anew so the view stays on the same content.
struct Anchor {
    /// Where the block starts in the source.
    start: usize,
    /// How many blocks before it start at the same byte, such as a quote and
    /// its first paragraph, or display maths split from one paragraph.
    rank: usize,
    /// The top line's offset from the block's first line.
    offset: isize,
}

impl Anchor {
    /// The same anchor in `new`, an edited copy of `old`. A block that starts
    /// before the edited bytes keeps its start and one that starts after them
    /// moves with them; one that starts inside them is lost.
    fn moved(self, old: &str, new: &str) -> Option<Anchor> {
        let (old, new) = (old.as_bytes(), new.as_bytes());
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let room = old.len().min(new.len()) - prefix;
        let suffix = old.iter().rev().zip(new.iter().rev());
        let suffix = suffix.take(room).take_while(|(a, b)| a == b).count();
        let start = if self.start < prefix {
            self.start
        } else if self.start >= old.len() - suffix {
            self.start + new.len() - old.len()
        } else {
            return None;
        };
        Some(Anchor { start, ..self })
    }
}

impl View {
    pub fn new(doc: Document, base: PathBuf, images: Images) -> Self {
        let mut view = View {
            doc: Rc::new(doc),
            base,
            images,
            layout: Layout::default(),
            width: 0,
            highlighted: false,
            height: 0,
            scroll: 0,
            notice: None,
            selected: None,
            kept: Vec::new(),
        };
        view.want_images();
        view
    }

    /// Show another document, from the top, with images read from `base`.
    pub fn show(&mut self, doc: Document, base: PathBuf) {
        self.doc = Rc::new(doc);
        self.base = base;
        self.scroll = 0;
        self.selected = None;
        self.want_images();
        self.relayout(None);
    }

    /// Scroll to the first heading matching `wanted`. Returns false when
    /// there is none.
    pub fn jump_to_heading(&mut self, wanted: &str) -> bool {
        let Some(start) = heading_start(&self.doc.blocks, wanted) else {
            return false;
        };
        if let Some(b) = self.layout.blocks.iter().find(|b| b.span.start == start) {
            self.scroll = b.rows.start.min(self.max_scroll());
        }
        true
    }

    /// Show a new version of the document, keeping the block at the top of the
    /// view in place, or the same line number when that block is gone. Images
    /// it no longer has are deleted on the next flush.
    pub fn reload(&mut self, doc: Document) {
        let anchor = self.anchor();
        let old = std::mem::replace(&mut self.doc, Rc::new(doc));
        let anchor = anchor.and_then(|a| a.moved(&old.source, &self.doc.source));
        self.selected = None;
        self.want_images();
        self.relayout(anchor);
    }

    fn want_images(&mut self) {
        let mut wanted = Vec::new();
        images::wanted_in(&self.doc.blocks, &self.base, &mut wanted);
        for (doc, base) in &self.kept {
            images::wanted_in(&doc.blocks, base, &mut wanted);
        }
        self.images.set_base(&self.base);
        self.images.want(wanted);
    }

    fn relayout(&mut self, anchor: Option<Anchor>) {
        if self.width > 0 {
            self.highlighted = highlight::ready().is_some();
            self.layout = render(&self.doc, self.width, &self.images);
            if let Some(row) = anchor.and_then(|a| self.find(&a)) {
                self.scroll = row;
            }
        }
        self.scroll = self.scroll.min(self.max_scroll());
        self.selected = self.selected.filter(|&i| i < self.layout.links.len());
    }

    /// What to keep at the top of the view. A view at the very top stays there.
    fn anchor(&self) -> Option<Anchor> {
        if self.scroll == 0 {
            return None;
        }
        let (index, offset) = self.layout.anchor(self.scroll)?;
        let blocks = &self.layout.blocks;
        let start = blocks[index].span.start;
        let rank = blocks[..index]
            .iter()
            .filter(|b| b.span.start == start)
            .count();
        Some(Anchor {
            start,
            rank,
            offset,
        })
    }

    /// The top line that puts `anchor` back where it was: the same offset into
    /// its block. A block that got shorter keeps the view on its last line.
    fn find(&self, anchor: &Anchor) -> Option<usize> {
        let b = self
            .layout
            .blocks
            .iter()
            .filter(|b| b.span.start == anchor.start)
            .nth(anchor.rank)?;
        let last = b.rows.len().saturating_sub(1) as isize;
        Some(b.rows.start.saturating_add_signed(anchor.offset.min(last)))
    }

    /// Take in images that became ready. Returns true when the view changed.
    pub fn receive_images(&mut self) -> bool {
        if !self.images.receive() {
            return false;
        }
        // The layout is still the one on screen, so the anchor comes from it.
        let anchor = self.anchor();
        self.relayout(anchor);
        true
    }

    /// Lay the document out again once the syntaxes for code have loaded.
    /// Returns true when the view changed.
    pub fn receive_syntaxes(&mut self) -> bool {
        if self.highlighted || self.width == 0 || highlight::ready().is_none() {
            return false;
        }
        let anchor = self.anchor();
        self.relayout(anchor);
        true
    }

    /// Send the terminal what it needs to show the images on the last frame.
    /// This goes straight to the terminal, outside ratatui's buffer.
    pub fn flush_images(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.images.flush(out, &self.layout.placements())
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Lay the document out again when the size changes, keeping roughly the
    /// same part of it in view.
    fn resize(&mut self, width: u16, height: u16) {
        if width != self.width {
            let old_len = self.layout.lines.len().max(1);
            self.highlighted = highlight::ready().is_some();
            self.layout = render(&self.doc, width, &self.images);
            self.scroll = self.scroll * self.layout.lines.len() / old_len;
            self.width = width;
            self.selected = self.selected.filter(|&i| i < self.layout.links.len());
        }
        self.height = height;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.layout.lines.len().saturating_sub(self.height as usize)
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll());
    }

    #[cfg(test)]
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.draw_in(frame, area, |_| None);
    }

    /// Draw the view in `area`. Each link that `href` gives an address for
    /// is marked as a terminal hyperlink (OSC 8), so it can be clicked.
    fn draw_in(
        &mut self,
        frame: &mut Frame,
        mut area: Rect,
        href: impl Fn(&Link) -> Option<String>,
    ) {
        if let Some(notice) = &self.notice
            && area.height > 0
        {
            area.height -= 1;
            let row = Rect::new(area.x, area.y + area.height, area.width, 1);
            let style = Style::new().add_modifier(Modifier::REVERSED);
            frame.render_widget(Paragraph::new(notice.as_str()).style(style), row);
        }
        self.resize(area.width, area.height);
        let lines = &self.layout.lines;
        let end = (self.scroll + area.height as usize).min(lines.len());
        let visible = Text::from(lines[self.scroll..end].to_vec());
        frame.render_widget(Paragraph::new(visible), area);
        // The cells of a link that are in view.
        let scroll = self.scroll;
        let cells = |spot: &LinkSpot| -> Vec<Rect> {
            spot.cells
                .iter()
                .filter(|(row, cols)| (scroll..end).contains(row) && cols.start < area.width)
                .map(|(row, cols)| {
                    let y = area.y + (row - scroll) as u16;
                    let width = cols.end.min(area.width) - cols.start;
                    Rect::new(area.x + cols.start, y, width, 1)
                })
                .collect()
        };
        let buf = frame.buffer_mut();
        // The selected link is shown in reverse.
        if let Some(spot) = self.selected.and_then(|i| self.layout.links.get(i)) {
            for rect in cells(spot) {
                buf.set_style(rect, Style::new().add_modifier(Modifier::REVERSED));
            }
        }
        for (i, spot) in self.layout.links.iter().enumerate() {
            let rects = cells(spot);
            if rects.is_empty() {
                continue;
            }
            let Some(url) = href(&spot.link) else {
                continue;
            };
            // Control characters would end the escape early.
            let url: String = url.chars().filter(|c| !c.is_control()).collect();
            for rect in rects {
                for x in rect.left()..rect.right() {
                    hyperlink(&mut buf[(x, rect.y)], i, &url);
                }
            }
        }
    }

    /// The link drawn at column `x` of row `y` of the view, if any.
    fn link_at(&self, x: u16, y: u16) -> Option<usize> {
        if y >= self.height {
            return None;
        }
        let line = self.scroll + y as usize;
        self.layout.links.iter().position(|spot| {
            spot.cells
                .iter()
                .any(|(row, cols)| *row == line && cols.contains(&x))
        })
    }

    /// Do what an action says. Returns what the browser must do, if anything.
    pub fn act(&mut self, action: Action) -> Option<Go> {
        let page = (self.height as isize).max(1);
        let half = (page / 2).max(1);
        match action {
            Action::Quit => return Some(Go::Quit),
            Action::Down => self.scroll_by(1),
            Action::Up => self.scroll_by(-1),
            Action::HalfDown => self.scroll_by(half),
            Action::HalfUp => self.scroll_by(-half),
            Action::PageDown => self.scroll_by(page),
            Action::PageUp => self.scroll_by(-page),
            Action::Top => self.scroll = 0,
            Action::Bottom => self.scroll = self.max_scroll(),
            Action::NextLink => self.select_link(true),
            Action::PrevLink => self.select_link(false),
            Action::Follow => {
                let spot = self.selected.and_then(|i| self.layout.links.get(i))?;
                return Some(Go::Follow(spot.link.clone()));
            }
            Action::Back => return Some(Go::Back),
            Action::Forward => return Some(Go::Forward),
        }
        None
    }

    /// The rows in view.
    fn in_view(&self) -> std::ops::Range<usize> {
        self.scroll..self.scroll + self.height as usize
    }

    /// Select the next or previous link, going round at the ends. With no
    /// link selected in view, start from the first or last one in view.
    fn select_link(&mut self, forward: bool) {
        let links = &self.layout.links;
        let row = |i: usize| links[i].cells.first().map_or(0, |c| c.0);
        let view = self.in_view();
        let selected = self.selected.filter(|&i| view.contains(&row(i)));
        let next = match (selected, forward) {
            (Some(i), true) => Some((i + 1) % links.len()),
            (Some(i), false) => Some((i + links.len() - 1) % links.len()),
            (None, true) => (0..links.len())
                .find(|&i| row(i) >= view.start)
                .or((!links.is_empty()).then_some(0)),
            (None, false) => (0..links.len())
                .rev()
                .find(|&i| row(i) < view.end)
                .or(links.len().checked_sub(1)),
        };
        let Some(next) = next else {
            return;
        };
        self.selected = Some(next);
        // Bring it into view, a little way down the page when that moves it.
        let row = row(next);
        if !self.in_view().contains(&row) {
            self.scroll = row
                .saturating_sub(self.height as usize / 3)
                .min(self.max_scroll());
        }
    }
}

/// Where the first heading matching `wanted` starts in the source, looking
/// inside quotes and lists too.
fn heading_start(blocks: &[doc::Block], wanted: &str) -> Option<usize> {
    blocks.iter().find_map(|b| match &b.kind {
        BlockKind::Heading { content, .. } => {
            let text: String = content
                .iter()
                .map(|i| match i {
                    Inline::Text { text, .. } => text.as_str(),
                    Inline::Break => " ",
                })
                .collect();
            vault::heading_matches(&text, wanted).then_some(b.span.start)
        }
        BlockKind::Quote(inner) => heading_start(inner, wanted),
        BlockKind::List { items, .. } => items
            .iter()
            .find_map(|item| heading_start(&item.blocks, wanted)),
        _ => None,
    })
}

/// Make `cell` part of link `id` to `url`. Each cell opens and closes the
/// link around its own text, so a cell redrawn alone is still right, and
/// the shared id lets the terminal treat them as one link.
fn hyperlink(cell: &mut Cell, id: usize, url: &str) {
    let text = cell.symbol();
    // The second column of a wide character holds nothing to wrap.
    let Some(width) = NonZeroU16::new(text.width() as u16) else {
        return;
    };
    let symbol = format!("\x1b]8;id=smd{id};{url}\x1b\\{text}\x1b]8;;\x1b\\");
    cell.set_symbol(&symbol);
    // The escapes take no room on screen.
    cell.set_diff_option(CellDiffOption::ForcedWidth(width));
}

/// A `file://` address for `path`, with the bytes a URL cannot hold as is
/// escaped.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_encoded_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// Apply what the watcher saw to the view.
fn apply(view: &mut View, path: &Path, vault: &Vault, change: Change) {
    match change {
        Change::Changed(source) => {
            view.reload(parse(vault, &source, path));
            view.set_notice(None);
        }
        // The path goes last so a narrow pane cuts it rather than the message.
        Change::Deleted => view.set_notice(Some(format!(
            "file deleted, showing the last version: {}",
            path.display()
        ))),
        Change::Unreadable(err) => {
            view.set_notice(Some(format!("cannot read file: {err}: {}", path.display())))
        }
    }
}

/// `source`, the note at `path`, parsed with its embedded images found.
pub fn parse(vault: &Vault, source: &str, path: &Path) -> Document {
    let mut doc = doc::parse(source);
    vault.resolve_embeds(&mut doc.blocks, path);
    doc
}

/// The folder a note's relative image paths are read from.
pub fn folder(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => ".".into(),
    }
}

/// A note left by following a link, to come back to.
struct Visited {
    path: PathBuf,
    doc: Rc<Document>,
    scroll: usize,
    selected: Option<usize>,
}

/// The view and the note it shows, the way back, and what is needed to
/// follow links from it.
pub struct Browser {
    view: View,
    path: PathBuf,
    watcher: Watcher,
    vault: Vault,
    config: Config,
    /// Where Back goes, the last place last.
    history: Vec<Visited>,
    /// Where Forward goes, after going back.
    ahead: Vec<Visited>,
    /// The notice on screen is about the last key, and goes at the next.
    passing: bool,
    /// Where a copied link's OSC 52 sequence is written: the terminal.
    clipboard: Box<dyn Write>,
    /// What opens a link that is handed on.
    opener: fn(&str) -> io::Result<()>,
}

impl Browser {
    pub fn new(view: View, path: PathBuf, watcher: Watcher, vault: Vault, config: Config) -> Self {
        Browser {
            view,
            path,
            watcher,
            vault,
            config,
            history: Vec::new(),
            ahead: Vec::new(),
            passing: false,
            clipboard: Box::new(io::stdout()),
            opener: open_with_system,
        }
    }

    /// Show a short message until the next key.
    fn say(&mut self, notice: String) {
        self.view.set_notice(Some(notice));
        self.passing = true;
    }

    /// Handle a mouse event: a click on a link follows it, and the wheel
    /// scrolls.
    fn mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if event.modifiers.is_empty() => {
                if std::mem::take(&mut self.passing) {
                    self.view.set_notice(None);
                }
                // The toolbar is the top row; the note is below it.
                let Some(row) = event.row.checked_sub(1) else {
                    let x = event.column as usize;
                    let back = BACK_BUTTON.width();
                    if x < back {
                        self.back();
                    } else if x < back + FORWARD_BUTTON.width() {
                        self.forward();
                    }
                    return;
                };
                if let Some(i) = self.view.link_at(event.column, row) {
                    self.view.selected = Some(i);
                    let link = self.view.layout.links[i].link.clone();
                    self.follow(&link);
                }
            }
            MouseEventKind::ScrollDown => self.view.scroll_by(WHEEL_LINES),
            MouseEventKind::ScrollUp => self.view.scroll_by(-WHEEL_LINES),
            _ => {}
        }
    }

    /// Handle a key press. Returns true when the reader should quit.
    fn key(&mut self, key: event::KeyEvent) -> bool {
        if std::mem::take(&mut self.passing) {
            self.view.set_notice(None);
        }
        let Some(action) = self.config.action(key) else {
            return false;
        };
        match self.view.act(action) {
            Some(Go::Quit) => return true,
            Some(Go::Follow(link)) => self.follow(&link),
            Some(Go::Back) => self.back(),
            Some(Go::Forward) => self.forward(),
            None => {}
        }
        false
    }

    fn follow(&mut self, link: &Link) {
        match self.vault.follow(link, &self.path) {
            Target::Note { path, heading } if path == self.path => {
                if let Some(h) = heading {
                    self.jump(&h);
                }
            }
            Target::Note { path, heading } => {
                let here = self.here();
                // Forward goes nowhere after a new link, so its notes keep
                // no images.
                let ahead = std::mem::take(&mut self.ahead);
                if self.open(path) {
                    self.history.push(here);
                    if let Some(h) = heading {
                        self.jump(&h);
                    }
                } else {
                    self.ahead = ahead;
                }
            }
            Target::Heading(h) => self.jump(&h),
            Target::Web(url) => self.hand_on(&url),
            Target::Other(path) => self.hand_on(&path.to_string_lossy()),
            Target::Missing(name) => self.say(format!("no note found: {name}")),
        }
    }

    fn jump(&mut self, heading: &str) {
        if !self.view.jump_to_heading(heading) {
            self.say(format!("no heading found: {heading}"));
        }
    }

    /// Go back to the note the last link was followed from, where it was.
    fn back(&mut self) {
        if let Some(visited) = self.history.pop() {
            let here = self.here();
            if self.revisit(visited) {
                self.ahead.push(here);
            }
        }
    }

    /// Go forward again to the note Back left.
    fn forward(&mut self) {
        if let Some(visited) = self.ahead.pop() {
            let here = self.here();
            if self.revisit(visited) {
                self.history.push(here);
            }
        }
    }

    /// Where the reader is now, to come back to.
    fn here(&self) -> Visited {
        Visited {
            path: self.path.clone(),
            doc: Rc::clone(&self.view.doc),
            scroll: self.view.scroll,
            selected: self.view.selected,
        }
    }

    /// Open a note again where it was left. Returns false when it cannot
    /// be read.
    fn revisit(&mut self, visited: Visited) -> bool {
        if !self.open(visited.path) {
            return false;
        }
        self.view.scroll = visited.scroll.min(self.view.max_scroll());
        self.view.selected = visited.selected;
        self.view.relayout(None);
        true
    }

    /// Draw the toolbar on the top row and the note below it.
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let bar = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let rest = Rect::new(
            area.x,
            area.y + bar.height,
            area.width,
            area.height - bar.height,
        );
        // Styled as a whole, so the bar reaches the right edge.
        let toolbar = self.toolbar();
        let style = toolbar.style;
        frame.render_widget(Paragraph::new(toolbar).style(style), bar);
        let (vault, path) = (&self.vault, &self.path);
        // With the mouse, smd follows clicks itself; without it, links are
        // hyperlinks for the terminal to open.
        let mouse = self.config.mouse;
        self.view.draw_in(frame, rest, |link| {
            (!mouse).then(|| href(vault, link, path))?
        });
    }

    /// The toolbar: Back and Forward, dimmed when there is nowhere to go,
    /// then the note's name.
    fn toolbar(&self) -> Line<'static> {
        let base = match theme::code_panel() {
            Some(bg) => Style::new().bg(bg),
            None => Style::new().add_modifier(Modifier::REVERSED),
        };
        let button = |label: &'static str, enabled: bool| {
            let style = if enabled {
                base.add_modifier(Modifier::BOLD)
            } else {
                base.add_modifier(Modifier::DIM)
            };
            Span::styled(label, style)
        };
        let name = self
            .path
            .strip_prefix(&self.vault.root)
            .unwrap_or(&self.path);
        let name = name.with_extension("");
        Line::from(vec![
            button(BACK_BUTTON, !self.history.is_empty()),
            button(FORWARD_BUTTON, !self.ahead.is_empty()),
            Span::styled(format!(" {}", name.display()), base),
        ])
        .style(base)
    }

    /// Show the note at `path`, watching it from now on. Returns false, with
    /// a notice, when it cannot be read.
    fn open(&mut self, path: PathBuf) -> bool {
        // Made before the read, so a save in between is seen.
        let watcher = Watcher::new(path.clone());
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(err) => {
                self.say(format!("cannot read file: {err}: {}", path.display()));
                return false;
            }
        };
        if !self.vault.holds(&path) {
            self.vault = Vault::for_file(&path);
        }
        let doc = parse(&self.vault, &source, &path);
        // Keep the images of the nearest notes Back and Forward lead to, and
        // of the one being left, which is about to join them.
        let back = self.history.iter().rev().take(KEEP_IMAGES_OF);
        let notes = back.chain(self.ahead.iter().rev().take(KEEP_IMAGES_OF));
        let mut kept: Vec<_> = notes.map(|v| (v.doc.clone(), folder(&v.path))).collect();
        kept.push((Rc::clone(&self.view.doc), folder(&self.path)));
        self.view.kept = kept;
        self.view.show(doc, folder(&path));
        self.view.set_notice(None);
        self.watcher = watcher;
        self.path = path;
        true
    }

    /// Hand a web link, or a file smd does not show, to whatever the config
    /// says.
    fn hand_on(&mut self, target: &str) {
        match self.config.web_links {
            WebLinks::Open => {
                if let Err(err) = (self.opener)(target) {
                    self.say(format!("cannot open {target}: {err}"));
                }
            }
            WebLinks::Copy => {
                let osc = format!("\x1b]52;c;{}\x1b\\", kitty::base64(target.as_bytes()));
                let out = &mut self.clipboard;
                match out.write_all(osc.as_bytes()).and_then(|()| out.flush()) {
                    Ok(()) => self.say(format!("copied {target}")),
                    Err(err) => self.say(format!("cannot copy: {err}")),
                }
            }
            WebLinks::Ignore => {}
        }
    }
}

/// Where clicking a link in the note at `from` goes: a web address, or the
/// file a note or other local link leads to. A heading in the same note
/// and a link that leads nowhere have none.
fn href(vault: &Vault, link: &Link, from: &Path) -> Option<String> {
    match vault.follow(link, from) {
        Target::Web(url) => Some(url),
        Target::Note { path, .. } | Target::Other(path) => Some(file_url(&path)),
        Target::Heading(_) | Target::Missing(_) => None,
    }
}

/// Open `target` with the system's opener, without waiting for it.
fn open_with_system(target: &str) -> io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(opener)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reaped in the background so it leaves no zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// The toolbar's buttons, as drawn from its left edge.
const BACK_BUTTON: &str = " ← Back ";
const FORWARD_BUTTON: &str = " Forward → ";

/// How many notes each way along Back and Forward keep their images.
const KEEP_IMAGES_OF: usize = 8;

/// How far one step of the mouse wheel scrolls.
const WHEEL_LINES: isize = 3;

/// Asks the terminal to report mouse presses and releases (not movement),
/// in the SGR form, and to stop.
pub const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
pub const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

/// Run the browser until the reader quits, reloading the note when its file
/// changes. Images are deleted from the terminal however it ends.
pub fn run(terminal: &mut DefaultTerminal, mut browser: Browser) -> io::Result<()> {
    let result = run_browser(terminal, &mut browser, &mut Live);
    let deleted = browser.view.images.delete_all(terminal.backend_mut());
    result.and(deleted)
}

/// Where the run loop gets the terminal's events and the time, so a test
/// can stand in for both.
trait Input {
    fn now(&self) -> Instant;
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

/// The real terminal and clock.
struct Live;

impl Input for Live {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

fn run_browser<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    browser: &mut Browser,
    input: &mut impl Input,
) -> io::Result<()> {
    let mut last_check = input.now();
    let mut redraw = true;
    loop {
        if redraw {
            terminal.draw(|frame| browser.draw(frame))?;
            browser.view.flush_images(terminal.backend_mut())?;
            redraw = false;
        }
        let elapsed = input.now().saturating_duration_since(last_check);
        if input.poll(CHECK_EVERY.saturating_sub(elapsed))? {
            // A resize needs no handling beyond the redraw.
            redraw = true;
            match input.read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press && browser.key(key) => {
                    return Ok(());
                }
                Event::Mouse(mouse) => browser.mouse(mouse),
                _ => {}
            }
        }
        if input.now().saturating_duration_since(last_check) >= CHECK_EVERY {
            last_check = input.now();
            if let Some(change) = browser.watcher.check() {
                apply(&mut browser.view, &browser.path, &browser.vault, change);
                redraw = true;
            }
            let view = &mut browser.view;
            // Images are made on other threads; this is where they show up.
            if view.receive_images() {
                redraw = true;
            }
            // So are the syntaxes code is coloured with.
            if view.receive_syntaxes() {
                redraw = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::parse;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    impl View {
        /// Act on a key with the default keys. Returns true for quit.
        fn key(&mut self, key: KeyEvent) -> bool {
            let action = Config::default().action(key);
            action.and_then(|a| self.act(a)) == Some(Go::Quit)
        }
    }

    fn new_view(md: &str) -> View {
        View::new(parse(md), PathBuf::new(), Images::new((8, 16)))
    }

    fn view(paragraphs: usize) -> View {
        let md: String = (0..paragraphs).map(|n| format!("line {n}\n\n")).collect();
        let mut v = new_view(&md);
        v.resize(20, 5);
        v
    }

    fn press(v: &mut View, code: KeyCode) -> bool {
        v.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn scrolling_stays_within_the_document() {
        // 10 paragraphs lay out as 19 lines; 5 fit, so the last top line is 14.
        let mut v = view(10);
        press(&mut v, KeyCode::Char('k'));
        assert_eq!(v.scroll, 0);
        press(&mut v, KeyCode::Char('j'));
        press(&mut v, KeyCode::Down);
        assert_eq!(v.scroll, 2);
        press(&mut v, KeyCode::PageDown);
        assert_eq!(v.scroll, 7);
        press(&mut v, KeyCode::Char('G'));
        assert_eq!(v.scroll, 14);
        press(&mut v, KeyCode::PageDown);
        assert_eq!(v.scroll, 14);
        press(&mut v, KeyCode::PageUp);
        assert_eq!(v.scroll, 9);
        press(&mut v, KeyCode::Char('g'));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn control_keys_scroll_by_half_and_whole_pages() {
        let mut v = view(10);
        let ctrl = |v: &mut View, c| v.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        ctrl(&mut v, 'd');
        assert_eq!(v.scroll, 2);
        ctrl(&mut v, 'f');
        assert_eq!(v.scroll, 7);
        ctrl(&mut v, 'b');
        assert_eq!(v.scroll, 2);
        ctrl(&mut v, 'u');
        assert_eq!(v.scroll, 0);
        // Control does not turn a letter into its plain key.
        assert!(!ctrl(&mut v, 'q'));
        ctrl(&mut v, 'g');
        ctrl(&mut v, 'j');
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn short_documents_do_not_scroll() {
        let mut v = view(1);
        press(&mut v, KeyCode::Char('G'));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn quit_keys() {
        let mut v = view(1);
        assert!(press(&mut v, KeyCode::Char('q')));
        assert!(press(&mut v, KeyCode::Esc));
        assert!(v.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!press(&mut v, KeyCode::Char('c')));
    }

    #[test]
    fn draw_reflows_on_resize() {
        let mut v = new_view("alpha beta gamma\n");
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal.draw(|f| v.draw(f)).unwrap();
        assert_eq!(v.layout.lines.len(), 1);
        terminal.backend_mut().resize(6, 3);
        terminal.draw(|f| v.draw(f)).unwrap();
        terminal
            .backend()
            .assert_buffer_lines(["alpha ", "beta  ", "gamma "]);
    }

    #[test]
    fn reload_keeps_the_scroll_position() {
        let mut v = view(10);
        press(&mut v, KeyCode::PageDown);
        assert_eq!(v.scroll, 5);
        // Appending keeps the same top line.
        v.reload(parse(
            &(0..12).map(|n| format!("line {n}\n\n")).collect::<String>(),
        ));
        assert_eq!(v.scroll, 5);
        assert_eq!(v.layout.lines.len(), 23);
        // A shorter document pulls the view back to its end.
        v.reload(parse("one\n\ntwo\n\nthree\n\nfour\n"));
        assert_eq!(v.scroll, 2);
    }

    /// A view of `md` read from a fresh directory holding `files`, with every
    /// image it names loaded.
    fn view_with_files(name: &str, md: &str, files: &[(&str, Vec<u8>)]) -> View {
        let dir = std::env::temp_dir().join(format!("smd-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, data) in files {
            std::fs::write(dir.join(file), data).unwrap();
        }
        let mut v = View::new(parse(md), dir.clone(), Images::new((10, 20)));
        crate::images::tests::settle(&mut v.images);
        let _ = std::fs::remove_dir_all(&dir);
        v
    }

    fn commands(out: Vec<u8>) -> Vec<String> {
        String::from_utf8(out)
            .unwrap()
            .split("\x1b\\")
            .filter(|c| !c.is_empty())
            .map(|c| c.chars().take_while(|&c| c != ';').collect())
            .collect()
    }

    #[test]
    fn placeholder_cells_survive_the_buffer_and_reach_the_terminal() {
        use crate::images::tests::png;
        use crate::kitty;
        use ratatui::backend::{Backend, CrosstermBackend};

        let md = "top\n\n![a](a.png)\n\nend\n";
        let mut v = view_with_files("cells", md, &[("a.png", png(20, 40))]);
        v.receive_images();
        let mut terminal = Terminal::new(TestBackend::new(6, 3)).unwrap();
        terminal.draw(|f| v.draw(f)).unwrap();
        let id = v.layout.placements()[0].id;
        let first = terminal.backend().buffer().clone();
        // 20x40 px is 2x2 cells, on rows 2 and 3 of the document.
        assert_eq!(first[(0, 2)].symbol(), kitty::cell(0, 0));
        assert_eq!(first[(1, 2)].symbol(), kitty::cell(0, 1));
        assert_eq!(first[(2, 2)].symbol(), " ");
        assert_eq!(first[(0, 2)].fg, kitty::colour(id));

        // Scrolling one line changes which image row each screen row shows.
        press(&mut v, KeyCode::Down);
        terminal.draw(|f| v.draw(f)).unwrap();
        let second = terminal.backend().buffer().clone();
        assert_eq!(second[(0, 1)].symbol(), kitty::cell(0, 0));
        assert_eq!(second[(0, 2)].symbol(), kitty::cell(1, 0));
        let diff = first.diff(&second);
        assert!(diff.contains(&(0, 2, &second[(0, 2)])));

        // The crossterm backend writes the cell whole, after its colour.
        let mut bytes = Vec::new();
        CrosstermBackend::new(&mut bytes)
            .draw(diff.into_iter())
            .unwrap();
        let bytes = String::from_utf8(bytes).unwrap();
        let colour = format!("38;2;{};{};{}", id >> 16 & 0xff, id >> 8 & 0xff, id & 0xff);
        let cell = kitty::cell(1, 0);
        assert!(bytes.contains(&colour), "{bytes:?}");
        assert!(bytes.find(&colour) < bytes.find(&cell), "{bytes:?}");
    }

    #[test]
    fn redraws_send_no_image_data_and_reload_deletes_removed_images() {
        use crate::images::tests::png;
        let md = "![a](a.png)\n\n![b](b.png)\n";
        let files = [("a.png", png(20, 20)), ("b.png", png(20, 20))];
        let mut v = view_with_files("reload", md, &files);
        v.receive_images();
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).unwrap();
        terminal.draw(|f| v.draw(f)).unwrap();
        let [a, b] = [0, 1].map(|i| v.layout.placements()[i].id);

        let mut out = Vec::new();
        v.flush_images(&mut out).unwrap();
        assert_eq!(
            commands(out),
            vec![
                format!("\x1b_Ga=t,f=100,i={a},q=2,m=0"),
                format!("\x1b_Ga=p,U=1,i={a},p=1,c=2,r=1,q=2"),
                format!("\x1b_Ga=t,f=100,i={b},q=2,m=0"),
                format!("\x1b_Ga=p,U=1,i={b},p=1,c=2,r=1,q=2"),
            ]
        );
        press(&mut v, KeyCode::Down);
        terminal.draw(|f| v.draw(f)).unwrap();
        let mut out = Vec::new();
        v.flush_images(&mut out).unwrap();
        assert!(out.is_empty());

        v.reload(parse("![b](b.png)\n\ntext\n"));
        terminal.draw(|f| v.draw(f)).unwrap();
        let mut out = Vec::new();
        v.flush_images(&mut out).unwrap();
        assert_eq!(commands(out), vec![format!("\x1b_Ga=d,d=I,i={a},q=2")]);
        assert_eq!(v.layout.placements()[0].id, b);
    }

    #[test]
    fn reload_keeps_the_top_block_in_place() {
        let mut v = view(10);
        press(&mut v, KeyCode::PageDown);
        assert_eq!(top(&v), "");
        press(&mut v, KeyCode::Down);
        assert_eq!(top(&v), "line 3");
        // Two paragraphs added above move the view down with its block.
        let md: String = (0..10).map(|n| format!("line {n}\n\n")).collect();
        v.reload(parse(&format!("new\n\nnew\n\n{md}")));
        assert_eq!(v.scroll, 10);
        assert_eq!(top(&v), "line 3");
        // A view at the top stays there when lines are added above.
        press(&mut v, KeyCode::Char('g'));
        v.reload(parse(&format!("more\n\n{md}")));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn reload_tells_blocks_with_the_same_text_apart() {
        let md = "same\n\n".repeat(10);
        let mut v = new_view(&md);
        v.resize(20, 5);
        v.scroll_by(8);
        // A copy added above pushes the block the view is on down with it.
        v.reload(parse(&format!("new\n\nsame\n\n{md}")));
        assert_eq!(v.scroll, 12);
    }

    /// The text of the top line of the view.
    fn top(v: &View) -> String {
        v.layout.lines[v.scroll].to_string()
    }

    /// A 20x5 view of `md` read from a fresh directory holding `a.png`, an
    /// image 5 rows tall, with the view scrolled `down` lines before the image
    /// arrives.
    fn arriving(name: &str, md: &str, down: usize) -> View {
        use crate::images::tests::png;
        let dir = std::env::temp_dir().join(format!("smd-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), png(20, 100)).unwrap();
        let mut v = View::new(parse(md), dir.clone(), Images::new((10, 20)));
        v.resize(20, 5);
        v.scroll_by(down as isize);
        assert_eq!(v.scroll, down);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !v.receive_images() {
            assert!(Instant::now() < deadline, "image not ready");
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = std::fs::remove_dir_all(&dir);
        v
    }

    fn paragraphs(n: usize) -> String {
        (0..n).map(|n| format!("line {n}\n\n")).collect()
    }

    #[test]
    fn an_image_arriving_above_the_view_keeps_the_top_line() {
        // As text the image is one line, so "line 1" is on line 6.
        let md = format!("top\n\n![a](a.png)\n\n{}", paragraphs(10));
        let v = arriving("above", &md, 6);
        assert_eq!(v.layout.blocks[1].rows, 2..7);
        assert_eq!(v.scroll, 10);
        assert_eq!(top(&v), "line 1");
    }

    #[test]
    fn an_image_arriving_below_the_view_keeps_the_top_line() {
        let md = format!("{}![a](a.png)\n", paragraphs(10));
        let v = arriving("below", &md, 2);
        assert_eq!(v.scroll, 2);
        assert_eq!(top(&v), "line 1");
    }

    #[test]
    fn a_view_at_the_top_stays_there_when_an_image_arrives() {
        let v = arriving("top", &format!("![a](a.png)\n\n{}", paragraphs(10)), 0);
        assert_eq!(v.scroll, 0);
        assert_eq!(v.layout.blocks[0].rows, 0..5);
    }

    #[test]
    fn notice_takes_the_bottom_row_and_clears() {
        let mut v = new_view("one\n\ntwo\n");
        let mut terminal = Terminal::new(TestBackend::new(8, 3)).unwrap();
        let path = Path::new("a.md");
        let vault = Vault::for_file(path);
        let apply = |v: &mut View, change| apply(v, path, &vault, change);
        apply(&mut v, Change::Deleted);
        terminal.draw(|f| v.draw(f)).unwrap();
        assert_eq!(v.height, 2);
        let buf = terminal.backend().buffer();
        let row: String = (0..8).map(|x| buf[(x, 2)].symbol()).collect();
        assert_eq!(row, "file del");
        assert_eq!(buf[(0, 0)].symbol(), "o");
        apply(&mut v, Change::Changed("back\n".into()));
        terminal.draw(|f| v.draw(f)).unwrap();
        terminal
            .backend()
            .assert_buffer_lines(["back    ", "        ", "        "]);
    }

    #[test]
    fn tab_selects_links_in_view_and_enter_follows() {
        let md = format!("[[a]] and [b](b.md)\n\n{}[[c|see c]]\n", paragraphs(10));
        let mut v = new_view(&md);
        v.resize(20, 5);
        let tab = |v: &mut View| v.act(Action::NextLink);
        assert_eq!(v.act(Action::Follow), None);
        tab(&mut v);
        assert_eq!(
            v.act(Action::Follow),
            Some(Go::Follow(Link {
                dest: "a".into(),
                wiki: true
            }))
        );
        tab(&mut v);
        assert_eq!(v.act(Action::Follow), Some(Go::Follow("b.md".into())));
        // The next one is off the page, so the view moves to it.
        tab(&mut v);
        assert_eq!(v.selected, Some(2));
        assert!(v.in_view().contains(&v.layout.links[2].cells[0].0));
        // Round to the first, and back again.
        tab(&mut v);
        assert_eq!(v.selected, Some(0));
        v.act(Action::PrevLink);
        assert_eq!(v.selected, Some(2));
        // Scrolled away from the selection, Tab starts from the view.
        v.act(Action::Top);
        v.act(Action::Down);
        v.act(Action::Down);
        tab(&mut v);
        assert_eq!(v.selected, Some(2));
    }

    #[test]
    fn the_selected_link_is_drawn_in_reverse() {
        let mut v = new_view("go [[far away]] now\n");
        let mut terminal = Terminal::new(TestBackend::new(30, 2)).unwrap();
        v.resize(30, 2);
        v.act(Action::NextLink);
        terminal.draw(|f| v.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let reversed: String = (0..30)
            .filter(|&x| buf[(x, 0)].modifier.contains(Modifier::REVERSED))
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert_eq!(reversed, "far away");
    }

    #[test]
    fn links_are_drawn_as_terminal_hyperlinks() {
        let mut v = new_view("go [ab](https://e.com) [[none]] now\n");
        let mut terminal = Terminal::new(TestBackend::new(30, 2)).unwrap();
        v.resize(30, 2);
        let href = |link: &Link| (!link.wiki).then(|| link.dest.clone());
        terminal.draw(|f| v.draw_in(f, f.area(), href)).unwrap();
        let buf = terminal.backend().buffer();
        let open = "\x1b]8;id=smd0;https://e.com\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        assert_eq!(buf[(3, 0)].symbol(), format!("{open}a{close}"));
        assert_eq!(buf[(4, 0)].symbol(), format!("{open}b{close}"));
        assert_eq!(buf[(2, 0)].symbol(), " ");
        // A link with no address is left as text.
        assert_eq!(buf[(6, 0)].symbol(), "n");
        // Each cell still takes one column.
        assert_eq!(buf[(5, 0)].symbol(), " ");
        assert_eq!(buf[(11, 0)].symbol(), "n");
    }

    #[test]
    fn clicking_a_link_follows_it_and_the_wheel_scrolls() {
        let home = format!("go [[Plan]] now\n\n{}", paragraphs(6));
        let (mut b, dir) = browser("click", &[("HOME.md", &home), ("Plan.md", "# Plan\n")]);
        let at = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let click = |column, row| at(MouseEventKind::Down(MouseButton::Left), column, row);
        // Next to the link, nothing happens. The note starts on the row
        // below the toolbar.
        b.mouse(click(1, 1));
        assert_eq!(b.path, dir.join("HOME.md"));
        b.mouse(at(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(b.view.scroll, 3);
        b.mouse(at(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(b.view.scroll, 0);
        b.mouse(click(4, 1));
        assert_eq!(b.path, dir.join("Plan.md"));
        // The toolbar's Back and Forward buttons.
        b.mouse(click(2, 0));
        assert_eq!(b.path, dir.join("HOME.md"));
        assert_eq!(b.view.selected, Some(0));
        b.mouse(click(BACK_BUTTON.width() as u16 + 2, 0));
        assert_eq!(b.path, dir.join("Plan.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forward_goes_back_to_where_back_left() {
        let home = "[[A]] [[B]]\n";
        let (mut b, dir) = browser(
            "forward",
            &[("HOME.md", home), ("A.md", "[[B]]\n"), ("B.md", "b\n")],
        );
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, dir.join("A.md"));
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("HOME.md"));
        enter(&mut b, KeyCode::Char('L'));
        assert_eq!(b.path, dir.join("A.md"));
        // Nothing further ahead.
        enter(&mut b, KeyCode::Char('L'));
        assert_eq!(b.path, dir.join("A.md"));
        // Following a new link forgets the way forward. Back leaves the
        // first link selected, so one Tab gets to the second.
        enter(&mut b, KeyCode::Char('H'));
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, dir.join("B.md"));
        assert!(b.ahead.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_toolbar_shows_the_buttons_and_the_note_name() {
        let (mut b, dir) = browser(
            "toolbar",
            &[("HOME.md", "[[work/Plan]]\n"), ("work/Plan.md", "p\n")],
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        let row = |t: &Terminal<TestBackend>, y| -> String {
            let buf = t.backend().buffer();
            (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect()
        };
        terminal.draw(|f| b.draw(f)).unwrap();
        assert_eq!(row(&terminal, 0).trim_end(), " ← Back  Forward →  HOME");
        // The bar reaches the right edge.
        assert_eq!(
            terminal.backend().buffer()[(39, 0)].bg,
            terminal.backend().buffer()[(0, 0)].bg
        );
        assert_eq!(
            terminal.backend().buffer()[(39, 0)].modifier,
            terminal.backend().buffer()[(0, 0)].modifier - Modifier::DIM
        );
        assert!(
            terminal.backend().buffer()[(2, 0)]
                .modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(row(&terminal, 1).trim_end(), "work/Plan");
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        terminal.draw(|f| b.draw(f)).unwrap();
        assert_eq!(
            row(&terminal, 0).trim_end(),
            " ← Back  Forward →  work/Plan"
        );
        assert!(
            terminal.backend().buffer()[(2, 0)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_urls_escape_what_a_url_cannot_hold() {
        assert_eq!(
            file_url(Path::new("/n/my note (1).md")),
            "file:///n/my%20note%20%281%29.md"
        );
    }

    #[test]
    fn notes_and_web_links_have_addresses_but_headings_do_not() {
        let (b, dir) = browser(
            "href",
            &[
                ("HOME.md", "[[Plan]] [[#Top]] [[gone]]\n"),
                ("work/Plan.md", "x\n"),
            ],
        );
        let addr = |dest: &str, wiki| {
            let link = Link {
                dest: dest.into(),
                wiki,
            };
            href(&b.vault, &link, &b.path)
        };
        assert_eq!(
            addr("Plan", true),
            Some(file_url(&dir.join("work/Plan.md")))
        );
        assert_eq!(addr("https://e.com", false), Some("https://e.com".into()));
        assert_eq!(addr("#Top", true), None);
        assert_eq!(addr("gone", true), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_wrapped_over_two_lines_covers_both() {
        let mut v = new_view("one two [[three four five]]\n");
        v.resize(14, 5);
        assert_eq!(v.layout.links[0].cells, vec![(0, 8..13), (1, 0..9)]);
    }

    #[test]
    fn jump_to_a_heading() {
        let md = format!("# Top\n\n{0}## Next steps\n\n{0}", paragraphs(10));
        let mut v = new_view(&md);
        v.resize(20, 5);
        assert!(v.jump_to_heading("next-steps"));
        assert_eq!(top(&v), "Next steps");
        assert!(!v.jump_to_heading("nowhere"));
    }

    /// A browser on `files` in a fresh vault, showing the first.
    fn browser(name: &str, files: &[(&str, &str)]) -> (Browser, PathBuf) {
        let dir = std::env::temp_dir().join(format!("smd-browse-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".obsidian")).unwrap();
        for (file, text) in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let path = dir.join(files[0].0);
        let vault = Vault::for_file(&path);
        let doc = super::parse(&vault, files[0].1, &path);
        let mut view = View::new(doc, folder(&path), Images::new((8, 16)));
        view.resize(30, 6);
        let watcher = Watcher::new(path.clone());
        (
            Browser::new(view, path, watcher, vault, Config::default()),
            dir,
        )
    }

    fn enter(b: &mut Browser, code: KeyCode) {
        b.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn following_links_and_coming_back() {
        let home = format!("{}[[Plan#Later]] [[gone]]\n", paragraphs(6));
        let plan = format!("# Plan\n\n{0}## Later\n\n{0}", paragraphs(8));
        let (mut b, dir) = browser("follow", &[("HOME.md", &home), ("work/Plan.md", &plan)]);
        b.view.act(Action::Bottom);
        let scroll = b.view.scroll;
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, dir.join("work/Plan.md"));
        assert_eq!(top(&b.view), "Later");
        // Back to where the link was, with it still selected.
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("HOME.md"));
        assert_eq!(b.view.scroll, scroll);
        assert_eq!(b.view.selected, Some(0));
        // A link to nothing says so until the next key.
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.view.notice.as_deref(), Some("no note found: gone"));
        assert_eq!(b.path, dir.join("HOME.md"));
        enter(&mut b, KeyCode::Char('j'));
        assert_eq!(b.view.notice, None);
        // Nothing to go back to.
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("HOME.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A browser as `browser` makes, with the syntaxes loaded first, so
    /// they cannot arrive part way through a test and cause a redraw.
    fn loaded(name: &str, files: &[(&str, &str)]) -> (Browser, PathBuf) {
        highlight::syntax_set();
        browser(name, files)
    }

    /// One step of a scripted run: an event that arrives a while into a
    /// poll, a poll that times out with nothing, or polls that time out
    /// until something new is drawn, for work on other threads.
    enum Step {
        After(Duration, Event),
        Wait,
        Drawn,
    }

    /// Input from a script, with a clock that moves only when it says.
    struct Script {
        now: Instant,
        steps: std::collections::VecDeque<Step>,
        /// How long each poll was asked to wait.
        timeouts: Vec<Duration>,
        /// What the terminal was sent.
        out: Shared,
        /// How much had been sent when the `Drawn` in front was reached, and
        /// when.
        sent: Option<(usize, Instant)>,
    }

    impl Input for Script {
        fn now(&self) -> Instant {
            self.now
        }

        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            self.timeouts.push(timeout);
            match self.steps.front() {
                Some(Step::After(after, _)) => {
                    assert!(*after <= timeout, "{after:?} is past the {timeout:?} poll");
                    self.now += *after;
                    Ok(true)
                }
                Some(Step::Wait) => {
                    self.steps.pop_front();
                    self.now += timeout;
                    Ok(false)
                }
                Some(Step::Drawn) => {
                    let len = self.out.0.borrow().len();
                    match self.sent {
                        Some((sent, _)) if len > sent => {
                            self.steps.pop_front();
                            self.sent = None;
                        }
                        Some((_, since)) => {
                            assert!(
                                since.elapsed() < Duration::from_secs(5),
                                "nothing new was drawn"
                            );
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        None => self.sent = Some((len, Instant::now())),
                    }
                    self.now += timeout;
                    Ok(false)
                }
                None => Err(io::Error::other("the script ran out")),
            }
        }

        fn read(&mut self) -> io::Result<Event> {
            match self.steps.pop_front() {
                Some(Step::After(_, event)) => Ok(event),
                _ => panic!("read with no event polled"),
            }
        }
    }

    fn pressed(code: KeyCode) -> Step {
        Step::After(
            Duration::ZERO,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        )
    }

    fn quit() -> Step {
        pressed(KeyCode::Char('q'))
    }

    /// The result of running `b` on `steps` in a 30x7 terminal, how many
    /// frames were drawn, and how long each poll waited.
    fn run_script(b: &mut Browser, steps: Vec<Step>) -> (io::Result<()>, usize, Vec<Duration>) {
        let out = Shared::default();
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 30, 7)),
        };
        let mut terminal =
            Terminal::with_options(CrosstermBackend::new(out.clone()), options).unwrap();
        let mut script = Script {
            now: Instant::now(),
            steps: steps.into(),
            timeouts: Vec::new(),
            out,
            sent: None,
        };
        let result = run_browser(&mut terminal, b, &mut script);
        (result, terminal.get_frame().count(), script.timeouts)
    }

    #[test]
    fn the_loop_draws_once_and_quits_on_a_quit_key() {
        let (mut b, dir) = loaded("loop-quit", &[("a.md", "one\n")]);
        let (result, frames, timeouts) = run_script(&mut b, vec![quit()]);
        assert!(result.is_ok());
        assert_eq!(frames, 1);
        assert_eq!(timeouts, [CHECK_EVERY]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loop_redraws_after_events_but_not_while_nothing_changes() {
        let (mut b, dir) = loaded("loop-idle", &[("a.md", "one\n")]);
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        let steps = vec![
            Step::Wait,
            Step::Wait,
            // Only a press quits; a release is one more event to draw after.
            Step::After(Duration::ZERO, Event::Key(release)),
            // A key with no action, part way into the wait for a check.
            Step::After(
                Duration::from_millis(100),
                Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)),
            ),
            Step::Wait,
            Step::After(Duration::ZERO, Event::Resize(30, 7)),
            quit(),
        ];
        let (result, frames, timeouts) = run_script(&mut b, steps);
        assert!(result.is_ok());
        assert_eq!(frames, 4);
        let ms = Duration::from_millis;
        // The poll after the key waits only for the rest of the check's time.
        assert_eq!(
            timeouts,
            [
                CHECK_EVERY,
                CHECK_EVERY,
                CHECK_EVERY,
                CHECK_EVERY,
                ms(50),
                CHECK_EVERY,
                CHECK_EVERY
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loop_ends_with_an_error_from_the_input() {
        let (mut b, dir) = loaded("loop-error", &[("a.md", "one\n")]);
        let (result, frames, _) = run_script(&mut b, vec![Step::Wait]);
        assert_eq!(result.unwrap_err().to_string(), "the script ran out");
        assert_eq!(frames, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loop_scrolls_with_the_wheel() {
        let (mut b, dir) = loaded("loop-mouse", &[("a.md", &paragraphs(10))]);
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let steps = vec![Step::After(Duration::ZERO, Event::Mouse(wheel)), quit()];
        let (result, frames, _) = run_script(&mut b, steps);
        assert!(result.is_ok());
        assert_eq!(frames, 2);
        assert_eq!(b.view.scroll, WHEEL_LINES as usize);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loop_reloads_the_note_when_its_file_changes() {
        let (mut b, dir) = loaded("loop-change", &[("a.md", "one\n")]);
        std::fs::write(dir.join("a.md"), "something else\n").unwrap();
        // The change shows once two checks agree on it.
        let steps = vec![Step::Wait, Step::Wait, Step::Wait, quit()];
        let (result, frames, _) = run_script(&mut b, steps);
        assert!(result.is_ok());
        assert_eq!(frames, 2);
        assert_eq!(top(&b.view), "something else");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loop_redraws_when_an_image_arrives() {
        use crate::images::tests::png;
        let (mut b, dir) = loaded("loop-image", &[("a.md", "one\n")]);
        std::fs::write(dir.join("a.png"), png(20, 100)).unwrap();
        b.view.show(parse("![a](a.png)\n"), dir.clone());
        let lines = b.view.layout.lines.len();
        let (result, frames, _) = run_script(&mut b, vec![Step::Drawn, quit()]);
        assert!(result.is_ok());
        assert_eq!(frames, 2);
        assert!(b.view.layout.lines.len() > lines, "the image took no rows");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loop_redraws_when_the_syntaxes_arrive() {
        let (mut b, dir) = loaded("loop-syntaxes", &[("a.md", "```rust\nfn main() {}\n```\n")]);
        highlight::syntax_set();
        // As if the note were laid out before the syntaxes had loaded.
        b.view.highlighted = false;
        let (result, frames, _) = run_script(&mut b, vec![Step::Wait, quit()]);
        assert!(result.is_ok());
        assert_eq!(frames, 2);
        assert!(b.view.highlighted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn syntaxes_lay_out_again_only_once_and_only_after_a_layout() {
        highlight::syntax_set();
        let mut v = View::new(parse("one\n"), ".".into(), Images::text());
        // Not laid out yet: there is nothing to redo.
        v.highlighted = false;
        assert!(!v.receive_syntaxes());
        v.resize(20, 5);
        assert!(v.highlighted);
        assert!(!v.receive_syntaxes());
        v.highlighted = false;
        assert!(v.receive_syntaxes());
        assert!(v.highlighted);
    }

    /// A writer whose bytes the test can read after handing it over.
    #[derive(Clone, Default)]
    struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer that always fails.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("no terminal"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn handing_on(name: &str, web_links: WebLinks) -> (Browser, PathBuf) {
        let (mut b, dir) = browser(name, &[("a.md", "one\n")]);
        b.config.web_links = web_links;
        b.opener = |target| panic!("opened {target}");
        b.clipboard = Box::new(Broken);
        (b, dir)
    }

    #[test]
    fn handing_on_opens_with_the_opener() {
        let (mut b, dir) = handing_on("hand-open", WebLinks::Open);
        b.opener = |target| {
            assert_eq!(target, "https://example.com/");
            Ok(())
        };
        b.hand_on("https://example.com/");
        assert_eq!(b.view.notice, None);
        b.opener = |target| Err(io::Error::other(format!("nothing opens {target}")));
        b.hand_on("https://example.com/");
        assert_eq!(
            b.view.notice.as_deref(),
            Some("cannot open https://example.com/: nothing opens https://example.com/")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handing_on_copies_through_the_terminal() {
        let (mut b, dir) = handing_on("hand-copy", WebLinks::Copy);
        let out = Shared::default();
        b.clipboard = Box::new(out.clone());
        b.hand_on("https://example.com/");
        assert_eq!(
            String::from_utf8(out.0.borrow().clone()).unwrap(),
            "\x1b]52;c;aHR0cHM6Ly9leGFtcGxlLmNvbS8=\x1b\\"
        );
        assert_eq!(
            b.view.notice.as_deref(),
            Some("copied https://example.com/")
        );
        b.clipboard = Box::new(Broken);
        b.hand_on("https://example.com/");
        assert_eq!(b.view.notice.as_deref(), Some("cannot copy: no terminal"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handing_on_can_do_nothing() {
        let (mut b, dir) = handing_on("hand-ignore", WebLinks::Ignore);
        b.hand_on("https://example.com/");
        assert_eq!(b.view.notice, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A browser that read `HOME.md`, with an image above a link to
    /// `Other.md`, waited for the image, scrolled below it and followed the
    /// link. Also the scroll it left `HOME.md` at.
    fn gone_past_an_image(name: &str) -> (Browser, PathBuf, usize) {
        use crate::images::tests::png;
        let home = format!("![a](a.png)\n\n{0}[[Other]]\n\n{0}", paragraphs(6));
        let (mut b, dir) = loaded(name, &[("HOME.md", "one\n"), ("Other.md", "other\n")]);
        std::fs::write(dir.join("a.png"), png(20, 100)).unwrap();
        std::fs::write(dir.join("HOME.md"), &home).unwrap();
        assert!(b.open(dir.join("HOME.md")));
        let (result, _, _) = run_script(&mut b, vec![Step::Drawn, quit()]);
        assert!(result.is_ok());
        // Below the image, with the link in view.
        b.view.scroll_by(16);
        let scroll = b.view.scroll;
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, dir.join("Other.md"));
        (b, dir, scroll)
    }

    #[test]
    fn back_lands_where_the_reader_was_below_an_image() {
        let (mut b, dir, scroll) = gone_past_an_image("back-scroll");
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("HOME.md"));
        // Waits for the image, in case it has to be made again: the scroll
        // must not move when it arrives.
        let key = images::key(&b.view.doc.blocks[0]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while b.view.images.ready(&key).is_none() {
            assert!(Instant::now() < deadline, "image not ready");
            std::thread::sleep(Duration::from_millis(1));
            b.view.receive_images();
        }
        assert_eq!(b.view.scroll, scroll);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn back_shows_the_images_at_once() {
        let (mut b, dir, _) = gone_past_an_image("back-images");
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("HOME.md"));
        assert_eq!(b.view.layout.placements().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_in_different_folders_show_their_own_image_of_the_same_name() {
        use crate::images::tests::png;
        let (mut b, dir) = loaded(
            "same-src",
            &[
                ("a/Note.md", "![x](img.png)\n\n[[Other]]\n"),
                ("b/Other.md", "![x](img.png)\n"),
            ],
        );
        std::fs::write(dir.join("a/img.png"), png(20, 100)).unwrap();
        std::fs::write(dir.join("b/img.png"), png(40, 40)).unwrap();
        let key = images::key(&b.view.doc.blocks[0]).unwrap();
        let wait = |b: &mut Browser| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while b.view.images.ready(&key).is_none() {
                assert!(Instant::now() < deadline, "image not ready");
                std::thread::sleep(Duration::from_millis(1));
                b.view.receive_images();
            }
            b.view.images.ready(&key).unwrap().px
        };
        // Read again now the image is there, once the first try is done.
        let deadline = Instant::now() + Duration::from_secs(5);
        while b.view.images.error(&key).is_none() && b.view.images.ready(&key).is_none() {
            assert!(Instant::now() < deadline, "first try not done");
            std::thread::sleep(Duration::from_millis(1));
            b.view.receive_images();
        }
        assert!(b.open(dir.join("a/Note.md")));
        assert_eq!(wait(&mut b), (20, 100));
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, dir.join("b/Other.md"));
        assert_eq!(wait(&mut b), (40, 40));
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(wait(&mut b), (20, 100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wiki(dest: &str) -> Link {
        Link {
            dest: dest.into(),
            wiki: true,
        }
    }

    #[test]
    fn following_each_kind_of_link() {
        let home = format!("# Top\n\n{0}## Later\n\n{0}", paragraphs(8));
        let (mut b, dir) = browser(
            "kinds",
            &[
                ("HOME.md", &home),
                ("Plan.md", "# Plan\n"),
                ("doc.pdf", "%PDF"),
            ],
        );
        // A heading in this note, by its name and on its own.
        b.follow(&wiki("HOME#Later"));
        assert_eq!(top(&b.view), "Later");
        b.view.act(Action::Top);
        b.follow(&wiki("#Later"));
        assert_eq!(top(&b.view), "Later");
        // This note with no heading goes nowhere.
        b.follow(&wiki("HOME"));
        assert_eq!(top(&b.view), "Later");
        assert!(b.history.is_empty());
        b.follow(&wiki("#nowhere"));
        assert_eq!(b.view.notice.as_deref(), Some("no heading found: nowhere"));
        // Web links and files smd does not show go to the opener.
        b.opener = |target| Err(io::Error::other(format!("asked for {target}")));
        b.follow(&Link::from("https://example.com/"));
        let notice = b.view.notice.clone().unwrap();
        assert!(
            notice.ends_with("asked for https://example.com/"),
            "{notice}"
        );
        b.follow(&Link::from("doc.pdf"));
        let notice = b.view.notice.clone().unwrap();
        let pdf = dir.join("doc.pdf");
        assert!(
            notice.ends_with(&format!("asked for {}", pdf.display())),
            "{notice}"
        );
        // Another note opens even when its heading is not there.
        b.follow(&wiki("Plan#Missing"));
        assert_eq!(b.path, dir.join("Plan.md"));
        assert_eq!(b.view.notice.as_deref(), Some("no heading found: Missing"));
        assert_eq!(b.history.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_link_drops_forward_unless_its_note_cannot_be_read() {
        let (mut b, dir) = browser(
            "drop-forward",
            &[("A.md", "a\n"), ("B.md", "b\n"), ("C.md", "c\n")],
        );
        b.follow(&wiki("B"));
        b.back();
        assert_eq!(b.ahead.len(), 1);
        // A note that cannot be read leaves the way forward as it was.
        std::fs::write(dir.join("C.md"), b"\xff\xfe").unwrap();
        b.follow(&wiki("C"));
        assert_eq!(b.path, dir.join("A.md"));
        assert!(
            b.view
                .notice
                .as_deref()
                .unwrap()
                .starts_with("cannot read file")
        );
        assert_eq!(b.ahead.len(), 1);
        assert!(b.history.is_empty());
        // One that opens drops it.
        std::fs::write(dir.join("C.md"), "c\n").unwrap();
        b.follow(&wiki("C"));
        assert_eq!(b.path, dir.join("C.md"));
        assert!(b.ahead.is_empty());
        b.forward();
        assert_eq!(b.path, dir.join("C.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
