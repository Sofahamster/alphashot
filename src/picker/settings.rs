//! Capture settings editor shown over the picker (F4, Tab, or a click on a settings field).
//!
//! The editor changes output format, focus look, cursor, alpha recovery, and DPI scale without
//! restarting. It edits a `draft` copy of the parsed `Cli`. `Picker::key` copies the draft over the
//! real arguments only when the editor returns `Edit::Apply` (Enter) and drops it on Esc.
//! The Preset row reads saved defaults; edits last for this invocation.
//!
//! `Field::Format` cycles PNG, WebP, and TIFF and rewrites the output file extension to match.
//! `Field::Scale` cycles Window DPI, automatic high DPI, and each active display above 100%
//! scaling. The display list is queried on first use of the Scale field and cached until F5. Enter
//! with high DPI selected is refused unless a matching display above 100% is active, because the
//! capture would fail later.

use alphashot::capture::{self, CaptureBackend, FocusState, MonitorInfo};
use crossterm::event::{KeyCode, KeyEvent};

use crate::{Cli, OutputFormat, TiffAlpha, presets};

/// One editable row. The discriminant order is the on-screen order and indexes `Field::ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Format,
    Look,
    Cursor,
    Alpha,
    Scale,
    Preset,
    Backend,
    Shadow,
    TiffAlpha,
}

impl Field {
    pub(super) const ALL: [Self; 9] = [
        Self::Format,
        Self::Look,
        Self::Cursor,
        Self::Alpha,
        Self::Scale,
        Self::Preset,
        Self::Backend,
        Self::Shadow,
        Self::TiffAlpha,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Format => "Format",
            Self::Look => "Look",
            Self::Cursor => "Cursor",
            Self::Alpha => "Alpha",
            Self::Scale => "Scale",
            Self::Preset => "Preset",
            Self::Backend => "Backend",
            Self::Shadow => "Shadow",
            Self::TiffAlpha => "TIFF alpha",
        }
    }

    pub(super) fn value(self, args: &Cli) -> String {
        match self {
            Self::Format => match output_format(args) {
                Some(OutputFormat::Png) => "PNG · lossless",
                Some(OutputFormat::Webp) => "WebP · lossless",
                Some(OutputFormat::Tiff) if args.output.tiff_alpha == TiffAlpha::Premultiplied => {
                    "TIFF"
                }
                Some(OutputFormat::Tiff) => "TIFF · lossless",
                _ => "Unsupported format",
            }
            .into(),
            Self::Look if args.backend == CaptureBackend::Wgc => "Current (WGC)".into(),
            Self::Look => if args.focus_state == FocusState::Focused {
                "Focused"
            } else {
                "Unfocused"
            }
            .into(),
            Self::Cursor => if args.include_cursor {
                "Included"
            } else {
                "Excluded"
            }
            .into(),
            Self::Alpha if args.backend == CaptureBackend::Wgc => "Native alpha (WGC)".into(),
            Self::Alpha if args.no_shadow => "Full window (no shadow)".into(),
            Self::Alpha => if args.transparent_client {
                "Full window"
            } else {
                "Corners + shadows"
            }
            .into(),
            Self::Scale if args.backend == CaptureBackend::Wgc && !args.high_dpi => {
                "Current DPI (WGC)".into()
            }
            Self::Scale if !args.high_dpi => "Window DPI".into(),
            Self::Scale => args
                .high_dpi_display
                .as_ref()
                .map(|name| format!("High DPI · {}", name.rsplit('\\').next().unwrap_or(name)))
                .unwrap_or_else(|| "High DPI (auto)".into()),
            Self::Preset => args.preset.clone().unwrap_or_else(|| "None".into()),
            Self::Backend => if args.backend == CaptureBackend::Backdrop {
                "Backdrop"
            } else {
                "WGC (no activation)"
            }
            .into(),
            Self::Shadow if args.backend == CaptureBackend::Wgc => if args.no_shadow {
                "Excluded (soft corners)"
            } else {
                "Outer shadow omitted"
            }
            .into(),
            Self::Shadow => if args.no_shadow {
                "Excluded"
            } else {
                "Included"
            }
            .into(),
            Self::TiffAlpha if args.output.clipboard_only => "Clipboard only".into(),
            Self::TiffAlpha if !matches!(output_format(args), Some(OutputFormat::Tiff)) => {
                "TIFF only".into()
            }
            Self::TiffAlpha => if args.output.tiff_alpha == TiffAlpha::Straight {
                "Straight"
            } else {
                "Premultiplied"
            }
            .into(),
        }
    }
}

/// Resolves the effective output format from the draft's format option and file name,
/// using the default name `window.png` when no path was given. `None` means the combination
/// is unsupported.
fn output_format(args: &Cli) -> Option<OutputFormat> {
    args.output
        .out_format
        .resolve(
            args.output
                .out
                .as_deref()
                .unwrap_or(std::path::Path::new("window.png")),
        )
        .ok()
}

/// What the picker does after a key reaches the editor.
pub(super) enum Edit {
    Continue,
    Apply,
    Cancel,
}

/// Editor state: a draft of the arguments being edited, the highlighted field, a cached
/// display list, and a message that replaces the field description (errors, hints).
pub(super) struct Editor {
    pub(super) draft: Cli,
    pub(super) selected: Field,
    pub(super) offset: usize,
    displays: Option<Vec<MonitorInfo>>,
    pub(super) message: Option<String>,
}

impl Editor {
    pub(super) fn new(args: &Cli, selected: Field) -> Self {
        Self {
            draft: args.clone(),
            selected,
            offset: 0,
            displays: None,
            message: None,
        }
    }

    /// Re-reads the monitor list. Returns false and sets `message` if enumeration fails.
    fn refresh(&mut self) -> bool {
        match capture::list_monitors() {
            Ok(displays) => {
                self.displays = Some(displays);
                self.message = None;
                true
            }
            Err(error) => {
                self.message = Some(format!(
                    "Could not read displays: {error}. Press F5 to retry."
                ));
                false
            }
        }
    }

    /// Returns whether a display that can satisfy the high-DPI setting is active: above 96 DPI
    /// and, if one was chosen, matching the chosen display name. Sets an explanatory message when
    /// not.
    fn scale_ready(&mut self) -> bool {
        if self.displays.is_none() && !self.refresh() {
            return false;
        }
        let ready = self.displays.as_ref().unwrap().iter().any(|m| {
            m.dpi > 96
                && self
                    .draft
                    .high_dpi_display
                    .as_ref()
                    .is_none_or(|name| name.eq_ignore_ascii_case(&m.name))
        });
        if !ready {
            self.message = Some("No matching display above 100% is active. Enable an extended display in Windows Settings and increase its scale (e.g. 200%). Press F5 to refresh. Setup: https://github.com/VirtualDrivers/Virtual-Display-Driver".into());
        }
        ready
    }

    /// Moves `field` by `step` (wrapping) and selects it. A step of 0 only selects the field.
    /// Scale cycles Window DPI, automatic high DPI, then each qualifying display; with no
    /// qualifying display it stays on Window DPI and sets an explanatory message.
    pub(super) fn change(&mut self, field: Field, step: isize) {
        self.selected = field;
        self.message = None;
        if step == 0 {
            return;
        }
        match field {
            Field::Backend => {
                self.draft.backend = if self.draft.backend == CaptureBackend::Backdrop {
                    CaptureBackend::Wgc
                } else {
                    CaptureBackend::Backdrop
                };
            }
            Field::Shadow => self.draft.no_shadow = !self.draft.no_shadow,
            Field::TiffAlpha
                if self.draft.output.clipboard_only
                    || !matches!(output_format(&self.draft), Some(OutputFormat::Tiff)) =>
            {
                self.message = Some("TIFF alpha applies only to saved TIFF files.".into());
            }
            Field::TiffAlpha => {
                self.draft.output.tiff_alpha =
                    if self.draft.output.tiff_alpha == TiffAlpha::Straight {
                        TiffAlpha::Premultiplied
                    } else {
                        TiffAlpha::Straight
                    };
            }
            Field::Preset => {
                let result = (|| -> anyhow::Result<()> {
                    let presets = presets::load(&self.draft)?;
                    anyhow::ensure!(
                        !presets.is_empty(),
                        "No saved presets. Use --save-preset NAME first."
                    );
                    let names: Vec<_> = presets.keys().collect();
                    let current = self
                        .draft
                        .preset
                        .as_ref()
                        .and_then(|name| names.iter().position(|candidate| *candidate == name))
                        .map_or(if step > 0 { -1 } else { 0 }, |index| index as isize);
                    let next = (current + step).rem_euclid(names.len() as isize) as usize;
                    presets::apply_named(&mut self.draft, &presets, names[next])
                })();
                if let Err(error) = result {
                    self.message = Some(format!("Preset: {}", error.root_cause()));
                }
            }
            Field::Format => {
                let current = match output_format(&self.draft) {
                    Some(OutputFormat::Png) => 0,
                    Some(OutputFormat::Webp) => 1,
                    Some(OutputFormat::Tiff) => 2,
                    _ => -step,
                };
                let (format, extension) = [
                    (OutputFormat::Png, "png"),
                    (OutputFormat::Webp, "webp"),
                    (OutputFormat::Tiff, "tiff"),
                ][(current + step).rem_euclid(3) as usize];
                self.draft.output.out_format = format;
                if !matches!(format, OutputFormat::Tiff) {
                    self.draft.output.tiff_alpha = TiffAlpha::Straight;
                }
                if let Some(path) = &mut self.draft.output.out {
                    path.set_extension(extension);
                }
            }
            Field::Look if self.draft.backend == CaptureBackend::Wgc => {
                self.message = Some("WGC keeps current appearance; no activation.".into());
            }
            Field::Look => {
                self.draft.focus_state = if self.draft.focus_state == FocusState::Focused {
                    FocusState::Unfocused
                } else {
                    FocusState::Focused
                }
            }
            Field::Cursor => self.draft.include_cursor = !self.draft.include_cursor,
            Field::Alpha if self.draft.backend == CaptureBackend::Wgc => {
                self.message = Some("WGC keeps native alpha; no backdrop alpha solve.".into());
            }
            Field::Alpha if self.draft.no_shadow => {
                self.message = Some("Without shadows, capture keeps full window alpha.".into());
            }
            Field::Alpha => self.draft.transparent_client = !self.draft.transparent_client,
            Field::Scale => {
                if self.displays.is_none() && !self.refresh() {
                    return;
                }
                let displays: Vec<_> = self
                    .displays
                    .as_ref()
                    .unwrap()
                    .iter()
                    .filter(|m| m.dpi > 96)
                    .collect();
                let current = if !self.draft.high_dpi {
                    0
                } else {
                    self.draft
                        .high_dpi_display
                        .as_ref()
                        .and_then(|name| {
                            displays
                                .iter()
                                .position(|m| m.name.eq_ignore_ascii_case(name))
                        })
                        .map_or(1, |i| i as isize + 2)
                };
                let next = (current + step).rem_euclid(displays.len() as isize + 2);
                if next != 0 && displays.is_empty() {
                    self.scale_ready();
                    return;
                }
                self.draft.high_dpi = next != 0;
                self.draft.high_dpi_display = if next >= 2 {
                    Some(displays[next as usize - 2].name.clone())
                } else {
                    None
                };
            }
        }
    }

    /// Handles one key. Enter applies unless high DPI is selected without a usable display.
    pub(super) fn key(&mut self, key: KeyEvent) -> Edit {
        match key.code {
            KeyCode::Esc => return Edit::Cancel,
            KeyCode::Enter => {
                if let Err(error) = self
                    .draft
                    .output
                    .tiff_alpha
                    .validate(output_format(&self.draft).unwrap_or(OutputFormat::Auto))
                {
                    self.message = Some(error.to_string());
                    return Edit::Continue;
                }
                if !self.draft.high_dpi || self.scale_ready() {
                    return Edit::Apply;
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = Field::ALL
                    [(self.selected as isize - 1).rem_euclid(Field::ALL.len() as isize) as usize];
                self.message = None;
            }
            KeyCode::Down | KeyCode::Tab => {
                self.selected = Field::ALL[(self.selected as usize + 1) % Field::ALL.len()];
                self.message = None;
            }
            KeyCode::Left => self.change(self.selected, -1),
            KeyCode::Right | KeyCode::Char(' ') => self.change(self.selected, 1),
            KeyCode::F(5) => {
                self.refresh();
            }
            _ => {}
        }
        Edit::Continue
    }

    /// Help text for the selected field, or the pending message if there is one.
    pub(super) fn description(&self) -> String {
        if let Some(message) = &self.message {
            return message.clone();
        }
        match self.selected {
            Field::Format => {
                let hint = "Formats use lossless compression and preserve alpha; premultiplied TIFF rounds 8-bit colors. PNG/TIFF keep DPI. Changing format also updates the filename extension.";
                self.draft.output.out.as_ref().map_or_else(|| hint.into(), |path| format!("{hint} File: {}", path.display()))
            }
            Field::Look if self.draft.backend == CaptureBackend::Wgc => "WGC keeps the window's current appearance without activating it. Choose Backdrop to request a focused or unfocused look.".into(),
            Field::Look => "Choose whether the window appears active or inactive.".into(),
            Field::Cursor => "Include or omit the mouse pointer in the saved image.".into(),
            Field::Alpha if self.draft.backend == CaptureBackend::Wgc => "WGC preserves the window's native alpha. The backdrop alpha-recovery choices do not apply; choose Backdrop to change them.".into(),
            Field::Alpha if self.draft.no_shadow => "Capture without shadows preserves full client and frame alpha. Opaque-client mode is unavailable until shadows are included again; its previous setting is kept.".into(),
            Field::Alpha => "Corners + shadows keeps app content opaque and tolerates small animations. Full window recovers client transparency too, and needs static content.".into(),
            Field::Scale if self.draft.backend == CaptureBackend::Wgc => "WGC uses the current DPI unless High DPI is selected. It temporarily moves the window only when needed, without activating it, then restores its placement. Exact client size may resize it temporarily. F5 refreshes displays.".into(),
            Field::Scale => "Choose Window DPI, automatic high DPI, or a specific display. High DPI needs an active extended display above 100% scale. F5 refreshes displays after Windows settings change.".into(),
            Field::Preset => "Choose saved capture defaults. Explicit command-line options keep their values. Save presets with --save-preset NAME; --list-presets lists them.".into(),
            Field::Backend => "Backdrop recovers transparency over white and black and can choose focus. WGC keeps the current appearance without activating the window.".into(),
            Field::Shadow if self.draft.backend == CaptureBackend::Wgc => "WGC omits the outer DWM shadow but can retain corner shadow. Excluding it uses an approximate one-pixel feather on standard DWM corners; custom chrome is unsupported.".into(),
            Field::Shadow => "Exclude the DWM shadow using frame bounds and an approximate one-pixel feather on standard rounded corners. Keeps the title bar and client alpha; custom chrome is unsupported.".into(),
            Field::TiffAlpha => "TIFF alone can declare straight or premultiplied alpha. Premultiplication rounds 8-bit colors and loses precision. PNG, WebP, previews and clipboard keep straight alpha internally. Choosing a non-TIFF format resets this to Straight.".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn shadow_free_alpha_and_wgc_scale_describe_effective_behavior() {
        for transparent_client in [false, true] {
            let mut args = Cli::parse_from(["alphashot", "--no-shadow"]);
            args.transparent_client = transparent_client;
            let mut editor = Editor::new(&args, Field::Alpha);
            assert_eq!(Field::Alpha.value(&editor.draft), "Full window (no shadow)");
            editor.change(Field::Alpha, 1);
            assert_eq!(editor.draft.transparent_client, transparent_client);
            assert!(editor.description().contains("full window alpha"));
            editor.change(Field::Shadow, 1);
            assert_eq!(
                Field::Alpha.value(&editor.draft),
                if transparent_client {
                    "Full window"
                } else {
                    "Corners + shadows"
                }
            );
            editor.change(Field::Alpha, 1);
            assert_ne!(editor.draft.transparent_client, transparent_client);
        }
        let args = Cli::parse_from(["alphashot", "--backend", "wgc"]);
        let mut editor = Editor::new(&args, Field::Scale);
        assert_eq!(Field::Scale.value(&editor.draft), "Current DPI (WGC)");
        assert!(editor.description().contains("only when needed"));
        assert!(editor.description().contains("without activating"));
        editor.draft.high_dpi = true;
        assert_eq!(Field::Scale.value(&editor.draft), "High DPI (auto)");
    }

    #[test]
    fn presets_cycle_with_cli_precedence_and_report_errors_without_partial_changes() {
        use clap::CommandFactory;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presets.ini");
        std::fs::write(
            &path,
            "[a]\ninclude-cursor=true\nfocus-state=unfocused\nbackend=wgc\nno-shadow=true\nout-format=tiff\ntiff-alpha=premultiplied\nmargin=11\n\
             [z]\ninclude-cursor=true\nmargin=22\n",
        )
        .unwrap();
        let matches = Cli::command()
            .try_get_matches_from([
                "alphashot",
                "--include-cursor=false",
                "--backend",
                "backdrop",
                "--no-shadow=false",
                "--tiff-alpha",
                "straight",
                "--focus-state",
                "focused",
            ])
            .unwrap();
        let mut args = Cli::with_sources(&matches).unwrap();
        args.presets_file = Some(path.clone());
        let mut editor = Editor::new(&args, Field::Preset);
        editor.change(Field::Preset, 1);
        assert_eq!(Field::Preset.value(&editor.draft), "a");
        assert!(!editor.draft.include_cursor);
        assert_eq!(editor.draft.focus_state, FocusState::Focused);
        assert_eq!(editor.draft.backend, CaptureBackend::Backdrop);
        assert!(!editor.draft.no_shadow);
        assert_eq!(editor.draft.output.tiff_alpha, TiffAlpha::Straight);
        assert_eq!(editor.draft.alpha.margin, 11);
        assert!(args.preset.is_none() && args.alpha.margin == 0);
        editor.change(Field::Preset, -1);
        assert_eq!(Field::Preset.value(&editor.draft), "z");
        assert_eq!(editor.draft.alpha.margin, 22);
        assert!(matches!(editor.key(KeyCode::Enter.into()), Edit::Apply));

        std::fs::write(&path, "[broken]\nunknown=true\n").unwrap();
        editor.change(Field::Preset, 1);
        assert!(editor.description().contains("Unknown preset key unknown"));
        assert_eq!(Field::Preset.value(&editor.draft), "z");
        assert_eq!(editor.draft.alpha.margin, 22);
        std::fs::remove_file(&path).unwrap();
        editor.change(Field::Preset, 1);
        assert!(editor.description().contains("--save-preset"));
        assert!(!path.exists());
        assert_eq!(editor.draft.alpha.margin, 22);
        editor.selected = Field::TiffAlpha;
        editor.key(KeyCode::Down.into());
        assert_eq!(editor.selected, Field::Format);
        editor.key(KeyCode::Up.into());
        assert_eq!(editor.selected, Field::TiffAlpha);
    }

    #[test]
    fn tiff_alpha_is_editable_only_for_saved_tiff_and_resets_with_format() {
        let args = Cli::parse_from(["alphashot", "--out", "window.tiff"]);
        let mut editor = Editor::new(&args, Field::TiffAlpha);
        assert_eq!(Field::Format.value(&editor.draft), "TIFF · lossless");
        editor.change(Field::TiffAlpha, 1);
        assert_eq!(Field::Format.value(&editor.draft), "TIFF");
        assert_eq!(editor.draft.output.tiff_alpha, TiffAlpha::Premultiplied);
        assert_eq!(Field::TiffAlpha.value(&editor.draft), "Premultiplied");
        assert_eq!(args.output.tiff_alpha, TiffAlpha::Straight);
        assert!(matches!(editor.key(KeyCode::Enter.into()), Edit::Apply));
        editor.change(Field::Format, 1);
        assert!(matches!(editor.draft.output.out_format, OutputFormat::Png));
        assert_eq!(editor.draft.output.tiff_alpha, TiffAlpha::Straight);
        editor.change(Field::TiffAlpha, 1);
        assert_eq!(editor.draft.output.tiff_alpha, TiffAlpha::Straight);
        assert!(editor.description().contains("saved TIFF"));
        let args = Cli::parse_from(["alphashot", "--clipboard-only", "--out-format", "tiff"]);
        let mut editor = Editor::new(&args, Field::TiffAlpha);
        editor.change(Field::TiffAlpha, 1);
        assert_eq!(editor.draft.output.tiff_alpha, TiffAlpha::Straight);
        assert_eq!(Field::TiffAlpha.value(&editor.draft), "Clipboard only");
        let args = Cli::parse_from(["alphashot", "--tiff-alpha", "premultiplied"]);
        let mut editor = Editor::new(&args, Field::TiffAlpha);
        assert!(matches!(editor.key(KeyCode::Enter.into()), Edit::Continue));
        assert!(editor.description().contains("requires TIFF"));
        editor.change(Field::Format, -1);
        assert!(matches!(editor.key(KeyCode::Enter.into()), Edit::Apply));
    }

    #[test]
    fn backend_and_shadow_controls_explain_wgc_without_changing_its_appearance() {
        let args = Cli::parse_from([
            "alphashot",
            "--backend",
            "wgc",
            "--focus-state",
            "unfocused",
        ]);
        let mut editor = Editor::new(&args, Field::Backend);
        assert_eq!(Field::Backend.value(&editor.draft), "WGC (no activation)");
        assert_eq!(Field::Look.value(&editor.draft), "Current (WGC)");
        assert_eq!(Field::Alpha.value(&editor.draft), "Native alpha (WGC)");
        assert_eq!(Field::Shadow.value(&editor.draft), "Outer shadow omitted");
        editor.change(Field::Look, 1);
        assert_eq!(editor.draft.focus_state, FocusState::Unfocused);
        assert!(editor.description().contains("no activation"));
        editor.change(Field::Alpha, 1);
        assert!(!editor.draft.transparent_client);
        assert!(editor.description().contains("native alpha"));
        editor.change(Field::Shadow, 1);
        assert!(editor.draft.no_shadow);
        assert_eq!(
            Field::Shadow.value(&editor.draft),
            "Excluded (soft corners)"
        );
        assert!(editor.description().contains("corner shadow"));
        editor.change(Field::Shadow, -1);
        assert!(!editor.draft.no_shadow);
        editor.change(Field::Backend, 1);
        assert_eq!(editor.draft.backend, CaptureBackend::Backdrop);
        assert_eq!(Field::Look.value(&editor.draft), "Unfocused");
        assert_eq!(Field::Shadow.value(&editor.draft), "Included");
        editor.change(Field::Shadow, 1);
        assert!(editor.draft.no_shadow);
        assert_eq!(Field::Shadow.value(&editor.draft), "Excluded");
        editor.change(Field::Look, 1);
        assert_eq!(editor.draft.focus_state, FocusState::Focused);
        assert_eq!(args.backend, CaptureBackend::Wgc);
        assert!(!args.no_shadow);
    }

    #[test]
    fn edits_keep_output_extensions_and_capture_flags_in_sync() {
        let args = Cli::parse_from(["alphashot", "-o", "window.png"]);
        let mut editor = Editor::new(&args, Field::Format);
        editor.change(Field::Format, 1);
        assert!(matches!(editor.draft.output.out_format, OutputFormat::Webp));
        assert_eq!(
            editor.draft.output.out.as_deref(),
            Some(std::path::Path::new("window.webp"))
        );
        editor.change(Field::Look, 1);
        editor.change(Field::Cursor, 1);
        editor.change(Field::Alpha, 1);
        assert_eq!(editor.draft.focus_state, FocusState::Unfocused);
        assert!(editor.draft.include_cursor && editor.draft.transparent_client);
        assert_eq!(
            args.output.out.as_deref(),
            Some(std::path::Path::new("window.png"))
        );
        assert!(matches!(editor.key(KeyCode::Enter.into()), Edit::Apply));
    }

    #[test]
    fn high_dpi_requires_scaled_displays_and_can_choose_a_device_or_turn_off() {
        let args = Cli::parse_from(["alphashot"]);
        let mut editor = Editor::new(&args, Field::Scale);
        editor.displays = Some(Vec::new());
        editor.change(Field::Scale, 1);
        assert!(!editor.draft.high_dpi);
        assert!(editor.description().contains("Windows Settings"));
        editor.displays = Some(vec![MonitorInfo {
            handle: 1,
            name: r"\\.\DISPLAY2".into(),
            description: "Virtual display".into(),
            rect: Default::default(),
            work: Default::default(),
            dpi: 192,
        }]);
        editor.change(Field::Scale, 1);
        assert!(editor.draft.high_dpi && editor.draft.high_dpi_display.is_none());
        editor.change(Field::Scale, 1);
        assert_eq!(
            editor.draft.high_dpi_display.as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        assert!(matches!(editor.key(KeyCode::Enter.into()), Edit::Apply));
        editor.change(Field::Scale, 1);
        assert!(!editor.draft.high_dpi && editor.draft.high_dpi_display.is_none());
        editor.draft.high_dpi = true;
        editor.displays = Some(Vec::new());
        assert!(matches!(editor.key(KeyCode::Enter.into()), Edit::Continue));
    }
}
