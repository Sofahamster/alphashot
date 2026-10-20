//! Captures a window composed over a white and a black backdrop, lists windows and monitors, and
//! defines the types shared with the CLI.
//!
//! Each pass places an opaque backdrop window behind the target, waits for a stable composed
//! frame and reads it. The result also carries the client rectangles to keep opaque and the
//! cursor as overlays for [`crate::alpha::recover`].
//!
//! The Win32 and DWM code lives in a private `windows` submodule; [`snapshot`] adds picker
//! thumbnails. On other targets the desktop functions are stubs that return an error, so the
//! solver and output code build and test everywhere.
//!
//! Capture restores window placement and the foreground window on success, error and
//! cancellation. It refuses hidden, cloaked or moving targets. Backdrop-sampling effects such as
//! blur are out of scope.
//!
//! # Shadow margins
//!
//! The backdrop must cover the DWM drop shadow. [`shadow_margins`] derives a radius `r` from the
//! shadow style, calibrated at 96 DPI and scaled by `dpi / 96`:
//!
//! | Style | `r` at 96 DPI | Use |
//! |-------|---------------|-----|
//! | 0     | 0             | no shadow |
//! | 1     | 16            | menus |
//! | 2 to 5 | 64           | windows; default 2 focused, 4 unfocused |
//!
//! The margins are `(r, r / 2, r, 3r / 2)`, rounded with ties to even, because the shadow falls
//! downward. [`CaptureOptions::pad`] is added on every side. Maximized windows cast no exterior
//! shadow and get neither.
//!
//! # High-DPI display selection
//!
//! [`CaptureOptions::high_dpi`] moves the window to a display scaled above 100% for more pixels
//! per logical unit. A named display ([`CaptureOptions::high_dpi_display`], matched ASCII
//! case-insensitive) must be active and above 96 DPI. Otherwise the highest DPI wins, ties going
//! to the larger work area. The error lists active displays and links
//! <https://github.com/Sofahamster/alphashot/blob/main/docs/high-dpi.md>.
//!
//! # Cursor shadow
//!
//! Composed frames omit the cursor. `cursor_shadow` approximates the Windows pointer shadow from
//! the cursor alpha: the canvas grows by 9 × 7 pixels, the alpha lands at (6, 4) at a quarter
//! strength (`(a & 0xfc) >> 2`), and two passes of a 3 × 3 box sum divided by 8 soften it. The
//! layer sits 3 pixels up and left of the cursor, so the black shadow falls 3 pixels right and 1
//! pixel down.
use anyhow::{Result, ensure};
use image::RgbImage;
#[cfg(any(windows, test))]
use image::RgbaImage;
use std::sync::atomic::AtomicBool;

#[cfg(windows)]
pub mod snapshot;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    capture, enable_per_monitor_dpi, list_monitors, list_windows, pick_cursor, pick_overlay,
    window_info,
};

/// Rectangle in physical screen pixels, laid out like Win32 `RECT`; negative left of or above
/// the primary monitor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Left edge, inclusive.
    pub left: i32,
    /// Top edge, inclusive.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl Rect {
    /// Width; negative for an inverted rectangle.
    pub fn width(self) -> i32 {
        self.right - self.left
    }
    /// Height; negative for an inverted rectangle.
    pub fn height(self) -> i32 {
        self.bottom - self.top
    }
    /// Smallest rectangle containing both.
    pub fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
    /// Returns true if `other` lies inside `self`, shared edges included.
    pub fn contains(self, other: Self) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }
    /// Moves each edge outward; negative amounts move it inward.
    pub fn inflate(self, left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left: self.left - left,
            top: self.top - top,
            right: self.right + right,
            bottom: self.bottom + bottom,
        }
    }
}

/// Top-level window as listed for the picker and reported with a capture.
#[derive(Debug)]
pub struct WindowInfo {
    /// `HWND` as an integer, which keeps the type `Send` and platform-neutral.
    pub handle: usize,
    /// Owning process ID.
    pub pid: u32,
    /// Executable basename; `None` if Windows denies access to the process.
    pub process_name: Option<String>,
    /// Window title; can be empty.
    pub title: String,
    /// Win32 class name, for windows without a useful title.
    pub class_name: String,
    /// Bounds in screen pixels.
    pub rect: Rect,
    /// DPI (96 is 100%); after a capture, the DPI of the composites.
    pub dpi: u32,
}

/// Active display.
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    /// `HMONITOR` as an integer.
    pub handle: usize,
    /// GDI device name such as `\\.\DISPLAY1`.
    pub name: String,
    /// Adapter description, which identifies virtual displays.
    pub description: String,
    /// Monitor bounds in screen pixels.
    pub rect: Rect,
    /// Bounds minus taskbar and docked bars; capture keeps the window and shadow inside.
    pub work: Rect,
    /// Effective DPI (96 is 100%).
    pub dpi: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
/// Activation state at capture; Windows draws active and inactive frames and shadows
/// differently.
pub enum FocusState {
    /// Activates the target for its active frame and larger shadow.
    #[default]
    Focused,
    /// Keeps a helper window in the foreground for the inactive frame.
    Unfocused,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
/// Backdrop extent past the window frame.
pub enum PadMode {
    /// [`shadow_margins`] plus [`CaptureOptions::pad`].
    #[default]
    Shadow,
    /// [`CaptureOptions::pad`] only, for windows without a DWM shadow.
    Uniform,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
/// Cursor shadow policy.
pub enum CursorShadow {
    /// Follows `SPI_GETCURSORSHADOW`.
    #[default]
    Auto,
    /// Always draws the shadow.
    On,
    /// Never draws the shadow.
    Off,
}

/// Parameters for [`capture`].
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Activation state; open menus require [`FocusState::Focused`].
    pub focus: FocusState,
    /// Margin shape around the window.
    pub pad_mode: PadMode,
    /// Extra pixels per side, at most 4096, for shadow the calibration misses.
    pub pad: u32,
    /// Style 0 to 5 for [`shadow_margins`]; `None` picks 2 focused, 4 unfocused.
    pub shadow_style: Option<u8>,
    /// Per-backdrop limit, at most 60000, on waiting for two matching frames outside the
    /// client; 0 accepts the first frame that shows the backdrop.
    pub settle_ms: u64,
    /// Moves the window to a display scaled above 100%; fails with an open menu, which the move
    /// would dismiss.
    pub high_dpi: bool,
    /// Device name of the high-DPI display; requires `high_dpi`.
    pub high_dpi_display: Option<String>,
    /// Reactivates the previous foreground window afterwards.
    pub restore_foreground: bool,
    /// Adds the cursor as an overlay if its hotspot lies in the captured area.
    pub include_cursor: bool,
    /// Shadow policy for the cursor overlay.
    pub cursor_shadow: CursorShadow,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            focus: FocusState::Focused,
            pad_mode: PadMode::Shadow,
            pad: 4,
            shadow_style: None,
            settle_ms: 250,
            high_dpi: false,
            high_dpi_display: None,
            restore_foreground: true,
            include_cursor: false,
            cursor_shadow: CursorShadow::Auto,
        }
    }
}

/// Result of [`capture`], ready for [`crate::alpha::recover`].
pub struct Composites {
    /// Composite over white.
    pub white: RgbImage,
    /// Composite over black, the same size as `white`.
    pub black: RgbImage,
    /// Target window, with `dpi` set to the capture DPI.
    pub window: WindowInfo,
    /// Client rectangles in composite pixels, excluding rounded frame corners.
    pub opaque_regions: Vec<image::math::Rect>,
    /// Cursor shadow and image layers.
    pub cursor: Vec<crate::alpha::Overlay>,
}

/// Picks the high-DPI display per the module docs; errors list active displays and setup links.
#[cfg(any(windows, test))]
fn high_dpi_display<'a>(
    monitors: &'a [MonitorInfo],
    name: Option<&str>,
) -> Result<&'a MonitorInfo> {
    let displays = monitors
        .iter()
        .map(|m| {
            format!(
                "{}: {} ({}%, {}x{})",
                m.name,
                m.description.escape_debug(),
                m.dpi * 100 / 96,
                m.rect.width(),
                m.rect.height()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let setup = format!(
        "Active displays: {displays}. Enable an extended virtual display and set its Windows display scale above 100% (e.g. 200%). Setup: https://github.com/Sofahamster/alphashot/blob/main/docs/high-dpi.md (driver: https://github.com/VirtualDrivers/Virtual-Display-Driver)."
    );
    if let Some(name) = name {
        let monitor = monitors
            .iter()
            .find(|monitor| monitor.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow::anyhow!("High-DPI monitor {name:?} is not active. {setup}"))?;
        ensure!(
            monitor.dpi > 96,
            "Monitor {name:?} is at {} DPI ({}%); high-DPI capture requires a scale above 100%. {setup}",
            monitor.dpi,
            monitor.dpi * 100 / 96
        );
        return Ok(monitor);
    }
    monitors
        .iter()
        .filter(|monitor| monitor.dpi > 96)
        .max_by_key(|monitor| {
            (
                monitor.dpi,
                i64::from(monitor.work.width()) * i64::from(monitor.work.height()),
            )
        })
        .ok_or_else(|| anyhow::anyhow!("No active monitor is scaled above 100%. {setup}"))
}

/// Shadow margins `(left, top, right, bottom)` per the module docs; public for tests.
///
/// # Errors
///
/// Fails for styles above 5.
#[doc(hidden)]
pub fn shadow_margins(style: u8, dpi: u32) -> Result<(i32, i32, i32, i32)> {
    ensure!(style <= 5, "Shadow style must be between 0 and 5");
    let radius = match style {
        0 => 0.0,
        1 => 16.0,
        _ => 64.0,
    } * f64::from(dpi.max(1))
        / 96.0;
    Ok((
        radius.round_ties_even() as i32,
        (radius * 0.5).round_ties_even() as i32,
        radius.round_ties_even() as i32,
        (radius * 1.5).round_ties_even() as i32,
    ))
}

/// Centers a `width` × `height` rectangle in `work`, scaled down to fit; public for tests.
#[doc(hidden)]
pub fn centered(work: Rect, width: i32, height: i32) -> Rect {
    let fit = 1.0_f64
        .min(f64::from(work.width()) / f64::from(width.max(1)))
        .min(f64::from(work.height()) / f64::from(height.max(1)));
    let width = (f64::from(width.max(1)) * fit).round_ties_even().max(1.0) as i32;
    let height = (f64::from(height.max(1)) * fit).round_ties_even().max(1.0) as i32;
    let left = work.left + (work.width() - width).max(0) / 2;
    let top = work.top + (work.height() - height).max(0) / 2;
    Rect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// Builds the cursor shadow layer per the module docs, in integer arithmetic for deterministic
/// output.
#[cfg(any(windows, test))]
fn cursor_shadow(cursor: &RgbaImage) -> RgbaImage {
    let (width, height) = (cursor.width() + 9, cursor.height() + 7);
    let mut alpha = vec![0_u8; (width * height) as usize];
    for (x, y, pixel) in cursor.enumerate_pixels() {
        alpha[((y + 4) * width + x + 6) as usize] = (pixel[3] & 0xfc) >> 2;
    }
    for _ in 0..2 {
        let mut blurred = alpha.clone();
        for y in 1..height - 1 {
            for x in 1..width - 1 {
                let mut sum = 0_u16;
                for dy in y - 1..=y + 1 {
                    for dx in x - 1..=x + 1 {
                        sum += u16::from(alpha[(dy * width + dx) as usize]);
                    }
                }
                blurred[(y * width + x) as usize] = ((sum & 0x7f8) >> 3) as u8;
            }
        }
        alpha = blurred;
    }
    RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([0, 0, 0, alpha[(y * width + x) as usize]])
    })
}

/// Makes the process per-monitor DPI aware; no-op on this platform.
#[cfg(not(windows))]
pub fn enable_per_monitor_dpi() {}
/// Lists capturable top-level windows.
///
/// # Errors
///
/// Always fails on this platform.
#[cfg(not(windows))]
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    anyhow::bail!("Window capture requires an interactive Windows desktop")
}
/// Lists active monitors.
///
/// # Errors
///
/// Always fails on this platform.
#[cfg(not(windows))]
pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    anyhow::bail!("Monitor enumeration requires Windows")
}
/// Describes the window with the given handle.
///
/// # Errors
///
/// Always fails on this platform.
#[cfg(not(windows))]
pub fn window_info(_: usize) -> Result<WindowInfo> {
    anyhow::bail!("Live capture requires Windows")
}
/// Returns the window under the cursor, or its root owner when the flag is set.
///
/// # Errors
///
/// Always fails on this platform.
#[cfg(not(windows))]
pub fn pick_cursor(_: bool) -> Result<usize> {
    anyhow::bail!("Live capture requires Windows")
}
/// Lets the user click a window, highlighted with a frame 1 to 32 pixels thick and limited by a
/// case-insensitive filter.
///
/// # Errors
///
/// Always fails on this platform.
#[cfg(not(windows))]
pub fn pick_overlay(_: Option<&str>, _: u32, _: &AtomicBool) -> Result<usize> {
    anyhow::bail!("Live capture requires Windows")
}
/// Captures the window over white and black backdrops.
///
/// # Errors
///
/// Always fails on this platform.
#[cfg(not(windows))]
pub fn capture(_: usize, _: &CaptureOptions, _: &AtomicBool) -> Result<Composites> {
    anyhow::bail!("Live capture requires Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_dpi_selection_requires_an_active_scaled_display() {
        let monitor = |handle, dpi, size| MonitorInfo {
            handle,
            name: format!(r"\\.\DISPLAY{handle}"),
            description: "Test display".into(),
            rect: Rect {
                left: 0,
                top: 0,
                right: size,
                bottom: size,
            },
            work: Rect {
                left: 0,
                top: 0,
                right: size,
                bottom: size,
            },
            dpi,
        };
        let displays = [
            monitor(1, 96, 1920),
            monitor(2, 192, 3840),
            monitor(3, 144, 7680),
        ];
        assert_eq!(high_dpi_display(&displays, None).unwrap().handle, 2);
        assert_eq!(
            high_dpi_display(&displays, Some(r"\\.\display3"))
                .unwrap()
                .handle,
            3
        );
        for (displays, name, message) in [
            (&displays[..1], None, "above 100%"),
            (&displays[..], Some(r"\\.\DISPLAY1"), "above 100%"),
            (&displays[..], Some(r"\\.\DISPLAY9"), "not active"),
            (&displays[..0], None, "above 100%"),
        ] {
            let error = high_dpi_display(displays, name).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
            assert!(error.contains("Active displays:"), "{error}");
            assert!(error.contains("Virtual-Display-Driver"), "{error}");
        }
    }

    #[test]
    fn calibrated_shadow_geometry_scales_with_dpi() {
        assert_eq!(shadow_margins(2, 96).unwrap(), (64, 32, 64, 96));
        assert_eq!(shadow_margins(4, 144).unwrap(), (96, 48, 96, 144));
        assert_eq!(shadow_margins(0, 192).unwrap(), (0, 0, 0, 0));
        assert!(shadow_margins(6, 96).is_err());
        let work = Rect {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        assert!(work.contains(centered(work, 4000, 2000)));
    }
    #[test]
    fn union_includes_overhanging_and_cascading_menu_bounds() {
        let parent = Rect {
            left: -400,
            top: 100,
            right: -100,
            bottom: 600,
        };
        let menu = Rect {
            left: -350,
            top: 120,
            right: 100,
            bottom: 300,
        };
        let submenu = Rect {
            left: 100,
            top: 250,
            right: 300,
            bottom: 650,
        };
        assert_eq!(
            parent.union(menu).union(submenu),
            Rect {
                left: -400,
                top: 100,
                right: 300,
                bottom: 650
            }
        );
        assert_eq!(parent.union(menu), menu.union(parent));
    }
    #[test]
    fn cursor_blend_clips_negative_coordinates_and_has_shadow() {
        let base = RgbImage::new(2, 2);
        let cursor = RgbaImage::from_pixel(2, 2, image::Rgba([255, 100, 0, 128]));
        let image = crate::alpha::recover(
            &base,
            &base,
            &crate::alpha::AlphaOptions {
                overlays: vec![crate::alpha::Overlay {
                    image: cursor.clone(),
                    x: -1,
                    y: -1,
                }],
                ..Default::default()
            },
        )
        .unwrap()
        .image;
        assert_eq!(image[(0, 0)].0, [128, 50, 0, 255]);
        assert_eq!(image[(1, 1)].0, [0, 0, 0, 255]);
        let shadow = cursor_shadow(&cursor);
        assert_eq!(shadow.dimensions(), (11, 9));
        assert!(shadow.pixels().any(|p| p[3] > 0));
    }
}
