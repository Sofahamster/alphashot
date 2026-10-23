//! Terminal output after a capture: progress line, saved-files summary, and a
//! Sixel thumbnail.
//!
//! - Redirected stdout receives only the saved paths, one per line.
//! - An interactive terminal gets a heading, the window title, image details,
//!   and the paths as OSC 8 hyperlinks.
//! - `NO_COLOR` (non-empty) and `TERM=dumb` keep the layout but drop colors,
//!   hyperlinks, and the thumbnail.
//!
//! Hyperlink URLs keep non-ASCII characters literal. Windows Terminal passes
//! the URL to ShellExecute, which misdecodes percent-encoded UTF-8 and fails on
//! paths such as `Büro.png`.
//!
//! The thumbnail composites the image over white so transparent areas show on
//! dark terminals. The saved file is unchanged.

use std::{
    env,
    fmt::Write as _,
    io::{self, IsTerminal, Write},
    path::Path,
};

use crate::sixel::encode as encode_sixel;
use alphashot::{capture::WindowInfo, output::OutputFormat};
use anyhow::Result;
use crossterm::style::{Color, Stylize};
#[cfg(test)]
use image::Rgba;
use image::RgbaImage;

const ACCENT: Color = Color::Rgb {
    r: 103,
    g: 211,
    b: 190,
};
const MUTED: Color = Color::Rgb {
    r: 139,
    g: 156,
    b: 175,
};

/// True when stdin and stdout are both terminals.
fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// True when colors, hyperlinks, and Sixel are allowed: interactive, `NO_COLOR`
/// unset or empty (no-color.org), and `TERM` not `dumb`.
pub(crate) fn styled() -> bool {
    interactive()
        && !env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
        && !env::var("TERM").is_ok_and(|value| value == "dumb")
}

/// True for characters that must not reach the terminal unescaped: controls, which start escape
/// sequences or break lines, and bidi overrides, isolates and line separators, which rearrange
/// the rest of the line.
///
/// `char::escape_debug` alone also escapes emoji joiners, variation selectors and combining
/// marks, which turns `❤️` into `❤\u{fe0f}` and `é` written as `e` + U+0301 into `e\u{301}`.
pub(crate) fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}')
}

/// Escapes control characters in untrusted text (window titles, paths) so they
/// cannot inject terminal escape sequences. Everything else stays literal.
fn clean(value: &str) -> String {
    let mut text = String::new();
    for c in value.chars() {
        if needs_escape(c) {
            text.extend(c.escape_debug());
        } else {
            text.push(c);
        }
    }
    text
}

/// Builds a `file:` URL for an OSC 8 hyperlink.
///
/// Handles drive, UNC, and `\\?\` verbatim paths. ASCII reserved characters are
/// percent-encoded; Unicode stays literal (see the module docs).
fn file_url(path: &Path) -> io::Result<String> {
    let absolute = std::path::absolute(path)?;
    let path = absolute.to_string_lossy();
    #[cfg(windows)]
    let path = path.replace('\\', "/");
    let path = if let Some(unc) = path.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else {
        path.strip_prefix("//?/").unwrap_or(&path).to_owned()
    };
    let mut url = if path.starts_with("//") {
        "file:".to_owned()
    } else if path.starts_with('/') {
        "file://".to_owned()
    } else {
        "file:///".to_owned()
    };
    for c in path.chars() {
        // Unicode stays literal: Windows Terminal/ShellExecute misdecodes percent-encoded UTF-8.
        if c.is_ascii_alphanumeric() || "/:-._~".contains(c) || (!c.is_ascii() && !c.is_control()) {
            url.push(c);
        } else {
            for byte in c.encode_utf8(&mut [0; 4]).bytes() {
                write!(url, "%{byte:02X}").unwrap();
            }
        }
    }
    Ok(url)
}

/// Prints the "Capturing" progress line to stderr, away from the paths on stdout.
pub fn capturing(title: &str) -> Result<()> {
    let title = clean(title);
    let mut stderr = io::stderr().lock();
    if styled() && io::stderr().is_terminal() {
        writeln!(stderr, "\n  {}  {title}", "◌ Capturing".with(MUTED))?;
    } else {
        writeln!(stderr, "Capturing {title}")?;
    }
    Ok(())
}

/// Writes the saved-files summary to `stream`.
///
/// Non-interactive output is the bare paths. Interactive output adds a heading,
/// title, and details; `styled` adds colors and hyperlinks. The flags are
/// parameters so tests need no terminal.
fn write_result(
    stream: &mut impl Write,
    paths: &[&Path],
    title: &str,
    details: &str,
    interactive: bool,
    styled: bool,
) -> Result<()> {
    if !interactive {
        for path in paths {
            writeln!(stream, "{}", path.display())?;
        }
        return Ok(());
    }
    let heading = if paths.len() == 1 {
        "✓ Capture saved"
    } else {
        "✓ Raw captures saved"
    };
    writeln!(stream)?;
    if styled {
        writeln!(stream, "  {}", heading.with(ACCENT).bold())?;
        writeln!(stream, "    {}", clean(title).bold())?;
        writeln!(stream, "    {}", details.with(MUTED))?;
    } else {
        writeln!(stream, "  {heading}\n    {}\n    {details}", clean(title))?;
    }
    writeln!(stream)?;
    for path in paths {
        let label = clean(&path.display().to_string());
        if styled && let Ok(url) = file_url(path) {
            writeln!(
                stream,
                "    \x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\",
                label.with(ACCENT).underlined()
            )?;
        } else {
            writeln!(stream, "    {label}")?;
        }
    }
    if styled {
        writeln!(stream, "    {}", "Ctrl+click to open".with(MUTED))?;
    }
    writeln!(stream)?;
    Ok(())
}

/// Reports the raw white and black composites saved by `--raw-dir`.
pub fn saved_raw(white: &Path, black: &Path, title: &str) -> Result<()> {
    write_result(
        &mut io::stdout().lock(),
        &[white, black],
        title,
        "PNG · white and black backgrounds",
        interactive(),
        styled(),
    )
}

/// Reports the saved result and, in a styled terminal, prints a Sixel thumbnail.
///
/// `disabled` is `--no-preview`. A thumbnail failure only warns on stderr; the
/// file is already saved.
pub fn saved(
    image: RgbaImage,
    path: &Path,
    window: &WindowInfo,
    format: OutputFormat,
    disabled: bool,
) -> Result<()> {
    let format = match format.resolve(path)? {
        OutputFormat::Png => "PNG",
        OutputFormat::Webp => "WebP · lossless",
        OutputFormat::Tiff => "TIFF · LZW",
        OutputFormat::Auto => unreachable!(),
    };
    let details = format!(
        "{} × {} px · {format} · {} DPI",
        image.width(),
        image.height(),
        window.dpi
    );
    let mut stdout = io::stdout().lock();
    write_result(
        &mut stdout,
        &[path],
        &window.title,
        &details,
        interactive(),
        styled(),
    )?;
    if !disabled && styled() {
        let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let image = thumbnail(&image, columns, rows);
        match encode_sixel(&image) {
            Ok(encoded) => writeln!(stdout, "    {encoded}\n")?,
            Err(error) => eprintln!("Warning: image saved, but thumbnail preview failed: {error}"),
        }
    }
    stdout.flush()?;
    Ok(())
}

/// Fits `image` into a panel of at most 40 columns by 12 rows, leaving 5 columns
/// and 3 rows for the indent and surrounding text.
fn thumbnail(image: &RgbaImage, columns: u16, rows: u16) -> RgbaImage {
    let width = u32::from(columns.saturating_sub(5).clamp(1, 40)) * 6;
    let height = u32::from(rows.saturating_sub(3).clamp(1, 12)) * 12;
    crate::sixel::fit(image, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Prints a synthetic capture result in an interactive terminal and saves fixtures under target/result-previews"]
    fn interactive_capture_result() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/result-previews");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Büro #1.png");
        let image = RgbaImage::from_fn(800, 500, |x, y| {
            if !(12..788).contains(&x) || !(12..488).contains(&y) {
                Rgba([0, 0, 0, 0])
            } else if y < 56 {
                Rgba([32, 58, 66, 255])
            } else if (40..260).contains(&x) && (90..460).contains(&y) {
                Rgba([225, 230, 235, 255])
            } else {
                Rgba([248, 248, 248, 255])
            }
        });
        alphashot::output::save(
            &image,
            &path,
            &alphashot::output::SaveOptions {
                overwrite: true,
                dpi: Some(144),
                ..Default::default()
            },
        )
        .unwrap();
        let window = WindowInfo {
            handle: 1,
            pid: 1,
            process_name: Some("example.exe".into()),
            title: "Example window".into(),
            class_name: String::new(),
            rect: Default::default(),
            dpi: 144,
        };
        let mut frame = b"\x1b[2J\x1b[H".to_vec();
        write_result(
            &mut frame,
            &[&path],
            &window.title,
            "800 × 500 px · PNG · 144 DPI",
            true,
            true,
        )
        .unwrap();
        std::fs::write(
            directory.join("result.ansi"),
            String::from_utf8(frame).unwrap().replace('\n', "\r\n"),
        )
        .unwrap();
        capturing(&window.title).unwrap();
        saved(image, &path, &window, OutputFormat::Png, false).unwrap();
    }

    #[test]
    fn result_paths_are_clickable_only_in_decorated_terminal_output() {
        let paths = [
            Path::new("capture white.png"),
            Path::new("capture black.png"),
        ];
        let mut piped = Vec::new();
        write_result(&mut piped, &paths, "Notes", "PNG", false, false).unwrap();
        assert_eq!(
            String::from_utf8(piped).unwrap(),
            "capture white.png\ncapture black.png\n"
        );

        let title = "Notes\x1b]8;;https://example.invalid\x07\ncaption";
        let mut decorated = Vec::new();
        write_result(
            &mut decorated,
            &paths[..1],
            title,
            "800 × 600 px · PNG · 96 DPI",
            true,
            true,
        )
        .unwrap();
        let decorated = String::from_utf8(decorated).unwrap();
        assert!(decorated.contains("✓ Capture saved"));
        assert!(decorated.contains("800 × 600 px · PNG · 96 DPI"));
        assert!(decorated.contains("Ctrl+click to open"));
        assert!(decorated.contains(&format!("\x1b]8;;{}\x1b\\", file_url(paths[0]).unwrap())));
        assert!(decorated.contains("\x1b]8;;\x1b\\"));
        assert!(!decorated.contains("\x1b]8;;https://"));
        assert!(decorated.contains("\\u{1b}]8;;https://example.invalid\\u{7}\\ncaption"));

        let mut plain = Vec::new();
        write_result(&mut plain, &paths, "Notes", "PNG", true, false).unwrap();
        let plain = String::from_utf8(plain).unwrap();
        assert!(plain.contains("✓ Raw captures saved"));
        assert!(plain.contains("capture white.png") && plain.contains("capture black.png"));
        assert!(!plain.contains('\x1b'));
        assert!(!plain.contains("Ctrl+click"));
    }

    #[test]
    fn titles_keep_emoji_and_combining_marks_but_escape_controls() {
        assert_eq!(
            clean("❤️ Café\u{301} हिन्दी 👨‍👩‍👧 C:\\x \"q\""),
            "❤️ Café\u{301} हिन्दी 👨‍👩‍👧 C:\\x \"q\""
        );
        assert_eq!(
            clean("a\x1b[2J\u{9b}b\u{202e}c\nd"),
            "a\\u{1b}[2J\\u{9b}b\\u{202e}c\\nd"
        );
    }

    #[test]
    fn file_links_are_absolute_and_escape_reserved_characters() {
        let url = file_url(Path::new("Büro #100% [done].png")).unwrap();
        assert!(url.starts_with("file:///"));
        assert!(url.ends_with("/Büro%20%23100%25%20%5Bdone%5D.png"));
        assert!(!url.contains('\\'));
        #[cfg(windows)]
        for (path, expected) in [
            (
                r"C:\Screenshots\window.png",
                "file:///C:/Screenshots/window.png",
            ),
            (
                r"\\?\C:\Screenshots\window.png",
                "file:///C:/Screenshots/window.png",
            ),
            (
                r"\\server\shots\window.png",
                "file://server/shots/window.png",
            ),
            (
                r"\\?\UNC\server\shots\window.png",
                "file://server/shots/window.png",
            ),
        ] {
            assert_eq!(file_url(Path::new(path)).unwrap(), expected);
        }
    }

    #[test]
    fn default_preview_is_complete_sixel_without_terminal_queries() {
        let source = RgbaImage::from_fn(12, 12, |x, _| {
            if x < 6 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 128])
            }
        });
        let preview = thumbnail(&source, 100, 40);
        let encoded = encode_sixel(&preview).unwrap();
        assert!(encoded.starts_with("\x1bP"));
        assert!(encoded.ends_with("\x1b\\"));
        assert!(encoded.contains("\"1;1;12;12"));
        assert!(!encoded.contains("\x1b[c"));
    }

    #[test]
    fn fits_wide_and_tall_images_without_stretching() {
        for (width, height, expected) in [(800, 400, (240, 120)), (400, 800, (72, 144))] {
            let source = RgbaImage::from_pixel(width, height, Rgba([255; 4]));
            let preview = thumbnail(&source, 100, 40);
            assert_eq!((preview.width(), preview.height()), expected);
        }
    }

    #[test]
    fn shows_transparency_without_changing_the_saved_pixels() {
        let source = RgbaImage::from_pixel(12, 12, Rgba([255, 0, 0, 0]));
        let preview = thumbnail(&source, 100, 40);
        assert!(preview.pixels().all(|pixel| pixel.0 == [255; 4]));
        assert!(source.pixels().all(|pixel| pixel.0 == [255, 0, 0, 0]));
    }

    #[test]
    fn respects_small_terminals_and_keeps_narrow_images_visible() {
        let source = RgbaImage::from_pixel(1, 1000, Rgba([255; 4]));
        for (columns, rows) in [(0, 0), (2, 4), (20, 10)] {
            let preview = thumbnail(&source, columns, rows);
            assert!(preview.width() > 0 && preview.height() > 0);
            assert!(preview.width() <= u32::from(columns.max(2) - 1).min(40) * 6);
            assert!(preview.height() <= u32::from(rows.saturating_sub(3).max(1)).min(12) * 12);
        }
    }
}
