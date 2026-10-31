//! The Windows desktop as pixels, with what changed since the last capture,
//! and the mouse pointer.
//!
//! DXGI desktop duplication hands over only new frames, each with the
//! regions Windows redrew and the ones it moved (a scroll), so a still screen
//! costs nothing and only changed rows are read back from the GPU. Where it is
//! unavailable (some remote sessions, a rotated display) or lost (a mode
//! change, the secure desktop), the whole screen is copied with GDI and
//! compared instead, and duplication is tried again a second later.
//!
//! The process is made per-monitor DPI aware first, so both see physical
//! pixels: unaware, Windows hands back a blurred, scaled-down screen.
use super::damage::{Damage, Move, Rect};
use super::{PixelFormat, Raw, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
    GetDC, GetDIBits, GetObjectW, ReleaseDC, SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING,
    DI_NORMAL, HCURSOR, HICON, ICONINFO, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

/// The virtual screen: left, top, width, height in physical pixels.
type Bounds = (i32, i32, u32, u32);

fn bounds() -> Bounds {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32,
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32,
        )
    }
}

/// Physical pixels from here on, for capture, the pointer and input alike.
pub fn dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

struct Output {
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    /// Where this output sits on the desktop.
    area: RECT,
    /// Its whole picture has been read once.
    primed: bool,
}

struct Duplication {
    outputs: Vec<Output>,
}

/// Where a capture writes: the next picture, made on first use from the one
/// before last when no one holds that any more. It then only needs the
/// regions the last capture changed, not a copy of the whole screen.
struct Target<'a> {
    frame: &'a mut Option<Arc<Vec<u8>>>,
    spare: &'a mut Option<Arc<Vec<u8>>>,
    /// Where the spare differs from the frame.
    stale: &'a mut Vec<Rect>,
    width: usize,
    height: usize,
    ready: bool,
}

impl Target<'_> {
    fn pixels(&mut self) -> &mut Vec<u8> {
        if !self.ready {
            self.ready = true;
            let size = self.width * self.height * 4;
            let next = match (self.spare.take(), self.frame.as_ref()) {
                // No other holder (no weak references are ever made).
                (Some(mut spare), Some(frame))
                    if spare.len() == size && Arc::strong_count(&spare) == 1 =>
                {
                    let pixels = Arc::get_mut(&mut spare).unwrap_or_else(|| unreachable!());
                    for rect in self.stale.iter() {
                        for y in rect.y..rect.y + rect.h {
                            let at = (y as usize * self.width + rect.x as usize) * 4;
                            let span = at..at + rect.w as usize * 4;
                            pixels[span.clone()].copy_from_slice(&frame[span]);
                        }
                    }
                    spare
                }
                (_, Some(frame)) => Arc::new(frame.as_ref().clone()),
                (_, None) => Arc::new(vec![0u8; size]),
            };
            self.stale.clear();
            *self.spare = self.frame.replace(next);
        }
        let frame = self.frame.as_mut().unwrap_or_else(|| unreachable!());
        Arc::get_mut(frame).unwrap_or_else(|| unreachable!())
    }
}

impl Duplication {
    fn open() -> Result<Self> {
        let failed = |what: &str, e: windows::core::Error| format!("{what}: {e}");
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| failed("DXGI", e))?;
            let mut outputs = Vec::new();
            let mut index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                index += 1;
                let mut screens = Vec::new();
                let mut at = 0;
                while let Ok(output) = adapter.EnumOutputs(at) {
                    at += 1;
                    let desc = output.GetDesc().map_err(|e| failed("output", e))?;
                    if desc.AttachedToDesktop.as_bool() {
                        if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                            && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
                        {
                            return Err("a rotated display".into());
                        }
                        screens.push((output, desc.DesktopCoordinates));
                    }
                }
                if screens.is_empty() {
                    continue;
                }
                let (mut device, mut context) = (None, None);
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .map_err(|e| failed("Direct3D", e))?;
                let device = device.ok_or("no Direct3D device")?;
                let context = context.ok_or("no Direct3D context")?;
                for (output, area) in screens {
                    let output: IDXGIOutput1 = output.cast().map_err(|e| failed("output", e))?;
                    let duplication = output
                        .DuplicateOutput(&device)
                        .map_err(|e| failed("duplication", e))?;
                    outputs.push(Output {
                        duplication,
                        device: device.clone(),
                        context: context.clone(),
                        staging: None,
                        area,
                        primed: false,
                    });
                }
            }
            if outputs.is_empty() {
                return Err("no display to duplicate".into());
            }
            Ok(Self { outputs })
        }
    }

    /// New pixels into `frame` and what they changed; `None` when nothing did.
    fn capture(&mut self, target: &mut Target, at: Bounds) -> Result<Option<Damage>> {
        let mut damage = Damage::default();
        let mut changed = false;
        let width = at.2 as usize;
        for output in &mut self.outputs {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // The first frame always comes: it is the whole output.
            let wait = if output.primed { 0 } else { 500 };
            match unsafe {
                output
                    .duplication
                    .AcquireNextFrame(wait, &mut info, &mut resource)
            } {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                Err(e) => return Err(format!("desktop duplication: {e}")),
            }
            let result = (|| -> Result<()> {
                // Only the pointer moved.
                if info.LastPresentTime == 0 && output.primed {
                    return Ok(());
                }
                let texture: ID3D11Texture2D = resource
                    .as_ref()
                    .ok_or("no desktop picture")?
                    .cast()
                    .map_err(|e| e.to_string())?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { texture.GetDesc(&mut desc) };
                if output.staging.is_none() {
                    let staging_desc = D3D11_TEXTURE2D_DESC {
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                        MipLevels: 1,
                        ArraySize: 1,
                        ..desc
                    };
                    let mut staging = None;
                    unsafe {
                        output
                            .device
                            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                    }
                    .map_err(|e| e.to_string())?;
                    output.staging = staging;
                }
                let staging = output.staging.as_ref().ok_or("no staging texture")?;
                let (left, top) = (
                    (output.area.left - at.0) as u32,
                    (output.area.top - at.1) as u32,
                );
                let mut moves = Vec::new();
                let mut dirty = Vec::new();
                if output.primed && info.TotalMetadataBufferSize > 0 {
                    let size = info.TotalMetadataBufferSize as usize;
                    let mut buffer = vec![
                        DXGI_OUTDUPL_MOVE_RECT::default();
                        size / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1
                    ];
                    let mut needed = 0;
                    unsafe {
                        output.duplication.GetFrameMoveRects(
                            (buffer.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32,
                            buffer.as_mut_ptr(),
                            &mut needed,
                        )
                    }
                    .map_err(|e| e.to_string())?;
                    buffer
                        .truncate(needed as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>());
                    moves = buffer;
                    let mut rects = vec![RECT::default(); size / std::mem::size_of::<RECT>() + 1];
                    unsafe {
                        output.duplication.GetFrameDirtyRects(
                            (rects.len() * std::mem::size_of::<RECT>()) as u32,
                            rects.as_mut_ptr(),
                            &mut needed,
                        )
                    }
                    .map_err(|e| e.to_string())?;
                    rects.truncate(needed as usize / std::mem::size_of::<RECT>());
                    dirty = rects;
                } else {
                    dirty.push(RECT {
                        left: 0,
                        top: 0,
                        right: desc.Width as i32,
                        bottom: desc.Height as i32,
                    });
                }
                let local = |r: &RECT| {
                    Rect {
                        x: r.left.max(0) as u32,
                        y: r.top.max(0) as u32,
                        w: (r.right - r.left).max(0) as u32,
                        h: (r.bottom - r.top).max(0) as u32,
                    }
                    .clip(desc.Width, desc.Height)
                };
                let mut copy: Vec<Rect> = dirty.iter().filter_map(local).collect();
                for m in &moves {
                    if let Some(to) = local(&m.DestinationRect) {
                        copy.push(to);
                        damage.moves.push(Move {
                            from: (
                                left + m.SourcePoint.x.max(0) as u32,
                                top + m.SourcePoint.y.max(0) as u32,
                            ),
                            to: Rect {
                                x: left + to.x,
                                y: top + to.y,
                                ..to
                            },
                        });
                    }
                }
                if copy.is_empty() {
                    return Ok(());
                }
                unsafe { output.context.CopyResource(staging, &texture) };
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe {
                    output
                        .context
                        .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                }
                .map_err(|e| e.to_string())?;
                let pixels = target.pixels();
                let pitch = mapped.RowPitch as usize;
                for rect in &copy {
                    for y in rect.y..rect.y + rect.h {
                        let from = unsafe {
                            std::slice::from_raw_parts(
                                (mapped.pData as *const u8)
                                    .add(y as usize * pitch + rect.x as usize * 4),
                                rect.w as usize * 4,
                            )
                        };
                        let to = ((top + y) as usize * width + (left + rect.x) as usize) * 4;
                        if let Some(target) = pixels.get_mut(to..to + rect.w as usize * 4) {
                            target.copy_from_slice(from);
                        }
                    }
                    damage.dirty.push(Rect {
                        x: left + rect.x,
                        y: top + rect.y,
                        ..*rect
                    });
                }
                unsafe { output.context.Unmap(staging, 0) };
                changed = true;
                Ok(())
            })();
            unsafe {
                let _ = output.duplication.ReleaseFrame();
            }
            result?;
            output.primed = true;
        }
        // A move's destination is redrawn here too; the viewer only needs the move.
        if !damage.moves.is_empty() {
            let moved: Vec<Rect> = damage.moves.iter().map(|m| m.to).collect();
            damage.dirty.retain(|rect| !moved.contains(rect));
        }
        Ok(changed.then_some(damage))
    }
}

/// The whole virtual screen copied with GDI, as BGRA.
fn grab(at: Bounds) -> Result<Vec<u8>> {
    let (left, top, width, height) = (at.0, at.1, at.2 as i32, at.3 as i32);
    if width <= 0 || height <= 0 {
        return Err("no screen to capture".into());
    }
    unsafe {
        let screen = GetDC(HWND::default());
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let old = SelectObject(memory, bitmap);
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            screen,
            left,
            top,
            SRCCOPY | CAPTUREBLT,
        );
        let mut info = bitmap_info(width, height);
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let lines = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(memory, old);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(HWND::default(), screen);
        copied.map_err(|e| format!("could not copy the screen: {e}"))?;
        if lines == 0 {
            return Err("could not read the screen".into());
        }
        Ok(bgra)
    }
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
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
    }
}

/// The desktop's pixels, from duplication when it works.
pub struct Screen {
    duplication: Option<Duplication>,
    retry: Instant,
    frame: Option<Arc<Vec<u8>>>,
    /// The picture before, reused for the next, and where it is out of date.
    spare: Option<Arc<Vec<u8>>>,
    stale: Vec<Rect>,
    at: Bounds,
    /// Why duplication is not in use, once.
    pub fallback: Option<String>,
    /// Return the screen next time even if nothing changed.
    refresh: bool,
}

impl Screen {
    pub fn open() -> Self {
        let (duplication, fallback) = match Duplication::open() {
            Ok(d) => (Some(d), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            duplication,
            retry: Instant::now() + Duration::from_secs(1),
            frame: None,
            spare: None,
            stale: Vec::new(),
            at: bounds(),
            fallback,
            refresh: false,
        }
    }

    /// The next capture returns the screen even if nothing changed.
    pub fn refresh(&mut self) {
        self.refresh = true;
    }

    pub fn duplicating(&self) -> bool {
        self.duplication.is_some()
    }

    /// The screen if it changed, with what changed when duplication knows.
    pub fn capture(&mut self) -> Result<Option<Raw>> {
        let at = bounds();
        if at != self.at {
            // Displays were added, removed or resized: start over.
            self.at = at;
            self.frame = None;
            self.spare = None;
            self.duplication = None;
            self.retry = Instant::now();
        }
        if self.duplication.is_none() && Instant::now() >= self.retry {
            self.retry = Instant::now() + Duration::from_secs(1);
            match Duplication::open() {
                Ok(d) => {
                    self.duplication = Some(d);
                    self.frame = None;
                    self.spare = None;
                }
                Err(e) => self.fallback = Some(e),
            }
        }
        if let Some(duplication) = &mut self.duplication {
            let first = self.frame.is_none();
            let mut target = Target {
                frame: &mut self.frame,
                spare: &mut self.spare,
                stale: &mut self.stale,
                width: at.2 as usize,
                height: at.3 as usize,
                ready: false,
            };
            let captured = duplication.capture(&mut target, at);
            if let Ok(Some(damage)) = &captured {
                // The spare now lacks this capture's changes.
                self.stale = damage
                    .dirty
                    .iter()
                    .chain(damage.moves.iter().map(|m| &m.to))
                    .copied()
                    .collect();
            }
            match captured {
                Ok(Some(damage)) => {
                    let pixels = self.frame.clone().ok_or("no desktop picture")?;
                    return Ok(Some(Raw {
                        pixels,
                        width: at.2,
                        height: at.3,
                        format: PixelFormat::Bgra,
                        // The first picture is whole; compare it with nothing.
                        damage: (!first).then_some(damage),
                    }));
                }
                Ok(None) => {
                    let again = std::mem::take(&mut self.refresh);
                    return Ok(self.frame.clone().filter(|_| again).map(|pixels| Raw {
                        pixels,
                        width: at.2,
                        height: at.3,
                        format: PixelFormat::Bgra,
                        damage: Some(Damage::default()),
                    }));
                }
                Err(e) => {
                    self.fallback = Some(e);
                    self.duplication = None;
                    self.frame = None;
                    self.spare = None;
                }
            }
        }
        Ok(Some(Raw {
            pixels: Arc::new(grab(at)?),
            width: at.2,
            height: at.3,
            format: PixelFormat::Bgra,
            damage: None,
        }))
    }
}

/// The pointer as last told to the viewer.
#[derive(Default)]
pub struct Pointer {
    last: Option<(i32, i32, bool)>,
    shape: Option<isize>,
    sent: Option<Instant>,
}

impl Pointer {
    /// A `cursor` message when the pointer moved, appeared, vanished or
    /// changed shape, at most 60 a second; `fresh` sends everything again.
    pub fn message(&mut self, fresh: bool) -> Option<Value> {
        if fresh {
            *self = Pointer::default();
        }
        if self
            .sent
            .is_some_and(|at| at.elapsed() < Duration::from_millis(16))
        {
            return None;
        }
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetCursorInfo(&mut info) }.ok()?;
        let visible = info.flags.0 & CURSOR_SHOWING.0 != 0 && !info.hCursor.is_invalid();
        let POINT { x, y } = info.ptScreenPos;
        let at = bounds();
        let state = (x, y, visible);
        let handle = info.hCursor.0 as isize;
        let new_shape = visible && self.shape != Some(handle);
        if self.last == Some(state) && !new_shape {
            return None;
        }
        self.last = Some(state);
        self.sent = Some(Instant::now());
        let fraction = |v: i32, origin: i32, size: u32| {
            (f64::from(v - origin) / f64::from(size.max(1))).clamp(0.0, 1.0)
        };
        let mut message = json!({"type": "cursor", "visible": visible,
            "x": fraction(x, at.0, at.2), "y": fraction(y, at.1, at.3)});
        if new_shape {
            self.shape = Some(handle);
            if let Some((png, width, height, hotspot)) = shape(info.hCursor) {
                use base64::Engine;
                message["shape"] = json!({"png": base64::engine::general_purpose::STANDARD.encode(png),
                    "width": width, "height": height, "hotspot": hotspot, "screen": [at.2, at.3]});
            }
        }
        Some(message)
    }
}

/// A cursor drawn as a PNG with alpha: drawn once on black and once on white,
/// the difference gives each pixel's opacity. Pixels that invert what is
/// under them (the text cursor) come out black.
fn shape(cursor: HCURSOR) -> Option<(Vec<u8>, u32, u32, [u32; 2])> {
    unsafe {
        let icon = HICON(cursor.0);
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let mut mask = BITMAP::default();
        GetObjectW(
            HGDIOBJ(info.hbmMask.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut mask as *mut BITMAP).cast()),
        );
        let width = mask.bmWidth.max(1);
        // A monochrome cursor stacks its AND and XOR masks in one bitmap.
        let height = if info.hbmColor.is_invalid() {
            (mask.bmHeight / 2).max(1)
        } else {
            mask.bmHeight.max(1)
        };
        let hotspot = [info.xHotspot, info.yHotspot];
        let _ = DeleteObject(info.hbmMask);
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor);
        }
        let draw = |background: u8| -> Option<Vec<u8>> {
            let screen = GetDC(HWND::default());
            let memory = CreateCompatibleDC(screen);
            let info = bitmap_info(width, height);
            let mut bits = std::ptr::null_mut();
            let bitmap: HBITMAP =
                CreateDIBSection(memory, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(memory, bitmap);
            let size = (width * height * 4) as usize;
            std::ptr::write_bytes(bits as *mut u8, background, size);
            let drawn = DrawIconEx(memory, 0, 0, icon, width, height, 0, None, DI_NORMAL);
            let pixels = std::slice::from_raw_parts(bits as *const u8, size).to_vec();
            SelectObject(memory, old);
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(memory);
            ReleaseDC(HWND::default(), screen);
            drawn.ok().map(|()| pixels)
        };
        let (on_black, on_white) = (draw(0)?, draw(255)?);
        let mut rgba = Vec::with_capacity(on_black.len());
        for (b, w) in on_black.chunks_exact(4).zip(on_white.chunks_exact(4)) {
            let (black, white) = (i32::from(b[1]), i32::from(w[1]));
            if white < black {
                rgba.extend_from_slice(&[0, 0, 0, 255]);
                continue;
            }
            let alpha = (255 - (white - black)).clamp(0, 255);
            let unmultiply = |c: u8| {
                if alpha == 0 {
                    0
                } else {
                    ((i32::from(c) * 255) / alpha).min(255) as u8
                }
            };
            rgba.extend_from_slice(&[
                unmultiply(b[2]),
                unmultiply(b[1]),
                unmultiply(b[0]),
                alpha as u8,
            ]);
        }
        let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba)?;
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        Some((png, width as u32, height as u32, hotspot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spare_picture_is_caught_up_instead_of_copied() {
        let (width, height) = (4usize, 2usize);
        let mut frame = Some(Arc::new(vec![1u8; width * height * 4]));
        // The picture before last: out of date in its second row.
        let mut old = vec![1u8; width * height * 4];
        old[width * 4..].fill(9);
        let mut spare = Some(Arc::new(old));
        let spare_at = Arc::as_ptr(spare.as_ref().unwrap());
        let mut stale = vec![Rect {
            x: 0,
            y: 1,
            w: 4,
            h: 1,
        }];
        let mut target = Target {
            frame: &mut frame,
            spare: &mut spare,
            stale: &mut stale,
            width,
            height,
            ready: false,
        };
        let pixels = target.pixels();
        assert!(pixels.iter().all(|p| *p == 1));
        pixels[0] = 5;
        // The spare's buffer became the new picture; the old picture is the spare.
        assert_eq!(Arc::as_ptr(frame.as_ref().unwrap()), spare_at);
        assert_eq!(frame.as_ref().unwrap()[0], 5);
        assert_eq!(spare.as_ref().unwrap()[0], 1);
        assert!(stale.is_empty());

        // Someone still holds the spare: the picture is copied instead.
        let held = spare.clone();
        let mut target = Target {
            frame: &mut frame,
            spare: &mut spare,
            stale: &mut stale,
            width,
            height,
            ready: false,
        };
        assert_eq!(target.pixels()[0], 5);
        assert_ne!(
            Arc::as_ptr(frame.as_ref().unwrap()),
            Arc::as_ptr(held.as_ref().unwrap())
        );
    }
}
