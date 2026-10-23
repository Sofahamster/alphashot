//! Opt-in desktop tests of shadow-free capture: the standard-frame corner mask and layered source alpha.
#![allow(clippy::undocumented_unsafe_blocks)]

use super::*;
use windows_sys::Win32::Foundation::SIZE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

fn source_alpha(x: usize, y: usize, width: usize, height: usize) -> u8 {
    let content = if y < 24 {
        128
    } else {
        [0_u16, 64, 128, 255][x * 4 / width]
    };
    (content * source_coverage(x, y, width, height) / 255) as u8
}

fn source_coverage(x: usize, y: usize, width: usize, height: usize) -> u16 {
    let (cx, cy) = (x.min(width - 1 - x), y.min(height - 1 - y));
    if cx < 4 && cy < 4 {
        [
            [0, 0, 64, 128],
            [0, 128, 255, 255],
            [64, 255, 255, 255],
            [128, 255, 255, 255],
        ][cy][cx]
    } else {
        255
    }
}

pub(super) struct Fixture {
    pub(super) hwnd: HWND,
    class: Vec<u16>,
    brush: HBRUSH,
    foreground: HWND,
    cursor: POINT,
}

unsafe extern "system" fn custom_frame_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCALCSIZE {
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

impl Fixture {
    pub(super) fn new(custom: bool, layered: bool, bounds: Rect, activate: bool) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let mut fixture = Self {
            hwnd: null_mut(),
            class: wide(&format!(
                "AlphashotThumbnailProbe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
            brush: unsafe { CreateSolidBrush(0x002864c8) },
            foreground: unsafe { GetForegroundWindow() },
            cursor: POINT::default(),
        };
        unsafe {
            let instance = GetModuleHandleW(null());
            assert_ne!(GetCursorPos(&mut fixture.cursor), 0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(if custom {
                    custom_frame_proc
                } else {
                    overlay_proc
                }),
                hInstance: instance,
                hbrBackground: fixture.brush,
                lpszClassName: fixture.class.as_ptr(),
                ..zeroed()
            };
            assert_ne!(RegisterClassW(&class), 0);
            fixture.hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW
                    | if activate { 0 } else { WS_EX_NOACTIVATE }
                    | if layered { WS_EX_LAYERED } else { 0 },
                fixture.class.as_ptr(),
                wide("Alphashot shadow probe").as_ptr(),
                if custom || layered {
                    WS_POPUP | WS_THICKFRAME
                } else {
                    WS_OVERLAPPEDWINDOW
                },
                bounds.left,
                bounds.top,
                bounds.width(),
                bounds.height(),
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!fixture.hwnd.is_null());
            if layered {
                let surface = Surface::new(bounds.width(), bounds.height()).unwrap();
                let pixels = std::slice::from_raw_parts_mut(
                    surface.bits,
                    bounds.width() as usize * bounds.height() as usize * 4,
                );
                for (i, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = (i % bounds.width() as usize, i / bounds.width() as usize);
                    let width = bounds.width() as usize;
                    let height = bounds.height() as usize;
                    let alpha = u16::from(source_alpha(x, y, width, height));
                    pixel.copy_from_slice(&[
                        (40 * alpha / 255) as u8,
                        (100 * alpha / 255) as u8,
                        (200 * alpha / 255) as u8,
                        alpha as u8,
                    ]);
                }
                let size = SIZE {
                    cx: bounds.width(),
                    cy: bounds.height(),
                };
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                    ..zeroed()
                };
                assert_ne!(
                    UpdateLayeredWindow(
                        fixture.hwnd,
                        null_mut(),
                        null(),
                        &size,
                        surface.dc,
                        &POINT::default(),
                        0,
                        &blend,
                        ULW_ALPHA,
                    ),
                    0
                );
            }
            ShowWindow(fixture.hwnd, SW_SHOWNOACTIVATE);
            UpdateWindow(fixture.hwnd);
            let activation_error = if activate {
                windows_sys::Win32::Foundation::SetLastError(0);
                let activated = SetForegroundWindow(fixture.hwnd);
                let error = windows_sys::Win32::Foundation::GetLastError();
                (activated == 0).then_some(error)
            } else {
                None
            };
            if let Some(error) = activation_error {
                eprintln!("Fixture SetForegroundWindow refused; last error {error}");
                assert_ne!(
                    SetWindowPos(
                        fixture.hwnd,
                        HWND_TOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE
                    ),
                    0
                );
                let moved = SetCursorPos(
                    bounds.left + bounds.width() * 7 / 8,
                    bounds.top + bounds.height() / 2,
                );
                let move_error = windows_sys::Win32::Foundation::GetLastError();
                assert_ne!(
                    moved, 0,
                    "Fixture SetCursorPos refused; last error {move_error}"
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
                let until = Instant::now() + Duration::from_secs(1);
                while GetForegroundWindow() != fixture.hwnd && Instant::now() < until {
                    wait(Duration::from_millis(10), &AtomicBool::new(false)).unwrap();
                }
                assert_ne!(
                    SetWindowPos(
                        fixture.hwnd,
                        HWND_NOTOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE
                    ),
                    0
                );
                assert_ne!(SetCursorPos(fixture.cursor.x, fixture.cursor.y), 0);
            }
            if activate {
                assert_eq!(GetForegroundWindow(), fixture.hwnd);
            }
        }
        fixture
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            if !self.hwnd.is_null() {
                DestroyWindow(self.hwnd);
            }
            UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
            DeleteObject(self.brush);
            SetCursorPos(self.cursor.x, self.cursor.y);
            if !self.foreground.is_null() && GetForegroundWindow() != self.foreground {
                SetForegroundWindow(self.foreground);
            }
        }
    }
}

struct WgcSource {
    request: std::sync::mpsc::Sender<Option<(usize, bool)>>,
    response: std::sync::mpsc::Receiver<Result<image::RgbaImage>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl WgcSource {
    fn new() -> Self {
        let (request, incoming) = std::sync::mpsc::channel();
        let (outgoing, response) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let source = crate::capture::snapshot::Snapshotter::new().unwrap();
            while let Ok(Some((handle, no_shadow))) = incoming.recv() {
                let image = if no_shadow {
                    capture_wgc(
                        &source,
                        handle,
                        &CaptureOptions {
                            backend: CaptureBackend::Wgc,
                            no_shadow: true,
                            ..Default::default()
                        },
                        &AtomicBool::new(false),
                    )
                    .map(|capture| capture.image)
                } else {
                    source.capture(handle, || false)
                };
                outgoing.send(image).unwrap();
            }
        });
        Self {
            request,
            response,
            worker: Some(worker),
        }
    }

    fn capture(&self, hwnd: HWND) -> image::RgbaImage {
        self.capture_with(hwnd, false)
    }

    fn capture_with(&self, hwnd: HWND, no_shadow: bool) -> image::RgbaImage {
        self.request.send(Some((hwnd as usize, no_shadow))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match self.response.try_recv() {
                Ok(image) => return image.unwrap(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => panic!("WGC worker stopped"),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "WGC fixture capture timed out");
            wait(Duration::from_millis(4), &AtomicBool::new(false)).unwrap();
        }
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        let _ = self.request.send(None);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
#[ignore = "Serial desktop test: native corner mask under an occluder, custom-caption and region refusal, exact layered alpha"]
fn corner_mask_keeps_covered_native_isolated_and_excludes_custom_sources() {
    enable_per_monitor_dpi();
    let bounds = centered(list_monitors().unwrap()[0].work, 320, 220);
    let options = CaptureOptions {
        backend: CaptureBackend::Wgc,
        no_shadow: true,
        ..Default::default()
    };
    let source = WgcSource::new();
    let custom = Fixture::new(true, false, bounds, false);
    unsafe {
        SetWindowLongPtrW(
            custom.hwnd,
            GWL_STYLE,
            (WS_OVERLAPPEDWINDOW | WS_VISIBLE) as isize,
        );
        assert_ne!(
            SetWindowPos(
                custom.hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
            ),
            0
        );
    }
    let error = validate_no_shadow(custom.hwnd, &options).unwrap_err();
    assert!(error.to_string().contains("custom chrome"));
    let target = Fixture::new(false, false, bounds, false);
    flush();
    wait(Duration::from_millis(180), &AtomicBool::new(false)).unwrap();
    let foreground = unsafe { GetForegroundWindow() };
    let placement = window_rect(target.hwnd).unwrap();
    let before = source.capture_with(target.hwnd, true);
    let cover = Overlay::new(0x0000ff00, false).unwrap();
    cover
        .place(HWND_TOPMOST, extended(target.hwnd).unwrap())
        .unwrap();
    flush();
    wait(Duration::from_millis(120), &AtomicBool::new(false)).unwrap();
    assert_eq!(source.capture_with(target.hwnd, true), before);
    assert_eq!(before[(0, 0)], image::Rgba([0; 4]));
    assert_eq!(
        before[(before.width() / 2, before.height() / 2)],
        image::Rgba([200, 100, 40, 255])
    );
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    assert_eq!(window_rect(target.hwnd).unwrap(), placement);
    unsafe {
        // SetWindowRgn owns the region on success; destroying the fixture releases it.
        assert_ne!(
            SetWindowRgn(target.hwnd, CreateRectRgn(0, 0, 160, 110), 0),
            0
        );
    }
    assert!(validate_no_shadow(target.hwnd, &options).is_err());
    let layered = Fixture::new(false, true, bounds, false);
    flush();
    wait(Duration::from_millis(120), &AtomicBool::new(false)).unwrap();
    assert_eq!(
        validate_no_shadow(layered.hwnd, &options)
            .unwrap()
            .corner_radius,
        0.0
    );
    let native = source.capture(layered.hwnd);
    assert_eq!(source.capture_with(layered.hwnd, true), native);
    assert_eq!(native[(120, 110)][3], 64);
    assert_eq!(native[(200, 110)][3], 128);
}

#[test]
#[ignore = "Serial desktop test: known layered alpha, translucent custom title bar and actual antialiased source corners"]
fn no_shadow_layered_capture_preserves_known_pixels() {
    enable_per_monitor_dpi();
    let bounds = centered(list_monitors().unwrap()[0].work, 320, 220);
    let target = Fixture::new(false, true, bounds, true);
    let foreground = unsafe { GetForegroundWindow() };
    flush();
    wait(Duration::from_millis(150), &AtomicBool::new(false)).unwrap();
    let before = window_info(target.hwnd as usize).unwrap();
    let pair = capture(
        target.hwnd as usize,
        &CaptureOptions {
            no_shadow: true,
            ..Default::default()
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(pair.white.dimensions(), (320, 220));
    assert!(pair.opaque_regions.is_empty());
    assert_eq!(pair.blend_space, crate::alpha::BlendSpace::Srgb);
    let image = crate::alpha::recover(
        &pair.white,
        &pair.black,
        &crate::alpha::AlphaOptions {
            crop: false,
            opaque_regions: pair.opaque_regions,
            blend_space: pair.blend_space,
            ..Default::default()
        },
    )
    .unwrap()
    .image;
    let expected = [
        ((40, 110), 0_u8),
        ((120, 110), 64),
        ((200, 110), 128),
        ((280, 110), 255),
        ((280, 12), 128),
        ((317, 219), 64),
        ((318, 218), 128),
        ((319, 219), 0),
    ];
    for &((x, y), alpha) in &expected {
        eprintln!(
            "known layered pixel ({x},{y}): expected=[200,100,40,{alpha}], actual={:?}, white={:?}, black={:?}",
            image[(x, y)].0,
            pair.white[(x, y)].0,
            pair.black[(x, y)].0
        );
    }
    assert_eq!(window_rect(target.hwnd).unwrap(), before.rect);
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    for ((x, y), alpha) in expected {
        assert!(
            image[(x, y)][3].abs_diff(alpha) <= 1,
            "alpha at ({x},{y}): {:?}, expected{alpha}",
            image[(x, y)].0
        );
        if alpha >= 64 {
            for (actual, expected) in image[(x, y)].0[..3].iter().zip([200_u8, 100, 40]) {
                assert!(
                    actual.abs_diff(expected) <= 2,
                    "color at ({x},{y}): {:?}",
                    image[(x, y)].0
                );
            }
        }
    }
    for (x, y, pixel) in image.enumerate_pixels() {
        let alpha = source_alpha(x as usize, y as usize, 320, 220);
        assert!(
            pixel[3].abs_diff(alpha) <= 1,
            "Source coverage at ({x},{y}): {:?}, expected alpha {alpha}",
            pixel.0
        );
        if alpha == 0 {
            assert_eq!(pixel.0, [0; 4]);
        } else {
            for (channel, source) in pixel.0[..3].iter().zip([200_u16, 100, 40]) {
                let premultiplied = source * u16::from(alpha) / 255;
                let quantized =
                    ((f32::from(premultiplied) * 255.0 / f32::from(alpha)).round()) as u8;
                assert!(
                    channel.abs_diff(quantized) <= 1,
                    "Source color including 8-bit premultiplication at ({x},{y}): {:?}, expected channel {quantized}",
                    pixel.0
                );
            }
        }
    }
}
