//! Eager Windows clipboard data: straight-alpha PNG, premultiplied DIBV5, and an optional file.
//!
//! All buffers are prepared before replacing the clipboard. Windows owns published
//! memory, so pasting still works after the owner window and process exit.

use std::{
    io,
    mem::{forget, size_of},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use image::RgbaImage;
use windows_sys::Win32::{
    Foundation::{GlobalFree, HGLOBAL, HWND},
    Graphics::Gdi::{BI_BITFIELDS, BITMAPV5HEADER, LCS_GM_IMAGES},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
            SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        Ole::{CF_DIBV5, CF_HDROP, DROPEFFECT_COPY},
    },
    UI::{
        ColorSystem::LCS_sRGB,
        Shell::{CFSTR_PREFERREDDROPEFFECT, DROPFILES},
        WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE},
    },
};

/// Publishes image formats first, so apps that choose by order prefer the PNG.
pub(crate) fn copy(image: &RgbaImage, path: Option<&Path>, dpi: u32) -> Result<()> {
    // SAFETY: the name is a static, null-terminated UTF-16 string.
    let png_format = unsafe { RegisterClipboardFormatW(windows_sys::w!("PNG")) };
    if png_format == 0 {
        return Err(io::Error::last_os_error()).context("Cannot register clipboard formats");
    }
    let mut data = vec![
        (png_format, Memory::new(&png(image, dpi)?)?),
        (u32::from(CF_DIBV5), Memory::new(&dib(image, dpi)?)?),
    ];
    if let Some(path) = path {
        // SAFETY: the name is a static, null-terminated UTF-16 string.
        let effect_format = unsafe { RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) };
        if effect_format == 0 {
            return Err(io::Error::last_os_error()).context("Cannot register clipboard formats");
        }
        data.push((u32::from(CF_HDROP), Memory::new(&file_drop(path)?)?));
        data.push((effect_format, Memory::new(&DROPEFFECT_COPY.to_le_bytes())?));
    }
    let _clipboard = Clipboard::open()?;
    // SAFETY: this thread has the clipboard open with a valid owner window.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error()).context("Cannot empty clipboard");
    }
    for (format, memory) in data {
        // SAFETY: the clipboard is open; memory is movable, unlocked, and still owned here.
        if unsafe { SetClipboardData(format, memory.0) }.is_null() {
            return Err(io::Error::last_os_error()).context("Cannot publish clipboard data");
        }
        forget(memory); // Windows now owns the allocation.
    }
    Ok(())
}

fn png(image: &RgbaImage, dpi: u32) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut encoder = png::Encoder::new(&mut data, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let ppm = (f64::from(dpi) / 0.0254).round() as u32;
    encoder.set_pixel_dims(Some(png::PixelDimensions {
        xppu: ppm,
        yppu: ppm,
        unit: png::Unit::Meter,
    }));
    let mut writer = encoder.write_header()?;
    writer.write_image_data(image.as_raw())?;
    writer.finish()?;
    Ok(data)
}

fn dib(image: &RgbaImage, dpi: u32) -> Result<Vec<u8>> {
    ensure!(image.width() > 0 && image.height() > 0, "Image is empty");
    let ppm = (f64::from(dpi) / 0.0254).round() as i32;
    let header = BITMAPV5HEADER {
        bV5Size: size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: image.width().try_into()?,
        bV5Height: image.height().try_into()?,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: image.as_raw().len().try_into()?,
        bV5XPelsPerMeter: ppm,
        bV5YPelsPerMeter: ppm,
        bV5RedMask: 0x00ff0000,
        bV5GreenMask: 0x0000ff00,
        bV5BlueMask: 0x000000ff,
        bV5AlphaMask: 0xff000000,
        bV5CSType: LCS_sRGB as u32,
        bV5Intent: LCS_GM_IMAGES as u32,
        ..Default::default()
    };
    let mut data = vec![0; size_of::<BITMAPV5HEADER>()];
    // SAFETY: the buffer fits this padding-free C header; unaligned writes need no alignment.
    unsafe {
        data.as_mut_ptr()
            .cast::<BITMAPV5HEADER>()
            .write_unaligned(header)
    };
    // Positive height and bottom-up BGRA also work in apps that reject top-down DIBs.
    for row in image.rows().rev() {
        for pixel in row {
            let [r, g, b, a] = pixel.0;
            // Windows bitmap readers expect premultiplied channels; PNG stays straight alpha.
            let premultiply = |color| ((u16::from(color) * u16::from(a) + 127) / 255) as u8;
            data.extend_from_slice(&[premultiply(b), premultiply(g), premultiply(r), a]);
        }
    }
    Ok(data)
}

fn file_drop(path: &Path) -> Result<Vec<u8>> {
    let absolute = std::path::absolute(path).context("Cannot resolve clipboard file path")?;
    let name = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    ensure!(!name.contains(&0), "Clipboard file path contains NUL");
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        fWide: 1,
        ..Default::default()
    };
    let mut data = vec![0; size_of::<DROPFILES>()];
    // SAFETY: the buffer fits the packed C header; the write may be unaligned.
    unsafe {
        data.as_mut_ptr()
            .cast::<DROPFILES>()
            .write_unaligned(header)
    };
    for unit in name.into_iter().chain([0, 0]) {
        data.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(data)
}

/// Frees unpublished buffers, including on preparation or SetClipboardData failures.
struct Memory(HGLOBAL);

impl Memory {
    fn new(data: &[u8]) -> Result<Self> {
        // SAFETY: allocates an owned movable block of the requested byte length.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, data.len()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error()).context("Cannot allocate clipboard memory");
        }
        let memory = Self(handle);
        // SAFETY: memory owns this live global allocation.
        let pointer = unsafe { GlobalLock(handle) }.cast::<u8>();
        if pointer.is_null() {
            return Err(io::Error::last_os_error()).context("Cannot lock clipboard memory");
        }
        // SAFETY: the locked destination fits data and does not overlap it; unlock once copied.
        unsafe {
            pointer.copy_from_nonoverlapping(data.as_ptr(), data.len());
            GlobalUnlock(handle);
        }
        Ok(memory)
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: only unpublished, unlocked allocations are dropped; published ones are forgotten.
        unsafe { GlobalFree(self.0) };
    }
}

/// Owns a message-only window: OpenClipboard(NULL) cannot reliably publish data.
struct Clipboard {
    window: HWND,
    open: bool,
}

impl Clipboard {
    fn open() -> Result<Self> {
        // SAFETY: STATIC is a built-in class; the invisible window is created on this thread.
        let window = unsafe {
            CreateWindowExW(
                0,
                windows_sys::w!("STATIC"),
                null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                null_mut(),
                null(),
            )
        };
        if window.is_null() {
            return Err(io::Error::last_os_error()).context("Cannot create clipboard owner window");
        }
        let mut clipboard = Self {
            window,
            open: false,
        };
        for attempt in 0..10 {
            // SAFETY: window is live and belongs to this thread.
            if unsafe { OpenClipboard(window) } != 0 {
                clipboard.open = true;
                return Ok(clipboard);
            }
            if attempt == 9 {
                return Err(io::Error::last_os_error()).context("Cannot open clipboard");
            }
            thread::sleep(Duration::from_millis(20));
        }
        unreachable!()
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: close only if this thread opened it; destroy the owner on its creating thread.
        unsafe {
            if self.open {
                CloseClipboard();
            }
            DestroyWindow(self.window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn image() -> RgbaImage {
        RgbaImage::from_fn(3, 2, |x, y| {
            Rgba([25 + y as u8, 100, 190, [0, 128, 255][x as usize]])
        })
    }

    #[test]
    fn png_preserves_straight_alpha_color_and_dpi() {
        let image = image();
        let data = png(&image, 144).unwrap();
        assert_eq!(
            image::load_from_memory_with_format(&data, image::ImageFormat::Png)
                .unwrap()
                .to_rgba8(),
            image
        );
        let reader = png::Decoder::new(io::Cursor::new(data))
            .read_info()
            .unwrap();
        assert!(reader.info().srgb.is_some());
        assert_eq!(reader.info().pixel_dims.unwrap().xppu, 5669);
    }

    #[test]
    fn dib_premultiplies_once_and_uses_bottom_up_bgra() {
        let data = dib(&image(), 144).unwrap();
        assert_eq!(data.len(), 124 + 3 * 2 * 4);
        // SAFETY: dib returned at least one fully initialized header; alignment is not required.
        let header = unsafe { data.as_ptr().cast::<BITMAPV5HEADER>().read_unaligned() };
        assert_eq!((header.bV5Width, header.bV5Height), (3, 2));
        assert_eq!((header.bV5Planes, header.bV5BitCount), (1, 32));
        assert_eq!(header.bV5Compression, BI_BITFIELDS);
        assert_eq!(header.bV5SizeImage, 24);
        assert_eq!(header.bV5AlphaMask, 0xff000000);
        assert_eq!(header.bV5CSType, LCS_sRGB as u32);
        assert_eq!(header.bV5XPelsPerMeter, 5669);
        assert_eq!(
            &data[124..],
            &[
                0, 0, 0, 0, 95, 50, 13, 128, 190, 100, 26, 255, 0, 0, 0, 0, 95, 50, 13, 128, 190,
                100, 25, 255,
            ]
        );
        assert!(dib(&RgbaImage::new(0, 0), 96).is_err());
    }

    #[test]
    fn file_drop_contains_an_absolute_unicode_path_and_double_nul() {
        let path = Path::new("Büro #1 🦀.png");
        let data = file_drop(path).unwrap();
        // SAFETY: file_drop returned a complete packed header.
        let header = unsafe { data.as_ptr().cast::<DROPFILES>().read_unaligned() };
        let offset = header.pFiles;
        let wide = header.fWide;
        assert_eq!(offset, 20);
        assert_eq!(wide, 1);
        let name = data[offset as usize..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|unit| u16::from_le_bytes(*unit))
            .collect::<Vec<_>>();
        assert_eq!(&name[name.len() - 2..], &[0, 0]);
        let expected = std::path::absolute(path).unwrap();
        assert_eq!(
            &name[..name.len() - 2],
            expected.as_os_str().encode_wide().collect::<Vec<_>>()
        );
        assert!(file_drop(Path::new("bad\0name.png")).is_err());
    }

    #[test]
    #[ignore = "Replaces the system clipboard; needs an interactive Windows session"]
    fn clipboard_image_only_has_no_file_formats() {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;

        copy(&image(), None, 144).unwrap();
        let _clipboard = Clipboard::open().unwrap();
        // SAFETY: this thread has the clipboard open; all format names are valid.
        unsafe {
            assert!(!GetClipboardData(RegisterClipboardFormatW(windows_sys::w!("PNG"))).is_null());
            let bitmap = GetClipboardData(u32::from(CF_DIBV5));
            assert!(!bitmap.is_null());
            let pointer = GlobalLock(bitmap).cast::<u8>();
            assert!(!pointer.is_null());
            let expected = dib(&image(), 144).unwrap();
            assert!(windows_sys::Win32::System::Memory::GlobalSize(bitmap) >= expected.len());
            assert_eq!(
                std::slice::from_raw_parts(pointer, expected.len()),
                expected
            );
            GlobalUnlock(bitmap);
            assert!(GetClipboardData(u32::from(CF_HDROP)).is_null());
            assert!(
                GetClipboardData(RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT)).is_null()
            );
        }
    }

    #[test]
    #[ignore = "Replaces the system clipboard; needs an interactive Windows session"]
    fn clipboard_retains_all_formats_after_process_exit() {
        use windows_sys::Win32::{
            System::DataExchange::GetClipboardData, UI::Shell::DragQueryFileW,
        };
        if let Some(path) = std::env::var_os("ALPHASHOT_TEST_CLIPBOARD_FILE") {
            copy(&image(), Some(Path::new(&path)), 144).unwrap();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Büro #1 🦀.webp");
        let image = image();
        alphashot::output::save(&image, &path, &Default::default()).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "clipboard::tests::clipboard_retains_all_formats_after_process_exit",
            ])
            .env("ALPHASHOT_TEST_CLIPBOARD_FILE", &path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let _clipboard = Clipboard::open().unwrap();
        // SAFETY: this thread has the clipboard open; names and output buffers are valid.
        unsafe {
            let png_format = RegisterClipboardFormatW(windows_sys::w!("PNG"));
            let handle = GetClipboardData(png_format);
            assert!(!handle.is_null());
            let pointer = GlobalLock(handle).cast::<u8>();
            assert!(!pointer.is_null());
            let len = windows_sys::Win32::System::Memory::GlobalSize(handle);
            let data = std::slice::from_raw_parts(pointer, len);
            assert_eq!(
                image::load_from_memory_with_format(data, image::ImageFormat::Png)
                    .unwrap()
                    .to_rgba8(),
                image
            );
            GlobalUnlock(handle);
            let bitmap = GetClipboardData(u32::from(CF_DIBV5));
            assert!(!bitmap.is_null());
            let pointer = GlobalLock(bitmap).cast::<u8>();
            assert!(!pointer.is_null());
            let expected = dib(&image, 144).unwrap();
            assert!(windows_sys::Win32::System::Memory::GlobalSize(bitmap) >= expected.len());
            assert_eq!(
                std::slice::from_raw_parts(pointer, expected.len()),
                expected
            );
            GlobalUnlock(bitmap);
            let files = GetClipboardData(u32::from(CF_HDROP));
            assert!(!files.is_null());
            assert_eq!(DragQueryFileW(files, u32::MAX, null_mut(), 0), 1);
            let mut name = vec![0; DragQueryFileW(files, 0, null_mut(), 0) as usize + 1];
            DragQueryFileW(files, 0, name.as_mut_ptr(), name.len() as u32);
            assert_eq!(
                &name[..name.len() - 1],
                path.as_os_str().encode_wide().collect::<Vec<_>>()
            );
            let effect = GetClipboardData(RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT));
            assert!(!effect.is_null());
            let pointer = GlobalLock(effect).cast::<u32>();
            assert!(!pointer.is_null());
            assert_eq!(*pointer, DROPEFFECT_COPY);
            GlobalUnlock(effect);
        }
    }
}
