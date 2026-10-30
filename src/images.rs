//! Images in the text flow.
//!
//! A block that is drawn as an image names it with a [`Key`]. The store makes
//! each image once, off the main thread, and the layout reserves rows for it
//! once it is ready. The first time an image is laid out it is uploaded to the
//! terminal; after that only its placement changes, on resize. An image no
//! longer in the document is deleted from the terminal.
//!
//! Until an image is ready, or if making it fails, its block shows as text.
//! A store made with [`Images::text`], for a terminal without kitty graphics,
//! makes no images at all and sends the terminal nothing.
//!
//! A job that panics fails like any other, with a short error. Its thread is
//! named [`JOB_THREAD`] so a panic hook can tell its panics from the app's.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use resvg::usvg::fontdb::Database;

use crate::doc::{Block, BlockKind, FigureKind};
use crate::theme;
use crate::{heading, kitty, maths, mermaid, raster};

/// What an image is made from. Two blocks with the same key share one image.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key(String);

/// PNG bytes and the pixel size from their header.
#[derive(Debug, Clone, PartialEq)]
pub struct Png {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl Png {
    /// Check that `data` is a PNG and read its size.
    pub fn new(data: Vec<u8>) -> Result<Png, String> {
        const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
        if data.len() < 24 || !data.starts_with(SIGNATURE) || &data[12..16] != b"IHDR" {
            return Err("not a PNG".into());
        }
        let be =
            |at: usize| u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        let (width, height) = (be(16), be(20));
        if width == 0 || height == 0 {
            return Err("PNG has no pixels".into());
        }
        Ok(Png {
            data,
            width,
            height,
        })
    }
}

/// Makes an image. It runs on its own thread.
pub type Job = Box<dyn FnOnce() -> Result<Png, String> + Send>;

/// The name of the threads jobs run on.
pub const JOB_THREAD: &str = "smd-image";

/// True on a thread running a job.
pub fn in_job() -> bool {
    std::thread::current().name() == Some(JOB_THREAD)
}

/// Run `job`, turning a panic into a short error.
fn run(job: Job) -> Result<Png, String> {
    panic::catch_unwind(AssertUnwindSafe(job)).unwrap_or_else(|payload| Err(panicked(&*payload)))
}

fn panicked(payload: &(dyn Any + Send)) -> String {
    let message = match payload.downcast_ref::<&str>() {
        Some(message) => message,
        None => payload.downcast_ref::<String>().map_or("", String::as_str),
    };
    match message.lines().next() {
        Some(line) if !line.is_empty() => format!("crashed: {line}"),
        _ => "crashed".into(),
    }
}

/// The ids of the images in the terminal, shared so a panic hook can delete
/// them when the app cannot.
#[derive(Clone, Default)]
pub struct Uploaded(Arc<Mutex<HashSet<u32>>>);

impl Uploaded {
    /// Delete every image in the terminal.
    pub fn delete_all(&self, out: &mut impl Write) -> io::Result<()> {
        // A panic while the lock was held leaves the set as it was.
        let ids = match self.0.lock() {
            Ok(ids) => ids,
            Err(poisoned) => poisoned.into_inner(),
        };
        for &id in ids.iter() {
            kitty::delete(out, id)?;
        }
        out.flush()
    }

    fn set(&self, id: u32, uploaded: bool) {
        let mut ids = match self.0.lock() {
            Ok(ids) => ids,
            Err(poisoned) => poisoned.into_inner(),
        };
        if uploaded {
            ids.insert(id);
        } else {
            ids.remove(&id);
        }
    }
}

/// An image that is ready to lay out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ready {
    pub id: u32,
    /// Size in pixels.
    pub px: (u32, u32),
}

/// An image laid out `cols` by `rows` cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
}

enum State {
    Loading,
    /// The PNG, until it has been uploaded.
    Ready {
        png: Option<Vec<u8>>,
        px: (u32, u32),
    },
    /// Making it failed, for this reason.
    Failed(String),
}

struct Entry {
    id: u32,
    state: State,
    /// The size the terminal has the image placed at. Set once it has been
    /// uploaded, which happens just before it is first placed.
    placed: Option<(u16, u16)>,
}

pub struct Images {
    /// Cell size in pixels, width then height.
    cell: (u16, u16),
    /// False when the terminal has no kitty graphics: every block is text.
    graphics: bool,
    entries: HashMap<Key, Entry>,
    /// The directory of the note on screen, which the keys of its images
    /// are relative to.
    base: PathBuf,
    next_id: u32,
    /// Uploaded images that left the document, to delete on the next flush.
    deleted: Vec<u32>,
    uploaded: Uploaded,
    tx: Sender<(Key, Result<Png, String>)>,
    rx: Receiver<(Key, Result<Png, String>)>,
}

impl Images {
    pub fn new(cell: (u16, u16)) -> Self {
        let (tx, rx) = channel();
        Images {
            cell: (cell.0.max(1), cell.1.max(1)),
            graphics: true,
            entries: HashMap::new(),
            base: PathBuf::new(),
            // Start at a random id, so another program's images in the same
            // terminal, or another smd's, are unlikely to share one.
            next_id: (RandomState::new().hash_one(std::process::id()) % u64::from(kitty::MAX_ID))
                as u32
                + 1,
            deleted: Vec::new(),
            uploaded: Uploaded::default(),
            tx,
            rx,
        }
    }

    /// A store that never makes an image, for text mode.
    pub fn text() -> Self {
        Images {
            graphics: false,
            ..Images::new((1, 1))
        }
    }

    pub fn cell(&self) -> (u16, u16) {
        self.cell
    }

    pub fn graphics(&self) -> bool {
        self.graphics
    }

    /// The images this store has in the terminal.
    pub fn uploaded(&self) -> Uploaded {
        self.uploaded.clone()
    }

    /// Take the keys of image files from now on as relative to `base`.
    pub fn set_base(&mut self, base: &Path) {
        self.base = base.to_path_buf();
    }

    /// `key` with an image file's path taken from the note's directory, so
    /// two notes' files of the same name are told apart.
    fn resolved(&self, key: &Key) -> Key {
        match key.0.strip_prefix("file:") {
            Some(src) => path_key(&self.base, src),
            None => key.clone(),
        }
    }

    /// Ask for the image `key`, made by `job` unless it is already known.
    pub fn request(&mut self, key: Key, job: Job) {
        let key = self.resolved(&key);
        if !self.graphics || self.entries.contains_key(&key) {
            return;
        }
        let id = self.next_id;
        self.next_id = id % kitty::MAX_ID + 1;
        let tx = self.tx.clone();
        let sent = key.clone();
        std::thread::Builder::new()
            .name(JOB_THREAD.into())
            .spawn(move || {
                let _ = tx.send((sent, run(job)));
            })
            .expect("failed to spawn thread");
        self.entries.insert(
            key,
            Entry {
                id,
                state: State::Loading,
                placed: None,
            },
        );
    }

    /// Ask for exactly these images: start the new ones, try again the ones
    /// that failed, and forget the rest, deleting from the terminal any that
    /// were uploaded.
    pub fn want(&mut self, wanted: Vec<(Key, Job)>) {
        let wanted: Vec<_> = wanted
            .into_iter()
            .map(|(key, job)| (self.resolved(&key), job))
            .collect();
        let keep: HashSet<&Key> = wanted.iter().map(|(k, _)| k).collect();
        let deleted = &mut self.deleted;
        self.entries.retain(|key, entry| {
            if matches!(entry.state, State::Failed(_)) {
                return false;
            }
            let kept = keep.contains(key);
            if !kept && entry.placed.is_some() {
                deleted.push(entry.id);
            }
            kept
        });
        for (key, job) in wanted {
            self.request(key, job);
        }
    }

    /// Take in the images that finished since the last call. Returns true when
    /// any did, so the layout must be redone.
    pub fn receive(&mut self) -> bool {
        let mut changed = false;
        while let Ok((key, result)) = self.rx.try_recv() {
            // Results for images that left the document are dropped.
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            entry.state = match result {
                Ok(png) => State::Ready {
                    px: (png.width, png.height),
                    png: Some(png.data),
                },
                Err(e) => State::Failed(e),
            };
            changed = true;
        }
        changed
    }

    /// The image `key`, if it is ready to lay out.
    pub fn ready(&self, key: &Key) -> Option<Ready> {
        match self.entries.get(&self.resolved(key))? {
            Entry {
                id,
                state: State::Ready { px, .. },
                ..
            } => Some(Ready { id: *id, px: *px }),
            _ => None,
        }
    }

    /// Why making the image `key` failed, if it did.
    pub fn error(&self, key: &Key) -> Option<&str> {
        match &self.entries.get(&self.resolved(key))?.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// Bring the terminal in line with a layout: delete images that left the
    /// document, upload images laid out for the first time and place images
    /// whose size changed. Nothing is sent for an image already placed at its
    /// size, so redraws send no image commands at all.
    pub fn flush(&mut self, out: &mut impl Write, placements: &[Placement]) -> io::Result<()> {
        let mut sent = false;
        for id in self.deleted.drain(..) {
            kitty::delete(out, id)?;
            self.uploaded.set(id, false);
            sent = true;
        }
        for p in placements {
            let Some(entry) = self.entries.values_mut().find(|e| e.id == p.id) else {
                continue;
            };
            let State::Ready { png, .. } = &mut entry.state else {
                continue;
            };
            if let Some(data) = png.take() {
                self.uploaded.set(entry.id, true);
                kitty::upload(out, entry.id, &data)?;
                sent = true;
            }
            if entry.placed != Some((p.cols, p.rows)) {
                kitty::place(out, entry.id, p.cols, p.rows)?;
                entry.placed = Some((p.cols, p.rows));
                sent = true;
            }
        }
        if sent {
            out.flush()?;
        }
        Ok(())
    }

    /// Delete every uploaded image from the terminal, as on exit.
    pub fn delete_all(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.want(Vec::new());
        self.flush(out, &[])
    }
}

/// The image a block is drawn as, if any. An image file's key is relative to
/// the note on screen.
pub fn key(block: &Block) -> Option<Key> {
    match &block.kind {
        BlockKind::Image { src, .. } if is_local(src) => Some(Key(format!("file:{src}"))),
        BlockKind::Figure {
            kind: FigureKind::Math,
            source,
        } => Some(Key(format!("math:{}:{source}", maths::dark_background()))),
        BlockKind::Figure {
            kind: FigureKind::Mermaid,
            source,
        } => Some(Key(format!(
            "mermaid:{}",
            mermaid::name(source, theme::current())
        ))),
        BlockKind::Heading { level, content } => {
            heading::name(*level, content, theme::current()).map(Key)
        }
        _ => None,
    }
}

/// The image a block is drawn as and how to make it. Relative paths are taken
/// from `base`, the directory of the markdown file.
pub fn wanted(block: &Block, base: &Path) -> Option<(Key, Job)> {
    let mut key = key(block)?;
    let job: Job = match &block.kind {
        BlockKind::Image { src, .. } => {
            key = path_key(base, src);
            let paths: Vec<_> = candidates(src).map(|src| base.join(src)).collect();
            Box::new(move || {
                // The first that is there, or the path as written for the error.
                let path = paths.iter().find(|p| p.is_file()).unwrap_or(&paths[0]);
                load(path)
            })
        }
        BlockKind::Figure {
            kind: FigureKind::Math,
            source,
        } => {
            let (source, dark) = (source.clone(), maths::dark_background());
            Box::new(move || maths::render(&source, dark))
        }
        BlockKind::Figure {
            kind: FigureKind::Mermaid,
            source,
        } => mermaid::job(source.clone(), theme::current()),
        BlockKind::Heading { level, content } => heading::job(*level, content, theme::current()),
        _ => return None,
    };
    Some((key, job))
}

/// The key of the image file `src` in the directory `base`.
fn path_key(base: &Path, src: &str) -> Key {
    Key(format!("path:{}", base.join(src).display()))
}

/// Every image the blocks, and the blocks inside them, are drawn as.
pub fn wanted_in(blocks: &[Block], base: &Path, out: &mut Vec<(Key, Job)>) {
    for block in blocks {
        out.extend(wanted(block, base));
        match &block.kind {
            BlockKind::Quote(inner) => wanted_in(inner, base, out),
            BlockKind::List { items, .. } => {
                for item in items {
                    wanted_in(&item.blocks, base, out);
                }
            }
            _ => {}
        }
    }
}

/// The files `src` may mean, in the order to look for them: as written,
/// then decoded, as editors write spaces in paths as %20; then the same
/// without a `#fragment` and then also without a `?query`, which links meant
/// for a web page carry, such as `logo.png#gh-dark-mode-only`.
fn candidates(src: &str) -> impl Iterator<Item = String> {
    let unfragmented = src.split_once('#').map(|(path, _)| path);
    let unqueried = unfragmented
        .unwrap_or(src)
        .split_once('?')
        .map(|(path, _)| path);
    let written = [Some(src), unfragmented, unqueried].into_iter().flatten();
    written.flat_map(|src| {
        [Some(src.to_string()), percent_decode(src)]
            .into_iter()
            .flatten()
    })
}

/// Images on the web are left as text.
fn is_local(src: &str) -> bool {
    !src.is_empty() && !src.contains("://") && !src.starts_with("data:")
}

/// `src` with its %XX escapes decoded, if it has any.
fn percent_decode(src: &str) -> Option<String> {
    if !src.contains('%') {
        return None;
    }
    let bytes = src.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// A PNG, GIF or JPEG file, told apart by its first bytes, or an SVG, told
/// by its name. A GIF shows its first frame.
fn load(path: &Path) -> Result<Png, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let named = |ext: &str| {
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
    };
    if named("svg") || named("svgz") {
        svg(&data, path.parent())
    } else if data.starts_with(b"GIF8") {
        gif(&data)
    } else if data.starts_with(b"\xff\xd8\xff") {
        jpeg(&data)
    } else {
        Png::new(data)
    }
}

/// The most pixels a GIF's screen may have.
const GIF_MAX_PIXELS: usize = 40_000_000;

/// A GIF's first frame, placed on its logical screen, which grows to hold the
/// frame if it is too small. What the frame does not cover, and its
/// transparent pixels, stay transparent.
fn gif(data: &[u8]) -> Result<Png, String> {
    let error = |e: gif::DecodingError| format!("GIF: {e}");
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(data).map_err(error)?;
    let screen = (usize::from(decoder.width()), usize::from(decoder.height()));
    let frame = decoder
        .read_next_frame()
        .map_err(error)?
        .ok_or("GIF has no frames")?;
    let (left, top) = (usize::from(frame.left), usize::from(frame.top));
    let frame_width = usize::from(frame.width);
    let width = screen.0.max(left + frame_width);
    let height = screen.1.max(top + usize::from(frame.height));
    if width * height > GIF_MAX_PIXELS {
        return Err("GIF too large".into());
    }
    let mut pixels = vec![0u8; width * height * 4];
    let n = frame_width * 4;
    for y in 0..usize::from(frame.height) {
        let at = ((top + y) * width + left) * 4;
        pixels[at..at + n].copy_from_slice(&frame.buffer[y * n..(y + 1) * n]);
    }
    let data = raster::encode(width as u32, height as u32, png::ColorType::Rgba, &pixels)?;
    Png::new(data)
}

/// Pixels per SVG user unit (a CSS pixel), as on a high-density screen, as
/// diagrams are drawn.
const SVG_SCALE: f32 = 2.0;

/// An SVG, drawn at [`SVG_SCALE`], or smaller to keep within
/// [`GIF_MAX_PIXELS`]. Images it names are read from `dir`; text is set in
/// the fonts mermaid labels use.
fn svg(data: &[u8], dir: Option<&Path>) -> Result<Png, String> {
    let options = resvg::usvg::Options {
        resources_dir: dir.map(Path::to_path_buf),
        fontdb: svg_fonts().clone(),
        ..Default::default()
    };
    let fallbacks = std::str::from_utf8(data).ok().map(css_fallbacks);
    let data = fallbacks.as_deref().map_or(data, str::as_bytes);
    let tree = resvg::usvg::Tree::from_data(data, &options).map_err(|e| format!("SVG: {e}"))?;
    let (w, h) = (tree.size().width(), tree.size().height());
    let most = (GIF_MAX_PIXELS as f32 / (w * h)).sqrt();
    let scale = SVG_SCALE.min(most);
    // Rounded down, so a shrunk SVG stays within the limit.
    let (width, height) = ((w * scale) as u32, (h * scale) as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or("SVG has no size")?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let pixels: Vec<u8> = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Png::new(raster::encode(
        width,
        height,
        png::ColorType::Rgba,
        &pixels,
    )?)
}

/// `text` with each CSS `var(--name, fallback)` replaced by its fallback.
/// SVGs made for web pages colour themselves this way, and resvg does not
/// read `var()`. One with no fallback is left alone.
fn css_fallbacks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("var(") {
        out.push_str(&rest[..at]);
        let inner = &rest[at + 4..];
        // The closing bracket, past any brackets inside, such as rgb(...).
        let mut depth = 0;
        let end = inner.char_indices().find_map(|(i, c)| match c {
            '(' => {
                depth += 1;
                None
            }
            ')' if depth == 0 => Some(i),
            ')' => {
                depth -= 1;
                None
            }
            _ => None,
        });
        match end.and_then(|end| Some((end, inner[..end].split_once(',')?.1))) {
            Some((end, fallback)) => {
                out.push_str(&css_fallbacks(fallback.trim()));
                rest = &inner[end + 1..];
            }
            None => {
                out.push_str("var(");
                rest = inner;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The fonts SVG text is set in: the proportional ones mermaid labels use,
/// for every family but monospace, and the monospace one code uses. Read
/// once. Named fonts an SVG asks for that are not here fall back to these.
fn svg_fonts() -> &'static Arc<Database> {
    static FONTS: OnceLock<Arc<Database>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut db = Database::new();
        let family = |db: &Database| {
            db.faces()
                .last()
                .and_then(|f| f.families.first())
                .map(|(name, _)| name.clone())
        };
        if let Some((regular, bold)) = raster::sans_files() {
            let _ = db.load_font_file(regular);
            if let Some(sans) = family(&db) {
                db.set_sans_serif_family(&sans);
                db.set_serif_family(&sans);
                db.set_cursive_family(&sans);
                db.set_fantasy_family(&sans);
            }
            if let Some(bold) = bold {
                let _ = db.load_font_file(bold);
            }
        }
        if let Some(mono) = raster::mono_file() {
            let _ = db.load_font_file(mono);
            if let Some(mono) = family(&db) {
                db.set_monospace_family(mono);
            }
        }
        Arc::new(db)
    })
}

/// A JPEG in the colours the decoder gives.
fn jpeg(data: &[u8]) -> Result<Png, String> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    let mut decoder = zune_jpeg::JpegDecoder::new(io::Cursor::new(data));
    let pixels = decoder.decode().map_err(|e| format!("JPEG: {e}"))?;
    let info = decoder.info().ok_or("JPEG has no size")?;
    let color = match decoder.output_colorspace() {
        Some(ColorSpace::RGB) => png::ColorType::Rgb,
        Some(ColorSpace::RGBA) => png::ColorType::Rgba,
        Some(ColorSpace::Luma) => png::ColorType::Grayscale,
        Some(ColorSpace::LumaA) => png::ColorType::GrayscaleAlpha,
        _ => return Err("JPEG colours not supported".into()),
    };
    let data = raster::encode(info.width.into(), info.height.into(), color, &pixels)?;
    Png::new(data)
}

/// Cells to show an image of `px` pixels at its natural size, shrunk to at
/// most `max_cols` columns, keeping its shape. A placeholder can address at
/// most [`kitty::MAX_CELLS`] rows and columns.
///
/// Ghostty stretches an image to fill its cells, so a shrunken image takes
/// whole rows first and the columns from them: rounding the rows up would
/// squash it sideways. `None` when it would be less than a row tall.
pub fn fit(px: (u32, u32), cell: (u16, u16), max_cols: u16) -> Option<(u16, u16)> {
    let (cw, ch) = (u64::from(cell.0.max(1)), u64::from(cell.1.max(1)));
    let (w, h) = (u64::from(px.0.max(1)), u64::from(px.1.max(1)));
    let limit = u64::from(kitty::MAX_CELLS);
    let max_cols = u64::from(max_cols).clamp(1, limit);
    let mut cols = w.div_ceil(cw);
    let mut rows = h.div_ceil(ch);
    if cols > max_cols || rows > limit {
        rows = (h * max_cols * cw / (w * ch)).min(rows).min(limit);
        if rows == 0 {
            return None;
        }
        cols = (w * rows * ch).div_ceil(h * cw).clamp(1, max_cols);
    }
    Some((cols as u16, rows as u16))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::doc::parse;
    use std::time::{Duration, Instant};

    /// A PNG header for an image `w` by `h` pixels; enough for `Png::new`.
    pub fn png(w: u32, h: u32) -> Vec<u8> {
        let mut data = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        data.extend(w.to_be_bytes());
        data.extend(h.to_be_bytes());
        data.extend([8, 6, 0, 0, 0]);
        data
    }

    pub fn job(data: Vec<u8>) -> Job {
        Box::new(move || Png::new(data))
    }

    pub fn file_key(src: &str) -> Key {
        Key(format!("file:{src}"))
    }

    /// Wait for the jobs started so far to finish.
    pub fn settle(images: &mut Images) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            images.receive();
            if !images
                .entries
                .values()
                .any(|e| matches!(e.state, State::Loading))
            {
                return;
            }
            assert!(Instant::now() < deadline, "images not ready");
            std::thread::sleep(Duration::from_millis(1));
        }
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
    fn png_size_comes_from_the_header() {
        let png = Png::new(png(1568, 224)).unwrap();
        assert_eq!((png.width, png.height), (1568, 224));
        assert!(Png::new(b"GIF89a".to_vec()).is_err());
        assert!(Png::new(super::tests::png(0, 4)).is_err());
    }

    #[test]
    fn fit_keeps_small_images_at_natural_size() {
        assert_eq!(fit((160, 32), (8, 16), 80), Some((20, 2)));
        // Part cells round up.
        assert_eq!(fit((161, 33), (8, 16), 80), Some((21, 3)));
    }

    #[test]
    fn fit_shrinks_wide_images_to_the_width_keeping_shape() {
        assert_eq!(fit((1600, 320), (8, 16), 100), Some((100, 10)));
        // 2.86 rows at 40 columns: two whole rows, and 28 columns to match.
        assert_eq!(fit((1568, 224), (17, 34), 40), Some((28, 2)));
    }

    /// The shown width over height, against the image's, in cells.
    fn shape_error(px: (u32, u32), cell: (u16, u16), size: (u16, u16)) -> f64 {
        let want = f64::from(px.0) / f64::from(px.1) * f64::from(cell.1) / f64::from(cell.0);
        (f64::from(size.0) - want * f64::from(size.1)).abs()
    }

    #[test]
    fn fit_keeps_a_wide_short_image_in_shape_at_one_to_three_rows() {
        let cell = (10, 20);
        // 134 by 4 cells, like a long heading.
        let px = (1340, 80);
        for (max_cols, want) in [(110, (101, 3)), (84, (67, 2)), (60, (34, 1))] {
            let size = fit(px, cell, max_cols).unwrap();
            assert_eq!(size, want, "in {max_cols} columns");
            assert!(shape_error(px, cell, size) < 1.0, "{size:?}");
        }
        // Less than a row tall in 33 columns.
        assert_eq!(fit(px, cell, 33), None);
        assert_eq!(fit((1000, 10), cell, 0), None);
    }

    #[test]
    fn fit_keeps_every_shrunken_image_in_shape() {
        let cell = (9, 19);
        for w in (50u32..3000).step_by(37) {
            for h in (10..400).step_by(13) {
                // Only shrunken images: at natural size part cells round up.
                for max_cols in [1, 7, 20, 40, 84, 200] {
                    if w.div_ceil(9) <= u32::from(max_cols) {
                        continue;
                    }
                    if let Some(size) = fit((w, h), cell, max_cols) {
                        assert!(size.0 <= max_cols, "{w}x{h} in {max_cols}: {size:?}");
                        assert!(shape_error((w, h), cell, size) < 1.0, "{w}x{h}: {size:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn fit_stays_within_the_placeholder_limit() {
        let (cols, rows) = fit((100, 100_000), (10, 20), 80).unwrap();
        assert_eq!(rows, kitty::MAX_CELLS);
        assert!((1..=80).contains(&cols));
        // As wide as a placeholder allows is still less than a row tall.
        assert_eq!(fit((100_000, 100), (10, 20), 1000), None);
        let (cols, rows) = fit((10_000, 1000), (10, 20), 1000).unwrap();
        assert_eq!((cols, rows), (280, 14));
        assert!(cols <= kitty::MAX_CELLS);
    }

    #[test]
    fn only_local_images_have_a_key() {
        let doc = parse("![a](pics/a.png)\n\n![b](https://x.org/b.png)\n\ntext\n");
        let keys: Vec<Option<Key>> = doc.blocks.iter().map(key).collect();
        assert_eq!(keys, vec![Some(file_key("pics/a.png")), None, None]);
    }

    #[test]
    fn display_maths_is_an_image_unless_it_is_broken() {
        let doc = parse("$$\\frac{1}{2}$$\n\n$$\\notacommand{x}$$\n\ninline $x$\n");
        let keys: Vec<Option<Key>> = doc.blocks.iter().map(key).collect();
        assert!(keys[0].is_some() && keys[1].is_some());
        assert_eq!(keys[2], None);
        let mut images = Images::new((10, 20));
        images.want(
            doc.blocks
                .iter()
                .filter_map(|b| wanted(b, Path::new(".")))
                .collect(),
        );
        settle(&mut images);
        assert!(images.ready(keys[0].as_ref().unwrap()).is_some());
        assert_eq!(images.ready(keys[1].as_ref().unwrap()), None);
    }

    #[test]
    fn images_load_from_the_markdown_directory() {
        let dir = std::env::temp_dir().join(format!("smd-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), png(30, 40)).unwrap();
        let doc = parse("![a](a.png)\n\n![gone](missing.png)\n");
        let mut images = Images::new((10, 20));
        images.set_base(&dir);
        images.want(doc.blocks.iter().filter_map(|b| wanted(b, &dir)).collect());
        settle(&mut images);
        let _ = std::fs::remove_dir_all(&dir);
        let a = images.ready(&file_key("a.png")).unwrap();
        assert_eq!(a.px, (30, 40));
        assert_eq!(images.ready(&file_key("missing.png")), None);
    }

    #[test]
    fn a_failed_image_is_tried_again_on_the_next_reload() {
        let dir = std::env::temp_dir().join(format!("smd-images-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = parse("![d](d.png)\n");
        let want = |images: &mut Images| {
            images.want(doc.blocks.iter().filter_map(|b| wanted(b, &dir)).collect());
            settle(images);
        };
        let mut images = Images::new((10, 20));
        images.set_base(&dir);
        want(&mut images);
        assert_eq!(images.ready(&file_key("d.png")), None);
        std::fs::write(dir.join("d.png"), png(10, 20)).unwrap();
        want(&mut images);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(images.ready(&file_key("d.png")).unwrap().px, (10, 20));
    }

    /// `data` written to a file and loaded from it.
    fn load_bytes(name: &str, data: &[u8]) -> Result<Png, String> {
        let dir = std::env::temp_dir().join(format!("smd-images-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        let png = load(&path);
        let _ = std::fs::remove_file(&path);
        png
    }

    /// A GIF of `frames` on a screen `width` by `height`.
    fn gif_bytes(width: u16, height: u16, frames: &[gif::Frame]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut encoder = gif::Encoder::new(&mut data, width, height, &[]).unwrap();
        for frame in frames {
            encoder.write_frame(frame).unwrap();
        }
        drop(encoder);
        data
    }

    /// The RGBA pixel at `x`, `y` of an RGBA PNG.
    fn pixel(png: &Png, x: u32, y: u32) -> [u8; 4] {
        let mut reader = png::Decoder::new(&png.data[..]).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).unwrap();
        let at = ((y * png.width + x) * 4) as usize;
        buf[at..at + 4].try_into().unwrap()
    }

    const RED_GREEN: [u8; 6] = [255, 0, 0, 0, 255, 0];

    #[test]
    fn a_gif_shows_its_first_frame() {
        let first = gif::Frame::from_palette_pixels(3, 2, [0, 1, 0, 1, 0, 1], RED_GREEN, None);
        let second = gif::Frame::from_palette_pixels(3, 2, [1; 6], RED_GREEN, None);
        let png = load_bytes("a.gif", &gif_bytes(3, 2, &[first, second])).unwrap();
        assert_eq!((png.width, png.height), (3, 2));
        assert_eq!(pixel(&png, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel(&png, 1, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn a_small_gif_frame_sits_on_its_screen() {
        let mut frame = gif::Frame::from_palette_pixels(2, 2, [0, 1, 1, 0], RED_GREEN, Some(1));
        (frame.left, frame.top) = (1, 2);
        let png = load_bytes("small.gif", &gif_bytes(4, 4, &[frame])).unwrap();
        assert_eq!((png.width, png.height), (4, 4));
        assert_eq!(pixel(&png, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&png, 1, 2), [255, 0, 0, 255]);
        assert_eq!(pixel(&png, 2, 2)[3], 0);
        assert_eq!(pixel(&png, 2, 3), [255, 0, 0, 255]);
    }

    #[test]
    fn a_gif_screen_too_big_fails() {
        let frame = gif::Frame::from_palette_pixels(1, 1, [0], RED_GREEN, None);
        assert!(load_bytes("huge.gif", &gif_bytes(65535, 65535, &[frame])).is_err());
    }

    #[test]
    fn a_gif_screen_grows_to_hold_its_first_frame() {
        let frame = gif::Frame::from_palette_pixels(3, 2, [0; 6], RED_GREEN, None);
        let png = load_bytes("zero.gif", &gif_bytes(0, 0, &[frame])).unwrap();
        assert_eq!((png.width, png.height), (3, 2));
        let mut frame = gif::Frame::from_palette_pixels(3, 2, [0; 6], RED_GREEN, None);
        frame.left = 1;
        let png = load_bytes("wide.gif", &gif_bytes(2, 2, &[frame])).unwrap();
        assert_eq!((png.width, png.height), (4, 2));
        assert_eq!(pixel(&png, 3, 1), [255, 0, 0, 255]);
    }

    /// The fixture is a 5 by 3 red PNG saved as JPEG with
    /// `sips -s format jpeg -s formatOptions 50`.
    #[test]
    fn a_jpeg_loads_at_its_size() {
        let jpeg = include_bytes!("../tests/fixtures/red-5x3.jpg");
        let png = load_bytes("red.jpg", jpeg).unwrap();
        assert_eq!((png.width, png.height), (5, 3));
        let [r, g, b] = pixel_rgb(&png);
        assert!(r > 150 && g < 80 && b < 80, "{r} {g} {b}");
    }

    /// The first pixel of an RGB PNG.
    fn pixel_rgb(png: &Png) -> [u8; 3] {
        let mut reader = png::Decoder::new(&png.data[..]).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).unwrap();
        buf[..3].try_into().unwrap()
    }

    #[test]
    fn other_files_and_broken_images_fail() {
        assert!(load_bytes("a.txt", b"hello").is_err());
        assert!(load_bytes("cut.gif", b"GIF89a").is_err());
        assert!(load_bytes("cut.jpg", b"\xff\xd8\xff\xe0").is_err());
        assert!(load_bytes("cut.svg", b"<svg").is_err());
    }

    /// An SVG `width` by `height` holding `body`.
    fn svg_bytes(width: u32, height: u32, body: &str) -> Vec<u8> {
        format!(
            "<svg xmlns='http://www.w3.org/2000/svg' xmlns:xlink='http://www.w3.org/1999/xlink' \
             width='{width}' height='{height}'>{body}</svg>"
        )
        .into_bytes()
    }

    /// Every pixel of an RGBA PNG.
    fn pixels_rgba(png: &Png) -> Vec<[u8; 4]> {
        let mut reader = png::Decoder::new(&png.data[..]).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).unwrap();
        buf.as_chunks::<4>().0.to_vec()
    }

    #[test]
    fn an_svg_is_drawn_at_twice_its_size() {
        let red = svg_bytes(5, 3, "<rect width='5' height='3' fill='red'/>");
        let png = load_bytes("red.svg", &red).unwrap();
        assert_eq!((png.width, png.height), (10, 6));
        assert!(pixels_rgba(&png).iter().all(|&p| p == [255, 0, 0, 255]));
        // Told by its name, in any case; what it does not cover stays clear.
        let empty = load_bytes("EMPTY.SVG", &svg_bytes(4, 4, "")).unwrap();
        assert!(pixels_rgba(&empty).iter().all(|p| p[3] == 0));
    }

    #[test]
    fn css_variables_give_way_to_their_fallbacks() {
        assert_eq!(
            css_fallbacks("fill=\"var(--ink, #1c1b18)\""),
            "fill=\"#1c1b18\""
        );
        assert_eq!(
            css_fallbacks("var(--a, var(--b, rgb(1, 2, 3))) var(--c)"),
            "rgb(1, 2, 3) var(--c)"
        );
        assert_eq!(css_fallbacks("no vars, var("), "no vars, var(");
        let teal = "<rect width='2' height='2' fill='var(--accent, #0b6e6e)'/>";
        let png = load_bytes("vars.svg", &svg_bytes(2, 2, teal)).unwrap();
        assert_eq!(pixels_rgba(&png)[0], [0x0b, 0x6e, 0x6e, 255]);
    }

    #[test]
    fn a_huge_svg_is_drawn_smaller() {
        let png = load_bytes("huge.svg", &svg_bytes(40_000, 20_000, "")).unwrap();
        let pixels = u64::from(png.width) * u64::from(png.height);
        assert!(
            pixels <= GIF_MAX_PIXELS as u64,
            "{} x {}",
            png.width,
            png.height
        );
        assert_eq!(png.width, png.height * 2);
    }

    #[test]
    fn an_svg_reads_images_beside_it_and_sets_text() {
        let dir = std::env::temp_dir().join(format!("smd-images-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("red.jpg"),
            include_bytes!("../tests/fixtures/red-5x3.jpg"),
        )
        .unwrap();
        let body = "<image xlink:href='red.jpg' width='5' height='3'/>";
        std::fs::write(dir.join("a.svg"), svg_bytes(5, 3, body)).unwrap();
        let text = "<text x='2' y='30' font-family='No Such Font' font-size='24'>Hi</text>";
        std::fs::write(dir.join("t.svg"), svg_bytes(60, 40, text)).unwrap();
        let image = load(&dir.join("a.svg"));
        let text = load(&dir.join("t.svg"));
        let _ = std::fs::remove_dir_all(&dir);
        let [r, g, b, a] = pixels_rgba(&image.unwrap())[0];
        assert!(r > 150 && g < 80 && b < 80 && a == 255, "{r} {g} {b} {a}");
        // Set in the fallback font, when the machine has one.
        if raster::sans_files().is_some() {
            assert!(pixels_rgba(&text.unwrap()).iter().any(|p| p[3] > 0));
        }
    }

    #[test]
    fn a_fragment_or_query_is_dropped_only_when_the_file_is_not_there() {
        let dir = std::env::temp_dir().join(format!("smd-images-suffix-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("img")).unwrap();
        std::fs::write(dir.join("img/logo.png"), png(10, 20)).unwrap();
        std::fs::write(dir.join("issue#12.png"), png(30, 40)).unwrap();
        std::fs::write(dir.join("a#b.png"), png(50, 60)).unwrap();
        std::fs::write(dir.join("both.png#a"), png(70, 80)).unwrap();
        std::fs::write(dir.join("both.png"), png(10, 20)).unwrap();
        std::fs::create_dir_all(dir.join("gone")).unwrap();
        let srcs = [
            "img/logo.png#gh-dark-mode-only",
            "img/logo.png?raw=true",
            "img/logo.png?raw=true#top",
            "issue#12.png",
            "a%23b.png",
            "both.png#a",
            "gone#x.png",
        ];
        let md: String = srcs.iter().map(|src| format!("![x]({src})\n\n")).collect();
        let doc = parse(&md);
        let mut images = Images::new((10, 20));
        images.set_base(&dir);
        images.want(doc.blocks.iter().filter_map(|b| wanted(b, &dir)).collect());
        settle(&mut images);
        let _ = std::fs::remove_dir_all(&dir);
        let px = |src| images.ready(&file_key(src)).map(|r| r.px);
        assert_eq!(px("img/logo.png#gh-dark-mode-only"), Some((10, 20)));
        assert_eq!(px("img/logo.png?raw=true"), Some((10, 20)));
        assert_eq!(px("img/logo.png?raw=true#top"), Some((10, 20)));
        // A file whose name has a # in it is read as it is written.
        assert_eq!(px("issue#12.png"), Some((30, 40)));
        assert_eq!(px("a%23b.png"), Some((50, 60)));
        assert_eq!(px("both.png#a"), Some((70, 80)));
        // A folder is not an image: the error is about the path as written.
        let error = images.error(&file_key("gone#x.png")).unwrap();
        assert!(error.contains("gone#x.png"), "{error}");
    }

    #[test]
    fn escaped_paths_are_decoded() {
        let dir = std::env::temp_dir().join(format!("smd-images-escape-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("my diagram.png"), png(10, 20)).unwrap();
        let doc = parse("![d](my%20diagram.png)\n");
        let mut images = Images::new((10, 20));
        images.set_base(&dir);
        images.want(doc.blocks.iter().filter_map(|b| wanted(b, &dir)).collect());
        settle(&mut images);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(images.ready(&file_key("my%20diagram.png")).is_some());
        assert_eq!(percent_decode("a%2"), Some("a%2".into()));
        assert_eq!(percent_decode("%e6%97%a5.png"), Some("日.png".into()));
        assert_eq!(percent_decode("plain.png"), None);
    }

    #[test]
    fn upload_once_then_place_only_when_the_size_changes() {
        let mut images = Images::new((10, 20));
        images.request(file_key("a.png"), job(png(100, 40)));
        settle(&mut images);
        let id = images.ready(&file_key("a.png")).unwrap().id;
        let at = |cols, rows| [Placement { id, cols, rows }];

        let mut out = Vec::new();
        images.flush(&mut out, &at(10, 2)).unwrap();
        assert_eq!(
            commands(out),
            vec![
                format!("\x1b_Ga=t,f=100,i={id},q=2,m=0"),
                format!("\x1b_Ga=p,U=1,i={id},p=1,c=10,r=2,q=2"),
            ]
        );
        // A redraw at the same size sends nothing.
        let mut out = Vec::new();
        images.flush(&mut out, &at(10, 2)).unwrap();
        assert!(out.is_empty());
        // A resize places it again without sending the data.
        let mut out = Vec::new();
        images.flush(&mut out, &at(5, 1)).unwrap();
        assert_eq!(
            commands(out),
            vec![format!("\x1b_Ga=p,U=1,i={id},p=1,c=5,r=1,q=2")]
        );
    }

    #[test]
    fn images_that_leave_are_deleted_once() {
        let mut images = Images::new((10, 20));
        images.request(file_key("a.png"), job(png(10, 20)));
        images.request(file_key("b.png"), job(png(10, 20)));
        settle(&mut images);
        let a = images.ready(&file_key("a.png")).unwrap().id;
        let b = images.ready(&file_key("b.png")).unwrap().id;
        assert_ne!(a, b);
        let mut out = Vec::new();
        let both = [
            Placement {
                id: a,
                cols: 1,
                rows: 1,
            },
            Placement {
                id: b,
                cols: 1,
                rows: 1,
            },
        ];
        images.flush(&mut out, &both).unwrap();

        // b stays; a was uploaded, so it is deleted.
        images.want(vec![(file_key("b.png"), job(png(10, 20)))]);
        assert_eq!(images.ready(&file_key("a.png")), None);
        let mut out = Vec::new();
        images.flush(&mut out, &both[1..]).unwrap();
        assert_eq!(commands(out), vec![format!("\x1b_Ga=d,d=I,i={a},q=2")]);

        let mut out = Vec::new();
        images.delete_all(&mut out).unwrap();
        assert_eq!(commands(out), vec![format!("\x1b_Ga=d,d=I,i={b},q=2")]);
        let mut out = Vec::new();
        images.delete_all(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn an_image_never_laid_out_is_never_sent() {
        let mut images = Images::new((10, 20));
        images.request(file_key("a.png"), job(png(10, 20)));
        settle(&mut images);
        let mut out = Vec::new();
        images.flush(&mut out, &[]).unwrap();
        images.delete_all(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn text_mode_makes_no_images_and_sends_nothing() {
        let mut images = Images::text();
        images.want(vec![(file_key("a.png"), job(png(10, 20)))]);
        images.request(file_key("b.png"), job(png(10, 20)));
        assert!(images.entries.is_empty());
        assert!(!images.receive());
        assert_eq!(images.ready(&file_key("a.png")), None);
        let mut out = Vec::new();
        images.flush(&mut out, &[]).unwrap();
        images.delete_all(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn a_job_that_panics_fails_with_a_short_error() {
        let mut images = Images::new((10, 20));
        images.request(
            file_key("a.png"),
            Box::new(|| panic!("index out of range\nmore")),
        );
        images.request(file_key("b.png"), Box::new(|| panic!("bad {}", 7)));
        images.request(file_key("c.png"), Box::new(|| std::panic::panic_any(1)));
        settle(&mut images);
        assert_eq!(
            images.error(&file_key("a.png")),
            Some("crashed: index out of range")
        );
        assert_eq!(images.error(&file_key("b.png")), Some("crashed: bad 7"));
        assert_eq!(images.error(&file_key("c.png")), Some("crashed"));
    }

    #[test]
    fn uploaded_tracks_the_images_in_the_terminal() {
        let mut images = Images::new((10, 20));
        images.request(file_key("a.png"), job(png(10, 20)));
        settle(&mut images);
        let id = images.ready(&file_key("a.png")).unwrap().id;
        let uploaded = images.uploaded();
        let mut out = Vec::new();
        uploaded.delete_all(&mut out).unwrap();
        assert!(out.is_empty());

        let at = [Placement {
            id,
            cols: 1,
            rows: 1,
        }];
        images.flush(&mut Vec::new(), &at).unwrap();
        let mut out = Vec::new();
        uploaded.delete_all(&mut out).unwrap();
        assert_eq!(commands(out), vec![format!("\x1b_Ga=d,d=I,i={id},q=2")]);

        images.delete_all(&mut Vec::new()).unwrap();
        let mut out = Vec::new();
        uploaded.delete_all(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn a_result_for_an_image_that_left_is_dropped() {
        let mut images = Images::new((10, 20));
        images.request(file_key("a.png"), job(png(10, 20)));
        images.want(Vec::new());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!images.receive());
        assert_eq!(images.ready(&file_key("a.png")), None);
    }
}
