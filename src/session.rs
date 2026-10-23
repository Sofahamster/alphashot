//! Repeated capture: retain a target, choose unused numbered paths, and wait for another shot.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alphashot::capture::{self, WindowInfo};
use anyhow::{Context, Result, ensure};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};

pub(crate) struct Target {
    handle: usize,
    pid: u32,
    class_name: String,
}

impl Target {
    pub(crate) fn new(window: &WindowInfo) -> Self {
        Self {
            handle: window.handle,
            pid: window.pid,
            class_name: window.class_name.clone(),
        }
    }

    fn validate(&self, window: WindowInfo) -> Result<WindowInfo> {
        ensure!(
            window.handle == self.handle
                && window.pid == self.pid
                && window.class_name == self.class_name,
            "Session target 0x{:X} was replaced; restart --repeat and choose the window again",
            self.handle
        );
        Ok(window)
    }

    pub(crate) fn window(&self) -> Result<WindowInfo> {
        let window = capture::window_info(self.handle).with_context(|| {
            format!(
                "Session target 0x{:X} is closed or unavailable; restart --repeat and choose a window again",
                self.handle
            )
        })?;
        self.validate(window)
    }
}

pub(crate) struct Paths {
    pub(crate) image: Option<PathBuf>,
    pub(crate) raw: Option<(PathBuf, PathBuf)>,
}

impl Paths {
    pub(crate) fn new(
        base: Option<&Path>,
        raw_dir: Option<&Path>,
        raw_prefix: &str,
        number: Option<u64>,
    ) -> Result<Self> {
        let image = base
            .map(|path| match number {
                Some(number) => numbered_path(path, number),
                None => Ok(path.to_owned()),
            })
            .transpose()?;
        let raw = raw_dir.map(|dir| {
            let prefix = match number {
                Some(number) => format!("{raw_prefix}-{number:04}"),
                None => raw_prefix.to_owned(),
            };
            (
                dir.join(format!("{prefix}-white.png")),
                dir.join(format!("{prefix}-black.png")),
            )
        });
        Ok(Self { image, raw })
    }

    pub(crate) fn next(
        base: Option<&Path>,
        raw_dir: Option<&Path>,
        raw_prefix: &str,
        number: &mut u64,
    ) -> Result<Self> {
        loop {
            *number = number
                .checked_add(1)
                .context("Session capture numbering exhausted")?;
            let paths = Self::new(base, raw_dir, raw_prefix, Some(*number))?;
            let mut occupied = false;
            for path in paths
                .image
                .iter()
                .chain(paths.raw.iter().flat_map(|(white, black)| [white, black]))
            {
                occupied |= path
                    .try_exists()
                    .with_context(|| format!("Cannot inspect {}", path.display()))?;
            }
            if !occupied {
                return Ok(paths);
            }
        }
    }
}

fn numbered_path(base: &Path, number: u64) -> Result<PathBuf> {
    alphashot::output::check_destination(base, true)?;
    let mut name = base
        .file_stem()
        .context("Output must be a file path")?
        .to_owned();
    name.push(format!("-{number:04}"));
    if let Some(extension) = base.extension() {
        name.push(".");
        name.push(extension);
    }
    Ok(base.with_file_name(name))
}

fn next_action(key: KeyEvent) -> Result<Option<bool>> {
    if !matches!(key.kind, KeyEventKind::Press) {
        return Ok(None);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        anyhow::bail!("Capture cancelled");
    }
    Ok(match key.code {
        KeyCode::Enter => Some(true),
        KeyCode::Esc | KeyCode::Char('q' | 'Q') => Some(false),
        _ => None,
    })
}

/// Raw mode exists only during this wait; all exit paths restore normal terminal input.
pub(crate) fn wait(target: &Target, cancelled: &AtomicBool) -> Result<bool> {
    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
    writeln!(
        io::stderr().lock(),
        "Change the application, then press Enter to capture again; q or Esc ends the session."
    )?;
    terminal::enable_raw_mode().context("Cannot enable session input")?;
    let _raw = RawMode;
    loop {
        ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
        target.window()?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && let Some(capture) = next_action(key)?
        {
            return Ok(capture);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_paths_skip_existing_images_and_raw_groups_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("Büro & 100%.webp");
        std::fs::write(dir.path().join("Büro & 100%-0001.webp"), b"image").unwrap();
        std::fs::write(dir.path().join("capture-0002-black.png"), b"raw").unwrap();
        std::fs::create_dir(dir.path().join("Büro & 100%-0003.webp")).unwrap();
        let mut number = 0;
        for expected in [4, 5] {
            let paths = Paths::next(Some(&base), Some(dir.path()), "capture", &mut number).unwrap();
            assert_eq!(number, expected);
            assert_eq!(
                paths.image.unwrap(),
                dir.path().join(format!("Büro & 100%-{expected:04}.webp"))
            );
            let (white, black) = paths.raw.unwrap();
            assert!(white.ends_with(format!("capture-{expected:04}-white.png")));
            assert!(black.ends_with(format!("capture-{expected:04}-black.png")));
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
        assert_eq!(
            std::fs::read(dir.path().join("Büro & 100%-0001.webp")).unwrap(),
            b"image"
        );
        let paths = Paths::next(None, None, "capture", &mut number).unwrap();
        assert!(paths.image.is_none() && paths.raw.is_none());
        let mut exhausted = u64::MAX;
        let error = Paths::next(None, None, "capture", &mut exhausted)
            .err()
            .unwrap();
        assert!(error.to_string().contains("numbering exhausted"));
        assert_eq!(
            numbered_path(Path::new("capture"), 10_000).unwrap(),
            Path::new("capture-10000")
        );
        assert!(numbered_path(dir.path(), 1).is_err());
        assert!(numbered_path(Path::new(""), 1).is_err());
    }

    #[test]
    fn target_keeps_identity_while_allowing_title_and_placement_changes() {
        let original = || WindowInfo {
            handle: 3,
            pid: 7,
            process_name: None,
            title: "Original".into(),
            class_name: "Editor".into(),
            rect: Default::default(),
            dpi: 96,
        };
        let target = Target::new(&original());
        let mut changed = original();
        changed.title = "Unsaved work".into();
        changed.rect.left = 20;
        changed.dpi = 144;
        assert!(target.validate(changed).is_ok());
        for (handle, pid, class_name) in [(4, 7, "Editor"), (3, 8, "Editor"), (3, 7, "Other")] {
            let error = target
                .validate(WindowInfo {
                    handle,
                    pid,
                    class_name: class_name.into(),
                    ..original()
                })
                .unwrap_err();
            assert!(error.to_string().contains("was replaced"));
            assert!(error.to_string().contains("restart --repeat"));
        }
        let error = Target::new(&WindowInfo {
            handle: 0,
            ..original()
        })
        .window()
        .unwrap_err();
        assert!(error.to_string().contains("closed or unavailable"));
        assert!(
            wait(&target, &AtomicBool::new(true))
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }

    #[test]
    fn session_keys_capture_quit_or_cancel_and_ignore_releases() {
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('Q'),
        ] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(next_action(key).unwrap(), Some(code == KeyCode::Enter));
            assert!(
                next_action(KeyEvent {
                    kind: KeyEventKind::Release,
                    ..key
                })
                .unwrap()
                .is_none()
            );
        }
        assert!(
            next_action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .unwrap()
                .is_none()
        );
        assert!(next_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)).is_err());
    }
}
