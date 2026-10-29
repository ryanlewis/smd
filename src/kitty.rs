//! The parts of the kitty graphics protocol smd uses: upload a PNG once, give
//! it a virtual placement (`U=1`) and draw it as Unicode placeholder cells.
//!
//! Every command asks for no reply (`q=2`). Inside herdr the replies come from
//! herdr rather than the outer terminal, and a reply that reached the key
//! reader would be read as key presses. The one query, in [`probe`], is sent
//! and answered before the key reader starts.

mod diacritics;

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::style::Color;

use crate::theme::{self, Theme};
use diacritics::DIACRITICS;

/// The character every placeholder cell starts with.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// The most rows or columns a placeholder can address, one per diacritic.
pub const MAX_CELLS: u16 = DIACRITICS.len() as u16;

/// The largest image id the foreground colour can carry.
pub const MAX_ID: u32 = 0xFF_FFFF;

/// Base64 bytes per upload command, as the protocol asks.
const CHUNK: usize = 4096;

/// One placeholder cell: the cell at `row`, `col` of the image named by the
/// cell's foreground colour. Both diacritics are always written.
pub fn cell(row: u16, col: u16) -> String {
    [
        PLACEHOLDER,
        DIACRITICS[usize::from(row)],
        DIACRITICS[usize::from(col)],
    ]
    .into_iter()
    .collect()
}

/// The foreground colour that names image `id` on its placeholder cells.
pub fn colour(id: u32) -> Color {
    Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}

/// Send a PNG to the terminal under `id`, without showing it.
pub fn upload(out: &mut impl Write, id: u32, png: &[u8]) -> io::Result<()> {
    let b64 = base64(png);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(CHUNK).collect();
    if chunks.is_empty() {
        return write!(out, "\x1b_Ga=t,f=100,i={id},q=2;\x1b\\");
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            write!(out, "\x1b_Ga=t,f=100,i={id},q=2,m={more};")?;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.write_all(chunk)?;
        out.write_all(b"\x1b\\")?;
    }
    Ok(())
}

/// Place image `id` as a virtual placement `cols` by `rows` cells. Placing it
/// again replaces the old placement, so the data is never sent twice.
pub fn place(out: &mut impl Write, id: u32, cols: u16, rows: u16) -> io::Result<()> {
    write!(out, "\x1b_Ga=p,U=1,i={id},p=1,c={cols},r={rows},q=2\x1b\\")
}

/// Delete image `id` and free its data.
pub fn delete(out: &mut impl Write, id: u32) -> io::Result<()> {
    write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

pub fn base64(data: &[u8]) -> String {
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

/// How long to wait for the terminal to answer the start-up queries.
const REPLY_TIMEOUT: Duration = Duration::from_millis(300);

/// Cell size used when the terminal reports none.
const FALLBACK_CELL: (u16, u16) = (8, 16);

/// The image id the graphics query asks about. A query stores nothing.
const QUERY_ID: u32 = 31;

/// What the terminal said about itself at start-up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    /// The size of one cell in pixels, width then height.
    pub cell: (u16, u16),
    /// Whether the background is dark or light, if the terminal said.
    pub theme: Option<Theme>,
    /// The background colour, if the terminal said.
    pub background: Option<theme::Rgb>,
    /// The terminal answered the kitty graphics query.
    pub graphics: bool,
}

/// Ask the terminal for its background colour with `OSC 11`, its cell size
/// with `CSI 16 t` and whether it knows the kitty graphics protocol.
///
/// The queries end with `CSI c` (DA1). Terminals answer in order, and nearly
/// all answer DA1, so the DA1 reply ends the wait: a terminal that sends it
/// without a graphics reply has no graphics. One that answers nothing is given
/// up on after a short timeout. The cell size falls back to the pixel size the
/// tty reports. The queries are sent on the alternate screen, which is left
/// again before this returns. Call it before the key reader starts: it reads
/// the replies from the terminal itself.
pub fn probe() -> Probe {
    let reply = query().unwrap_or_default();
    Probe {
        cell: parse_cell_reply(&reply)
            .or_else(window_cell_size)
            .unwrap_or(FALLBACK_CELL),
        theme: theme::parse_background_reply(&reply),
        background: theme::parse_background_colour(&reply),
        graphics: parse_graphics_reply(&reply),
    }
}

fn window_cell_size() -> Option<(u16, u16)> {
    cell_size(ratatui::crossterm::terminal::window_size().ok()?)
}

/// The size of one cell in the window `size` reports, if it reports pixels.
fn cell_size(size: ratatui::crossterm::terminal::WindowSize) -> Option<(u16, u16)> {
    if size.columns == 0 || size.rows == 0 {
        return None;
    }
    let cell = (size.width / size.columns, size.height / size.rows);
    (cell.0 > 0 && cell.1 > 0).then_some(cell)
}

/// The queries, in the order the replies are read.
fn queries() -> String {
    format!("\x1b]11;?\x1b\\\x1b[16t\x1b_Gi={QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c")
}

/// The terminal's replies to the start-up queries, read until the DA1 reply.
#[cfg(unix)]
fn query() -> Option<Vec<u8>> {
    use ratatui::crossterm::terminal;

    // SAFETY: isatty only looks at the descriptor.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return None;
    }
    let was_raw = terminal::is_raw_mode_enabled().ok()?;
    if !was_raw {
        terminal::enable_raw_mode().ok()?;
    }
    let reply = exchange(&mut io::stdout(), libc::STDIN_FILENO);
    if !was_raw {
        let _ = terminal::disable_raw_mode();
    }
    Some(reply)
}

/// Send the queries to `out` and read the replies from `fd` until the DA1
/// reply or the timeout.
#[cfg(unix)]
fn exchange(out: &mut impl Write, fd: libc::c_int) -> Vec<u8> {
    // On the alternate screen, so a terminal that prints the graphics query
    // as text (Terminal.app does) leaves nothing behind once it is left.
    let sent = write!(out, "\x1b[?1049h{}", queries())
        .and_then(|()| out.flush())
        .is_ok();
    let reply = if sent {
        read_replies(Instant::now() + REPLY_TIMEOUT, |left| read_chunk(fd, left))
    } else {
        Vec::new()
    };
    let _ = out.write_all(b"\x1b[?1049l").and_then(|()| out.flush());
    reply
}

/// Collect chunks from `next` until they hold the DA1 reply or `deadline`
/// passes. `next` is given the time left and returns `None` when nothing came
/// in that time or the input ended.
fn read_replies(deadline: Instant, mut next: impl FnMut(Duration) -> Option<Vec<u8>>) -> Vec<u8> {
    let mut buf = Vec::new();
    while !has_da1_reply(&buf) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(chunk) = next(left) else {
            break;
        };
        buf.extend_from_slice(&chunk);
    }
    buf
}

/// The bytes waiting on `fd`, waiting up to `left` for some to arrive.
#[cfg(unix)]
fn read_chunk(fd: libc::c_int, left: Duration) -> Option<Vec<u8>> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = left.as_millis().clamp(1, 1000) as libc::c_int;
    // SAFETY: one valid pollfd for the duration of the call.
    if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
        return None;
    }
    let mut chunk = [0u8; 256];
    // SAFETY: reads at most chunk.len() bytes into chunk.
    let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
    if n <= 0 {
        return None;
    }
    Some(chunk[..n as usize].to_vec())
}

#[cfg(not(unix))]
fn query() -> Option<Vec<u8>> {
    None
}

/// Whether `buf` holds the DA1 reply, `CSI ? ... c`.
fn has_da1_reply(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    text.split("\x1b[?").skip(1).any(|reply| {
        reply
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .is_some_and(|end| reply[end..].starts_with('c'))
    })
}

/// Whether `buf` holds a reply to the graphics query, `APC G i=31;... ST`.
/// An error reply counts too: the terminal still knows the protocol.
fn parse_graphics_reply(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    let id = format!("i={QUERY_ID}");
    text.split("\x1b_G").skip(1).any(|reply| {
        let keys = reply.split(';').next().unwrap_or("");
        keys.split(',').any(|key| key == id)
    })
}

/// Find the reply to `CSI 16 t`, `CSI 6 ; height ; width t`, in what the
/// terminal sent back.
fn parse_cell_reply(buf: &[u8]) -> Option<(u16, u16)> {
    let text = String::from_utf8_lossy(buf);
    for reply in text.split("\x1b[6;").skip(1) {
        let Some(end) = reply.find('t') else {
            continue;
        };
        let mut parts = reply[..end].split(';');
        let (Some(h), Some(w), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if let (Ok(h), Ok(w)) = (h.parse::<u16>(), w.parse::<u16>())
            && h > 0
            && w > 0
        {
            return Some((w, h));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

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
    fn cells_carry_both_diacritics_and_are_one_column() {
        assert_eq!(cell(0, 0), "\u{10EEEE}\u{0305}\u{0305}");
        assert_eq!(cell(2, 1), "\u{10EEEE}\u{030E}\u{030D}");
        // Every row and column the table covers makes a single-width cell.
        for n in 0..MAX_CELLS {
            assert_eq!(cell(n, MAX_CELLS - 1 - n).width(), 1, "cell {n}");
        }
    }

    #[test]
    fn colour_carries_the_id() {
        assert_eq!(colour(0x12_34_56), Color::Rgb(0x12, 0x34, 0x56));
    }

    #[test]
    fn upload_is_chunked_png_with_no_replies() {
        let data = vec![7u8; 3 * CHUNK / 4 + 1];
        let mut out = Vec::new();
        upload(&mut out, 42, &data).unwrap();
        let out = String::from_utf8(out).unwrap();
        let commands: Vec<&str> = out.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        assert_eq!(commands.len(), 2);
        assert!(commands[0].starts_with("\x1b_Ga=t,f=100,i=42,q=2,m=1;"));
        assert!(commands[1].starts_with("\x1b_Gm=0;"));
        let sent: String = commands
            .iter()
            .map(|c| &c[c.find(';').unwrap() + 1..])
            .collect();
        assert_eq!(sent, base64(&data));
    }

    #[test]
    fn place_and_delete_commands() {
        let mut out = Vec::new();
        place(&mut out, 9, 30, 4).unwrap();
        delete(&mut out, 9).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b_Ga=p,U=1,i=9,p=1,c=30,r=4,q=2\x1b\\\x1b_Ga=d,d=I,i=9,q=2\x1b\\"
        );
    }

    #[test]
    fn cell_reply_is_found_among_other_bytes() {
        assert_eq!(parse_cell_reply(b"\x1b[6;34;17t\x1b[0n"), Some((17, 34)));
        assert_eq!(parse_cell_reply(b"j\x1b[6;16;8tk\x1b[0n"), Some((8, 16)));
        assert_eq!(parse_cell_reply(b"\x1b[0n"), None);
        assert_eq!(parse_cell_reply(b"\x1b[6;0;0t"), None);
        assert_eq!(parse_cell_reply(b"\x1b[6;34"), None);
    }

    #[test]
    fn graphics_reply_is_found_by_its_id() {
        let kitty = b"\x1b[6;34;17t\x1b_Gi=31;OK\x1b\\\x1b[?62;22c";
        assert!(parse_graphics_reply(kitty));
        assert!(parse_graphics_reply(b"\x1b_Gi=31;ENOTSUPPORTED:x\x1b\\"));
        // DA1 alone: no graphics.
        assert!(!parse_graphics_reply(b"\x1b[6;34;17t\x1b[?1;2c"));
        assert!(!parse_graphics_reply(b"\x1b_Gi=310;OK\x1b\\"));
        assert!(!parse_graphics_reply(b""));
    }

    #[test]
    fn da1_reply_ends_the_wait() {
        assert!(has_da1_reply(b"\x1b[?62;22c"));
        assert!(has_da1_reply(b"\x1b_Gi=31;OK\x1b\\\x1b[?1;2c"));
        assert!(has_da1_reply(b"\x1b[?6c"));
        assert!(!has_da1_reply(b"\x1b[?62;22"));
        assert!(!has_da1_reply(b"\x1b[6;34;17t"));
        assert!(!has_da1_reply(b"\x1b[?1;2t"));
    }

    #[test]
    fn queries_end_with_da1() {
        assert_eq!(
            queries(),
            "\x1b]11;?\x1b\\\x1b[16t\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c"
        );
    }
}
