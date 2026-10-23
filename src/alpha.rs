//! Recovers straight (unpremultiplied) RGBA from a white and a black composite of the same
//! window, draws cursor overlays and crops to the visible content.
//!
//! # Model
//!
//! Composition blends window color `C` with coverage `α` over the backdrop, per channel in
//! the source's blend space. With a white (1.0) and a black (0.0) backdrop:
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
//! - The default solve uses encoded sRGB, matching 8-bit SDR DWM backdrop composition.
//!   Explicit linear-light inputs are decoded through a 256-entry table before solving.
//! - Each channel yields its own `W − B`. The solver takes the median of the three, so one
//!   channel thrown off by quantization, clipping or colored subpixel text cannot move `α`. The
//!   minimum or maximum would follow that outlier. `α` is clamped to `[0, 1]`.
//! - Color is `B / max(α, epsilon)` in the same space as the alpha solve. Linear-light color is
//!   then encoded to sRGB. Alpha is never gamma-corrected; bytes are rounded with ties to even.
//!   Pixels with `α <= epsilon` become `[0, 0, 0, 0]`, which avoids division by zero.
//! - [`AlphaOptions::opaque_regions`] get `α = 1` and the black-pass color. Client content such
//!   as a caret or an animation can change between passes, and the difference would read as
//!   transparency and cut holes into the window.
//! - [`Overlay`]s are composited after the solve with Porter-Duff "over" on sRGB bytes, in list
//!   order. Windows draws the cursor the same way on top of the composed desktop. Over a
//!   transparent pixel the result is the overlay's own color and alpha, untinted by either
//!   backdrop.
//! - A nonzero [`AlphaOptions::corner_radius`] caps corner alpha with a one-pixel feather before
//!   overlays. Straight RGB stays unchanged wherever alpha survives. This approximates standard
//!   DWM corners; it does not recover an exact shadow-free native silhouette.
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

/// Space in which the source combined its color with the white and black backdrops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendSpace {
    /// Decode to linear light before solving, then encode the recovered color to sRGB.
    Linear,
    /// Solve on encoded sRGB bytes, matching the supported 8-bit SDR backdrop capture path.
    #[default]
    Srgb,
}

/// Parameters for [`recover`]; the default crops to every non-transparent pixel.
#[derive(Debug, Clone)]
pub struct AlphaOptions {
    /// Space in which the source composed the white and black images.
    pub blend_space: BlendSpace,
    /// Approximate rounded-frame radius in physical pixels; zero leaves corners unchanged.
    /// Caps existing alpha with a one-pixel feather before overlays and cropping.
    pub corner_radius: f32,
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
            blend_space: BlendSpace::Srgb,
            corner_radius: 0.0,
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

/// Decodes a composite into its declared blend space through a lookup table.
fn decode(image: &RgbImage, space: BlendSpace) -> Vec<[f32; 3]> {
    let lut: [f32; 256] = std::array::from_fn(|i| {
        let encoded = i as f32 / 255.0;
        match space {
            BlendSpace::Linear => srgb_to_linear(encoded),
            BlendSpace::Srgb => encoded,
        }
    });
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
/// region leaves the canvas, if `epsilon` or `crop_threshold` is out of range or not finite,
/// or if the corner radius is negative or not finite.
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
    ensure!(
        options.corner_radius.is_finite() && options.corner_radius >= 0.0,
        "Corner radius must be finite and nonnegative"
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
    let (w, b) = (
        decode(white, options.blend_space),
        decode(black, options.blend_space),
    );
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
                let value = c / alpha.max(options.epsilon);
                byte(match options.blend_space {
                    BlendSpace::Linear => linear_to_srgb(value),
                    BlendSpace::Srgb => value,
                })
            }
        });
        if options.corner_radius > 0.0 {
            alpha = alpha.min(corner_coverage(x, y, width, height, options.corner_radius));
            if byte(alpha) == 0 {
                color = [0; 3];
            }
        }
        apply_overlays(&mut color, &mut alpha, x, y, &options.overlays);
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
    Ok(RecoveredImage {
        image: crop_image(image, crop, options),
    })
}

/// Draws overlays and crops an existing straight-alpha image without recovering or forcing alpha.
/// `blend_space`, `opaque_regions` and `epsilon` apply only to [`recover`].
///
/// # Errors
///
/// Fails for an empty image, more than 40 million pixels, a non-finite/out-of-range threshold,
/// or a negative/non-finite corner radius.
pub fn finish(mut image: RgbaImage, options: &AlphaOptions) -> Result<RecoveredImage> {
    let (width, height) = image.dimensions();
    ensure!(width > 0 && height > 0, "Image must not be empty");
    ensure!(
        u64::from(width) * u64::from(height) <= 40_000_000,
        "Image canvas exceeds 40 million pixels"
    );
    ensure!(
        options.crop_threshold.is_finite() && (0.0..=1.0).contains(&options.crop_threshold),
        "Crop threshold must be in [0, 1]"
    );
    ensure!(
        options.corner_radius.is_finite() && options.corner_radius >= 0.0,
        "Corner radius must be finite and nonnegative"
    );
    let mut crop = (width, height, 0, 0);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let mut color = [pixel[0], pixel[1], pixel[2]];
        let mut alpha = f32::from(pixel[3]) / 255.0;
        if options.corner_radius > 0.0 {
            alpha = alpha.min(corner_coverage(x, y, width, height, options.corner_radius));
            if byte(alpha) == 0 {
                color = [0; 3];
            }
        }
        apply_overlays(&mut color, &mut alpha, x, y, &options.overlays);
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
    Ok(RecoveredImage {
        image: crop_image(image, crop, options),
    })
}

fn corner_coverage(x: u32, y: u32, width: u32, height: u32, radius: f32) -> f32 {
    let radius = radius.min(width.min(height) as f32 / 2.0);
    let dx = radius - (x.min(width - 1 - x) as f32 + 0.5);
    let dy = radius - (y.min(height - 1 - y) as f32 + 0.5);
    if dx <= 0.0 || dy <= 0.0 {
        return 1.0;
    }
    // Cap rather than multiply: existing antialiasing and translucent content keep their alpha.
    (radius + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0)
}

fn apply_overlays(color: &mut [u8; 3], alpha: &mut f32, x: u32, y: u32, overlays: &[Overlay]) {
    for overlay in overlays {
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
        let composed = 1.0 - (1.0 - over_alpha) * (1.0 - *alpha);
        if composed > 0.0 {
            *color = std::array::from_fn(|c| {
                ((f32::from(over[c]) * over_alpha
                    + f32::from(color[c]) * *alpha * (1.0 - over_alpha))
                    / composed)
                    .round_ties_even() as u8
            });
        }
        *alpha = composed;
    }
}

fn crop_image(
    mut image: RgbaImage,
    bounds: (u32, u32, u32, u32),
    options: &AlphaOptions,
) -> RgbaImage {
    if options.crop && bounds.0 < bounds.2 && bounds.1 < bounds.3 {
        let x = bounds.0.saturating_sub(options.margin);
        let y = bounds.1.saturating_sub(options.margin);
        let right = bounds.2.saturating_add(options.margin).min(image.width());
        let bottom = bounds.3.saturating_add(options.margin).min(image.height());
        image = image::imageops::crop_imm(&image, x, y, right - x, bottom - y).to_image();
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn corner_mask_is_symmetric_feathered_and_preserves_existing_alpha_and_color() {
        for dpi in [96.0, 120.0, 144.0, 192.0] {
            for dips in [4.0, 8.0] {
                let radius = dips * dpi / 96.0;
                let options = AlphaOptions {
                    crop: false,
                    corner_radius: radius,
                    ..Default::default()
                };
                let opaque = finish(
                    RgbaImage::from_pixel(48, 40, Rgba([200, 100, 40, 255])),
                    &options,
                )
                .unwrap()
                .image;
                assert_eq!(opaque[(0, 0)], Rgba([0; 4]));
                assert!(opaque.pixels().any(|p| p[3] > 0 && p[3] < 255));
                assert_eq!(opaque[(24, 0)], Rgba([200, 100, 40, 255]));
                for (x, y, p) in opaque.enumerate_pixels() {
                    assert_eq!(p, &opaque[(47 - x, y)]);
                    assert_eq!(p, &opaque[(x, 39 - y)]);
                    if p[3] > 0 && p[3] < 255 {
                        let distance = (radius - x.min(47 - x) as f32 - 0.5)
                            .hypot(radius - y.min(39 - y) as f32 - 0.5);
                        assert!((distance - radius).abs() < 0.5);
                    }
                }
                for alpha in [64, 128] {
                    let image = finish(
                        RgbaImage::from_pixel(48, 40, Rgba([200, 100, 40, alpha])),
                        &options,
                    )
                    .unwrap()
                    .image;
                    assert_eq!(image[(24, 0)], Rgba([200, 100, 40, alpha]));
                    for (pixel, mask) in image.pixels().zip(opaque.pixels()) {
                        assert_eq!(pixel[3], alpha.min(mask[3]));
                        assert_eq!(
                            pixel.0[..3],
                            if pixel[3] == 0 {
                                [0; 3]
                            } else {
                                [200, 100, 40]
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn corner_mask_matches_between_recovery_and_native_finishing_and_precedes_cursor() {
        let image = RgbaImage::from_pixel(32, 24, Rgba([100, 80, 40, 128]));
        let black = RgbImage::from_pixel(32, 24, Rgb([50, 40, 20]));
        let white = RgbImage::from_pixel(32, 24, Rgb([177, 167, 147]));
        let options = AlphaOptions {
            blend_space: BlendSpace::Srgb,
            crop: false,
            corner_radius: 8.0,
            overlays: vec![Overlay {
                image: RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 255])),
                x: 0,
                y: 0,
            }],
            ..Default::default()
        };
        let native = finish(image, &options).unwrap().image;
        let solved = recover(&white, &black, &options).unwrap().image;
        assert_eq!(native, solved);
        assert_eq!(native[(0, 0)], Rgba([10, 20, 30, 255]));
        assert_eq!(native[(31, 23)], Rgba([0; 4]));
    }

    #[test]
    fn corner_mask_validates_radius_and_handles_small_or_square_canvases() {
        let image = RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 64]));
        assert_eq!(
            finish(image.clone(), &AlphaOptions::default())
                .unwrap()
                .image,
            image
        );
        assert_eq!(
            finish(
                image.clone(),
                &AlphaOptions {
                    corner_radius: 100.0,
                    ..Default::default()
                }
            )
            .unwrap()
            .image,
            image
        );
        for radius in [f32::NAN, f32::INFINITY, -1.0] {
            let options = AlphaOptions {
                corner_radius: radius,
                ..Default::default()
            };
            assert!(finish(image.clone(), &options).is_err());
            assert!(recover(&RgbImage::new(1, 1), &RgbImage::new(1, 1), &options).is_err());
        }
    }
    #[test]
    fn default_recovery_matches_measured_sdr_pixels() {
        // Native shadow samples at 96/192 DPI and authored UpdateLayeredWindow colors.
        let samples = [
            ([199; 3], [0; 3], [0, 0, 0, 56]),
            ([198; 3], [0; 3], [0, 0, 0, 57]),
            ([241, 216, 201], [50, 25, 10], [199, 100, 40, 64]),
            ([227, 177, 147], [100, 50, 20], [199, 100, 40, 128]),
            ([200, 100, 40], [200, 100, 40], [200, 100, 40, 255]),
            ([255; 3], [0; 3], [0; 4]),
        ];
        for (white, black, expected) in samples {
            let image = recover(
                &RgbImage::from_pixel(1, 1, Rgb(white)),
                &RgbImage::from_pixel(1, 1, Rgb(black)),
                &AlphaOptions::default(),
            )
            .unwrap()
            .image;
            assert_eq!(
                image[(0, 0)].0,
                expected,
                "white={white:?}, black={black:?}"
            );
        }
    }

    #[test]
    fn encoded_layered_composites_recover_native_alpha_and_quantized_color() {
        let alphas = [0u8, 7, 31, 64, 96, 128, 191, 253, 255];
        let color = [200u8, 100, 40];
        let black = RgbImage::from_fn(alphas.len() as u32, 1, |x, _| {
            Rgb(color
                .map(|channel| (u16::from(channel) * u16::from(alphas[x as usize]) / 255) as u8))
        });
        let white = RgbImage::from_fn(alphas.len() as u32, 1, |x, _| {
            Rgb(black[(x, 0)]
                .0
                .map(|channel| channel + (255 - alphas[x as usize])))
        });
        // These are also the UpdateLayeredWindow alpha-64 samples.
        assert_eq!(white[(3, 0)].0, [241, 216, 201]);
        assert_eq!(black[(3, 0)].0, [50, 25, 10]);
        let result = recover(
            &white,
            &black,
            &AlphaOptions {
                blend_space: BlendSpace::Srgb,
                crop: false,
                ..Default::default()
            },
        )
        .unwrap()
        .image;
        for (pixel, alpha) in result.pixels().zip(alphas) {
            assert_eq!(pixel[3], alpha);
            if alpha == 0 {
                assert_eq!(pixel.0, [0; 4]);
            } else {
                // Source premultiplication loses less than one byte, magnified by 255/alpha.
                let tolerance = 255u16.div_ceil(u16::from(alpha));
                for (actual, expected) in pixel.0[..3].iter().zip(color) {
                    assert!(
                        u16::from(actual.abs_diff(expected)) <= tolerance,
                        "{pixel:?}, alpha={alpha}"
                    );
                }
            }
        }
        assert_eq!(AlphaOptions::default().blend_space, BlendSpace::Srgb);
    }

    #[test]
    fn native_images_keep_alpha_and_share_overlay_and_crop_behavior() {
        let mut image = RgbaImage::new(3, 1);
        image.put_pixel(1, 0, Rgba([255, 0, 0, 128]));
        let mut options = AlphaOptions {
            opaque_regions: vec![image::math::Rect {
                x: 0,
                y: 0,
                width: 3,
                height: 1,
            }],
            ..Default::default()
        };
        let preserved = finish(image.clone(), &options).unwrap().image;
        assert_eq!(preserved.dimensions(), (1, 1));
        assert_eq!(preserved[(0, 0)].0, [255, 0, 0, 128]);
        options.overlays.push(Overlay {
            image: RgbaImage::from_pixel(1, 1, Rgba([0, 255, 0, 128])),
            x: 1,
            y: 0,
        });
        assert_eq!(
            finish(image.clone(), &options).unwrap().image[(0, 0)].0,
            [85, 170, 0, 192]
        );
        options.margin = 1;
        assert_eq!(
            finish(image.clone(), &options).unwrap().image.dimensions(),
            (3, 1)
        );
        options.crop_threshold = f32::NAN;
        assert!(finish(image, &options).is_err());
        assert!(finish(RgbaImage::new(0, 1), &AlphaOptions::default()).is_err());
    }

    #[test]
    fn recovery_crop_decides_before_alpha_is_quantized() {
        let mut white = RgbImage::from_pixel(3, 1, Rgb([255; 3]));
        white.put_pixel(1, 0, Rgb([180; 3]));
        let black = RgbImage::new(3, 1);
        let alpha = 1.0 - srgb_to_linear(180.0 / 255.0);
        let quantized = f32::from(byte(alpha)) / 255.0;
        let threshold = (alpha + quantized) / 2.0;
        assert_ne!(alpha > threshold, quantized > threshold);
        let image = recover(
            &white,
            &black,
            &AlphaOptions {
                blend_space: BlendSpace::Linear,
                crop_threshold: threshold,
                ..Default::default()
            },
        )
        .unwrap()
        .image;
        assert_eq!(
            image.dimensions(),
            if alpha > threshold { (1, 1) } else { (3, 1) }
        );
    }

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
    fn explicitly_linear_composites_recover_transparency_color_and_crop() {
        let (w, b) = composites(40, 32);
        let result = recover(
            &w,
            &b,
            &AlphaOptions {
                blend_space: BlendSpace::Linear,
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
                blend_space: BlendSpace::Linear,
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
