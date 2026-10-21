//! Throwaway spike: how should smd show a table wider than the pane? This is
//! not part of the smd binary.
//!
//! For every table in the given files and every pane width, it tries these
//! outcomes in order and reports the first that stays readable:
//!
//! 1. the table fits as text;
//! 2. it fits as text once cells wrap, breaking at most a couple of words
//!    mid-word, in fewer lines than cards would take;
//! 3. an image with cells wrapped at a fixed cap, if its text stays big enough;
//! 4. each row as a card of `Header: value` lines.
//!
//! It also reports what smd does today (an image with nothing wrapped).
//!
//! Run: `cargo run --release --example tables -- [--widths 80,120,160,200]
//! [--cell WxH] [--show] [--all] [--png DIR] FILE...`
//!
//! `--show` draws what each width would show, images included (kitty
//! graphics, direct placement; Ghostty or kitty, not inside herdr). `--all`
//! draws every candidate, not only the one picked. `--png` writes the images.

#![allow(dead_code)]

#[path = "../../src/kitty/diacritics.rs"]
mod diacritics;
#[path = "../../src/doc.rs"]
mod doc;
#[path = "../../src/raster.rs"]
mod raster;
mod table;
#[path = "../../src/theme.rs"]
mod theme;

/// Just enough of `src/images.rs` for `raster`.
mod images {
    pub struct Png {
        pub data: Vec<u8>,
    }

    impl Png {
        pub fn new(data: Vec<u8>) -> Result<Png, String> {
            Ok(Png { data })
        }
    }
}

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use doc::{Block, BlockKind, FigureKind, InlineStyle};
use table::{SIZE, Stats, Table};
use theme::Theme;

/// Narrowest a column may be made by wrapping, unless its text is shorter.
/// Only for the report: the heuristic keeps words whole instead.
const FLOOR: usize = 12;
/// Most words wrapped text may break mid-word, not at punctuation.
const MAX_HARD: usize = 2;
/// Characters per line in an image cell.
const CAP: usize = 40;
/// Smallest image text worth showing, against the terminal's own text.
const MIN_SCALE: f32 = 0.7;

struct Args {
    widths: Vec<usize>,
    cell: (u16, u16),
    show: bool,
    all: bool,
    png: Option<PathBuf>,
    files: Vec<PathBuf>,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        widths: vec![80, 120, 160, 200],
        cell: cell_size().unwrap_or((10, 22)),
        show: false,
        all: false,
        png: None,
        files: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--widths" => {
                a.widths = value()?
                    .split(',')
                    .map(|w| w.parse().map_err(|_| format!("bad width {w}")))
                    .collect::<Result<_, _>>()?
            }
            "--cell" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--cell is WxH")?;
                a.cell = (
                    w.parse().map_err(|_| "bad cell width")?,
                    h.parse().map_err(|_| "bad cell height")?,
                );
            }
            "--show" => a.show = true,
            "--all" => {
                a.show = true;
                a.all = true;
            }
            "--png" => a.png = Some(value()?.into()),
            _ if arg.starts_with("--") => return Err(format!("unknown flag {arg}")),
            _ => a.files.push(arg.into()),
        }
    }
    if a.files.is_empty() {
        return Err(
            "usage: tables [--widths 80,120] [--cell WxH] [--show] [--all] \
                    [--png DIR] FILE..."
                .into(),
        );
    }
    Ok(a)
}

/// The terminal's cell size in pixels, from the window size it reports.
fn cell_size() -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ fills a zeroed winsize.
    let ws = unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) != 0 {
            return None;
        }
        ws
    };
    if ws.ws_xpixel == 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

/// Every table in the document, including inside quotes and lists.
fn tables(blocks: &[Block], out: &mut Vec<String>) {
    for block in blocks {
        match &block.kind {
            BlockKind::Figure {
                kind: FigureKind::Table,
                source,
            } => out.push(source.clone()),
            BlockKind::Quote(inner) => tables(inner, out),
            BlockKind::List { items, .. } => {
                for item in items {
                    tables(&item.blocks, out);
                }
            }
            _ => {}
        }
    }
}

/// A copy of `images::fit`: the cells an image of `px` takes at most
/// `max_cols` wide.
fn fit(px: (u32, u32), cell: (u16, u16), max_cols: u16) -> Option<(u16, u16)> {
    let (cw, ch) = (u64::from(cell.0.max(1)), u64::from(cell.1.max(1)));
    let (w, h) = (u64::from(px.0.max(1)), u64::from(px.1.max(1)));
    let limit = diacritics::DIACRITICS.len() as u64;
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

/// An image drawn once per table, whatever the pane width.
struct Image {
    png: Vec<u8>,
    px: (u32, u32),
}

impl Image {
    fn new(table: &Table, cap: Option<usize>) -> Result<Image, String> {
        let png = table::draw(table, Theme::Dark, cap)?.png()?.data;
        // The size, from the PNG header; `Canvas::size` is test-only.
        let be = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
        let px = (be(16), be(20));
        Ok(Image { png, px })
    }

    /// The cells it takes in `avail` columns, and the size of its text
    /// against the terminal's: a character's width in the image, shown,
    /// over a cell's width.
    fn placed(&self, avail: usize, cell: (u16, u16)) -> Option<((u16, u16), f32)> {
        let avail = avail.clamp(1, u16::MAX as usize) as u16;
        let size = fit(self.px, cell, avail)?;
        let advance = raster::measure(&raster::fonts().ok()?.regular, SIZE, "M");
        let shown = f32::from(size.0) * f32::from(cell.0) / self.px.0 as f32;
        Some((size, shown * advance / f32::from(cell.0)))
    }
}

enum Choice {
    Text,
    Wrapped { widths: Vec<usize> },
    Capped { size: (u16, u16), scale: f32 },
    Cards,
}

impl Choice {
    fn label(&self) -> String {
        match self {
            Choice::Text => "1 text".into(),
            Choice::Wrapped { .. } => "2 wrapped text".into(),
            Choice::Capped { scale, .. } => format!("3 capped image {:.0}%", scale * 100.0),
            Choice::Cards => "4 cards".into(),
        }
    }
}

fn wrapped(table: &Table, avail: usize, floor: usize) -> Option<(Vec<usize>, Stats)> {
    let widths = table.fit_widths(avail, floor)?;
    let (_, stats) = table.text(&widths, |b, _| b);
    Some((widths, stats))
}

fn choose(table: &Table, avail: usize, capped: Option<&Image>, cell: (u16, u16)) -> Choice {
    if table.width() <= avail {
        return Choice::Text;
    }
    let (cards, _) = table.cards(avail, |b, _| b);
    if let Some((widths, stats)) = wrapped(table, avail, FLOOR)
        && stats.hard <= MAX_HARD
        && stats.heights.iter().sum::<usize>() + 4 <= cards.len()
    {
        return Choice::Wrapped { widths };
    }
    if let Some((size, scale)) = capped.and_then(|i| i.placed(avail, cell))
        && scale >= MIN_SCALE
    {
        return Choice::Capped { size, scale };
    }
    Choice::Cards
}

fn styled(base: Style, style: &InlineStyle) -> Style {
    let mut s = base;
    if style.emphasis {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if style.strong {
        s = s.add_modifier(Modifier::BOLD);
    }
    if style.strikethrough {
        s = s.add_modifier(Modifier::CROSSED_OUT);
    }
    if style.code {
        s = s.fg(Color::Yellow);
    }
    if style.math {
        s = s.fg(Color::Cyan);
    }
    if style.link.is_some() {
        s = s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
    }
    s
}

fn ansi(line: &[Span]) -> String {
    let mut out = String::new();
    for span in line {
        let mut codes = Vec::new();
        let m = span.style.add_modifier;
        for (flag, code) in [
            (Modifier::BOLD, "1"),
            (Modifier::DIM, "2"),
            (Modifier::ITALIC, "3"),
            (Modifier::UNDERLINED, "4"),
            (Modifier::CROSSED_OUT, "9"),
        ] {
            if m.contains(flag) {
                codes.push(code);
            }
        }
        match span.style.fg {
            Some(Color::Yellow) => codes.push("33"),
            Some(Color::Blue) => codes.push("34"),
            Some(Color::Cyan) => codes.push("36"),
            Some(Color::DarkGray) => codes.push("90"),
            _ => {}
        }
        if codes.is_empty() {
            out.push_str(&span.content);
        } else {
            let _ = write!(out, "\x1b[{}m{}\x1b[0m", codes.join(";"), span.content);
        }
    }
    out
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Show a PNG at `size` cells with a kitty direct placement.
fn kitty(out: &mut impl Write, png: &[u8], size: (u16, u16)) -> io::Result<()> {
    let data = base64(png);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let chunk = std::str::from_utf8(chunk).unwrap();
        if i == 0 {
            let (c, r) = size;
            write!(
                out,
                "\x1b_Ga=T,f=100,q=2,c={c},r={r},m={more};{chunk}\x1b\\"
            )?;
        } else {
            write!(out, "\x1b_Gm={more};{chunk}\x1b\\")?;
        }
    }
    writeln!(out)
}

fn print_text(out: &mut impl Write, table: &Table, widths: &[usize]) -> io::Result<()> {
    let (lines, _) = table.text(widths, styled);
    for line in lines {
        writeln!(out, "{}", ansi(&line))?;
    }
    Ok(())
}

fn print_cards(out: &mut impl Write, table: &Table, width: usize) -> io::Result<()> {
    let (lines, _) = table.cards(width, styled);
    for line in lines {
        writeln!(out, "{}", ansi(&line))?;
    }
    Ok(())
}

fn label(table: &Table) -> String {
    let head: Vec<String> = table.rows[0]
        .iter()
        .map(|cell| cell.iter().map(|(t, _)| t.as_str()).collect())
        .collect();
    let head = head.join(" | ");
    match head.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &head[..i]),
        None => head,
    }
}

fn main() -> io::Result<()> {
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "cell {}x{} px; floor {FLOOR}, word cap {}, cap {CAP}, min scale {MIN_SCALE}",
        args.cell.0,
        args.cell.1,
        table::WORD_CAP
    )?;
    for file in &args.files {
        let source = std::fs::read_to_string(file)?;
        let mut found = Vec::new();
        tables(&doc::parse(&source).blocks, &mut found);
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        writeln!(out, "\n# {} ({} tables)", file.display(), found.len())?;
        for (n, source) in found.iter().enumerate() {
            let table = Table::parse(source);
            let whole = Image::new(&table, None);
            let capped = Image::new(&table, Some(CAP));
            writeln!(
                out,
                "\n## {name} #{}: {} ({} cols x {} rows, natural {} wide)",
                n + 1,
                label(&table),
                table.columns(),
                table.rows.len().saturating_sub(1),
                table.width(),
            )?;
            if let Some(dir) = &args.png {
                std::fs::create_dir_all(dir)?;
                let stem = format!("{}-{}", name.trim_end_matches(".md"), n + 1);
                for (image, kind) in [(&whole, "whole"), (&capped, "capped")] {
                    if let Ok(image) = image {
                        std::fs::write(dir.join(format!("{stem}-{kind}.png")), &image.png)?;
                    }
                }
            }
            for &avail in &args.widths {
                let now = whole
                    .as_ref()
                    .ok()
                    .and_then(|i| i.placed(avail, args.cell))
                    .map_or("-".into(), |(_, s)| format!("{:.0}%", s * 100.0));
                let cap = capped
                    .as_ref()
                    .ok()
                    .and_then(|i| i.placed(avail, args.cell))
                    .map_or("-".into(), |((c, r), s)| {
                        format!("{:.0}% {c}x{r}", s * 100.0)
                    });
                let wrap = match wrapped(&table, avail, FLOOR) {
                    Some((w, s)) => format!(
                        "{} lines, cols {w:?} row mean {:.1} max {}, breaks {} soft {} hard",
                        s.heights.iter().sum::<usize>() + 4,
                        s.mean(),
                        s.max(),
                        s.soft,
                        s.hard
                    ),
                    None => format!("floor {FLOOR} does not fit"),
                };
                let choice = choose(&table, avail, capped.as_ref().ok(), args.cell);
                let (cards, stats) = table.cards(avail, |b, _| b);
                let cards = format!("{} lines, {} hard", cards.len(), stats.hard);
                writeln!(
                    out,
                    "  {avail:>3}: {:<22} | now {now:>4} | capped {cap:<12} | cards {cards:<18} | wrap {wrap}",
                    choice.label()
                )?;
                if !args.show {
                    continue;
                }
                writeln!(out)?;
                match &choice {
                    Choice::Text => print_text(&mut out, &table, &table.natural())?,
                    Choice::Wrapped { widths, .. } => print_text(&mut out, &table, widths)?,
                    Choice::Capped { size, .. } => {
                        kitty(&mut out, &capped.as_ref().unwrap().png, *size)?
                    }
                    Choice::Cards => print_cards(&mut out, &table, avail)?,
                }
                if args.all {
                    if !matches!(choice, Choice::Cards) {
                        writeln!(out, "  [cards]")?;
                        print_cards(&mut out, &table, avail)?;
                    }
                    if !matches!(choice, Choice::Wrapped { .. })
                        && let Some((widths, _)) = wrapped(&table, avail, FLOOR)
                    {
                        writeln!(out, "  [wrapped text, floor {FLOOR}]")?;
                        print_text(&mut out, &table, &widths)?;
                    }
                    if !matches!(choice, Choice::Capped { .. })
                        && let Ok(image) = &capped
                        && let Some((size, _)) = image.placed(avail, args.cell)
                    {
                        writeln!(out, "  [capped image]")?;
                        kitty(&mut out, &image.png, size)?;
                    }
                    if let Ok(image) = &whole
                        && let Some((size, _)) = image.placed(avail, args.cell)
                    {
                        writeln!(out, "  [today: whole image]")?;
                        kitty(&mut out, &image.png, size)?;
                    }
                }
                writeln!(out)?;
            }
        }
    }
    Ok(())
}
