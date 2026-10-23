//! Named capture defaults, stored as strict INI sections and replaced atomically on save.

use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use alphashot::capture::{CaptureBackend, FocusState, PadMode};
use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;

use crate::{Cli, OutputFormat, TiffAlpha};

#[derive(Clone, Debug)]
pub(crate) struct Preset {
    out_dir: Option<PathBuf>,
    out_format: OutputFormat,
    tiff_alpha: TiffAlpha,
    include_cursor: bool,
    focus_state: FocusState,
    backend: CaptureBackend,
    no_shadow: bool,
    margin: u32,
    pad_mode: PadMode,
    pad: u32,
    high_dpi: bool,
    high_dpi_display: Option<String>,
}

impl Default for Preset {
    fn default() -> Self {
        Self {
            out_dir: None,
            out_format: OutputFormat::Auto,
            tiff_alpha: TiffAlpha::Straight,
            include_cursor: false,
            focus_state: FocusState::Focused,
            backend: CaptureBackend::Backdrop,
            no_shadow: false,
            margin: 0,
            pad_mode: PadMode::Shadow,
            pad: 4,
            high_dpi: false,
            high_dpi_display: None,
        }
    }
}

pub(crate) fn name(value: &str) -> std::result::Result<String, String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        Err("Preset names must be 1..64 ASCII letters, digits, hyphens or underscores".into())
    } else {
        Ok(value.to_owned())
    }
}

fn file_path(args: &Cli) -> Result<PathBuf> {
    if let Some(path) = &args.presets_file {
        return Ok(path.clone());
    }
    let appdata = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .context("APPDATA is unavailable; choose a preset file with --presets-file PATH")?;
    Ok(PathBuf::from(appdata).join("alphashot/presets.ini"))
}

pub(crate) fn load(args: &Cli) -> Result<BTreeMap<String, Preset>> {
    let path = file_path(args)?;
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text).with_context(|| format!("Invalid preset file {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn parse(text: &str) -> Result<BTreeMap<String, Preset>> {
    let mut presets = BTreeMap::new();
    let mut current = None;
    let mut keys = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = name(section).map_err(anyhow::Error::msg)?;
            ensure!(
                !presets.contains_key(&name),
                "Duplicate preset [{name}] at line {}",
                index + 1
            );
            presets.insert(name.clone(), Preset::default());
            current = Some(name);
            keys.clear();
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("Expected key=value at line {}", index + 1))?;
        let (key, value) = (key.trim(), value.trim());
        let current = current
            .as_ref()
            .with_context(|| format!("Expected a [preset-name] before line {}", index + 1))?;
        ensure!(
            keys.insert(key),
            "Duplicate key {key} in preset [{current}] at line {}",
            index + 1
        );
        let preset = presets.get_mut(current).unwrap();
        let field = || format!("Invalid {key} in preset [{current}] at line {}", index + 1);
        ensure!(
            !value.chars().any(char::is_control),
            "{}: control characters are not allowed",
            field()
        );
        match key {
            "out-dir" => preset.out_dir = (!value.is_empty()).then(|| PathBuf::from(value)),
            "out-format" => {
                preset.out_format = OutputFormat::from_str(value, false)
                    .map_err(anyhow::Error::msg)
                    .with_context(field)?;
            }
            "tiff-alpha" => {
                preset.tiff_alpha = TiffAlpha::from_str(value, false)
                    .map_err(anyhow::Error::msg)
                    .with_context(field)?;
            }
            "include-cursor" => preset.include_cursor = value.parse().with_context(field)?,
            "backend" => {
                preset.backend = CaptureBackend::from_str(value, false)
                    .map_err(anyhow::Error::msg)
                    .with_context(field)?;
            }
            "no-shadow" => preset.no_shadow = value.parse().with_context(field)?,
            "focus-state" => {
                preset.focus_state = FocusState::from_str(value, false)
                    .map_err(anyhow::Error::msg)
                    .with_context(field)?;
            }
            "margin" => preset.margin = value.parse().with_context(field)?,
            "pad-mode" => {
                preset.pad_mode = PadMode::from_str(value, false)
                    .map_err(anyhow::Error::msg)
                    .with_context(field)?;
            }
            "pad" => {
                preset.pad = value.parse().with_context(field)?;
                ensure!(preset.pad <= 4096, "{}: expected 0..4096", field());
            }
            "high-dpi" => preset.high_dpi = value.parse().with_context(field)?,
            "high-dpi-display" => {
                preset.high_dpi_display = (!value.is_empty()).then(|| value.to_owned());
            }
            _ => bail!("Unknown preset key {key} at line {}", index + 1),
        }
    }
    for (name, preset) in &presets {
        if !matches!(preset.out_format, OutputFormat::Auto) {
            preset
                .tiff_alpha
                .validate(preset.out_format)
                .with_context(|| format!("Preset [{name}]"))?;
        }
        ensure!(
            preset.high_dpi || preset.high_dpi_display.is_none(),
            "Preset [{name}]: high-dpi-display requires high-dpi=true"
        );
    }
    Ok(presets)
}

pub(crate) fn validate_dpi(args: &Cli) -> Result<()> {
    ensure!(
        args.high_dpi || args.high_dpi_display.is_none(),
        "--high-dpi-display requires --high-dpi or a preset with high-dpi=true"
    );
    Ok(())
}

pub(crate) fn apply_named(
    args: &mut Cli,
    presets: &BTreeMap<String, Preset>,
    name: &str,
) -> Result<()> {
    let preset = presets
        .get(name)
        .with_context(|| format!("No preset named {name:?}; use --list-presets"))?;
    let mut draft = args.clone();
    let inherited = |field: &str| !args.preset_overrides.iter().any(|id| id == field);
    if inherited("out_dir") {
        draft.output.out_dir = if args.output.clipboard_only {
            None
        } else {
            preset.out_dir.clone()
        };
    }
    if inherited("out_format") {
        draft.output.out_format = preset.out_format;
    }
    if inherited("tiff_alpha") {
        draft.output.tiff_alpha = if args.output.clipboard_only {
            TiffAlpha::Straight
        } else {
            preset.tiff_alpha
        };
    }
    if inherited("include_cursor") {
        draft.include_cursor = preset.include_cursor;
    }
    if inherited("focus_state") {
        draft.focus_state = preset.focus_state;
    }
    if inherited("backend") {
        draft.backend = preset.backend;
    }
    if inherited("no_shadow") {
        draft.no_shadow = preset.no_shadow;
    }
    if inherited("margin") {
        draft.alpha.margin = preset.margin;
    }
    if inherited("pad_mode") {
        draft.pad_mode = preset.pad_mode;
    }
    if inherited("pad") {
        draft.pad = preset.pad;
    }
    if inherited("high_dpi") {
        draft.high_dpi = preset.high_dpi;
    }
    if inherited("high_dpi_display") {
        draft.high_dpi_display = if draft.high_dpi {
            preset.high_dpi_display.clone()
        } else {
            None
        };
    }
    validate_dpi(&draft)?;
    draft.preset = Some(name.to_owned());
    *args = draft;
    Ok(())
}

fn enum_name(value: impl ValueEnum) -> String {
    value.to_possible_value().unwrap().get_name().to_owned()
}

fn scalar(value: &str) -> Result<&str> {
    ensure!(
        value.trim() == value && !value.chars().any(char::is_control),
        "Preset values cannot contain control characters or surrounding whitespace"
    );
    Ok(value)
}

pub(crate) fn save(args: &Cli, preset_name: &str) -> Result<()> {
    name(preset_name).map_err(anyhow::Error::msg)?;
    validate_dpi(args)?;
    let mut presets = load(args)?;
    presets.insert(
        preset_name.to_owned(),
        Preset {
            out_dir: args.output.out_dir.clone(),
            out_format: args.output.out_format,
            tiff_alpha: args.output.tiff_alpha,
            include_cursor: args.include_cursor,
            focus_state: args.focus_state,
            backend: args.backend,
            no_shadow: args.no_shadow,
            margin: args.alpha.margin,
            pad_mode: args.pad_mode,
            pad: args.pad,
            high_dpi: args.high_dpi,
            high_dpi_display: args.high_dpi_display.clone(),
        },
    );
    let mut text = String::new();
    for (name, preset) in &presets {
        let directory = preset
            .out_dir
            .as_deref()
            .unwrap_or(Path::new(""))
            .to_str()
            .context("Preset output folders must be valid Unicode")?;
        writeln!(text, "[{name}]")?;
        writeln!(text, "out-dir={}", scalar(directory)?)?;
        writeln!(text, "out-format={}", enum_name(preset.out_format))?;
        writeln!(text, "tiff-alpha={}", enum_name(preset.tiff_alpha))?;
        writeln!(text, "include-cursor={}", preset.include_cursor)?;
        writeln!(text, "focus-state={}", enum_name(preset.focus_state))?;
        writeln!(text, "backend={}", enum_name(preset.backend))?;
        writeln!(text, "no-shadow={}", preset.no_shadow)?;
        writeln!(text, "margin={}", preset.margin)?;
        writeln!(text, "pad-mode={}", enum_name(preset.pad_mode))?;
        writeln!(text, "pad={}", preset.pad)?;
        writeln!(text, "high-dpi={}", preset.high_dpi)?;
        writeln!(
            text,
            "high-dpi-display={}\n",
            scalar(preset.high_dpi_display.as_deref().unwrap_or(""))?
        )?;
    }
    parse(&text)?;
    let path = file_path(args)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(text.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("Cannot save presets to {}", path.display()))?;
    eprintln!("Saved preset {preset_name:?} to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn cli(args: &[&str]) -> Cli {
        let matches = Cli::command()
            .try_get_matches_from(std::iter::once("alphashot").chain(args.iter().copied()))
            .unwrap();
        Cli::with_sources(&matches).unwrap()
    }

    fn sample() -> BTreeMap<String, Preset> {
        parse(
            "[work]\nout-dir=saved\nout-format=tiff\ntiff-alpha=premultiplied\ninclude-cursor=true\n\
             focus-state=unfocused\nbackend=wgc\nno-shadow=true\nmargin=18\npad-mode=uniform\npad=27\n\
             high-dpi=true\nhigh-dpi-display=DISPLAY2\n",
        )
        .unwrap()
    }

    #[test]
    fn each_field_is_inherited_and_explicit_cli_defaults_still_win() {
        let presets = sample();
        let mut args = cli(&[]);
        apply_named(&mut args, &presets, "work").unwrap();
        assert_eq!(args.output.out_dir.as_deref(), Some(Path::new("saved")));
        assert!(matches!(args.output.out_format, OutputFormat::Tiff));
        assert_eq!(args.output.tiff_alpha, TiffAlpha::Premultiplied);
        assert!(args.include_cursor);
        assert_eq!(args.focus_state, FocusState::Unfocused);
        assert_eq!(args.backend, CaptureBackend::Wgc);
        assert!(args.no_shadow);
        assert_eq!(args.alpha.margin, 18);
        assert_eq!(args.pad_mode, PadMode::Uniform);
        assert_eq!(args.pad, 27);
        assert!(args.high_dpi);
        assert_eq!(args.high_dpi_display.as_deref(), Some("DISPLAY2"));

        let mut args = cli(&[
            "--out-dir",
            "cli",
            "--out-format",
            "auto",
            "--tiff-alpha",
            "straight",
            "--include-cursor=false",
            "--backend",
            "backdrop",
            "--no-shadow=false",
            "--focus-state",
            "focused",
            "--margin",
            "0",
            "--pad-mode",
            "shadow",
            "--pad",
            "4",
            "--high-dpi=false",
        ]);
        apply_named(&mut args, &presets, "work").unwrap();
        assert_eq!(args.output.out_dir.as_deref(), Some(Path::new("cli")));
        assert!(matches!(args.output.out_format, OutputFormat::Auto));
        assert_eq!(args.output.tiff_alpha, TiffAlpha::Straight);
        assert!(!args.include_cursor);
        assert_eq!(args.focus_state, FocusState::Focused);
        assert_eq!(args.backend, CaptureBackend::Backdrop);
        assert!(!args.no_shadow);
        assert_eq!(args.alpha.margin, 0);
        assert_eq!(args.pad_mode, PadMode::Shadow);
        assert_eq!(args.pad, 4);
        assert!(!args.high_dpi);
        assert!(args.high_dpi_display.is_none());
    }

    #[test]
    fn dpi_display_override_can_use_the_presets_prerequisite() {
        let presets = sample();
        let mut args = cli(&["--high-dpi-display", "DISPLAY3"]);
        apply_named(&mut args, &presets, "work").unwrap();
        assert!(args.high_dpi);
        assert_eq!(args.high_dpi_display.as_deref(), Some("DISPLAY3"));
        assert!(validate_dpi(&cli(&["--high-dpi-display", "DISPLAY3"])).is_err());
        let mut args = cli(&["--high-dpi=false", "--high-dpi-display", "DISPLAY3"]);
        assert!(apply_named(&mut args, &presets, "work").is_err());
        assert!(
            args.output.out_dir.is_none(),
            "Failed application must be atomic"
        );
        assert!(args.preset.is_none());
    }

    #[test]
    fn explicit_output_wins_and_clipboard_only_ignores_the_saved_folder() {
        let presets = sample();
        let mut args = cli(&["--out", "explicit.png"]);
        apply_named(&mut args, &presets, "work").unwrap();
        assert_eq!(args.output.path_for("Title"), Path::new("explicit.png"));
        let mut args = cli(&[]);
        apply_named(&mut args, &presets, "work").unwrap();
        let path = args.output.path_for("Title");
        assert_eq!(path.parent(), Some(Path::new("saved")));
        assert_eq!(path.extension().unwrap(), "tiff");
        let mut args = cli(&["--clipboard-only"]);
        apply_named(&mut args, &presets, "work").unwrap();
        assert!(args.output.clipboard_only && args.output.out_dir.is_none());
        assert_eq!(args.output.tiff_alpha, TiffAlpha::Straight);
        assert!(args.include_cursor);
    }

    #[test]
    fn parsing_is_strict_about_sections_keys_types_ranges_and_dpi() {
        for text in [
            "[bad name]",
            "[]",
            "[bad]suffix",
            "margin=1",
            "[x]\n[x]",
            "[x]\nmargin=1\nmargin=2",
            "[x]\nwindow-hwnd=1",
            "[x]\neditor=paint.exe",
            "[x]\nout-format=jpeg",
            "[x]\ntiff-alpha=associated",
            "[x]\ntiff-alpha=straight\ntiff-alpha=premultiplied",
            "[x]\nout-format=png\ntiff-alpha=premultiplied",
            "[x]\nout-format=webp\ntiff-alpha=premultiplied",
            "[x]\ninclude-cursor=1",
            "[x]\nhigh-dpi=TRUE",
            "[x]\nfocus-state=active",
            "[x]\nbackend=unknown",
            "[x]\nbackend=WGC",
            "[x]\nbackend=wgc\nbackend=backdrop",
            "[x]\nno-shadow=1",
            "[x]\nno-shadow=true\nno-shadow=false",
            "[x]\nmargin=-1",
            "[x]\nmargin=4294967296",
            "[x]\npad-mode=calibrated",
            "[x]\npad=4097",
            "[x]\npad=-1",
            "[x]\nhigh-dpi-display=DISPLAY2",
            "[x]\nout-dir=foo\tbar",
        ] {
            assert!(parse(text).is_err(), "Accepted {text:?}");
        }
        assert!(name(&"a".repeat(65)).is_err());
        assert!(name("../elsewhere").is_err());
        assert!(name("Work_2026-10").is_ok());
        let parsed =
            parse("; comment\r\n[x]\r\npad=4096\r\nmargin=4294967295\r\n# comment\n").unwrap();
        assert_eq!(parsed["x"].pad, 4096);
        assert_eq!(parsed["x"].margin, u32::MAX);
        assert_eq!(parsed["x"].backend, CaptureBackend::Backdrop);
        assert!(!parsed["x"].no_shadow);
        assert_eq!(parsed["x"].tiff_alpha, TiffAlpha::Straight);
        let automatic = parse("[x]\nout-format=auto\ntiff-alpha=premultiplied").unwrap();
        assert_eq!(automatic["x"].tiff_alpha, TiffAlpha::Premultiplied);
    }

    #[test]
    fn loading_missing_files_creates_nothing_and_unknown_names_fail() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("not-created");
        let mut args = cli(&[]);
        args.presets_file = Some(parent.join("presets.ini"));
        assert!(load(&args).unwrap().is_empty());
        assert!(!parent.exists());
        assert!(apply_named(&mut args, &BTreeMap::new(), "missing").is_err());
        assert!(!parent.exists());
    }

    #[test]
    fn saving_round_trips_only_requested_settings_and_preserves_other_presets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config/presets.ini");
        let mut args = cli(&[
            "--out-dir",
            "Büro = captures",
            "--out-format",
            "tiff",
            "--tiff-alpha",
            "premultiplied",
            "--include-cursor",
            "--backend",
            "wgc",
            "--no-shadow",
            "--focus-state",
            "unfocused",
            "--margin",
            "11",
            "--pad-mode",
            "uniform",
            "--pad",
            "16",
            "--high-dpi",
            "--high-dpi-display",
            "DISPLAY2",
            "--out",
            "ignored.png",
            "--editor",
            "paint.exe",
            "--editor-arg=ignored",
            "--window-hwnd",
            "1",
            "--no-clipboard",
            "--no-preview",
            "--transparent-client",
        ]);
        args.presets_file = Some(path.clone());
        save(&args, "work").unwrap();
        args.alpha.margin = 7;
        save(&args, "other").unwrap();
        args.alpha.margin = 3;
        save(&args, "work").unwrap();
        let presets = load(&args).unwrap();
        assert_eq!(presets.len(), 2);
        assert_eq!(presets["other"].margin, 7);
        assert_eq!(presets["work"].margin, 3);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().filter(|line| line.contains('=')).count(), 24);
        for unwanted in [
            "ignored",
            "editor",
            "window-hwnd",
            "clipboard",
            "transparent-client",
        ] {
            assert!(!text.contains(unwanted));
        }
        let mut applied = cli(&[]);
        apply_named(&mut applied, &presets, "work").unwrap();
        assert_eq!(applied.output.out_dir, args.output.out_dir);
        assert!(matches!(applied.output.out_format, OutputFormat::Tiff));
        assert_eq!(applied.output.tiff_alpha, TiffAlpha::Premultiplied);
        assert!(applied.include_cursor && applied.high_dpi);
        assert_eq!(applied.focus_state, args.focus_state);
        assert_eq!(applied.backend, args.backend);
        assert_eq!(applied.no_shadow, args.no_shadow);
        assert_eq!(applied.pad_mode, args.pad_mode);
        assert_eq!(applied.pad, args.pad);
        assert_eq!(applied.high_dpi_display, args.high_dpi_display);
        assert!(applied.output.out.is_none() && applied.output.editor.is_none());
        assert!(!applied.output.no_clipboard && !applied.transparent_client);
    }

    #[test]
    fn invalid_saves_preserve_existing_bytes_and_leave_no_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presets.ini");
        let mut args = cli(&[]);
        args.presets_file = Some(path.clone());
        fs::write(&path, "[existing]\nmargin=12\n").unwrap();
        let original = fs::read(&path).unwrap();
        assert!(save(&args, "bad name").is_err());
        args.output.out_dir = Some("newline\ninjection".into());
        assert!(save(&args, "new").is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        args.output.out_dir = None;
        args.output.out_format = OutputFormat::Webp;
        args.output.tiff_alpha = TiffAlpha::Premultiplied;
        let error = save(&args, "new").unwrap_err();
        assert!(format!("{error:#}").contains("requires TIFF"));
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::write(&path, "[existing]\nunknown=true\n").unwrap();
        let original = fs::read(&path).unwrap();
        args.output.out_dir = None;
        assert!(save(&args, "new").is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn failed_atomic_replace_preserves_the_old_file_and_cleans_the_temporary_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presets.ini");
        let mut args = cli(&[]);
        args.presets_file = Some(path.clone());
        fs::write(&path, "[existing]\nmargin=12\n").unwrap();
        let original = fs::read(&path).unwrap();
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(save(&args, "new").is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
