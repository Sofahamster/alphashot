//! Command-line entry point for `alphashot`.
//!
//! `run` drives one invocation:
//!
//! 1. Enable per-monitor DPI awareness so coordinates and sizes are physical pixels.
//! 2. For `--list-displays` and `--list-windows`, print tab-separated rows to
//!    stdout and exit. Text fields are escaped so a tab or newline in a title
//!    cannot split a row.
//! 3. Select the target by handle (`--window-hwnd`), window filters, or pick mode.
//! 4. Validate the output and raw-composite destinations before capturing. The
//!    output must not name the same file as a raw composite.
//! 5. Solve backdrop composites or capture native WGC alpha, save,
//!    copy the image to the clipboard, and print the report. `--clipboard-only`
//!    skips saving; `--editor` opens the saved image in the chosen application.
//! 6. With `--repeat`, retain the target and settings and wait for the next shot.
//!
//! Exit codes: 0 success, 1 runtime error, 2 argument error (clap default). A
//! closed stdout pipe exits 1 without a message.
//!
//! When stdout is redirected, it receives only the saved paths, one per line.
//! Progress, prompts, and warnings go to stderr.

use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use alphashot::{
    alpha::{self, AlphaOptions},
    capture::{
        self, CaptureBackend, CaptureOptions, CursorShadow, FocusState, MonitorInfo, PadMode,
        WindowFilter, WindowInfo,
    },
    output::{self, OutputFormat, SaveOptions, TiffAlpha},
};
use anyhow::{Context, Result, ensure};
use clap::{
    ArgGroup, Args, CommandFactory, FromArgMatches, Parser, ValueEnum, builder::TypedValueParser,
};

#[cfg(windows)]
mod clipboard;
mod picker;
mod presets;
mod report;
mod session;
mod sixel;

// Plain comments: clap folds `///` on derived items into `--help`.

// Output destination, format, and reporting.
#[derive(Args, Clone)]
struct OutputArgs {
    /// Output path; defaults to a timestamp and window title.
    #[arg(short, long = "out", alias = "output")]
    out: Option<PathBuf>,
    /// Folder for automatic output filenames; --out takes precedence.
    #[arg(long, value_name = "DIR", value_parser = clap::builder::PathBufValueParser::new().try_map(nonempty_path))]
    out_dir: Option<PathBuf>,
    /// Output format, otherwise inferred from the file extension.
    #[arg(long, alias = "format", value_enum, default_value = "auto")]
    out_format: OutputFormat,
    /// TIFF alpha association; PNG/WebP always use straight alpha. Premultiplication loses 8-bit precision.
    #[arg(long, value_enum, default_value = "straight")]
    tiff_alpha: TiffAlpha,
    /// Replace existing output files. Writes are atomic.
    #[arg(short, long)]
    force: bool,
    /// Disable window-picker and saved-image thumbnail previews.
    #[arg(long)]
    no_preview: bool,
    /// Save without copying the image and file to the Windows clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Copy only the transparent image to the clipboard, without saving files.
    #[arg(long, conflicts_with_all = ["out", "out_dir", "tiff_alpha", "raw_dir", "no_clipboard", "editor", "editor_arg"])]
    clipboard_only: bool,
    /// Open the saved image in this executable after capture.
    #[arg(long, value_name = "EXE", value_parser = clap::builder::PathBufValueParser::new().try_map(editor_executable))]
    editor: Option<PathBuf>,
    /// Argument for --editor; repeat for multiple arguments, including flags.
    #[arg(
        long,
        value_name = "ARG",
        requires = "editor",
        allow_hyphen_values = true
    )]
    editor_arg: Vec<OsString>,
}

impl OutputArgs {
    /// Resolves the explicit file or the automatic filename in the selected folder.
    fn path_for(&self, title: &str) -> PathBuf {
        self.out.clone().unwrap_or_else(|| {
            let mut path = self.out_dir.clone().unwrap_or_default();
            path.push(output::default_path(title, self.out_format));
            path
        })
    }
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
    group(ArgGroup::new("window_selector").args(["window_title", "process", "window_class"]).multiple(true)),
    after_help = "Run without arguments to search and select a window. F2 picks by clicking; F3 picks under the cursor after a delay; F4 edits capture settings."
)]
struct Cli {
    /// Apply the named preset's defaults; explicit command-line options take precedence.
    #[arg(long, value_name = "NAME", value_parser = presets::name)]
    preset: Option<String>,
    /// Save these settings as a named preset and exit without capturing.
    #[arg(long, value_name = "NAME", value_parser = presets::name, conflicts_with_all = ["list_presets", "list_windows", "list_displays"])]
    save_preset: Option<String>,
    /// Print saved preset names and exit without capturing.
    #[arg(long, conflicts_with_all = ["preset", "list_windows", "list_displays"])]
    list_presets: bool,
    /// Preset file; defaults to %APPDATA%/alphashot/presets.ini.
    #[arg(long, value_name = "PATH", value_parser = clap::builder::PathBufValueParser::new().try_map(nonempty_path))]
    presets_file: Option<PathBuf>,
    #[arg(skip)]
    preset_overrides: Vec<String>,
    /// Print visible windows as tab-separated values and exit; window selectors filter.
    #[arg(long, conflicts_with_all = ["window_hwnd", "list_displays"])]
    list_windows: bool,
    /// Print active displays (names for --high-dpi-display) as tab-separated values and exit.
    #[arg(long, exclusive = true)]
    list_displays: bool,
    /// Target window handle in decimal or 0x hexadecimal.
    #[arg(long, alias = "hwnd", value_parser = parse_handle, conflicts_with_all = ["window_selector", "wait", "pick_mode"])]
    window_hwnd: Option<usize>,
    /// Select once, capture again with Enter, and quit with q or Esc. Files are numbered.
    #[arg(long, conflicts_with = "list_windows")]
    repeat: bool,
    /// Match a visible window title substring, ignoring case; combine selectors with AND.
    #[arg(long, alias = "title", value_parser = nonempty)]
    window_title: Option<String>,
    /// Match an exact executable basename (e.g. notepad.exe), ignoring case.
    #[arg(long, value_parser = nonempty)]
    process: Option<String>,
    /// Match an exact Win32 window class, ignoring case.
    #[arg(long, value_parser = nonempty)]
    window_class: Option<String>,
    /// Wait up to this many seconds for a visible window matching the selectors (0..3600).
    #[arg(long, requires = "window_selector", value_parser = delay)]
    wait: Option<f64>,
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
    /// Pick the owned window under the cursor (e.g. a dialog) instead of its root owner.
    #[arg(long)]
    pick_child: bool,
    /// Capture using white/black backdrops or WGC without activating the window.
    #[arg(long, value_enum, default_value = "backdrop")]
    backend: CaptureBackend,
    /// Exclude DWM shadow; standard rounded frames use an approximate feathered corner mask.
    #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true", default_value = "false", require_equals = true)]
    no_shadow: bool,
    /// Backdrop backend: capture the window's focused or unfocused appearance; WGC keeps it as is.
    #[arg(long, value_enum, default_value = "focused")]
    focus_state: FocusState,
    /// Render on a high-DPI monitor; WGC moves temporarily only when needed, without activation.
    /// Automatic selection chooses the highest DPI, then largest display area.
    #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true", default_value = "false", require_equals = true)]
    high_dpi: bool,
    /// High-DPI Windows display name, e.g. \\.\DISPLAY2.
    #[arg(long, value_parser = nonempty)]
    high_dpi_display: Option<String>,
    /// Temporarily set exact physical client pixels before capture; never resamples the image.
    #[arg(long, value_name = "WIDTHxHEIGHT", value_parser = parse_client_size)]
    client_size: Option<(u32, u32)>,
    /// Skip restoring the previous foreground window.
    #[arg(long)]
    no_restore_foreground: bool,
    /// Draw a cursor snapshot over the recovered image.
    #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true", default_value = "false", require_equals = true)]
    include_cursor: bool,
    /// Cursor shadow setting; auto follows the Windows preference.
    #[arg(long, value_enum, default_value = "auto")]
    cursor_shadow: CursorShadow,
    /// Backdrop backend: recover client transparency too (requires static content); WGC keeps native alpha.
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

impl Cli {
    fn validate_backend_output(&self) -> Result<()> {
        ensure!(
            self.backend != CaptureBackend::Wgc || self.raw_dir.is_none(),
            "--raw-dir requires --backend backdrop; WGC returns native RGBA without white/black composites"
        );
        Ok(())
    }

    fn window_filter(&self) -> WindowFilter {
        WindowFilter {
            title: self.window_title.clone(),
            process: self.process.clone(),
            class_name: self.window_class.clone(),
        }
    }
    /// Keeps actual CLI provenance when the picker later applies another preset.
    fn with_sources(matches: &clap::ArgMatches) -> std::result::Result<Self, clap::Error> {
        let mut args = Self::from_arg_matches(matches)?;
        args.preset_overrides = matches
            .ids()
            .filter(|id| {
                matches.value_source(id.as_str()) == Some(clap::parser::ValueSource::CommandLine)
            })
            .map(ToString::to_string)
            .collect();
        Ok(args)
    }
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
/// Parses physical client dimensions within the capture canvas limit.
fn parse_client_size(text: &str) -> std::result::Result<(u32, u32), String> {
    let error = "Expected WIDTHxHEIGHT with positive dimensions and at most 40 million pixels";
    let (width, height) = text.split_once(['x', 'X']).ok_or(error)?;
    let (width, height) = (
        width.parse::<u32>().map_err(|_| error)?,
        height.parse::<u32>().map_err(|_| error)?,
    );
    if width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 40_000_000 {
        Ok((width, height))
    } else {
        Err(error.into())
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

fn nonempty_path(path: PathBuf) -> std::result::Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        Err("Path must not be empty".into())
    } else {
        Ok(path)
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

/// Accepts native Windows executables so untrusted filenames never reach a batch shell.
fn editor_executable(path: PathBuf) -> std::result::Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("Editor executable must not be empty".into());
    }
    #[cfg(windows)]
    if path.extension().is_some_and(|ext| {
        !ext.to_str()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    }) {
        return Err(
            "Editor must be a native .exe program; .bat and .cmd scripts are not supported".into(),
        );
    }
    Ok(path)
}

/// Keeps each explicit argument and the absolute saved path separate, without a shell.
fn editor_command(executable: &Path, args: &[OsString], path: &Path) -> Result<Command> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .arg(std::path::absolute(path).context("Cannot resolve editor file path")?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

/// Solves alpha, saves or copies the image, and prints the report.
///
/// Opaque regions come from the capture unless `--transparent-client` is set.
fn write_capture(
    capture: &capture::Composites,
    path: Option<&Path>,
    output: &OutputArgs,
    alpha: &AlphaArgs,
    transparent_client: bool,
) -> Result<()> {
    let mut options = alpha.options();
    options.blend_space = capture.blend_space;
    options.corner_radius = capture.corner_radius;
    if !transparent_client {
        options.opaque_regions.clone_from(&capture.opaque_regions);
    }
    options.overlays.clone_from(&capture.cursor);
    let result = alpha::recover(&capture.white, &capture.black, &options)?;
    write_image(result.image, &capture.window, path, output)
}

/// Saves, copies and reports the canonical straight-alpha pixels from either backend.
fn write_image(
    image: image::RgbaImage,
    window: &WindowInfo,
    path: Option<&Path>,
    output: &OutputArgs,
) -> Result<()> {
    let dpi = window.dpi;
    let Some(path) = path else {
        #[cfg(windows)]
        {
            clipboard::copy(&image, None, dpi).context("Clipboard copy failed")?;
            writeln!(
                io::stderr().lock(),
                "Copied {} × {} px · {dpi} DPI to clipboard",
                image.width(),
                image.height()
            )?;
            return Ok(());
        }
        #[cfg(not(windows))]
        anyhow::bail!("Clipboard-only capture requires Windows");
    };
    output::save(
        &image,
        path,
        &SaveOptions {
            format: output.out_format,
            tiff_alpha: output.tiff_alpha,
            dpi: Some(dpi),
            overwrite: output.force,
        },
    )
    .with_context(|| format!("Cannot save capture to {}", path.display()))?;
    #[cfg(windows)]
    if !output.no_clipboard
        && let Err(error) = clipboard::copy(&image, Some(path), dpi)
    {
        eprintln!("Warning: Image saved, but clipboard copy failed: {error:#}");
    }
    if let Some(editor) = &output.editor
        && let Err(error) = editor_command(editor, &output.editor_arg, path)
            .and_then(|mut command| command.spawn().context("Cannot launch editor"))
    {
        eprintln!("Warning: Image saved, but editor launch failed: {error:#}");
    }
    report::saved(image, path, window, output.out_format, output.no_preview)?;
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
    writeln!(
        stream,
        "hwnd\tpid\tprocess\twidth\theight\tdpi\ttitle\tclass"
    )?;
    for window in windows {
        writeln!(
            stream,
            "0x{:X}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            window.handle,
            window.pid,
            tsv_field(window.process_name.as_deref().unwrap_or("<unknown>")),
            window.rect.width(),
            window.rect.height(),
            window.dpi,
            tsv_field(&window.title),
            tsv_field(&window.class_name)
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

/// Chooses a window from the searchable list, pre-filtered by the window selectors.
///
/// One selector match returns without prompting. Several matches without a
/// terminal print the candidates to stderr and fail.
fn pick_list(windows: Vec<WindowInfo>, args: &mut Cli) -> Result<picker::Selection> {
    if !args.window_filter().is_empty() {
        ensure!(
            !windows.is_empty(),
            "No visible window matched the requested selectors; inspect --list-windows or adjust --window-title, --process or --window-class"
        );
        if windows.len() == 1 {
            return Ok(picker::Selection::Window(windows[0].handle));
        }
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            print_windows(&windows, io::stderr().lock())?;
            anyhow::bail!(
                "{} windows matched the requested selectors; narrow --window-title, --process or --window-class, or use --window-hwnd",
                windows.len()
            );
        }
    }
    ensure!(!windows.is_empty(), "No selectable windows found");
    picker::pick(windows, args)
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

/// Polls the shared selectors, retaining the first nonempty set without resolving ambiguity.
fn matching_windows(
    filter: &WindowFilter,
    seconds: Option<f64>,
    cancelled: &AtomicBool,
    mut list: impl FnMut() -> Result<Vec<WindowInfo>>,
) -> Result<Vec<WindowInfo>> {
    let until = seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    loop {
        ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
        let mut windows = list()?;
        windows.retain(|window| filter.matches(window));
        if !windows.is_empty() || until.is_none() {
            return Ok(windows);
        }
        let remaining = until.unwrap().saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "No visible window matched the requested selectors within {} seconds; inspect --list-windows or adjust --window-title, --process or --window-class",
            seconds.unwrap()
        );
        wait(
            remaining.min(Duration::from_millis(100)).as_secs_f64(),
            cancelled,
        )?;
    }
}

/// Runs one invocation; see the module docs.
///
/// Destination checks run before `capture::capture` so a bad path fails before any capture.
fn run(mut args: Cli) -> Result<()> {
    if args.list_presets {
        for name in presets::load(&args)?.keys() {
            writeln!(io::stdout().lock(), "{name}")?;
        }
        return Ok(());
    }
    if let Some(name) = args.preset.clone() {
        let presets = presets::load(&args)?;
        presets::apply_named(&mut args, &presets, &name)?;
    }
    presets::validate_dpi(&args)?;
    if let Some(name) = &args.save_preset {
        return presets::save(&args, name);
    }
    if !args.list_displays && !args.list_windows {
        args.validate_backend_output()?;
    }
    capture::enable_per_monitor_dpi();
    if args.list_displays {
        return print_displays(&capture::list_monitors()?, io::stdout().lock());
    }
    if args.repeat {
        ensure!(
            io::stdin().is_terminal() && io::stderr().is_terminal(),
            "--repeat requires interactive stdin and stderr; run it in a terminal (stdout may be redirected)"
        );
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    install_cancel(&cancelled)?;
    let filter = args.window_filter();
    let windows = if args.list_windows
        || args.wait.is_some()
        || (args.window_hwnd.is_none() && args.pick_mode == PickMode::List)
    {
        matching_windows(&filter, args.wait, &cancelled, capture::list_windows)?
    } else {
        Vec::new()
    };
    if args.list_windows {
        return print_windows(&windows, io::stdout().lock());
    }
    let selection = if let Some(handle) = args.window_hwnd {
        picker::Selection::Window(handle)
    } else {
        match args.pick_mode {
            PickMode::List => pick_list(windows, &mut args)?,
            PickMode::Cursor => picker::Selection::Cursor,
            PickMode::Overlay => picker::Selection::Overlay,
        }
    };
    args.validate_backend_output()?;
    let handle = match selection {
        picker::Selection::Window(handle) => handle,
        picker::Selection::Cursor => {
            eprintln!(
                "Move the cursor over your window; sampling in {} seconds",
                args.pick_delay
            );
            wait(args.pick_delay, &cancelled)?;
            capture::pick_cursor(!args.pick_child)?
        }
        picker::Selection::Overlay => {
            eprintln!("Click the window to capture; Esc cancels");
            wait(args.pick_delay, &cancelled)?;
            capture::pick_overlay(&filter, args.pick_overlay_border_thickness, &cancelled)?
        }
    };
    let mut window = capture::window_info(handle)?;
    ensure!(
        filter.matches(&window),
        "The selected window does not match the requested selectors; inspect --list-windows or choose another window"
    );
    let base = if args.output.clipboard_only {
        None
    } else {
        let path = args.output.path_for(&window.title);
        args.output
            .tiff_alpha
            .validate(args.output.out_format.resolve(&path)?)?;
        Some(path)
    };
    if args.repeat {
        // Numbered sessions never replace previous shots, even with --force.
        args.output.force = false;
    }
    let target = session::Target::new(&window);
    let mut number = 0;
    #[cfg(windows)]
    let mut wgc = None;
    loop {
        if args.repeat {
            window = target.window()?;
        }
        let paths = if args.repeat {
            session::Paths::next(
                base.as_deref(),
                args.raw_dir.as_deref(),
                &args.raw_prefix,
                &mut number,
            )?
        } else {
            session::Paths::new(
                base.as_deref(),
                args.raw_dir.as_deref(),
                &args.raw_prefix,
                None,
            )?
        };
        if let Some(path) = &paths.image {
            output::check_destination(path, args.output.force)?;
        }
        if let Some((white, black)) = &paths.raw {
            output::check_destination(white, args.output.force)?;
            output::check_destination(black, args.output.force)?;
            if let Some(path) = &paths.image {
                ensure!(
                    !same_path(path, white)? && !same_path(path, black)?,
                    "Recovered output must differ from the raw capture paths"
                );
            }
        }
        report::capturing(&window.title)?;
        wait(args.post_pick_delay, &cancelled)?;
        if args.repeat {
            target.window()?;
        }
        let options = CaptureOptions {
            backend: args.backend,
            no_shadow: args.no_shadow,
            focus: args.focus_state,
            pad_mode: args.pad_mode,
            pad: args.pad,
            shadow_style: args.shadow_style_hint,
            settle_ms: args.settle_ms,
            high_dpi: args.high_dpi,
            high_dpi_display: args.high_dpi_display.clone(),
            client_size: args.client_size,
            restore_foreground: !args.no_restore_foreground,
            include_cursor: args.include_cursor,
            cursor_shadow: args.cursor_shadow,
        };
        match args.backend {
            CaptureBackend::Backdrop => {
                let capture = capture::capture(handle, &options, &cancelled)?;
                if let Some((white, black)) = paths.raw {
                    for (image, path) in [(&capture.white, &white), (&capture.black, &black)] {
                        output::save_composite(image, path, capture.window.dpi, args.output.force)
                            .with_context(|| {
                                format!("Cannot save raw capture to {}", path.display())
                            })?;
                    }
                    report::saved_raw(&white, &black, &capture.window.title)?;
                }
                write_capture(
                    &capture,
                    paths.image.as_deref(),
                    &args.output,
                    &args.alpha,
                    args.transparent_client,
                )?;
            }
            CaptureBackend::Wgc => {
                #[cfg(windows)]
                {
                    if wgc.is_none() {
                        wgc = Some(capture::snapshot::Snapshotter::new().context(
                            "Cannot initialize WGC; use --backend backdrop if unavailable",
                        )?);
                    }
                    let capture =
                        capture::capture_wgc(wgc.as_ref().unwrap(), handle, &options, &cancelled)?;
                    let mut alpha = args.alpha.options();
                    alpha.overlays = capture.cursor;
                    let result = alpha::finish(capture.image, &alpha)?;
                    write_image(
                        result.image,
                        &capture.window,
                        paths.image.as_deref(),
                        &args.output,
                    )?;
                }
                #[cfg(not(windows))]
                anyhow::bail!("WGC capture requires Windows");
            }
        }
        if !args.repeat || !session::wait(&target, &cancelled)? {
            break;
        }
    }
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
    let matches = Cli::command().get_matches();
    let args = Cli::with_sources(&matches).unwrap_or_else(|error| error.exit());
    match run(args) {
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

    fn composites() -> capture::Composites {
        use alphashot::capture::Rect;
        use image::{Rgb, RgbImage};

        capture::Composites {
            corner_radius: 0.0,
            blend_space: alpha::BlendSpace::Srgb,
            white: RgbImage::from_fn(3, 2, |x, _| {
                Rgb([[255; 3], [127, 127, 255], [10, 20, 30]][x as usize])
            }),
            black: RgbImage::from_fn(3, 2, |x, _| {
                Rgb([[0; 3], [0, 0, 128], [10, 20, 30]][x as usize])
            }),
            window: WindowInfo {
                handle: 1,
                pid: 1,
                process_name: None,
                title: "Editor test".into(),
                class_name: String::new(),
                rect: Rect {
                    left: 0,
                    top: 0,
                    right: 3,
                    bottom: 2,
                },
                dpi: 144,
            },
            opaque_regions: Vec::new(),
            cursor: Vec::new(),
        }
    }

    #[test]
    fn editor_command_preserves_argument_boundaries_and_uses_an_absolute_path() {
        let executable = Path::new(r"C:\Program Files\Paint & more\paint.exe");
        let args = [
            "--new-window",
            "two words",
            "&|<>^()%!",
            "quote\" and trailing\\",
            "",
        ]
        .map(OsString::from);
        let path = Path::new("Büro & 100% ! (copy).png");
        let command = editor_command(executable, &args, path).unwrap();
        let absolute = std::path::absolute(path).unwrap();
        assert_eq!(command.get_program(), executable);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            args.iter()
                .map(OsString::as_os_str)
                .chain([absolute.as_os_str()])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn editor_launch_failure_keeps_the_successful_transparent_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.png");
        let mut args =
            Cli::try_parse_from(["alphashot", "--no-crop", "--no-clipboard", "--no-preview"])
                .unwrap();
        args.output.editor = Some(directory.path().join("missing-editor.exe"));
        let capture = composites();
        let expected = alpha::recover(&capture.white, &capture.black, &args.alpha.options())
            .unwrap()
            .image;
        assert!(
            expected
                .pixels()
                .any(|pixel| pixel[3] > 0 && pixel[3] < 255)
        );
        write_capture(&capture, Some(&path), &args.output, &args.alpha, true).unwrap();
        assert_eq!(image::open(path).unwrap().to_rgba8(), expected);
    }

    #[test]
    fn saved_capture_preserves_measured_sdr_shadow_alpha() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shadow.png");
        let args = Cli::parse_from(["alphashot", "--no-crop", "--no-clipboard", "--no-preview"]);
        let mut capture = composites();
        capture.blend_space = alpha::BlendSpace::Srgb;
        capture.white = image::RgbImage::from_pixel(1, 1, image::Rgb([199; 3]));
        capture.black = image::RgbImage::new(1, 1);
        write_capture(&capture, Some(&path), &args.output, &args.alpha, true).unwrap();
        assert_eq!(
            image::open(path).unwrap().into_rgba8()[(0, 0)].0,
            [0, 0, 0, 56]
        );
    }

    #[test]
    fn saved_capture_applies_its_corner_metadata_before_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("corners.png");
        let args = Cli::parse_from(["alphashot", "--no-crop", "--no-clipboard", "--no-preview"]);
        let mut capture = composites();
        capture.white = image::RgbImage::from_pixel(32, 24, image::Rgb([100, 80, 40]));
        capture.black = capture.white.clone();
        capture.corner_radius = 8.0;
        write_capture(&capture, Some(&path), &args.output, &args.alpha, false).unwrap();
        let image = image::open(path).unwrap().into_rgba8();
        assert_eq!(image[(0, 0)].0, [0; 4]);
        assert_eq!(image[(16, 12)].0, [100, 80, 40, 255]);
        assert!(image.pixels().any(|p| p[3] > 0 && p[3] < 255));
    }

    #[test]
    fn native_output_saves_straight_rgba_without_an_alpha_solve() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.png");
        let args = Cli::try_parse_from(["alphashot", "--no-clipboard", "--no-preview"]).unwrap();
        let native =
            image::RgbaImage::from_raw(3, 1, vec![0, 0, 0, 0, 199, 100, 40, 64, 90, 180, 250, 128])
                .unwrap();
        write_image(
            native.clone(),
            &composites().window,
            Some(&path),
            &args.output,
        )
        .unwrap();
        assert_eq!(image::open(path).unwrap().to_rgba8(), native);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Replaces the system clipboard; needs an interactive Windows session"]
    fn clipboard_only_creates_no_files_or_stdout_paths() {
        if std::env::var_os("ALPHASHOT_TEST_CLIPBOARD_ONLY").is_some() {
            let args = Cli::try_parse_from(["alphashot", "--clipboard-only", "--no-crop"]).unwrap();
            write_capture(&composites(), None, &args.output, &args.alpha, true).unwrap();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "tests::clipboard_only_creates_no_files_or_stdout_paths",
                "--nocapture",
            ])
            .env("ALPHASHOT_TEST_CLIPBOARD_ONLY", "1")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        assert!(!String::from_utf8_lossy(&result.stdout).contains(".png"));
        assert!(String::from_utf8_lossy(&result.stderr).contains("Copied 3 × 2 px · 144 DPI"));
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Holds the system clipboard open; needs an interactive Windows session"]
    fn clipboard_failures_only_fail_without_a_saved_file() {
        use std::sync::mpsc;
        use windows_sys::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};

        let (ready, opened) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let thread = thread::spawn(move || {
            // SAFETY: this worker opens and closes the clipboard on the same thread.
            let opened = unsafe { OpenClipboard(std::ptr::null_mut()) } != 0;
            ready.send(opened).unwrap();
            if opened {
                let _ = wait.recv();
                // SAFETY: this worker still has the clipboard open.
                unsafe { CloseClipboard() };
            }
        });
        assert!(opened.recv().unwrap(), "Cannot hold the clipboard open");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("saved.png");
        let args = Cli::try_parse_from(["alphashot", "--no-crop", "--no-preview"]).unwrap();
        let capture = composites();
        let saved = write_capture(&capture, Some(&path), &args.output, &args.alpha, true);
        let copied = write_capture(&capture, None, &args.output, &args.alpha, true);
        release.send(()).unwrap();
        thread.join().unwrap();
        assert!(saved.is_ok());
        assert!(path.is_file());
        assert!(
            copied
                .unwrap_err()
                .to_string()
                .contains("Clipboard copy failed")
        );
    }

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
            class_name: "Test\tClass".into(),
        };
        let mut out = Vec::new();
        print_windows(&[window], &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "hwnd\tpid\tprocess\twidth\theight\tdpi\ttitle\tclass\n0xAB\t7\t<unknown>\t640\t480\t96\ta\\tb ❤️ \\\"x\\\"\tTest\\tClass\n"
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
        let mut out = Vec::new();
        print_windows(&[], &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
    }

    #[test]
    fn selector_polling_preserves_ambiguity_and_stops_on_timeout_or_cancellation() {
        let cancelled = AtomicBool::new(false);
        let filter = WindowFilter {
            title: Some("TARGET".into()),
            process: Some("test.exe".into()),
            class_name: Some("testclass".into()),
        };
        let window = |handle, title: &str| WindowInfo {
            handle,
            pid: 7,
            process_name: Some("Test.EXE".into()),
            title: title.into(),
            class_name: "TestClass".into(),
            rect: capture::Rect::default(),
            dpi: 96,
        };
        let mut polls = 0;
        let windows = matching_windows(&filter, Some(1.0), &cancelled, || {
            polls += 1;
            if polls == 1 {
                Ok(vec![window(1, "Not yet")])
            } else {
                Ok(vec![window(2, "Target one"), window(3, "Target two")])
            }
        })
        .unwrap();
        assert_eq!(polls, 2);
        assert_eq!(windows.iter().map(|w| w.handle).collect::<Vec<_>>(), [2, 3]);
        let error = matching_windows(&filter, Some(0.0), &cancelled, || Ok(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("within 0 seconds"), "{error}");
        assert!(error.contains("--list-windows"), "{error}");
        assert!(
            matching_windows(
                &WindowFilter::default(),
                None,
                &cancelled,
                || Ok(Vec::new())
            )
            .unwrap()
            .is_empty()
        );
        let error = matching_windows(&filter, Some(3600.0), &cancelled, || {
            cancelled.store(true, Ordering::Relaxed);
            Ok(Vec::new())
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        matching_windows(&filter, Some(0.0), &cancelled, || {
            panic!("Cancellation must be checked before enumeration")
        })
        .unwrap_err();
    }

    #[test]
    fn filtered_list_selects_only_unique_matches() {
        let mut args = Cli::parse_from(["alphashot", "--process", "test.exe"]);
        let window = WindowInfo {
            handle: 42,
            pid: 7,
            process_name: Some("test.exe".into()),
            title: "Window".into(),
            class_name: "TestClass".into(),
            rect: capture::Rect::default(),
            dpi: 96,
        };
        assert_eq!(
            pick_list(vec![window], &mut args).unwrap(),
            picker::Selection::Window(42)
        );
        assert!(
            pick_list(Vec::new(), &mut args)
                .unwrap_err()
                .to_string()
                .contains("--list-windows")
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
            vec!["--sixel"],
            vec!["--raw-prefix", "../oops"],
            vec!["--window-hwnd", "12", "--window-title", "x"],
            vec!["--clipboard-only", "--out", "capture.png"],
            vec!["--clipboard-only", "--out-dir", "captures"],
            vec!["--clipboard-only", "--raw-dir", "raw"],
            vec!["--clipboard-only", "--no-clipboard"],
            vec!["--clipboard-only", "--editor", "paint.exe"],
            vec!["--clipboard-only", "--editor-arg=--wait"],
            vec!["--editor-arg", "--wait"],
            vec!["--editor", ""],
            vec!["--window-hwnd", "12", "--process", "x.exe"],
            vec!["--window-hwnd", "12", "--window-class", "x"],
            vec!["--window-hwnd", "12", "--wait", "0"],
            vec!["--wait", "0"],
            vec!["--list-windows", "--wait", "0"],
            vec!["--window-title", "x", "--wait", "3600.1"],
            vec!["--window-title", "x", "--wait", "NaN"],
            vec!["--window-title", "x", "--wait", "inf"],
            vec!["--window-title", "x", "--wait=-1"],
            vec!["--process", " "],
            vec!["--window-class", ""],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("alphashot").chain(args)).is_err());
        }
        assert!(
            !Cli::try_parse_from(["alphashot"])
                .unwrap()
                .output
                .no_clipboard
        );
        assert!(
            Cli::try_parse_from(["alphashot", "--no-clipboard"])
                .unwrap()
                .output
                .no_clipboard
        );
        assert!(
            Cli::try_parse_from(["alphashot", "--clipboard-only"])
                .unwrap()
                .output
                .clipboard_only
        );
        let editor = Cli::try_parse_from([
            "alphashot",
            "--editor",
            "paint.exe",
            "--editor-arg",
            "--new-window",
            "--editor-arg=two words",
        ])
        .unwrap()
        .output;
        assert_eq!(editor.editor.as_deref(), Some(Path::new("paint.exe")));
        assert_eq!(editor.editor_arg, ["--new-window", "two words"]);
        #[cfg(windows)]
        {
            for executable in [
                "paint.bat",
                "paint.CMD",
                "paint.cmd.",
                "paint.BAT ",
                "paint.ps1",
            ] {
                assert!(Cli::try_parse_from(["alphashot", "--editor", executable]).is_err());
            }
            for executable in ["paint", "paint.EXE"] {
                assert!(Cli::try_parse_from(["alphashot", "--editor", executable]).is_ok());
            }
        }
        assert!(Cli::try_parse_from(["alphashot", "--window-hwnd", "0x123"]).is_ok());
        for selector in ["--window-title", "--process", "--window-class"] {
            for seconds in ["0", "0.5", "3600"] {
                assert!(
                    Cli::try_parse_from(["alphashot", selector, "x", "--wait", seconds]).is_ok()
                );
            }
        }
        let args = Cli::parse_from([
            "alphashot",
            "--list-windows",
            "--window-title",
            "Project",
            "--process",
            "code.exe",
            "--window-class",
            "Chrome_WidgetWin_1",
            "--wait",
            "1",
        ]);
        let filter = args.window_filter();
        assert_eq!(filter.title.as_deref(), Some("Project"));
        assert_eq!(filter.process.as_deref(), Some("code.exe"));
        assert_eq!(filter.class_name.as_deref(), Some("Chrome_WidgetWin_1"));
        assert!(
            presets::validate_dpi(
                &Cli::try_parse_from(["alphashot", "--high-dpi-display", "display2"]).unwrap()
            )
            .is_err()
        );
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

    #[test]
    fn client_size_is_optional_and_parses_bounded_physical_dimensions() {
        assert_eq!(
            Cli::try_parse_from(["alphashot"]).unwrap().client_size,
            None
        );
        for (text, expected) in [
            ("1x1", (1, 1)),
            ("1024X768", (1024, 768)),
            ("8000x5000", (8000, 5000)),
            ("40000000x1", (40_000_000, 1)),
        ] {
            assert_eq!(
                Cli::try_parse_from(["alphashot", "--client-size", text])
                    .unwrap()
                    .client_size,
                Some(expected)
            );
        }
        for text in [
            "",
            "800",
            "800x",
            "x600",
            "0x600",
            "800x0",
            "8000x5001",
            "4294967295x1",
            "-1x600",
            "800x600x1",
            "NaNx600",
            "800 x600",
        ] {
            assert!(
                Cli::try_parse_from(["alphashot", "--client-size", text]).is_err(),
                "{text:?}"
            );
        }
    }
}
