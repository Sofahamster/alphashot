//! Output of straight-alpha images and default file names.
//!
//! Images use 8-bit sRGB and straight alpha internally. PNG and WebP keep straight alpha;
//! TIFF can explicitly associate color with alpha, which quantizes translucent colors:
//!
//! - PNG: `sRGB` chunk (perceptual); DPI becomes `pHYs` at `round(dpi / 0.0254)` pixels per metre.
//! - WebP: lossless VP8L; the encoder stores no DPI.
//! - TIFF: LZW-compressed RGB plus `UnassociatedAlpha` by default, or `AssociatedAlpha` with
//!   [`TiffAlpha::Premultiplied`]; DPI as inch resolution. Only encoded TIFF samples change.
//!
//! [`OutputFormat::Auto`] maps `.png` or no extension to PNG, `.webp` to WebP and `.tif` or
//! `.tiff` to TIFF, ignoring ASCII case, and rejects other extensions. [`save_composite`] writes
//! a raw pass as RGB PNG.
//!
//! # Atomic writes
//!
//! Each file is encoded into a temporary file in the destination directory, synced and renamed
//! into place, so a crash or encoder error leaves no truncated image. The same directory keeps
//! the rename on one volume. Missing directories are created. Without overwrite,
//! [`check_destination`] fails before capture touches the desktop, and a no-clobber rename keeps
//! files created during the capture.
//!
//! # Default names
//!
//! [`default_path`] builds `YYYY-MM-DD-HH-MM-SS-<slug>.<ext>`, in local time on Windows and UTC
//! elsewhere. The slug lowercases ASCII letters and digits, collapses every other run into one
//! `-`, trims dashes, stops at 80 bytes and falls back to `window`.
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use image::{RgbImage, RgbaImage};

/// Container format of the recovered image.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum OutputFormat {
    /// Follows the file extension.
    #[default]
    Auto,
    /// PNG.
    Png,
    /// Lossless WebP without DPI.
    Webp,
    /// LZW-compressed TIFF.
    Tiff,
}

impl OutputFormat {
    /// Resolves `Auto` from the extension of `path`; other variants pass through.
    ///
    /// # Errors
    ///
    /// Fails for `Auto` with an extension other than `png`, `webp`, `tif` or `tiff`.
    pub fn resolve(self, path: &Path) -> Result<Self> {
        match self {
            Self::Auto => match path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str()
            {
                "png" | "" => Ok(Self::Png),
                "webp" => Ok(Self::Webp),
                "tif" | "tiff" => Ok(Self::Tiff),
                extension => bail!(
                    "Unsupported output extension .{extension}; use .png, .webp, .tiff or --out-format"
                ),
            },
            other => Ok(other),
        }
    }
}

/// Alpha association stored in a TIFF file; the input image always remains straight RGBA.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum TiffAlpha {
    /// Stores color independently of alpha (`UnassociatedAlpha`), without quantization.
    #[default]
    Straight,
    /// Multiplies each color by alpha (`AssociatedAlpha`), with 8-bit rounding loss.
    Premultiplied,
}

impl TiffAlpha {
    /// Checks the association against a resolved output format.
    ///
    /// # Errors
    ///
    /// Premultiplied alpha requires TIFF; PNG and WebP store straight alpha.
    pub fn validate(self, format: OutputFormat) -> Result<()> {
        ensure!(
            self == Self::Straight || matches!(format, OutputFormat::Tiff),
            "Premultiplied alpha requires TIFF; use --out-format tiff or --tiff-alpha straight"
        );
        Ok(())
    }
}

/// Parameters for [`save`].
#[derive(Clone, Debug)]
pub struct SaveOptions {
    /// Output format.
    pub format: OutputFormat,
    /// TIFF alpha association; premultiplied samples are supported only by TIFF.
    pub tiff_alpha: TiffAlpha,
    /// Resolution for PNG and TIFF metadata, usually the capture DPI.
    pub dpi: Option<u32>,
    /// Replaces an existing file.
    pub overwrite: bool,
}

impl Default for SaveOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Auto,
            tiff_alpha: TiffAlpha::Straight,
            dpi: None,
            overwrite: false,
        }
    }
}

/// Checks that `path` can receive the output before a capture touches the desktop.
///
/// # Errors
///
/// Fails if the file exists without `overwrite`, if `path` has no file name or is a directory,
/// or if its existence cannot be determined.
pub fn check_destination(path: &Path, overwrite: bool) -> Result<()> {
    ensure!(
        overwrite || !path.try_exists()?,
        "{} already exists; use --force to replace it",
        path.display()
    );
    ensure!(path.file_name().is_some(), "Output must be a file path");
    ensure!(!path.is_dir(), "Output is a directory: {}", path.display());
    Ok(())
}

/// Encodes into a temporary file next to `path` and renames it into place.
fn write_atomic(
    path: &Path,
    overwrite: bool,
    encode: impl FnOnce(&mut fs::File) -> Result<()>,
) -> Result<()> {
    check_destination(path, overwrite)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    encode(temporary.as_file_mut()).with_context(|| format!("Cannot encode {}", path.display()))?;
    temporary.as_file_mut().sync_all()?;
    if overwrite {
        temporary.persist(path).map_err(|e| e.error)?;
    } else {
        temporary.persist_noclobber(path).map_err(|e| e.error)?;
    }
    Ok(())
}

/// Writes 8-bit RGB or RGBA PNG through the `png` crate, which exposes `sRGB` and `pHYs`.
fn png(
    file: &mut fs::File,
    pixels: &[u8],
    width: u32,
    height: u32,
    rgba: bool,
    dpi: Option<u32>,
) -> Result<()> {
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(if rgba {
        png::ColorType::Rgba
    } else {
        png::ColorType::Rgb
    });
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    if let Some(dpi) = dpi {
        let ppm = (f64::from(dpi) / 0.0254).round() as u32;
        encoder.set_pixel_dims(Some(png::PixelDimensions {
            xppu: ppm,
            yppu: ppm,
            unit: png::Unit::Meter,
        }));
    }
    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)?;
    writer.finish()?;
    Ok(())
}

/// Writes a raw composite as RGB PNG with DPI.
///
/// # Errors
///
/// Fails as [`check_destination`] does and on I/O or encoder errors.
pub fn save_composite(image: &RgbImage, path: &Path, dpi: u32, overwrite: bool) -> Result<()> {
    write_atomic(path, overwrite, |file| {
        png(
            file,
            image.as_raw(),
            image.width(),
            image.height(),
            false,
            Some(dpi),
        )
    })
}

/// Writes a straight-alpha RGBA image.
///
/// # Errors
///
/// Fails for an unsupported extension or alpha association, as [`check_destination`] does,
/// and on I/O or encoder errors. A failure leaves an existing file unchanged.
pub fn save(image: &RgbaImage, path: &Path, options: &SaveOptions) -> Result<()> {
    let format = options.format.resolve(path)?;
    options.tiff_alpha.validate(format)?;
    write_atomic(path, options.overwrite, |file| {
        match format {
            OutputFormat::Png => png(
                file,
                image.as_raw(),
                image.width(),
                image.height(),
                true,
                options.dpi,
            )?,
            OutputFormat::Webp => {
                image::codecs::webp::WebPEncoder::new_lossless(file).encode(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::Rgba8,
                )?;
            }
            OutputFormat::Tiff => {
                use tiff::{
                    encoder::{Compression, Rational, TiffEncoder, colortype},
                    tags::{ExtraSamples, ResolutionUnit},
                };
                let mut encoder = TiffEncoder::new(file)?.with_compression(Compression::Lzw);
                let mut output =
                    encoder.new_image::<colortype::RGB8>(image.width(), image.height())?;
                let mut pixels = std::borrow::Cow::Borrowed(image.as_raw().as_slice());
                let association = if options.tiff_alpha == TiffAlpha::Premultiplied {
                    for pixel in pixels.to_mut().as_chunks_mut::<4>().0 {
                        let alpha = u16::from(pixel[3]);
                        for channel in &mut pixel[..3] {
                            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
                        }
                    }
                    ExtraSamples::AssociatedAlpha
                } else {
                    ExtraSamples::UnassociatedAlpha
                };
                output.extra_samples(&[association])?;
                if let Some(dpi) = options.dpi {
                    output.resolution(ResolutionUnit::Inch, Rational { n: dpi, d: 1 });
                }
                output.write_data(&pixels)?;
            }
            OutputFormat::Auto => unreachable!(),
        }
        Ok(())
    })
}

/// Current time as `YYYY-MM-DD-HH-MM-SS`; off Windows, a UTC conversion without a date crate.
fn timestamp() -> String {
    #[cfg(windows)]
    let (year, month, day, hour, minute, second) = {
        let mut local = windows_sys::Win32::Foundation::SYSTEMTIME::default();
        // SAFETY: GetLocalTime writes one SYSTEMTIME to the live, aligned `local`.
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut local) };
        (
            local.wYear,
            local.wMonth,
            local.wDay,
            local.wHour,
            local.wMinute,
            local.wSecond,
        )
    };
    // Unix time to a UTC civil date.
    #[cfg(not(windows))]
    let (year, month, day, hour, minute, second) = {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut days = seconds / 86400;
        let mut year = 1970_u16;
        let leap = |year: u16| {
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
        };
        while days >= if leap(year) { 366 } else { 365 } {
            days -= if leap(year) { 366 } else { 365 };
            year += 1;
        }
        let months = [
            31,
            if leap(year) { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut month = 1;
        for length in months {
            if days < length {
                break;
            }
            days -= length;
            month += 1;
        }
        (
            year,
            month,
            days as u16 + 1,
            (seconds / 3600 % 24) as u16,
            (seconds / 60 % 60) as u16,
            (seconds % 60) as u16,
        )
    };
    format!("{year:04}-{month:02}-{day:02}-{hour:02}-{minute:02}-{second:02}")
}

/// Relative default path `YYYY-MM-DD-HH-MM-SS-<slug>.<ext>`; `Auto` gives `.png`.
pub fn default_path(title: &str, format: OutputFormat) -> PathBuf {
    let mut slug = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 80 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "window" } else { slug };
    let timestamp = timestamp();
    let extension = match format {
        OutputFormat::Webp => "webp",
        OutputFormat::Tiff => "tiff",
        _ => "png",
    };
    PathBuf::from(format!("{timestamp}-{slug}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn automatic_names_start_with_a_readable_datetime_and_safe_title() {
        for (format, extension) in [
            (OutputFormat::Png, "png"),
            (OutputFormat::Webp, "webp"),
            (OutputFormat::Tiff, "tiff"),
        ] {
            let path = default_path("A / Window: Title?", format);
            let name = path.to_str().unwrap();
            let date = &name[..19];
            assert_eq!(
                date.split('-').map(str::len).collect::<Vec<_>>(),
                [4, 2, 2, 2, 2, 2]
            );
            assert!(date.chars().all(|c| c.is_ascii_digit() || c == '-'));
            assert!(name.ends_with(&format!("-a-window-title.{extension}")));
        }
        assert!(
            default_path("???", OutputFormat::Auto)
                .to_str()
                .unwrap()
                .ends_with("-window.png")
        );
    }

    #[test]
    fn formats_round_trip_and_never_overwrite_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let image =
            RgbaImage::from_fn(3, 2, |x, _| Rgba([25, 100, 190, [0, 128, 255][x as usize]]));
        for extension in ["png", "webp", "tiff"] {
            let path = dir.path().join(format!("image.{extension}"));
            let options = SaveOptions {
                dpi: Some(144),
                ..Default::default()
            };
            save(&image, &path, &options).unwrap();
            assert_eq!(image::open(&path).unwrap().to_rgba8(), image);
            let before = fs::read(&path).unwrap();
            assert!(save(&image, &path, &options).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            save(
                &image,
                &path,
                &SaveOptions {
                    overwrite: true,
                    ..options
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn tiff_alpha_tags_and_samples_match_without_mutating_straight_rgba() {
        use tiff::{
            decoder::{Decoder, DecodingResult},
            tags::Tag,
        };
        let directory = tempfile::tempdir().unwrap();
        let image = RgbaImage::from_fn(5, 1, |x, _| {
            Rgba([255, 128, 64, [0, 1, 64, 128, 255][x as usize]])
        });
        let original = image.clone();
        assert_eq!(SaveOptions::default().tiff_alpha, TiffAlpha::Straight);
        for (mode, tag, expected) in [
            (TiffAlpha::Straight, 2, image.as_raw().clone()),
            (
                TiffAlpha::Premultiplied,
                1,
                vec![
                    0, 0, 0, 0, 1, 1, 0, 1, 64, 32, 16, 64, 128, 64, 32, 128, 255, 128, 64, 255,
                ],
            ),
        ] {
            let path = directory.path().join(format!("{mode:?}.tif"));
            save(
                &image,
                &path,
                &SaveOptions {
                    tiff_alpha: mode,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut decoder = Decoder::new(fs::File::open(path).unwrap()).unwrap();
            assert_eq!(
                decoder.get_tag_u16_vec(Tag::ExtraSamples).unwrap(),
                vec![tag]
            );
            let DecodingResult::U8(pixels) = decoder.read_image().unwrap() else {
                panic!("Expected 8-bit samples")
            };
            assert_eq!(pixels, expected);
            assert_eq!(image, original);
        }
    }

    #[test]
    fn premultiplied_alpha_rejects_non_tiff_without_creating_files_or_directories() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("not-created");
        let image = RgbaImage::from_pixel(1, 1, Rgba([255, 128, 64, 64]));
        for extension in ["png", "webp"] {
            let error = save(
                &image,
                &parent.join(format!("image.{extension}")),
                &SaveOptions {
                    tiff_alpha: TiffAlpha::Premultiplied,
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("requires TIFF"));
            assert!(!parent.exists());
        }
    }

    #[test]
    fn png_retains_dpi() {
        let dir = tempfile::tempdir().unwrap();
        let image = RgbaImage::from_pixel(3, 2, Rgba([25, 100, 190, 128]));
        let path = dir.path().join("image.png");
        save(
            &image,
            &path,
            &SaveOptions {
                dpi: Some(192),
                ..Default::default()
            },
        )
        .unwrap();
        let decoder = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()));
        assert_eq!(
            decoder.read_info().unwrap().info().pixel_dims.unwrap().xppu,
            7559
        );
    }
}
