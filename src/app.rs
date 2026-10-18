//! The full-screen view: a scrollable window onto the laid-out document.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::doc::{self, Document};
use crate::render::render;
use crate::watch::{Change, Watcher};

/// How often to look at the file on disk. A change shows after two checks.
const CHECK_EVERY: Duration = Duration::from_millis(150);

pub struct View {
    doc: Document,
    lines: Vec<Line<'static>>,
    /// Width the lines were laid out for; zero before the first layout.
    width: u16,
    height: u16,
    scroll: usize,
    /// A message shown on the bottom row, such as the file being deleted.
    notice: Option<String>,
}

impl View {
    pub fn new(doc: Document) -> Self {
        View {
            doc,
            lines: Vec::new(),
            width: 0,
            height: 0,
            scroll: 0,
            notice: None,
        }
    }

    /// Show a new version of the document, keeping the same line number at the
    /// top of the view.
    pub fn reload(&mut self, doc: Document) {
        self.doc = doc;
        if self.width > 0 {
            self.lines = render(&self.doc, self.width);
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Lay the document out again when the size changes, keeping roughly the
    /// same part of it in view.
    fn resize(&mut self, width: u16, height: u16) {
        if width != self.width {
            let old_len = self.lines.len().max(1);
            self.lines = render(&self.doc, width);
            self.scroll = self.scroll * self.lines.len() / old_len;
            self.width = width;
        }
        self.height = height;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.height as usize)
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll());
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let mut area = frame.area();
        if let Some(notice) = &self.notice
            && area.height > 0
        {
            area.height -= 1;
            let row = Rect::new(area.x, area.y + area.height, area.width, 1);
            let style = Style::new().add_modifier(Modifier::REVERSED);
            frame.render_widget(Paragraph::new(notice.as_str()).style(style), row);
        }
        self.resize(area.width, area.height);
        let end = (self.scroll + area.height as usize).min(self.lines.len());
        let visible = Text::from(self.lines[self.scroll..end].to_vec());
        frame.render_widget(Paragraph::new(visible), area);
    }

    /// Handle a key press. Returns true when the reader should quit.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let page = (self.height as isize).max(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll = self.max_scroll(),
            _ => {}
        }
        false
    }
}

/// Apply what the watcher saw to the view.
fn apply(view: &mut View, path: &Path, change: Change) {
    match change {
        Change::Changed(source) => {
            view.reload(doc::parse(&source));
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

/// Run the view until the reader quits, reloading it when the watched file at
/// `path` changes. Mouse capture stays off so the terminal's own selection
/// keeps working.
pub fn run(
    terminal: &mut DefaultTerminal,
    mut view: View,
    path: &Path,
    mut watcher: Watcher,
) -> io::Result<()> {
    let mut last_check = Instant::now();
    let mut redraw = true;
    loop {
        if redraw {
            terminal.draw(|frame| view.draw(frame))?;
            redraw = false;
        }
        if event::poll(CHECK_EVERY.saturating_sub(last_check.elapsed()))? {
            // A resize needs no handling beyond the redraw.
            redraw = true;
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && view.key(key)
            {
                return Ok(());
            }
        }
        if last_check.elapsed() >= CHECK_EVERY {
            last_check = Instant::now();
            if let Some(change) = watcher.check() {
                apply(&mut view, path, change);
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

    fn view(paragraphs: usize) -> View {
        let md: String = (0..paragraphs).map(|n| format!("line {n}\n\n")).collect();
        let mut v = View::new(parse(&md));
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
        let mut v = View::new(parse("alpha beta gamma\n"));
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal.draw(|f| v.draw(f)).unwrap();
        assert_eq!(v.lines.len(), 1);
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
        assert_eq!(v.lines.len(), 23);
        // A shorter document pulls the view back to its end.
        v.reload(parse("one\n\ntwo\n\nthree\n\nfour\n"));
        assert_eq!(v.scroll, 2);
    }

    #[test]
    fn notice_takes_the_bottom_row_and_clears() {
        let mut v = View::new(parse("one\n\ntwo\n"));
        let mut terminal = Terminal::new(TestBackend::new(8, 3)).unwrap();
        let path = Path::new("a.md");
        apply(&mut v, path, Change::Deleted);
        terminal.draw(|f| v.draw(f)).unwrap();
        assert_eq!(v.height, 2);
        let buf = terminal.backend().buffer();
        let row: String = (0..8).map(|x| buf[(x, 2)].symbol()).collect();
        assert_eq!(row, "file del");
        assert_eq!(buf[(0, 0)].symbol(), "o");
        apply(&mut v, path, Change::Changed("back\n".into()));
        terminal.draw(|f| v.draw(f)).unwrap();
        terminal
            .backend()
            .assert_buffer_lines(["back    ", "        ", "        "]);
    }
}
