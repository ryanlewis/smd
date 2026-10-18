//! Images in the text flow.
//!
//! A block that is drawn as an image names it with a [`Key`]. The store makes
//! each image once, off the main thread, and the layout reserves rows for it
//! once it is ready. The first time an image is laid out it is uploaded to the
//! terminal; after that only its placement changes, on resize. An image no
//! longer in the document is deleted from the terminal.
//!
//! Until an image is ready, or if making it fails, its block shows as text.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::doc::{Block, BlockKind, FigureKind};
use crate::theme::{self, Theme};
use crate::{kitty, maths, mermaid, table};

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
    entries: HashMap<Key, Entry>,
    next_id: u32,
    /// Uploaded images that left the document, to delete on the next flush.
    deleted: Vec<u32>,
    tx: Sender<(Key, Result<Png, String>)>,
    rx: Receiver<(Key, Result<Png, String>)>,
}

impl Images {
    pub fn new(cell: (u16, u16)) -> Self {
        let (tx, rx) = channel();
        Images {
            cell: (cell.0.max(1), cell.1.max(1)),
            entries: HashMap::new(),
            // Start at a random id, so another program's images in the same
            // terminal, or another smd's, are unlikely to share one.
            next_id: (RandomState::new().hash_one(std::process::id()) % u64::from(kitty::MAX_ID))
                as u32
                + 1,
            deleted: Vec::new(),
            tx,
            rx,
        }
    }

    pub fn cell(&self) -> (u16, u16) {
        self.cell
    }

    /// Ask for the image `key`, made by `job` unless it is already known.
    pub fn request(&mut self, key: Key, job: Job) {
        if self.entries.contains_key(&key) {
            return;
        }
        let id = self.next_id;
        self.next_id = id % kitty::MAX_ID + 1;
        let tx = self.tx.clone();
        let sent = key.clone();
        std::thread::spawn(move || {
            let _ = tx.send((sent, job()));
        });
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
        match self.entries.get(key)? {
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
        match &self.entries.get(key)?.state {
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

/// The image a block is drawn as, if any.
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
        BlockKind::Figure {
            kind: FigureKind::Table,
            source,
        } => Some(table_key(source, theme::current())),
        _ => None,
    }
}

/// The image a block is drawn as and how to make it. Relative paths are taken
/// from `base`, the directory of the markdown file.
pub fn wanted(block: &Block, base: &Path) -> Option<(Key, Job)> {
    let key = key(block)?;
    let job: Job = match &block.kind {
        BlockKind::Image { src, .. } => {
            let path = base.join(src);
            let decoded = percent_decode(src).map(|src| base.join(src));
            Box::new(move || match (load(&path), decoded) {
                // Editors write spaces in paths as %20.
                (Err(_), Some(decoded)) if !path.exists() => load(&decoded),
                (result, _) => result,
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
        BlockKind::Figure {
            kind: FigureKind::Table,
            source,
        } => {
            let (source, theme) = (source.clone(), theme::current());
            Box::new(move || table::image(&source, theme))
        }
        _ => return None,
    };
    Some((key, job))
}

/// A table's image depends on its source and the theme, never on the width.
fn table_key(source: &str, theme: Theme) -> Key {
    Key(format!("table:{}:{source}", theme.name()))
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

fn load(path: &Path) -> Result<Png, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Png::new(data)
}

/// Cells to show an image of `px` pixels at its natural size, shrunk to at
/// most `max_cols` columns, keeping its shape. A placeholder can address at
/// most [`kitty::MAX_CELLS`] rows and columns.
pub fn fit(px: (u32, u32), cell: (u16, u16), max_cols: u16) -> (u16, u16) {
    let (cw, ch) = (u64::from(cell.0.max(1)), u64::from(cell.1.max(1)));
    let (w, h) = (u64::from(px.0.max(1)), u64::from(px.1.max(1)));
    let limit = u64::from(kitty::MAX_CELLS);
    let max_cols = u64::from(max_cols).clamp(1, limit);
    let mut cols = w.div_ceil(cw);
    let mut rows = h.div_ceil(ch);
    if cols > max_cols {
        cols = max_cols;
        rows = (h * cols * cw).div_ceil(w * ch);
    }
    if rows > limit {
        rows = limit;
        cols = (w * rows * ch).div_ceil(h * cw);
    }
    (cols.clamp(1, max_cols) as u16, rows.clamp(1, limit) as u16)
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
        assert_eq!(fit((160, 32), (8, 16), 80), (20, 2));
        // Part cells round up.
        assert_eq!(fit((161, 33), (8, 16), 80), (21, 3));
    }

    #[test]
    fn fit_shrinks_wide_images_to_the_width_keeping_shape() {
        assert_eq!(fit((1600, 320), (8, 16), 100), (100, 10));
        assert_eq!(fit((1568, 224), (17, 34), 40), (40, 3));
        assert_eq!(fit((1000, 10), (10, 20), 0), (1, 1));
    }

    #[test]
    fn fit_stays_within_the_placeholder_limit() {
        let (cols, rows) = fit((100, 100_000), (10, 20), 80);
        assert_eq!(rows, kitty::MAX_CELLS);
        assert!((1..=80).contains(&cols));
        assert_eq!(fit((100_000, 100), (10, 20), 1000).0, kitty::MAX_CELLS);
    }

    #[test]
    fn a_table_is_keyed_on_its_source_and_theme() {
        let doc = parse("| a |\n|---|\n| 1 |\n");
        assert_eq!(
            key(&doc.blocks[0]),
            Some(table_key("| a |\n|---|\n| 1 |", Theme::Dark))
        );
        assert_ne!(
            table_key("| a |", Theme::Dark),
            table_key("| a |", Theme::Light)
        );
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
        want(&mut images);
        assert_eq!(images.ready(&file_key("d.png")), None);
        std::fs::write(dir.join("d.png"), png(10, 20)).unwrap();
        want(&mut images);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(images.ready(&file_key("d.png")).unwrap().px, (10, 20));
    }

    #[test]
    fn escaped_paths_are_decoded() {
        let dir = std::env::temp_dir().join(format!("smd-images-escape-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("my diagram.png"), png(10, 20)).unwrap();
        let doc = parse("![d](my%20diagram.png)\n");
        let mut images = Images::new((10, 20));
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
    fn a_result_for_an_image_that_left_is_dropped() {
        let mut images = Images::new((10, 20));
        images.request(file_key("a.png"), job(png(10, 20)));
        images.want(Vec::new());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!images.receive());
        assert_eq!(images.ready(&file_key("a.png")), None);
    }
}
