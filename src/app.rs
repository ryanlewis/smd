//! The full-screen view: a scrollable window onto the laid-out document, and
//! the browser that moves it from note to note along links.

use std::collections::HashSet;
use std::io::{self, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
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

use crate::config::{Action, Align, Config, Tasks, WebLinks};
use crate::doc::{self, BlockKind, Document, Inline, Link};
use crate::embed;
use crate::help;
use crate::highlight;
use crate::images::{self, Images};
use crate::kitty;
use crate::render::{Layout, render_folds};
use crate::search::{self, Search};
use crate::theme;
use crate::vault::{self, Target, Vault};
use crate::watch::{Change, Watcher};

use unicode_width::UnicodeWidthStr;

/// How often to look at the file on disk. A change shows after two checks.
const CHECK_EVERY: Duration = Duration::from_millis(150);

/// How long after sending images to ask the terminal to repaint, at each
/// check. kitty may finish an image after it drew the cells that show it,
/// and then leaves them blank until something else changes on screen.
const REPAINT_FOR: Duration = Duration::from_secs(1);

pub struct View {
    /// Shared with the place Back or Forward keeps when the reader leaves.
    doc: Rc<Document>,
    /// The directory relative image paths are read from.
    base: PathBuf,
    images: Images,
    layout: Layout,
    /// Width the lines were laid out for; zero before the first layout.
    width: u16,
    /// The widest the text gets, and where it sits in a wider window.
    max_width: Option<u16>,
    align: Align,
    /// How far right of the view's left edge the text was last drawn.
    left: u16,
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
    /// The number typed before a key, to repeat it or go to that line.
    count: Option<usize>,
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
    /// Read the file again.
    Reload,
    /// Show the help page.
    Help,
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
            max_width: None,
            align: Align::Centre,
            left: 0,
            highlighted: false,
            height: 0,
            scroll: 0,
            notice: None,
            prompt: None,
            search: Search::default(),
            count: None,
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
        self.clear_search();
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
        if doc.source != self.doc.source {
            self.replace(doc);
        }
    }

    /// `reload` even when the text is the same, as an embedded note may
    /// have changed.
    fn replace(&mut self, doc: Document) {
        let anchor = self.anchor();
        let kept = self.current_match();
        let span = kept
            .as_ref()
            .and_then(|(a, _)| Some(self.block(a)?.span.clone()));
        let old = std::mem::replace(&mut self.doc, Rc::new(doc));
        let anchor = anchor.and_then(|a| a.moved(&old.source, &self.doc.source));
        // The match is only the same one while its block's text is.
        let kept = kept.zip(span).and_then(|((a, nth), span)| {
            let a = a.moved(&old.source, &self.doc.source)?;
            let text = &old.source[span.clone()];
            let new = self.doc.source.get(a.start..a.start + span.len());
            (new == Some(text)).then_some((a, nth))
        });
        let flipped = std::mem::take(&mut self.flipped);
        let flipped = flipped.into_iter();
        self.flipped = flipped
            .filter_map(|at| moved(at, &old.source, &self.doc.source))
            .collect();
        self.selected = None;
        self.want_images();
        self.relayout_keeping(anchor, kept);
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
        let kept = self.current_match();
        self.relayout_keeping(anchor, kept);
    }

    /// `relayout`, with the match the reader is on given by `kept`, as
    /// `current_match` gives it.
    /// A current match that was in view and is pushed out of it is brought
    /// back into view.
    fn relayout_keeping(&mut self, anchor: Option<Anchor>, kept: Option<(Anchor, usize)>) {
        let seen = self.current_seen();
        if self.width > 0 {
            self.lay_out(kept);
            if let Some(row) = anchor.and_then(|a| self.find(&a)) {
                self.scroll = row;
            }
        }
        self.settle(seen);
    }

    /// Lay the document out at the view's width, with the match `kept`
    /// names current, as `refresh_search` takes it.
    fn lay_out(&mut self, kept: Option<(Anchor, usize)>) {
        self.highlighted = highlight::ready().is_some();
        self.layout = render_folds(&self.doc, self.width, &self.images, &self.flipped);
        self.refresh_search(kept);
    }

    /// Keep the scroll and the selection within a new layout, and bring
    /// back into view a current match that was `seen` before it.
    fn settle(&mut self, seen: bool) {
        self.scroll = self.scroll.min(self.max_scroll());
        self.selected = self.selected.filter(|&s| self.has(s));
        if seen && !self.current_seen() {
            self.show_current();
        }
    }

    /// What to keep at the top of the view. A view at the very top stays there.
    fn anchor(&self) -> Option<Anchor> {
        if self.scroll == 0 {
            return None;
        }
        let (index, offset) = self.layout.anchor(self.scroll)?;
        Some(self.anchor_of(index, offset))
    }

    /// The anchor of the block at `index` in the layout, `offset` lines into it.
    fn anchor_of(&self, index: usize, offset: isize) -> Anchor {
        let blocks = &self.layout.blocks;
        let start = blocks[index].span.start;
        let rank = blocks[..index]
            .iter()
            .filter(|b| b.span.start == start)
            .count();
        Anchor {
            start,
            rank,
            offset,
        }
    }

    /// The block `anchor` names in the layout.
    fn block(&self, anchor: &Anchor) -> Option<&crate::render::BlockRows> {
        self.layout
            .blocks
            .iter()
            .filter(|b| b.span.start == anchor.start)
            .nth(anchor.rank)
    }

    /// The match the reader is on, as the innermost block it is in and how
    /// many matches come before it in that block, so a new layout can find
    /// it again even when the block's lines wrap differently.
    fn current_match(&self) -> Option<(Anchor, usize)> {
        let i = self.search.current?;
        let row = self.search.matches.get(i)?.row;
        let blocks = &self.layout.blocks;
        let index = blocks.iter().rposition(|b| b.rows.contains(&row))?;
        // Matches are in row order, and the current one is in the block.
        let start = blocks[index].rows.start;
        let nth = i - self.search.matches.partition_point(|m| m.row < start);
        Some((self.anchor_of(index, 0), nth))
    }

    /// Whether any of the current match is in view.
    fn current_seen(&self) -> bool {
        let current = self.search.current.and_then(|i| self.search.matches.get(i));
        current.is_some_and(|m| m.seen(&self.in_view()))
    }

    /// Scroll as little as brings all of the current match into view.
    fn show_current(&mut self) {
        let Some(m) = self.search.current.and_then(|i| self.search.matches.get(i)) else {
            return;
        };
        let rows = m.rows();
        let height = (self.height as usize).max(1);
        if rows.end > self.scroll + height {
            self.scroll = rows.end - height;
        }
        self.scroll = self.scroll.min(rows.start).min(self.max_scroll());
    }

    /// The top line that puts `anchor` back where it was: the same offset into
    /// its block. A block that got shorter keeps the view on its last line.
    fn find(&self, anchor: &Anchor) -> Option<usize> {
        let b = self.block(anchor)?;
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
    /// Returns whether anything was sent.
    pub fn flush_images(&mut self, out: &mut impl Write) -> io::Result<bool> {
        self.images.flush(out, &self.layout.placements())
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Lay the document out again when the size changes, keeping roughly the
    /// same part of it in view, and the current match when it was in view.
    fn resize(&mut self, width: u16, height: u16) {
        let seen = (width, height) != (self.width, self.height) && self.current_seen();
        if width != self.width {
            let kept = self.current_match();
            let old_len = self.layout.lines.len().max(1);
            self.width = width;
            self.lay_out(kept);
            self.scroll = self.scroll * self.layout.lines.len() / old_len;
        }
        self.height = height;
        self.settle(seen);
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
        // The bottom row, kept for the status when there is room for it.
        // A message takes the row even when nothing else would fit.
        let message = self.prompt.is_some() || self.notice.is_some();
        let status = area.height > 1 || (area.height == 1 && message);
        if status {
            area.height -= 1;
        }
        // The text keeps to `max_width`, at the left or in the middle.
        let width = self.max_width.map_or(area.width, |m| m.min(area.width));
        self.left = match self.align {
            Align::Centre => (area.width - width) / 2,
            Align::Left => 0,
        };
        // The status row keeps the whole width.
        let row = Rect::new(area.x, area.y + area.height, area.width, 1);
        let area = Rect::new(area.x + self.left, area.y, width, area.height);
        self.resize(area.width, area.height);
        if status {
            self.draw_status(frame, row);
        }
        let lines = &self.layout.lines;
        let end = (self.scroll + area.height as usize).min(lines.len());
        let visible = Text::from(lines[self.scroll..end].to_vec());
        frame.render_widget(Paragraph::new(visible), area);
        crate::heading::draw_sized(frame.buffer_mut(), area, self.scroll, &self.layout.sized);
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
            // Matches are in row order, so only those in view are looked at.
            let matches = &self.search.matches;
            let first = matches.partition_point(|m| m.rows().end <= scroll);
            let last = matches.partition_point(|m| m.row < end);
            for (i, m) in matches.iter().enumerate().take(last).skip(first) {
                let style = if self.search.current == Some(i) {
                    marked.add_modifier(Modifier::REVERSED)
                } else {
                    marked
                };
                for rect in cells(&m.cells) {
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

    /// Draw the status row: the prompt or notice on the left, and the
    /// number typed so far and the page on the right.
    fn draw_status(&self, frame: &mut Frame, row: Rect) {
        let style = bar_style();
        frame.render_widget(Paragraph::new("").style(style), row);
        let mut right = self.page();
        if let Some(count) = self.count {
            right = format!("{count}  {right}");
        }
        let right = format!(" {right} ");
        let right_width = right.width() as u16;
        let room = row.width.saturating_sub(right_width);
        // The message wins on a pane too narrow for both.
        let left = self.prompt.as_ref().or(self.notice.as_ref());
        let left_width = left.map_or(0, |l| l.width() as u16 + 1);
        let (left_row, right_row) = if right_width <= row.width && left_width <= room {
            let right_row = Rect::new(row.x + room, row.y, row.width - room, 1);
            (Rect::new(row.x, row.y, room, 1), Some(right_row))
        } else {
            (row, None)
        };
        if let Some(left) = left {
            let text = Paragraph::new(left.as_str()).style(style.add_modifier(Modifier::BOLD));
            frame.render_widget(text, left_row);
        }
        if let Some(right_row) = right_row {
            frame.render_widget(Paragraph::new(right).style(style), right_row);
        }
    }

    /// The link or checkbox drawn at column `x` of row `y` of the view, if any.
    fn stop_at(&self, x: u16, y: u16) -> Option<Stop> {
        if y >= self.height {
            return None;
        }
        let x = x.checked_sub(self.left).filter(|&x| x < self.width)?;
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
    #[cfg(test)]
    pub fn act(&mut self, action: Action) -> Option<Go> {
        self.act_counted(action, None)
    }

    /// Do what an action says, with the number typed before its key: a
    /// scroll goes that many times as far, top and bottom go to that line,
    /// and link selection and `n` and `N` move that many times. Back,
    /// forward and the rest ignore it here.
    fn act_counted(&mut self, action: Action, count: Option<usize>) -> Option<Go> {
        let times = count.unwrap_or(1);
        let page = (self.height as isize).max(1);
        let half = (page / 2).max(1);
        let scroll = |v: &mut View, by: isize| v.scroll_by(by.saturating_mul(times as isize));
        match action {
            Action::Quit => return Some(Go::Quit),
            Action::Down => scroll(self, 1),
            Action::Up => scroll(self, -1),
            Action::HalfDown => scroll(self, half),
            Action::HalfUp => scroll(self, -half),
            Action::PageDown => scroll(self, page),
            Action::PageUp => scroll(self, -page),
            Action::Top | Action::Bottom if count.is_some() => {
                self.scroll = (times - 1).min(self.max_scroll());
            }
            Action::Top => self.scroll = 0,
            Action::Bottom => self.scroll = self.max_scroll(),
            Action::NextLink | Action::PrevLink => {
                // Going round, more than once round is the same as less.
                let round = self.stops().len().max(1);
                let times = if times > round {
                    times % round + round
                } else {
                    times
                };
                for _ in 0..times {
                    self.select_link(action == Action::NextLink);
                }
            }
            Action::Follow => return Some(self.choose(self.selected?)),
            // Only a checkbox: a selected link stays where it is.
            Action::Toggle => match self.selected? {
                stop @ Stop::Task(_) => return Some(self.choose(stop)),
                Stop::Link(_) | Stop::Fold(_) => {}
            },
            Action::Back => return Some(Go::Back),
            Action::Forward => return Some(Go::Forward),
            Action::Search => self.start_search(),
            Action::NextMatch => return Some(self.next_match(true, times)),
            Action::PrevMatch => return Some(self.next_match(false, times)),
            Action::Reload => return Some(Go::Reload),
            Action::Help => return Some(Go::Help),
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
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                self.clear_search();
                return None;
            }
            _ => return None,
        }
        // A new term numbers its matches anew, so none is current.
        self.refresh_search(None);
        None
    }

    /// Find the search term in the layout again, keeping the prompt's count
    /// up to date while it is typed. The match `kept` names, as
    /// `current_match` gave it in the old layout, stays current when it is
    /// still there.
    fn refresh_search(&mut self, kept: Option<(Anchor, usize)>) {
        let kept = kept.and_then(|(anchor, nth)| Some((self.block(&anchor)?.rows.clone(), nth)));
        self.search.refresh(&self.layout, &self.doc, kept);
        if self.search.typing {
            self.prompt = Some(self.search.prompt());
        }
    }

    /// Give up the search and stop marking its matches.
    fn clear_search(&mut self) {
        self.search = Search::default();
        self.prompt = None;
    }

    /// Go `times` matches on or back in the search, going round at the
    /// ends.
    fn next_match(&mut self, forward: bool, times: usize) -> Go {
        if !self.search.active() {
            return Go::Say("no search: press / to search".into());
        }
        let found = self.search.step_by(forward, self.in_view(), times);
        self.go_to_match(found.map(|(i, round)| (i, round.then_some(forward))))
    }

    /// Make `found` the current match and bring it into view, a little way
    /// down the page when that moves it. Says which match it is, or that
    /// there are none.
    fn go_to_match(&mut self, found: Option<(usize, Option<bool>)>) -> Go {
        let wrapped = found.and_then(|(_, w)| w);
        if let Some((i, _)) = found {
            self.search.current = Some(i);
            let rows = self.search.matches[i].rows();
            let row = rows.start;
            let view = self.in_view();
            if !view.contains(&row) || !view.contains(&(rows.end - 1)) {
                self.scroll = row
                    .saturating_sub(self.height as usize / 3)
                    .min(self.max_scroll());
            }
        }
        Go::Say(self.search.status(wrapped))
    }

    /// Where the view is as "page X/Y": the page its top line is on, and the
    /// last page once the end is in view.
    fn page(&self) -> String {
        let height = (self.height as usize).max(1);
        let pages = self.layout.lines.len().div_ceil(height).max(1);
        let page = if self.scroll >= self.max_scroll() {
            pages
        } else {
            self.scroll / height + 1
        };
        format!("page {page}/{pages}")
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
        // The title stays put rather than the current match.
        let scroll = self.scroll;
        self.relayout(None);
        if let (Some((_, old)), Some((i, new))) = (before, row(&self.layout)) {
            let screen = old.saturating_sub(scroll);
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
        let view = self.in_view();
        let at = self
            .selected
            .and_then(|s| links.iter().position(|&l| l == s));
        let selected = at.filter(|&i| view.contains(&rows[i]));
        let row = |i: usize| rows[i];
        let Some((next, _)) = search::step_round(links.len(), selected, forward, view, row) else {
            return;
        };
        self.selected = Some(links[next]);
        // Bring it into view, a little way down the page when that moves it,
        // and all of it when it fits, as a heading drawn larger takes rows.
        let row = rows[next];
        let last = self.cells(links[next]).last().map_or(row, |c| c.0);
        let view = self.in_view();
        if !view.contains(&row) || !view.contains(&last) {
            let down = (last + 1).saturating_sub(self.height as usize);
            self.scroll = row
                .saturating_sub(self.height as usize / 3)
                .max(down.min(row))
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
    // The second column of a wide character holds nothing to wrap, nor do
    // the cells under a heading drawn larger. Its first cell keeps its width.
    let width = match cell.diff_option {
        CellDiffOption::Skip => return,
        CellDiffOption::ForcedWidth(width) => width,
        _ => match NonZeroU16::new(text.width() as u16) {
            Some(width) => width,
            None => return,
        },
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
    /// It was read from stdin, so it cannot be read again.
    piped: bool,
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
    /// The note on screen was read from stdin, or is the help page: it has
    /// no file to reload or tick tasks in.
    piped: bool,
    vault: Vault,
    config: Config,
    /// Where Back goes, the last place last.
    history: Vec<Visited>,
    /// Where Forward goes, after going back.
    ahead: Vec<Visited>,
    /// The notice on screen is about the last key, and goes at the next.
    passing: bool,
    /// The notice it covers, such as the file being deleted, to show again.
    covered: Option<String>,
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
            piped: false,
            vault,
            config,
            history: Vec::new(),
            ahead: Vec::new(),
            passing: false,
            covered: None,
            replying: None,
            missed: None,
            clipboard: Box::new(io::stdout()),
            opener: open_with_system,
        };
        browser.view.max_width = browser.config.max_width;
        browser.view.align = browser.config.align;
        browser.check_tasks();
        browser
    }

    /// The same browser, its note read from stdin: it is not reloaded and
    /// its tasks cannot be ticked.
    pub fn piped(mut self) -> Self {
        self.piped = true;
        self.check_tasks();
        self
    }

    /// Show a short message until the next key.
    fn say(&mut self, notice: String) {
        if !self.passing {
            self.covered = self.view.notice.take();
        }
        self.view.set_notice(Some(notice));
        self.passing = true;
    }

    /// Take away a notice about the last key, showing the one it covered.
    fn unsay(&mut self) {
        if std::mem::take(&mut self.passing) {
            self.view.set_notice(self.covered.take());
        }
    }

    /// Handle a mouse event: a click on a link follows it, and the wheel
    /// scrolls.
    fn mouse(&mut self, event: MouseEvent) {
        // Clicks wait until the search term is typed.
        if self.view.search.typing {
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if event.modifiers.is_empty() => {
                self.unsay();
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
        self.unsay();
        if self.view.search.typing {
            if let Some(go) = self.view.search_key(key) {
                self.go(go);
            }
            return false;
        }
        if self.count_digit(key) {
            return false;
        }
        let count = self.view.count.take();
        // Esc drops a number typed so far, rather than quitting.
        if count.is_some() && key.code == KeyCode::Esc {
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
        match self.view.act_counted(action, count) {
            Some(Go::Quit) => return true,
            Some(go @ (Go::Back | Go::Forward)) => {
                let times = count.unwrap_or(1).min(HISTORY_LIMIT);
                for _ in 0..times {
                    self.go(go.clone());
                }
            }
            Some(go) => self.go(go),
            None => {}
        }
        false
    }

    /// Take a digit key, not bound to an action, as part of the number typed
    /// before a key. A number starts with 1 to 9. Returns true when it was
    /// taken.
    fn count_digit(&mut self, key: event::KeyEvent) -> bool {
        let digit = match key.code {
            KeyCode::Char(c) if (key.modifiers - KeyModifiers::SHIFT).is_empty() => c.to_digit(10),
            _ => None,
        };
        let Some(digit) = digit.filter(|_| self.config.action(key).is_none()) else {
            return false;
        };
        let count = &mut self.view.count;
        match *count {
            None if digit == 0 => {}
            None => *count = Some(digit as usize),
            // More digits than this go nowhere a document can.
            Some(n) if n >= COUNT_LIMIT / 10 => {}
            Some(n) => *count = Some(n * 10 + digit as usize),
        }
        true
    }

    /// Show the help page as a note of its own, so Back returns from it.
    /// Like a note read from stdin, it has no file to reload or tick tasks
    /// in.
    fn help(&mut self) {
        let path = self.vault.root.join(HELP_NAME);
        if self.piped && self.path == path {
            return;
        }
        let here = self.here();
        self.ahead.clear();
        let page = help::page(&self.config);
        self.show(path, &page, true);
        self.remember(here);
    }

    /// Read the file again by hand, as the watcher does when it changes.
    fn reload(&mut self) {
        // The path of a note read from stdin is made up.
        if self.piped {
            self.say("nothing to reload".into());
            return;
        }
        match std::fs::read_to_string(&self.path) {
            Ok(source) => {
                self.view.replace(parse(&self.vault, &source, &self.path));
                self.view.set_notice(None);
                self.covered = None;
                self.check_tasks();
            }
            Err(err) => self.say(format!("cannot reload: {err}: {}", self.path.display())),
        }
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
            Go::Reload => self.reload(),
            Go::Help => self.help(),
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
        self.view.tasks = self.config.tasks == Tasks::Toggle && !self.piped && writable(&self.path);
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
            Target::Note { path, heading } if path == self.path && !self.piped => {
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
            piped: self.piped,
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
        if !showing {
            if visited.piped {
                // Stdin cannot be read twice: its text is shown again.
                let source = visited.doc.source.clone();
                self.show(visited.path.clone(), &source, true);
            } else if !self.open(visited.path.clone()) {
                return false;
            }
        }
        // Folds are kept by where they start, so only in the same text.
        if self.view.doc.source == visited.doc.source {
            self.view.flipped = visited.flipped.clone();
        }
        self.view.scroll = visited.scroll;
        self.view.selected = visited.selected;
        self.view.relayout(None);
        // The place gone back to stays put rather than the current match.
        self.view.scroll = visited.scroll.min(self.view.max_scroll());
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
        let base = bar_style();
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
        self.show(path, &source, false);
        self.watcher = watcher;
        true
    }

    /// Show `source` as the note at `path`, read from stdin when `piped`.
    fn show(&mut self, path: PathBuf, source: &str, piped: bool) {
        if !self.vault.holds(&path) {
            self.vault = Vault::for_file(&path);
        }
        let doc = parse(&self.vault, source, &path);
        // Keep the images of the nearest notes Back and Forward lead to, and
        // of the one being left, which is about to join them.
        let back = self.history.iter().rev().take(KEEP_IMAGES_OF);
        let notes = back.chain(self.ahead.iter().rev().take(KEEP_IMAGES_OF));
        let mut kept: Vec<_> = notes.map(|v| (v.doc.clone(), folder(&v.path))).collect();
        kept.push((Rc::clone(&self.view.doc), folder(&self.path)));
        self.view.kept = kept;
        self.view.show(doc, folder(&path));
        self.view.set_notice(None);
        self.covered = None;
        self.missed = None;
        if piped {
            self.watcher = Watcher::new(path.clone());
        }
        self.path = path;
        self.piped = piped;
        self.check_tasks();
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

/// How the toolbar and the status row are drawn: on the code panel's
/// colour, or reversed when there is none.
fn bar_style() -> Style {
    match theme::code_panel() {
        Some(bg) => Style::new().bg(bg),
        None => Style::new().add_modifier(Modifier::REVERSED),
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

/// The name the help page goes by on the toolbar.
const HELP_NAME: &str = "help";

/// How many notes each way along Back and Forward keep their images.
const KEEP_IMAGES_OF: usize = 8;

/// How many places Back can go to. Following a link past this drops the
/// oldest.
const HISTORY_LIMIT: usize = 100;

/// The longest pause between two keys of a terminal reply.
const REPLY_GAP: Duration = Duration::from_millis(50);

/// The number typed before a key stays below this.
const COUNT_LIMIT: usize = 1_000_000;

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
    // After SIGHUP the terminal is gone, so failing to write to it is
    // expected and not worth reporting.
    if stop_signal().is_some() {
        return Ok(());
    }
    result.and(deleted)
}

/// Set to SIGTERM or SIGHUP when one arrives, else 0. The run loop sees it
/// and ends as if the reader had quit, so the terminal is restored and the
/// images removed.
static STOP: AtomicI32 = AtomicI32::new(0);

/// The signal that asked smd to stop, if one did.
pub fn stop_signal() -> Option<i32> {
    Some(STOP.load(Ordering::Relaxed)).filter(|&signal| signal != 0)
}

/// End smd by `signal`, as it would have ended without the handler, so
/// whoever sent it sees the usual exit status.
#[cfg(unix)]
pub fn raise(signal: i32) {
    // SAFETY: sets the default action and raises a signal; no memory is
    // touched.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

#[cfg(not(unix))]
pub fn raise(_: i32) {}

/// True until smd starts to set the terminal up. Until then a signal ends
/// smd at once, as nothing needs restoring.
static AT_ONCE: AtomicBool = AtomicBool::new(true);

/// Whether `reading stdin…` is on stderr's line, for a signal to clear.
static STDIN_HINT: AtomicBool = AtomicBool::new(false);

/// The end of a pipe a stop signal writes a byte to, to wake the thread that
/// makes sure smd ends (see `make_sure_smd_ends`). -1 when there is none.
static WAKE: AtomicI32 = AtomicI32::new(-1);

/// The shared memory objects to remove if smd is stuck when asked to stop.
static OBJECTS: OnceLock<images::Uploaded> = OnceLock::new();

/// How long the terminal may go without taking any output, after a stop
/// signal, before smd is taken to be stuck writing to it and is ended.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// How often to look again whether the terminal takes output while smd is
/// busy rather than stuck after a stop signal.
const STOP_RECHECK: Duration = Duration::from_millis(100);

/// What clears the stdin hint: back to the line's start, then erase it.
pub const CLEAR_LINE: &str = "\r\x1b[K";

/// Say whether the stdin hint is on stderr's line.
pub fn stdin_hint(showing: bool) {
    STDIN_HINT.store(showing, Ordering::Relaxed);
}

/// Remove the objects `uploaded` holds if a stop signal cannot end the run
/// loop, as when smd is stuck writing to a terminal that does not read.
pub fn remove_on_stop(uploaded: images::Uploaded) {
    let _ = OBJECTS.set(uploaded);
}

/// Make SIGTERM and SIGHUP end smd cleanly, and Ctrl-C too while smd waits
/// for piped stdin. A signal smd was started with ignored, as SIGHUP under
/// `nohup`, stays ignored. Until `stop_later`, a signal ends smd at once,
/// after clearing the stdin hint if it shows.
#[cfg(unix)]
pub fn stop_on_signals() {
    if let Some(read) = wake_pipe() {
        let _ = std::thread::Builder::new()
            .name("stop".into())
            .spawn(move || make_sure_smd_ends(read));
    }
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        if !ignored(signal) {
            handle(
                signal,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

#[cfg(not(unix))]
pub fn stop_on_signals() {}

/// From now on a stop signal ends the run loop rather than smd at once, and
/// Ctrl-C is left to the terminal: it is a key while the terminal is raw.
#[cfg(unix)]
pub fn stop_later() {
    if !ignored(libc::SIGINT) {
        handle(libc::SIGINT, libc::SIG_DFL);
    }
    AT_ONCE.store(false, Ordering::Relaxed);
}

#[cfg(not(unix))]
pub fn stop_later() {
    AT_ONCE.store(false, Ordering::Relaxed);
}

/// Only loads and stores atomics and calls `write` and `raise`, which are
/// safe in a signal handler.
#[cfg(unix)]
extern "C" fn on_signal(signal: libc::c_int) {
    if AT_ONCE.load(Ordering::Relaxed) {
        if STDIN_HINT.load(Ordering::Relaxed) {
            // SAFETY: writes a constant buffer to stderr.
            unsafe {
                libc::write(
                    libc::STDERR_FILENO,
                    CLEAR_LINE.as_ptr().cast(),
                    CLEAR_LINE.len(),
                )
            };
        }
        // SA_RESETHAND has set the default action back, so this ends smd as
        // the signal would have once the handler returns.
        // SAFETY: raise is async-signal safe.
        unsafe { libc::raise(signal) };
        return;
    }
    STOP.store(signal, Ordering::Relaxed);
    let wake = WAKE.load(Ordering::Relaxed);
    if wake >= 0 {
        // SAFETY: writes one byte from a local; the pipe does not block.
        unsafe { libc::write(wake, [1u8].as_ptr().cast(), 1) };
    }
}

/// Whether `signal` is ignored, as SIGHUP is under `nohup`.
#[cfg(unix)]
fn ignored(signal: libc::c_int) -> bool {
    // SAFETY: a zeroed sigaction is valid; a null new action only reads the
    // current one.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, std::ptr::null(), &mut old) == 0
            && old.sa_sigaction == libc::SIG_IGN
    }
}

/// Handle `signal` with `handler`, a function or `SIG_DFL`.
#[cfg(unix)]
fn handle(signal: libc::c_int, handler: libc::sighandler_t) {
    // SAFETY: a zeroed sigaction is valid; the handler is async-signal
    // safe. SA_RESTART keeps other system calls from failing with EINTR.
    // SA_RESETHAND makes a second signal stop smd at once.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_RESTART | libc::SA_RESETHAND;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

/// A pipe for the stop signal to wake a thread with. Its write end, which
/// never blocks, goes to `WAKE`; the read end is returned. Neither is passed
/// on to child processes.
#[cfg(unix)]
fn wake_pipe() -> Option<libc::c_int> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe writes two descriptors into fds; fcntl only changes
    // flags on them.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return None;
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let flags = libc::fcntl(fds[1], libc::F_GETFL);
        libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    WAKE.store(fds[1], Ordering::Relaxed);
    Some(fds[0])
}

/// Wait on `read` for a stop signal, then end smd here if the terminal takes
/// no output for `STOP_GRACE`: smd is then stuck writing to it. While the
/// terminal takes output, even slowly as over ssh, smd is busy rather than
/// stuck, as with a large document to lay out, and the run loop is left to
/// end it and restore the terminal. Only the shared memory objects are
/// removed, as the terminal cannot be written to. Their lock is never held
/// across a write, so this does not wait on a stuck thread.
#[cfg(unix)]
fn make_sure_smd_ends(read: libc::c_int) {
    let mut byte = 0u8;
    loop {
        // SAFETY: reads at most one byte into a local.
        let n = unsafe { libc::read(read, (&raw mut byte).cast(), 1) };
        if n == 1 {
            break;
        }
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return;
    }
    loop {
        match takes_output(libc::STDOUT_FILENO, STOP_GRACE) {
            Ok(true) => std::thread::sleep(STOP_RECHECK),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            _ => break,
        }
    }
    if let Some(objects) = OBJECTS.get() {
        objects.remove_objects();
    }
    if let Some(signal) = stop_signal() {
        raise(signal);
    }
}

/// Whether a write to `fd` would go through within `wait` rather than wait
/// longer. Asked with select, as poll on macOS does not work with terminals.
#[cfg(unix)]
fn takes_output(fd: libc::c_int, wait: Duration) -> io::Result<bool> {
    let mut wait = libc::timeval {
        tv_sec: wait.as_secs() as libc::time_t,
        tv_usec: wait.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: a zeroed fd_set is valid and fd is below FD_SETSIZE; select
    // only looks at the descriptor and writes to the set and the timeval.
    let ready = unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        libc::select(
            fd + 1,
            std::ptr::null_mut(),
            &mut set,
            std::ptr::null_mut(),
            &mut wait,
        )
    };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ready > 0)
}

/// Where the run loop gets the terminal's events and the time, so a test
/// can stand in for both.
trait Input {
    fn now(&self) -> Instant;
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
    /// Whether smd was asked to stop by a signal.
    fn stopped(&self) -> bool;
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

    fn stopped(&self) -> bool {
        stop_signal().is_some()
    }
}

fn run_browser<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    browser: &mut Browser,
    input: &mut impl Input,
) -> io::Result<()> {
    let mut last_check = input.now();
    let mut redraw = true;
    // Until when to ask the terminal to repaint (see `REPAINT_FOR`).
    let mut repaint_until = None;
    loop {
        // Polls wait at most CHECK_EVERY, so this is seen soon after.
        if input.stopped() {
            return Ok(());
        }
        if redraw {
            // Images go before the cells that show them, and a layout made
            // while drawing, as on a resize, sends its images just after.
            let before = browser.view.flush_images(terminal.backend_mut())?;
            terminal.draw(|frame| browser.draw(frame))?;
            let after = browser.view.flush_images(terminal.backend_mut())?;
            if before || after {
                repaint_until = Some(input.now() + REPAINT_FOR);
            }
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
            if repaint_until.is_some_and(|until| last_check < until) {
                crate::kitty::repaint(terminal.backend_mut())?;
            }
            if !browser.piped
                && let Some(change) = browser.watcher.check()
            {
                apply(&mut browser.view, &browser.path, &browser.vault, change);
                // What the watcher saw is newer than a covered notice.
                browser.covered = None;
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
    fn the_page_is_where_the_top_line_is_and_the_last_at_the_end() {
        // 19 lines, 5 to a page.
        let mut v = view(10);
        assert_eq!(v.page(), "page 1/4");
        v.scroll_by(4);
        assert_eq!(v.page(), "page 1/4");
        v.scroll_by(3);
        assert_eq!(v.page(), "page 2/4");
        v.act(Action::Bottom);
        assert_eq!(v.page(), "page 4/4");
        // An empty note, or one shorter than the view, is one page.
        assert_eq!(view(0).page(), "page 1/1");
        assert_eq!(view(1).page(), "page 1/1");
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
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        terminal.draw(|f| v.draw(f)).unwrap();
        assert_eq!(v.layout.lines.len(), 1);
        terminal.backend_mut().resize(6, 4);
        terminal.draw(|f| v.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(rows(buf)[..3], ["alpha ", "beta  ", "gamma "]);
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
        let mut terminal = Terminal::new(TestBackend::new(6, 4)).unwrap();
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

    /// Search `v` for `term` and go to match `i`.
    fn on_match(v: &mut View, term: &str, i: usize) {
        v.search.term = term.into();
        v.refresh_search(None);
        v.go_to_match(Some((i, None)));
    }

    #[test]
    fn the_current_match_stays_when_images_arrive_or_the_width_changes() {
        use crate::images::tests::png;
        let dir = std::env::temp_dir().join(format!("smd-app-{}-kept", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), png(20, 100)).unwrap();
        let md = "pin\n\n![a](a.png)\n\npin one pin two\n\npin\n";
        let mut v = View::new(parse(md), dir.clone(), Images::new((10, 20)));
        v.resize(20, 5);
        // The second "pin" in the paragraph after the image.
        on_match(&mut v, "pin", 2);
        let before = v.search.matches[2].clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !v.receive_images() {
            assert!(Instant::now() < deadline, "image not ready");
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = std::fs::remove_dir_all(&dir);
        // The image is five lines, not one, so the match moved down.
        assert_eq!(v.search.current, Some(2));
        assert_eq!(v.search.matches[2].row, before.row + 4);
        assert_eq!(v.search.matches[2].cells[0].1, before.cells[0].1);
        // Narrower, the paragraph wraps and the match starts its second line.
        v.resize(10, 5);
        assert_eq!(v.search.current, Some(2));
        let now = &v.search.matches[2];
        assert_eq!(now.cells, [(before.row + 5, 0..3)]);
        // A match that is gone leaves none current.
        on_match(&mut v, "two", 0);
        v.reload(parse("pin\n"));
        assert_eq!(v.search.current, None);
        // So does an edit to its block: the second "pin" is not the one the
        // reader was on any more.
        v.reload(parse("pin one pin two\n\nend\n"));
        on_match(&mut v, "pin", 1);
        v.reload(parse("pin pin one pin two\n\nend\n"));
        assert_eq!(v.search.current, None);
        // An edit to another block keeps it.
        on_match(&mut v, "pin", 2);
        v.reload(parse("pin pin one pin two\n\nthe end\n"));
        assert_eq!(v.search.current, Some(2));
    }

    #[test]
    fn a_current_match_in_view_stays_in_view_when_a_resize_pushes_it_down() {
        // At 40 columns the first paragraph is two lines and "needle" is on
        // line 3; at 10 it is six lines and "needle" is on line 7.
        let words = "one two three four five six seven eight nine ten";
        let md = format!("{words}\n\nneedle\n\n{}", paragraphs(10));
        let mut v = new_view(&md);
        v.resize(40, 5);
        on_match(&mut v, "needle", 0);
        assert_eq!((v.scroll, v.search.matches[0].row), (0, 3));
        v.resize(10, 5);
        assert_eq!(v.search.current, Some(0));
        assert_eq!(v.search.matches[0].row, 7);
        assert_eq!(v.scroll, 3);
        // Shorter, it is still kept on the bottom line.
        v.resize(10, 3);
        assert_eq!(v.scroll, 5);
        // A match out of view does not move the view.
        let mut v = new_view(&md);
        v.resize(40, 5);
        on_match(&mut v, "line 9", 0);
        v.scroll_by(-100);
        v.resize(10, 5);
        assert_eq!(v.search.current, Some(0));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn a_match_half_in_view_does_not_move_the_view_on_a_relayout() {
        // At 40 columns "eight nine" is on lines 0 and 1.
        let md = format!(
            "one two three four five six seven eight nine ten\n\n{}",
            paragraphs(10)
        );
        let mut v = new_view(&md);
        v.resize(40, 4);
        on_match(&mut v, "eight nine", 0);
        v.scroll = 1;
        v.relayout(None);
        assert_eq!(v.scroll, 1);
    }

    #[test]
    fn a_match_wrapped_over_two_lines_is_marked_on_both() {
        // At 40 columns "nine" goes to the second line.
        let md = format!(
            "one two three four five six seven eight nine ten\n\n{}",
            paragraphs(4)
        );
        let mut v = new_view(&md);
        v.resize(40, 4);
        on_match(&mut v, "eight nine", 0);
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        terminal.draw(|f| v.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let marked = |y: u16| -> String {
            (0..40)
                .filter(|&x| buf[(x, y)].bg == Color::Yellow)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        };
        assert_eq!((marked(0), marked(1)), ("eight".into(), "nine".into()));
        assert!(buf[(0, 1)].modifier.contains(Modifier::REVERSED));
        // Half of it in view is enough to mark it and to step from it.
        v.scroll = 1;
        terminal.draw(|f| v.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0, 0)].bg, Color::Yellow);
        assert_eq!(v.search.step(true, v.in_view()), Some((0, true)));
    }

    #[test]
    fn going_to_a_wrapped_match_brings_both_its_lines_into_view() {
        // "eight" is on row 4, the last in view, and "nine" on row 5.
        let md = format!(
            "a\n\nb\n\none two three four five six seven eight nine ten\n\n{}",
            paragraphs(4)
        );
        let mut v = new_view(&md);
        v.resize(40, 5);
        on_match(&mut v, "eight nine", 0);
        assert_eq!(v.search.matches[0].rows(), 4..6);
        assert!(v.in_view().contains(&5), "scroll {}", v.scroll);
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
        assert_eq!(rows(buf), ["one     ", "        ", "file del"]);
        apply(&mut v, Change::Changed("back\n".into()));
        terminal.draw(|f| v.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(rows(buf), ["back    ", "        ", "        "]);
    }

    /// The text of each row of `buf`.
    fn rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        let area = buf.area;
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
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
    fn tab_brings_all_of_a_tall_link_into_view() {
        let md = format!("{}[A](https://e.com)\n\n{}", paragraphs(4), paragraphs(10));
        let mut v = new_view(&md);
        v.resize(20, 5);
        // Laid out as a heading three times the size, the link takes three
        // rows, and only its first is in view.
        let row = v.layout.links[0].cells[0].0;
        v.layout.links[0].cells = (row..row + 3).map(|r| (r, 0..3)).collect();
        v.scroll = row - 4;
        v.act(Action::NextLink);
        assert_eq!(v.selected, Some(Stop::Link(0)));
        let view = v.in_view();
        assert!(view.contains(&row) && view.contains(&(row + 2)), "{view:?}");
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
    fn a_link_in_a_sized_heading_wraps_its_osc66_cell() {
        let mut cell = Cell::new("\x1b]66;s=2:w=1;a\x1b\\");
        let two = NonZeroU16::new(2).unwrap();
        cell.set_diff_option(CellDiffOption::ForcedWidth(two));
        hyperlink(&mut cell, 0, "https://e.com");
        assert_eq!(
            cell.symbol(),
            "\x1b]8;id=smd0;https://e.com\x1b\\\x1b]66;s=2:w=1;a\x1b\\\x1b]8;;\x1b\\"
        );
        assert_eq!(cell.diff_option, CellDiffOption::ForcedWidth(two));
        // The cells it covers are left alone.
        let mut under = Cell::new(" ");
        under.set_diff_option(CellDiffOption::Skip);
        hyperlink(&mut under, 0, "https://e.com");
        assert_eq!(under.symbol(), " ");
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
    fn max_width_keeps_the_text_narrow_at_the_left_or_in_the_middle() {
        let md = format!("{}\n\n```\n{}\n```\n", "word ".repeat(40), "x".repeat(50));
        for (align, left) in [(Align::Centre, 20), (Align::Left, 0)] {
            let mut v = new_view(&md);
            v.max_width = Some(20);
            v.align = align;
            let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
            terminal.draw(|f| v.draw(f)).unwrap();
            assert_eq!((v.width, v.left), (20, left));
            assert!(v.layout.lines.iter().all(|l| l.width() <= 20));
            let buf = terminal.backend().buffer();
            // Every row but the status row at the bottom.
            for y in 0..11 {
                let row: String = (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect();
                let (before, rest) = row.split_at(left as usize);
                let (_, after) = rest.split_at(20);
                assert!(
                    before.trim().is_empty() && after.trim().is_empty(),
                    "{row:?}"
                );
            }
            let first: String = (left..left + 20)
                .map(|x| buf[(x, 0)].symbol().to_string())
                .collect();
            assert!(first.starts_with("word word"), "{first:?}");
        }
        // A narrower window than the limit uses all of it.
        let mut v = new_view(&md);
        v.max_width = Some(20);
        let mut terminal = Terminal::new(TestBackend::new(15, 5)).unwrap();
        terminal.draw(|f| v.draw(f)).unwrap();
        assert_eq!((v.width, v.left), (15, 0));
    }

    #[test]
    fn a_sized_heading_is_drawn_where_max_width_moves_it() {
        use crate::heading::{SizedCell, SizedLine};
        let mut v = new_view("[A](https://e.com)\n");
        v.max_width = Some(20);
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        let href = |link: &Link| Some(link.dest.clone());
        terminal.draw(|f| v.draw_in(f, f.area(), href)).unwrap();
        assert_eq!((v.width, v.left), (20, 20));
        // Laid out as a heading twice the size: the layout is kept, as the
        // width has not changed.
        v.layout.sized.push(SizedLine {
            row: 0,
            scale: 2,
            cells: vec![SizedCell {
                col: 0,
                width: 1,
                text: "A".into(),
            }],
        });
        v.layout.links[0].cells = vec![(0, 0..2), (1, 0..2)];
        // The frame as drawn, with the cells the terminal is not sent.
        let frame = terminal.draw(|f| v.draw_in(f, f.area(), href)).unwrap();
        let buf = frame.buffer;
        let erase = "\x1b[1B\x1b[2X\x1b[A\x1b[2X";
        assert_eq!(
            buf[(20, 0)].symbol(),
            format!(
                "\x1b]8;id=smd0;https://e.com\x1b\\{erase}\x1b]66;s=2:w=1;A\x1b\\\x1b]8;;\x1b\\"
            )
        );
        let two = NonZeroU16::new(2).unwrap();
        assert_eq!(buf[(20, 0)].diff_option, CellDiffOption::ForcedWidth(two));
        for (x, y) in [(21, 0), (20, 1), (21, 1)] {
            assert_eq!(buf[(x, y)].diff_option, CellDiffOption::Skip, "({x}, {y})");
        }
        assert_eq!(buf[(0, 0)].diff_option, CellDiffOption::None);
        // A click on any of its cells finds the link.
        assert!(matches!(v.stop_at(21, 1), Some(Stop::Link(0))));
        assert!(v.stop_at(1, 1).is_none());
    }

    #[test]
    fn a_click_finds_a_link_moved_by_max_width() {
        let (mut b, dir) = browser(
            "narrow",
            &[("HOME.md", "go [[Plan]] now\n"), ("Plan.md", "# Plan\n")],
        );
        b.view.max_width = Some(20);
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal.draw(|f| b.draw(f)).unwrap();
        assert_eq!(b.view.left, 20);
        // The status row keeps the whole width: the page sits at its right end.
        let buf = terminal.backend().buffer();
        let status: String = (0..60).map(|x| buf[(x, 5)].symbol().to_string()).collect();
        assert!(status.trim_end().len() > 50, "{status:?}");
        assert_eq!(buf[(0, 5)].style(), buf[(59, 5)].style());
        // A search match is marked where its text is drawn.
        for code in "/now".chars().map(KeyCode::Char).chain([KeyCode::Enter]) {
            enter(&mut b, code);
        }
        terminal.draw(|f| b.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let marked: Vec<(u16, String)> = (0..60)
            .filter(|&x| buf[(x, 1)].bg == Color::Yellow)
            .map(|x| (x, buf[(x, 1)].symbol().to_string()))
            .collect();
        let (xs, text): (Vec<u16>, String) = marked.into_iter().unzip();
        assert_eq!(text, "now");
        assert!(xs.iter().all(|x| (20..40).contains(x)), "{xs:?}");
        let click = |column| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        // Where the link would be without the limit, nothing happens.
        b.mouse(click(4));
        assert_eq!(b.path, dir.join("HOME.md"));
        b.mouse(click(24));
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
    fn a_note_read_from_stdin_is_shown_with_its_tasks_locked() {
        // As smd makes it: named stdin, its links read from here.
        let path = PathBuf::from("stdin");
        let vault = Vault::for_file(&path);
        let source = "- [ ] one [[README]]\n";
        let doc = super::parse(&vault, source, &path);
        let mut view = View::new(doc, folder(&path), Images::new((8, 16)));
        view.resize(40, 6);
        let watcher = Watcher::new(path.clone());
        let mut b = Browser::new(view, path.clone(), watcher, vault, Config::default()).piped();
        assert!(!b.view.tasks);
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        terminal.draw(|f| b.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let bar: String = (0..40).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(bar.trim_end().ends_with(" stdin"), "{bar}");
        // Tab passes over the checkbox to the link, which leads to a file
        // here, where tasks can be ticked again.
        enter(&mut b, KeyCode::Tab);
        assert_eq!(b.view.selected, Some(Stop::Link(0)));
        enter(&mut b, KeyCode::Enter);
        assert_eq!(b.path, PathBuf::from("./README.md"));
        assert!(!b.piped);
        // Back shows the text read from stdin again, still locked.
        b.back();
        assert_eq!(b.path, path);
        assert!(b.piped && !b.view.tasks);
        assert_eq!(b.view.doc.source, source);
    }

    #[test]
    fn question_mark_opens_the_help_with_the_keys_as_bound_and_back_returns() {
        let (mut b, dir) = browser("help", &[("a.md", &paragraphs(30))]);
        b.config = Config::from_str("[keys]\ndown = [\"J\", \"j\"]").unwrap();
        press_all(&mut b, "5j");
        assert_eq!(b.view.scroll, 5);
        press_all(&mut b, "?");
        let source = b.view.doc.source.clone();
        assert!(source.contains("| `J`, `j` | `down` |"), "{source}");
        assert!(b.toolbar().to_string().trim_end().ends_with(" help"));
        assert_eq!(b.view.scroll, 0);
        // It is a note like any other: it scrolls and can be searched.
        press_all(&mut b, "J");
        assert_eq!(b.view.scroll, 1);
        press_all(&mut b, "/reload");
        enter(&mut b, KeyCode::Enter);
        assert!(status_row(&mut b).starts_with("match 1/"));
        // But it has no file: nothing to reload, and no tasks.
        press_all(&mut b, "r");
        assert!(status_row(&mut b).starts_with("nothing to reload"));
        assert!(!b.view.tasks);
        // Asking again does not stack another page to go back through.
        press_all(&mut b, "?");
        assert_eq!(b.history.len(), 1);
        enter(&mut b, KeyCode::Backspace);
        assert_eq!(b.path, dir.join("a.md"));
        assert_eq!(b.view.scroll, 5);
        assert!(!b.piped);
        // Forward shows it again, from where it was left.
        enter(&mut b, KeyCode::Char('L'));
        assert_eq!(b.view.doc.source, source);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn r_on_a_note_read_from_stdin_says_there_is_nothing_to_reload() {
        let (b, dir) = browser("reload-piped", &[("stdin", "piped\n")]);
        let mut b = b.piped();
        // A file of the same name is not read in its place.
        std::fs::write(&b.path, "on disk\n").unwrap();
        press_all(&mut b, "r");
        assert_eq!(top(&b.view), "piped");
        assert!(status_row(&mut b).starts_with("nothing to reload"));
        let _ = std::fs::remove_dir_all(dir);
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

    #[test]
    fn the_search_prompt_keeps_up_and_lets_go() {
        let md = format!(
            "> [!note]- Shut\n> needle\n\nneedle a\n\nneedle b\n\n{}",
            paragraphs(6)
        );
        let (mut b, dir) = browser("prompt", &[("note.md", &md), ("Plan.md", "# Plan\n")]);
        let search = |b: &mut Browser, term: &str| {
            enter(b, KeyCode::Char('/'));
            for c in term.chars() {
                enter(b, KeyCode::Char(c));
            }
        };
        // Ctrl-C gives the prompt up.
        search(&mut b, "needle");
        b.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(
            (b.view.prompt.clone(), b.view.search.active()),
            (None, false)
        );
        // Opening the callout while typing updates the count on the prompt.
        search(&mut b, "needle");
        assert_eq!(b.view.prompt.as_deref(), Some("/needle  2 matches"));
        let at = b.view.layout.folds[0].at;
        b.view.fold(at);
        assert_eq!(b.view.prompt.as_deref(), Some("/needle  3 matches"));
        b.view.fold(at);
        enter(&mut b, KeyCode::Enter);
        enter(&mut b, KeyCode::Char('n'));
        assert_eq!(b.view.notice.as_deref(), Some("match 2/2"));
        // A new layout keeps the reader on the same match, "needle b",
        // though it is the third match now, not the second.
        b.view.fold(at);
        assert_eq!(b.view.search.current, Some(2));
        enter(&mut b, KeyCode::Char('/'));
        assert_eq!(b.view.search.current, None, "a new search starts afresh");
        enter(&mut b, KeyCode::Esc);
        // Another note starts with no search.
        b.follow(&wiki("Plan"));
        assert!(!b.view.search.active());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_number_before_n_or_n_steps_that_many_matches() {
        // Five matches, all on the first row, so all in view.
        let (mut b, dir) = browser("count-match", &[("a.md", "ab ab ab ab ab\n")]);
        let notice = |b: &Browser| b.view.notice.clone().unwrap_or_default();
        press_all(&mut b, "/ab");
        enter(&mut b, KeyCode::Enter);
        assert_eq!(notice(&b), "match 1/5");
        let mut steps = Vec::new();
        for keys in ["3n", "3n", "3N", "2N", "12n", "1n"] {
            press_all(&mut b, keys);
            steps.push(notice(&b));
        }
        assert_eq!(
            steps,
            [
                "match 4/5",
                "match 2/5, wrapped to the top",
                "match 4/5, wrapped to the bottom",
                "match 2/5",
                "match 4/5, wrapped to the top",
                "match 5/5",
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_click_while_typing_a_search_is_passed_over() {
        let (mut b, dir) = browser(
            "click-search",
            &[("HOME.md", "go [[Plan]] now\n"), ("Plan.md", "# Plan\n")],
        );
        enter(&mut b, KeyCode::Char('/'));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        b.mouse(click);
        assert_eq!(b.path, dir.join("HOME.md"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn wiki(dest: &str) -> Link {
        Link {
            dest: dest.into(),
            wiki: true,
        }
    }

    /// The bottom row of the screen after drawing `b` 30 wide and 8 high.
    fn status_row(b: &mut Browser) -> String {
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        terminal.draw(|f| b.draw(f)).unwrap();
        rows(terminal.backend().buffer()).pop().unwrap()
    }

    fn press_all(b: &mut Browser, keys: &str) {
        for c in keys.chars() {
            enter(b, KeyCode::Char(c));
        }
    }

    #[test]
    fn a_number_before_a_key_repeats_it_or_goes_to_that_line() {
        let (mut b, dir) = browser("count", &[("a.md", &paragraphs(30))]);
        assert_eq!(status_row(&mut b).trim(), "page 1/10");
        press_all(&mut b, "5j");
        assert_eq!(b.view.scroll, 5);
        press_all(&mut b, "2k");
        assert_eq!(b.view.scroll, 3);
        // The line counts from 1, so line 42 is the 41st below the top.
        press_all(&mut b, "42G");
        assert_eq!(b.view.scroll, 41);
        press_all(&mut b, "1g");
        assert_eq!(b.view.scroll, 0);
        // The number typed so far shows on the bottom row.
        press_all(&mut b, "12");
        assert!(status_row(&mut b).ends_with(" 12  page 1/10 "));
        // Digits after it go on the end, until a key uses it.
        press_all(&mut b, "3j");
        assert_eq!(b.view.scroll, 53);
        assert_eq!(b.view.count, None);
        // Esc drops the number, and does not quit.
        press_all(&mut b, "gg4");
        assert!(!b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(b.view.count, None);
        press_all(&mut b, "j");
        assert_eq!(b.view.scroll, 1);
        // A number does not start with 0, and is held below the limit.
        press_all(&mut b, "0");
        assert_eq!(b.view.count, None);
        press_all(&mut b, "10");
        assert_eq!(b.view.count, Some(10));
        press_all(&mut b, &"9".repeat(12));
        assert!(b.view.count.unwrap() < COUNT_LIMIT);
        // Esc with no number still quits.
        b.view.count = None;
        assert!(b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn digits_typed_into_the_search_prompt_are_not_a_number() {
        let (mut b, dir) = browser("count-search", &[("a.md", &paragraphs(30))]);
        press_all(&mut b, "/12");
        assert_eq!(b.view.count, None);
        assert!(b.view.prompt.as_deref().unwrap().starts_with("/12"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_digit_bound_to_an_action_is_not_a_number() {
        let (mut b, dir) = browser("count-bound", &[("a.md", &paragraphs(30))]);
        b.config = Config::from_str("[keys]\ndown = [\"2\"]").unwrap();
        press_all(&mut b, "2");
        assert_eq!(b.view.scroll, 1);
        assert_eq!(b.view.count, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn r_reloads_the_file_and_says_when_it_cannot() {
        let (mut b, dir) = browser("reload", &[("a.md", "old\n")]);
        std::fs::write(&b.path, "new\n").unwrap();
        press_all(&mut b, "r");
        assert_eq!(top(&b.view), "new");
        std::fs::remove_file(&b.path).unwrap();
        press_all(&mut b, "r");
        assert_eq!(top(&b.view), "new");
        assert!(status_row(&mut b).starts_with("cannot reload: "));
        // The message goes at the next key.
        press_all(&mut b, "j");
        assert_eq!(status_row(&mut b).trim(), "page 1/1");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn r_picks_up_a_change_to_an_embedded_note() {
        let (mut b, dir) = browser("reload-embed", &[("a.md", "![[b]]\n"), ("b.md", "old\n")]);
        std::fs::write(dir.join("b.md"), "new\n").unwrap();
        press_all(&mut b, "r");
        let text: Vec<String> = b.view.layout.lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("new")), "{text:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_reload_leaves_the_deleted_notice_after_the_next_key() {
        let (mut b, dir) = browser("reload-deleted", &[("a.md", "text\n")]);
        std::fs::remove_file(&b.path).unwrap();
        apply(&mut b.view, &b.path, &b.vault, Change::Deleted);
        press_all(&mut b, "r");
        assert!(status_row(&mut b).starts_with("cannot reload: "));
        press_all(&mut b, "j");
        assert!(status_row(&mut b).starts_with("file deleted"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_message_shows_when_the_note_has_one_row() {
        let (mut b, dir) = browser("one-row", &[("a.md", "[[Gone]]\n")]);
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        let mut terminal = Terminal::new(TestBackend::new(30, 2)).unwrap();
        terminal.draw(|f| b.draw(f)).unwrap();
        let row = rows(terminal.backend().buffer()).pop().unwrap();
        assert!(row.starts_with("no note found"), "{row:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_error_shows_on_the_bottom_row_until_the_next_key() {
        let (mut b, dir) = browser("error", &[("a.md", "[[Gone]]\n")]);
        enter(&mut b, KeyCode::Tab);
        enter(&mut b, KeyCode::Enter);
        let row = status_row(&mut b);
        assert!(row.starts_with("no note found: Gone"), "{row:?}");
        // The page is still shown when both fit.
        assert!(row.ends_with(" page 1/1 "), "{row:?}");
        // Any key clears it, even one that does nothing.
        enter(&mut b, KeyCode::Char('z'));
        assert_eq!(status_row(&mut b).trim(), "page 1/1");
        let _ = std::fs::remove_dir_all(dir);
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
    /// until something new is drawn, for work on other threads, or a
    /// signal asking smd to stop.
    enum Step {
        After(Duration, Event),
        Wait,
        Drawn,
        Stop,
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
                Some(Step::Stop) => panic!("polled after a signal to stop"),
                None => Err(io::Error::other("the script ran out")),
            }
        }

        fn stopped(&self) -> bool {
            matches!(self.steps.front(), Some(Step::Stop))
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
    fn a_signal_to_stop_ends_the_loop_like_a_quit_key() {
        let (mut b, dir) = loaded("loop-stop", &[("a.md", "one\n")]);
        let (result, frames, timeouts) = run_script(&mut b, vec![Step::Wait, Step::Stop]);
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
    fn the_loop_does_not_reload_a_note_read_from_stdin() {
        let (b, dir) = loaded("loop-piped", &[("a.md", "one\n")]);
        let mut b = b.piped();
        // A file at the same path is not the note shown.
        std::fs::write(dir.join("a.md"), "something else\n").unwrap();
        let steps = vec![Step::Wait, Step::Wait, Step::Wait, quit()];
        let (result, frames, _) = run_script(&mut b, steps);
        assert!(result.is_ok());
        assert_eq!(frames, 1);
        assert_eq!(top(&b.view), "one");
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
    fn the_loop_sends_an_image_before_the_cells_that_show_it() {
        use crate::images::tests::png;
        let (mut b, dir) = loaded("loop-image-first", &[("a.md", "one\n")]);
        std::fs::write(dir.join("a.png"), png(20, 100)).unwrap();
        b.view.show(parse("![a](a.png)\n"), dir.clone());
        let out = Shared::default();
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 30, 7)),
        };
        let mut terminal =
            Terminal::with_options(CrosstermBackend::new(out.clone()), options).unwrap();
        // Checks for two seconds after the image is drawn.
        let mut steps = vec![Step::Drawn];
        steps.extend((0..14).map(|_| Step::Wait));
        steps.push(quit());
        let mut script = Script {
            now: Instant::now(),
            steps: steps.into(),
            timeouts: Vec::new(),
            out: out.clone(),
            sent: None,
        };
        assert!(run_browser(&mut terminal, &mut b, &mut script).is_ok());
        let sent = String::from_utf8(out.0.borrow().clone()).unwrap();
        let image = sent.find("\x1b_G").expect("no image sent");
        let cells = sent.find('\u{10EEEE}').expect("no placeholder cells");
        assert!(image < cells, "{sent:?}");
        // kitty may finish the image after it drew the cells, so it is asked
        // to repaint at each check for a second after, and then no more.
        let repaint = "\x1b[?25h\x1b[?25l";
        assert!(sent.rfind(repaint).is_some_and(|r| r > cells), "{sent:?}");
        let repaints = sent.matches(repaint).count();
        assert!((5..=7).contains(&repaints), "{repaints} repaints");
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

    #[cfg(unix)]
    #[test]
    fn a_signal_is_seen_as_ignored_only_when_it_is() {
        // SIGUSR2 stands in for SIGHUP under nohup; nothing else uses it.
        // SAFETY: sets and reads one signal's disposition.
        unsafe { libc::signal(libc::SIGUSR2, libc::SIG_IGN) };
        assert!(ignored(libc::SIGUSR2));
        unsafe { libc::signal(libc::SIGUSR2, libc::SIG_DFL) };
        assert!(!ignored(libc::SIGUSR2));
    }

    #[cfg(unix)]
    #[test]
    fn output_is_taken_until_the_reader_stops_reading() {
        // A pipe nobody reads stands in for a terminal that stopped reading.
        let mut fds = [0; 2];
        let wait = Duration::from_millis(20);
        // SAFETY: pipe writes two descriptors into fds; fcntl only sets a
        // flag; the writes are from a local; both descriptors are ours and
        // closed once.
        unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            let flags = libc::fcntl(fds[1], libc::F_GETFL);
            libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
            assert!(takes_output(fds[1], wait).unwrap());
            let chunk = [0u8; 4096];
            while libc::write(fds[1], chunk.as_ptr().cast(), chunk.len()) > 0 {}
            assert!(!takes_output(fds[1], wait).unwrap());
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
