//! Throwaway spike for smd#2: do kitty graphics through Unicode placeholders
//! work well enough inside a herdr pane? This is not part of the smd binary.
//!
//! It shows scrolling text with a heading drawn as an image, a mermaid PNG and
//! an OSC 66 heading. Both images are uploaded once, placed with `U=1` virtual
//! placements and drawn as U+10EEEE placeholder cells, so scrolling only
//! redraws text.
//!
//! Run: `cargo run --example spike -- [--bench N] [--delay MS] [--log FILE]
//! [--no-images] [--reupload]`
//! Keys: j/k or arrows scroll a line, space/b a page, g/G top/bottom,
//! r re-uploads both images, q quits.

mod diacritics;

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use diacritics::DIACRITICS;
use font8x8::{BASIC_FONTS, UnicodeFonts};

const DIAGRAM_PNG: &[u8] = include_bytes!("diagram.png");
const HEADING: &str = "smd spike: a heading drawn as an image";
const PLACEHOLDER: char = '\u{10EEEE}';
const HEADING_ID: u32 = 0x5A_00_01;
const DIAGRAM_ID: u32 = 0x5A_00_02;
const CHUNK: usize = 4096;
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

struct RawMode(libc::termios);

impl RawMode {
    fn enable() -> io::Result<Self> {
        // SAFETY: plain termios calls on stdin with a zeroed, then filled, struct.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawMode(orig))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the settings read in `enable`.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}

fn winsize() -> libc::winsize {
    // SAFETY: TIOCGWINSZ fills a winsize struct; on failure it stays zeroed.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        libc::ioctl(1, libc::TIOCGWINSZ, &mut ws);
        ws
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Char(u8),
    Up,
    Down,
    PageUp,
    PageDown,
}

enum Event {
    Key(Key),
    /// Reply to `CSI 5 n`, used to time how long the terminal took to get here.
    Dsr,
    /// Reply to `CSI 16 t`: cell height and width in pixels.
    CellSize(u16, u16),
    /// A kitty graphics reply, without the leading `G`.
    Gfx(String),
}

#[derive(Default)]
struct Input {
    buf: Vec<u8>,
    queued: VecDeque<Event>,
}

impl Input {
    fn fill(&mut self, timeout: Duration) {
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            return;
        }
        let mut chunk = [0u8; 4096];
        // SAFETY: reads at most chunk.len() bytes into chunk.
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n > 0 {
            self.buf.extend_from_slice(&chunk[..n as usize]);
        }
        self.parse();
    }

    fn parse(&mut self) {
        while let Some(&first) = self.buf.first() {
            if first != 0x1b {
                self.buf.remove(0);
                self.queued.push_back(Event::Key(Key::Char(first)));
                continue;
            }
            if self.buf.len() < 2 {
                return;
            }
            match self.buf[1] {
                b'[' => {
                    let Some(end) = self.buf[2..].iter().position(|b| (0x40..=0x7e).contains(b))
                    else {
                        return;
                    };
                    let params = String::from_utf8_lossy(&self.buf[2..2 + end]).into_owned();
                    let fin = self.buf[2 + end];
                    self.buf.drain(..3 + end);
                    if let Some(ev) = csi_event(&params, fin) {
                        self.queued.push_back(ev);
                    }
                }
                b'_' => {
                    let Some(end) = self.buf[2..].windows(2).position(|w| w == b"\x1b\\") else {
                        return;
                    };
                    let body = String::from_utf8_lossy(&self.buf[2..2 + end]).into_owned();
                    self.buf.drain(..4 + end);
                    if let Some(g) = body.strip_prefix('G') {
                        self.queued.push_back(Event::Gfx(g.to_string()));
                    }
                }
                _ => {
                    self.buf.remove(0);
                }
            }
        }
    }

    /// Waits for the first event matching `want`, keeping the others queued.
    fn wait_for(&mut self, timeout: Duration, want: impl Fn(&Event) -> bool) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = self.queued.iter().position(&want) {
                return self.queued.remove(i);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.fill(deadline - now);
        }
    }

    fn take_gfx(&mut self) -> Vec<String> {
        let mut gfx = Vec::new();
        self.queued.retain(|e| match e {
            Event::Gfx(g) => {
                gfx.push(g.clone());
                false
            }
            _ => true,
        });
        gfx
    }
}

fn csi_event(params: &str, fin: u8) -> Option<Event> {
    match (fin, params) {
        (b'n', "0") => Some(Event::Dsr),
        (b'A', _) => Some(Event::Key(Key::Up)),
        (b'B', _) => Some(Event::Key(Key::Down)),
        (b'~', "5") => Some(Event::Key(Key::PageUp)),
        (b'~', "6") => Some(Event::Key(Key::PageDown)),
        (b't', p) => {
            let mut it = p.split(';');
            if it.next()? != "6" {
                return None;
            }
            let h = it.next()?.parse().ok()?;
            let w = it.next()?.parse().ok()?;
            Some(Event::CellSize(h, w))
        }
        _ => None,
    }
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn png_size(png: &[u8]) -> (u32, u32) {
    let be = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]);
    (be(16), be(20))
}

/// Draws `text` with the 8x8 font, each font pixel `scale` pixels square.
fn render_heading(text: &str, scale: u32) -> (u32, u32, Vec<u8>) {
    let glyph = 8 * scale;
    let w = glyph * text.chars().count() as u32;
    let h = glyph;
    let mut px = vec![0u8; (w * h * 4) as usize];
    let lerp = |a: u8, b: u8, t: f32| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
    for (i, ch) in text.chars().enumerate() {
        let Some(rows) = BASIC_FONTS.get(ch) else {
            continue;
        };
        for (gy, bits) in rows.iter().enumerate() {
            for gx in 0..8 {
                if bits & (1 << gx) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = i as u32 * glyph + gx * scale + dx;
                        let y = gy as u32 * scale + dy;
                        let t = x as f32 / w as f32;
                        let o = ((y * w + x) * 4) as usize;
                        px[o..o + 4].copy_from_slice(&[
                            lerp(0x7a, 0xbb, t),
                            lerp(0xa2, 0x9a, t),
                            0xf7,
                            0xff,
                        ]);
                    }
                }
            }
        }
    }
    (w, h, px)
}

/// Cells needed to show a `px` image at its natural size, shrunk to fit.
fn fit(px: (u32, u32), cell: (u16, u16), max_cols: u16, max_rows: u16) -> (u16, u16) {
    let (cw, ch) = (u32::from(cell.0.max(1)), u32::from(cell.1.max(1)));
    let limit = DIACRITICS.len() as u32;
    let max_cols = u32::from(max_cols).clamp(1, limit);
    let max_rows = u32::from(max_rows).clamp(1, limit);
    let mut cols = px.0.div_ceil(cw);
    let mut rows = px.1.div_ceil(ch);
    if cols > max_cols {
        cols = max_cols;
        rows = (u64::from(px.1) * u64::from(cols * cw)).div_ceil(u64::from(px.0 * ch)) as u32;
    }
    (
        cols.clamp(1, max_cols) as u16,
        rows.clamp(1, max_rows) as u16,
    )
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

struct Image {
    id: u32,
    name: &'static str,
    control: String,
    data: Vec<u8>,
    px: (u32, u32),
    cols: u16,
    rows: u16,
    upload_ms: Option<f64>,
    reply: String,
}

enum Line {
    Text(String),
    /// A heading sent with OSC 66 at scale 2; it covers this row and the next.
    Big(String),
    BigTail,
    Image {
        idx: usize,
        row: u16,
    },
}

const PARAGRAPH: &str = "This is ordinary text that stays text, so you can select and copy it. \
    smd keeps prose like this as plain characters and only turns headings, mermaid, maths and \
    wide tables into images. Scrolling moves these lines and the placeholder cells together; \
    the image data itself is uploaded once and never sent again.";

fn build_lines(width: u16, images: &[Image]) -> Vec<Line> {
    let width = usize::from(width.max(10));
    let mut lines = vec![Line::Big("OSC 66 heading".into()), Line::BigTail];
    let para = |lines: &mut Vec<Line>, n: usize| {
        lines.push(Line::Text(String::new()));
        let text = format!("Paragraph {n}. {PARAGRAPH}");
        lines.extend(wrap(&text, width).into_iter().map(Line::Text));
    };
    for n in 1..=3 {
        para(&mut lines, n);
    }
    // Two sections, each led by an image when there is one, so the text-only
    // control scrolls the same paragraphs.
    for idx in 0..2 {
        if let Some(img) = images.get(idx) {
            lines.push(Line::Text(String::new()));
            lines.extend((0..img.rows).map(|row| Line::Image { idx, row }));
        }
        for n in 0..12 {
            para(&mut lines, 4 + idx * 12 + n);
        }
    }
    lines
}

fn placeholder(s: &mut String, img: &Image, row: u16) {
    let id = img.id;
    let _ = write!(
        s,
        "\x1b[38;2;{};{};{}m",
        (id >> 16) & 0xff,
        (id >> 8) & 0xff,
        id & 0xff
    );
    for col in 0..img.cols {
        s.push(PLACEHOLDER);
        s.push(DIACRITICS[usize::from(row)]);
        s.push(DIACRITICS[usize::from(col)]);
    }
    s.push_str("\x1b[39m");
}

fn upload(out: &mut impl Write, img: &Image) -> io::Result<()> {
    let b64 = base64(&img.data);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(CHUNK).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            write!(out, "\x1b_G{},i={},m={more};", img.control, img.id)?;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.write_all(chunk)?;
        out.write_all(b"\x1b\\")?;
    }
    out.flush()
}

struct Stats {
    uploads: Vec<f64>,
    redraws: Vec<f64>,
    places: Vec<f64>,
    lost: usize,
    gfx: Vec<String>,
}

struct App {
    images: Vec<Image>,
    lines: Vec<Line>,
    top: usize,
    cols: u16,
    rows: u16,
    cell: (u16, u16),
    cell_src: &'static str,
    stats: Stats,
    input: Input,
}

impl App {
    fn view_rows(&self) -> usize {
        usize::from(self.rows.saturating_sub(1)).max(1)
    }

    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.view_rows())
    }

    /// Sends `CSI 5 n` and returns how long the reply took, in milliseconds.
    fn round_trip(&mut self, out: &mut impl Write, t0: Instant) -> io::Result<Option<f64>> {
        out.write_all(b"\x1b[5n")?;
        out.flush()?;
        let got = self
            .input
            .wait_for(REPLY_TIMEOUT, |e| matches!(e, Event::Dsr))
            .is_some();
        if !got {
            self.stats.lost += 1;
        }
        Ok(got.then(|| t0.elapsed().as_secs_f64() * 1000.0))
    }

    fn query_cell(&mut self, out: &mut impl Write, ws: &libc::winsize) -> io::Result<()> {
        out.write_all(b"\x1b[16t\x1b[5n")?;
        out.flush()?;
        let reply = self.input.wait_for(REPLY_TIMEOUT, |e| {
            matches!(e, Event::CellSize(..) | Event::Dsr)
        });
        if let Some(Event::CellSize(h, w)) = reply {
            // The DSR reply is still to come; drop it.
            self.input
                .wait_for(REPLY_TIMEOUT, |e| matches!(e, Event::Dsr));
            if h > 0 && w > 0 {
                (self.cell, self.cell_src) = ((w, h), "CSI 16 t");
                return Ok(());
            }
        }
        if ws.ws_xpixel > 0 && ws.ws_ypixel > 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            self.cell = (ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row);
            self.cell_src = "TIOCGWINSZ";
        } else {
            (self.cell, self.cell_src) = ((8, 16), "guess");
        }
        Ok(())
    }

    fn upload_all(&mut self, out: &mut impl Write) -> io::Result<()> {
        for i in 0..self.images.len() {
            let t0 = Instant::now();
            upload(out, &self.images[i])?;
            let prefix = format!("i={};", self.images[i].id);
            let reply = self.input.wait_for(
                REPLY_TIMEOUT,
                |e| matches!(e, Event::Gfx(g) if g.starts_with(&prefix)),
            );
            let img = &mut self.images[i];
            match reply {
                Some(Event::Gfx(g)) => {
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    self.stats.uploads.push(ms);
                    img.upload_ms = Some(ms);
                    img.reply = g;
                }
                _ => {
                    img.upload_ms = None;
                    img.reply = "no reply".into();
                }
            }
        }
        Ok(())
    }

    /// Reads the window size, works out image cells and places the images.
    fn relayout(&mut self, out: &mut impl Write) -> io::Result<()> {
        let ws = winsize();
        self.cols = ws.ws_col.max(20);
        self.rows = ws.ws_row.max(5);
        self.query_cell(out, &ws)?;
        let t0 = Instant::now();
        let max_rows = self.rows - 1;
        for img in &mut self.images {
            (img.cols, img.rows) = fit(img.px, self.cell, self.cols, max_rows);
            write!(
                out,
                "\x1b_Ga=p,U=1,i={},p=1,c={},r={},q=1\x1b\\",
                img.id, img.cols, img.rows
            )?;
        }
        if let Some(ms) = self.round_trip(out, t0)? {
            self.stats.places.push(ms);
        }
        self.lines = build_lines(self.cols, &self.images);
        self.top = self.top.min(self.max_top());
        Ok(())
    }

    fn status(&self) -> String {
        let up = |img: &Image| match img.upload_ms {
            Some(ms) => format!("{} {ms:.1}ms", img.name),
            None => format!("{} {}", img.name, img.reply),
        };
        let last = |v: &[f64]| v.last().map_or("-".into(), |ms| format!("{ms:.1}ms"));
        let s = format!(
            " line {}/{} | upload {} | redraw {} | place {} | cell {}x{} ({}) | j/k space/b r q",
            self.top,
            self.max_top(),
            self.images.iter().map(up).collect::<Vec<_>>().join(", "),
            last(&self.stats.redraws),
            last(&self.stats.places),
            self.cell.0,
            self.cell.1,
            self.cell_src,
        );
        s.chars().take(usize::from(self.cols)).collect()
    }

    fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        let view = self.view_rows();
        let mut s = String::from("\x1b[?2026h");
        let mut prev_big = false;
        for i in 0..view {
            let y = i + 1;
            match self.lines.get(self.top + i) {
                Some(Line::Big(t)) if i + 1 < view => {
                    let t: String = t.chars().take(usize::from(self.cols) / 2).collect();
                    let _ = write!(
                        s,
                        "\x1b[{y};1H\x1b[2K\x1b[{};1H\x1b[2K\x1b[{y};1H\x1b]66;s=2;{t}\x07",
                        y + 1
                    );
                    prev_big = true;
                    continue;
                }
                Some(Line::BigTail) if prev_big => {}
                line => {
                    let _ = write!(s, "\x1b[{y};1H\x1b[2K");
                    match line {
                        Some(Line::Text(t) | Line::Big(t)) => s.push_str(t),
                        Some(Line::Image { idx, row }) => {
                            placeholder(&mut s, &self.images[*idx], *row)
                        }
                        _ => {}
                    }
                }
            }
            prev_big = false;
        }
        let _ = write!(
            s,
            "\x1b[{};1H\x1b[2K\x1b[7m{}\x1b[0m",
            self.rows,
            self.status()
        );
        s.push_str("\x1b[?2026l");
        let t0 = Instant::now();
        out.write_all(s.as_bytes())?;
        if let Some(ms) = self.round_trip(out, t0)? {
            self.stats.redraws.push(ms);
        }
        self.stats.gfx.extend(self.input.take_gfx());
        Ok(())
    }

    fn scroll(&mut self, key: Key) {
        let page = self.view_rows();
        self.top = match key {
            Key::Char(b'j') | Key::Down => self.top + 1,
            Key::Char(b'k') | Key::Up => self.top.saturating_sub(1),
            Key::Char(b' ') | Key::PageDown => self.top + page,
            Key::Char(b'b') | Key::PageUp => self.top.saturating_sub(page),
            Key::Char(b'g') => 0,
            Key::Char(b'G') => usize::MAX,
            _ => self.top,
        }
        .min(self.max_top());
    }
}

struct Opts {
    bench: Option<usize>,
    delay: Duration,
    log: Option<String>,
    /// Text only, as a control for the image runs.
    no_images: bool,
    /// Re-upload both images every bench frame, as terminal-browser did.
    reupload: bool,
}

fn opts() -> Opts {
    let mut o = Opts {
        bench: None,
        delay: Duration::ZERO,
        log: None,
        no_images: false,
        reupload: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bench" => o.bench = args.next().and_then(|v| v.parse().ok()),
            "--delay" => {
                o.delay =
                    Duration::from_millis(args.next().and_then(|v| v.parse().ok()).unwrap_or(0))
            }
            "--log" => o.log = args.next(),
            "--no-images" => o.no_images = true,
            "--reupload" => o.reupload = true,
            _ => {}
        }
    }
    o
}

fn summary(app: &App, total: Duration) -> String {
    let mut s = String::new();
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "-".into());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let _ = writeln!(s, "args: {}", args.join(" "));
    let _ = writeln!(
        s,
        "terminal: TERM={} TERM_PROGRAM={} HERDR_ENV={}",
        env("TERM"),
        env("TERM_PROGRAM"),
        env("HERDR_ENV")
    );
    let _ = writeln!(
        s,
        "size: {}x{} cells, cell {}x{}px ({})",
        app.cols, app.rows, app.cell.0, app.cell.1, app.cell_src
    );
    for img in &app.images {
        let ms = img.upload_ms.map_or("-".into(), |ms| format!("{ms:.1}ms"));
        let _ = writeln!(
            s,
            "upload {}: {}x{}px, {} bytes sent as {}, {ms}, reply {:?}, placed as {}x{} cells",
            img.name,
            img.px.0,
            img.px.1,
            img.data.len(),
            img.control,
            img.reply,
            img.cols,
            img.rows
        );
    }
    let _ = writeln!(s, "uploads: {}", stats_line(&app.stats.uploads));
    let _ = writeln!(s, "place: {}", stats_line(&app.stats.places));
    let _ = writeln!(s, "redraw: {}", stats_line(&app.stats.redraws));
    let _ = writeln!(s, "lost replies: {}", app.stats.lost);
    let _ = writeln!(s, "graphics replies after upload: {:?}", app.stats.gfx);
    let _ = writeln!(s, "run time: {:.1}s", total.as_secs_f64());
    s
}

fn stats_line(v: &[f64]) -> String {
    if v.is_empty() {
        return "no samples".into();
    }
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!(
        "n={} min {:.2}ms median {:.2}ms p95 {:.2}ms max {:.2}ms",
        v.len(),
        v[0],
        at(0.5),
        at(0.95),
        v[v.len() - 1]
    )
}

fn main() -> io::Result<()> {
    let opts = opts();
    let start = Instant::now();
    let raw = RawMode::enable()?;
    let mut out = io::stdout().lock();
    out.write_all(b"\x1b[?1049h\x1b[?25l")?;

    let mut app = App {
        images: Vec::new(),
        lines: Vec::new(),
        top: 0,
        cols: 80,
        rows: 24,
        cell: (8, 16),
        cell_src: "guess",
        stats: Stats {
            uploads: Vec::new(),
            redraws: Vec::new(),
            places: Vec::new(),
            lost: 0,
            gfx: Vec::new(),
        },
        input: Input::default(),
    };
    app.query_cell(&mut out, &winsize())?;

    // A heading two text rows tall, drawn at the cell size seen at start.
    let scale = (u32::from(app.cell.1) * 2 / 8).max(1);
    let (w, h, rgba) = render_heading(HEADING, scale);
    let image = |id, name, control: String, data: Vec<u8>, px| Image {
        id,
        name,
        control,
        data,
        px,
        cols: 1,
        rows: 1,
        upload_ms: None,
        reply: String::new(),
    };
    app.images = vec![
        image(
            HEADING_ID,
            "heading",
            format!("a=t,f=32,s={w},v={h},q=0"),
            rgba,
            (w, h),
        ),
        image(
            DIAGRAM_ID,
            "diagram",
            "a=t,f=100,q=0".into(),
            DIAGRAM_PNG.to_vec(),
            png_size(DIAGRAM_PNG),
        ),
    ];
    if opts.no_images {
        app.images.clear();
    }
    app.upload_all(&mut out)?;
    app.relayout(&mut out)?;
    app.draw(&mut out)?;

    let mut size = (app.cols, app.rows);
    if let Some(frames) = opts.bench {
        let mut down = true;
        for _ in 0..frames {
            std::thread::sleep(opts.delay);
            if app.max_top() == 0 {
                break;
            }
            if app.top == app.max_top() {
                down = false;
            } else if app.top == 0 {
                down = true;
            }
            app.scroll(if down { Key::Down } else { Key::Up });
            let ws = winsize();
            if opts.reupload || (ws.ws_col, ws.ws_row) != size {
                if opts.reupload {
                    app.upload_all(&mut out)?;
                }
                app.relayout(&mut out)?;
                size = (app.cols, app.rows);
            }
            app.draw(&mut out)?;
        }
    } else {
        loop {
            let key = app
                .input
                .wait_for(Duration::from_millis(100), |e| matches!(e, Event::Key(_)));
            let ws = winsize();
            if (ws.ws_col, ws.ws_row) != size {
                app.relayout(&mut out)?;
                size = (app.cols, app.rows);
                app.draw(&mut out)?;
            }
            match key {
                Some(Event::Key(Key::Char(b'q'))) => break,
                Some(Event::Key(Key::Char(b'r'))) => {
                    app.upload_all(&mut out)?;
                    app.relayout(&mut out)?;
                    app.draw(&mut out)?;
                }
                Some(Event::Key(k)) => {
                    app.scroll(k);
                    app.draw(&mut out)?;
                }
                _ => {}
            }
        }
    }

    for img in &app.images {
        write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", img.id)?;
    }
    out.write_all(b"\x1b[?25h\x1b[?1049l")?;
    out.flush()?;
    drop(raw);

    let report = summary(&app, start.elapsed());
    print!("{report}");
    if let Some(path) = opts.log {
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{report}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            assert_eq!(base64(input.as_bytes()), want);
        }
    }

    #[test]
    fn fit_shrinks_wide_images_keeping_aspect() {
        assert_eq!(fit((160, 32), (8, 16), 80, 40), (20, 2));
        assert_eq!(fit((1600, 320), (8, 16), 100, 40), (100, 10));
    }
}
