//! Interactive window picker. Lists top-level windows, filters them by query, shows a Sixel
//! thumbnail of the highlighted window, and returns a [`Selection`]. The settings editor in
//! `settings` adjusts capture options before the choice.
//!
//! The picker runs on the alternate screen of stderr and leaves stdout free for scripted output.
//! `pick` fails when stdin or stderr is not a terminal. Setting edits last for the run and are not
//! persisted. `draw` renders, `settings` edits options, and `thumbnail` captures previews in the
//! background.
//!
//! `Picker` holds the window list, the query, the ranked indices of matching windows, the selected
//! row, the scroll offset, and an optional settings `Editor`. While the editor is open it receives
//! all keys. F5 re-enumerates windows, reapplies the `--window-title` filter, and bumps the preview
//! epoch so cached snapshots are discarded. Esc and Ctrl+C cancel with an error. Mouse events are
//! translated by `draw::View::mouse` into key events or selection changes.
//!
//! The searched text is `"<process name> <title>"`, lowercased. Every whitespace-separated query
//! word must match somewhere in the text, in any order. A contiguous substring match ranks above a
//! subsequence match (see `fuzzy_score`).
//!
//! `Terminal::enter` enables raw mode, the alternate screen, and mouse capture, and hides the
//! cursor and disables line wrapping. `Drop` reverts all of it, including a pending synchronized
//! update, and ignores errors so the original error reaches the caller. The event loop polls every
//! 16 ms so finished thumbnails are drawn without a key press.

use std::io::{self, IsTerminal};

use alphashot::capture::{self, WindowInfo};
use anyhow::{Result, bail, ensure};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{
        self, DisableLineWrap, EnableLineWrap, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};

mod draw;
mod settings;
mod thumbnail;

/// RAII guard for the terminal modes the picker changes. Dropping it restores the terminal, so
/// every exit path from `pick` leaves the shell usable.
struct Terminal;

/// How the user chose to continue. Cancelling is reported as an error, not as a variant.
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    /// Capture this window handle (Enter, or a double click on a row).
    Window(usize),
    /// Let the user click the target window on an overlay (F2).
    Overlay,
    /// Capture the window under the cursor after the configured delay (F3).
    Cursor,
}

impl Terminal {
    /// Enters raw mode and the alternate screen on stderr. The guard exists before the escape
    /// sequences are written, so a failure part way through still restores the terminal on drop.
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let terminal = Self;
        execute!(
            io::stderr(),
            EnterAlternateScreen,
            EnableMouseCapture,
            Hide,
            DisableLineWrap
        )?;
        Ok(terminal)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = execute!(
            io::stderr(),
            EndSynchronizedUpdate,
            ResetColor,
            SetAttribute(Attribute::Reset),
            EnableLineWrap,
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// Scores lowercase `text` against lowercase `query`; `None` if a query word is missing.
///
/// Each whitespace-separated word matches independently. The tuple compares
/// lexicographically, lower is better:
///
/// 0. words matched only as a subsequence;
/// 1. extra bytes those subsequence matches span beyond the word length;
/// 2. sum of the byte offsets where the words start.
fn fuzzy_score(text: &str, query: &str) -> Option<(usize, usize, usize)> {
    let mut score = (0, 0, 0);
    for word in query.split_whitespace() {
        if let Some(position) = text.find(word) {
            score.2 += position;
        } else {
            let mut end = 0;
            let mut start = None;
            for character in word.chars() {
                let position = end + text[end..].find(character)?;
                start.get_or_insert(position);
                end = position + character.len_utf8();
            }
            let start = start?;
            score.0 += 1;
            score.1 += end - start - word.len();
            score.2 += start;
        }
    }
    Some(score)
}

/// Mutable state of one picker session.
///
/// `matches` holds indices into `windows` in rank order. `last_click` detects double clicks and
/// is cleared by any key or filter change. `frame` is the last frame drawn, kept for diffing.
struct Picker {
    windows: Vec<WindowInfo>,
    query: String,
    matches: Vec<usize>,
    selected: usize,
    offset: usize,
    last_click: Option<(usize, std::time::Instant)>,
    editor: Option<settings::Editor>,
    frame: Option<draw::Frame>,
    preview: thumbnail::Preview,
}

impl Picker {
    fn new(windows: Vec<WindowInfo>) -> Self {
        let matches = (0..windows.len()).collect();
        Self {
            windows,
            query: String::new(),
            matches,
            selected: 0,
            offset: 0,
            last_click: None,
            editor: None,
            frame: None,
            preview: thumbnail::Preview::default(),
        }
    }

    /// Recomputes `matches` for the current query and resets selection and scroll to the top.
    /// Ties keep window enumeration order because the sort is stable.
    fn refilter(&mut self) {
        self.last_click = None;
        let query = self.query.to_lowercase();
        let mut matches: Vec<_> = self
            .windows
            .iter()
            .enumerate()
            .filter_map(|(index, window)| {
                let text = format!(
                    "{} {}",
                    window.process_name.as_deref().unwrap_or(""),
                    window.title
                )
                .to_lowercase();
                fuzzy_score(&text, &query).map(|score| (score, index))
            })
            .collect();
        matches.sort_by_key(|(score, _)| *score);
        self.matches = matches.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
        self.offset = 0;
    }

    /// Moves the selection by `amount` rows and clamps it to the match list.
    fn move_by(&mut self, amount: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.matches.len().saturating_sub(1));
    }

    /// Applies one key event. Returns the selection when the session ends, `None` to continue,
    /// and an error on Esc or Ctrl+C. `visible` is the PageUp/PageDown step. `args` changes only
    /// when the editor returns `Edit::Apply`.
    fn key(
        &mut self,
        key: KeyEvent,
        visible: usize,
        args: &mut crate::Cli,
    ) -> Result<Option<Selection>> {
        self.last_click = None;
        if matches!(key.code, KeyCode::Char('c' | 'C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            bail!("Window selection cancelled");
        }
        if let Some(editor) = &mut self.editor {
            match editor.key(key) {
                settings::Edit::Continue => {}
                settings::Edit::Apply => *args = self.editor.take().unwrap().draft,
                settings::Edit::Cancel => self.editor = None,
            }
            return Ok(None);
        }
        match key.code {
            KeyCode::Esc => bail!("Window selection cancelled"),
            KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.refilter();
            }
            // Windows reports AltGr as Ctrl+Alt, so Ctrl and Alt together still type a character
            // (`@`, `\`, `{` on German and French layouts). Ctrl or Alt alone does not.
            KeyCode::Char(character)
                if !character.is_control() && {
                    let held = key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
                    held.is_empty() || held == KeyModifiers::CONTROL | KeyModifiers::ALT
                } =>
            {
                self.query.push(character);
                self.refilter();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(visible as isize)),
            KeyCode::PageDown => self.move_by(visible as isize),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.matches.len().saturating_sub(1),
            KeyCode::F(2) => return Ok(Some(Selection::Overlay)),
            KeyCode::F(3) => return Ok(Some(Selection::Cursor)),
            KeyCode::F(4) | KeyCode::Tab => {
                self.editor = Some(settings::Editor::new(args, settings::Field::Format))
            }
            KeyCode::Enter => {
                return Ok(self
                    .matches
                    .get(self.selected)
                    .map(|&index| Selection::Window(self.windows[index].handle)));
            }
            _ => {}
        }
        Ok(None)
    }
}

/// Runs the picker until the user selects a window or cancels.
///
/// `title_filter` is the `--window-title` filter, reapplied on F5. `args` is updated in place
/// when the user applies settings edits. Thumbnails run unless previews are disabled or output
/// is unstyled. Fails when stdin or stderr is not a terminal, when the terminal is smaller than
/// 60x12, or on cancel.
pub fn pick(
    windows: Vec<WindowInfo>,
    title_filter: Option<&str>,
    args: &mut crate::Cli,
) -> Result<Selection> {
    ensure!(
        io::stdin().is_terminal() && io::stderr().is_terminal(),
        "Interactive window selection requires a terminal; use --window-title or --window-hwnd"
    );
    let _terminal = Terminal::enter()?;
    let mut picker = Picker::new(windows);
    if !args.output.no_preview && crate::report::styled() {
        picker.preview.start();
    }
    let (mut columns, mut rows) = terminal::size().unwrap_or((80, 24));
    let mut view = draw::draw(&mut picker, args, &mut io::stderr().lock(), columns, rows)?;
    loop {
        if picker.preview.poll() {
            view = draw::draw(&mut picker, args, &mut io::stderr().lock(), columns, rows)?;
        }
        if !event::poll(std::time::Duration::from_millis(16))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => Some(key),
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                ) =>
            {
                view.mouse(&mut picker, args, mouse).map(KeyEvent::from)
            }
            Event::Resize(width, height) => {
                (columns, rows) = (width, height);
                None
            }
            _ => continue,
        };
        if let Some(key) = key {
            if key.code == KeyCode::F(5) && picker.editor.is_none() {
                picker.preview.epoch += 1;
                picker.windows = capture::list_windows()?;
                if let Some(filter) = title_filter {
                    let filter = filter.to_lowercase();
                    picker
                        .windows
                        .retain(|window| window.title.to_lowercase().contains(&filter));
                }
                picker.refilter();
            } else if let Some(selection) = picker.key(key, view.visible, args)? {
                return Ok(selection);
            }
        }
        view = draw::draw(&mut picker, args, &mut io::stderr().lock(), columns, rows)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn window(handle: usize, process: Option<&str>, title: &str) -> WindowInfo {
        WindowInfo {
            handle,
            pid: handle as u32,
            process_name: process.map(str::to_owned),
            title: title.into(),
            class_name: String::new(),
            rect: capture::Rect {
                left: 120,
                top: 80,
                right: 1400,
                bottom: 980,
            },
            dpi: 144,
        }
    }

    #[test]
    fn filters_by_process_and_title_and_selects_the_matching_row() {
        use clap::Parser;
        let mut args = crate::Cli::parse_from(["alphashot"]);
        let mut picker = Picker::new(vec![
            window(10, Some("notepad.exe"), "Shopping list"),
            window(20, Some("code.exe"), "Notes — Éditeur"),
            window(30, None, "Another window"),
        ]);
        for character in "SHOP ntpd".chars() {
            picker
                .key(KeyCode::Char(character).into(), 10, &mut args)
                .unwrap();
        }
        assert_eq!(picker.matches, [0]);
        assert_eq!(
            picker.key(KeyCode::Enter.into(), 10, &mut args).unwrap(),
            Some(Selection::Window(10))
        );
        // AltGr arrives as Ctrl+Alt on Windows; Ctrl or Alt alone types nothing.
        picker.query.clear();
        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
        ] {
            picker
                .key(KeyEvent::new(KeyCode::Char('@'), modifiers), 10, &mut args)
                .unwrap();
        }
        assert_eq!(picker.query, "@");
        picker.query = "ÉD".into();
        picker.refilter();
        assert_eq!(picker.matches, [1]);
        picker.query = "another".into();
        picker.refilter();
        assert_eq!(
            picker.key(KeyCode::Enter.into(), 10, &mut args).unwrap(),
            Some(Selection::Window(30))
        );
        picker.query = "missing".into();
        picker.refilter();
        picker.move_by(1);
        assert_eq!(
            picker.key(KeyCode::Enter.into(), 10, &mut args).unwrap(),
            None
        );
        picker
            .key(
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
                10,
                &mut args,
            )
            .unwrap();
        picker.key(KeyCode::Down.into(), 10, &mut args).unwrap();
        assert_eq!(
            picker.key(KeyCode::Enter.into(), 10, &mut args).unwrap(),
            Some(Selection::Window(20))
        );
        assert_eq!(
            picker.key(KeyCode::F(2).into(), 10, &mut args).unwrap(),
            Some(Selection::Overlay)
        );
        assert_eq!(
            picker.key(KeyCode::F(3).into(), 10, &mut args).unwrap(),
            Some(Selection::Cursor)
        );
        assert!(picker.key(KeyCode::Esc.into(), 10, &mut args).is_err());
        assert!(
            picker
                .key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    10,
                    &mut args
                )
                .is_err()
        );
    }

    #[test]
    fn prefers_contiguous_matches_and_never_prints_title_control_sequences() {
        assert!(fuzzy_score("notepad", "note") < fuzzy_score("no other text", "note"));
        let mut picker = Picker::new(vec![window(1, Some("test.exe"), "bad\x1b[2J\ncaption")]);
        let mut output = Vec::new();
        use clap::Parser;
        let args = crate::Cli::parse_from(["alphashot"]);
        draw::draw(&mut picker, &args, &mut output, 120, 40).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("test.exe"));
        assert!(output.contains("bad\\u{1b}[2J\\ncaption"));
        assert!(output.contains("Click a window"));
        assert!(output.contains("Cursor in 2s"));
    }

    #[test]
    fn settings_apply_and_cancel_without_losing_the_window_selection() {
        use clap::Parser;
        let mut args = crate::Cli::parse_from([
            "alphashot",
            "-o",
            "window.png",
            "--focus-state",
            "unfocused",
        ]);
        let mut picker = Picker::new(vec![
            window(10, Some("notepad.exe"), "Notes"),
            window(20, Some("code.exe"), "Project"),
        ]);
        picker.query = "code".into();
        picker.refilter();
        for key in [KeyCode::F(4), KeyCode::Right, KeyCode::Esc] {
            assert_eq!(picker.key(key.into(), 10, &mut args).unwrap(), None);
        }
        assert_eq!(
            args.output.out.as_deref(),
            Some(std::path::Path::new("window.png"))
        );
        for key in [
            KeyCode::Tab,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Enter,
        ] {
            assert_eq!(picker.key(key.into(), 10, &mut args).unwrap(), None);
        }
        assert!(picker.editor.is_none());
        assert_eq!(picker.query, "code");
        assert_eq!(
            args.output.out.as_deref(),
            Some(std::path::Path::new("window.webp"))
        );
        assert!(matches!(args.output.out_format, crate::OutputFormat::Webp));
        assert_eq!(args.focus_state, capture::FocusState::Focused);
        assert!(args.include_cursor && args.transparent_client);
        assert_eq!(
            picker.key(KeyCode::Enter.into(), 10, &mut args).unwrap(),
            Some(Selection::Window(20))
        );
    }

    #[test]
    #[ignore = "Requires an interactive terminal; set WebP and include cursor, select code.exe, then cancel the second picker"]
    fn interactive_picker_restores_terminal_after_selection_and_cancel() {
        let raw_before = terminal::is_raw_mode_enabled().unwrap();
        use clap::Parser;
        let mut args = crate::Cli::parse_from(["alphashot"]);
        let windows = vec![
            window(10, Some("notepad.exe"), "Shopping list"),
            window(20, Some("code.exe"), "Notes"),
        ];
        assert_eq!(
            pick(windows, None, &mut args).unwrap(),
            Selection::Window(20)
        );
        assert!(matches!(args.output.out_format, crate::OutputFormat::Webp));
        assert!(args.include_cursor);
        assert_eq!(terminal::is_raw_mode_enabled().unwrap(), raw_before);
        assert!(
            pick(
                vec![window(10, None, "Cancel this picker")],
                None,
                &mut args
            )
            .is_err()
        );
        assert_eq!(terminal::is_raw_mode_enabled().unwrap(), raw_before);
    }
}
