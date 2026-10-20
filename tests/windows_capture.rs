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

#[path = "../src/sixel.rs"]
mod sixel;

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
            assert!(image.pixels().all(|p| p[3] == 255));
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
        let _duplicate = Fixture::new();
        let ambiguous = std::process::Command::new(env!("CARGO_BIN_EXE_alphashot"))
            .args(["--window-title", "Alphashot capture test"])
            .output()
            .unwrap();
        assert_eq!(ambiguous.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&ambiguous.stderr).contains("2 windows matched --window-title")
        );
        assert!(ambiguous.stdout.is_empty());
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
