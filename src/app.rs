//! The full-screen view: a scrollable window onto the laid-out document, and
//! the browser that moves it from note to note along links.

use std::collections::HashSet;
use std::io::{self, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::{Cell, CellDiffOption};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame, Terminal};

use crate::config::{Action, Config, Tasks, WebLinks};
use crate::doc::{self, BlockKind, Document, Inline, Link};
use crate::embed;
use crate::highlight;
use crate::images::{self, Images};
use crate::kitty;
use crate::render::{Layout, render_folds};
use crate::search::Search;
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
    /// What is being typed on the bottom row, such as a search term. It
    /// takes the row over `notice`.
    prompt: Option<String>,
    /// The term searched for and where it is drawn.
    search: Search,
    /// The selected link or checkbox.
    selected: Option<Stop>,
    /// Whether checkboxes can be ticked: the config allows it and the file
    /// can be written. When not, Tab, Enter and clicks pass over them.
    tasks: bool,
    /// Other notes, with their image directories, whose images are kept so
    /// they show at once when the reader goes back to them.
    kept: Vec<(Rc<Document>, PathBuf)>,
    /// The starts of the blocks the reader folded the other way from how
    /// they start.
    flipped: HashSet<usize>,
}

/// Something Tab stops at: a link, a task's checkbox or a folding title, by
/// its index in the layout's links, tasks or folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Link(usize),
    Task(usize),
    Fold(usize),
}

/// What an action asks of the browser around the view.
#[derive(Debug, Clone, PartialEq)]
pub enum Go {
    Quit,
    Follow(Link),
    /// Tick or untick the task whose mark is at this byte of the source.
    Toggle {
        mark: usize,
        done: bool,
    },
    Back,
    Forward,
    /// Fold the block that starts at this byte of the source open or shut.
    Fold(usize),
    /// Show a short message on the bottom row until the next key.
    Say(String),
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
        let start = moved(self.start, old, new)?;
        Some(Anchor { start, ..self })
    }
}

/// Byte `at` of `old` in `new`, an edited copy of it: a byte before the
/// edited bytes stays and one after them moves with them; one inside them
/// is lost.
fn moved(at: usize, old: &str, new: &str) -> Option<usize> {
    let (old, new) = (old.as_bytes(), new.as_bytes());
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev());
    let suffix = suffix.take(room).take_while(|(a, b)| a == b).count();
    if at < prefix {
        Some(at)
    } else if at >= old.len() - suffix {
        Some(at + new.len() - old.len())
    } else {
        None
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
            prompt: None,
            search: Search::default(),
            selected: None,
            tasks: false,
            kept: Vec::new(),
            flipped: HashSet::new(),
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
        self.search.current = None;
        self.flipped.clear();
        self.want_images();
        self.relayout(None);
    }

    /// Scroll to the anchor named `wanted`, such as a footnote, or else the
    /// first heading matching it, opening any shut fold around it. Returns
    /// false when there is neither.
    pub fn jump_to_heading(&mut self, wanted: &str) -> bool {
        let anchor = self.doc.anchor(wanted);
        let Some(at) = anchor.or_else(|| heading_start(&self.doc.source, &self.doc.blocks, wanted))
        else {
            return false;
        };
        self.open_folds_around(at);
        let blocks = &self.layout.blocks;
        let block = match anchor {
            // The innermost block the anchor is in.
            Some(_) => blocks.iter().rfind(|b| b.span.contains(&at)),
            None => blocks.iter().find(|b| b.span.start == at),
        };
        if let Some(b) = block {
            self.scroll = b.rows.start.min(self.max_scroll());
        }
        true
    }

    /// Show a new version of the document, keeping the block at the top of the
    /// view in place, or the same line number when that block is gone. Images
    /// it no longer has are deleted on the next flush.
    pub fn reload(&mut self, doc: Document) {
        // Such as the watcher seeing a checkbox this view already ticked.
        if doc.source == self.doc.source {
            return;
        }
        let anchor = self.anchor();
        let old = std::mem::replace(&mut self.doc, Rc::new(doc));
        let anchor = anchor.and_then(|a| a.moved(&old.source, &self.doc.source));
        let flipped = std::mem::take(&mut self.flipped);
        let flipped = flipped.into_iter();
        self.flipped = flipped
            .filter_map(|at| moved(at, &old.source, &self.doc.source))
            .collect();
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
            self.layout = render_folds(&self.doc, self.width, &self.images, &self.flipped);
            if let Some(row) = anchor.and_then(|a| self.find(&a)) {
                self.scroll = row;
            }
            self.search.refresh(&self.layout, &self.doc);
        }
        self.scroll = self.scroll.min(self.max_scroll());
        self.selected = self.selected.filter(|&s| self.has(s));
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
            self.layout = render_folds(&self.doc, width, &self.images, &self.flipped);
            self.scroll = self.scroll * self.layout.lines.len() / old_len;
            self.width = width;
            self.selected = self.selected.filter(|&s| self.has(s));
            self.search.refresh(&self.layout, &self.doc);
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
        if let Some(notice) = self.prompt.as_ref().or(self.notice.as_ref())
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
        // The cells of a link or checkbox that are in view.
        let scroll = self.scroll;
        let cells = |spot: &[(usize, std::ops::Range<u16>)]| -> Vec<Rect> {
            spot.iter()
                .filter(|(row, cols)| (scroll..end).contains(row) && cols.start < area.width)
                .map(|(row, cols)| {
                    let y = area.y + (row - scroll) as u16;
                    let width = cols.end.min(area.width) - cols.start;
                    Rect::new(area.x + cols.start, y, width, 1)
                })
                .collect()
        };
        let buf = frame.buffer_mut();
        // Where the search term is drawn, the match the reader is on in
        // reverse.
        if self.search.active() {
            let marked = Style::new().fg(Color::Black).bg(Color::Yellow);
            for (i, m) in self.search.matches.iter().enumerate() {
                let Some(cols) = &m.cols else {
                    continue;
                };
                let style = if self.search.current == Some(i) {
                    marked.add_modifier(Modifier::REVERSED)
                } else {
                    marked
                };
                for rect in cells(&[(m.row, cols.clone())]) {
                    buf.set_style(rect, style);
                }
            }
        }
        // The selected link or checkbox is shown in reverse.
        if let Some(stop) = self.selected {
            for rect in cells(&self.cells(stop)) {
                buf.set_style(rect, Style::new().add_modifier(Modifier::REVERSED));
            }
        }
        for (i, spot) in self.layout.links.iter().enumerate() {
            let rects = cells(&spot.cells);
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

    /// The link or checkbox drawn at column `x` of row `y` of the view, if any.
    fn stop_at(&self, x: u16, y: u16) -> Option<Stop> {
        if y >= self.height {
            return None;
        }
        let line = self.scroll + y as usize;
        let hit = |stop: &Stop| {
            self.cells(*stop)
                .iter()
                .any(|(row, cols)| *row == line && cols.contains(&x))
        };
        // A link in a folding title is under the title's stop too.
        let stops = self.stops().into_iter().filter(hit);
        stops.min_by_key(|stop| matches!(stop, Stop::Fold(_)))
    }

    /// The links, and the checkboxes when they can be ticked, in the order
    /// they are drawn.
    fn stops(&self) -> Vec<Stop> {
        let links = (0..self.layout.links.len()).map(Stop::Link);
        let tasks = (0..self.layout.tasks.len()).map(Stop::Task);
        let tasks = tasks.filter(|_| self.tasks);
        let folds = (0..self.layout.folds.len()).map(Stop::Fold);
        let mut stops: Vec<Stop> = links.chain(tasks).chain(folds).collect();
        stops.sort_by_key(|&s| self.cells(s).first().map(|(row, cols)| (*row, cols.start)));
        stops
    }

    /// Whether `stop` is in the layout and can be selected.
    fn has(&self, stop: Stop) -> bool {
        match stop {
            Stop::Link(i) => i < self.layout.links.len(),
            Stop::Task(i) => self.tasks && i < self.layout.tasks.len(),
            Stop::Fold(i) => i < self.layout.folds.len(),
        }
    }

    /// The cells `stop` is drawn on, line by line.
    fn cells(&self, stop: Stop) -> Vec<(usize, std::ops::Range<u16>)> {
        match stop {
            Stop::Link(i) => self.layout.links[i].cells.clone(),
            Stop::Task(i) => vec![self.layout.tasks[i].cell.clone()],
            Stop::Fold(i) => vec![self.layout.folds[i].cell.clone()],
        }
    }

    /// What choosing `stop` does: follow the link, tick the checkbox, or
    /// fold the block.
    fn choose(&self, stop: Stop) -> Go {
        match stop {
            Stop::Fold(i) => Go::Fold(self.layout.folds[i].at),
            Stop::Link(i) => Go::Follow(self.layout.links[i].link.clone()),
            Stop::Task(i) => {
                let task = &self.layout.tasks[i];
                Go::Toggle {
                    mark: task.mark,
                    done: task.done,
                }
            }
        }
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
            Action::Follow => return Some(self.choose(self.selected?)),
            // Only a checkbox: a selected link stays where it is.
            Action::Toggle => match self.selected? {
                stop @ Stop::Task(_) => return Some(self.choose(stop)),
                Stop::Link(_) | Stop::Fold(_) => {}
            },
            Action::Back => return Some(Go::Back),
            Action::Forward => return Some(Go::Forward),
            Action::Search => self.start_search(),
            Action::NextMatch => return Some(self.next_match(true)),
            Action::PrevMatch => return Some(self.next_match(false)),
        }
        None
    }

    /// Start typing a search term on the bottom row.
    fn start_search(&mut self) {
        self.search = Search {
            typing: true,
            ..Search::default()
        };
        self.prompt = Some(self.search.prompt());
    }

    /// Take a key typed into the search prompt: a character adds to the
    /// term, Backspace takes one off, Enter goes to the first match from
    /// the top of the view and Esc gives the search up.
    fn search_key(&mut self, key: event::KeyEvent) -> Option<Go> {
        match key.code {
            KeyCode::Char(c) if (key.modifiers - KeyModifiers::SHIFT).is_empty() => {
                self.search.term.push(c);
            }
            KeyCode::Backspace => {
                self.search.term.pop();
            }
            KeyCode::Enter => {
                self.search.typing = false;
                self.prompt = None;
                if !self.search.active() {
                    return None;
                }
                let found = self.search.first_from(self.scroll);
                let found = found.map(|(i, round)| (i, round.then_some(true)));
                return Some(self.go_to_match(found));
            }
            KeyCode::Esc => {
                self.clear_search();
                return None;
            }
            _ => return None,
        }
        self.search.refresh(&self.layout, &self.doc);
        self.prompt = Some(self.search.prompt());
        None
    }

    /// Give up the search and stop marking its matches.
    fn clear_search(&mut self) {
        self.search = Search::default();
        self.prompt = None;
    }

    /// Go to the next or previous match of the search.
    fn next_match(&mut self, forward: bool) -> Go {
        if !self.search.active() {
            return Go::Say("no search: press / to search".into());
        }
        let found = self.search.step(forward, self.in_view());
        self.go_to_match(found.map(|(i, round)| (i, round.then_some(forward))))
    }

    /// Make `found` the current match and bring it into view, a little way
    /// down the page when that moves it. Says which match it is, or that
    /// there are none.
    fn go_to_match(&mut self, found: Option<(usize, Option<bool>)>) -> Go {
        let wrapped = found.and_then(|(_, w)| w);
        if let Some((i, _)) = found {
            self.search.current = Some(i);
            let row = self.search.matches[i].row;
            if !self.in_view().contains(&row) {
                self.scroll = row
                    .saturating_sub(self.height as usize / 3)
                    .min(self.max_scroll());
            }
        }
        Go::Say(self.search.status(wrapped))
    }

    /// Open each shut block that holds the byte `at`, outermost first, so
    /// what starts there is drawn.
    fn open_folds_around(&mut self, at: usize) {
        loop {
            let layout = &self.layout;
            let shut = layout.folds.iter().find(|f| {
                !f.open
                    && layout
                        .blocks
                        .iter()
                        .any(|b| b.span.start == f.at && b.span.contains(&at))
            });
            let Some(fold) = shut else {
                return;
            };
            if !self.flipped.remove(&fold.at) {
                self.flipped.insert(fold.at);
            }
            self.relayout(None);
        }
    }

    /// Fold the block starting at `at` open or shut, keeping its title on
    /// the same row of the screen and selected if it was.
    fn fold(&mut self, at: usize) {
        let row = |layout: &Layout| {
            let spot = layout.folds.iter().position(|f| f.at == at)?;
            Some((spot, layout.folds[spot].cell.0))
        };
        let before = row(&self.layout);
        let was_selected = before.is_some_and(|(i, _)| self.selected == Some(Stop::Fold(i)));
        if !self.flipped.remove(&at) {
            self.flipped.insert(at);
        }
        self.relayout(None);
        if let (Some((_, old)), Some((i, new))) = (before, row(&self.layout)) {
            let screen = old.saturating_sub(self.scroll);
            self.scroll = new.saturating_sub(screen).min(self.max_scroll());
            if was_selected {
                self.selected = Some(Stop::Fold(i));
            }
        }
    }

    /// The rows in view.
    fn in_view(&self) -> std::ops::Range<usize> {
        self.scroll..self.scroll + self.height as usize
    }

    /// Select the next or previous link or checkbox, going round at the
    /// ends. With none selected in view, start from the first or last one in
    /// view.
    fn select_link(&mut self, forward: bool) {
        let links = self.stops();
        let rows: Vec<usize> = links
            .iter()
            .map(|&s| self.cells(s).first().map_or(0, |c| c.0))
            .collect();
        let row = |i: usize| rows[i];
        let view = self.in_view();
        let at = self
            .selected
            .and_then(|s| links.iter().position(|&l| l == s));
        let selected = at.filter(|&i| view.contains(&row(i)));
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
        self.selected = Some(links[next]);
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
/// inside quotes and lists too. A heading matches by its text or, as one
/// with `:tada:` in it is shown with an emoji, by its source.
fn heading_start(source: &str, blocks: &[doc::Block], wanted: &str) -> Option<usize> {
    blocks.iter().find_map(|b| match &b.kind {
        BlockKind::Heading { content, .. } => {
            let text: String = content
                .iter()
                .map(|i| match i {
                    Inline::Text { text, .. } => text.as_str(),
                    Inline::Break => " ",
                })
                .collect();
            let typed = source.get(b.span.clone()).unwrap_or("");
            (vault::heading_matches(&text, wanted) || vault::heading_matches(typed, wanted))
                .then_some(b.span.start)
        }
        BlockKind::Quote(inner)
        | BlockKind::Callout { blocks: inner, .. }
        | BlockKind::Details { blocks: inner, .. } => heading_start(source, inner, wanted),
        BlockKind::List { items, .. } => items
            .iter()
            .find_map(|item| heading_start(source, &item.blocks, wanted)),
        BlockKind::Definition { definitions, .. } => definitions
            .iter()
            .find_map(|details| heading_start(source, details, wanted)),
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

/// Whether the file at `path` can be written. Opening it to write changes
/// nothing until something is written.
fn writable(path: &Path) -> bool {
    std::fs::OpenOptions::new().write(true).open(path).is_ok()
}

/// Write the task mark at byte `mark` of the file at `path` as ticked, or as
/// unticked when it was `done`, and return the file's new text. Only that
/// byte is written, in place, so the file keeps its other bytes, its
/// permissions and its links. The file is read again first, and must still
/// be `shown`, so a change made elsewhere since is not written over.
fn toggle_task(path: &Path, shown: &str, mark: usize, done: bool) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    let fail = |e: io::Error| format!("cannot tick the task: {e}: {}", path.display());
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(fail)?;
    let mut source = String::new();
    file.read_to_string(&mut source).map_err(fail)?;
    if source != shown {
        return Err("the file changed on disk; showing the new version first".into());
    }
    let was = source.as_bytes().get(mark).copied();
    let now = match (was, done) {
        (Some(b'x' | b'X'), true) => b' ',
        (Some(b' '), false) => b'x',
        _ => return Err("cannot tick the task: its checkbox is not where it was".into()),
    };
    file.seek(SeekFrom::Start(mark as u64)).map_err(fail)?;
    file.write_all(&[now]).map_err(fail)?;
    // One ASCII byte for another keeps the text valid UTF-8.
    let mut bytes = source.into_bytes();
    bytes[mark] = now;
    Ok(String::from_utf8(bytes).expect("an ASCII byte for an ASCII byte"))
}

/// `source`, the note at `path`, parsed with its embedded images found.
pub fn parse(vault: &Vault, source: &str, path: &Path) -> Document {
    // Without a reported background the theme is a guess, and the logo
    // left out could be the right one.
    let theme = theme::reported().then(theme::current);
    let mut doc = doc::parse_for(source, theme);
    vault.resolve_embeds(&mut doc.blocks, path);
    embed::embed_notes(vault, &mut doc, path, theme);
    doc
}

/// The folder a note's relative image paths are read from.
pub fn folder(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => ".".into(),
    }
}

/// Back's or Forward's list of places, from a browser.
type Stack = fn(&mut Browser) -> &mut Vec<Visited>;

/// A note left by following a link, to come back to.
struct Visited {
    path: PathBuf,
    doc: Rc<Document>,
    scroll: usize,
    selected: Option<Stop>,
    flipped: HashSet<usize>,
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
    /// When the last key of a terminal reply arriving as keys came, while
    /// inside one.
    replying: Option<Instant>,
    /// The note Back or Forward could not read last, unless a note has been
    /// opened since.
    missed: Option<PathBuf>,
    /// Where a copied link's OSC 52 sequence is written: the terminal.
    clipboard: Box<dyn Write>,
    /// What opens a link that is handed on.
    opener: fn(&str) -> io::Result<()>,
}

impl Browser {
    pub fn new(view: View, path: PathBuf, watcher: Watcher, vault: Vault, config: Config) -> Self {
        let mut browser = Browser {
            view,
            path,
            watcher,
            vault,
            config,
            history: Vec::new(),
            ahead: Vec::new(),
            passing: false,
            replying: None,
            missed: None,
            clipboard: Box::new(io::stdout()),
            opener: open_with_system,
        };
        browser.check_tasks();
        browser
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
                if let Some(stop) = self.view.stop_at(event.column, row) {
                    self.view.selected = Some(stop);
                    self.go(self.view.choose(stop));
                }
            }
            MouseEventKind::ScrollDown => self.view.scroll_by(WHEEL_LINES),
            MouseEventKind::ScrollUp => self.view.scroll_by(-WHEEL_LINES),
            _ => {}
        }
    }

    /// Handle a key press. Returns true when the reader should quit.
    fn key(&mut self, key: event::KeyEvent) -> bool {
        if self.reply(key) {
            return false;
        }
        if std::mem::take(&mut self.passing) {
            self.view.set_notice(None);
        }
        if self.view.search.typing {
            if let Some(go) = self.view.search_key(key) {
                self.go(go);
            }
            return false;
        }
        // Esc leaves a search before it quits.
        if key.code == KeyCode::Esc && key.modifiers.is_empty() && self.view.search.active() {
            self.view.clear_search();
            return false;
        }
        let Some(action) = self.config.action(key) else {
            return false;
        };
        match self.view.act(action) {
            Some(Go::Quit) => return true,
            Some(go) => self.go(go),
            None => {}
        }
        false
    }

    /// Do what the view asks, other than quit.
    fn go(&mut self, go: Go) {
        match go {
            Go::Quit => {}
            Go::Follow(link) => self.follow(&link),
            Go::Toggle { mark, done } => self.toggle(mark, done),
            Go::Back => self.back(),
            Go::Forward => self.forward(),
            Go::Fold(at) => self.view.fold(at),
            Go::Say(notice) => self.say(notice),
        }
    }

    /// Tick or untick a task in the file, and show the file as it now is,
    /// with the same checkbox selected.
    fn toggle(&mut self, mark: usize, done: bool) {
        match toggle_task(&self.path, &self.view.doc.source, mark, done) {
            Ok(source) => {
                let selected = self.view.selected;
                self.view.reload(parse(&self.vault, &source, &self.path));
                self.view.selected = selected.filter(|&s| self.view.has(s));
            }
            Err(err) => self.say(err),
        }
    }

    /// Let checkboxes be ticked when the config allows it and the note can
    /// be written.
    fn check_tasks(&mut self) {
        self.view.tasks = self.config.tasks == Tasks::Toggle && writable(&self.path);
        if let Some(Stop::Task(_)) = self.view.selected
            && !self.view.tasks
        {
            self.view.selected = None;
        }
    }

    /// Whether `key` is part of a terminal reply. A reply to a start-up
    /// query that comes after smd stopped waiting for it reaches crossterm
    /// as keys: an OSC or APC reply starts with Alt+] or Alt+_, has one key
    /// per character and ends with Alt+\ (ST) or Ctrl+G (BEL). A key that
    /// cannot be in a reply, such as Esc or an arrow, ends it and is handled
    /// as usual. A reply comes all at once, so so does a pause between keys:
    /// the key after one is the reader's.
    fn reply(&mut self, key: event::KeyEvent) -> bool {
        let now = Instant::now();
        let alt = key.modifiers == KeyModifiers::ALT;
        let inside = self
            .replying
            .take()
            .is_some_and(|last| now - last < REPLY_GAP);
        if !inside {
            let starts = alt && matches!(key.code, KeyCode::Char(']' | '_'));
            if starts && self.config.action(key).is_none() {
                self.replying = Some(now);
                return true;
            }
            return false;
        }
        match key.code {
            KeyCode::Char('\\') if alt => {}
            KeyCode::Char('g') if key.modifiers == KeyModifiers::CONTROL => {}
            KeyCode::Char(_) if (key.modifiers - KeyModifiers::SHIFT).is_empty() => {
                self.replying = Some(now);
            }
            _ => return false,
        }
        true
    }

    fn follow(&mut self, link: &Link) {
        match self.vault.follow(link, &self.path) {
            Target::Note { path, heading } if path == self.path => {
                if let Some(h) = heading {
                    self.jump_here(&h);
                }
            }
            Target::Note { path, heading } => {
                let here = self.here();
                // Forward goes nowhere after a new link, so its notes keep
                // no images.
                let ahead = std::mem::take(&mut self.ahead);
                if self.open(path) {
                    self.remember(here);
                    if let Some(h) = heading {
                        self.jump(&h);
                    }
                } else {
                    self.ahead = ahead;
                }
            }
            Target::Heading(h) => self.jump_here(&h),
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

    /// Jump to a heading in this note, as a link does: Back comes back to
    /// where the link was. A jump that goes nowhere leaves no Back step.
    fn jump_here(&mut self, heading: &str) {
        let here = self.here();
        self.jump(heading);
        if self.view.scroll != here.scroll {
            self.remember(here);
            self.ahead.clear();
        }
    }

    /// Add a place for Back to go to, dropping the oldest past
    /// [`HISTORY_LIMIT`]. Forward only holds places Back gave up, so it
    /// stays within the limit too.
    fn remember(&mut self, here: Visited) {
        if self.history.len() >= HISTORY_LIMIT {
            self.history.remove(0);
        }
        self.history.push(here);
    }

    /// Go back to where the last link was followed from.
    fn back(&mut self) {
        self.step(|b| &mut b.history, |b| &mut b.ahead);
    }

    /// Go forward again to the note Back left.
    fn forward(&mut self) {
        self.step(|b| &mut b.ahead, |b| &mut b.history);
    }

    /// Go to the last place on `from`, leaving this one on `to`. A note that
    /// cannot be read stays on `from`, as it may be back in a moment, such as
    /// after an editor's save; if it still cannot be read on the next try,
    /// with nothing opened in between, it is passed for the place before it.
    fn step(&mut self, from: Stack, to: Stack) {
        let Some(visited) = from(self).pop() else {
            return;
        };
        let here = self.here();
        if self.revisit(&visited) {
            to(self).push(here);
        } else if self.missed.as_ref() != Some(&visited.path) {
            self.missed = Some(visited.path.clone());
            from(self).push(visited);
        } else {
            self.step(from, to);
            if self.path != here.path {
                self.say(format!("passed {}: cannot read it", visited.path.display()));
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
            flipped: self.view.flipped.clone(),
        }
    }

    /// Open a note again where it was left. Returns false when it cannot
    /// be read.
    fn revisit(&mut self, visited: &Visited) -> bool {
        // A place in the note on screen, unchanged since, needs no reading.
        let showing = visited.path == self.path && Rc::ptr_eq(&visited.doc, &self.view.doc);
        if !showing && !self.open(visited.path.clone()) {
            return false;
        }
        // Folds are kept by where they start, so only in the same text.
        if self.view.doc.source == visited.doc.source {
            self.view.flipped = visited.flipped.clone();
        }
        self.view.scroll = visited.scroll;
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
        self.missed = None;
        self.watcher = watcher;
        self.path = path;
        self.check_tasks();
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

/// How many places Back can go to. Following a link past this drops the
/// oldest.
const HISTORY_LIMIT: usize = 100;

/// The longest pause between two keys of a terminal reply.
const REPLY_GAP: Duration = Duration::from_millis(50);

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
                // Its permissions may have changed too.
                browser.check_tasks();
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
        assert_eq!(v.selected, Some(Stop::Link(2)));
        assert!(v.in_view().contains(&v.layout.links[2].cells[0].0));
        // Round to the first, and back again.
        tab(&mut v);
        assert_eq!(v.selected, Some(Stop::Link(0)));
        v.act(Action::PrevLink);
        assert_eq!(v.selected, Some(Stop::Link(2)));
        // Scrolled away from the selection, Tab starts from the view.
        v.act(Action::Top);
        v.act(Action::Down);
        v.act(Action::Down);
        tab(&mut v);
        assert_eq!(v.selected, Some(Stop::Link(2)));
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
        assert_eq!(b.view.selected, Some(Stop::Link(0)));
        b.mouse(click(BACK_BUTTON.width() as u16 + 2, 0));
        assert_eq!(b.path, dir.join("Plan.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_stops_at_checkboxes_and_enter_ticks_them_in_the_file() {
        let note = "- [ ] buy [[Bread]]\n- [x] milk\n";
        let (mut b, dir) = browser("tick", &[("HOME.md", note), ("Bread.md", "b\n")]);
        let path = dir.join("HOME.md");
        // In the order they are drawn: the box, its link, the next box.
        let mut stops = Vec::new();
        for _ in 0..3 {
            enter(&mut b, KeyCode::Tab);
            stops.push(b.view.selected.unwrap());
        }
        assert_eq!(stops, [Stop::Task(0), Stop::Link(0), Stop::Task(1)]);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "- [ ] buy [[Bread]]\n- [ ] milk\n"
        );
        // Shown at once, still selected, so it can be ticked again, with x.
        assert_eq!(b.view.selected, Some(Stop::Task(1)));
        assert!(!b.view.layout.tasks[1].done);
        enter(&mut b, KeyCode::Char('x'));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), note);
        // The watcher seeing the write changes nothing on screen.
        b.view.reload(parse(note));
        assert_eq!(b.view.selected, Some(Stop::Task(1)));
        // x leaves a selected link alone.
        enter(&mut b, KeyCode::BackTab);
        enter(&mut b, KeyCode::Char('x'));
        assert_eq!(b.path, path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_or_a_click_on_a_callout_title_folds_it() {
        let note = "> [!note]- Shut\n> hidden [[Plan]]\n\nafter [[Plan]]\n";
        let (mut b, dir) = browser("fold", &[("HOME.md", note), ("Plan.md", "p\n")]);
        let rows = |b: &Browser| b.view.layout.lines.len();
        assert_eq!(rows(&b), 3);
        // The title is the first stop; the shut body's link is not drawn.
        enter(&mut b, KeyCode::Tab);
        assert_eq!(b.view.selected, Some(Stop::Fold(0)));
        assert_eq!(b.view.stops(), [Stop::Fold(0), Stop::Link(0)]);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(rows(&b), 4);
        assert_eq!(b.view.selected, Some(Stop::Fold(0)));
        assert_eq!(
            b.view.stops(),
            [Stop::Fold(0), Stop::Link(0), Stop::Link(1)]
        );
        assert_eq!(b.path, dir.join("HOME.md"));
        // A click on the title, below the toolbar, shuts it again.
        b.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(rows(&b), 3);
        // A new note starts with its callouts as they are written.
        enter(&mut b, KeyCode::Enter);
        assert_eq!(rows(&b), 4);
        b.view.show(parse(note), dir.clone());
        assert_eq!(rows(&b), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_on_a_details_summary_opens_it() {
        let note = "<details><summary>More</summary>\n\nsee [[Plan]]\n\n</details>\n";
        let (mut b, dir) = browser("details", &[("HOME.md", note), ("Plan.md", "p\n")]);
        let rows = |b: &Browser| b.view.layout.lines.len();
        assert_eq!(rows(&b), 1);
        enter(&mut b, KeyCode::Tab);
        assert_eq!(b.view.stops(), [Stop::Fold(0)]);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(rows(&b), 2);
        assert_eq!(b.view.stops(), [Stop::Fold(0), Stop::Link(0)]);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(rows(&b), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn back_comes_back_to_the_folds_as_they_were_left() {
        let note = "> [!note]- Shut\n> go [[Plan]]\n\nafter [[Plan]]\n";
        let (mut b, dir) = browser("fold-back", &[("HOME.md", note), ("Plan.md", "p\n")]);
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        enter(&mut b, KeyCode::Tab);
        assert_eq!(b.view.selected, Some(Stop::Link(0)));
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, dir.join("Plan.md"));
        b.back();
        assert_eq!(b.view.layout.lines.len(), 4);
        assert_eq!(b.view.selected, Some(Stop::Link(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_on_a_link_in_a_folding_title_follows_it() {
        let note = "> [!note]- See [[Plan]]\n> body\n";
        let (mut b, dir) = browser("fold-link", &[("HOME.md", note), ("Plan.md", "p\n")]);
        // "│ ▸ ✎ See Plan": Plan is at columns 10 to 13.
        b.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(b.path, dir.join("Plan.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_ticks_a_checkbox() {
        let (mut b, dir) = browser("tick-click", &[("HOME.md", "- [ ] one\n")]);
        let click = |column| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        // Not the text after the box. The note starts below the toolbar.
        b.mouse(click(7));
        assert_eq!(
            std::fs::read_to_string(dir.join("HOME.md")).unwrap(),
            "- [ ] one\n"
        );
        b.mouse(click(3));
        assert_eq!(
            std::fs::read_to_string(dir.join("HOME.md")).unwrap(),
            "- [x] one\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkboxes_are_passed_over_when_they_cannot_be_ticked() {
        let note = "- [ ] buy [[Bread]]\n";
        // A file smd cannot write.
        let (mut b, dir) = browser("tick-locked", &[("HOME.md", note), ("Bread.md", "b\n")]);
        let path = dir.join("HOME.md");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms.clone()).unwrap();
        b.check_tasks();
        enter(&mut b, KeyCode::Tab);
        assert_eq!(b.view.selected, Some(Stop::Link(0)));
        // A file it can write, with the config saying not to.
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        b.config.tasks = Tasks::ReadOnly;
        b.check_tasks();
        b.view.selected = None;
        enter(&mut b, KeyCode::Tab);
        assert_eq!(b.view.selected, Some(Stop::Link(0)));
        b.config.tasks = Tasks::Toggle;
        b.check_tasks();
        b.view.selected = None;
        enter(&mut b, KeyCode::Tab);
        assert_eq!(b.view.selected, Some(Stop::Task(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_task_is_not_ticked_over_a_change_made_elsewhere() {
        let (mut b, dir) = browser("tick-changed", &[("HOME.md", "- [ ] one\n")]);
        let path = dir.join("HOME.md");
        std::fs::write(&path, "- [ ] one\n- [ ] two\n").unwrap();
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "- [ ] one\n- [ ] two\n"
        );
        assert!(b.view.notice.as_ref().unwrap().contains("changed on disk"));
        // A mark that is no longer a checkbox is not written either.
        let got = toggle_task(&path, "- [ ] one\n- [ ] two\n", 0, false);
        assert!(got.unwrap_err().contains("not where it was"));
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
        // A heading shown with an emoji for its shortcode.
        let md = format!("# Top\n\n{0}## :tada: Release\n\n{0}", paragraphs(10));
        let mut v = new_view(&md);
        v.resize(20, 5);
        assert!(v.jump_to_heading("tada-release"));
        assert_eq!(top(&v), "🎉 Release");
    }

    #[test]
    fn jump_to_a_heading_in_a_shut_callout_opens_it() {
        let md = format!(
            "{0}> [!faq]- More\n> ## Details\n> text\n\n{0}",
            paragraphs(10)
        );
        let mut v = new_view(&md);
        v.resize(20, 5);
        assert!(v.jump_to_heading("details"));
        assert_eq!(top(&v), "│ Details");
    }

    #[test]
    fn jump_to_a_footnote_reference_in_a_shut_callout_opens_it() {
        let md = format!(
            "{0}> [!faq]- More\n> Said[^n].\n\n{0}[^n]: The note.\n",
            paragraphs(10)
        );
        let mut v = new_view(&md);
        v.resize(20, 5);
        let shown = |v: &View| {
            v.layout
                .lines
                .iter()
                .any(|l| l.to_string().contains("Said"))
        };
        assert!(!shown(&v));
        assert!(v.jump_to_heading("fnref:1"));
        assert_eq!(top(&v), "│ Said[1].");
    }

    #[test]
    fn jump_to_a_heading_in_shut_details_opens_it() {
        let md = format!(
            "{0}<details><summary>More</summary>\n\n## Inside\n\ntext\n\n</details>\n\n{0}",
            paragraphs(10)
        );
        let mut v = new_view(&md);
        v.resize(20, 5);
        assert!(v.jump_to_heading("inside"));
        assert_eq!(top(&v), "  Inside");
    }

    #[test]
    fn jump_to_a_heading_in_shut_details_in_a_definition_opens_it() {
        let md = format!(
            "{0}Term\n: <details><summary>More</summary>\n\n  ## Inside\n\n  </details>\n\n{0}",
            paragraphs(10)
        );
        let mut v = new_view(&md);
        v.resize(20, 5);
        assert!(v.jump_to_heading("inside"));
        assert!(top(&v).ends_with("Inside"), "{:?}", top(&v));
    }

    #[test]
    fn a_fold_stays_open_when_the_note_is_edited_above_it() {
        let note = "> [!note]- Shut\n> body\n";
        let mut v = new_view(note);
        v.resize(20, 5);
        v.fold(0);
        assert_eq!(v.layout.lines.len(), 2);
        v.reload(parse(&format!("new line\n\n{note}")));
        assert_eq!(v.layout.lines.len(), 4);
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
    fn search_marks_matches_steps_through_them_and_esc_clears_it() {
        // Paragraph n is on row 2n; "needle" is on rows 6, 20 and 32.
        let md: String = (0..20)
            .map(|n| match n {
                3 | 10 | 16 => format!("line {n} Needle\n\n"),
                _ => format!("line {n}\n\n"),
            })
            .collect();
        let (mut b, dir) = browser("search", &[("note.md", &md)]);
        let notice = |b: &Browser| b.view.notice.clone().unwrap_or_default();
        enter(&mut b, KeyCode::Char('/'));
        for c in "needle".chars() {
            enter(&mut b, KeyCode::Char(c));
        }
        assert_eq!(b.view.prompt.as_deref(), Some("/needle  3 matches"));
        assert_eq!(b.view.scroll, 0, "typing does not move the view");
        // Enter goes to the first match, a third of the way down the view.
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.view.prompt, None);
        assert_eq!((b.view.scroll, notice(&b)), (4, "match 1/3".into()));
        let mut steps = Vec::new();
        for code in [
            KeyCode::Char('n'),
            KeyCode::Char('n'),
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Char('N'),
        ] {
            enter(&mut b, code);
            steps.push((b.view.scroll, notice(&b)));
        }
        assert_eq!(
            steps,
            [
                (18, "match 2/3".into()),
                (30, "match 3/3".into()),
                (4, "match 1/3, wrapped to the top".into()),
                (30, "match 3/3, wrapped to the bottom".into()),
                (18, "match 2/3".into()),
            ]
        );
        // The match on screen is marked, the current one in reverse.
        let mut terminal = Terminal::new(TestBackend::new(30, 7)).unwrap();
        terminal.draw(|f| b.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        // The toolbar takes the top row; row 20 is the third in view.
        let y = 1 + 20 - 18;
        let marked: String = (0..30)
            .filter(|&x| buf[(x, y)].bg == Color::Yellow)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect();
        assert_eq!(marked, "Needle");
        assert!(buf[(8, y)].modifier.contains(Modifier::REVERSED));
        // A capital makes case count.
        enter(&mut b, KeyCode::Char('/'));
        enter(&mut b, KeyCode::Char('N'));
        enter(&mut b, KeyCode::Char('E'));
        assert_eq!(b.view.prompt.as_deref(), Some("/NE  no matches"));
        enter(&mut b, KeyCode::Enter);
        assert_eq!(notice(&b), "no matches for NE");
        // Esc clears the search rather than quitting; the next one quits.
        assert!(!b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!b.view.search.active());
        assert!(b.view.search.matches.is_empty());
        assert!(b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        let _ = std::fs::remove_dir_all(dir);
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
        // This note with no heading, a heading already at the top and a
        // missing heading go nowhere, and leave no Back step.
        assert_eq!(b.history.len(), 2);
        b.follow(&wiki("HOME"));
        b.follow(&wiki("#Later"));
        assert_eq!(top(&b.view), "Later");
        b.follow(&wiki("#nowhere"));
        assert_eq!(b.view.notice.as_deref(), Some("no heading found: nowhere"));
        assert_eq!(b.history.len(), 2);
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
        assert_eq!(b.history.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_footnote_and_its_back_link_jump_in_the_note_and_back_returns() {
        let home = format!(
            "# Top\n\n{0}Said[^n].\n\n{0}[^n]: The note.\n",
            paragraphs(8)
        );
        let (mut b, _dir) = browser("footnote", &[("HOME.md", &home)]);
        let link = |b: &Browser, dest: &str| {
            let spot = b.view.layout.links.iter().find(|l| l.link.dest == dest);
            spot.expect("a link").link.clone()
        };
        b.view.jump_to_heading("fnref:1");
        assert_eq!(top(&b.view), "Said[1].");
        b.follow(&link(&b, "#fn:1"));
        // The footnotes end the note, so the view stops at its end, which
        // shows them.
        assert_eq!(b.view.scroll, b.view.max_scroll());
        let shown = b.view.layout.lines[b.view.scroll..].iter();
        let shown: Vec<_> = shown.map(ToString::to_string).collect();
        assert!(shown.iter().any(|l| l == "1. The note. ↩"), "{shown:?}");
        b.back();
        assert_eq!(top(&b.view), "Said[1].");
        b.forward();
        b.follow(&link(&b, "#fnref:1"));
        assert_eq!(top(&b.view), "Said[1].");
        assert_eq!(b.history.len(), 2);
    }

    #[test]
    fn back_and_forward_step_through_jumps_in_one_note() {
        let home = format!("# Top\n\n{0}## Later\n\n{0}## Last\n\n{0}", paragraphs(8));
        let (mut b, dir) = browser("jumps", &[("HOME.md", &home), ("Plan.md", "# Plan\n")]);
        b.follow(&wiki("#Later"));
        b.follow(&wiki("#Last"));
        assert_eq!(top(&b.view), "Last");
        let doc = Rc::clone(&b.view.doc);
        b.back();
        assert_eq!(top(&b.view), "Later");
        // The note on screen is not read again.
        assert!(Rc::ptr_eq(&doc, &b.view.doc));
        b.back();
        assert_eq!(top(&b.view), "Top");
        assert!(b.history.is_empty());
        b.forward();
        assert_eq!(top(&b.view), "Later");
        // A new jump drops Forward, as a link to another note does.
        b.follow(&wiki("#Top"));
        assert!(b.ahead.is_empty());
        // Back from another note lands on the jump's heading, not the top.
        b.follow(&wiki("Plan"));
        b.back();
        assert_eq!(b.path, dir.join("HOME.md"));
        assert_eq!(top(&b.view), "Top");
        b.back();
        assert_eq!(top(&b.view), "Later");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn back_keeps_only_the_latest_places() {
        let home = format!("# Top\n\n{0}## Later\n\n{0}", paragraphs(8));
        let (mut b, dir) = browser("limit", &[("HOME.md", &home)]);
        for _ in 0..HISTORY_LIMIT + 5 {
            b.follow(&wiki("#Later"));
            b.follow(&wiki("#Top"));
        }
        assert_eq!(b.history.len(), HISTORY_LIMIT);
        // The newest place is still the next one Back goes to.
        b.back();
        assert_eq!(top(&b.view), "Later");
        while !b.history.is_empty() {
            b.back();
        }
        assert_eq!(b.ahead.len(), HISTORY_LIMIT);
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

    /// The sources of the image blocks in `blocks`, and in quotes in them.
    fn image_srcs(blocks: &[doc::Block]) -> Vec<String> {
        let mut srcs = Vec::new();
        for block in blocks {
            match &block.kind {
                BlockKind::Image { src, .. } => srcs.push(src.clone()),
                BlockKind::Quote(inner)
                | BlockKind::Callout { blocks: inner, .. }
                | BlockKind::Details { blocks: inner, .. } => srcs.extend(image_srcs(inner)),
                BlockKind::List { items, .. } => {
                    for item in items {
                        srcs.extend(image_srcs(&item.blocks));
                    }
                }
                _ => {}
            }
        }
        srcs
    }

    #[test]
    fn a_logo_for_the_other_background_is_left_out() {
        let md = "![logo](dark.png#gh-dark-mode-only)\n\n![logo](light.png#gh-light-mode-only)\n\n\
                  > ![logo](light.png#gh-light-mode-only)\n\n![shot](shot.png)\n";
        let vault = Vault::for_file(Path::new("/n/README.md"));
        // The tests run with no background reported, so both logos stay.
        // doc::parse_for's tests cover leaving one out.
        let doc = super::parse(&vault, md, Path::new("/n/README.md"));
        assert_eq!(image_srcs(&doc.blocks).len(), 4);
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
        assert_eq!(b.view.selected, Some(Stop::Link(0)));
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

    /// The keys crossterm makes of `reply`, a terminal's reply to a query
    /// sent at start-up, as seen through a pty: ESC and the next character
    /// become one key with Alt, and every other character a key of its own.
    fn reply_keys(reply: &str) -> Vec<KeyEvent> {
        let mut keys = Vec::new();
        let mut chars = reply.chars();
        while let Some(c) = chars.next() {
            keys.push(match c {
                '\x1b' => KeyEvent::new(KeyCode::Char(chars.next().unwrap()), KeyModifiers::ALT),
                '\x07' => KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
                c if c.is_ascii_uppercase() => KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT),
                c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            });
        }
        keys
    }

    #[test]
    fn late_terminal_replies_are_not_taken_as_keys() {
        let (mut b, dir) = browser("replies", &[("a.md", &paragraphs(20))]);
        b.view.scroll_by(5);
        let replies = [
            // The background colour, ended by ST and by BEL.
            "\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\",
            "\x1b]11;rgb:ffff/ffff/ffff\x07",
            // Whether the terminal has kitty graphics.
            "\x1b_Gi=31;OK\x1b\\",
        ];
        for key in replies.iter().flat_map(|r| reply_keys(r)) {
            assert!(!b.key(key), "{key:?} quit");
        }
        // Not moved by the g, G, f or b in them.
        assert_eq!(b.view.scroll, 5);
        // Keys after a reply still work, Esc on its own among them.
        enter(&mut b, KeyCode::Char('j'));
        assert_eq!(b.view.scroll, 6);
        assert!(b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_after_the_readers_own_alt_bracket_still_work() {
        let (mut b, dir) = browser("alt-bracket", &[("a.md", &paragraphs(20))]);
        b.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT));
        // Later than any part of a reply would come.
        std::thread::sleep(REPLY_GAP);
        enter(&mut b, KeyCode::Char('j'));
        assert_eq!(b.view.scroll, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_note_that_cannot_be_read_stays_on_the_way_back_and_forward() {
        let (mut b, dir) = browser(
            "unreadable",
            &[("HOME.md", "[[Other]]\n"), ("Other.md", "other\n")],
        );
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, dir.join("Other.md"));
        // Gone for a moment, as during an editor's save.
        std::fs::rename(dir.join("HOME.md"), dir.join("away")).unwrap();
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("Other.md"));
        assert!(
            b.view
                .notice
                .as_deref()
                .unwrap()
                .starts_with("cannot read file")
        );
        std::fs::rename(dir.join("away"), dir.join("HOME.md")).unwrap();
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("HOME.md"));
        // And the same going forward.
        std::fs::rename(dir.join("Other.md"), dir.join("away")).unwrap();
        enter(&mut b, KeyCode::Char('L'));
        assert_eq!(b.path, dir.join("HOME.md"));
        std::fs::rename(dir.join("away"), dir.join("Other.md")).unwrap();
        enter(&mut b, KeyCode::Char('L'));
        assert_eq!(b.path, dir.join("Other.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_note_gone_for_good_is_passed_on_the_second_try() {
        let (mut b, dir) = browser(
            "gone-for-good",
            &[("A.md", "[[B]]\n"), ("B.md", "[[C]]\n"), ("C.md", "c\n")],
        );
        for _ in 0..2 {
            enter(&mut b, KeyCode::Tab);
            enter(&mut b, KeyCode::Enter);
        }
        assert_eq!(b.path, dir.join("C.md"));
        std::fs::remove_file(dir.join("B.md")).unwrap();
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("C.md"));
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("A.md"));
        let notice = b.view.notice.clone().unwrap();
        assert!(
            notice.starts_with("passed") && notice.contains("B.md"),
            "{notice}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_note_missed_once_is_not_passed_after_going_elsewhere() {
        let (mut b, dir) = browser(
            "missed-once",
            &[
                ("A.md", "[[B]]\n"),
                ("B.md", "[[C]]\n"),
                ("C.md", "[[D]]\n"),
                ("D.md", "d\n"),
            ],
        );
        for _ in 0..3 {
            enter(&mut b, KeyCode::Tab);
            enter(&mut b, KeyCode::Enter);
        }
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("C.md"));
        // Back from C while B is being saved, then forward to D and back.
        std::fs::rename(dir.join("B.md"), dir.join("away")).unwrap();
        enter(&mut b, KeyCode::Backspace);
        std::fs::rename(dir.join("away"), dir.join("B.md")).unwrap();
        enter(&mut b, KeyCode::Char('L'));
        assert_eq!(b.path, dir.join("D.md"));
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("C.md"));
        // B is being saved again: one miss keeps it.
        std::fs::rename(dir.join("B.md"), dir.join("away")).unwrap();
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("C.md"));
        std::fs::rename(dir.join("away"), dir.join("B.md")).unwrap();
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("B.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
