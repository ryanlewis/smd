//! Display maths drawn as an image with `latex-rust`, a pure-Rust TeX layout
//! engine with STIX Two Math built in.
//!
//! The image has a transparent background and glyphs in a colour that reads on
//! the terminal's background. It is drawn at a fixed pixel size, so a resize
//! only places it again.

use latex_rust::{
    Color, Dim, MathFont, MathStyle, PngBackground, PngOptions, layout_with_em_size_pt, parse,
    render_png,
};

use crate::images::Png;

/// Em size in points; with [`DPI`] this is 28 px, close to the text in a
/// terminal with 17x34 px cells.
const FONT_SIZE_PT: i64 = 12;
const DPI: i64 = 168;
const EM_PX: i64 = FONT_SIZE_PT * DPI / 72;

/// The largest width or height drawn, in pixels. Anything bigger, such as
/// `\rule{1em}{3000em}`, stays as text rather than taking gigabytes.
const MAX_SIDE: u32 = 4096;

/// Whether the terminal's background is dark.
///
/// Always dark for now; this is the one place to read the background from
/// once start-up detection lands.
pub fn dark_background() -> bool {
    true
}

/// The glyph colour for a dark or a light background.
fn ink(dark: bool) -> Color {
    if dark {
        Color::rgb(0xe6, 0xe6, 0xe6)
    } else {
        Color::rgb(0x1a, 0x1a, 0x1a)
    }
}

/// Typeset `source`, LaTeX maths without the `$$`, in display style.
pub fn render(source: &str, dark: bool) -> Result<Png, String> {
    Png::new(png(source, dark)?)
}

fn png(source: &str, dark: bool) -> Result<Vec<u8>, String> {
    let font = MathFont::stix_two_math().map_err(|e| e.to_string())?;
    let mut options = PngOptions::new();
    options.font_size_pt = Dim::from_i64(FONT_SIZE_PT);
    options.dpi = Dim::from_i64(DPI);
    options.color = ink(dark);
    options.background = PngBackground::Transparent;
    let ast = parse(source).map_err(|e| e.to_string())?;
    let tree = layout_with_em_size_pt(&ast, &font, MathStyle::Display, &options.font_size_pt)
        .map_err(|e| e.to_string())?;
    let em = Dim::from_i64(EM_PX);
    let px = |d: Dim| (d * &em).ceil_to_u32().map_err(|e| e.to_string());
    let (w, h) = (px(tree.width.clone())?, px(&tree.height + &tree.depth)?);
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("maths too large: {w}x{h} px"));
    }
    render_png(&tree, &font, &options).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_sum_and_matrix_render() {
        for source in [
            r"\frac{a+b}{c^2}",
            r"\sum_{k=1}^{n} k = \frac{n(n+1)}{2}",
            r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
        ] {
            assert!(render(source, true).is_ok(), "{source}");
        }
    }

    #[test]
    fn a_fraction_is_taller_than_one_line() {
        // The height is the second number in the PNG header.
        let height = |source| {
            let data = png(source, true).unwrap();
            u32::from_be_bytes(data[20..24].try_into().unwrap())
        };
        assert!(height(r"\frac{1}{2}") > height("x"));
    }

    #[test]
    fn broken_maths_is_an_error() {
        assert!(render(r"\frac{1}{", true).is_err());
        assert!(render(r"\notacommand{x}", true).is_err());
    }

    #[test]
    fn huge_maths_is_an_error_not_a_huge_image() {
        assert!(render(r"\hspace{3000em}\rule{1em}{3000em}", true).is_err());
        assert!(render(r"\rule{1em}{200em}", true).is_err());
    }

    #[test]
    fn the_ink_differs_by_background() {
        assert_ne!(png("x", true), png("x", false));
    }
}
