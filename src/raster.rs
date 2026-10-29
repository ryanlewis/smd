//! Drawing text into images: a font from the system, an RGBA canvas and PNG
//! encoding.

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, FontVec, GlyphImageFormat, PxScale, ScaleFont, point};

use crate::images::Png;

/// A font file and, for a `.ttc` holding several faces, which one.
type Face = (&'static str, u32);

/// Monospace fonts to look for, regular then bold.
const FONTS: &[(Face, Face)] = &[
    (
        ("/System/Library/Fonts/Menlo.ttc", 0),
        ("/System/Library/Fonts/Menlo.ttc", 1),
    ),
    (
        ("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 0),
        (
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
            0,
        ),
    ),
    (
        ("/usr/share/fonts/TTF/DejaVuSansMono.ttf", 0),
        ("/usr/share/fonts/TTF/DejaVuSansMono-Bold.ttf", 0),
    ),
    (
        (
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            0,
        ),
        (
            "/usr/share/fonts/truetype/liberation/LiberationMono-Bold.ttf",
            0,
        ),
    ),
];

pub struct Fonts {
    pub regular: FontVec,
    bold: Option<FontVec>,
}

impl Fonts {
    /// The bold face, or the regular one when there is none.
    pub fn bold(&self) -> &FontVec {
        self.bold.as_ref().unwrap_or(&self.regular)
    }
}

/// The system's monospace font, read once.
pub fn fonts() -> Result<&'static Fonts, String> {
    static FONTS_FOUND: OnceLock<Option<Fonts>> = OnceLock::new();
    FONTS_FOUND
        .get_or_init(|| {
            FONTS.iter().find_map(|&(regular, bold)| {
                let regular = load(regular)?;
                let bold = load(bold);
                Some(Fonts { regular, bold })
            })
        })
        .as_ref()
        .ok_or_else(|| "no monospace font found".to_string())
}

/// Bold proportional fonts to look for, for headings.
const SANS_BOLD: &[Face] = &[
    ("/System/Library/Fonts/Helvetica.ttc", 1),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 0),
    ("/usr/share/fonts/TTF/DejaVuSans-Bold.ttf", 0),
    (
        "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
        0,
    ),
];

/// The system's bold proportional font, read once.
pub fn sans_bold() -> Result<&'static FontVec, String> {
    static FOUND: OnceLock<Option<FontVec>> = OnceLock::new();
    FOUND
        .get_or_init(|| SANS_BOLD.iter().find_map(|&face| load(face)))
        .as_ref()
        .ok_or_else(|| "no bold sans-serif font found".to_string())
}

/// Every bold proportional font `sans_bold` could pick on this machine.
#[cfg(test)]
pub fn sans_bold_all() -> Vec<FontVec> {
    SANS_BOLD.iter().filter_map(|&face| load(face)).collect()
}

fn load((path, index): Face) -> Option<FontVec> {
    let data = std::fs::read(path).ok()?;
    FontVec::try_from_vec_and_index(data, index).ok()
}

/// Colour emoji fonts to look for, for emoji in headings.
const EMOJI: &[Face] = &[
    ("/System/Library/Fonts/Apple Color Emoji.ttc", 0),
    ("/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf", 0),
    ("/usr/share/fonts/noto/NotoColorEmoji.ttf", 0),
    ("/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf", 0),
];

/// A colour emoji font, whose glyphs are PNG images rather than outlines.
pub struct Emoji(FontRef<'static>);

/// The system's colour emoji font, read once.
pub fn emoji() -> Option<&'static Emoji> {
    static FOUND: OnceLock<Option<Emoji>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            EMOJI.iter().find_map(|&(path, index)| {
                let data = map(path)?;
                FontRef::try_from_slice_and_index(data, index)
                    .ok()
                    .map(Emoji)
            })
        })
        .as_ref()
}

/// The file at `path`, mapped into memory for the rest of the run. Apple's
/// emoji font is about 200 MB, and a heading reads a few of its images.
#[cfg(unix)]
fn map(path: &str) -> Option<&'static [u8]> {
    use std::os::fd::AsRawFd;

    let file = std::fs::File::open(path).ok()?;
    let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
    if len == 0 {
        return None;
    }
    // SAFETY: a read-only private mapping of a whole file that stays open
    // until the mapping is made. It is never unmapped, so the slice lives as
    // long as the program. System fonts are not rewritten in place.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: the mapping is `len` readable bytes and is never unmapped.
    Some(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

#[cfg(not(unix))]
fn map(path: &str) -> Option<&'static [u8]> {
    Some(std::fs::read(path).ok()?.leak())
}

impl Emoji {
    /// Whether the font has a colour image for `c`.
    pub fn has(&self, c: char) -> bool {
        let id = self.0.glyph_id(c);
        id.0 != 0
            && self
                .0
                .glyph_raster_image2(id, 64)
                .is_some_and(|img| matches!(img.format, GlyphImageFormat::Png))
    }

    /// The image for `c` from the strike nearest `size` pixels per em, and
    /// that strike's pixels per em.
    pub fn bitmap(&self, c: char, size: f32) -> Option<(Bitmap, f32)> {
        let id = self.0.glyph_id(c);
        let want = size.ceil().clamp(1.0, f32::from(u16::MAX)) as u16;
        let img = self.0.glyph_raster_image2(id, want)?;
        if !matches!(img.format, GlyphImageFormat::Png) || img.pixels_per_em == 0 {
            return None;
        }
        Some((Bitmap::decode(img.data)?, f32::from(img.pixels_per_em)))
    }
}

/// A decoded image in straight RGBA.
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pixels: Vec<Rgba>,
}

impl Bitmap {
    /// Decode a PNG of any colour type to RGBA.
    pub fn decode(data: &[u8]) -> Option<Bitmap> {
        let mut decoder = png::Decoder::new(data);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        let bytes = &buf[..info.buffer_size()];
        let pixels: Vec<Rgba> = match info.color_type {
            png::ColorType::Rgba => bytes.as_chunks::<4>().0.to_vec(),
            png::ColorType::Rgb => bytes
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            png::ColorType::GrayscaleAlpha => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            png::ColorType::Grayscale => bytes.iter().map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::Indexed => return None,
        };
        (pixels.len() == info.width as usize * info.height as usize).then_some(Bitmap {
            width: info.width,
            height: info.height,
            pixels,
        })
    }

    /// The average colour of the area from `left`, `top` to `right`,
    /// `bottom`, in pixels, weighted by how much of each pixel it covers.
    fn average(&self, left: f32, top: f32, right: f32, bottom: f32) -> Rgba {
        let mut sum = [0.0f32; 4];
        let mut area = 0.0;
        for y in top.floor() as u32..(bottom.ceil() as u32).min(self.height) {
            let h = (bottom.min(y as f32 + 1.0) - top.max(y as f32)).max(0.0);
            for x in left.floor() as u32..(right.ceil() as u32).min(self.width) {
                let w = (right.min(x as f32 + 1.0) - left.max(x as f32)).max(0.0);
                let p = self.pixels[(y * self.width + x) as usize];
                let a = f32::from(p[3]) / 255.0 * w * h;
                for i in 0..3 {
                    sum[i] += f32::from(p[i]) * a;
                }
                sum[3] += a;
                area += w * h;
            }
        }
        if sum[3] <= 0.0 || area <= 0.0 {
            return [0; 4];
        }
        [
            (sum[0] / sum[3]).round() as u8,
            (sum[1] / sum[3]).round() as u8,
            (sum[2] / sum[3]).round() as u8,
            (sum[3] / area * 255.0).round() as u8,
        ]
    }
}

/// Straight (not premultiplied) RGBA.
pub type Rgba = [u8; 4];

/// An image to draw on, transparent to start with.
pub struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<Rgba>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            pixels: vec![[0; 4]; width as usize * height as usize],
        }
    }

    #[cfg(test)]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[cfg(test)]
    pub fn pixels(&self) -> impl Iterator<Item = &Rgba> {
        self.pixels.iter()
    }

    /// Paint `colour` over the pixel at `x`, `y`, `coverage` of it covered.
    /// Points off the canvas are skipped.
    pub fn blend(&mut self, x: i64, y: i64, colour: Rgba, coverage: f32) {
        if x < 0 || y < 0 || x >= i64::from(self.width) || y >= i64::from(self.height) {
            return;
        }
        let dst = &mut self.pixels[y as usize * self.width as usize + x as usize];
        let a = f32::from(colour[3]) / 255.0 * coverage.clamp(0.0, 1.0);
        let da = f32::from(dst[3]) / 255.0;
        let out = a + da * (1.0 - a);
        if out <= 0.0 {
            return;
        }
        for i in 0..3 {
            let c = (f32::from(colour[i]) * a + f32::from(dst[i]) * da * (1.0 - a)) / out;
            dst[i] = c.round() as u8;
        }
        dst[3] = (out * 255.0).round() as u8;
    }

    /// Draw `text` from `x` on the baseline `y`. Returns where it ends.
    pub fn text(
        &mut self,
        font: &FontVec,
        size: f32,
        x: f32,
        y: f32,
        text: &str,
        colour: Rgba,
    ) -> f32 {
        let scaled = font.as_scaled(PxScale::from(size));
        let mut x = x;
        let mut previous = None;
        for c in text.chars() {
            let id = scaled.glyph_id(c);
            if let Some(previous) = previous {
                x += scaled.kern(previous, id);
            }
            let glyph = id.with_scale_and_position(size, point(x, y));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i64 + i64::from(gx);
                    let py = bounds.min.y as i64 + i64::from(gy);
                    self.blend(px, py, colour, coverage);
                });
            }
            x += scaled.h_advance(id);
            previous = Some(id);
        }
        x
    }

    /// Draw `bitmap` scaled into the box from `x`, `y`, `width` by `height`
    /// pixels. Each pixel takes the average of the image under it.
    pub fn image(&mut self, bitmap: &Bitmap, x: f32, y: f32, width: f32, height: f32) {
        if width <= 0.0 || height <= 0.0 || bitmap.width == 0 || bitmap.height == 0 {
            return;
        }
        let (sx, sy) = (bitmap.width as f32 / width, bitmap.height as f32 / height);
        let x0 = x.floor() as i64;
        let y0 = y.floor() as i64;
        let x1 = (x + width).ceil() as i64;
        let y1 = (y + height).ceil() as i64;
        for py in y0..y1 {
            // The part of the image row range under this pixel.
            let top = ((py as f32 - y) * sy).max(0.0);
            let bottom = ((py as f32 + 1.0 - y) * sy).min(bitmap.height as f32);
            if bottom <= top {
                continue;
            }
            for px in x0..x1 {
                let left = ((px as f32 - x) * sx).max(0.0);
                let right = ((px as f32 + 1.0 - x) * sx).min(bitmap.width as f32);
                if right <= left {
                    continue;
                }
                let colour = bitmap.average(left, top, right, bottom);
                // Partly covered edge pixels take part of the colour.
                let cover = (right - left) / sx * (bottom - top) / sy;
                self.blend(px, py, colour, cover);
            }
        }
    }

    pub fn png(&self) -> Result<Png, String> {
        Png::new(self.encode()?)
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        encode(
            self.width,
            self.height,
            png::ColorType::Rgba,
            self.pixels.as_flattened(),
        )
    }
}

/// PNG bytes for 8-bit `pixels` in `color`, row by row.
pub fn encode(
    width: u32,
    height: u32,
    color: png::ColorType,
    pixels: &[u8],
) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let mut encoder = png::Encoder::new(&mut data, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(pixels).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(data)
}

/// How wide `text` is at `size` pixels.
pub fn measure(font: &FontVec, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(previous) = previous {
            width += scaled.kern(previous, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

/// The pixels `text` covers when drawn from `x` on the baseline 0, as left,
/// top, right and bottom, or `None` when it has no ink.
pub fn ink(font: &FontVec, size: f32, x: f32, text: &str) -> Option<[f32; 4]> {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut x = x;
    let mut previous = None;
    let mut out: Option<[f32; 4]> = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(previous) = previous {
            x += scaled.kern(previous, id);
        }
        if let Some(outline) = font.outline_glyph(id.with_scale_and_position(size, point(x, 0.0))) {
            let b = outline.px_bounds();
            out = Some(match out {
                None => [b.min.x, b.min.y, b.max.x, b.max.y],
                Some([l, t, r, bt]) => [
                    l.min(b.min.x),
                    t.min(b.min.y),
                    r.max(b.max.x),
                    bt.max(b.max.y),
                ],
            });
        }
        x += scaled.h_advance(id);
        previous = Some(id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending_over_transparent_keeps_the_colour() {
        let mut canvas = Canvas::new(2, 1);
        canvas.blend(0, 0, [200, 100, 50, 255], 0.5);
        assert_eq!(canvas.pixels[0], [200, 100, 50, 128]);
        canvas.blend(0, 0, [0, 0, 0, 255], 1.0);
        assert_eq!(canvas.pixels[0], [0, 0, 0, 255]);
        // Off the canvas is ignored.
        canvas.blend(-1, 0, [0, 0, 0, 255], 1.0);
        canvas.blend(2, 0, [0, 0, 0, 255], 1.0);
        assert_eq!(canvas.pixels[1], [0; 4]);
    }

    #[test]
    fn an_image_is_averaged_into_its_box() {
        // Two by one: opaque red, then clear. Squeezed into one pixel, it is
        // red at half cover.
        let mut canvas = Canvas::new(2, 1);
        canvas.blend(0, 0, [255, 0, 0, 255], 1.0);
        let bitmap = Bitmap::decode(&canvas.encode().unwrap()).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (2, 1));
        let mut out = Canvas::new(1, 1);
        out.image(&bitmap, 0.0, 0.0, 1.0, 1.0);
        assert_eq!(out.pixels[0], [255, 0, 0, 128]);
        // Stretched to four pixels, it is red then clear.
        let mut out = Canvas::new(4, 1);
        out.image(&bitmap, 0.0, 0.0, 4.0, 1.0);
        assert_eq!(out.pixels[1], [255, 0, 0, 255]);
        assert_eq!(out.pixels[2], [0; 4]);
    }

    #[test]
    fn canvas_encodes_as_png_of_its_size() {
        let mut canvas = Canvas::new(30, 7);
        canvas.blend(1, 1, [255, 0, 0, 255], 1.0);
        let data = canvas.encode().unwrap();
        // Width and height sit at the start of the IHDR chunk.
        assert_eq!(data[16..24], crate::images::tests::png(30, 7)[16..24]);
        assert!(canvas.png().is_ok());
    }

    /// A PNG of `width` by `height` with the given colour type and depth.
    fn png_of(
        (width, height): (u32, u32),
        color: png::ColorType,
        depth: png::BitDepth,
        palette: Option<(&[u8], Option<&[u8]>)>,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        if let Some((colours, alpha)) = palette {
            encoder.set_palette(colours.to_vec());
            if let Some(alpha) = alpha {
                encoder.set_trns(alpha.to_vec());
            }
        }
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    fn pixels_of(bitmap: &Bitmap) -> &[Rgba] {
        &bitmap.pixels
    }

    #[test]
    fn decode_keeps_rgba_and_adds_opaque_alpha_to_rgb() {
        let rgba = png_of(
            (2, 1),
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            None,
            &[1, 2, 3, 4, 5, 6, 7, 8],
        );
        let bitmap = Bitmap::decode(&rgba).unwrap();
        assert_eq!(pixels_of(&bitmap), [[1, 2, 3, 4], [5, 6, 7, 8]]);
        let rgb = png_of(
            (2, 1),
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            None,
            &[1, 2, 3, 4, 5, 6],
        );
        let bitmap = Bitmap::decode(&rgb).unwrap();
        assert_eq!(pixels_of(&bitmap), [[1, 2, 3, 255], [4, 5, 6, 255]]);
    }

    #[test]
    fn decode_spreads_grey_over_the_colour_channels() {
        let grey = png_of(
            (2, 1),
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            None,
            &[10, 200],
        );
        let bitmap = Bitmap::decode(&grey).unwrap();
        assert_eq!(
            pixels_of(&bitmap),
            [[10, 10, 10, 255], [200, 200, 200, 255]]
        );
        let with_alpha = png_of(
            (2, 1),
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            None,
            &[10, 20, 200, 255],
        );
        let bitmap = Bitmap::decode(&with_alpha).unwrap();
        assert_eq!(pixels_of(&bitmap), [[10, 10, 10, 20], [200, 200, 200, 255]]);
    }

    #[test]
    fn decode_scales_low_bit_depth_grey_to_eight_bits() {
        // One bit per pixel, packed from the high bit: black, white.
        let one = png_of(
            (2, 1),
            png::ColorType::Grayscale,
            png::BitDepth::One,
            None,
            &[0b0100_0000],
        );
        let bitmap = Bitmap::decode(&one).unwrap();
        assert_eq!(pixels_of(&bitmap), [[0, 0, 0, 255], [255, 255, 255, 255]]);
        // Four bits per pixel: 0x0 and 0xF.
        let four = png_of(
            (2, 1),
            png::ColorType::Grayscale,
            png::BitDepth::Four,
            None,
            &[0x0F],
        );
        let bitmap = Bitmap::decode(&four).unwrap();
        assert_eq!(pixels_of(&bitmap), [[0, 0, 0, 255], [255, 255, 255, 255]]);
    }

    #[test]
    fn decode_keeps_the_high_byte_of_sixteen_bit_samples() {
        let rgba = png_of(
            (1, 1),
            png::ColorType::Rgba,
            png::BitDepth::Sixteen,
            None,
            &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
        );
        let bitmap = Bitmap::decode(&rgba).unwrap();
        assert_eq!(pixels_of(&bitmap), [[0x12, 0x56, 0x9A, 0xDE]]);
        let grey = png_of(
            (1, 1),
            png::ColorType::Grayscale,
            png::BitDepth::Sixteen,
            None,
            &[0x80, 0xFF],
        );
        let bitmap = Bitmap::decode(&grey).unwrap();
        assert_eq!(pixels_of(&bitmap), [[0x80, 0x80, 0x80, 255]]);
    }

    #[test]
    fn decode_looks_up_palette_colours_and_their_transparency() {
        let colours = [255, 0, 0, 0, 255, 0];
        let opaque = png_of(
            (2, 1),
            png::ColorType::Indexed,
            png::BitDepth::Eight,
            Some((&colours, None)),
            &[1, 0],
        );
        let bitmap = Bitmap::decode(&opaque).unwrap();
        assert_eq!(pixels_of(&bitmap), [[0, 255, 0, 255], [255, 0, 0, 255]]);
        // A tRNS chunk gives the first entry half cover and leaves the rest
        // opaque.
        let clear = png_of(
            (2, 1),
            png::ColorType::Indexed,
            png::BitDepth::Eight,
            Some((&colours, Some(&[128]))),
            &[0, 1],
        );
        let bitmap = Bitmap::decode(&clear).unwrap();
        assert_eq!(pixels_of(&bitmap), [[255, 0, 0, 128], [0, 255, 0, 255]]);
        // Packed two bits per pixel.
        let packed = png_of(
            (2, 1),
            png::ColorType::Indexed,
            png::BitDepth::Two,
            Some((&colours, None)),
            &[0b0100_0000],
        );
        let bitmap = Bitmap::decode(&packed).unwrap();
        assert_eq!(pixels_of(&bitmap), [[0, 255, 0, 255], [255, 0, 0, 255]]);
    }

    #[test]
    fn decode_refuses_data_that_is_not_a_whole_png() {
        assert!(Bitmap::decode(&[]).is_none());
        assert!(Bitmap::decode(b"not a png at all").is_none());
        let good = png_of(
            (4, 4),
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            None,
            &[9; 64],
        );
        assert!(Bitmap::decode(&good).is_some());
        // Cut off in the header, and in the middle of the image data.
        assert!(Bitmap::decode(&good[..20]).is_none());
        assert!(Bitmap::decode(&good[..good.len() - 20]).is_none());
        // A flipped byte in the compressed data fails the checksum.
        let mut bad = good.clone();
        let at = bad.len() - 20;
        bad[at] ^= 0xFF;
        assert!(Bitmap::decode(&bad).is_none());
    }

    #[test]
    fn encode_reports_pixels_that_do_not_fit_the_size() {
        assert!(encode(2, 2, png::ColorType::Rgba, &[0; 3]).is_err());
        let data = encode(2, 1, png::ColorType::Grayscale, &[7, 9]).unwrap();
        let bitmap = Bitmap::decode(&data).unwrap();
        assert_eq!(pixels_of(&bitmap), [[7, 7, 7, 255], [9, 9, 9, 255]]);
    }
}
