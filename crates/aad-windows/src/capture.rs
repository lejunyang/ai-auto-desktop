//! A window's own rendering as pixels (PrintWindow), never what lies over it.
use aad_platform::{Error, Pixels};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::HiDpi::{GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, PW_RENDERFULLCONTENT};

type Result<T> = std::result::Result<T, Error>;

/// The largest capture returned on either side, in physical pixels.
pub const MAX_CAPTURE_SIDE: i32 = 8_192;
/// A window larger than this is refused rather than buffered whole.
pub const MAX_WINDOW_PIXELS: i64 = 64 * 1024 * 1024;

/// A window's visible frame, without the invisible resize border Windows 10+
/// adds outside it (which `GetWindowRect` includes).
pub fn extended_frame(handle: HWND) -> Option<RECT> {
    let mut frame = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            handle,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut frame as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    (frame.right > frame.left && frame.bottom > frame.top).then_some(frame)
}

pub fn intersect(a: RECT, b: RECT) -> Option<RECT> {
    let rect = RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

/// The whole window as top-down BGRA, rendered by the window itself.
///
/// `PrintWindow` with `PW_RENDERFULLCONTENT` asks the window (and DWM, for
/// DirectComposition and GPU content) to render into our bitmap. Unlike a
/// copy of the screen it never contains another window laid over the target,
/// so a screenshot cannot leak what some other application shows, and it does
/// not need the target in front. The price: a minimized window renders
/// nothing (refused before this), and an application that ignores the request
/// renders black.
///
/// A DPI-virtualised window (unaware, or system aware on a monitor of another
/// scale) renders at its own logical size while Windows shows it stretched.
/// Its rendering is scaled to the physical window size the same way, so the
/// capture lines up with the physical bounds UIA reports. Such a capture is
/// as soft as what is on screen, no sharper.
pub fn print_window(handle: HWND, window: &RECT) -> Result<Vec<u8>> {
    let width = window.right - window.left;
    let height = window.bottom - window.top;
    if width <= 0 || height <= 0 {
        return Err(Error::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "the window has no area to capture",
        ));
    }
    if i64::from(width) * i64::from(height) > MAX_WINDOW_PIXELS {
        return Err(Error::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "the window is larger than a capture may buffer",
        ));
    }
    let (own_width, own_height) = own_window_size(handle)
        .filter(|&(w, h)| w > 0 && h > 0 && i64::from(w) * i64::from(h) <= MAX_WINDOW_PIXELS)
        .unwrap_or((width, height));
    let rendered = render_window(handle, own_width, own_height)?;
    if (own_width, own_height) == (width, height) {
        return Ok(rendered);
    }
    let image = image::RgbaImage::from_raw(own_width as u32, own_height as u32, rendered)
        .ok_or_else(|| Error::new("DRIVER.ACTION_FAILED", "malformed window rendering"))?;
    Ok(image::imageops::resize(
        &image,
        width as u32,
        height as u32,
        image::imageops::FilterType::Triangle,
    )
    .into_raw())
}

/// The window's size in its own DPI space: what `PrintWindow` renders.
pub fn own_window_size(handle: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe {
        let previous = SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(handle));
        let read = GetWindowRect(handle, &mut rect);
        if !previous.0.is_null() {
            SetThreadDpiAwarenessContext(previous);
        }
        read.ok()?;
    }
    Some((rect.right - rect.left, rect.bottom - rect.top))
}

/// `PrintWindow` into a bitmap of the given size, read back as BGRA.
pub fn render_window(handle: HWND, width: i32, height: i32) -> Result<Vec<u8>> {
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    unsafe {
        let screen = GetDC(HWND::default());
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let old = SelectObject(memory, bitmap);
        let printed = PrintWindow(handle, memory, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT));
        // GetDIBits wants the bitmap out of any DC.
        SelectObject(memory, old);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(HWND::default(), screen);
        if !printed.as_bool() {
            return Err(Error::new(
                "DRIVER.ACTION_FAILED",
                "the window did not render itself for the screenshot",
            ));
        }
        if lines != height {
            return Err(Error::new(
                "DRIVER.ACTION_FAILED",
                "the rendered window could not be read back",
            ));
        }
    }
    Ok(bgra)
}

/// Cut `region` (screen pixels) out of a window-sized BGRA buffer as RGBA,
/// keeping at most MAX_CAPTURE_SIDE on each side from the top-left.
pub fn crop(bgra: &[u8], window: &RECT, region: &RECT) -> Pixels {
    let stride = (window.right - window.left) as usize * 4;
    let full_width = region.right - region.left;
    let full_height = region.bottom - region.top;
    let width = full_width.min(MAX_CAPTURE_SIDE);
    let height = full_height.min(MAX_CAPTURE_SIDE);
    let left = (region.left - window.left) as usize;
    let top = (region.top - window.top) as usize;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let start = (top + row) * stride + left * 4;
        for pixel in bgra[start..start + width as usize * 4].chunks_exact(4) {
            // PrintWindow leaves alpha undefined; the image is opaque.
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    Pixels {
        width: width as u32,
        height: height as u32,
        rgba,
        clipped: width < full_width || height < full_height,
    }
}
