//! Native window pixels through Windows Graphics Capture (WGC).
//!
//! WGC reads a window's own composition surface, so covered and partly off-screen windows show
//! their content, and the window is not activated, raised or moved. `BitBlt` or desktop
//! duplication would show whatever covers the window. WGC omits the desktop behind the window,
//! which rules it out for the two-backdrop solve. Its premultiplied alpha is converted to
//! straight RGBA without forcing client pixels opaque.
//!
//! # Threading
//!
//! [`Snapshotter::new`] enters the multithreaded WinRT apartment and creates one hardware D3D11
//! device, which the picker reuses on its background thread because device creation costs more
//! than a snapshot. The type is `!Send` and `!Sync`: the apartment must be left on the thread
//! that entered it, and the immediate context does not support concurrent use.
//!
//! [`Snapshotter::capture`] polls a free-threaded one-buffer frame pool, copies the frame to a
//! staging texture and converts BGRA to RGBA. The session closes before the conversion so
//! Windows stops capturing the window early.
//!
//! # Cancellation and limits
//!
//! `cancelled` is polled before the session starts and while waiting for the frame and the GPU
//! copy. One 500 ms deadline covers both waits. WGC delivers no content for minimized windows,
//! so they are rejected. Windows above 40 million pixels are rejected to bound the copy, and a
//! resize before the frame arrives fails because the frame no longer matches the item size.
use anyhow::{Context, Result, bail, ensure};
use image::RgbaImage;
use std::{
    thread,
    time::{Duration, Instant},
};
use windows::{
    Graphics::{
        Capture::{
            Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
            IDirect3D11CaptureFramePoolStatics2, IGraphicsCaptureSessionStatics,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{DXGI_ERROR_WAS_STILL_DRAWING, IDXGIDevice},
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
    },
    core::{Interface, factory},
};

/// Balances a successful `RoInitialize` on drop; the `Rc` marker keeps it on that thread.
struct Apartment(std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: an `Apartment` exists only after `RoInitialize` succeeded on this thread, and
        // `!Send` keeps it there, so this call balances that initialization.
        unsafe { RoUninitialize() };
    }
}

/// WGC snapshot source bound to the creating thread.
///
/// Field order releases the COM objects before `_apartment` leaves the Windows Runtime.
pub struct Snapshotter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt: IDirect3DDevice,
    interop: IGraphicsCaptureItemInterop,
    pool_factory: IDirect3D11CaptureFramePoolStatics2,
    _apartment: Apartment,
}

/// Closes the capture session and frame pool on every exit path.
struct Session {
    capture: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.capture.Close();
        let _ = self.pool.Close();
    }
}

impl Snapshotter {
    /// Enters the multithreaded apartment on this thread and creates the D3D11 device.
    ///
    /// # Errors
    ///
    /// Fails if the thread is in a single-threaded apartment, WGC is unsupported or no hardware
    /// D3D11 device exists.
    pub fn new() -> Result<Self> {
        // SAFETY: RoInitialize takes a flag by value. The guard below balances a success; on
        // failure `?` returns before the guard exists.
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        let apartment = Apartment(std::marker::PhantomData);
        // Generated static methods cache factories past apartment teardown and DLL unloading.
        let session_factory = factory::<GraphicsCaptureSession, IGraphicsCaptureSessionStatics>()?;
        let mut supported = false;
        // SAFETY: the owned factory lives in this apartment; supported is a valid out parameter.
        unsafe {
            (session_factory.vtable().IsSupported)(session_factory.as_raw(), &mut supported)
                .ok()?;
        }
        ensure!(supported, "Windows Graphics Capture is unavailable");
        let (mut device, mut context) = (None, None);
        // SAFETY: D3D_DRIVER_TYPE_HARDWARE requires no adapter and no software module; the out
        // parameters are live `Option`s that receive owned interfaces.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.context("No D3D11 device")?;
        let dxgi: IDXGIDevice = device.cast()?;
        // SAFETY: `dxgi` comes from the live D3D11 device above.
        let winrt = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        Ok(Self {
            device,
            context: context.context("No D3D11 context")?,
            winrt,
            interop: factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?,
            pool_factory: factory::<Direct3D11CaptureFramePool, IDirect3D11CaptureFramePoolStatics2>(
            )?,
            _apartment: apartment,
        })
    }

    /// Takes one straight-alpha RGBA snapshot of the window `handle`.
    ///
    /// # Errors
    ///
    /// Fails if the window is closed, minimized, empty, above 40 million pixels or resized, if
    /// frame and readback exceed 500 ms, if `cancelled` returns `true`, and on WGC or D3D errors.
    pub fn capture(&self, handle: usize, cancelled: impl Fn() -> bool) -> Result<RgbaImage> {
        let mut image = self.capture_raw(handle, cancelled)?;
        for pixel in image.pixels_mut() {
            pixel.0 = unpremultiply(pixel.0);
        }
        Ok(image)
    }

    fn capture_raw(&self, handle: usize, cancelled: impl Fn() -> bool) -> Result<RgbaImage> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowDisplayAffinity, IsIconic, IsWindow, IsWindowVisible,
        };
        ensure!(
            // SAFETY: IsWindow and IsIconic accept any handle value and dereference nothing.
            unsafe { IsWindow(handle as _) } != 0,
            "WGC target window is closed"
        );
        // SAFETY: these functions only read window state; affinity is a live u32 output.
        unsafe {
            ensure!(IsIconic(handle as _) == 0, "WGC target window is minimized");
            ensure!(
                IsWindowVisible(handle as _) != 0,
                "WGC target window is hidden"
            );
            let mut affinity = 0;
            ensure!(
                GetWindowDisplayAffinity(handle as _, &mut affinity) == 0 || affinity == 0,
                "WGC target excludes screen capture through display affinity"
            );
        }
        ensure!(!cancelled(), "WGC capture cancelled");
        // SAFETY: CreateForWindow validates the HWND and fails for a window closed since the
        // check above; the interop factory lives as long as `&self`.
        let item: GraphicsCaptureItem = unsafe { self.interop.CreateForWindow(HWND(handle as _))? };
        let size = item.Size()?;
        ensure!(
            size.Width > 0
                && size.Height > 0
                && i64::from(size.Width) * i64::from(size.Height) <= 40_000_000,
            "WGC window is too large or empty"
        );
        // SAFETY: the apartment owns this uncached factory and device; size was validated,
        // and Type::from_abi takes ownership of the returned frame pool after a successful call.
        let pool: Direct3D11CaptureFramePool = unsafe {
            let mut result = std::ptr::null_mut();
            (self.pool_factory.vtable().CreateFreeThreaded)(
                self.pool_factory.as_raw(),
                self.winrt.as_raw(),
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                1,
                size,
                &mut result,
            )
            .and_then(|| windows::core::Type::from_abi(result))?
        };
        let session = Session {
            capture: pool.CreateCaptureSession(&item)?,
            pool,
        };
        session.capture.SetIsCursorCaptureEnabled(false).context("Cannot disable native WGC cursor capture; WGC and picker previews require Windows 10 version 2004 (build 19041) or newer")?;
        // Best effort without consent prompts; Windows may still draw the border.
        let _ = session.capture.SetIsBorderRequired(false);
        session.capture.StartCapture()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        let frame = loop {
            ensure!(!cancelled(), "WGC capture cancelled");
            ensure!(
                // SAFETY: IsWindow accepts an HWND by value and reads no application memory.
                unsafe { IsWindow(handle as _) } != 0,
                "WGC target closed during capture"
            );
            if let Ok(frame) = session.pool.TryGetNextFrame() {
                break frame;
            }
            ensure!(Instant::now() < deadline, "No window frame arrived");
            thread::sleep(Duration::from_millis(4));
        };
        let content = frame.ContentSize()?;
        ensure!(
            content.Width == size.Width && content.Height == size.Height,
            "Window resized during WGC capture"
        );
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: the surface belongs to this device, so the access yields an ID3D11Texture2D;
        // GetInterface returns an owned reference.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is writable and `texture` is valid.
        unsafe {
            texture.GetDesc(&mut desc);
        }
        ensure!(
            desc.Width >= size.Width as u32 && desc.Height >= size.Height as u32,
            "Invalid capture surface"
        );
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        // SAFETY: `desc` describes a CPU-readable staging texture without bind flags, no initial
        // data is passed, and the out parameter is a live `Option`.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        let staging = staging.context("No staging texture")?;
        // SAFETY: both textures belong to this device and share size and format; `!Sync` keeps
        // other threads off the context.
        unsafe {
            self.context.CopyResource(&staging, &texture);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        loop {
            ensure!(
                !cancelled() && Instant::now() < deadline,
                "WGC readback timed out or cancelled"
            );
            // SAFETY: `staging` has CPU read access and subresource 0; `mapped` is a live out
            // parameter. DO_NOT_WAIT reports a busy GPU as an error so the loop can poll.
            match unsafe {
                self.context.Map(
                    &staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Ok(()) => break,
                Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(error) => bail!(error),
            }
        }
        // Stops window capture before the CPU conversion.
        let _ = frame.Close();
        drop(session);
        let image = if mapped.pData.is_null() || mapped.RowPitch < size.Width as u32 * 4 {
            Err(anyhow::anyhow!("Invalid WGC row pitch"))
        } else {
            let mut image = RgbaImage::new(size.Width as u32, size.Height as u32);
            for (y, row) in image
                .as_mut()
                .chunks_exact_mut(size.Width as usize * 4)
                .enumerate()
            {
                // SAFETY: after Map, `pData` holds `RowPitch` bytes for each of
                // `desc.Height >= size.Height` rows until Unmap. `y < size.Height` and
                // `row.len() == size.Width * 4 <= RowPitch` keep the slice inside row `y`, and
                // nothing writes the mapping while it is borrowed.
                let source = unsafe {
                    std::slice::from_raw_parts(
                        mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                        row.len(),
                    )
                };
                for (out, bgra) in row
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(source.as_chunks::<4>().0)
                {
                    out.copy_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
                }
            }
            Ok(image)
        };
        // SAFETY: subresource 0 of `staging` is mapped and no `source` slice outlives this.
        unsafe {
            self.context.Unmap(&staging, 0);
        }
        image
    }
}

fn unpremultiply([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    if a == 0 {
        return [0; 4];
    }
    let straight =
        |color| ((u16::from(color) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8;
    [straight(r), straight(g), straight(b), a]
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
