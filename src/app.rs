//! The full-screen view: a scrollable window onto the laid-out document.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::doc::Document;
use crate::render::render;

pub struct View {
    doc: Document,
    lines: Vec<Line<'static>>,
    /// Width the lines were laid out for; zero before the first layout.
    width: u16,
    height: u16,
    scroll: usize,
}

impl View {
    pub fn new(doc: Document) -> Self {
        View {
            doc,
            lines: Vec::new(),
            width: 0,
            height: 0,
            scroll: 0,
        }
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
        let area = frame.area();
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

/// Run the view until the reader quits. Mouse capture stays off so the
/// terminal's own selection keeps working.
pub fn run(terminal: &mut DefaultTerminal, mut view: View) -> io::Result<()> {
    loop {
        terminal.draw(|frame| view.draw(frame))?;
        // A resize needs no handling beyond the redraw at the top of the loop.
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && view.key(key)
        {
            return Ok(());
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
}
