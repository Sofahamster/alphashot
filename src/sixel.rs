//! Sixel support for the result thumbnail.
//!
//! Encoding uses `icy_sixel`. Terminals report no reliable cell size, so this
//! module assumes one and never queries the terminal.

use anyhow::Result;
use image::{Rgba, RgbaImage, imageops};

/// Assumed terminal cell size in pixels, as `(width, height)`.
///
/// Windows Terminal (`WT_SESSION`) gets 10x20; other terminals get 6x12.
pub fn cell_pixels() -> (u32, u32) {
    // Windows Terminal renders Sixel on a virtual 10x20 grid, independent of font zoom.
    if std::env::var_os("WT_SESSION").is_some() {
        (10, 20)
    } else {
        (6, 12)
    }
}

/// Pixel size of a `columns` x `rows` cell area, height rounded down to a multiple of 6.
pub fn bounds(columns: u16, rows: u16) -> (u32, u32) {
    let (w, h) = cell_pixels();
    // Sixel paints six-pixel bands; round down so the last band stays inside the panel.
    (u32::from(columns) * w, u32::from(rows) * h / 6 * 6)
}

/// Encodes `image` as a Sixel escape string (DCS header through string terminator).
pub fn encode(image: &RgbaImage) -> Result<String> {
    icy_sixel::sixel_string(
        image.as_raw(),
        image.width() as i32,
        image.height() as i32,
        icy_sixel::PixelFormat::RGBA8888,
        icy_sixel::DiffusionMethod::Auto,
        icy_sixel::MethodForLargest::Auto,
        icy_sixel::MethodForRep::Auto,
        icy_sixel::Quality::AUTO,
    )
    .map_err(|error| anyhow::anyhow!("Sixel encoding failed: {error}"))
}

/// Scales `image` down to fit `width` x `height`, keeping the aspect ratio, and
/// returns it over an opaque white canvas.
///
/// Never enlarges. Sixel has no alpha; the white backing keeps transparent
/// regions visible on dark terminals.
pub fn fit(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let scale = (f64::from(width) / f64::from(image.width()))
        .min(f64::from(height) / f64::from(image.height()))
        .min(1.0);
    let width = (f64::from(image.width()) * scale).round().max(1.0) as u32;
    let height = (f64::from(image.height()) * scale).round().max(1.0) as u32;
    let resized = imageops::thumbnail(image, width, height);
    let mut canvas = RgbaImage::from_pixel(width, height, Rgba([255; 4]));
    imageops::overlay(&mut canvas, &resized, 0, 0);
    canvas
}
