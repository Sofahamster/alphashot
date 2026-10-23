//! Opt-in desktop tests of WGC capture: alpha conversion, device lifetime and the public capture paths.
#![allow(clippy::undocumented_unsafe_blocks)]

use super::*;
use crate::capture;
use std::{
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{HWND, POINT, SIZE},
    Graphics::{
        Dwm::{
            DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE, DwmGetWindowAttribute,
            DwmSetWindowAttribute,
        },
        Gdi::*,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

#[test]
fn native_premultiplied_bytes_convert_once_to_straight_rgba() {
    for (premultiplied, straight) in [
        ([255, 40, 10, 0], [0; 4]),
        ([50, 25, 10, 64], [199, 100, 40, 64]),
        ([100, 50, 20, 128], [199, 100, 40, 128]),
        ([200, 100, 40, 255], [200, 100, 40, 255]),
        ([2, 1, 0, 1], [255, 255, 0, 1]),
    ] {
        assert_eq!(unpremultiply(premultiplied), straight);
    }
}

unsafe extern "system" fn fixture_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe {
        if message == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let color = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if color == -1 {
                for y in paint.rcPaint.top..paint.rcPaint.bottom {
                    let brush =
                        CreateSolidBrush(0x00aa0000 | ((y as u32 & 255) << 8) | (y as u32 >> 8));
                    let row = windows_sys::Win32::Foundation::RECT {
                        top: y,
                        bottom: y + 1,
                        ..paint.rcPaint
                    };
                    FillRect(dc, &row, brush);
                    DeleteObject(brush);
                }
            } else {
                let brush = CreateSolidBrush(color as u32);
                FillRect(dc, &paint.rcPaint, brush);
                DeleteObject(brush);
            }
            EndPaint(hwnd, &paint);
            return 0;
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

struct Fixture {
    hwnd: HWND,
    class: Vec<u16>,
}
impl Fixture {
    fn new(name: &str, style: u32, layered: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let class: Vec<u16> = format!(
            "AlphashotWgcProbe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
        .encode_utf16()
        .chain([0])
        .collect();
        let title: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let bounds = capture::centered(capture::list_monitors().unwrap()[0].work, 320, 220);
        unsafe {
            let instance = GetModuleHandleW(null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(fixture_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | if layered { WS_EX_LAYERED } else { 0 },
                class.as_ptr(),
                title.as_ptr(),
                style,
                bounds.left,
                bounds.top,
                bounds.width(),
                bounds.height(),
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!hwnd.is_null());
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0x00d28c46);
            if !layered {
                let round = 2u32;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                    (&round as *const u32).cast(),
                    size_of::<u32>() as u32,
                );
            }
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            if !layered {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                UpdateWindow(hwnd);
            }
            Self { hwnd, class }
        }
    }

    fn color(&self, color: u32) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, color as isize);
            InvalidateRect(self.hwnd, null(), 0);
            UpdateWindow(self.hwnd);
        }
    }

    fn layered(&self) {
        unsafe {
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            let header = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 320,
                    biHeight: -220,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = null_mut();
            let bitmap = CreateDIBSection(dc, &header, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!bitmap.is_null());
            let previous = SelectObject(dc, bitmap);
            let data = std::slice::from_raw_parts_mut(bits.cast::<u8>(), 320 * 220 * 4);
            for (i, pixel) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (i % 320, i / 320);
                let alpha = if (20..28).contains(&y) {
                    [0u16, 7, 31, 64, 96, 128, 191, 255][y - 20]
                } else if x < 8 && y < 8 {
                    0
                } else {
                    [0u16, 64, 128, 255][x / 80]
                };
                pixel.copy_from_slice(&[
                    (40 * alpha / 255) as u8,
                    (100 * alpha / 255) as u8,
                    (200 * alpha / 255) as u8,
                    alpha as u8,
                ]);
            }
            let origin = POINT { x: 0, y: 0 };
            let size = SIZE { cx: 320, cy: 220 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
                ..Default::default()
            };
            assert_ne!(
                UpdateLayeredWindow(
                    self.hwnd,
                    screen,
                    null(),
                    &size,
                    dc,
                    &origin,
                    0,
                    &blend,
                    ULW_ALPHA
                ),
                0
            );
            SelectObject(dc, previous);
            DeleteObject(bitmap);
            DeleteDC(dc);
            ReleaseDC(null_mut(), screen);
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
        }
    }
}

fn pump(duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        unsafe {
            let mut message = Default::default();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
}

struct Foreground(HWND);
impl Drop for Foreground {
    fn drop(&mut self) {
        unsafe {
            SetForegroundWindow(self.0);
        }
    }
}

struct Cursor(POINT);
impl Cursor {
    fn save() -> Self {
        let mut position = POINT::default();
        assert_ne!(unsafe { GetCursorPos(&mut position) }, 0);
        Self(position)
    }
}
impl Drop for Cursor {
    fn drop(&mut self) {
        if unsafe { SetCursorPos(self.0.x, self.0.y) } == 0 {
            eprintln!(
                "Could not restore test cursor: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[test]
#[ignore = "Serial desktop test: repeated Snapshotter apartment/device teardown and recreation"]
fn sequential_snapshot_devices_survive_worker_and_caller_teardown() {
    capture::enable_per_monitor_dpi();
    let foreground = unsafe { GetForegroundWindow() };
    let target = Fixture::new("WGC lifetime layered fixture", WS_POPUP, true);
    target.layered();
    pump(Duration::from_millis(100));
    for index in 0..10 {
        let handle = target.hwnd as usize;
        let worker = thread::spawn(move || {
            let source = Snapshotter::new()?;
            let image = source.capture_raw(handle, || false)?;
            drop(source);
            Ok::<_, anyhow::Error>(image)
        });
        let deadline = Instant::now() + Duration::from_secs(4);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline, "Device lifetime probe stalled");
            pump(Duration::from_millis(4));
        }
        let image = worker.join().unwrap().unwrap();
        assert_eq!(image[(200, 110)].0, [100, 50, 20, 128]);
        eprintln!("sequential worker device {index}: captured and fully dropped");
    }
    // Models a picker worker exiting before the invocation's capture device is created.
    let source = Snapshotter::new().unwrap();
    let image = source.capture(target.hwnd as usize, || false).unwrap();
    assert_eq!(image[(200, 110)].0, [199, 100, 40, 128]);
    drop(source);
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
}

fn run_capture_task(task: impl FnOnce() -> Result<()> + Send + 'static) {
    let worker = thread::spawn(task);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline, "Public WGC capture task stalled");
        pump(Duration::from_millis(4));
    }
    worker.join().unwrap().unwrap();
}

#[test]
#[ignore = "Serial desktop integration: public WGC alpha, covered target, shared device, cursor and failure state"]
fn public_wgc_preserves_layered_alpha_and_desktop_state() {
    use capture::{CaptureBackend, CaptureOptions, CursorShadow};
    use std::sync::atomic::AtomicBool;

    capture::enable_per_monitor_dpi();
    let _cursor = Cursor::save();
    let target = Fixture::new("Public WGC known layered/AA source", WS_POPUP, true);
    target.layered();
    let cover = Fixture::new("Public WGC green occluder", WS_POPUP, false);
    cover.color(0x0032e61e);
    pump(Duration::from_millis(200));
    let foreground = unsafe { GetForegroundWindow() } as usize;
    let bounds = capture::window_info(target.hwnd as usize).unwrap().rect;
    if unsafe { SetCursorPos(bounds.left + 40, bounds.top + 110) } == 0 {
        panic!(
            "Cannot position test cursor: {}",
            std::io::Error::last_os_error()
        );
    }
    let handle = target.hwnd as usize;
    run_capture_task(move || {
        let source = Snapshotter::new()?;
        let cancelled = AtomicBool::new(false);
        let options = CaptureOptions {
            backend: CaptureBackend::Wgc,
            pad: 0,
            client_size: Some((320, 220)),
            ..Default::default()
        };
        let first = capture::capture_wgc(&source, handle, &options, &cancelled)?;
        let second = capture::capture_wgc(&source, handle, &options, &cancelled)?;
        assert_eq!(first.image, second.image);
        assert_eq!(first.image.dimensions(), (320, 220));
        for (x, expected) in [
            (40, [0; 4]),
            (120, [199, 100, 40, 64]),
            (200, [199, 100, 40, 128]),
            (280, [200, 100, 40, 255]),
        ] {
            assert_eq!(first.image[(x, 110)].0, expected);
        }
        for (y, alpha) in (20..28).zip([0, 7, 31, 64, 96, 128, 191, 255]) {
            assert_eq!(first.image[(160, y)][3], alpha);
        }
        assert!(first.cursor.is_empty());
        let shadow_free = CaptureOptions {
            no_shadow: true,
            pad: 4,
            ..options.clone()
        };
        let exact = capture::capture_wgc(&source, handle, &shadow_free, &cancelled)?;
        assert_eq!(
            exact.image, first.image,
            "No-shadow must preserve source alpha and ignore default padding"
        );
        for corner in [2u32, 1] {
            unsafe {
                assert!(
                    DwmSetWindowAttribute(
                        handle as HWND,
                        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                        (&corner as *const u32).cast(),
                        size_of::<u32>() as u32,
                    ) >= 0
                );
            }
            let result = capture::capture_wgc(&source, handle, &shadow_free, &cancelled);
            if corner == 2 {
                assert!(
                    result.is_err(),
                    "Forced DWM rounding is not an exact source silhouette"
                );
            } else {
                assert_eq!(result?.image, first.image);
            }
        }
        let padded = capture::capture_wgc(
            &source,
            handle,
            &CaptureOptions {
                pad: 3,
                include_cursor: true,
                cursor_shadow: CursorShadow::Off,
                ..options.clone()
            },
            &cancelled,
        )?;
        assert_eq!(padded.image.dimensions(), (326, 226));
        assert_eq!(padded.image[(123, 113)].0, first.image[(120, 110)].0);
        assert_eq!(padded.image[(0, 0)].0, [0; 4]);
        assert!(
            !padded.cursor.is_empty(),
            "Visible cursor inside the canvas must be a separate layer"
        );
        let finished = crate::alpha::finish(
            padded.image,
            &crate::alpha::AlphaOptions {
                crop: false,
                overlays: padded.cursor,
                ..Default::default()
            },
        )?;
        assert_eq!(finished.image.dimensions(), (326, 226));
        for invalid in [
            CaptureOptions {
                backend: CaptureBackend::Backdrop,
                ..options.clone()
            },
            CaptureOptions {
                client_size: Some((0, 220)),
                ..options.clone()
            },
            CaptureOptions {
                high_dpi: true,
                high_dpi_display: Some(r"\\.\NO_SUCH_DISPLAY".into()),
                ..options.clone()
            },
        ] {
            assert!(capture::capture_wgc(&source, handle, &invalid, &cancelled).is_err());
            assert_eq!(capture::window_info(handle)?.rect, bounds);
            assert_eq!(unsafe { GetForegroundWindow() } as usize, foreground);
        }
        assert!(capture::capture_wgc(&source, handle, &options, &AtomicBool::new(true)).is_err());
        unsafe {
            assert_ne!(
                SetWindowDisplayAffinity(handle as HWND, WDA_EXCLUDEFROMCAPTURE),
                0
            );
        }
        assert!(capture::capture_wgc(&source, handle, &options, &cancelled).is_err());
        unsafe {
            assert_ne!(SetWindowDisplayAffinity(handle as HWND, WDA_NONE), 0);
            ShowWindow(handle as HWND, SW_HIDE);
        }
        assert!(capture::capture_wgc(&source, handle, &options, &cancelled).is_err());
        unsafe {
            ShowWindow(handle as HWND, SW_SHOWMINNOACTIVE);
        }
        assert!(capture::capture_wgc(&source, handle, &options, &cancelled).is_err());
        assert_ne!(unsafe { IsIconic(handle as HWND) }, 0);
        unsafe {
            ShowWindow(handle as HWND, SW_SHOWNOACTIVATE);
        }
        assert!(capture::capture_wgc(&source, 0, &options, &cancelled).is_err());
        assert_eq!(capture::window_info(handle)?.rect, bounds);
        assert_eq!(unsafe { GetForegroundWindow() } as usize, foreground);
        Ok(())
    });
}

#[test]
#[ignore = "Serial desktop integration: public WGC safe-max shadow suppression stays tight and nonactivating"]
fn public_wgc_shadow_free_maximized_capture_has_no_default_padding() {
    use capture::{CaptureBackend, CaptureOptions};
    use std::sync::atomic::AtomicBool;

    capture::enable_per_monitor_dpi();
    let _restore = Foreground(unsafe { GetForegroundWindow() });
    let target = Fixture::new("Public WGC maximized no-shadow", WS_OVERLAPPEDWINDOW, false);
    let handle = target.hwnd as usize;
    let before = capture::window_info(handle).unwrap().rect;
    let foreground = unsafe { GetForegroundWindow() } as usize;
    run_capture_task(move || {
        let source = Snapshotter::new()?;
        let native = capture::capture_wgc(
            &source,
            handle,
            &CaptureOptions {
                backend: CaptureBackend::Wgc,
                no_shadow: true,
                ..Default::default()
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(native.image[(0, 0)], image::Rgba([0; 4]));
        assert_eq!(capture::window_info(handle)?.rect, before);
        assert_eq!(unsafe { GetForegroundWindow() } as usize, foreground);
        Ok(())
    });
    unsafe {
        ShowWindow(target.hwnd, SW_MAXIMIZE);
    }
    pump(Duration::from_millis(250));
    let foreground = unsafe { GetForegroundWindow() } as usize;
    let bounds = capture::window_info(target.hwnd as usize).unwrap().rect;
    let handle = target.hwnd as usize;
    run_capture_task(move || {
        let source = Snapshotter::new()?;
        let result = capture::capture_wgc(
            &source,
            handle,
            &CaptureOptions {
                backend: CaptureBackend::Wgc,
                no_shadow: true,
                ..Default::default()
            },
            &AtomicBool::new(false),
        )?;
        let mut frame = windows_sys::Win32::Foundation::RECT::default();
        unsafe {
            assert!(
                DwmGetWindowAttribute(
                    handle as HWND,
                    DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                    (&mut frame as *mut windows_sys::Win32::Foundation::RECT).cast(),
                    size_of::<windows_sys::Win32::Foundation::RECT>() as u32
                ) >= 0
            );
        }
        assert_eq!(
            result.image.dimensions(),
            (
                (frame.right - frame.left) as u32,
                (frame.bottom - frame.top) as u32
            )
        );
        assert!(result.image.pixels().all(|pixel| pixel[3] == 255));
        assert_eq!(capture::window_info(handle)?.rect, bounds);
        assert_ne!(unsafe { IsZoomed(handle as HWND) }, 0);
        assert_eq!(unsafe { GetForegroundWindow() } as usize, foreground);
        Ok(())
    });
}
