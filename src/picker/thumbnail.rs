//! Background thumbnail generation for the picker's preview pane.
//!
//! Capture and Sixel encoding run on one worker thread (`window-preview`). The UI thread posts a
//! `Request` and polls for a result without blocking.
//!
//! The worker holds at most one running job and one pending request. A new request replaces the
//! pending one and discards any finished result, so fast scrolling captures only the row where the
//! user stops. A request starts after it is `DEBOUNCE` old. Each request gets a generation number
//! from an atomic counter. Capture and encode check it through a `cancelled` callback, and
//! `Worker::poll` drops results from an older generation. The picker bumps `Request::epoch` on F5,
//! and the worker clears its cache when the epoch changes.
//!
//! The cache holds `CACHE_SIZE` snapshots in most-recently-used order, each downscaled to fit
//! 1024x768, with the Sixel encodings of its two most recent pixel sizes. Failed captures are not
//! cached, so the next selection retries.
//!
//! On Windows the worker captures with `capture::snapshot::Snapshotter` (Windows Graphics Capture),
//! which reads window content without activating, raising, or moving the window. It checks that the
//! handle still belongs to the same process id to reject a recycled handle. On other platforms
//! capture always fails. If the worker thread dies, `Preview::poll` marks the preview as failed.

use anyhow::Result;
use image::{RgbaImage, imageops};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Delay between a request being posted and the worker starting it.
const DEBOUNCE: Duration = Duration::from_millis(100);
/// Number of window snapshots kept by the worker.
const CACHE_SIZE: usize = 4;

/// Identity of a window for caching. The process id guards against handle reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Window {
    pub handle: usize,
    pub pid: u32,
}

/// A preview request. `pixels` is the largest image size in pixels that fits the pane, and
/// `epoch` invalidates the worker cache when it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub window: Window,
    pub pixels: (u32, u32),
    pub epoch: u64,
}

/// A Sixel-encoded thumbnail and the pixel size of the encoded image, which the drawing code
/// needs to center it in the pane.
pub(super) struct Encoded {
    pub data: String,
    pub pixels: (u32, u32),
}
/// Captures a window. The callback reports whether the request was superseded, so a long
/// capture can stop early.
type Capture = Box<dyn FnMut(Window, &dyn Fn() -> bool) -> Result<RgbaImage>>;
/// A finished job: the generation it was started for and the image, or `None` on failure.
type Completion = (u64, Option<Arc<Encoded>>);

/// State shared between the UI thread and the worker, protected by a mutex. `pending` is the
/// single replaceable request with its post time and generation. `complete` is the latest
/// result not yet taken.
#[derive(Default)]
struct Shared {
    pending: Option<(Request, Instant, u64)>,
    complete: Option<Completion>,
    stop: bool,
}

/// Handle to the worker thread. `version` is the current generation; bumping it cancels
/// in-flight work.
pub(super) struct Worker {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    version: Arc<AtomicU64>,
    thread: Option<thread::JoinHandle<()>>,
}

/// A cached snapshot (`None` if capture failed) and its recent encodings, newest first.
struct Cached {
    window: Window,
    image: Option<RgbaImage>,
    encodings: VecDeque<((u32, u32), Arc<Encoded>)>,
}

impl Worker {
    /// Starts the worker thread. `factory` runs on the worker thread so thread-affine capture
    /// state is created there.
    fn spawn(factory: impl FnOnce() -> Capture + Send + 'static) -> Result<Self> {
        let shared = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        let version = Arc::new(AtomicU64::new(0));
        let (state, serial) = (shared.clone(), version.clone());
        let thread = thread::Builder::new()
            .name("window-preview".into())
            .spawn(move || {
                let mut capture = factory();
                let (mut completed, mut epoch) = (0, 0);
                let mut cache = VecDeque::<Cached>::new();
                loop {
                    let (request, generation) = {
                        let (lock, changed) = &*state;
                        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if state.stop {
                                return;
                            }
                            if let Some((request, since, generation)) = state.pending
                                && generation != completed
                            {
                                let remaining = DEBOUNCE.saturating_sub(since.elapsed());
                                if remaining.is_zero() {
                                    break (request, generation);
                                }
                                state = changed
                                    .wait_timeout(state, remaining)
                                    .unwrap_or_else(|e| e.into_inner())
                                    .0;
                                continue;
                            }
                            state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let cancelled = || serial.load(Ordering::Relaxed) != generation;
                    if request.epoch != epoch {
                        cache.clear();
                        epoch = request.epoch;
                    }
                    let position = cache
                        .iter()
                        .position(|entry| entry.window == request.window);
                    let mut entry = position.and_then(|i| cache.remove(i)).unwrap_or_else(|| {
                        let image = capture(request.window, &cancelled).ok().map(downsize);
                        Cached {
                            window: request.window,
                            image,
                            encodings: VecDeque::new(),
                        }
                    });
                    let result = if cancelled() {
                        None
                    } else if let Some((_, encoding)) = entry
                        .encodings
                        .iter()
                        .find(|(size, _)| *size == request.pixels)
                    {
                        Some(encoding.clone())
                    } else {
                        entry.image.as_ref().and_then(|image| {
                            let image =
                                crate::sixel::fit(image, request.pixels.0, request.pixels.1);
                            if cancelled() {
                                return None;
                            }
                            crate::sixel::encode(&image).ok().map(|data| {
                                let encoding = Arc::new(Encoded {
                                    data,
                                    pixels: (image.width(), image.height()),
                                });
                                entry
                                    .encodings
                                    .push_front((request.pixels, encoding.clone()));
                                entry.encodings.truncate(2);
                                encoding
                            })
                        })
                    };
                    // Returning to a window must retry a transient capture failure.
                    if entry.image.is_some() {
                        cache.push_front(entry);
                        cache.truncate(CACHE_SIZE);
                    }
                    let mut state = state.0.lock().unwrap_or_else(|e| e.into_inner());
                    if !cancelled() && !state.stop {
                        state.complete = Some((generation, result));
                    }
                    completed = generation;
                }
            })?;
        Ok(Self {
            shared,
            version,
            thread: Some(thread),
        })
    }

    /// Posts or clears the pending request. An identical request is a no-op so redraws do not
    /// restart the debounce timer.
    fn request(&self, request: Option<Request>) {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending.map(|(key, _, _)| key) == request {
            return;
        }
        let generation = self.version.fetch_add(1, Ordering::Relaxed) + 1;
        state.pending = request.map(|key| (key, Instant::now(), generation));
        state.complete = None;
        self.shared.1.notify_one();
    }

    /// Takes a finished result if it still matches the current generation.
    fn poll(&self) -> Option<Completion> {
        let complete = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .complete
            .take()?;
        (complete.0 == self.version.load(Ordering::Relaxed)).then_some(complete)
    }
}

/// Signals the worker to stop. The thread is joined only if it already finished, because a
/// capture in progress would otherwise delay leaving the picker; it then ends on its own.
impl Drop for Worker {
    fn drop(&mut self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.shared.1.notify_one();
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            let _ = self.thread.take().unwrap().join();
        }
    }
}

/// Preview state seen by the UI: the current request, whether previews are enabled, the
/// epoch sent with requests, the latest image, and whether the last attempt failed.
#[derive(Default)]
pub(super) struct Preview {
    worker: Option<Worker>,
    pub request: Option<Request>,
    pub enabled: bool,
    pub epoch: u64,
    pub image: Option<Arc<Encoded>>,
    pub failed: bool,
}

impl Preview {
    /// Enables previews and spawns the worker. If spawning fails, the preview shows as
    /// unavailable.
    pub fn start(&mut self) {
        self.enabled = true;
        self.worker = Worker::spawn(|| {
            #[cfg(windows)]
            {
                let mut native = None;
                Box::new(move |window: Window, cancelled: &dyn Fn() -> bool| {
                    anyhow::ensure!(
                        alphashot::capture::window_info(window.handle)?.pid == window.pid,
                        "Window was replaced"
                    );
                    if native.is_none() {
                        native = Some(alphashot::capture::snapshot::Snapshotter::new()?);
                    }
                    native.as_ref().unwrap().capture(window.handle, cancelled)
                })
            }
            #[cfg(not(windows))]
            Box::new(|_, _| anyhow::bail!("Window previews require Windows"))
        })
        .ok();
    }

    /// Sets the wanted preview and clears the shown image when it changes, so an old image is
    /// never drawn for a new selection.
    pub fn request(&mut self, request: Option<Request>) {
        if self.request == request {
            return;
        }
        self.request = request;
        self.image = None;
        self.failed = self.worker.is_none();
        if let Some(worker) = &self.worker {
            worker.request(request);
        }
    }

    /// Collects a finished result. Returns true when the screen needs a redraw.
    pub fn poll(&mut self) -> bool {
        if let Some((_, image)) = self.worker.as_ref().and_then(Worker::poll) {
            self.failed = image.is_none();
            self.image = image;
            return true;
        }
        if self.request.is_some()
            && !self.failed
            && self.worker.as_ref().is_some_and(|worker| {
                worker
                    .thread
                    .as_ref()
                    .is_some_and(|thread| thread.is_finished())
            })
        {
            self.image = None;
            self.failed = true;
            return true;
        }
        false
    }
}

/// Scales a snapshot down to fit 1024x768, keeping its aspect ratio.
///
/// `imageops::thumbnail` resizes to exactly the given size, so the target is computed here.
fn downsize(image: RgbaImage) -> RgbaImage {
    let (width, height) = image.dimensions();
    if width <= 1024 && height <= 768 {
        return image;
    }
    let scale = (1024.0 / f64::from(width)).min(768.0 / f64::from(height));
    let width = (f64::from(width) * scale).round().max(1.0) as u32;
    let height = (f64::from(height) * scale).round().max(1.0) as u32;
    imageops::thumbnail(&image, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsizing_keeps_the_aspect_ratio() {
        for (size, expected) in [
            ((1920, 1080), (1024, 576)),
            ((500, 1400), (274, 768)),
            ((4000, 1), (1024, 1)),
            ((640, 480), (640, 480)),
        ] {
            let image = RgbaImage::new(size.0, size.1);
            assert_eq!(downsize(image).dimensions(), expected, "{size:?}");
        }
    }

    fn request(handle: usize, pixels: (u32, u32)) -> Request {
        Request {
            window: Window {
                handle,
                pid: handle as u32,
            },
            pixels,
            epoch: 0,
        }
    }

    fn ready(preview: &mut Preview) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !preview.poll() {
            assert!(Instant::now() < deadline, "Preview did not complete");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn slow_capture_does_not_block_keys_and_pending_work_is_replaced() {
        use clap::Parser;
        use crossterm::event::KeyCode;
        let (started, receive) = std::sync::mpsc::sync_channel(1);
        let (release, gate) = std::sync::mpsc::sync_channel(1);
        let worker = Worker::spawn(move || {
            Box::new(move |window, _| {
                started.send(window.handle).unwrap();
                gate.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(RgbaImage::from_pixel(640, 480, image::Rgba([255; 4])))
            })
        })
        .unwrap();
        let mut preview = Preview {
            worker: Some(worker),
            ..Default::default()
        };
        preview.request(Some(request(1, (168, 144))));
        assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap(), 1);
        let mut picker = super::super::Picker::new(vec![
            super::super::tests::window(1, None, "One"),
            super::super::tests::window(2, None, "Two"),
        ]);
        let mut args = crate::Cli::parse_from(["alphashot"]);
        let start = Instant::now();
        for handle in 2..=100 {
            preview.request(Some(request(handle, (168, 144))));
            picker.key(KeyCode::Down.into(), 10, &mut args).unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "Keyboard handling waited for the capture"
        );
        release.send(()).unwrap();
        assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap(), 100);
        assert!(
            !preview.poll(),
            "The obsolete first capture must never be published"
        );
        release.send(()).unwrap();
        ready(&mut preview);
        let original = preview.image.clone().unwrap();

        preview.request(Some(request(100, (84, 72))));
        ready(&mut preview);
        assert!(!Arc::ptr_eq(&original, preview.image.as_ref().unwrap()));
        preview.request(Some(request(100, (168, 144))));
        ready(&mut preview);
        assert!(
            Arc::ptr_eq(&original, preview.image.as_ref().unwrap()),
            "Reuse the encoding when returning to a cached size"
        );
        assert!(
            receive.try_recv().is_err(),
            "Resizing must not recapture the window"
        );
        preview.request(None);
        assert!(preview.image.is_none());
        preview.request(Some(request(100, (168, 144))));
        ready(&mut preview);
        assert!(
            Arc::ptr_eq(&original, preview.image.as_ref().unwrap()),
            "Hiding and restoring a preview must reuse its encoding"
        );
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn debounce_failures_cache_eviction_and_refresh_stay_local() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let worker = Worker::spawn(move || {
            Box::new(move |window, _| {
                recorded.lock().unwrap().push(window.handle);
                anyhow::ensure!(window.handle != 99, "Mock capture failed");
                Ok(RgbaImage::from_pixel(16, 12, image::Rgba([255; 4])))
            })
        })
        .unwrap();
        let mut preview = Preview {
            worker: Some(worker),
            ..Default::default()
        };
        for handle in 1..=20 {
            preview.request(Some(request(handle, (168, 144))));
        }
        ready(&mut preview);
        assert_eq!(*calls.lock().unwrap(), [20]);
        preview.request(Some(request(99, (168, 144))));
        ready(&mut preview);
        assert!(preview.failed && preview.image.is_none());
        preview.request(Some(request(20, (168, 144))));
        ready(&mut preview);
        preview.request(Some(request(99, (168, 144))));
        ready(&mut preview);
        assert_eq!(
            calls.lock().unwrap().iter().filter(|&&id| id == 99).count(),
            2
        );
        for handle in 21..=25 {
            preview.request(Some(request(handle, (168, 144))));
            ready(&mut preview);
        }
        preview.request(Some(request(20, (168, 144))));
        ready(&mut preview);
        assert_eq!(
            calls.lock().unwrap().iter().filter(|&&id| id == 20).count(),
            2,
            "Old snapshots must be evicted"
        );
        preview.request(Some(Request {
            epoch: 1,
            ..request(20, (168, 144))
        }));
        ready(&mut preview);
        assert_eq!(
            calls.lock().unwrap().iter().filter(|&&id| id == 20).count(),
            3,
            "Explicit refresh must recapture"
        );
    }
}
