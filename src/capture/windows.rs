//! Win32 backend that captures a window over a white and a black backdrop for the alpha solver.
//!
//! Windows 10 and 11 draw translucent shadows, rounded corners and antialiased frame edges that
//! a screenshot flattens onto the desktop behind them. This module places a solid white window,
//! then a solid black window, directly behind the target, screenshots the composed desktop each
//! time and passes both frames to [`crate::alpha`], which solves straight alpha from the
//! difference. User documentation:
//! [How it works](https://github.com/Sofahamster/alphashot/blob/main/docs/how-it-works.md).
//!
//! The module also lists windows and monitors and picks a window under the cursor
//! ([`pick_cursor`]) or by hover and click ([`pick_overlay`]).
//!
//! # Invariants
//!
//! - Rectangles, cursor positions and bitmap sizes are physical pixels.
//! - [`Restore`] puts back window placement and the previous foreground window on success, on
//!   error and on cancellation.
//! - Overlay windows belong to the calling thread, which pumps messages while they are shown.
//!
//! Non-goals: DXGI desktop duplication (GDI `BitBlt` of the screen DC returns the DWM
//! composition), reconstruction of blur materials such as Acrylic and Mica (their pixels depend
//! on the backdrop, so the solver reads them as transparency), and elevation requests.
//!
//! # Capture pipeline
//!
//! [`capture`] runs these steps in order:
//!
//! 1. [`enable_per_monitor_dpi`]: without per-monitor awareness v2, Windows virtualizes
//!    coordinates on scaled monitors and the backdrops miss the target.
//! 2. Require a visible target that DWM does not cloak. DWM cloaks windows on other virtual
//!    desktops and suspended UWP frames.
//! 3. Collect open native menus with [`open_menus`]. An open menu rules out unfocused capture,
//!    `--high-dpi`, `--client-size` and activation, because each dismisses the menu.
//! 4. Record placement and foreground window in [`Restore`]. If the foreground is to be restored
//!    or the unfocused look is requested, call `AllowSetForegroundWindow` for this process while
//!    the foreground lock still permits it; the later `SetForegroundWindow` calls need it.
//! 5. Create hidden white and black backdrop [`Overlay`]s. They take no focus or mouse input,
//!    stay out of the taskbar, and show without a DWM fade, which would blend into the
//!    composites. Unfocused capture adds an activatable 1x1 off-screen focus sink.
//! 6. With `--high-dpi`, center the target on the chosen monitor and require it to report that
//!    monitor's DPI. DWM bitmap-stretches DPI-unaware windows, which blurs the composites.
//! 7. Restore a minimized target, or a maximized target with `--client-size`, then apply the
//!    requested physical client dimensions, measuring and adjusting for the application's frame.
//!    Activate it unless its root owner is already the foreground
//!    root, then wait 80 ms: some activation animations, such as Windows Terminal's caption
//!    buttons, pause between frames, and the stability check would accept such a frame.
//! 8. For the unfocused look, activate the focus sink so the target draws its inactive frame.
//!    The capture fails if the resulting foreground root does not match the requested state.
//! 9. Compute the capture rectangle: DWM extended frame bounds plus shadow margins and `--pad`.
//!    If it leaves the monitor workspace and no menu is open, [`fit_workspace`] moves or shrinks
//!    the window; an explicit client size permits movement only. Menu rectangles with style 1
//!    margins join the union, which must stay on the virtual screen.
//! 10. Snapshot the cursor before the backdrops appear. Inside the capture rectangle it becomes
//!     a layer over the recovered image, with an optional drop shadow below it.
//! 11. Compute [`opaque_regions`], which the stability check ignores and the solver treats as
//!     opaque.
//! 12. For each backdrop, hide the other one, check that target and menus have not changed, and
//!     place the backdrop behind the target with [`Overlay::place`].
//! 13. Repeat `DwmFlush` and a screenshot until a frame shows the backdrop on its outer rim
//!     ([`backdrop_matches`]) and equals the previous frame outside the opaque regions
//!     ([`unchanged_outside`]). Client content may animate; frame and shadow must be still.
//!     After `settle_ms` the latest frame is used with a warning. A missing backdrop fails the
//!     capture after `settle_ms` plus 1 s.
//! 14. Check target and menus again and hide the backdrop.

use super::*;
use anyhow::{Context, bail};
use std::{
    ffi::c_void,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        Once,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{Dwm::*, Gdi::*},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
    },
    UI::{
        HiDpi::*,
        Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON},
        WindowsAndMessaging::*,
    },
};

/// Encodes `value` as a NUL-terminated UTF-16 string for `W` Win32 APIs.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
/// Decodes a UTF-16 buffer up to its first NUL.
fn from_wide(value: &[u16]) -> String {
    let length = value.iter().position(|&c| c == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

/// Returns the executable file name of `pid`, or `None` if the process is inaccessible or gone.
fn process_name(pid: u32) -> Option<String> {
    // SAFETY: OpenProcess takes plain values; the null failure result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut path = vec![0_u16; 32768];
    let mut length = path.len() as u32;
    // SAFETY: `process` is an open handle; `path` holds `length` u16 elements, and the call
    // writes the result length to the live `length`.
    let success = unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) };
    // SAFETY: `process` was opened above and is closed once, with no later use.
    unsafe { CloseHandle(process) };
    if success == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&path[..length as usize]);
    std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Returns the adapter description for a GDI device name such as `\\.\DISPLAY1`, or an empty
/// string if no adapter matches.
fn display_description(name: &str) -> String {
    let mut device = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        // SAFETY: DISPLAY_DEVICEW is plain data; all-zero is valid.
        ..unsafe { zeroed() }
    };
    for index in 0.. {
        // SAFETY: a null device name enumerates adapters; `device` is live with `cb` set.
        if unsafe { EnumDisplayDevicesW(null(), index, &mut device, 0) } == 0 {
            break;
        }
        if from_wide(&device.DeviceName).eq_ignore_ascii_case(name) {
            return from_wide(&device.DeviceString);
        }
    }
    String::new()
}
/// Converts a Win32 `BOOL` result into an error carrying `GetLastError` and the operation name.
fn check(success: i32, operation: &str) -> Result<()> {
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context(operation.to_owned());
    }
    Ok(())
}
fn rect(rect: RECT) -> Rect {
    Rect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// Switches the process to per-monitor DPI awareness v2 once.
///
/// Public entry points call this so rectangles, cursor positions and bitmap sizes are physical
/// pixels. A failure, for example after a manifest set the awareness, keeps the existing one.
pub fn enable_per_monitor_dpi() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: the call takes a predefined pseudo-handle constant and no pointers.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    });
}

/// Dispatches pending messages so this thread's overlay windows repaint while it waits.
fn pump() {
    // SAFETY: MSG is plain data, so zeroed is valid; `message` outlives each call, and
    // PeekMessageW reads only this thread's queue.
    unsafe {
        let mut message = zeroed();
        while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Sleeps for `duration` in 15 ms slices, pumping messages and honoring cancellation.
fn wait(duration: Duration, cancelled: &AtomicBool) -> Result<()> {
    let until = Instant::now() + duration;
    loop {
        ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
        pump();
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(Duration::from_millis(15)));
    }
}

/// Pumps messages and waits for the next DWM composition, which puts earlier window changes on
/// screen.
fn flush() {
    pump();
    // SAFETY: DwmFlush takes no arguments.
    unsafe {
        DwmFlush();
    }
}
/// Returns the window rectangle including the invisible resize borders.
fn window_rect(hwnd: HWND) -> Result<Rect> {
    let mut bounds = RECT::default();
    // SAFETY: `bounds` is a live RECT; an invalid `hwnd` makes the call fail.
    unsafe {
        check(GetWindowRect(hwnd, &mut bounds), "GetWindowRect")?;
    }
    Ok(rect(bounds))
}
/// Returns the physical client rectangle on the screen, including mirrored client layouts.
fn client_rect(hwnd: HWND) -> Result<Rect> {
    let mut client = RECT::default();
    // SAFETY: `client` is a live RECT, which is two consecutive POINTs. With exactly two points,
    // MapWindowPoints swaps left and right for mirrored (WS_EX_LAYOUTRTL) windows, where client
    // x = 0 is the right edge; ClientToScreen of the origin would give the wrong corner there.
    unsafe {
        check(GetClientRect(hwnd, &mut client), "GetClientRect")?;
        MapWindowPoints(
            hwnd,
            null_mut(),
            (&mut client as *mut RECT).cast::<POINT>(),
            2,
        );
    }
    Ok(rect(client))
}
/// Returns the DWM extended frame bounds, which exclude the invisible resize borders, or the
/// window rectangle if DWM fails.
fn extended(hwnd: HWND) -> Result<Rect> {
    let mut bounds = RECT::default();
    // SAFETY: the out pointer refers to the live `bounds` with size_of RECT, the type
    // DWMWA_EXTENDED_FRAME_BOUNDS writes.
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut bounds as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    } >= 0
    {
        Ok(rect(bounds))
    } else {
        window_rect(hwnd)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ShadowPolicy {
    blend_space: crate::alpha::BlendSpace,
    corner_radius: f32,
}

/// Standard DWM frames use an approximate mask; per-pixel layered sources stay unmasked.
fn validate_no_shadow(hwnd: HWND, options: &CaptureOptions) -> Result<ShadowPolicy> {
    if !options.no_shadow {
        return Ok(ShadowPolicy::default());
    }
    ensure!(
        open_menus(hwnd)?.is_empty(),
        "--no-shadow cannot separate open menu shadows; close the menu and retry"
    );
    // SAFETY: the error state is thread-local. A zero extended style is valid on success.
    let (extended_style, style_error) = unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        (style, windows_sys::Win32::Foundation::GetLastError())
    };
    ensure!(
        extended_style != 0 || style_error == 0,
        "--no-shadow could not read target window styles: Win32 error {style_error}"
    );
    let layered = extended_style & WS_EX_LAYERED as isize != 0;
    // SAFETY: IsZoomed accepts the handle by value and returns false for invalid handles.
    let maximized = unsafe { IsZoomed(hwnd) } != 0;
    if !layered && shadow_free_maximized(maximized, options) {
        return Ok(ShadowPolicy::default());
    }
    if layered {
        ensure!(
            shadow_free_layered(hwnd),
            "--no-shadow cannot mask this layered window safely; forced DWM rounding or an unknown shadow policy is unsupported"
        );
        return Ok(ShadowPolicy {
            blend_space: crate::alpha::BlendSpace::Srgb,
            corner_radius: 0.0,
        });
    }
    // SAFETY: these calls only read the live window's style and DPI.
    let (style, dpi) = unsafe {
        (
            GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
            GetDpiForWindow(hwnd),
        )
    };
    let frame = extended(hwnd)?;
    let client = client_rect(hwnd)?;
    let metric = if extended_style & WS_EX_TOOLWINDOW as isize != 0 {
        SM_CYSMCAPTION
    } else {
        SM_CYCAPTION
    };
    let mut non_client = 0_i32;
    let mut region = RECT::default();
    // SAFETY: the metric and DPI are plain inputs; DWM writes to the live BOOL and RECT.
    let native = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_ENABLED as u32,
            (&mut non_client as *mut i32).cast(),
            size_of::<i32>() as u32,
        ) >= 0
            && non_client != 0
            && GetWindowRgnBox(hwnd, &mut region) == ERROR
            && native_frame(style, frame, client, GetSystemMetricsForDpi(metric, dpi))
    };
    ensure!(
        native,
        "--no-shadow corner masking requires a standard DWM caption and frame; custom chrome, window regions and unidentifiable frames are unsupported"
    );
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        // SAFETY: OSVERSIONINFOW contains only integers and a fixed UTF-16 array.
        ..unsafe { zeroed() }
    };
    ensure!(
        // SAFETY: RtlGetVersion writes the initialized, correctly sized OSVERSIONINFOW.
        unsafe { RtlGetVersion(&mut version) } >= 0,
        "--no-shadow could not query the Windows version"
    );
    let mut preference = -1_i32;
    // SAFETY: DWM writes the corner-preference enum to the live i32.
    let preference = (unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&mut preference as *mut i32).cast(),
            size_of::<i32>() as u32,
        )
    } >= 0)
        .then_some(preference);
    // SAFETY: these predicates read window/session state without changing it.
    let square = unsafe {
        IsZoomed(hwnd) != 0
            || IsWindowArranged(hwnd) != 0
            || GetSystemMetrics(SM_REMOTESESSION) != 0
    };
    Ok(ShadowPolicy {
        blend_space: crate::alpha::BlendSpace::Srgb,
        corner_radius: standard_corner_radius(preference, version.dwBuildNumber, dpi, square)?,
    })
}

fn native_frame(style: u32, frame: Rect, client: Rect, caption_height: i32) -> bool {
    style & WS_CAPTION == WS_CAPTION
        && style & WS_CHILD == 0
        && caption_height > 0
        && frame.contains(client)
        && client.width() > 0
        && client.height() > 0
        && client.top - frame.top >= caption_height
}

fn standard_corner_radius(
    preference: Option<i32>,
    build: u32,
    dpi: u32,
    square: bool,
) -> Result<f32> {
    ensure!(
        build >= 18362 && dpi > 0,
        "--no-shadow could not identify the Windows build or target DPI"
    );
    if build < 22000 || square {
        return Ok(0.0);
    }
    // shortcut: calibrated standard-frame radii; revisit if Windows changes its geometry.
    let radius = match preference {
        Some(DWMWCP_DEFAULT | DWMWCP_ROUND) => 8.0,
        Some(DWMWCP_ROUNDSMALL) => 4.0,
        Some(DWMWCP_DONOTROUND) => 0.0,
        _ => bail!("--no-shadow cannot identify this window's DWM corner preference"),
    };
    Ok(radius * dpi as f32 / 96.0)
}

fn shadow_free_layered(hwnd: HWND) -> bool {
    let mut non_client = 1_i32;
    let mut corner = -1_i32;
    // SAFETY: DWM writes a BOOL to the live i32. Failure keeps the conservative default.
    let non_client_result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_ENABLED as u32,
            (&mut non_client as *mut i32).cast(),
            size_of::<i32>() as u32,
        )
    };
    // Forced DWM rounding adds frame/shadow pixels even when non-client rendering is disabled.
    // SAFETY: DWM writes the documented corner-preference enum to the live i32.
    let corner_result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&mut corner as *mut i32).cast(),
            size_of::<i32>() as u32,
        )
    };
    // SAFETY: both last-error functions affect only this thread. A zero class style is valid,
    // so a cleared last error distinguishes it from a failed GetClassLongPtrW call.
    let class_has_no_shadow = unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        let style = GetClassLongPtrW(hwnd, GCL_STYLE);
        (style != 0 || windows_sys::Win32::Foundation::GetLastError() == 0)
            && style & CS_DROPSHADOW as usize == 0
    };
    non_client_result >= 0
        && non_client == 0
        && class_has_no_shadow
        && corner_result >= 0
        && matches!(corner, DWMWCP_DEFAULT | DWMWCP_DONOTROUND)
}

fn shadow_free_maximized(maximized: bool, options: &CaptureOptions) -> bool {
    maximized
        && (options.backend == CaptureBackend::Wgc
            || (!options.high_dpi && options.client_size.is_none()))
}
/// Reports whether DWM cloaks the window, as it does for windows on other virtual desktops and
/// suspended app frames that `IsWindowVisible` reports as visible.
fn cloaked(hwnd: HWND) -> bool {
    let mut value = 0_u32;
    // SAFETY: the out pointer refers to the live `value` with size_of u32, the type of
    // DWMWA_CLOAKED.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut value as *mut u32).cast(),
            size_of::<u32>() as u32,
        ) >= 0
            && value != 0
    }
}
/// Returns the root owner, so owned dialogs, popups and menus compare equal to their app window.
fn root(hwnd: HWND) -> HWND {
    // SAFETY: GetAncestor takes plain values and returns null for invalid handles.
    let owner = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    if owner.is_null() { hwnd } else { owner }
}

/// `EnumThreadWindows` callback that collects visible popup menu windows (class `#32768`).
///
/// # Safety
///
/// `data` must point to a live `Vec<usize>` that nothing else accesses during the enumeration.
unsafe extern "system" fn enum_menu(hwnd: HWND, data: LPARAM) -> i32 {
    // SAFETY: EnumThreadWindows calls this synchronously; `data` is the exclusively borrowed
    // `Vec<usize>` from `open_menus`. GetClassNameW writes at most 32 u16 into `class`.
    unsafe {
        let mut class = [0_u16; 32];
        let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
        if IsWindowVisible(hwnd) != 0 && String::from_utf16_lossy(&class[..length]) == "#32768" {
            (&mut *(data as *mut Vec<usize>)).push(hwnd as usize);
        }
    }
    1
}

/// Returns the open native menus of the target's root owner with their bounds, sorted by
/// handle so two results compare equal while no menu opens, closes or moves.
fn open_menus(hwnd: HWND) -> Result<Vec<(usize, Rect)>> {
    // SAFETY: a null process-id pointer is allowed; the call returns 0 for invalid windows.
    let thread = unsafe { GetWindowThreadProcessId(hwnd, null_mut()) };
    ensure!(
        thread != 0,
        "Target window disappeared while finding its menus"
    );
    let mut gui = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        // SAFETY: GUITHREADINFO holds integers, handles and a RECT; all-zero is valid.
        ..unsafe { zeroed() }
    };
    // SAFETY: `gui` is a live GUITHREADINFO with `cbSize` set as the API requires.
    unsafe {
        check(
            GetGUIThreadInfo(thread, &mut gui),
            "GetGUIThreadInfo target menu owner",
        )?;
    }
    if gui.flags & (GUI_INMENUMODE | GUI_POPUPMENUMODE | GUI_SYSTEMMENUMODE) == 0
        || root(gui.hwndMenuOwner) != root(hwnd)
    {
        return Ok(Vec::new());
    }
    let mut handles = Vec::new();
    // SAFETY: `enum_menu` reads LPARAM as `Vec<usize>`, the type passed here; `handles`
    // outlives the synchronous enumeration.
    unsafe {
        check(
            EnumThreadWindows(
                thread,
                Some(enum_menu),
                (&mut handles as *mut Vec<usize>) as isize,
            ),
            "EnumThreadWindows target menus",
        )?;
    }
    let mut menus = handles
        .into_iter()
        .map(|handle| Ok((handle, extended(handle as HWND)?)))
        .collect::<Result<Vec<_>>>()?;
    menus.sort_unstable_by_key(|(handle, _)| *handle);
    Ok(menus)
}

/// Describes a window by raw handle; handles that fail `IsWindow` return an error.
pub fn window_info(handle: usize) -> Result<WindowInfo> {
    enable_per_monitor_dpi();
    let hwnd = handle as HWND;
    // SAFETY: the calls take `hwnd` by value and fail softly if the window has gone. Each
    // buffer is passed with its length, and returned lengths are clamped to >= 0.
    unsafe {
        ensure!(IsWindow(hwnd) != 0, "Invalid window handle: 0x{handle:X}");
        let mut title = vec![0_u16; (GetWindowTextLengthW(hwnd).max(0) + 1) as usize];
        let length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0) as usize;
        let mut class = [0_u16; 256];
        let class_length =
            GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        Ok(WindowInfo {
            handle,
            pid,
            process_name: process_name(pid),
            title: String::from_utf16_lossy(&title[..length]),
            class_name: String::from_utf16_lossy(&class[..class_length]),
            rect: window_rect(hwnd)?,
            dpi: GetDpiForWindow(hwnd).max(96),
        })
    }
}

/// `EnumWindows` callback that keeps visible, non-minimized, uncloaked windows of at least
/// 80x60 pixels, including untitled windows selectable by process or class.
///
/// # Safety
///
/// `data` must point to a live `Vec<WindowInfo>` that nothing else accesses during the
/// enumeration.
unsafe extern "system" fn enum_window(hwnd: HWND, data: LPARAM) -> i32 {
    // SAFETY: EnumWindows calls this synchronously; `data` is the exclusively borrowed
    // `Vec<WindowInfo>` from `list_windows`.
    unsafe {
        if IsWindowVisible(hwnd) != 0
            && IsIconic(hwnd) == 0
            && !cloaked(hwnd)
            && let Ok(info) = window_info(hwnd as usize)
            && info.rect.width() >= 80
            && info.rect.height() >= 60
        {
            (&mut *(data as *mut Vec<WindowInfo>)).push(info);
        }
    }
    1
}

/// Lists capturable top-level windows front to back.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    enable_per_monitor_dpi();
    let mut result = Vec::new();
    // SAFETY: `enum_window` reads LPARAM as `Vec<WindowInfo>`, the type passed here; `result`
    // outlives the synchronous enumeration.
    unsafe {
        check(
            EnumWindows(
                Some(enum_window),
                (&mut result as *mut Vec<WindowInfo>) as isize,
            ),
            "EnumWindows",
        )?;
    }
    Ok(result)
}

/// `EnumDisplayMonitors` callback that records bounds, workspace and effective DPI.
///
/// # Safety
///
/// `data` must point to a live `Vec<MonitorInfo>` that nothing else accesses during the
/// enumeration.
unsafe extern "system" fn enum_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    // SAFETY: MONITORINFOEXW is plain data; `cbSize` is the EX size, so `szDevice` is filled.
    // EnumDisplayMonitors calls this synchronously; `data` is the exclusively borrowed
    // `Vec<MonitorInfo>` from `list_monitors`.
    unsafe {
        let mut info: MONITORINFOEXW = zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info.monitorInfo) != 0 {
            let (mut x, mut y) = (96, 96);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y);
            let name = from_wide(&info.szDevice);
            (&mut *(data as *mut Vec<MonitorInfo>)).push(MonitorInfo {
                handle: monitor as usize,
                description: display_description(&name),
                name,
                rect: rect(info.monitorInfo.rcMonitor),
                work: rect(info.monitorInfo.rcWork),
                dpi: x.max(96),
            });
        }
    }
    1
}

/// Lists active monitors with bounds, workspace and effective DPI.
pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    enable_per_monitor_dpi();
    let mut result = Vec::new();
    // SAFETY: null DC and clip rectangle enumerate all monitors; `enum_monitor` reads LPARAM
    // as `Vec<MonitorInfo>`, and `result` outlives the synchronous enumeration.
    unsafe {
        check(
            EnumDisplayMonitors(
                null_mut(),
                null(),
                Some(enum_monitor),
                (&mut result as *mut Vec<MonitorInfo>) as isize,
            ),
            "EnumDisplayMonitors",
        )?;
    }
    Ok(result)
}

/// Returns the top-level window under the cursor, or its root owner with `root_owner`.
///
/// `WindowFromPoint` returns the deepest child control; a child cannot be captured because
/// the backdrop is a top-level window and cannot sit between a child and its parent.
pub fn pick_cursor(root_owner: bool) -> Result<usize> {
    enable_per_monitor_dpi();
    let mut point = POINT::default();
    // SAFETY: `point` is a live POINT.
    unsafe {
        check(GetCursorPos(&mut point), "GetCursorPos")?;
    }
    // SAFETY: WindowFromPoint takes the point by value and returns null when nothing is there.
    let hwnd = unsafe { WindowFromPoint(point) };
    ensure!(!hwnd.is_null(), "No window under the cursor");
    Ok(if root_owner {
        root(hwnd)
    } else {
        top_level(hwnd)
    } as usize)
}

/// Returns the top-level window that contains `hwnd`, or `hwnd` itself if it has no parent.
fn top_level(hwnd: HWND) -> HWND {
    // SAFETY: GetAncestor takes plain values and returns null for invalid handles.
    let top = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if top.is_null() { hwnd } else { top }
}

/// Window procedure for overlays; `DefWindowProcW` paints the class brush as background.
///
/// # Safety
///
/// Called by the system with the arguments of a message sent to an overlay window.
unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the arguments come unchanged from the system's dispatch of this message.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// A borderless solid-color popup used as capture backdrop, picker border and focus sink.
///
/// Each instance registers its own window class because the class brush sets the color. The
/// creating thread owns the window and must pump messages while it is shown.
struct Overlay {
    hwnd: HWND,
    class: Vec<u16>,
    brush: HBRUSH,
}
impl Overlay {
    /// Creates a hidden 1x1 off-screen overlay with background `color` (`0x00BBGGRR`).
    ///
    /// Without `activate` the overlay takes neither focus nor mouse input; the focus sink sets
    /// `activate`.
    fn new(color: u32, activate: bool) -> Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let class = wide(&format!(
            "Alphashot-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // SAFETY: WNDCLASSW is plain data, so zeroed fields are valid. `class` is NUL-terminated
        // and lives in the Overlay until Drop unregisters it; the empty title outlives
        // CreateWindowExW. Failure paths release earlier objects. `disabled` is a live i32.
        unsafe {
            let instance = GetModuleHandleW(null());
            let brush = CreateSolidBrush(color);
            ensure!(!brush.is_null(), "CreateSolidBrush failed");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(overlay_proc),
                hInstance: instance,
                hbrBackground: brush,
                lpszClassName: class.as_ptr(),
                ..zeroed()
            };
            if RegisterClassW(&wc) == 0 {
                let error = std::io::Error::last_os_error();
                DeleteObject(brush);
                return Err(error).context("RegisterClassW");
            }
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW
                    | if activate {
                        0
                    } else {
                        WS_EX_NOACTIVATE | WS_EX_TRANSPARENT
                    },
                class.as_ptr(),
                wide("").as_ptr(),
                WS_POPUP,
                -32000,
                -32000,
                1,
                1,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                let error = std::io::Error::last_os_error();
                UnregisterClassW(class.as_ptr(), instance);
                DeleteObject(brush);
                return Err(error).context("CreateWindowExW");
            }
            // A DWM fade on show or hide would blend into the composites.
            let disabled = 1_i32;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_TRANSITIONS_FORCEDISABLED as u32,
                (&disabled as *const i32).cast(),
                size_of::<i32>() as u32,
            );
            Ok(Self { hwnd, class, brush })
        }
    }
    /// Shows the overlay at `bounds`, directly behind `behind` in z-order.
    ///
    /// If UIPI denies `behind` as insert-after window (ERROR_ACCESS_DENIED for a
    /// higher-integrity target), the overlay moves to the non-topmost band and is shown only if
    /// `behind` stays above it.
    fn place(&self, behind: HWND, bounds: Rect) -> Result<()> {
        // SAFETY: `self.hwnd` is a live window of this thread; `behind` is a handle or HWND_*
        // constant passed by value.
        unsafe {
            if SetWindowPos(
                self.hwnd,
                behind,
                bounds.left,
                bounds.top,
                bounds.width(),
                bounds.height(),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(5) || IsWindow(behind) == 0 {
                    return Err(error).context("SetWindowPos overlay");
                }
                // UIPI rejected a higher-integrity insert-after window; use the normal band.
                let retry = check(
                    SetWindowPos(
                        self.hwnd,
                        HWND_NOTOPMOST,
                        bounds.left,
                        bounds.top,
                        bounds.width(),
                        bounds.height(),
                        SWP_NOACTIVATE,
                    ),
                    "Position capture backdrop",
                )
                .and_then(|()| {
                    ensure!(
                        window_above(behind, self.hwnd),
                        "Backdrop would cover the target window"
                    );
                    check(
                        SetWindowPos(
                            self.hwnd,
                            null_mut(),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOACTIVATE
                                | SWP_NOZORDER
                                | SWP_NOMOVE
                                | SWP_NOSIZE
                                | SWP_SHOWWINDOW,
                        ),
                        "Show capture backdrop",
                    )
                });
                if let Err(retry) = retry {
                    self.hide();
                    return Err(retry).context(
                        "Windows denied backdrop placement behind this window. It may be running elevated; run Alphashot from an administrator terminal, or run the target without elevation",
                    );
                }
            }
        }
        pump();
        Ok(())
    }
    /// Hides the overlay for reuse.
    fn hide(&self) {
        // SAFETY: `self.hwnd` is a live window of this thread.
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

/// Reports whether `upper` precedes `lower` in the top-level z-order and `lower` still exists.
fn window_above(upper: HWND, lower: HWND) -> bool {
    struct Order {
        upper: HWND,
        lower: HWND,
        above: bool,
    }
    /// `EnumWindows` callback that stops at the first of the two windows.
    ///
    /// # Safety
    ///
    /// `data` must point to a live `Order` that nothing else accesses during the enumeration.
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> i32 {
        // SAFETY: EnumWindows calls this synchronously; `data` is the exclusively borrowed
        // `Order` from `window_above`.
        let order = unsafe { &mut *(data as *mut Order) };
        if hwnd == order.upper {
            order.above = true;
            0
        } else {
            i32::from(hwnd != order.lower)
        }
    }
    let mut order = Order {
        upper,
        lower,
        above: false,
    };
    // SAFETY: `visit` reads LPARAM as `Order`, the type passed here; `order` outlives the
    // synchronous enumeration. The 0 result of a stopped enumeration is ignored.
    unsafe {
        EnumWindows(Some(visit), &mut order as *mut Order as LPARAM);
        order.above && IsWindow(lower) != 0
    }
}
impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: this value owns the window, class and brush from `Overlay::new`. Raw handles
        // make Overlay !Send, so Drop runs on the creating thread. The window goes before its
        // class, and the brush after the class that references it.
        unsafe {
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
            DeleteObject(self.brush);
        }
    }
}

/// Returns the root window the user clicks while hovering.
///
/// Four topmost, click-through strips of `thickness` pixels frame the hovered window. `filter`
/// limits hover targets to windows matching every selector. Esc or `cancelled` aborts.
/// The loop polls mouse and keyboard state every 15 ms and installs no global hook.
pub fn pick_overlay(
    filter: &WindowFilter,
    thickness: u32,
    cancelled: &AtomicBool,
) -> Result<usize> {
    enable_per_monitor_dpi();
    ensure!(
        (1..=32).contains(&thickness),
        "Picker border thickness must be between 1 and 32"
    );
    let borders = (0..4)
        .map(|_| Overlay::new(0x00ffaa00, false))
        .collect::<Result<Vec<_>>>()?;
    let mut hovered = 0;
    // SAFETY: GetAsyncKeyState takes a virtual-key code by value.
    let mut button_was_down = unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON)) } < 0;
    eprintln!("Move over a window and click to select it. Esc or Ctrl+C cancels.");
    loop {
        ensure!(
            // SAFETY: GetAsyncKeyState takes a virtual-key code by value.
            unsafe { GetAsyncKeyState(i32::from(VK_ESCAPE)) } >= 0,
            "Window selection cancelled"
        );
        let handle = pick_cursor(true)?;
        let hwnd = handle as HWND;
        // SAFETY: IsWindowVisible takes the handle by value and returns 0 for invalid windows.
        let info = window_info(handle).ok().filter(|info| unsafe { IsWindowVisible(hwnd) != 0 }
            && !cloaked(hwnd) && info.rect.width() >= 16 && info.rect.height() >= 16
            && !borders.iter().any(|border| border.hwnd == hwnd)
            && filter.matches(info));
        let current = if info.is_some() { handle } else { 0 };
        if current != hovered {
            hovered = current;
            if let Some(info) = info {
                eprintln!("0x{:X} {}", info.handle, info.title.escape_debug());
            }
        }
        if hovered != 0 {
            let b = extended(hovered as HWND)?;
            let t = thickness as i32;
            let rectangles = [
                Rect {
                    left: b.left - t,
                    top: b.top - t,
                    right: b.right + t,
                    bottom: b.top,
                },
                Rect {
                    left: b.left - t,
                    top: b.top,
                    right: b.left,
                    bottom: b.bottom,
                },
                Rect {
                    left: b.right,
                    top: b.top,
                    right: b.right + t,
                    bottom: b.bottom,
                },
                Rect {
                    left: b.left - t,
                    top: b.bottom,
                    right: b.right + t,
                    bottom: b.bottom + t,
                },
            ];
            for (border, bounds) in borders.iter().zip(rectangles) {
                border.place(HWND_TOPMOST, bounds)?;
            }
        } else {
            for border in &borders {
                border.hide();
            }
        }
        // SAFETY: GetAsyncKeyState takes a virtual-key code by value.
        let down = unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON)) } < 0;
        if down && !button_was_down && hovered != 0 {
            return Ok(hovered);
        }
        button_was_down = down;
        wait(Duration::from_millis(15), cancelled)?;
    }
}

/// A memory DC with a selected 32-bit top-down DIB section that exposes its pixels.
///
/// Creation sets fields step by step, so `Drop` releases a partially built surface.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}
impl Surface {
    /// Allocates a `width` x `height` BGRA surface of at most 40 million pixels, which keeps
    /// the byte count from overflowing and rejects oversized captures early.
    fn new(width: i32, height: i32) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && i64::from(width) * i64::from(height) <= 40_000_000,
            "Invalid capture size or capture exceeds 40 million pixels"
        );
        // SAFETY: BITMAPINFO is plain data; the header describes a positive-size 32 bpp
        // top-down BI_RGB DIB. Each handle is stored in `surface` before the next fallible
        // step, so early returns release it through Surface::drop.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            ensure!(!dc.is_null(), "CreateCompatibleDC failed");
            let mut surface = Self {
                dc,
                bitmap: null_mut(),
                previous: null_mut(),
                bits: null_mut(),
                width,
                height,
            };
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..zeroed()
                },
                ..zeroed()
            };
            let mut bits: *mut c_void = null_mut();
            surface.bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            ensure!(
                !surface.bitmap.is_null() && !bits.is_null(),
                "CreateDIBSection failed"
            );
            surface.bits = bits.cast();
            surface.previous = SelectObject(dc, surface.bitmap);
            ensure!(
                !surface.previous.is_null() && surface.previous as isize != -1,
                "SelectObject failed"
            );
            Ok(surface)
        }
    }
    /// Returns the BGRA pixels; call `GdiFlush` first to complete pending GDI drawing.
    fn bytes(&self) -> &[u8] {
        // SAFETY: the DIB section maps width * height * 4 bytes at `bits` (32 bpp rows have no
        // padding) until Drop; the shared borrow excludes `fill` while the slice lives.
        unsafe {
            std::slice::from_raw_parts(self.bits, self.width as usize * self.height as usize * 4)
        }
    }
    /// Sets every byte to `value`.
    fn fill(&mut self, value: u8) {
        // SAFETY: `bits` points to width * height * 4 writable bytes, and `&mut self` excludes
        // live `bytes` slices.
        unsafe {
            std::ptr::write_bytes(
                self.bits,
                value,
                self.width as usize * self.height as usize * 4,
            );
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: this surface owns the DC and bitmap. GDI cannot delete a selected bitmap, so
        // the original object goes back first and the DC goes last. Null handles are skipped.
        unsafe {
            if !self.previous.is_null() && self.previous as isize != -1 {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            DeleteDC(self.dc);
        }
    }
}

/// Copies the composed desktop inside `bounds` (virtual-screen coordinates) into an RGB image.
///
/// `CAPTUREBLT` includes layered windows such as menus. The copy omits the cursor.
fn screenshot(bounds: Rect) -> Result<RgbImage> {
    let surface = Surface::new(bounds.width(), bounds.height())?;
    // SAFETY: the screen DC is checked for null and released once after BitBlt. `surface`
    // covers the copied area, and GdiFlush completes the blit before `bytes` reads it.
    unsafe {
        let screen = GetDC(null_mut());
        ensure!(!screen.is_null(), "Cannot access the desktop DC");
        let result = BitBlt(
            surface.dc,
            0,
            0,
            bounds.width(),
            bounds.height(),
            screen,
            bounds.left,
            bounds.top,
            SRCCOPY | CAPTUREBLT,
        );
        let error = std::io::Error::last_os_error();
        ReleaseDC(null_mut(), screen);
        if result == 0 {
            return Err(error).context("BitBlt desktop capture");
        }
        GdiFlush();
    }
    let mut image = RgbImage::new(bounds.width() as u32, bounds.height() as u32);
    for (pixel, bgra) in image.pixels_mut().zip(surface.bytes().as_chunks::<4>().0) {
        pixel.0 = [bgra[2], bgra[1], bgra[0]];
    }
    Ok(image)
}

/// A cursor image with straight alpha, its screen position and its hotspot.
struct Cursor {
    point: POINT,
    hotspot: (i32, i32),
    image: RgbaImage,
}
/// Snapshots the visible cursor, or returns `None` if it is hidden.
///
/// The cursor is drawn over white and over black and its alpha solved from the difference,
/// which covers color, monochrome (XOR mask) and premultiplied cursors.
fn cursor() -> Result<Option<Cursor>> {
    // SAFETY: CURSORINFO, ICONINFO and BITMAP are plain data; `cbSize` is set. GetIconInfo
    // returns bitmap copies that this function deletes once; `info.hCursor` is a shared system
    // handle and is not destroyed. DrawIconEx draws into surfaces of `width` x `height`, and
    // GdiFlush completes drawing before `bytes` reads them.
    unsafe {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..zeroed()
        };
        check(GetCursorInfo(&mut info), "GetCursorInfo")?;
        if info.flags & CURSOR_SHOWING == 0 || info.hCursor.is_null() {
            return Ok(None);
        }
        let mut icon: ICONINFO = zeroed();
        check(GetIconInfo(info.hCursor, &mut icon), "GetIconInfo")?;
        let mut bitmap: BITMAP = zeroed();
        let has_color = !icon.hbmColor.is_null();
        let source = if has_color {
            icon.hbmColor
        } else {
            icon.hbmMask
        };
        let measured = GetObjectW(
            source,
            size_of::<BITMAP>() as i32,
            (&mut bitmap as *mut BITMAP).cast(),
        ) != 0;
        if !icon.hbmColor.is_null() {
            DeleteObject(icon.hbmColor);
        }
        if !icon.hbmMask.is_null() {
            DeleteObject(icon.hbmMask);
        }
        let width = if measured {
            bitmap.bmWidth
        } else {
            GetSystemMetrics(SM_CXCURSOR)
        }
        .max(1);
        let height = if measured {
            bitmap.bmHeight / if has_color { 1 } else { 2 }
        } else {
            GetSystemMetrics(SM_CYCURSOR)
        }
        .max(1);
        let mut white = Surface::new(width, height)?;
        let mut black = Surface::new(width, height)?;
        white.fill(255);
        black.fill(0);
        check(
            DrawIconEx(
                white.dc,
                0,
                0,
                info.hCursor,
                width,
                height,
                0,
                null_mut(),
                DI_NORMAL,
            ),
            "DrawIconEx white",
        )?;
        check(
            DrawIconEx(
                black.dc,
                0,
                0,
                info.hCursor,
                width,
                height,
                0,
                null_mut(),
                DI_NORMAL,
            ),
            "DrawIconEx black",
        )?;
        GdiFlush();
        let mut image = RgbaImage::new(width as u32, height as u32);
        for ((pixel, w), b) in image
            .pixels_mut()
            .zip(white.bytes().as_chunks::<4>().0)
            .zip(black.bytes().as_chunks::<4>().0)
        {
            let mut delta = [
                i16::from(w[0]) - i16::from(b[0]),
                i16::from(w[1]) - i16::from(b[1]),
                i16::from(w[2]) - i16::from(b[2]),
            ];
            delta.sort_unstable();
            let alpha = (255 - delta[1]).clamp(0, 255) as u8;
            let straight = |value: u8| {
                if alpha == 0 {
                    0
                } else {
                    ((u32::from(value) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255)
                        as u8
                }
            };
            pixel.0 = [straight(b[2]), straight(b[1]), straight(b[0]), alpha];
        }
        Ok(Some(Cursor {
            point: info.ptScreenPos,
            hotspot: (icon.xHotspot as i32, icon.yHotspot as i32),
            image,
        }))
    }
}

/// Moves and resizes the target without changing its z-order or activating it.
fn move_window(hwnd: HWND, bounds: Rect) -> Result<()> {
    // SAFETY: SetWindowPos takes handles and integers by value; failures are reported.
    unsafe {
        check(
            SetWindowPos(
                hwnd,
                null_mut(),
                bounds.left,
                bounds.top,
                bounds.width(),
                bounds.height(),
                SWP_NOZORDER | SWP_NOACTIVATE,
            ),
            "Move target window",
        )
    }
}

/// Refuses an application-constrained size rather than capturing different client dimensions.
fn require_client_size(hwnd: HWND, requested: (u32, u32)) -> Result<()> {
    let actual = client_rect(hwnd)?;
    ensure!(
        actual.width() == requested.0 as i32 && actual.height() == requested.1 as i32,
        "Requested client size {}x{}; actual client size {}x{}. The application constrained or refused the requested size",
        requested.0,
        requested.1,
        actual.width(),
        actual.height()
    );
    Ok(())
}

/// Measures the application's actual frame after each resize; custom chrome can change it.
fn resize_client(hwnd: HWND, requested: (u32, u32), cancelled: &AtomicBool) -> Result<()> {
    for _ in 0..4 {
        ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
        let client = client_rect(hwnd)?;
        if client.width() == requested.0 as i32 && client.height() == requested.1 as i32 {
            return Ok(());
        }
        let window = window_rect(hwnd)?;
        let invalid = || {
            format!(
                "Requested client size {}x{}; actual client size {}x{}. The required outer size is invalid",
                requested.0,
                requested.1,
                client.width(),
                client.height()
            )
        };
        let width = window
            .width()
            .checked_add(requested.0 as i32 - client.width())
            .filter(|&value| value > 0)
            .with_context(invalid)?;
        let height = window
            .height()
            .checked_add(requested.1 as i32 - client.height())
            .filter(|&value| value > 0)
            .with_context(invalid)?;
        // SAFETY: dimensions are positive and checked above; no pointers are passed. Keeping
        // the position avoids moving to another monitor while adjusting the client size.
        unsafe {
            check(
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    0,
                    0,
                    width,
                    height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                ),
                "Resize target client",
            )
            .with_context(|| {
                let actual = client_rect(hwnd).unwrap_or(client);
                format!(
                    "Requested client size {}x{}; actual client size {}x{}",
                    requested.0,
                    requested.1,
                    actual.width(),
                    actual.height()
                )
            })?;
        }
        flush();
        wait(Duration::from_millis(100), cancelled)?;
    }
    require_client_size(hwnd, requested)
}

/// Desktop state recorded before capture and restored in `Drop`.
///
/// `move_back` is set once the capture restores, moves or resizes the target, so an untouched
/// window keeps its placement. `WINDOWPLACEMENT` stores the restored position, which differs
/// from a snapped window's geometry, so a normal window then moves to its original rectangle.
struct Restore {
    target: HWND,
    foreground: HWND,
    placement: WINDOWPLACEMENT,
    original: Rect,
    move_back: bool,
    restore_foreground: bool,
    nonactivating: bool,
    original_dpi: u32,
    original_monitor: usize,
}
impl Restore {
    fn geometry(&mut self) -> Result<()> {
        if !self.move_back {
            return Ok(());
        }
        // SAFETY: the placement was initialized by GetWindowPlacement before any mutation.
        let placed = unsafe {
            ensure!(
                IsWindow(self.target) != 0,
                "Target closed before geometry restoration"
            );
            let mut placement = self.placement;
            if self.nonactivating {
                placement.showCmd = SW_SHOWNOACTIVATE as u32;
            }
            SetWindowPlacement(self.target, &placement) != 0
        };
        if !placed {
            eprintln!("Warning: could not restore the target window placement");
        }
        flush();
        thread::sleep(Duration::from_millis(120));
        if self.placement.showCmd == SW_SHOWNORMAL as u32
            && window_rect(self.target)? != self.original
        {
            move_window(self.target, self.original)?;
            flush();
            thread::sleep(Duration::from_millis(100));
        }
        if self.nonactivating {
            ensure!(
                window_rect(self.target)? == self.original,
                "The application refused its original window geometry during restoration"
            );
            // SAFETY: these read-only queries accept the live HWND by value.
            let actual = unsafe {
                (
                    GetDpiForWindow(self.target),
                    MonitorFromWindow(self.target, MONITOR_DEFAULTTONEAREST) as usize,
                )
            };
            ensure!(
                actual == (self.original_dpi, self.original_monitor),
                "The application did not return to its original monitor and DPI"
            );
            ensure!(placed, "Could not restore original target placement");
        }
        self.move_back = false;
        Ok(())
    }
}
impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(error) = self.geometry() {
            eprintln!("Warning: could not restore target geometry: {error}");
        }
        // SAFETY: both handles pass IsWindow before use; GetWindowPlacement filled `placement`
        // with `length` set.
        unsafe {
            if self.restore_foreground
                && !self.foreground.is_null()
                && IsWindow(self.foreground) != 0
                && GetForegroundWindow() != self.foreground
                && SetForegroundWindow(self.foreground) == 0
            {
                eprintln!("Warning: Windows refused to restore the previous foreground window");
            }
        }
    }
}

/// Returns the capture rectangle: DWM frame bounds plus shadow margins and `--pad`.
///
/// The default shadow style is 2 when focused and 4 when unfocused. Maximized windows draw no
/// exterior shadow and get neither margins nor padding.
fn bounds(hwnd: HWND, options: &CaptureOptions) -> Result<Rect> {
    let ext = extended(hwnd)?;
    if options.no_shadow {
        validate_no_shadow(hwnd, options)?;
        return Ok(ext);
    }
    // SAFETY: IsZoomed takes the handle by value and returns 0 for invalid windows.
    if unsafe { IsZoomed(hwnd) } != 0 {
        return Ok(ext);
    }
    let style = options.shadow_style.unwrap_or({
        if options.focus == FocusState::Focused {
            2
        } else {
            4
        }
    });
    let (l, t, r, b) = if options.pad_mode == PadMode::Shadow {
        // SAFETY: GetDpiForWindow takes the handle by value and returns 0 for invalid windows.
        shadow_margins(style, unsafe { GetDpiForWindow(hwnd) }.max(96))?
    } else {
        (0, 0, 0, 0)
    };
    let pad = options.pad as i32;
    Ok(ext.inflate(l + pad, t + pad, r + pad, b + pad))
}

/// Moves the window until its capture rectangle lies inside `work` and returns that rectangle.
///
/// Outside the workspace the taskbar or the screen edge hides the backdrops. A window larger
/// than the workspace is shrunk and centered with `high_dpi` and rejected otherwise. An explicit
/// client size always rejects shrinking.
/// Applications may react to a move, so each attempt re-measures; four attempts are allowed.
fn fit_workspace(
    hwnd: HWND,
    work: Rect,
    options: &CaptureOptions,
    cancelled: &AtomicBool,
) -> Result<Rect> {
    for _ in 0..4 {
        let capture = if options.backend == CaptureBackend::Wgc {
            extended(hwnd)?
        } else {
            bounds(hwnd, options)?
        };
        if work.contains(capture) {
            return Ok(capture);
        }
        let mut window = window_rect(hwnd)?;
        if capture.width() > work.width() || capture.height() > work.height() {
            if let Some((width, height)) = options.client_size {
                let actual = client_rect(hwnd)?;
                bail!(
                    "Requested client size {width}x{height}; actual client size {}x{}. The client, frame and shadow cannot fit this monitor's workspace; choose a smaller --client-size or a larger display",
                    actual.width(),
                    actual.height()
                );
            }
            ensure!(
                options.high_dpi,
                "The window and its shadow are larger than this monitor's workspace; resize the window or use --high-dpi"
            );
            let width = window.width() - (capture.width() - work.width()).max(0) - 2;
            let height = window.height() - (capture.height() - work.height()).max(0) - 2;
            ensure!(
                width >= 80 && height >= 60,
                "The window and its shadow cannot fit this monitor's workspace"
            );
            window = centered(work, width, height);
        } else {
            let dx = (work.left - capture.left).max(0) - (capture.right - work.right).max(0);
            let dy = (work.top - capture.top).max(0) - (capture.bottom - work.bottom).max(0);
            window = Rect {
                left: window.left + dx,
                top: window.top + dy,
                right: window.right + dx,
                bottom: window.bottom + dy,
            };
        }
        move_window(hwnd, window)?;
        flush();
        wait(Duration::from_millis(100), cancelled)?;
        if let Some(requested) = options.client_size {
            require_client_size(hwnd, requested)?;
        }
    }
    bail!("The application would not fit within the monitor workspace, including its shadow")
}

/// Reports whether at least 95% of the 2 px outer rim, outside the target and menu
/// rectangles, shows the backdrop (all channels >= 238 for white, <= 18 for black).
///
/// A failing rim means another window covers the capture area or DWM has not composed the
/// backdrop yet. Shadows and animated content fill the interior, so only the rim is checked.
fn backdrop_matches(image: &RgbImage, capture: Rect, foreground: &[Rect], white: bool) -> bool {
    let mut matching = 0_u64;
    let mut count = 0_u64;
    for (x, y, pixel) in image.enumerate_pixels() {
        if x >= 2 && y >= 2 && x + 2 < image.width() && y + 2 < image.height() {
            continue;
        }
        let (x, y) = (x as i32 + capture.left, y as i32 + capture.top);
        if foreground.iter().any(|target| {
            x >= target.left && x < target.right && y >= target.top && y < target.bottom
        }) {
            continue;
        }
        matching += u64::from(
            pixel
                .0
                .iter()
                .all(|&c| if white { c >= 238 } else { c <= 18 }),
        );
        count += 1;
    }
    count == 0 || matching * 100 >= count * 95
}

/// Reports whether two frames are identical outside `regions`.
///
/// Carets and video inside the opaque client regions may change; the frame, corners and
/// shadow carry the alpha and must be still.
fn unchanged_outside(a: &RgbImage, b: &RgbImage, regions: &[image::math::Rect]) -> bool {
    a.enumerate_pixels()
        .zip(b.pixels())
        .all(|((x, y, pa), pb)| {
            pa == pb
                || regions
                    .iter()
                    .any(|r| x >= r.x && x - r.x < r.width && y >= r.y && y - r.y < r.height)
        })
}

/// Returns the client area minus the frame's corner squares (16 px at 96 DPI), clipped to the
/// capture, as two overlapping rectangles in capture pixels.
///
/// The solver treats these pixels as opaque, which removes noise from content that changed
/// between passes. Rounded corners are translucent even under custom client chrome, so the
/// corner squares stay in the alpha solve.
fn opaque_regions(hwnd: HWND, frame: Rect, capture: Rect) -> Result<Vec<image::math::Rect>> {
    let client = client_rect(hwnd)?;
    // SAFETY: GetDpiForWindow takes the handle by value and returns 0 for invalid windows.
    let radius = (16 * unsafe { GetDpiForWindow(hwnd) }.max(96) / 96) as i32;
    let mut regions = Vec::new();
    for safe in [
        frame.inflate(-radius, 0, -radius, 0),
        frame.inflate(0, -radius, 0, -radius),
    ] {
        let left = client.left.max(safe.left).max(capture.left);
        let top = client.top.max(safe.top).max(capture.top);
        let right = client.right.min(safe.right).min(capture.right);
        let bottom = client.bottom.min(safe.bottom).min(capture.bottom);
        if right > left && bottom > top {
            regions.push(image::math::Rect {
                x: (left - capture.left) as u32,
                y: (top - capture.top) as u32,
                width: (right - left) as u32,
                height: (bottom - top) as u32,
            });
        }
    }
    Ok(regions)
}

/// Captures window `handle` over a white and a black backdrop; the module documentation lists
/// the steps.
///
/// Placement and foreground are restored before return, also on error and on `cancelled`.
pub fn capture(
    handle: usize,
    options: &CaptureOptions,
    cancelled: &AtomicBool,
) -> Result<Composites> {
    ensure!(
        options.backend == CaptureBackend::Backdrop,
        "The WGC backend returns native RGBA; use capture_wgc instead of backdrop composites"
    );
    enable_per_monitor_dpi();
    ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
    ensure!(
        options.high_dpi_display.is_none() || options.high_dpi,
        "A high-DPI monitor requires high_dpi to be enabled"
    );
    ensure!(
        options.pad <= 4096 && options.settle_ms <= 60_000,
        "Padding or settle time is too large"
    );
    ensure!(
        options.shadow_style.is_none_or(|style| style <= 5),
        "Shadow style must be between 0 and 5"
    );
    ensure!(
        options.client_size.is_none_or(|(width, height)| width > 0
            && height > 0
            && u64::from(width) * u64::from(height) <= 40_000_000),
        "Client size must have positive dimensions and at most 40 million pixels"
    );
    let hwnd = handle as HWND;
    let mut info = window_info(handle)?;
    ensure!(
        // SAFETY: IsWindowVisible takes the handle by value and returns 0 for invalid windows.
        unsafe { IsWindowVisible(hwnd) } != 0 && !cloaked(hwnd),
        "Target must be a visible window on the current desktop"
    );
    ensure!(
        top_level(hwnd) == hwnd,
        "Target 0x{handle:X} is a child control; capture its top-level window 0x{:X} instead",
        top_level(hwnd) as usize
    );
    validate_no_shadow(hwnd, options)?;
    let menus = open_menus(hwnd)?;
    ensure!(
        menus.is_empty() || options.focus == FocusState::Focused,
        "Open menus require focused capture; close the menu before using --focus-state unfocused"
    );
    ensure!(
        menus.is_empty() || !options.high_dpi,
        "Move the window to the desired monitor before opening its menu; --high-dpi would dismiss the open menu"
    );
    ensure!(
        menus.is_empty() || options.client_size.is_none(),
        "Resize the window before opening its menu; --client-size would dismiss the open menu"
    );
    let destination = if options.high_dpi {
        Some(high_dpi_display(&list_monitors()?, options.high_dpi_display.as_deref())?.clone())
    } else {
        None
    };
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        // SAFETY: WINDOWPLACEMENT holds integers, POINTs and a RECT; all-zero is valid.
        ..unsafe { zeroed() }
    };
    // SAFETY: `placement` is a live WINDOWPLACEMENT with `length` set as the API requires.
    unsafe {
        check(
            GetWindowPlacement(hwnd, &mut placement),
            "GetWindowPlacement",
        )?;
    }
    let mut restore = Restore {
        target: hwnd,
        // SAFETY: GetForegroundWindow takes no arguments; Restore checks a null result.
        foreground: unsafe { GetForegroundWindow() },
        placement,
        original: info.rect,
        // SAFETY: IsIconic takes the handle by value and returns 0 for invalid windows.
        move_back: options.no_shadow || unsafe { IsIconic(hwnd) } != 0,
        restore_foreground: options.restore_foreground,
        nonactivating: false,
        original_dpi: info.dpi,
        original_monitor: 0,
    };
    if options.restore_foreground || options.focus == FocusState::Unfocused {
        // Grant this process foreground rights before the target takes focus; restoring the
        // foreground and activating the sink need them.
        // SAFETY: AllowSetForegroundWindow takes a process id by value.
        unsafe {
            AllowSetForegroundWindow(std::process::id());
        }
    }
    let white = Overlay::new(0x00ffffff, false)?;
    let black = Overlay::new(0x00000000, false)?;
    let sink = if options.focus == FocusState::Unfocused {
        Some(Overlay::new(0, true)?)
    } else {
        None
    };
    let mut workspace = None;
    if let Some(monitor) = destination {
        restore.move_back = true;
        // SAFETY: ShowWindow takes the handle by value and fails softly for invalid windows.
        unsafe {
            ShowWindow(
                hwnd,
                if options.client_size.is_some() {
                    SW_SHOWNORMAL
                } else {
                    SW_RESTORE
                },
            );
        }
        wait(Duration::from_millis(60), cancelled)?;
        let current = window_rect(hwnd)?;
        move_window(
            hwnd,
            centered(monitor.work, current.width(), current.height()),
        )?;
        flush();
        wait(Duration::from_millis(150), cancelled)?;
        // SAFETY: GetDpiForWindow takes the handle by value and returns 0 for invalid windows.
        let actual_dpi = unsafe { GetDpiForWindow(hwnd) };
        ensure!(
            actual_dpi == monitor.dpi,
            "The application reports {actual_dpi} DPI on a {}-DPI display. High-DPI capture requires a per-monitor DPI-aware application; disable --high-dpi for this window",
            monitor.dpi
        );
        eprintln!(
            "Capturing on {} ({}) at {} DPI",
            monitor.name,
            monitor.description.escape_debug(),
            monitor.dpi
        );
        workspace = Some(monitor.work);
    }
    // SAFETY: both state checks take the handle by value.
    if unsafe { IsIconic(hwnd) != 0 || (options.client_size.is_some() && IsZoomed(hwnd) != 0) } {
        restore.move_back = true;
        // SAFETY: ShowWindow takes the handle by value and fails softly.
        unsafe {
            ShowWindow(
                hwnd,
                if options.client_size.is_some() {
                    SW_SHOWNORMAL
                } else {
                    SW_RESTORE
                },
            );
        }
        flush();
        wait(Duration::from_millis(60), cancelled)?;
    }
    if let Some(requested) = options.client_size {
        restore.move_back = true;
        resize_client(hwnd, requested, cancelled)?;
    }
    // Activate only a background target; activating an active menu owner can cancel menu
    // tracking.
    // SAFETY: GetForegroundWindow takes no arguments; a null result has a null root.
    if root(unsafe { GetForegroundWindow() }) != root(hwnd) {
        ensure!(
            menus.is_empty(),
            "Keep the target and its open menu in the foreground while capturing"
        );
        // SAFETY: both calls take the handle by value; a refused activation is detected by
        // the focus check below.
        unsafe {
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
        }
        // Some activation animations, such as Windows Terminal's caption buttons, pause between
        // frames; the stability check would accept such a frame, so activation waits 80 ms.
        wait(Duration::from_millis(80), cancelled)?;
    }
    if let Some(sink) = &sink {
        // SAFETY: `sink.hwnd` is a live window of this thread.
        unsafe {
            ShowWindow(sink.hwnd, SW_SHOW);
            SetForegroundWindow(sink.hwnd);
        }
    }
    flush();
    // SAFETY: GetForegroundWindow takes no arguments; a null result has a null root.
    let focused = root(unsafe { GetForegroundWindow() }) == root(hwnd);
    ensure!(
        focused == (options.focus == FocusState::Focused),
        "Windows refused the requested focus state; activate the target and retry"
    );
    let mut capture_bounds = bounds(hwnd, options)?;
    if menus.is_empty() {
        let work = if let Some(work) = workspace {
            work
        } else {
            // SAFETY: MonitorFromWindow takes the handle by value and, with
            // MONITOR_DEFAULTTONEAREST, always returns a monitor.
            let handle = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) } as usize;
            list_monitors()?
                .into_iter()
                .find(|m| m.handle == handle)
                .context("The target monitor is no longer active")?
                .work
        };
        if !work.contains(capture_bounds) {
            restore.move_back = true;
            capture_bounds = fit_workspace(hwnd, work, options, cancelled)?;
        }
    }
    if let Some(requested) = options.client_size {
        require_client_size(hwnd, requested)?;
    }
    let target = extended(hwnd)?;
    let mut foreground = vec![target];
    for &(menu, rect) in &menus {
        foreground.push(rect);
        let (l, t, r, b) = if options.pad_mode == PadMode::Shadow {
            // SAFETY: GetDpiForWindow takes the handle by value and returns 0 for a closed menu.
            shadow_margins(1, unsafe { GetDpiForWindow(menu as HWND) }.max(96))?
        } else {
            (0, 0, 0, 0)
        };
        let pad = options.pad as i32;
        capture_bounds = capture_bounds.union(rect.inflate(l + pad, t + pad, r + pad, b + pad));
    }
    let screen = Rect {
        // SAFETY: GetSystemMetrics takes a metric index by value.
        left: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        // SAFETY: GetSystemMetrics takes a metric index by value.
        top: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        // SAFETY: GetSystemMetrics takes a metric index by value.
        right: unsafe {
            GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN)
        },
        // SAFETY: GetSystemMetrics takes a metric index by value.
        bottom: unsafe {
            GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN)
        },
    };
    ensure!(
        screen.contains(capture_bounds),
        "The window or its open menu extends off-screen; close the menu so the window can be nudged inward, or use --high-dpi"
    );
    // SAFETY: GetDpiForWindow takes the handle by value and returns 0 for invalid windows.
    info.dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let shadow_policy = validate_no_shadow(hwnd, options)?;
    let cursor = if options.include_cursor {
        cursor()?
    } else {
        None
    };
    let opaque_regions = if options.no_shadow {
        Vec::new()
    } else {
        opaque_regions(hwnd, target, capture_bounds)?
    };
    let mut images = Vec::with_capacity(2);
    for (overlay, white_pass) in [(&white, true), (&black, false)] {
        if white_pass {
            black.hide();
        } else {
            white.hide();
        }
        ensure!(
            extended(hwnd)? == target && open_menus(hwnd)? == menus,
            "Target or its open menus moved, resized, opened or closed; retry with a stationary window and menu"
        );
        if let Some(requested) = options.client_size {
            require_client_size(hwnd, requested)?;
        }
        overlay.place(hwnd, capture_bounds)?;
        let started = Instant::now();
        let settled = started + Duration::from_millis(options.settle_ms);
        let mut previous: Option<RgbImage> = None;
        let image = loop {
            flush();
            ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
            let probe = screenshot(capture_bounds)?;
            let now = Instant::now();
            if !backdrop_matches(&probe, capture_bounds, &foreground, white_pass) {
                ensure!(
                    now < settled + Duration::from_secs(1),
                    "Backdrop verification failed; remove windows overlapping the capture area"
                );
                overlay.place(hwnd, capture_bounds)?;
                previous = None;
                continue;
            }
            let stable = previous
                .as_ref()
                .is_some_and(|old| unchanged_outside(old, &probe, &opaque_regions));
            if stable || now >= settled {
                if !stable && options.settle_ms > 0 {
                    eprintln!(
                        "Warning: the window was still changing after {} ms; raise --settle-ms if its frame or shadow shows artifacts",
                        options.settle_ms
                    );
                }
                break probe;
            }
            previous = Some(probe);
        };
        ensure!(
            extended(hwnd)? == target && open_menus(hwnd)? == menus,
            "Target or its open menus changed while capturing; retry with a stationary window and menu"
        );
        if let Some(requested) = options.client_size {
            require_client_size(hwnd, requested)?;
        }
        overlay.hide();
        flush();
        images.push(image);
    }
    let black = images.pop().context("Missing black composite")?;
    let white = images.pop().context("Missing white composite")?;
    ensure!(
        validate_no_shadow(hwnd, options)? == shadow_policy,
        "Target shadow or composition policy changed while capturing; retry with a stationary window"
    );
    let layers = cursor_layers(cursor, capture_bounds, options.cursor_shadow);
    Ok(Composites {
        blend_space: shadow_policy.blend_space,
        corner_radius: shadow_policy.corner_radius,
        white,
        black,
        window: info,
        opaque_regions,
        cursor: layers,
    })
}

fn cursor_layers(
    cursor: Option<Cursor>,
    capture_bounds: Rect,
    policy: CursorShadow,
) -> Vec<crate::alpha::Overlay> {
    let mut layers = Vec::new();
    if let Some(cursor) = cursor
        && cursor.point.x >= capture_bounds.left
        && cursor.point.x < capture_bounds.right
        && cursor.point.y >= capture_bounds.top
        && cursor.point.y < capture_bounds.bottom
    {
        let x = cursor.point.x - cursor.hotspot.0 - capture_bounds.left;
        let y = cursor.point.y - cursor.hotspot.1 - capture_bounds.top;
        let mut shadow_enabled = 0_i32;
        // SAFETY: SPI_GETCURSORSHADOW writes a BOOL to the live i32 output value.
        unsafe {
            SystemParametersInfoW(
                SPI_GETCURSORSHADOW,
                0,
                (&mut shadow_enabled as *mut i32).cast(),
                0,
            );
        }
        if policy == CursorShadow::On || (policy == CursorShadow::Auto && shadow_enabled != 0) {
            layers.push(crate::alpha::Overlay {
                image: cursor_shadow(&cursor.image),
                x: x - 3,
                y: y - 3,
            });
        }
        layers.push(crate::alpha::Overlay {
            image: cursor.image,
            x,
            y,
        });
    }
    layers
}

fn wgc_monitor_move(current: usize, dpi: u32, desired: &MonitorInfo, named: bool) -> bool {
    current != desired.handle && (named || dpi < desired.dpi)
}

fn scaled_wgc_rect(work: Rect, original: Rect, from_dpi: u32, to_dpi: u32) -> Result<Rect> {
    ensure!(
        from_dpi > 0 && to_dpi > 0,
        "Cannot scale a window with unknown DPI"
    );
    let ratio = f64::from(to_dpi) / f64::from(from_dpi);
    let width = (f64::from(original.width()) * ratio).round_ties_even();
    let height = (f64::from(original.height()) * ratio).round_ties_even();
    ensure!(
        width > 0.0
            && height > 0.0
            && width <= f64::from(i32::MAX)
            && height <= f64::from(i32::MAX),
        "Scaled window dimensions are invalid"
    );
    Ok(centered(work, width as i32, height as i32))
}

/// Arms nonactivating restoration before the first geometry mutation; no guard for the fast path.
fn prepare_wgc_geometry(
    hwnd: HWND,
    window: &WindowInfo,
    options: &CaptureOptions,
    cancelled: &AtomicBool,
) -> Result<Option<Restore>> {
    let resize = if let Some(requested) = options.client_size {
        let client = client_rect(hwnd)?;
        (client.width(), client.height()) != (requested.0 as i32, requested.1 as i32)
    } else {
        false
    };
    if !options.high_dpi && !resize {
        return Ok(None);
    }
    let monitors = list_monitors()?;
    // SAFETY: MonitorFromWindow only reads the current monitor association.
    let current_handle = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) } as usize;
    let current = monitors
        .iter()
        .find(|m| m.handle == current_handle)
        .context("Target monitor is no longer active")?;
    let desired = if options.high_dpi {
        high_dpi_display(&monitors, options.high_dpi_display.as_deref())?
    } else {
        current
    };
    let relocate = options.high_dpi
        && wgc_monitor_move(
            current_handle,
            window.dpi,
            desired,
            options.high_dpi_display.is_some(),
        );
    if options.high_dpi && !relocate {
        ensure!(
            window.dpi == current.dpi && window.dpi >= desired.dpi,
            "The application reports {} DPI on a {}-DPI display; high-DPI capture requires a per-monitor DPI-aware application",
            window.dpi,
            current.dpi
        );
    }
    if !relocate && !resize {
        eprintln!(
            "Capturing on {} ({}) at {} DPI",
            current.name,
            current.description.escape_debug(),
            current.dpi
        );
        return Ok(None);
    }
    ensure!(
        open_menus(hwnd)?.is_empty(),
        "Close the open menu before WGC moves or resizes its owner"
    );
    // SAFETY: zero is valid for WINDOWPLACEMENT; Win32 fills the initialized output below.
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..unsafe { zeroed() }
    };
    // SAFETY: placement is a live output with the required length initialized.
    unsafe {
        check(
            GetWindowPlacement(hwnd, &mut placement),
            "Get target placement",
        )?;
        ensure!(
            placement.showCmd == SW_SHOWNORMAL as u32
                && IsZoomed(hwnd) == 0
                && IsWindowArranged(hwnd) == 0,
            "WGC cannot temporarily move or resize a maximized or snapped window without changing its state; restore the window first"
        );
    }
    let destination = if relocate {
        Some(scaled_wgc_rect(
            desired.work,
            window.rect,
            window.dpi,
            desired.dpi,
        )?)
    } else {
        None
    };
    ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
    let restore = Restore {
        target: hwnd,
        foreground: null_mut(),
        placement,
        original: window.rect,
        move_back: true,
        restore_foreground: false,
        nonactivating: true,
        original_dpi: window.dpi,
        original_monitor: current_handle,
    };
    if let Some(destination) = destination {
        move_window(hwnd, destination)?;
        flush();
        wait(Duration::from_millis(150), cancelled)?;
        // SAFETY: these are read-only queries of the target after its DPI transition.
        let (actual_dpi, actual_monitor) = unsafe {
            (
                GetDpiForWindow(hwnd),
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) as usize,
            )
        };
        ensure!(
            actual_dpi == desired.dpi && actual_monitor == desired.handle,
            "The application reports {actual_dpi} DPI after moving to {} at {} DPI; high-DPI capture requires a per-monitor DPI-aware application",
            desired.name,
            desired.dpi
        );
    }
    if let Some(requested) = options.client_size {
        resize_client(hwnd, requested, cancelled)?;
    }
    if relocate {
        let work = desired.work;
        let frame = fit_workspace(hwnd, work, options, cancelled)?;
        let center = centered(work, frame.width(), frame.height());
        let outer = window_rect(hwnd)?;
        let (dx, dy) = (center.left - frame.left, center.top - frame.top);
        if dx != 0 || dy != 0 {
            move_window(
                hwnd,
                Rect {
                    left: outer.left + dx,
                    top: outer.top + dy,
                    right: outer.right + dx,
                    bottom: outer.bottom + dy,
                },
            )?;
            flush();
            wait(Duration::from_millis(100), cancelled)?;
        }
        ensure!(
            work.contains(extended(hwnd)?),
            "The application would not fit within the WGC monitor workspace"
        );
    }
    if let Some(requested) = options.client_size {
        require_client_size(hwnd, requested)?;
    }
    if options.high_dpi {
        // SAFETY: the target was remeasured after every geometry adjustment.
        let actual = unsafe {
            (
                GetDpiForWindow(hwnd),
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) as usize,
            )
        };
        ensure!(
            actual
                == (
                    desired.dpi,
                    if relocate {
                        desired.handle
                    } else {
                        current_handle
                    }
                ),
            "The application changed its capture monitor or DPI while resizing"
        );
        let monitor = if relocate { desired } else { current };
        eprintln!(
            "Capturing on {} ({}) at {} DPI",
            monitor.name,
            monitor.description.escape_debug(),
            monitor.dpi
        );
    }
    Ok(Some(restore))
}

/// Captures native straight RGBA, keeping ordinary WGC stationary and nonactivating.
/// Reuse `source` on its creating thread for repeated captures. Focus options apply to backdrop
/// capture; WGC keeps the current appearance. Padding adds transparent pixels, not a backdrop.
///
/// # Errors
///
/// Fails for unavailable/protected targets, geometry changes, incompatible current DPI/client
/// dimensions, unsupported shadow removal, cancellation, and bounded WGC frame/readback errors.
pub fn capture_wgc(
    source: &snapshot::Snapshotter,
    handle: usize,
    options: &CaptureOptions,
    cancelled: &AtomicBool,
) -> Result<CapturedImage> {
    enable_per_monitor_dpi();
    ensure!(
        options.backend == CaptureBackend::Wgc,
        "capture_wgc requires the WGC backend"
    );
    ensure!(!cancelled.load(Ordering::Relaxed), "Capture cancelled");
    ensure!(
        options.high_dpi_display.is_none() || options.high_dpi,
        "A high-DPI monitor requires high_dpi to be enabled"
    );
    ensure!(options.pad <= 4096, "Padding is too large");
    ensure!(
        options.client_size.is_none_or(|(width, height)| width > 0
            && height > 0
            && u64::from(width) * u64::from(height) <= 40_000_000),
        "Client size must have positive dimensions and at most 40 million pixels"
    );
    let hwnd = handle as HWND;
    let window = window_info(handle)?;
    ensure!(
        // SAFETY: these predicates accept an HWND by value and read window state only.
        unsafe { IsWindowVisible(hwnd) } != 0 && unsafe { IsIconic(hwnd) } == 0 && !cloaked(hwnd),
        "WGC target must be visible, not minimized, and on the current desktop"
    );
    ensure!(top_level(hwnd) == hwnd, "WGC requires a top-level window");
    validate_no_shadow(hwnd, options)?;
    let mut restore = prepare_wgc_geometry(hwnd, &window, options, cancelled)?;
    let window = if restore.is_some() {
        window_info(handle)?
    } else {
        window
    };
    let shadow_policy = validate_no_shadow(hwnd, options)?;
    let bounds = extended(hwnd)?;
    ensure!(
        bounds.width() > 0 && bounds.height() > 0,
        "WGC target frame is empty"
    );
    let cursor = if options.include_cursor {
        cursor()?
    } else {
        None
    };
    let mut image = source.capture(handle, || cancelled.load(Ordering::Relaxed))?;
    ensure!(
        window_rect(hwnd)? == window.rect && extended(hwnd)? == bounds,
        "Target moved or resized during WGC capture; retry with a stationary window"
    );
    ensure!(
        image.dimensions() == (bounds.width() as u32, bounds.height() as u32),
        "WGC surface size does not match the target's frame bounds; this window geometry is unsupported"
    );
    ensure!(
        validate_no_shadow(hwnd, options)? == shadow_policy,
        "Target shadow policy changed during WGC capture; retry with a stationary window"
    );
    if shadow_policy.corner_radius > 0.0 {
        image = crate::alpha::finish(
            image,
            &crate::alpha::AlphaOptions {
                crop: false,
                corner_radius: shadow_policy.corner_radius,
                ..Default::default()
            },
        )?
        .image;
    }
    let pad = if options.no_shadow { 0 } else { options.pad };
    if pad > 0 {
        let width = image.width() + 2 * pad;
        let height = image.height() + 2 * pad;
        ensure!(
            u64::from(width) * u64::from(height) <= 40_000_000,
            "Padded WGC canvas exceeds 40 million pixels"
        );
        let mut padded = RgbaImage::new(width, height);
        image::imageops::replace(&mut padded, &image, i64::from(pad), i64::from(pad));
        image = padded;
    }
    let cursor = cursor_layers(
        cursor,
        bounds.inflate(pad as i32, pad as i32, pad as i32, pad as i32),
        options.cursor_shadow,
    );
    if let Some(restore) = &mut restore {
        restore.geometry()?;
    }
    Ok(CapturedImage {
        image,
        window,
        cursor,
    })
}

#[cfg(test)]
#[path = "shadow_tests.rs"]
mod shadow_tests;

#[cfg(test)]
#[path = "wgc_geometry_tests.rs"]
mod wgc_geometry_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn wgc_geometry_plans_only_needed_monitor_moves_and_scales_physical_pixels() {
        let work = Rect {
            left: 2000,
            top: 0,
            right: 5840,
            bottom: 2160,
        };
        let desired = MonitorInfo {
            handle: 2,
            name: "Virtual".into(),
            description: String::new(),
            rect: work,
            work,
            dpi: 192,
        };
        assert!(wgc_monitor_move(1, 96, &desired, false));
        assert!(!wgc_monitor_move(1, 192, &desired, false));
        assert!(!wgc_monitor_move(1, 288, &desired, false));
        assert!(wgc_monitor_move(1, 192, &desired, true));
        assert!(!wgc_monitor_move(2, 192, &desired, true));
        assert!(!wgc_monitor_move(2, 96, &desired, false));
        let original = Rect {
            left: 10,
            top: 10,
            right: 810,
            bottom: 610,
        };
        let scaled = scaled_wgc_rect(work, original, 96, 192).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (1600, 1200));
        assert_eq!((scaled.left, scaled.top), (3120, 480));
        let fit = scaled_wgc_rect(work, original, 96, 480).unwrap();
        assert_eq!((fit.width(), fit.height()), (2880, 2160));
        assert!(work.contains(fit));
        assert!(scaled_wgc_rect(work, original, 0, 192).is_err());
        assert!(scaled_wgc_rect(work, original, 96, 0).is_err());
    }

    #[test]
    fn native_corner_selection_covers_policy_dpi_and_square_states() {
        for dpi in [96, 120, 144, 192] {
            for (preference, radius) in [
                (DWMWCP_DEFAULT, 8.0),
                (DWMWCP_ROUND, 8.0),
                (DWMWCP_ROUNDSMALL, 4.0),
                (DWMWCP_DONOTROUND, 0.0),
            ] {
                assert_eq!(
                    standard_corner_radius(Some(preference), 26200, dpi, false).unwrap(),
                    radius * dpi as f32 / 96.0
                );
                assert_eq!(
                    standard_corner_radius(Some(preference), 26200, dpi, true).unwrap(),
                    0.0
                );
                assert_eq!(
                    standard_corner_radius(Some(preference), 19045, dpi, false).unwrap(),
                    0.0
                );
            }
        }
        assert_eq!(standard_corner_radius(None, 19045, 96, false).unwrap(), 0.0);
        for (preference, build, dpi) in [
            (None, 26200, 96),
            (Some(99), 26200, 96),
            (Some(DWMWCP_DEFAULT), 0, 96),
            (Some(DWMWCP_DEFAULT), 26200, 0),
        ] {
            assert!(standard_corner_radius(preference, build, dpi, false).is_err());
        }
    }

    #[test]
    fn native_frame_gate_excludes_full_client_custom_chrome_and_missing_style_flags() {
        let frame = Rect {
            left: 0,
            top: 0,
            right: 320,
            bottom: 220,
        };
        let client = Rect {
            left: 1,
            top: 23,
            right: 319,
            bottom: 219,
        };
        assert!(native_frame(WS_OVERLAPPEDWINDOW, frame, client, 23));
        assert!(native_frame(
            WS_OVERLAPPEDWINDOW & !WS_THICKFRAME,
            frame,
            client,
            23
        ));
        assert!(!native_frame(WS_OVERLAPPEDWINDOW, frame, frame, 23));
        for style in [
            WS_POPUP | WS_THICKFRAME,
            WS_OVERLAPPEDWINDOW & !WS_DLGFRAME,
            WS_OVERLAPPEDWINDOW | WS_CHILD,
        ] {
            assert!(!native_frame(style, frame, client, 23));
        }
        assert!(!native_frame(WS_OVERLAPPEDWINDOW, frame, client, 0));
    }
    #[test]
    fn backdrop_entrypoint_never_falls_back_for_wgc_options() {
        let error = capture(
            0,
            &CaptureOptions {
                backend: CaptureBackend::Wgc,
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("use capture_wgc"));
    }

    #[test]
    fn backdrop_policy_matches_measured_sdr_shadow() {
        let policy = validate_no_shadow(null_mut(), &CaptureOptions::default()).unwrap();
        let image = crate::alpha::recover(
            &RgbImage::from_pixel(1, 1, image::Rgb([199; 3])),
            &RgbImage::new(1, 1),
            &crate::alpha::AlphaOptions {
                blend_space: policy.blend_space,
                ..Default::default()
            },
        )
        .unwrap()
        .image;
        assert_eq!(image[(0, 0)].0, [0, 0, 0, 56]);
    }

    #[test]
    fn no_shadow_maximized_support_accounts_for_backend_and_window_normalization() {
        let mut options = CaptureOptions {
            no_shadow: true,
            ..Default::default()
        };
        assert!(!shadow_free_maximized(false, &options));
        assert!(shadow_free_maximized(true, &options));
        options.client_size = Some((800, 600));
        assert!(!shadow_free_maximized(true, &options));
        options.client_size = None;
        options.high_dpi = true;
        assert!(!shadow_free_maximized(true, &options));
        options.backend = CaptureBackend::Wgc;
        assert!(shadow_free_maximized(true, &options));
        options.client_size = Some((800, 600));
        assert!(shadow_free_maximized(true, &options));
        assert!(!shadow_free_maximized(false, &options));
        assert_eq!(
            validate_no_shadow(null_mut(), &CaptureOptions::default()).unwrap(),
            ShadowPolicy::default()
        );
    }

    #[test]
    fn client_size_validation_rejects_invalid_dimensions_before_accessing_the_target() {
        assert_eq!(CaptureOptions::default().client_size, None);
        for requested in [(0, 1), (1, 0), (8000, 5001), (u32::MAX, 1), (1, u32::MAX)] {
            let error = capture(
                0,
                &CaptureOptions {
                    client_size: Some(requested),
                    ..Default::default()
                },
                &AtomicBool::new(false),
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains("Client size"), "{error}");
        }
        let error = capture(
            0,
            &CaptureOptions {
                client_size: Some((40_000_000, 1)),
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("Invalid window handle"),
            "{error}"
        );
    }

    #[test]
    #[ignore = "Needs ALPHASHOT_TEST_RESTRICTED_HWND: an existing elevated window; only places an off-screen test backdrop"]
    fn backdrop_placement_handles_restricted_insert_after_windows() {
        enable_per_monitor_dpi();
        let target = std::env::var("ALPHASHOT_TEST_RESTRICTED_HWND")
            .expect("Set ALPHASHOT_TEST_RESTRICTED_HWND to the decimal window handle")
            .parse::<usize>()
            .unwrap() as HWND;
        let bounds = Rect {
            left: -32000,
            top: -32000,
            right: -31999,
            bottom: -31999,
        };
        let overlay = Overlay::new(0xffffff, false).unwrap();
        // SAFETY: `target` is a tester-supplied handle passed by value; `overlay.hwnd` lives
        // until `overlay` drops, and the RECTs are live locals.
        unsafe {
            let foreground = GetForegroundWindow();
            let mut original = RECT::default();
            assert_ne!(GetWindowRect(target, &mut original), 0);
            assert_eq!(
                SetWindowPos(
                    overlay.hwnd,
                    target,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE
                ),
                0
            );
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(5));
            overlay.place(target, bounds).unwrap();
            assert!(window_above(target, overlay.hwnd));
            assert!(!window_above(overlay.hwnd, target));
            assert_ne!(IsWindowVisible(overlay.hwnd), 0);
            assert_eq!(GetForegroundWindow(), foreground);
            let mut after = RECT::default();
            assert_ne!(GetWindowRect(target, &mut after), 0);
            assert_eq!(rect(after), rect(original));
        }
        let other = Overlay::new(0, false).unwrap();
        other.place(HWND_TOPMOST, bounds).unwrap();
        assert!(window_above(other.hwnd, overlay.hwnd));
        assert!(!window_above(overlay.hwnd, other.hwnd));
        other.place(overlay.hwnd, bounds).unwrap();
        assert!(window_above(overlay.hwnd, other.hwnd));
    }

    #[test]
    fn process_names_use_the_executable_basename_and_allow_inaccessible_processes() {
        let expected = std::env::current_exe().unwrap();
        assert_eq!(
            process_name(std::process::id()).as_deref(),
            expected.file_name().and_then(|name| name.to_str())
        );
        assert!(process_name(0).is_none());
    }

    #[test]
    fn backdrop_verification_excludes_menu_pixels_but_checks_the_background() {
        let capture = Rect {
            left: -100,
            top: 20,
            right: 0,
            bottom: 80,
        };
        let parent = Rect {
            left: -90,
            top: 30,
            right: -60,
            bottom: 70,
        };
        let menu = Rect {
            left: -80,
            top: 40,
            right: 0,
            bottom: 65,
        };
        for white in [true, false] {
            let background = if white { 255 } else { 0 };
            let mut image = RgbImage::from_pixel(100, 60, Rgb([background; 3]));
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                let (x, y) = (x as i32 + capture.left, y as i32 + capture.top);
                if [parent, menu]
                    .iter()
                    .any(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
                {
                    *pixel = Rgb([80, 120, 180]);
                }
            }
            assert!(!backdrop_matches(&image, capture, &[parent], white));
            assert!(backdrop_matches(&image, capture, &[parent, menu], white));
            for y in 4..56 {
                for x in 4..96 {
                    image.put_pixel(x, y, Rgb([100; 3]));
                }
            }
            assert!(
                backdrop_matches(&image, capture, &[parent, menu], white),
                "Shadow and animated interior pixels must not fail backdrop verification"
            );
            for y in 0..60 {
                for x in 0..10 {
                    image.put_pixel(x, y, Rgb([255 - background; 3]));
                }
            }
            assert!(!backdrop_matches(&image, capture, &[parent, menu], white));
        }
    }
}
