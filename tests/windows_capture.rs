//! Desktop integration tests that drive disposable Win32 fixture windows.
//!
//! The unsafe blocks call Win32 APIs on fixture windows this file creates, on the
//! creating thread, so per-block SAFETY comments would repeat one argument.
#![cfg(windows)]
#![allow(clippy::undocumented_unsafe_blocks)]

use alphashot::{
    alpha::{AlphaOptions, recover},
    capture::{self, CaptureOptions, FocusState, Rect},
};
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::*,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

static DESKTOP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
#[ignore = "Serial desktop integration: covered WGC CLI file/editor/clipboard output and native alpha association"]
fn wgc_cli_keeps_native_alpha_and_uses_the_shared_output_pipeline() {
    use std::{
        process::{Command, Output, Stdio},
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::SIZE,
        System::{
            DataExchange::*,
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::{CF_DIBV5, CF_HDROP},
        },
        UI::Shell::CFSTR_PREFERREDDROPEFFECT,
    };

    fn run(command: &mut Command) -> Output {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            unsafe {
                let mut message = zeroed();
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "WGC CLI stalled: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn clipboard_bytes(format: u32) -> Vec<u8> {
        unsafe {
            let handle = GetClipboardData(format);
            assert!(!handle.is_null());
            let pointer = GlobalLock(handle).cast::<u8>();
            assert!(!pointer.is_null());
            let bytes = std::slice::from_raw_parts(pointer, GlobalSize(handle)).to_vec();
            GlobalUnlock(handle);
            bytes
        }
    }

    let _desktop = DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let target = Fixture::new();
    let bounds = capture::centered(capture::list_monitors().unwrap()[0].work, 320, 220);
    unsafe {
        SetWindowLongPtrW(target.hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE) as isize);
        SetWindowLongPtrW(
            target.hwnd,
            GWL_EXSTYLE,
            (WS_EX_LAYERED | WS_EX_NOACTIVATE) as isize,
        );
        assert_ne!(
            SetWindowPos(
                target.hwnd,
                null_mut(),
                bounds.left,
                bounds.top,
                320,
                220,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
            ),
            0
        );
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
                ..zeroed()
            },
            ..zeroed()
        };
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(dc, &header, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        assert!(!bitmap.is_null());
        let previous = SelectObject(dc, bitmap);
        for (index, pixel) in std::slice::from_raw_parts_mut(bits.cast::<u8>(), 320 * 220 * 4)
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            let alpha = [0u16, 64, 128, 255][index % 320 / 80];
            pixel.copy_from_slice(&[
                (40 * alpha / 255) as u8,
                (100 * alpha / 255) as u8,
                (200 * alpha / 255) as u8,
                alpha as u8,
            ]);
        }
        let origin = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
            ..zeroed()
        };
        let updated = UpdateLayeredWindow(
            target.hwnd,
            screen,
            null(),
            &SIZE { cx: 320, cy: 220 },
            dc,
            &origin,
            0,
            &blend,
            ULW_ALPHA,
        );
        SelectObject(dc, previous);
        DeleteObject(bitmap);
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);
        assert_ne!(updated, 0);
    }
    let cover = Fixture::new();
    unsafe {
        assert_ne!(
            SetWindowPos(
                cover.hwnd,
                HWND_TOPMOST,
                bounds.left - 20,
                bounds.top - 20,
                360,
                260,
                SWP_NOACTIVATE
            ),
            0
        );
        SetWindowLongPtrW(cover.hwnd, GWLP_USERDATA, 240);
        InvalidateRect(cover.hwnd, null(), 0);
        UpdateWindow(cover.hwnd);
        assert!(windows_sys::Win32::Graphics::Dwm::DwmFlush() >= 0);
    }
    let before = saved_geometry(target.hwnd);
    let foreground = unsafe { GetForegroundWindow() };
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("native transparent.png");
    let common = [
        "--backend",
        "wgc",
        "--no-preview",
        "--no-crop",
        "--pad",
        "0",
        "--window-hwnd",
    ];
    let saved = run(Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args(common)
        .arg(format!("0x{:X}", target.hwnd as usize))
        .args(["--no-clipboard", "--out"])
        .arg(&out)
        .arg("--editor")
        .arg(directory.path().join("missing editor.exe")));
    assert_eq!(
        String::from_utf8(saved.stdout).unwrap().trim(),
        out.display().to_string()
    );
    assert!(
        String::from_utf8_lossy(&saved.stderr).contains("Image saved, but editor launch failed")
    );
    let image = image::open(&out).unwrap().to_rgba8();
    assert_eq!(image.dimensions(), (320, 220));
    for (x, expected) in [
        (40, [0; 4]),
        (120, [199, 100, 40, 64]),
        (200, [199, 100, 40, 128]),
        (280, [200, 100, 40, 255]),
    ] {
        assert_eq!(image[(x, 110)].0, expected);
    }
    assert_eq!(saved_geometry(target.hwnd), before);
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    let copied = run(Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args(common)
        .arg(format!("0x{:X}", target.hwnd as usize))
        .arg("--clipboard-only")
        .current_dir(directory.path()));
    assert!(copied.stdout.is_empty());
    assert!(String::from_utf8_lossy(&copied.stderr).contains("Copied"));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    assert_eq!(saved_geometry(target.hwnd), before);
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);

    struct Clipboard;
    impl Drop for Clipboard {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }
    unsafe {
        let deadline = Instant::now() + Duration::from_secs(1);
        while OpenClipboard(null_mut()) == 0 {
            assert!(
                Instant::now() < deadline,
                "Clipboard stayed busy after WGC CLI exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _clipboard = Clipboard;
        let png = clipboard_bytes(RegisterClipboardFormatW(windows_sys::w!("PNG")));
        assert_eq!(
            image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .unwrap()
                .to_rgba8(),
            image
        );
        let dib = clipboard_bytes(u32::from(CF_DIBV5));
        let header = dib.as_ptr().cast::<BITMAPV5HEADER>().read_unaligned();
        assert_eq!((header.bV5Width, header.bV5Height), (320, 220));
        let sample = 124 + ((220 - 1 - 110) * 320 + 120) * 4;
        assert_eq!(&dib[sample..sample + 4], &[10, 25, 50, 64]);
        assert!(GetClipboardData(u32::from(CF_HDROP)).is_null());
        assert!(GetClipboardData(RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT)).is_null());
    }
}

#[path = "../src/sixel.rs"]
mod sixel;

#[path = "../src/session.rs"]
#[allow(dead_code)]
mod session;

#[test]
#[ignore = "Uses disposable desktop windows to verify occluded WGC snapshots and measure the release preview pipeline"]
fn snapshots_occluded_windows_without_changing_focus_or_placement() {
    use std::time::{Duration, Instant};
    let _desktop = DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let target = Fixture::new();
    let cover = Fixture::new();
    let bounds = capture::centered(capture::list_monitors().unwrap()[0].work, 1280, 900);
    unsafe {
        for hwnd in [target.hwnd, cover.hwnd] {
            assert_ne!(
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    bounds.left,
                    bounds.top,
                    bounds.width(),
                    bounds.height(),
                    SWP_NOZORDER | SWP_NOACTIVATE
                ),
                0
            );
        }
        SetWindowLongPtrW(cover.hwnd, GWLP_USERDATA, 240);
        InvalidateRect(cover.hwnd, null(), 0);
        UpdateWindow(cover.hwnd);
    }
    let before = capture::window_info(target.hwnd as usize).unwrap();
    let foreground = unsafe { GetForegroundWindow() } as usize;
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let handle = target.hwnd as usize;
    let task = std::thread::spawn(move || -> anyhow::Result<()> {
        let initialized = Instant::now();
        let snapshotter = capture::snapshot::Snapshotter::new()?;
        eprintln!("WGC initialization: {:?}", initialized.elapsed());
        for i in 0..5 {
            let started = Instant::now();
            let image = snapshotter.capture(handle, || false)?;
            let captured = started.elapsed();
            let center = image[(image.width() / 2, image.height() / 2)];
            assert_eq!(
                center.0,
                [68, 136, 204, 255],
                "Must capture the blue target, not its grey occluder"
            );
            assert!(
                image
                    .pixels()
                    .all(|pixel| pixel[3] != 0 || pixel.0 == [0; 4])
            );
            let dimensions = (image.width(), image.height());
            let started = Instant::now();
            let cached = if image.width() > 1024 || image.height() > 768 {
                image::imageops::thumbnail(&image, 1024, 768)
            } else {
                image
            };
            let (width, height) = sixel::bounds(28, 12);
            let thumbnail = sixel::fit(&cached, width, height);
            let resized = started.elapsed();
            let started = Instant::now();
            let encoded = sixel::encode(&thumbnail)?;
            let encoding = started.elapsed();
            eprintln!(
                "preview sample {i}: {}x{}, capture={captured:?}, resize={resized:?}, encode={encoding:?}, bytes={}",
                dimensions.0,
                dimensions.1,
                encoded.len()
            );
        }
        assert!(snapshotter.capture(0, || false).is_err());
        assert!(snapshotter.capture(handle, || true).is_err());
        send.send(()).unwrap();
        Ok(())
    });
    let until = Instant::now() + Duration::from_secs(15);
    while receive.try_recv().is_err() && !task.is_finished() {
        unsafe {
            let mut message = zeroed();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        assert!(Instant::now() < until, "WGC test timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
    task.join().unwrap().unwrap();
    assert_eq!(capture::window_info(handle).unwrap().rect, before.rect);
    assert_eq!(unsafe { GetForegroundWindow() } as usize, foreground);
}

unsafe extern "system" fn fixture_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe {
        if message == WM_TIMER && wparam == 2 {
            let color = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, color % 250 + 1);
            InvalidateRect(hwnd, null(), 0);
            return 0;
        }
        let color = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if message == WM_PAINT && color > 0 {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let brush = CreateSolidBrush((color as u32) * 0x010101);
            FillRect(dc, &paint.rcPaint, brush);
            DeleteObject(brush);
            EndPaint(hwnd, &paint);
            return 0;
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

struct Fixture {
    hwnd: HWND,
    brush: HBRUSH,
    class: Vec<u16>,
    foreground: HWND,
    cursor: POINT,
}
impl Fixture {
    fn new() -> Self {
        capture::enable_per_monitor_dpi();
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let class: Vec<u16> = format!(
            "AlphashotTest-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let work = capture::list_monitors().unwrap()[0].work;
        let bounds = capture::centered(work, 440, 280);
        unsafe {
            let foreground = GetForegroundWindow();
            let mut cursor = POINT::default();
            assert_ne!(GetCursorPos(&mut cursor), 0);
            let brush = CreateSolidBrush(0x00cc8844);
            let instance = GetModuleHandleW(null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(fixture_proc),
                hInstance: instance,
                hbrBackground: brush,
                lpszClassName: class.as_ptr(),
                ..zeroed()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let title: Vec<u16> = "Alphashot capture test"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
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
            let fixture = Self {
                hwnd,
                brush,
                class,
                foreground,
                cursor,
            };
            // The opt-in desktop test needs real foreground permission, as a user launch has.
            if SetForegroundWindow(hwnd) == 0 {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                SetCursorPos(
                    bounds.left + bounds.width() / 2,
                    bounds.top + bounds.height() / 2,
                );
                let inputs = [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP].map(|flags| INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dwFlags: flags,
                            ..zeroed()
                        },
                    },
                });
                assert_eq!(
                    SendInput(
                        inputs.len() as u32,
                        inputs.as_ptr(),
                        size_of::<INPUT>() as i32
                    ),
                    2
                );
                let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while GetForegroundWindow() != hwnd && std::time::Instant::now() < until {
                    let mut message = zeroed();
                    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                SetWindowPos(
                    hwnd,
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                SetCursorPos(cursor.x, cursor.y);
            }
            fixture
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
            DeleteObject(self.brush);
            SetCursorPos(self.cursor.x, self.cursor.y);
            if !self.foreground.is_null() {
                SetForegroundWindow(self.foreground);
            }
        }
    }
}

struct GeometryFixture {
    window: Fixture,
    state: Box<GeometryFixtureState>,
}

struct GeometryFixtureState {
    custom_frame: bool,
    refuse_size: bool,
    requested: (u32, u32),
    saw_requested: bool,
    cancel_on_resize: Option<std::sync::Arc<AtomicBool>>,
}

impl GeometryFixture {
    fn new(custom_frame: bool) -> Self {
        let mut fixture = Self {
            window: Fixture::new(),
            state: Box::new(GeometryFixtureState {
                custom_frame,
                refuse_size: false,
                requested: (640, 420),
                saw_requested: false,
                cancel_on_resize: None,
            }),
        };
        unsafe {
            SetWindowLongPtrW(
                fixture.window.hwnd,
                GWLP_USERDATA,
                (&mut *fixture.state as *mut GeometryFixtureState) as isize,
            );
            SetWindowLongPtrW(
                fixture.window.hwnd,
                GWLP_WNDPROC,
                geometry_fixture_proc as *const () as isize,
            );
            SetWindowPos(
                fixture.window.hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        fixture
    }
}

unsafe extern "system" fn geometry_fixture_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GeometryFixtureState;
        if !state.is_null() {
            if message == WM_NCCALCSIZE && (*state).custom_frame {
                let client = if wparam != 0 {
                    &mut (*(lparam as *mut NCCALCSIZE_PARAMS)).rgrc[0]
                } else {
                    &mut *(lparam as *mut RECT)
                };
                // Changing chrome at a width threshold requires another measured adjustment.
                let inset = if client.right - client.left >= 600 {
                    24
                } else {
                    8
                };
                client.left += inset;
                client.top += inset;
                client.right -= inset;
                client.bottom -= inset;
                return 0;
            }
            if message == WM_WINDOWPOSCHANGING && (*state).refuse_size {
                let position = &mut *(lparam as *mut WINDOWPOS);
                if position.flags & SWP_NOSIZE == 0 {
                    position.cx = position.cx.min(480);
                    position.cy = position.cy.min(320);
                }
            }
        }
        let result = DefWindowProcW(hwnd, message, wparam, lparam);
        if !state.is_null() && message == WM_SIZE && wparam != SIZE_MINIMIZED as usize {
            let mut client = RECT::default();
            assert_ne!(GetClientRect(hwnd, &mut client), 0);
            if (client.right as u32, client.bottom as u32) == (*state).requested {
                (*state).saw_requested = true;
                if let Some(cancelled) = &(*state).cancel_on_resize {
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        result
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SavedGeometry {
    outer: Rect,
    flags: u32,
    show_command: u32,
    minimized_position: Option<(i32, i32)>,
    maximized_position: (i32, i32),
    normal: Rect,
}

fn saved_geometry(hwnd: HWND) -> SavedGeometry {
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..unsafe { zeroed() }
    };
    assert_ne!(unsafe { GetWindowPlacement(hwnd, &mut placement) }, 0);
    let normal = placement.rcNormalPosition;
    SavedGeometry {
        outer: capture::window_info(hwnd as usize).unwrap().rect,
        flags: placement.flags,
        show_command: placement.showCmd,
        // Windows ignores ptMinPosition unless WPF_SETMINPOSITION is set.
        minimized_position: (placement.flags & WPF_SETMINPOSITION != 0)
            .then_some((placement.ptMinPosition.x, placement.ptMinPosition.y)),
        maximized_position: (placement.ptMaxPosition.x, placement.ptMaxPosition.y),
        normal: Rect {
            left: normal.left,
            top: normal.top,
            right: normal.right,
            bottom: normal.bottom,
        },
    }
}

#[test]
#[ignore = "Uses an interactive Windows desktop and resizes native and custom-frame fixture windows"]
fn captures_exact_client_sizes_and_restores_window_states() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for custom_frame in [false, true] {
        let mut fixture = GeometryFixture::new(custom_frame);
        for (state, show) in [
            ("normal", SW_SHOWNORMAL),
            ("minimized", SW_MINIMIZE),
            ("maximized", SW_MAXIMIZE),
            ("minimized-from-maximized", SW_MINIMIZE),
        ] {
            unsafe { ShowWindow(fixture.window.hwnd, show) };
            let before = saved_geometry(fixture.window.hwnd);
            fixture.state.saw_requested = false;
            let requested = fixture.state.requested;
            let pair = capture::capture(
                fixture.window.hwnd as usize,
                &CaptureOptions {
                    client_size: Some(requested),
                    ..Default::default()
                },
                &AtomicBool::new(false),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "custom_frame={custom_frame}, state={state}, requested={requested:?}: {error:#}"
                )
            });
            assert!(
                fixture.state.saw_requested,
                "custom_frame={custom_frame}, state={state}"
            );
            assert_eq!(pair.white.dimensions(), pair.black.dimensions());
            assert!(pair.white.width() >= requested.0);
            assert!(pair.white.height() >= requested.1);
            assert_eq!(
                saved_geometry(fixture.window.hwnd),
                before,
                "custom_frame={custom_frame}, state={state}"
            );
        }
    }
}

#[test]
#[ignore = "Uses an interactive Windows desktop to check native corner masking, exact client sizing, maximized bounds and restoration"]
fn no_shadow_retains_native_and_maximized_chrome() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let before = saved_geometry(fixture.hwnd);
    let foreground = unsafe { GetForegroundWindow() };
    let style = unsafe { GetWindowLongPtrW(fixture.hwnd, GWL_STYLE) };
    let native = capture::capture(
        fixture.hwnd as usize,
        &CaptureOptions {
            no_shadow: true,
            client_size: Some((640, 420)),
            ..Default::default()
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(native.blend_space, alphashot::alpha::BlendSpace::Srgb);
    let image = recover(
        &native.white,
        &native.black,
        &AlphaOptions {
            blend_space: native.blend_space,
            corner_radius: native.corner_radius,
            crop: false,
            ..Default::default()
        },
    )
    .unwrap()
    .image;
    assert!(image.width() >= 640 && image.height() > 420);
    if native.corner_radius > 0.0 {
        assert_eq!(image[(0, 0)].0, [0; 4]);
        assert!(image.pixels().any(|p| p[3] > 0 && p[3] < 255));
    }
    eprintln!(
        "Native SDR center: white={:?}, black={:?}, rgba={:?}, dpi={}",
        native.white[(image.width() / 2, image.height() / 2)].0,
        native.black[(image.width() / 2, image.height() / 2)].0,
        image[(image.width() / 2, image.height() / 2)].0,
        native.window.dpi
    );
    assert_eq!(
        image[(image.width() / 2, image.height() / 2)].0,
        [0x44, 0x88, 0xcc, 255]
    );
    assert_eq!(saved_geometry(fixture.hwnd), before);
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    assert_eq!(unsafe { GetWindowLongPtrW(fixture.hwnd, GWL_STYLE) }, style);

    unsafe { ShowWindow(fixture.hwnd, SW_MAXIMIZE) };
    let before = saved_geometry(fixture.hwnd);
    let frame = frame(fixture.hwnd);
    let pair = capture::capture(
        fixture.hwnd as usize,
        &CaptureOptions {
            no_shadow: true,
            ..Default::default()
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(pair.blend_space, alphashot::alpha::BlendSpace::Srgb);
    assert_eq!(
        pair.white.dimensions(),
        (frame.width() as u32, frame.height() as u32)
    );
    assert!(
        pair.opaque_regions.is_empty(),
        "No-shadow capture must retain client alpha"
    );
    let image = recover(
        &pair.white,
        &pair.black,
        &AlphaOptions {
            corner_radius: pair.corner_radius,
            crop: false,
            ..Default::default()
        },
    )
    .unwrap()
    .image;
    let mut client = RECT::default();
    assert_ne!(unsafe { GetClientRect(fixture.hwnd, &mut client) }, 0);
    assert!(
        image.height() > client.bottom as u32,
        "The native title bar must remain in the image"
    );
    assert_eq!(
        image[(image.width() / 2, image.height() / 2)].0,
        [0x44, 0x88, 0xcc, 255]
    );
    assert_eq!(saved_geometry(fixture.hwnd), before);
}

#[test]
#[ignore = "Uses an interactive Windows desktop and an application-constrained disposable window"]
fn refuses_constrained_client_sizes_and_restores_geometry() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut fixture = GeometryFixture::new(false);
    let before = saved_geometry(fixture.window.hwnd);
    let mut client = RECT::default();
    assert_ne!(
        unsafe { GetClientRect(fixture.window.hwnd, &mut client) },
        0
    );
    let actual = (
        480 - (before.outer.width() - client.right),
        320 - (before.outer.height() - client.bottom),
    );
    fixture.state.refuse_size = true;
    let error = capture::capture(
        fixture.window.hwnd as usize,
        &CaptureOptions {
            client_size: Some(fixture.state.requested),
            ..Default::default()
        },
        &AtomicBool::new(false),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("Requested client size 640x420"), "{error}");
    assert!(
        error.contains(&format!("actual client size {}x{}", actual.0, actual.1)),
        "{error}"
    );
    assert_eq!(saved_geometry(fixture.window.hwnd), before);
}

#[test]
#[ignore = "Uses an interactive Windows desktop and cancels or fails after resizing disposable windows"]
fn restores_client_geometry_after_cancellation_and_workspace_failure() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut fixture = GeometryFixture::new(false);
    for show in [SW_SHOWNORMAL, SW_MAXIMIZE, SW_MINIMIZE] {
        unsafe { ShowWindow(fixture.window.hwnd, show) };
        let before = saved_geometry(fixture.window.hwnd);
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        fixture.state.cancel_on_resize = Some(std::sync::Arc::clone(&cancelled));
        fixture.state.saw_requested = false;
        let error = capture::capture(
            fixture.window.hwnd as usize,
            &CaptureOptions {
                client_size: Some(fixture.state.requested),
                ..Default::default()
            },
            &cancelled,
        )
        .err()
        .unwrap();
        fixture.state.cancel_on_resize = None;
        assert!(fixture.state.saw_requested);
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert_eq!(saved_geometry(fixture.window.hwnd), before);
    }

    unsafe { ShowWindow(fixture.window.hwnd, SW_RESTORE) };
    let before = saved_geometry(fixture.window.hwnd);
    let work = capture::list_monitors().unwrap()[0].work;
    fixture.state.requested = ((work.width() - 100).max(320) as u32, 220);
    fixture.state.saw_requested = false;
    let error = capture::capture(
        fixture.window.hwnd as usize,
        &CaptureOptions {
            client_size: Some(fixture.state.requested),
            pad_mode: capture::PadMode::Uniform,
            pad: 4096,
            ..Default::default()
        },
        &AtomicBool::new(false),
    )
    .err()
    .unwrap();
    assert!(fixture.state.saw_requested);
    assert!(error.to_string().contains("workspace"), "{error}");
    assert!(
        error.to_string().contains("Requested client size"),
        "{error}"
    );
    assert_eq!(saved_geometry(fixture.window.hwnd), before);
}

#[test]
#[ignore = "Captures a disposable window repeatedly after changing its content and title"]
fn repeated_capture_keeps_target_settings_and_restores_the_desktop() {
    use alphashot::output::{self, SaveOptions};
    let _desktop = DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let original = capture::window_info(fixture.hwnd as usize).unwrap();
    let target = session::Target::new(&original);
    let foreground = unsafe { GetForegroundWindow() };
    let mut cursor = POINT::default();
    assert_ne!(unsafe { GetCursorPos(&mut cursor) }, 0);
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("session.png");
    std::fs::write(directory.path().join("session-0001.png"), b"keep image").unwrap();
    std::fs::write(directory.path().join("capture-0002-white.png"), b"keep raw").unwrap();
    let options = CaptureOptions::default();
    let cancelled = AtomicBool::new(false);
    let mut number = 0;
    for (index, color) in [40_u8, 220].into_iter().enumerate() {
        let title = format!("Changed session title {index}");
        let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        unsafe {
            assert_ne!(SetWindowTextW(fixture.hwnd, wide.as_ptr()), 0);
            SetWindowLongPtrW(fixture.hwnd, GWLP_USERDATA, isize::from(color));
            InvalidateRect(fixture.hwnd, null(), 0);
            UpdateWindow(fixture.hwnd);
        }
        assert_eq!(target.window().unwrap().title, title);
        let paths =
            session::Paths::next(Some(&base), Some(directory.path()), "capture", &mut number)
                .unwrap();
        assert_eq!(number, index as u64 + 3);
        let capture = capture::capture(fixture.hwnd as usize, &options, &cancelled).unwrap();
        let result = recover(
            &capture.white,
            &capture.black,
            &AlphaOptions {
                crop: false,
                opaque_regions: capture.opaque_regions,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            result.image[(result.image.width() / 2, result.image.height() / 2)].0,
            [color, color, color, 255]
        );
        let image = paths.image.unwrap();
        output::save(&result.image, &image, &SaveOptions::default()).unwrap();
        let (white, black) = paths.raw.unwrap();
        output::save_composite(&capture.white, &white, capture.window.dpi, false).unwrap();
        output::save_composite(&capture.black, &black, capture.window.dpi, false).unwrap();
        assert!(image::open(image).unwrap().color().has_alpha());
        assert_eq!(target.window().unwrap().rect, original.rect);
        assert_eq!(unsafe { GetForegroundWindow() }, foreground);
        let mut after = POINT::default();
        assert_ne!(unsafe { GetCursorPos(&mut after) }, 0);
        assert_eq!((after.x, after.y), (cursor.x, cursor.y));
    }
    assert_eq!(
        std::fs::read(directory.path().join("session-0001.png")).unwrap(),
        b"keep image"
    );
    assert_eq!(
        std::fs::read(directory.path().join("capture-0002-white.png")).unwrap(),
        b"keep raw"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 8);
    assert_ne!(unsafe { DestroyWindow(fixture.hwnd) }, 0);
    let error = target.window().unwrap_err();
    assert!(error.to_string().contains("closed or unavailable"));
    assert!(error.to_string().contains("restart --repeat"));
}

#[test]
#[ignore = "Uses an interactive Windows desktop and briefly shows a disposable test window"]
fn captures_real_window_and_restores_it_on_success_and_cancel() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let cancelled = AtomicBool::new(false);
    let original = capture::window_info(fixture.hwnd as usize).unwrap().rect;
    for focus in [FocusState::Focused, FocusState::Unfocused] {
        let pair = capture::capture(
            fixture.hwnd as usize,
            &CaptureOptions {
                focus,
                include_cursor: true,
                ..Default::default()
            },
            &cancelled,
        )
        .unwrap();
        assert_eq!(pair.white.dimensions(), pair.black.dimensions());
        let recovered = recover(
            &pair.white,
            &pair.black,
            &AlphaOptions {
                crop: false,
                overlays: pair.cursor,
                ..Default::default()
            },
        )
        .unwrap()
        .image;
        assert_eq!(
            recovered[(recovered.width() / 2, recovered.height() / 2)].0,
            [0x44, 0x88, 0xcc, 255]
        );
        assert_eq!(recovered[(0, 0)][3], 0);
    }
    let foreground_before = unsafe { GetForegroundWindow() };
    let high_dpi = capture::capture(
        fixture.hwnd as usize,
        &CaptureOptions {
            high_dpi: true,
            ..Default::default()
        },
        &cancelled,
    );
    if capture::list_monitors()
        .unwrap()
        .iter()
        .any(|monitor| monitor.dpi > 96)
    {
        assert!(high_dpi.unwrap().window.dpi > 96);
    } else {
        let error = high_dpi.err().unwrap().to_string();
        assert!(
            error.contains("No active monitor is scaled above 100%"),
            "{error}"
        );
        assert!(error.contains("Active displays:"), "{error}");
        assert_eq!(unsafe { GetForegroundWindow() }, foreground_before);
    }
    assert_eq!(
        capture::window_info(fixture.hwnd as usize).unwrap().rect,
        original
    );
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("window.png");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args([
            "--window-hwnd",
            &format!("0x{:X}", fixture.hwnd as usize),
            "--raw-dir",
        ])
        .arg(directory.path())
        .arg("-o")
        .arg(&out)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    unsafe {
        AllowSetForegroundWindow(child.id());
    }
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        unsafe {
            let mut message = zeroed();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if started.elapsed() > std::time::Duration::from_secs(15) {
            child.kill().unwrap();
            panic!("CLI capture did not finish within 15 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(image::open(&out).unwrap().color().has_alpha());
    assert!(directory.path().join("capture-white.png").exists());
    assert!(directory.path().join("capture-black.png").exists());
    assert_eq!(
        String::from_utf8(result.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            directory
                .path()
                .join("capture-white.png")
                .display()
                .to_string(),
            directory
                .path()
                .join("capture-black.png")
                .display()
                .to_string(),
            out.display().to_string(),
        ]
    );
    assert!(!result.stderr.contains(&0x1b));
    let listed = capture::list_windows().unwrap();
    let executable = std::env::current_exe().unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|w| w.handle == fixture.hwnd as usize)
            .unwrap()
            .process_name
            .as_deref(),
        executable.file_name().unwrap().to_str()
    );
    let before = std::fs::read(&out).unwrap();
    let existing = std::process::Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args([
            "--window-hwnd",
            &format!("0x{:X}", fixture.hwnd as usize),
            "-o",
        ])
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(existing.status.code(), Some(1));
    assert_eq!(std::fs::read(&out).unwrap(), before);
    let alias = std::process::Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args([
            "--window-hwnd",
            &format!("0x{:X}", fixture.hwnd as usize),
            "--force",
            "--raw-dir",
        ])
        .arg(directory.path())
        .arg("-o")
        .arg(directory.path().join("capture-white.png"))
        .output()
        .unwrap();
    assert_eq!(alias.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&alias.stderr).contains("must differ from the raw capture paths")
    );
    {
        let duplicate = Fixture::new();
        let title = format!("Alphashot targeting ambiguity {}", std::process::id());
        let wide_title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        for hwnd in [fixture.hwnd, duplicate.hwnd] {
            assert_ne!(unsafe { SetWindowTextW(hwnd, wide_title.as_ptr()) }, 0);
        }
        let ambiguous_out = directory.path().join("ambiguous.png");
        let ambiguous = std::process::Command::new(env!("CARGO_BIN_EXE_alphashot"))
            .args([
                "--window-title",
                &title,
                "--process",
                executable.file_name().unwrap().to_str().unwrap(),
                "--wait",
                "0",
                "--no-clipboard",
                "--out",
            ])
            .arg(&ambiguous_out)
            .output()
            .unwrap();
        assert_eq!(ambiguous.status.code(), Some(1));
        let error = String::from_utf8_lossy(&ambiguous.stderr);
        assert!(
            error.contains("2 windows matched the requested selectors"),
            "{error}"
        );
        assert!(error.contains("narrow --window-title"), "{error}");
        assert!(error.contains("--process or --window-class"), "{error}");
        assert!(error.contains("--window-hwnd"), "{error}");
        for hwnd in [fixture.hwnd, duplicate.hwnd] {
            let window = capture::window_info(hwnd as usize).unwrap();
            assert!(error.contains(&format!("0x{:X}", window.handle)), "{error}");
            assert!(error.contains(&window.class_name), "{error}");
        }
        assert!(ambiguous.stdout.is_empty());
        assert!(!ambiguous_out.exists());
    }
    let mut placement: WINDOWPLACEMENT = unsafe { zeroed() };
    placement.length = size_of::<WINDOWPLACEMENT>() as u32;
    assert_ne!(
        unsafe { GetWindowPlacement(fixture.hwnd, &mut placement) },
        0
    );
    let cancelled = AtomicBool::new(true);
    assert!(
        capture::capture(
            fixture.hwnd as usize,
            &CaptureOptions {
                high_dpi: true,
                ..Default::default()
            },
            &cancelled
        )
        .is_err()
    );
    assert_eq!(
        capture::window_info(fixture.hwnd as usize).unwrap().rect,
        original
    );
}

#[test]
#[ignore = "Uses an interactive desktop to list, hide and show a disposable untitled window"]
fn lists_combined_selectors_and_waits_for_an_untitled_window() {
    use std::process::Command;
    let _desktop = DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let info = capture::window_info(fixture.hwnd as usize).unwrap();
    let process = info.process_name.as_ref().unwrap().to_uppercase();
    let class = info.class_name.to_uppercase();
    let list = |selectors: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_alphashot"))
            .arg("--list-windows")
            .args(selectors)
            .output()
            .unwrap()
    };
    let result = list(&[
        "--window-title",
        "CAPTURE TEST",
        "--process",
        &process,
        "--window-class",
        &class,
    ]);
    assert!(result.status.success());
    let table = String::from_utf8(result.stdout).unwrap();
    assert_eq!(table.lines().count(), 2, "{table}");
    assert!(table.lines().next().unwrap().ends_with("\tclass"));
    for selectors in [
        vec![
            "--process",
            "__alphashot_missing__.exe",
            "--window-class",
            &class,
        ],
        vec!["--process", &process, "--window-class", "__missing_class__"],
    ] {
        let result = list(&selectors);
        assert!(result.status.success());
        assert_eq!(String::from_utf8(result.stdout).unwrap().lines().count(), 1);
    }
    unsafe {
        assert_ne!(SetWindowTextW(fixture.hwnd, [0_u16].as_ptr()), 0);
        ShowWindow(fixture.hwnd, SW_HIDE);
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args([
            "--list-windows",
            "--process",
            &process,
            "--window-class",
            &class,
            "--wait",
            "2",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(child.try_wait().unwrap().is_none());
    unsafe {
        ShowWindow(fixture.hwnd, SW_SHOWNOACTIVATE);
        UpdateWindow(fixture.hwnd);
    }
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let table = String::from_utf8(result.stdout).unwrap();
    let fields: Vec<_> = table.lines().nth(1).unwrap().split('\t').collect();
    assert_eq!(fields[0], format!("0x{:X}", fixture.hwnd as usize));
    assert!(
        fields[6].is_empty(),
        "An untitled window must keep its empty title"
    );
    assert_eq!(fields[7], info.class_name);
    unsafe {
        ShowWindow(fixture.hwnd, SW_HIDE);
    }
    let result = list(&["--window-class", &class, "--wait", "0"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("within 0 seconds"));
}

#[test]
#[ignore = "Uses an interactive desktop and disposable windows in separate processes"]
fn restores_foreground_after_capturing_another_process() {
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};

    if std::env::var_os("ALPHASHOT_FOREGROUND_FIXTURE").is_some() {
        let fixture = Fixture::new();
        let work = capture::list_monitors().unwrap()[0].work;
        unsafe {
            assert_ne!(
                SetWindowPos(
                    fixture.hwnd,
                    null_mut(),
                    work.left,
                    work.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE
                ),
                0
            );
        }
        println!("fixture:{}", fixture.hwnd as usize);
        std::io::stdout().flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while unsafe { IsWindow(fixture.hwnd) } != 0 && Instant::now() < deadline {
            unsafe {
                let mut message = zeroed();
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        return;
    }

    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let foreground_before = unsafe { GetForegroundWindow() };
    let mut target = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "restores_foreground_after_capturing_another_process",
            "--nocapture",
        ])
        .env("ALPHASHOT_FOREGROUND_FIXTURE", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = std::io::BufReader::new(target.stdout.take().unwrap());
    let handle = loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "Target fixture exited before opening its window"
        );
        if let Some(handle) = line.trim().strip_prefix("fixture:") {
            break handle.parse::<usize>().unwrap();
        }
    };
    let mut previous = Fixture::new();
    previous.foreground = foreground_before;
    assert_eq!(unsafe { GetForegroundWindow() }, previous.hwnd);
    let original = capture::window_info(handle).unwrap().rect;
    let directory = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args([
            "--window-hwnd",
            &format!("0x{handle:X}"),
            "--no-preview",
            "-o",
        ])
        .arg(directory.path().join("window.png"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        unsafe {
            let mut message = zeroed();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("Cross-process capture did not finish within 15 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = child.wait_with_output().unwrap();
    let foreground = unsafe { GetForegroundWindow() };
    let restored = capture::window_info(handle).unwrap().rect;
    unsafe {
        PostMessageW(handle as HWND, WM_CLOSE, 0, 0);
    }
    target.wait().unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert_eq!(foreground, previous.hwnd, "{stderr}");
    assert_eq!(restored, original);
    assert!(
        !stderr.contains("restore the previous foreground"),
        "{stderr}"
    );
}

#[test]
#[ignore = "Uses an interactive Windows desktop and moves only a disposable test window"]
fn nudges_animated_windows_restores_after_cancel_and_captures_maximized_windows() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let work = capture::list_monitors().unwrap()[0].work;
    unsafe {
        assert_ne!(
            SetWindowPos(
                fixture.hwnd,
                null_mut(),
                work.left,
                work.top,
                440,
                280,
                SWP_NOZORDER | SWP_NOACTIVATE
            ),
            0
        );
        SetWindowLongPtrW(fixture.hwnd, GWLP_USERDATA, 1);
        assert_ne!(SetTimer(fixture.hwnd, 2, 20, None), 0);
    }
    let edge = capture::window_info(fixture.hwnd as usize).unwrap().rect;
    let pair = capture::capture(
        fixture.hwnd as usize,
        &CaptureOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        capture::window_info(fixture.hwnd as usize).unwrap().rect,
        edge
    );
    assert!(!pair.opaque_regions.is_empty());
    let image = recover(
        &pair.white,
        &pair.black,
        &AlphaOptions {
            crop: false,
            opaque_regions: pair.opaque_regions,
            ..Default::default()
        },
    )
    .unwrap()
    .image;
    let x = image.width() / 2;
    let y = image.height() / 2;
    assert_eq!(&image[(x, y)].0[..3], &pair.black[(x, y)].0);
    assert_eq!(image[(x, y)][3], 255);
    assert_eq!(image[(0, 0)][3], 0);
    unsafe {
        KillTimer(fixture.hwnd, 2);
    }

    let cancelled = std::sync::Arc::new(AtomicBool::new(false));
    let signal = std::sync::Arc::clone(&cancelled);
    let handle = fixture.hwnd as usize;
    let timer = std::thread::spawn(move || {
        // Cancel after the capture nudges the window inward, so the cancel lands mid-capture.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while capture::window_info(handle).map(|info| info.rect).ok() == Some(edge)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        signal.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let error = capture::capture(
        fixture.hwnd as usize,
        &CaptureOptions::default(),
        &cancelled,
    )
    .err()
    .unwrap();
    timer.join().unwrap();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert_eq!(
        capture::window_info(fixture.hwnd as usize).unwrap().rect,
        edge
    );

    unsafe {
        ShowWindow(fixture.hwnd, SW_MAXIMIZE);
    }
    let pair = capture::capture(
        fixture.hwnd as usize,
        &CaptureOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_ne!(unsafe { IsZoomed(fixture.hwnd) }, 0);
    let frame = frame(fixture.hwnd);
    assert_eq!(
        pair.white.dimensions(),
        (frame.width() as u32, frame.height() as u32)
    );
}

#[test]
#[ignore = "Uses an interactive Windows desktop, moves the cursor over a disposable test window and mirrors it"]
fn picks_top_level_windows_and_keeps_mirrored_clients_opaque() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let class: Vec<u16> = "BUTTON".encode_utf16().chain(Some(0)).collect();
    let child = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE,
            20,
            20,
            120,
            60,
            fixture.hwnd,
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    };
    assert!(!child.is_null());
    let mut point = POINT { x: 80, y: 50 };
    unsafe {
        ClientToScreen(fixture.hwnd, &mut point);
        SetCursorPos(point.x, point.y);
        // The new window may need a moment to be shown above other windows.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while WindowFromPoint(point) != child && std::time::Instant::now() < until {
            let mut message = zeroed();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            WindowFromPoint(point),
            child,
            "Button must be under the cursor"
        );
    }
    assert_eq!(capture::pick_cursor(false).unwrap(), fixture.hwnd as usize);
    let error = capture::capture(
        child as usize,
        &CaptureOptions::default(),
        &AtomicBool::new(false),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("child control"), "{error}");
    unsafe {
        DestroyWindow(child);
        let style = GetWindowLongPtrW(fixture.hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(fixture.hwnd, GWL_EXSTYLE, style | WS_EX_LAYOUTRTL as isize);
        SetWindowPos(
            fixture.hwnd,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        // Mirrored: client x = 0 maps to the right edge of the client area.
        let mut origin = POINT::default();
        ClientToScreen(fixture.hwnd, &mut origin);
        assert!(origin.x > frame(fixture.hwnd).left + 100);
        SetWindowLongPtrW(fixture.hwnd, GWLP_USERDATA, 1);
        assert_ne!(SetTimer(fixture.hwnd, 2, 20, None), 0);
    }
    let pair = capture::capture(
        fixture.hwnd as usize,
        &CaptureOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    unsafe {
        KillTimer(fixture.hwnd, 2);
    }
    let image = recover(
        &pair.white,
        &pair.black,
        &AlphaOptions {
            crop: false,
            opaque_regions: pair.opaque_regions.clone(),
            ..Default::default()
        },
    )
    .unwrap()
    .image;
    let (x, y) = (image.width() / 2, image.height() / 2);
    assert!(
        pair.opaque_regions
            .iter()
            .any(|r| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height),
        "The client center must be opaque: {:?}",
        pair.opaque_regions
    );
    assert_eq!(image[(x, y)][3], 255);
    assert_eq!(&image[(x, y)].0[..3], &pair.black[(x, y)].0);
}

unsafe extern "system" fn menu_fixture_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe {
        if message == WM_CANCELMODE || (message == WM_TIMER && wparam == 1) {
            EndMenu();
            0
        } else {
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }
}

struct MenuFixture {
    hwnd: HWND,
    worker: Option<std::thread::JoinHandle<()>>,
    finished: std::sync::Arc<AtomicBool>,
}
impl MenuFixture {
    fn new(main_menu: bool) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let finished = std::sync::Arc::new(AtomicBool::new(false));
        let worker_finished = std::sync::Arc::clone(&finished);
        let worker = std::thread::spawn(move || {
            let fixture = Fixture::new();
            unsafe {
                SetWindowLongPtrW(
                    fixture.hwnd,
                    GWLP_WNDPROC,
                    menu_fixture_proc as *const () as isize,
                );
                let bar = CreateMenu();
                let menu = CreatePopupMenu();
                assert!(!bar.is_null() && !menu.is_null());
                let label: Vec<u16> = "Wide menu item "
                    .repeat(8)
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                assert_ne!(AppendMenuW(menu, MF_STRING, 1, label.as_ptr()), 0);
                let title: Vec<u16> = "&File".encode_utf16().chain(Some(0)).collect();
                assert_ne!(AppendMenuW(bar, MF_POPUP, menu as usize, title.as_ptr()), 0);
                assert_ne!(SetMenu(fixture.hwnd, bar), 0);
                DrawMenuBar(fixture.hwnd);
                let settled = std::time::Instant::now() + std::time::Duration::from_millis(150);
                while std::time::Instant::now() < settled {
                    let mut message = zeroed();
                    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                // Bound cleanup if the test fails during the Windows modal menu loop.
                assert_ne!(SetTimer(fixture.hwnd, 1, 15_000, None), 0);
                sender.send(fixture.hwnd as usize).unwrap();
                if main_menu {
                    SendMessageW(
                        fixture.hwnd,
                        WM_SYSCOMMAND,
                        SC_KEYMENU as usize,
                        i32::from(b'f') as isize,
                    );
                } else {
                    let rect = capture::window_info(fixture.hwnd as usize).unwrap().rect;
                    TrackPopupMenuEx(
                        menu,
                        TPM_RETURNCMD | TPM_NONOTIFY,
                        rect.left + 20,
                        rect.top + 50,
                        fixture.hwnd,
                        null(),
                    );
                }
                while !worker_finished.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut message = zeroed();
                    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                KillTimer(fixture.hwnd, 1);
            }
        });
        Self {
            hwnd: receiver.recv().unwrap() as HWND,
            worker: Some(worker),
            finished,
        }
    }

    fn menu(&self) -> Option<HWND> {
        let class: Vec<u16> = "#32768".encode_utf16().chain(Some(0)).collect();
        let thread = unsafe { GetWindowThreadProcessId(self.hwnd, null_mut()) };
        let mut previous = null_mut();
        loop {
            let menu = unsafe { FindWindowExW(null_mut(), previous, class.as_ptr(), null()) };
            if menu.is_null() {
                return None;
            }
            if unsafe { GetWindowThreadProcessId(menu, null_mut()) } == thread
                && unsafe { IsWindowVisible(menu) } != 0
            {
                return Some(menu);
            }
            previous = menu;
        }
    }
}
impl Drop for MenuFixture {
    fn drop(&mut self) {
        self.finished
            .store(true, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            PostMessageW(self.hwnd, WM_CANCELMODE, 0, 0);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn frame(hwnd: HWND) -> Rect {
    let mut bounds = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut bounds as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        ) < 0
        {
            assert_ne!(GetWindowRect(hwnd, &mut bounds), 0);
        }
    }
    Rect {
        left: bounds.left,
        top: bounds.top,
        right: bounds.right,
        bottom: bounds.bottom,
    }
}

#[test]
#[ignore = "Uses an interactive Windows desktop and opens a disposable window's main menu"]
fn captures_menu_wider_than_parent_with_zero_extra_padding() {
    let _desktop = DESKTOP_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for main_menu in [true, false] {
        let fixture = MenuFixture::new(main_menu);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let menu = loop {
            if let Some(menu) = fixture.menu() {
                break menu;
            }
            assert!(
                std::time::Instant::now() < until,
                "The test window's File menu did not open"
            );
            std::thread::sleep(std::time::Duration::from_millis(15));
        };
        // Wait for the menu-opening animation before recording stationary bounds.
        std::thread::sleep(std::time::Duration::from_millis(350));
        let parent = frame(fixture.hwnd);
        let menu_bounds = frame(menu);
        unsafe {
            let owner_thread = GetWindowThreadProcessId(fixture.hwnd, null_mut());
            let menu_thread = GetWindowThreadProcessId(menu, null_mut());
            let mut gui = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..zeroed()
            };
            assert_ne!(GetGUIThreadInfo(owner_thread, &mut gui), 0);
            assert_eq!(owner_thread, menu_thread);
            assert_ne!(gui.flags & GUI_INMENUMODE, 0);
            assert_eq!(gui.hwndMenuOwner, fixture.hwnd);
        }
        assert!(menu_bounds.width() > parent.width());
        assert!(menu_bounds.right > parent.right);
        let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(fixture.hwnd) };
        let (l, t, r, b) = capture::shadow_margins(2, dpi).unwrap();
        let parent_capture = parent.inflate(l, t, r, b);
        let (l, t, r, b) = capture::shadow_margins(1, dpi).unwrap();
        let expected = parent_capture.union(menu_bounds.inflate(l, t, r, b));
        let (white, black, recovered) = if main_menu {
            let directory = tempfile::tempdir().unwrap();
            let output = directory.path().join("menu.png");
            let result = std::process::Command::new(env!("CARGO_BIN_EXE_alphashot"))
                .args([
                    "--window-hwnd",
                    &format!("0x{:X}", fixture.hwnd as usize),
                    "--pad",
                    "0",
                    "--margin",
                    "1",
                    "--include-cursor",
                    "--no-crop",
                    "--no-preview",
                    "--raw-dir",
                ])
                .arg(directory.path())
                .arg("-o")
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            (
                image::open(directory.path().join("capture-white.png"))
                    .unwrap()
                    .to_rgb8(),
                image::open(directory.path().join("capture-black.png"))
                    .unwrap()
                    .to_rgb8(),
                image::open(output).unwrap().to_rgba8(),
            )
        } else {
            let pair = capture::capture(
                fixture.hwnd as usize,
                &CaptureOptions {
                    pad: 0,
                    include_cursor: true,
                    ..Default::default()
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            let recovered = recover(
                &pair.white,
                &pair.black,
                &AlphaOptions {
                    crop: false,
                    overlays: pair.cursor,
                    ..Default::default()
                },
            )
            .unwrap()
            .image;
            (pair.white, pair.black, recovered)
        };
        assert_eq!(
            white.dimensions(),
            (expected.width() as u32, expected.height() as u32)
        );
        assert_eq!(white.dimensions(), black.dimensions());
        assert_eq!(
            fixture.menu(),
            Some(menu),
            "Capture must leave the menu open"
        );
        let x = menu_bounds.right - 8;
        let y = menu_bounds.top + menu_bounds.height() / 2;
        assert!(x >= parent.right, "Probe must be outside the parent window");
        assert_eq!(
            recovered[((x - expected.left) as u32, (y - expected.top) as u32)][3],
            255
        );
        let foreground_before = unsafe { GetForegroundWindow() };
        if !main_menu {
            for options in [
                CaptureOptions {
                    focus: FocusState::Unfocused,
                    ..Default::default()
                },
                CaptureOptions {
                    high_dpi: true,
                    ..Default::default()
                },
                CaptureOptions {
                    client_size: Some((640, 420)),
                    ..Default::default()
                },
            ] {
                assert!(
                    capture::capture(fixture.hwnd as usize, &options, &AtomicBool::new(false))
                        .is_err()
                );
                assert_eq!(fixture.menu(), Some(menu));
                assert_eq!(unsafe { GetForegroundWindow() }, foreground_before);
            }
        }
    }
}
