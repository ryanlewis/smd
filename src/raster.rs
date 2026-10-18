//! Drawing text into images: a font from the system, an RGBA canvas and PNG
//! encoding.

use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};

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

fn load((path, index): Face) -> Option<FontVec> {
    let data = std::fs::read(path).ok()?;
    FontVec::try_from_vec_and_index(data, index).ok()
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

    pub fn fill(&mut self, x: i64, y: i64, w: i64, h: i64, colour: Rgba) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, colour, 1.0);
            }
        }
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

    pub fn png(&self) -> Result<Png, String> {
        Png::new(self.encode()?)
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        let mut data = Vec::new();
        let mut encoder = png::Encoder::new(&mut data, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(self.pixels.as_flattened())
            .map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
        Ok(data)
    }
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
    fn canvas_encodes_as_png_of_its_size() {
        let mut canvas = Canvas::new(30, 7);
        canvas.fill(0, 0, 3, 3, [255, 0, 0, 255]);
        let data = canvas.encode().unwrap();
        // Width and height sit at the start of the IHDR chunk.
        assert_eq!(data[16..24], crate::images::tests::png(30, 7)[16..24]);
        assert!(canvas.png().is_ok());
    }
}
