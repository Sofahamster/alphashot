//! Command-line entry point for `alphashot`.
//!
//! `run` drives one invocation:
//!
//! 1. Enable per-monitor DPI awareness so coordinates and sizes are physical pixels.
//! 2. For `--list-displays` and `--list-windows`, print tab-separated rows to
//!    stdout and exit. Text fields are escaped so a tab or newline in a title
//!    cannot split a row.
//! 3. Select the target by handle (`--window-hwnd`), title filter, or pick mode.
//! 4. Validate the output and raw-composite destinations before capturing. The
//!    output must not name the same file as a raw composite.
//! 5. Capture the white and black backdrop composites, solve for alpha, save,
//!    and print the report.
//!
//! Exit codes: 0 success, 1 runtime error, 2 argument error (clap default). A
//! closed stdout pipe exits 1 without a message.
//!
//! When stdout is redirected, it receives only the saved paths, one per line.
//! Progress, prompts, and warnings go to stderr.

use std::{
    io::{self, IsTerminal, Write},
    path::{Component, Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use alphashot::{
    alpha::{self, AlphaOptions},
    capture::{self, CaptureOptions, CursorShadow, FocusState, MonitorInfo, PadMode, WindowInfo},
    output::{self, OutputFormat, SaveOptions},
};
#[cfg(windows)]
use anyhow::Context;
use anyhow::{Result, ensure};
use clap::{Args, Parser, ValueEnum};

mod picker;
mod report;
mod sixel;

// Plain comments: clap folds `///` on derived items into `--help`.

// Output destination, format, and reporting.
#[derive(Args, Clone)]
struct OutputArgs {
    /// Output path; defaults to a timestamp and window title.
    #[arg(short, long = "out", alias = "output")]
    out: Option<PathBuf>,
    /// Output format, otherwise inferred from the file extension.
    #[arg(long, alias = "format", value_enum, default_value = "auto")]
    out_format: OutputFormat,
    /// Replace existing output files. Writes are atomic.
    #[arg(short, long)]
    force: bool,
    /// Disable window-picker and saved-image thumbnail previews.
    #[arg(long)]
    no_preview: bool,
}

// Alpha solve and crop settings.
#[derive(Args, Clone)]
struct AlphaArgs {
    /// Preserve the full canvas instead of cropping to nonzero alpha.
    #[arg(long)]
    no_crop: bool,
    /// Crop to alpha greater than this value (0..1).
    #[arg(long, default_value_t = 0.0, value_parser = unit_float)]
    crop_threshold: f32,
    /// Extra pixels around the alpha crop.
    #[arg(long, default_value_t = 0)]
    margin: u32,
    /// Minimum alpha used in division (greater than 0, at most 1).
    #[arg(long, hide = true, default_value_t = 1e-6, value_parser = positive_unit_float)]
    alpha_epsilon: f32,
}
impl AlphaArgs {
    /// Converts CLI flags to library options; `--no-crop` inverts into `crop`.
    fn options(&self) -> AlphaOptions {
        AlphaOptions {
            crop: !self.no_crop,
            crop_threshold: self.crop_threshold,
            margin: self.margin,
            epsilon: self.alpha_epsilon,
            ..Default::default()
        }
    }
}

// Window selection when no handle is given: searchable list, cursor sampling
// after a delay, or click overlay.
#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum PickMode {
    #[default]
    List,
    Cursor,
    Overlay,
}

// Top-level arguments. Field `///` comments are the `--help` text.
#[derive(Parser, Clone)]
#[command(
    version,
    about = "Capture a Windows window with transparent corners and shadows",
    after_help = "Run without arguments to search and select a window. F2 picks by clicking; F3 picks under the cursor after a delay; F4 edits capture settings."
)]
struct Cli {
    /// Print visible windows as tab-separated values and exit; --window-title filters.
    #[arg(long, conflicts_with_all = ["window_hwnd", "list_displays"])]
    list_windows: bool,
    /// Print active displays (names for --high-dpi-display) as tab-separated values and exit.
    #[arg(long, exclusive = true)]
    list_displays: bool,
    /// Target window handle in decimal or 0x hexadecimal.
    #[arg(long, alias = "hwnd", value_parser = parse_handle, conflicts_with_all = ["window_title", "pick_mode"])]
    window_hwnd: Option<usize>,
    /// Match a visible window title; choose interactively if several match.
    #[arg(long, alias = "title", value_parser = nonempty)]
    window_title: Option<String>,
    /// Searchable row picker, delayed cursor sampling, or a click-to-select border.
    #[arg(long, value_enum, default_value = "list")]
    pick_mode: PickMode,
    /// Seconds before cursor sampling or starting the overlay picker.
    #[arg(long, default_value_t = 2.0, value_parser = delay)]
    pick_delay: f64,
    /// Seconds to wait after choosing a window (e.g. to open a menu).
    #[arg(long, default_value_t = 0.0, value_parser = delay)]
    post_pick_delay: f64,
    /// Overlay picker border thickness in physical pixels.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=32))]
    pick_overlay_border_thickness: u32,
    /// Pick the child window under the cursor instead of its root owner.
    #[arg(long)]
    pick_child: bool,
    /// Capture the window's focused or unfocused appearance.
    #[arg(long, value_enum, default_value = "focused")]
    focus_state: FocusState,
    /// Move to an active monitor above 100% scale; errors if none is available.
    #[arg(long)]
    high_dpi: bool,
    /// High-DPI Windows display name, e.g. \\.\DISPLAY2.
    #[arg(long, requires = "high_dpi", value_parser = nonempty)]
    high_dpi_display: Option<String>,
    /// Skip restoring the previous foreground window.
    #[arg(long)]
    no_restore_foreground: bool,
    /// Draw a cursor snapshot over the recovered image.
    #[arg(long)]
    include_cursor: bool,
    /// Cursor shadow setting; auto follows the Windows preference.
    #[arg(long, value_enum, default_value = "auto")]
    cursor_shadow: CursorShadow,
    /// Recover transparency inside the client area too (requires static content).
    #[arg(long)]
    transparent_client: bool,
    /// Use calibrated DWM shadow margins or simple symmetric padding.
    #[arg(long, value_enum, default_value = "shadow")]
    pad_mode: PadMode,
    /// DWM shadow style (0..5), otherwise inferred from focus/maximized state.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=5))]
    shadow_style_hint: Option<u8>,
    /// Extra physical pixels beyond the shadow, or the uniform padding.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(0..=4096))]
    pad: u32,
    /// Maximum milliseconds per backdrop to wait until the window stops changing; 0 takes the first frame.
    #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(0..=60000))]
    settle_ms: u64,
    /// Also save the original white/black PNG composites, with DPI metadata.
    #[arg(long)]
    raw_dir: Option<PathBuf>,
    /// Filename prefix for raw captures (a filename, not a path).
    #[arg(long, default_value = "capture", value_parser = filename)]
    raw_prefix: String,
    #[command(flatten)]
    output: OutputArgs,
    #[command(flatten)]
    alpha: AlphaArgs,
}

// Value parsers for clap. `str::parse` accepts NaN and infinity; these reject them.

/// Parses a finite float in `0..=1`.
fn unit_float(text: &str) -> std::result::Result<f32, String> {
    let value: f32 = text
        .parse()
        .map_err(|_| "Expected a number between 0 and 1")?;
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err("Expected a finite number between 0 and 1".into())
    }
}
/// Parses a float in `(0, 1]`; the value is a divisor.
fn positive_unit_float(text: &str) -> std::result::Result<f32, String> {
    let value = unit_float(text)?;
    if value > 0.0 {
        Ok(value)
    } else {
        Err("Expected a number greater than 0".into())
    }
}
/// Parses seconds in `0..=3600`; the cap keeps `Duration::from_secs_f64` from overflowing.
fn delay(text: &str) -> std::result::Result<f64, String> {
    let value: f64 = text
        .parse()
        .map_err(|_| "Expected seconds between 0 and 3600")?;
    if value.is_finite() && (0.0..=3600.0).contains(&value) {
        Ok(value)
    } else {
        Err("Expected finite seconds between 0 and 3600".into())
    }
}
/// Parses a nonzero window handle in decimal or `0x` hexadecimal, as `--list-windows` prints it.
fn parse_handle(text: &str) -> std::result::Result<usize, String> {
    let value = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        usize::from_str_radix(hex, 16)
    } else {
        text.parse()
    }
    .map_err(|_| "Expected a nonzero decimal or 0x hexadecimal window handle")?;
    if value > 0 {
        Ok(value)
    } else {
        Err("Window handle must not be zero".into())
    }
}
/// Rejects empty and whitespace-only values.
fn nonempty(text: &str) -> std::result::Result<String, String> {
    if text.trim().is_empty() {
        Err("Value must not be empty".into())
    } else {
        Ok(text.to_owned())
    }
}
/// Accepts one path component that is a valid Windows filename, so the raw
/// prefix cannot leave `--raw-dir`.
fn filename(text: &str) -> std::result::Result<String, String> {
    if text.trim().is_empty()
        || text.ends_with(['.', ' '])
        || text
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || !matches!(
            Path::new(text).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        )
    {
        Err("Prefix must be a single valid filename without path separators".into())
    } else {
        Ok(text.to_owned())
    }
}

/// Solves alpha from the composites, saves the image, and prints the report.
///
/// Opaque regions come from the capture unless `--transparent-client` is set.
fn write_capture(
    capture: &capture::Composites,
    path: &Path,
    output: &OutputArgs,
    alpha: &AlphaArgs,
    transparent_client: bool,
) -> Result<()> {
    let mut options = alpha.options();
    if !transparent_client {
        options.opaque_regions.clone_from(&capture.opaque_regions);
    }
    options.overlays.clone_from(&capture.cursor);
    let result = alpha::recover(&capture.white, &capture.black, &options)?;
    let dpi = capture.window.dpi;
    output::save(
        &result.image,
        path,
        &SaveOptions {
            format: output.out_format,
            dpi: Some(dpi),
            overwrite: output.force,
        },
    )?;
    report::saved(
        result.image,
        path,
        &capture.window,
        output.out_format,
        output.no_preview,
    )?;
    Ok(())
}

/// Escapes a TSV text field as `escape_debug` does, but only for controls, bidi controls,
/// backslashes and quotes, so emoji and combining marks stay readable.
fn tsv_field(value: &str) -> String {
    let mut text = String::new();
    for c in value.chars() {
        if report::needs_escape(c) || matches!(c, '\\' | '\'' | '"') {
            text.extend(c.escape_debug());
        } else {
            text.push(c);
        }
    }
    text
}

/// Writes the window table as tab-separated values with a header row.
///
/// Escaping keeps each window on one line.
fn print_windows(windows: &[WindowInfo], mut stream: impl Write) -> Result<()> {
    writeln!(stream, "hwnd\tpid\tprocess\twidth\theight\tdpi\ttitle")?;
    for window in windows {
        writeln!(
            stream,
            "0x{:X}\t{}\t{}\t{}\t{}\t{}\t{}",
            window.handle,
            window.pid,
            tsv_field(window.process_name.as_deref().unwrap_or("<unknown>")),
            window.rect.width(),
            window.rect.height(),
            window.dpi,
            tsv_field(&window.title)
        )?;
    }
    Ok(())
}

/// Writes the monitor table as tab-separated values with a header row.
///
/// `name` is the value `--high-dpi-display` takes. `scale` is the DPI as a percentage of 96.
fn print_displays(monitors: &[MonitorInfo], mut stream: impl Write) -> Result<()> {
    writeln!(
        stream,
        "name\tleft\ttop\twidth\theight\twork_width\twork_height\tdpi\tscale\tdescription"
    )?;
    for monitor in monitors {
        writeln!(
            stream,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            monitor.name,
            monitor.rect.left,
            monitor.rect.top,
            monitor.rect.width(),
            monitor.rect.height(),
            monitor.work.width(),
            monitor.work.height(),
            monitor.dpi,
            monitor.dpi * 100 / 96,
            tsv_field(&monitor.description)
        )?;
    }
    Ok(())
}

/// Chooses a window from the searchable list, pre-filtered by `--window-title`.
///
/// One title match returns without prompting. Several matches without a
/// terminal print the candidates to stderr and fail.
fn pick_list(args: &mut Cli) -> Result<picker::Selection> {
    let title_filter = args.window_title.clone();
    let filter = title_filter.as_deref();
    let mut windows = capture::list_windows()?;
    if let Some(filter) = filter {
        let needle = filter.to_lowercase();
        windows.retain(|window| window.title.to_lowercase().contains(&needle));
        ensure!(
            !windows.is_empty(),
            "No visible window matched the requested title"
        );
        if windows.len() == 1 {
            return Ok(picker::Selection::Window(windows[0].handle));
        }
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            print_windows(&windows, io::stderr().lock())?;
            anyhow::bail!(
                "{} windows matched --window-title; use a more specific title or --window-hwnd",
                windows.len()
            );
        }
    }
    ensure!(!windows.is_empty(), "No selectable windows found");
    picker::pick(windows, filter, args)
}

/// Installs a Ctrl+C handler that sets `cancelled`, so waits and the capture
/// can restore the desktop state before exiting.
fn install_cancel(cancelled: &Arc<AtomicBool>) -> Result<()> {
    #[cfg(windows)]
    {
        let cancelled = Arc::clone(cancelled);
        ctrlc::set_handler(move || cancelled.store(true, Ordering::Relaxed))
            .context("Cannot install Ctrl+C handler")?;
    }
    #[cfg(not(windows))]
    let _ = cancelled;
    Ok(())
}
/// Sleeps in 15 ms steps; fails with "Capture cancelled" once `cancelled` is set.
fn wait(seconds: f64, cancelled: &AtomicBool) -> Result<()> {
    let until = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < until {
        ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
        thread::sleep(
            until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(15)),
        );
    }
    Ok(())
}

/// Runs one invocation; see the module docs.
///
/// Destination checks run before `capture::capture` so a bad path fails before any capture.
fn run(mut args: Cli) -> Result<()> {
    capture::enable_per_monitor_dpi();
    if args.list_displays {
        return print_displays(&capture::list_monitors()?, io::stdout().lock());
    }
    if args.list_windows {
        let mut windows = capture::list_windows()?;
        if let Some(filter) = &args.window_title {
            let needle = filter.to_lowercase();
            windows.retain(|window| window.title.to_lowercase().contains(&needle));
        }
        return print_windows(&windows, io::stdout().lock());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let selection = if let Some(handle) = args.window_hwnd {
        picker::Selection::Window(handle)
    } else {
        match args.pick_mode {
            PickMode::List => pick_list(&mut args)?,
            PickMode::Cursor => picker::Selection::Cursor,
            PickMode::Overlay => picker::Selection::Overlay,
        }
    };
    install_cancel(&cancelled)?;
    let handle = match selection {
        picker::Selection::Window(handle) => handle,
        picker::Selection::Cursor => {
            eprintln!(
                "Move the cursor over your window; sampling in {} seconds",
                args.pick_delay
            );
            wait(args.pick_delay, &cancelled)?;
            let handle = capture::pick_cursor(!args.pick_child)?;
            if let Some(filter) = &args.window_title {
                ensure!(
                    capture::window_info(handle)?
                        .title
                        .to_lowercase()
                        .contains(&filter.to_lowercase()),
                    "The window under the cursor does not match --window-title"
                );
            }
            handle
        }
        picker::Selection::Overlay => {
            eprintln!("Click the window to capture; Esc cancels");
            wait(args.pick_delay, &cancelled)?;
            capture::pick_overlay(
                args.window_title.as_deref(),
                args.pick_overlay_border_thickness,
                &cancelled,
            )?
        }
    };
    let window = capture::window_info(handle)?;
    let path = args
        .output
        .out
        .clone()
        .unwrap_or_else(|| output::default_path(&window.title, args.output.out_format));
    args.output.out_format.resolve(&path)?;
    output::check_destination(&path, args.output.force)?;
    let raw = args.raw_dir.as_ref().map(|dir| {
        (
            dir.join(format!("{}-white.png", args.raw_prefix)),
            dir.join(format!("{}-black.png", args.raw_prefix)),
        )
    });
    if let Some((white, black)) = &raw {
        output::check_destination(white, args.output.force)?;
        output::check_destination(black, args.output.force)?;
        ensure!(
            !same_path(&path, white)? && !same_path(&path, black)?,
            "Recovered output must differ from the raw capture paths"
        );
    }
    report::capturing(&window.title)?;
    wait(args.post_pick_delay, &cancelled)?;
    let capture = capture::capture(
        handle,
        &CaptureOptions {
            focus: args.focus_state,
            pad_mode: args.pad_mode,
            pad: args.pad,
            shadow_style: args.shadow_style_hint,
            settle_ms: args.settle_ms,
            high_dpi: args.high_dpi,
            high_dpi_display: args.high_dpi_display,
            restore_foreground: !args.no_restore_foreground,
            include_cursor: args.include_cursor,
            cursor_shadow: args.cursor_shadow,
        },
        &cancelled,
    )?;
    if let Some((white, black)) = raw {
        output::save_composite(
            &capture.white,
            &white,
            capture.window.dpi,
            args.output.force,
        )?;
        output::save_composite(
            &capture.black,
            &black,
            capture.window.dpi,
            args.output.force,
        )?;
        report::saved_raw(&white, &black, &capture.window.title)?;
    }
    write_capture(
        &capture,
        &path,
        &args.output,
        &args.alpha,
        args.transparent_client,
    )?;
    Ok(())
}

/// Reports whether two paths name the same file, existing or not.
///
/// Existing paths are canonicalized. Missing paths are made absolute with `.`
/// and `..` resolved lexically. The final comparison is ordinal and case-insensitive
/// (`CompareStringOrdinal`), as NTFS compares names.
fn same_path(a: &Path, b: &Path) -> Result<bool> {
    fn normalized(path: &Path) -> Result<PathBuf> {
        if path.try_exists()? {
            return Ok(path.canonicalize()?);
        }
        let absolute = std::path::absolute(path)?;
        let mut parts = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        Ok(parts)
    }
    let (a, b) = (normalized(a)?, normalized(b)?);
    #[cfg(windows)]
    {
        // Ordinal ignore-case uses the Unicode uppercase table NTFS uses.
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
        let (a, b) = (
            a.as_os_str().encode_wide().collect::<Vec<_>>(),
            b.as_os_str().encode_wide().collect::<Vec<_>>(),
        );
        let (Ok(a_len), Ok(b_len)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
            return Ok(false);
        };
        // SAFETY: both buffers outlive the call and their lengths are passed.
        Ok(unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) } == CSTR_EQUAL)
    }
    #[cfg(not(windows))]
    {
        Ok(a == b)
    }
}

/// Exits 0 on success, 1 on error. A closed stdout pipe (`| head`) exits 1 silently.
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                return ExitCode::FAILURE;
            }
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn missing_paths_compare_with_unicode_case_folding() {
        let dir = tempfile::tempdir().unwrap();
        let (lower, upper) = (
            dir.path().join("ä-white.png"),
            dir.path().join("Ä-WHITE.png"),
        );
        assert!(same_path(&lower, &upper).unwrap());
        assert!(!same_path(&lower, &dir.path().join("a-white.png")).unwrap());
    }

    #[test]
    fn list_output_is_tab_separated_with_escaped_text() {
        use alphashot::capture::Rect;
        let rect = |left, top, right, bottom| Rect {
            left,
            top,
            right,
            bottom,
        };
        let window = WindowInfo {
            handle: 0xab,
            pid: 7,
            title: "a\tb ❤️ \"x\"".into(),
            process_name: None,
            rect: rect(0, 0, 640, 480),
            dpi: 96,
            class_name: String::new(),
        };
        let mut out = Vec::new();
        print_windows(&[window], &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "hwnd\tpid\tprocess\twidth\theight\tdpi\ttitle\n0xAB\t7\t<unknown>\t640\t480\t96\ta\\tb ❤️ \\\"x\\\"\n"
        );
        let monitor = MonitorInfo {
            name: "DISPLAY1".into(),
            rect: rect(-10, 5, 1910, 1085),
            work: rect(-10, 5, 1910, 1045),
            dpi: 144,
            description: "Mon\titor".into(),
            handle: 1,
        };
        let mut out = Vec::new();
        print_displays(&[monitor], &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let mut lines = out.lines();
        assert_eq!(lines.next().unwrap().split('\t').count(), 10);
        assert_eq!(
            lines.next().unwrap(),
            "DISPLAY1\t-10\t5\t1920\t1080\t1920\t1040\t144\t150\tMon\\titor"
        );
    }

    #[test]
    fn cli_rejects_unsafe_or_nonsensical_arguments() {
        for args in [
            vec!["--list-displays", "--window-title", "x"],
            vec!["--list-windows", "--window-hwnd", "1"],
            vec!["--window-hwnd", "0"],
            vec!["recover", "white.png"],
            vec!["--crop-threshold", "NaN"],
            vec!["--pick-delay", "inf"],
            vec!["--alpha-epsilon", "0"],
            vec!["--webp-quality", "100"],
            vec!["--webp-lossy"],
            vec!["--high-dpi-display", "display2"],
            vec!["--sixel"],
            vec!["--raw-prefix", "../oops"],
            vec!["--window-hwnd", "12", "--window-title", "x"],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("alphashot").chain(args)).is_err());
        }
        assert!(Cli::try_parse_from(["alphashot"]).is_ok());
        assert!(Cli::try_parse_from(["alphashot", "--window-hwnd", "0x123"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "alphashot",
                "--high-dpi",
                "--high-dpi-display",
                r"\\.\DISPLAY2"
            ])
            .is_ok()
        );
    }
}
