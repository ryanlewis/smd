//! The full-screen view: a scrollable window onto the laid-out document.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::doc::{self, Document};
use crate::images::{self, Images};
use crate::render::{Layout, render};
use crate::watch::{Change, Watcher};

/// How often to look at the file on disk. A change shows after two checks.
const CHECK_EVERY: Duration = Duration::from_millis(150);

pub struct View {
    doc: Document,
    /// The directory relative image paths are read from.
    base: PathBuf,
    images: Images,
    layout: Layout,
    /// Width the lines were laid out for; zero before the first layout.
    width: u16,
    height: u16,
    scroll: usize,
    /// A message shown on the bottom row, such as the file being deleted.
    notice: Option<String>,
}

impl View {
    pub fn new(doc: Document, base: PathBuf, images: Images) -> Self {
        let mut view = View {
            doc,
            base,
            images,
            layout: Layout::default(),
            width: 0,
            height: 0,
            scroll: 0,
            notice: None,
        };
        view.want_images();
        view
    }

    /// Show a new version of the document, keeping the same line number at the
    /// top of the view. Images it no longer has are deleted on the next flush.
    pub fn reload(&mut self, doc: Document) {
        self.doc = doc;
        self.want_images();
        self.relayout();
    }

    fn want_images(&mut self) {
        let mut wanted = Vec::new();
        images::wanted_in(&self.doc.blocks, &self.base, &mut wanted);
        self.images.want(wanted);
    }

    fn relayout(&mut self) {
        if self.width > 0 {
            self.layout = render(&self.doc, self.width, &self.images);
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Take in images that became ready. Returns true when the view changed.
    pub fn receive_images(&mut self) -> bool {
        if !self.images.receive() {
            return false;
        }
        self.relayout();
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
            self.layout = render(&self.doc, width, &self.images);
            self.scroll = self.scroll * self.layout.lines.len() / old_len;
            self.width = width;
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
        let lines = &self.layout.lines;
        let end = (self.scroll + area.height as usize).min(lines.len());
        let visible = Text::from(lines[self.scroll..end].to_vec());
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
/// keeps working. Images are deleted from the terminal however the view ends.
pub fn run(
    terminal: &mut DefaultTerminal,
    mut view: View,
    path: &Path,
    watcher: Watcher,
) -> io::Result<()> {
    let result = run_view(terminal, &mut view, path, watcher);
    let deleted = view.images.delete_all(terminal.backend_mut());
    result.and(deleted)
}

fn run_view(
    terminal: &mut DefaultTerminal,
    view: &mut View,
    path: &Path,
    mut watcher: Watcher,
) -> io::Result<()> {
    let mut last_check = Instant::now();
    let mut redraw = true;
    loop {
        if redraw {
            terminal.draw(|frame| view.draw(frame))?;
            view.flush_images(terminal.backend_mut())?;
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
                apply(view, path, change);
                redraw = true;
            }
            // Images are made on other threads; this is where they show up.
            if view.receive_images() {
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
    fn notice_takes_the_bottom_row_and_clears() {
        let mut v = new_view("one\n\ntwo\n");
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
