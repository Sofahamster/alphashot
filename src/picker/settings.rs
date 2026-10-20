//! Capture settings editor shown over the picker (F4, Tab, or a click on a settings field).
//!
//! The editor changes output format, focus look, cursor, alpha recovery, and DPI scale without
//! restarting. It edits a `draft` copy of the parsed `Cli`. `Picker::key` copies the draft over the
//! real arguments only when the editor returns `Edit::Apply` (Enter) and drops it on Esc. Nothing
//! is persisted.
//!
//! `Field::Format` cycles PNG, WebP, and TIFF and rewrites the output file extension to match.
//! `Field::Scale` cycles Window DPI, automatic high DPI, and each active display above 100%
//! scaling. The display list is queried on first use of the Scale field and cached until F5. Enter
//! with high DPI selected is refused unless a matching display above 100% is active, because the
//! capture would fail later.

use alphashot::capture::{self, FocusState, MonitorInfo};
use crossterm::event::{KeyCode, KeyEvent};

use crate::{Cli, OutputFormat};

/// One editable row. The discriminant order is the on-screen order and indexes `Field::ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Format,
    Look,
    Cursor,
    Alpha,
    Scale,
}

impl Field {
    pub(super) const ALL: [Self; 5] = [
        Self::Format,
        Self::Look,
        Self::Cursor,
        Self::Alpha,
        Self::Scale,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Format => "Format",
            Self::Look => "Look",
            Self::Cursor => "Cursor",
            Self::Alpha => "Alpha",
            Self::Scale => "Scale",
        }
    }

    pub(super) fn value(self, args: &Cli) -> String {
        match self {
            Self::Format => match output_format(args) {
                Some(OutputFormat::Png) => "PNG · lossless",
                Some(OutputFormat::Webp) => "WebP · lossless",
                Some(OutputFormat::Tiff) => "TIFF · lossless",
                _ => "Unsupported format",
            }
            .into(),
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
            Self::Alpha => if args.transparent_client {
                "Full window"
            } else {
                "Corners + shadows"
            }
            .into(),
            Self::Scale if !args.high_dpi => "Window DPI".into(),
            Self::Scale => args
                .high_dpi_display
                .as_ref()
                .map(|name| format!("High DPI · {}", name.rsplit('\\').next().unwrap_or(name)))
                .unwrap_or_else(|| "High DPI (auto)".into()),
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
    displays: Option<Vec<MonitorInfo>>,
    pub(super) message: Option<String>,
}

impl Editor {
    pub(super) fn new(args: &Cli, selected: Field) -> Self {
        Self {
            draft: args.clone(),
            selected,
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
                if let Some(path) = &mut self.draft.output.out {
                    path.set_extension(extension);
                }
            }
            Field::Look => {
                self.draft.focus_state = if self.draft.focus_state == FocusState::Focused {
                    FocusState::Unfocused
                } else {
                    FocusState::Focused
                }
            }
            Field::Cursor => self.draft.include_cursor = !self.draft.include_cursor,
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
                if !self.draft.high_dpi || self.scale_ready() {
                    return Edit::Apply;
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = Field::ALL[(self.selected as isize - 1).rem_euclid(5) as usize];
                self.message = None;
            }
            KeyCode::Down | KeyCode::Tab => {
                self.selected = Field::ALL[(self.selected as usize + 1) % 5];
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
                let hint = "All formats are lossless and preserve alpha. PNG/TIFF keep DPI. Changing format also updates the filename extension.";
                self.draft.output.out.as_ref().map_or_else(|| hint.into(), |path| format!("{hint} File: {}", path.display()))
            }
            Field::Look => "Choose whether the window appears active or inactive.".into(),
            Field::Cursor => "Include or omit the mouse pointer in the saved image.".into(),
            Field::Alpha => "Corners + shadows keeps app content opaque and tolerates small animations. Full window recovers client transparency too, and needs static content.".into(),
            Field::Scale => "Choose Window DPI, automatic high DPI, or a specific display. High DPI needs an active extended display above 100% scale. F5 refreshes displays after Windows settings change.".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
