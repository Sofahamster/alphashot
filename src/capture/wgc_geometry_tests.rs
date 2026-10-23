//! Serial, opt-in real-DPI capture tests; ordinary checks never move desktop windows.
#![allow(clippy::undocumented_unsafe_blocks)]

use super::*;

fn placement(hwnd: HWND) -> (u32, u32, [i32; 8]) {
    let mut value = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..unsafe { zeroed() }
    };
    assert_ne!(unsafe { GetWindowPlacement(hwnd, &mut value) }, 0);
    let min = if value.flags & WPF_SETMINPOSITION == 0 {
        POINT::default()
    } else {
        value.ptMinPosition
    };
    (
        value.showCmd,
        value.flags,
        [
            value.rcNormalPosition.left,
            value.rcNormalPosition.top,
            value.rcNormalPosition.right,
            value.rcNormalPosition.bottom,
            min.x,
            min.y,
            value.ptMaxPosition.x,
            value.ptMaxPosition.y,
        ],
    )
}

#[test]
#[ignore = "Serial desktop test: needs a lower-DPI source and a scaled physical/virtual destination; temporarily moves a disposable window"]
fn wgc_high_dpi_restores_geometry_and_selects_masks_after_dpi_changes() {
    enable_per_monitor_dpi();
    let monitors = list_monitors().unwrap();
    let desired = high_dpi_display(&monitors, None).unwrap().clone();
    let original_monitor = monitors.iter().min_by_key(|m| m.dpi).unwrap();
    assert!(
        original_monitor.dpi < desired.dpi,
        "Enable a higher-DPI extended display before running this test"
    );
    let fixture = shadow_tests::Fixture::new(
        false,
        false,
        centered(original_monitor.work, 480, 320),
        false,
    );
    flush();
    wait(Duration::from_millis(150), &AtomicBool::new(false)).unwrap();
    let handle = fixture.hwnd as usize;
    let before = window_info(handle).unwrap();
    let original_placement = placement(fixture.hwnd);
    let original_handle = original_monitor.handle;
    let foreground = unsafe { GetForegroundWindow() } as usize;
    let worker = thread::spawn(move || -> Result<()> {
        let source = snapshot::Snapshotter::new()?;
        let base = CaptureOptions {
            backend: CaptureBackend::Wgc,
            pad: 0,
            ..Default::default()
        };
        let restored = || {
            let actual = window_info(handle).unwrap();
            assert_eq!((actual.rect, actual.dpi), (before.rect, before.dpi));
            assert_eq!(placement(handle as HWND), original_placement);
            assert_eq!(
                unsafe { MonitorFromWindow(handle as HWND, MONITOR_DEFAULTTONEAREST) } as usize,
                original_handle
            );
            assert_eq!(unsafe { GetForegroundWindow() } as usize, foreground);
        };
        let direct = capture_wgc(&source, handle, &base, &AtomicBool::new(false))?;
        assert_eq!(
            (direct.window.rect, direct.window.dpi),
            (before.rect, before.dpi)
        );
        restored();
        let resized = capture_wgc(
            &source,
            handle,
            &CaptureOptions {
                client_size: Some((640, 400)),
                ..base.clone()
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(
            (resized.window.rect.left, resized.window.rect.top),
            (before.rect.left, before.rect.top),
            "Exact resize without relocation must preserve position"
        );
        assert!(resized.image.width() >= 640 && resized.image.height() > 400);
        restored();
        let high = CaptureOptions {
            high_dpi: true,
            high_dpi_display: Some(desired.name.clone()),
            no_shadow: true,
            ..base.clone()
        };
        for corner in [DWMWCP_ROUND, DWMWCP_ROUNDSMALL] {
            unsafe {
                assert!(
                    DwmSetWindowAttribute(
                        handle as HWND,
                        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                        (&corner as *const i32).cast(),
                        size_of::<i32>() as u32
                    ) >= 0
                );
            }
            let captured = capture_wgc(&source, handle, &high, &AtomicBool::new(false))?;
            assert_eq!(captured.window.dpi, desired.dpi);
            assert!(desired.rect.contains(captured.window.rect));
            assert_eq!(
                captured.image[(captured.image.width() / 2, captured.image.height() / 2)].0,
                [200, 100, 40, 255]
            );
            let mask = crate::alpha::finish(
                RgbaImage::from_pixel(
                    captured.image.width(),
                    captured.image.height(),
                    image::Rgba([255; 4]),
                ),
                &crate::alpha::AlphaOptions {
                    crop: false,
                    corner_radius: if corner == DWMWCP_ROUND { 8.0 } else { 4.0 }
                        * desired.dpi as f32
                        / 96.0,
                    ..Default::default()
                },
            )?
            .image;
            assert!(
                captured
                    .image
                    .pixels()
                    .zip(mask.pixels())
                    .all(|(pixel, mask)| pixel[3] <= mask[3]),
                "Mask must use destination physical DPI"
            );
            restored();
        }
        let sized = CaptureOptions {
            client_size: Some((640, 400)),
            ..high.clone()
        };
        let mut geometry =
            prepare_wgc_geometry(handle as HWND, &before, &sized, &AtomicBool::new(false))?
                .unwrap();
        let client = client_rect(handle as HWND)?;
        assert_eq!((client.width(), client.height()), (640, 400));
        let at_destination = window_info(handle)?;
        let destination_placement = placement(handle as HWND);
        let exact = capture_wgc(&source, handle, &sized, &AtomicBool::new(false))?;
        assert_eq!(exact.window.dpi, desired.dpi);
        assert!(exact.image.width() >= 640 && exact.image.height() > 400);
        assert_eq!(window_info(handle)?.rect, at_destination.rect);
        assert_eq!(placement(handle as HWND), destination_placement);
        geometry.geometry()?;
        restored();
        // WGC rejects protected targets only after geometry preparation, exercising failure cleanup.
        unsafe {
            assert_ne!(
                SetWindowDisplayAffinity(handle as HWND, WDA_EXCLUDEFROMCAPTURE),
                0
            );
        }
        let failed = capture_wgc(&source, handle, &high, &AtomicBool::new(false));
        unsafe {
            assert_ne!(SetWindowDisplayAffinity(handle as HWND, WDA_NONE), 0);
        }
        assert!(failed.is_err());
        restored();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let dpi = desired.dpi;
        let cancellation = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if unsafe { GetDpiForWindow(handle as HWND) } == dpi {
                    trigger.store(true, Ordering::Relaxed);
                    return true;
                }
                thread::sleep(Duration::from_millis(2));
            }
            false
        });
        let result = capture_wgc(&source, handle, &high, &cancelled);
        assert!(
            cancellation.join().unwrap(),
            "Cancel only after the target actually moved"
        );
        assert!(result.err().unwrap().to_string().contains("cancelled"));
        restored();
        Ok(())
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline, "WGC high-DPI task stalled");
        wait(Duration::from_millis(4), &AtomicBool::new(false)).unwrap();
    }
    worker.join().unwrap().unwrap();
}
