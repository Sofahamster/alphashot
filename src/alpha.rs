//! Recovers straight (unpremultiplied) RGBA from a white and a black composite of the same
//! window, draws cursor overlays and crops to the visible content.
//!
//! # Model
//!
//! Desktop composition blends window color `C` with coverage `α` over the backdrop, per channel
//! in linear light. With a white (1.0) and a black (0.0) backdrop:
//!
//! ```text
//! W = α·C + 1 − α
//! B = α·C
//! α = 1 − (W − B)
//! C = B / α
//! ```
//!
//! # Implementation
//!
//! - A 256-entry table decodes the 8-bit sRGB inputs to linear light. The model holds only in
//!   linear light; solving on encoded values makes semi-transparent shadows too dark or too
//!   light.
//! - Each channel yields its own `W − B`. The solver takes the median of the three, so one
//!   channel thrown off by quantization, clipping or colored subpixel text cannot move `α`. The
//!   minimum or maximum would follow that outlier. `α` is clamped to `[0, 1]`.
//! - Color is `B / max(α, epsilon)`, encoded to sRGB and rounded to a byte with ties to even.
//!   Pixels with `α <= epsilon` become `[0, 0, 0, 0]`, which avoids division by zero.
//! - [`AlphaOptions::opaque_regions`] get `α = 1` and the black-pass color. Client content such
//!   as a caret or an animation can change between passes, and the difference would read as
//!   transparency and cut holes into the window.
//! - [`Overlay`]s are composited after the solve with Porter-Duff "over" on sRGB bytes, in list
//!   order. Windows draws the cursor the same way on top of the composed desktop. Over a
//!   transparent pixel the result is the overlay's own color and alpha, untinted by either
//!   backdrop.
//! - With [`AlphaOptions::crop`] set, the output is cropped to the bounding box of pixels whose
//!   final alpha exceeds [`AlphaOptions::crop_threshold`], grown by [`AlphaOptions::margin`] and
//!   clamped to the canvas. Overlays count toward the box. If no pixel passes, the full canvas is
//!   returned.
//!
//! # Limits
//!
//! The composites must share non-zero dimensions of at most 40 million pixels, which bounds the
//! two linear `f32` buffers (24 bytes per pixel). Opaque regions must lie inside the canvas,
//! `epsilon` in `(0, 1]` and `crop_threshold` in `[0, 1]`.
//!
//! Alignment: the inputs must be pixel-aligned captures of the same geometry. Blur materials that
//! sample the backdrop do not follow the model and recover as an approximation.
use anyhow::{Result, ensure};
use image::{RgbImage, Rgba, RgbaImage};

/// Parameters for [`recover`]; the default crops to every non-transparent pixel.
#[derive(Debug, Clone)]
pub struct AlphaOptions {
    /// Crops to the bounding box of pixels with alpha above `crop_threshold`.
    pub crop: bool,
    /// Alpha in `[0, 1]` a pixel must exceed to count for cropping; `0.0` keeps faint shadows.
    pub crop_threshold: f32,
    /// Pixels added on each side of the crop box.
    pub margin: u32,
    /// Alpha in `(0, 1]` at or below which a pixel becomes transparent black.
    pub epsilon: f32,
    /// Rectangles in composite pixels that stay opaque with the black-pass color.
    pub opaque_regions: Vec<image::math::Rect>,
    /// Layers drawn in order over the recovered image, before cropping.
    pub overlays: Vec<Overlay>,
}

/// Straight-alpha sRGB layer, such as the cursor, kept out of the alpha solve and clipped to the
/// canvas.
#[derive(Debug, Clone)]
pub struct Overlay {
    /// Layer pixels.
    pub image: RgbaImage,
    /// Left edge in composite pixels; may be negative.
    pub x: i32,
    /// Top edge in composite pixels; may be negative.
    pub y: i32,
}

impl Default for AlphaOptions {
    fn default() -> Self {
        Self {
            crop: true,
            crop_threshold: 0.0,
            margin: 0,
            epsilon: 1e-6,
            opaque_regions: Vec::new(),
            overlays: Vec::new(),
        }
    }
}

/// Output of [`recover`].
pub struct RecoveredImage {
    /// Straight-alpha sRGB pixels.
    pub image: RgbaImage,
}

/// sRGB decode per IEC 61966-2-1.
pub(crate) fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB encode; clamps first because quantization can push `B / α` above 1.
fn linear_to_srgb(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.0031308 {
        12.92 * value
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// Rounds `[0, 1]` to a byte with ties to even, so midpoints do not bias upward.
pub(crate) fn byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round_ties_even() as u8
}

/// Decodes a composite to linear light through a lookup table.
fn linear(image: &RgbImage) -> Vec<[f32; 3]> {
    let lut: [f32; 256] = std::array::from_fn(|i| srgb_to_linear(i as f32 / 255.0));
    image
        .pixels()
        .map(|p| p.0.map(|v| lut[v as usize]))
        .collect()
}

/// Recovers straight RGBA from a white and a black composite; see the [module docs](self).
///
/// # Errors
///
/// Fails if the composites are empty, differ in size or exceed 40 million pixels, if an opaque
/// region leaves the canvas, or if `epsilon` or `crop_threshold` is out of range or not finite.
pub fn recover(
    white: &RgbImage,
    black: &RgbImage,
    options: &AlphaOptions,
) -> Result<RecoveredImage> {
    ensure!(
        white.width() > 0 && white.height() > 0,
        "Composite images must not be empty"
    );
    ensure!(
        white.dimensions() == black.dimensions(),
        "White and black composites must have the same size"
    );
    ensure!(
        options.epsilon.is_finite() && options.epsilon > 0.0 && options.epsilon <= 1.0,
        "Epsilon must be in (0, 1]"
    );
    ensure!(
        options.crop_threshold.is_finite() && (0.0..=1.0).contains(&options.crop_threshold),
        "Crop threshold must be in [0, 1]"
    );
    let (width, height) = white.dimensions();
    ensure!(
        options.opaque_regions.iter().all(|r| r.width > 0
            && r.height > 0
            && r.x.saturating_add(r.width) <= width
            && r.y.saturating_add(r.height) <= height),
        "Opaque regions must fit inside the composites"
    );
    // Bounds the two linear buffers.
    ensure!(
        u64::from(width) * u64::from(height) <= 40_000_000,
        "Composite canvas exceeds 40 million pixels"
    );
    let (w, b) = (linear(white), linear(black));
    let mut image = RgbaImage::new(width, height);
    // Content box as (left, top, right, bottom), right and bottom exclusive.
    let mut crop = (width, height, 0, 0);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let i = (y * width + x) as usize;
        let (white, black) = (w[i], b[i]);
        let mut difference = std::array::from_fn::<_, 3, _>(|c| white[c] - black[c]);
        // The median ignores one outlier channel.
        difference.sort_unstable_by(f32::total_cmp);
        let opaque = options
            .opaque_regions
            .iter()
            .any(|r| x >= r.x && x - r.x < r.width && y >= r.y && y - r.y < r.height);
        let mut alpha = if opaque {
            1.0
        } else {
            (1.0 - difference[1]).clamp(0.0, 1.0)
        };
        let mut color = black.map(|c| {
            if alpha <= options.epsilon {
                0
            } else {
                byte(linear_to_srgb(c / alpha.max(options.epsilon)))
            }
        });
        for overlay in &options.overlays {
            let ox = i64::from(x) - i64::from(overlay.x);
            let oy = i64::from(y) - i64::from(overlay.y);
            if ox < 0
                || oy < 0
                || ox >= i64::from(overlay.image.width())
                || oy >= i64::from(overlay.image.height())
            {
                continue;
            }
            // Porter-Duff "over" in sRGB, matching how Windows draws the cursor.
            let over = overlay.image[(ox as u32, oy as u32)];
            let over_alpha = f32::from(over[3]) / 255.0;
            let composed = 1.0 - (1.0 - over_alpha) * (1.0 - alpha);
            if composed > 0.0 {
                color = std::array::from_fn(|c| {
                    ((f32::from(over[c]) * over_alpha
                        + f32::from(color[c]) * alpha * (1.0 - over_alpha))
                        / composed)
                        .round_ties_even() as u8
                });
            }
            alpha = composed;
        }
        *pixel = Rgba([color[0], color[1], color[2], byte(alpha)]);
        if alpha > options.crop_threshold {
            crop = (
                crop.0.min(x),
                crop.1.min(y),
                crop.2.max(x + 1),
                crop.3.max(y + 1),
            );
        }
    }
    if options.crop && crop.0 < crop.2 && crop.1 < crop.3 {
        let x = crop.0.saturating_sub(options.margin);
        let y = crop.1.saturating_sub(options.margin);
        let right = crop.2.saturating_add(options.margin).min(width);
        let bottom = crop.3.saturating_add(options.margin).min(height);
        image = image::imageops::crop_imm(&image, x, y, right - x, bottom - y).to_image();
    }
    Ok(RecoveredImage { image })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn opaque_client_keeps_one_frame_without_cutting_holes_in_animations() {
        let mut white = RgbImage::from_pixel(12, 10, Rgb([255; 3]));
        let mut black = RgbImage::new(12, 10);
        for y in 2..8 {
            for x in 2..10 {
                white.put_pixel(x, y, Rgb([100, 120, 140]));
                black.put_pixel(x, y, Rgb([100, 120, 140]));
            }
        }
        white.put_pixel(5, 5, Rgb([255; 3]));
        black.put_pixel(5, 5, Rgb([0; 3]));
        let mut options = AlphaOptions {
            crop: false,
            ..Default::default()
        };
        assert_eq!(
            recover(&white, &black, &options).unwrap().image[(5, 5)][3],
            0
        );
        options.opaque_regions.push(image::math::Rect {
            x: 3,
            y: 3,
            width: 6,
            height: 4,
        });
        let image = recover(&white, &black, &options).unwrap().image;
        assert_eq!(image[(5, 5)].0, [0, 0, 0, 255]);
        assert_eq!(image[(4, 4)].0, [100, 120, 140, 255]);
        assert_eq!(image[(0, 0)][3], 0);
        options.opaque_regions[0].width = 100;
        assert!(recover(&white, &black, &options).is_err());
    }

    #[test]
    fn overlays_compose_over_recovered_pixels_and_extend_crop() {
        let mut white = RgbImage::from_pixel(6, 4, Rgb([255; 3]));
        let mut black = RgbImage::new(6, 4);
        for (x, y) in [(0, 0), (1, 0)] {
            white.put_pixel(x, y, Rgb([40, 80, 120]));
            black.put_pixel(x, y, Rgb([40, 80, 120]));
        }
        let mut options = AlphaOptions {
            overlays: vec![Overlay {
                image: RgbaImage::from_pixel(1, 2, Rgba([255, 0, 0, 128])),
                x: 1,
                y: 0,
            }],
            ..Default::default()
        };
        let image = recover(&white, &black, &options).unwrap().image;
        assert_eq!(image.dimensions(), (2, 2));
        // Matches blending into both composites in sRGB.
        assert_eq!(image[(1, 0)].0, [148, 40, 60, 255]);
        let pixel = image[(1, 1)].0;
        assert!(
            pixel
                .iter()
                .zip([255, 0, 0, 128])
                .all(|(&p, e)| (i32::from(p) - e).abs() <= 1),
            "{pixel:?}"
        );
        options.crop = false;
        assert_eq!(
            recover(&white, &black, &options).unwrap().image[(0, 1)],
            Rgba([0; 4])
        );
    }

    fn composites(width: u32, height: u32) -> (RgbImage, RgbImage) {
        let mut white = RgbImage::from_pixel(width, height, Rgb([255; 3]));
        let mut black = RgbImage::new(width, height);
        for y in 8..height - 8 {
            for x in 8..width - 8 {
                let alpha = if x == 8 || y == 8 { 0.4 } else { 1.0 };
                let color = [0.13 + x as f32 / 200.0, 0.2 + y as f32 / 200.0, 0.3];
                white.put_pixel(
                    x,
                    y,
                    Rgb(color.map(|c| byte(linear_to_srgb(c * alpha + 1.0 - alpha)))),
                );
                black.put_pixel(x, y, Rgb(color.map(|c| byte(linear_to_srgb(c * alpha)))));
            }
        }
        (white, black)
    }

    #[test]
    fn recovers_transparency_color_and_crop() {
        let (w, b) = composites(40, 32);
        let result = recover(
            &w,
            &b,
            &AlphaOptions {
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.image.dimensions(), (24, 16));
        assert!((i32::from(result.image[(0, 0)][3]) - 102).abs() <= 2);
        assert_eq!(result.image[(1, 1)][3], 255);
        let uncropped = recover(
            &w,
            &b,
            &AlphaOptions {
                crop: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(uncropped.image[(0, 0)], Rgba([0; 4]));
    }

    #[test]
    fn rejects_unequal_sizes() {
        let w = RgbImage::from_pixel(7, 5, Rgb([255; 3]));
        let b = RgbImage::new(3, 1);
        assert!(recover(&w, &b, &AlphaOptions::default()).is_err());
        let result = recover(&w, &RgbImage::new(7, 5), &AlphaOptions::default()).unwrap();
        assert!(result.image.pixels().all(|p| p.0 == [0; 4]));
    }

    #[test]
    fn validates_library_options_and_handles_single_pixel() {
        let w = RgbImage::from_pixel(1, 1, Rgb([255; 3]));
        let b = RgbImage::new(1, 1);
        assert_eq!(
            recover(&w, &b, &AlphaOptions::default()).unwrap().image[(0, 0)],
            Rgba([0; 4])
        );
        assert!(
            recover(
                &w,
                &b,
                &AlphaOptions {
                    epsilon: f32::NAN,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
