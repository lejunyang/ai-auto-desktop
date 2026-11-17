//! The Windows clipboard: its text, read without changing it, and pasting
//! text through it while keeping what the user copied.
//!
//! A paste offers its text by delayed rendering: a window of ours owns the
//! clipboard and supplies the text only when someone asks for it. That
//! request is the target pasting, so restoring waits for it (and for the
//! target to close the clipboard) instead of guessing at a delay -- an early
//! restore would paste the user's old clipboard instead. Anyone but the
//! target process asking for the text -- clipboard history, managers, sync
//! tools -- is refused, so the text reaches only the target.
use aad_platform::Error;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, GetOpenClipboardWindow, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowThreadProcessId,
    PeekMessageW, RegisterClassExW, HWND_MESSAGE, MSG, PM_REMOVE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSEXW,
};

type Result<T> = std::result::Result<T, Error>;

const CF_UNICODETEXT: u32 = 13;

/// A registered clipboard format by name.
fn clipboard_format(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) }
}
/// The format with which a clipboard's source (a password manager) asks
/// clipboard monitors to leave its content alone.
const EXCLUDE_FROM_MONITORS: &str = "ExcludeClipboardContentFromMonitorProcessing";
const CAN_INCLUDE_IN_HISTORY: &str = "CanIncludeInClipboardHistory";
const CAN_UPLOAD_TO_CLOUD: &str = "CanUploadToCloudClipboard";

/// The clipboard, opened and closed again on drop. Another process may hold
/// it for a moment, so opening retries briefly before giving up.
struct OpenedClipboard;

impl OpenedClipboard {
    fn open(owner: Option<HWND>) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_millis(1_000);
        loop {
            if unsafe { OpenClipboard(owner.unwrap_or_default()) }.is_ok() {
                return Ok(Self);
            }
            if Instant::now() >= deadline {
                return Err(Error::new(
                    "DRIVER.CLIPBOARD_BUSY",
                    "another application kept the clipboard open",
                )
                .retryable());
            }
            pump_for(Duration::from_millis(20));
        }
    }
}

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// Dispatch this thread's messages for a while: a clipboard owner window
/// must answer WM_RENDERFORMAT, which arrives as a sent message.
pub fn pump_for(duration: Duration) {
    let until = Instant::now() + duration;
    let mut message = MSG::default();
    loop {
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                DispatchMessageW(&message);
            }
        }
        if Instant::now() >= until {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The bytes of one clipboard format, from an open clipboard.
fn clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let global = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(global) };
    if size == 0 {
        return None;
    }
    let data = unsafe { GlobalLock(global) } as *const u8;
    if data.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
    let _ = unsafe { GlobalUnlock(global) };
    Some(bytes)
}

fn utf16_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// The clipboard's text, at most `max_chars` of it, with its full length;
/// refused when its source marked it as not for clipboard readers.
pub fn text(max_chars: usize) -> Result<(Option<String>, usize)> {
    let _open = OpenedClipboard::open(None)?;
    // A password manager marks what it copies; that is not ours to read.
    if unsafe { IsClipboardFormatAvailable(clipboard_format(EXCLUDE_FROM_MONITORS)) }.is_ok() {
        return Err(Error::new(
            "DRIVER.CLIPBOARD_PROTECTED",
            "the clipboard's source marked its content as not for clipboard readers",
        ));
    }
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
        return Ok((None, 0));
    }
    let Some(bytes) = clipboard_bytes(CF_UNICODETEXT) else {
        return Ok((None, 0));
    };
    let text = utf16_text(&bytes);
    let total = text.chars().count();
    Ok((Some(text.chars().take(max_chars).collect()), total))
}

/// Formats whose clipboard handle is a GDI object, not memory: they cannot
/// be copied byte-wise. Bitmaps survive anyway as CF_DIB/CF_DIBV5, which
/// Windows synthesises from them.
const GDI_FORMATS: [u32; 8] = [2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E];
/// The most clipboard content a paste preserves; beyond it the paste is
/// refused rather than lose what the user copied.
const MAX_PRESERVED_BYTES: usize = 32 * 1024 * 1024;

thread_local! {
    /// The formats this thread's paste window offers, each with its bytes,
    /// and how often one was asked for.
    static PASTE_FORMATS: std::cell::RefCell<Vec<(u32, Vec<u8>)>> = const { std::cell::RefCell::new(Vec::new()) };
    static PASTE_RENDERS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Requests for the text from anyone but the paste target, not answered.
    static PASTE_REFUSED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The process the text is for, and whether the paste has been asked for.
    static PASTE_TARGET: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static PASTE_KEYS_SENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the process now reading the clipboard is the one being pasted
/// into. A request from anyone else -- clipboard managers, history, remote
/// desktop clipboard sync -- gets no data and leaves the offer pending.
fn requester_is_target() -> bool {
    let requester = unsafe { GetOpenClipboardWindow() }.unwrap_or_default();
    if requester.0.is_null() {
        // Opened without a window: only plausible once the paste is under way.
        return PASTE_KEYS_SENT.with(|sent| sent.get());
    }
    let mut process = 0u32;
    unsafe { GetWindowThreadProcessId(requester, Some(&mut process)) };
    process != 0 && process == PASTE_TARGET.with(|target| target.get())
}

/// Every format on the clipboard, copied, to put back afterwards.
fn save_clipboard() -> Result<Vec<(u32, Vec<u8>)>> {
    let _open = OpenedClipboard::open(None)?;
    let mut saved = Vec::new();
    let mut total = 0usize;
    let mut format = 0u32;
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if GDI_FORMATS.contains(&format) {
            continue;
        }
        if let Some(bytes) = clipboard_bytes(format) {
            total += bytes.len();
            if total > MAX_PRESERVED_BYTES {
                return Err(Error::new(
                    "DRIVER.CLIPBOARD_TOO_LARGE",
                    "the clipboard holds more than a paste can preserve; nothing was pasted",
                ));
            }
            saved.push((format, bytes));
        }
    }
    Ok(saved)
}

fn global_copy(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let data = GlobalLock(global) as *mut u8;
        if data.is_null() {
            let _ = GlobalFree(global);
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        let _ = GlobalUnlock(global);
        Some(global)
    }
}

unsafe extern "system" fn paste_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let offered = |format: u32| {
        PASTE_FORMATS.with(|formats| formats.borrow().iter().any(|(f, _)| *f == format))
    };
    let render = |format: u32| {
        let bytes = PASTE_FORMATS.with(|formats| {
            formats
                .borrow()
                .iter()
                .find(|(f, _)| *f == format)
                .map(|(_, bytes)| bytes.clone())
        });
        if let Some(global) = bytes.as_deref().and_then(global_copy) {
            let _ = SetClipboardData(format, HANDLE(global.0));
        }
        PASTE_RENDERS.with(|renders| renders.set(renders.get() + 1));
    };
    match message {
        // The target asked for a format: it is pasting now. Anyone else is
        // refused, so the offer stays pending for the target.
        WM_RENDERFORMAT if offered(wparam.0 as u32) => {
            if requester_is_target() {
                render(wparam.0 as u32);
            } else {
                PASTE_REFUSED.with(|refused| refused.set(refused.get() + 1));
            }
            LRESULT(0)
        }
        // Never rendered for everyone: the clipboard is restored before the
        // window goes, and an unrendered offer simply lapses.
        WM_RENDERALLFORMATS => LRESULT(0),
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// A message-only window owning the clipboard for one paste.
struct PasteWindow(HWND);

impl PasteWindow {
    fn create() -> Result<Self> {
        static CLASS: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
        let class_name = CLASS.get_or_init(|| {
            let name: Vec<u16> = format!("AadPasteOwner_{}", std::process::id())
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(paste_window_proc),
                lpszClassName: PCWSTR(name.as_ptr()),
                ..Default::default()
            };
            unsafe { RegisterClassExW(&class) };
            name
        });
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )
        }
        .map_err(|error| {
            Error::new(
                "DRIVER.UNAVAILABLE",
                format!("no clipboard owner window: {error}"),
            )
            .retryable()
        })?;
        Ok(Self(hwnd))
    }
}

impl Drop for PasteWindow {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// The first format this thread's paste offers: the one that tells our
/// offer from a copy of its markers.
fn first_format() -> u32 {
    PASTE_FORMATS.with(|formats| formats.borrow().first().map_or(CF_UNICODETEXT, |(f, _)| *f))
}

/// Put a delayed-rendering offer of every format on the clipboard, owned by
/// `owner`.
fn offer_formats(owner: HWND) -> Result<()> {
    let _open = OpenedClipboard::open(Some(owner))?;
    unsafe { EmptyClipboard() }
        .map_err(|error| Error::new("DRIVER.CLIPBOARD_BUSY", format!("clipboard: {error}")))?;
    // SetClipboardData returns NULL for a delayed offer by design, so
    // success is read back rather than taken from its result.
    let formats: Vec<u32> =
        PASTE_FORMATS.with(|formats| formats.borrow().iter().map(|(f, _)| *f).collect());
    for format in &formats {
        let _ = unsafe { SetClipboardData(*format, HANDLE::default()) };
    }
    if formats
        .iter()
        .any(|format| unsafe { IsClipboardFormatAvailable(*format) }.is_err())
    {
        return Err(Error::new(
            "DRIVER.CLIPBOARD_BUSY",
            "the clipboard did not accept the paste's offer",
        ));
    }
    // Ask well-behaved watchers -- Windows' clipboard history and cloud
    // clipboard among them -- not to ask at all. (Measured: without this the
    // history service's refused request left Edit controls unable to paste.)
    for name in [
        EXCLUDE_FROM_MONITORS,
        CAN_INCLUDE_IN_HISTORY,
        CAN_UPLOAD_TO_CLOUD,
    ] {
        if let Some(global) = global_copy(&0u32.to_le_bytes()) {
            let _ = unsafe { SetClipboardData(clipboard_format(name), HANDLE(global.0)) };
        }
    }
    Ok(())
}

/// Whether the clipboard now looks like a re-published copy of our offer:
/// its markers without its text. Clipboard sync tools do this.
fn echo_of_offer() -> bool {
    let Ok(_open) = OpenedClipboard::open(None) else {
        return false;
    };
    unsafe {
        IsClipboardFormatAvailable(clipboard_format(EXCLUDE_FROM_MONITORS)).is_ok()
            && IsClipboardFormatAvailable(first_format()).is_err()
    }
}

/// Wait (bounded) until nobody asks for the offer and nobody holds the
/// clipboard open, so the target will be able to open it.
fn settle_clipboard(owner: HWND) {
    let until = Instant::now() + Duration::from_millis(600);
    let mut quiet_since = Instant::now();
    let mut refusals = PASTE_REFUSED.with(|refused| refused.get());
    while Instant::now() < until {
        pump_for(Duration::from_millis(15));
        let now = PASTE_REFUSED.with(|refused| refused.get());
        // Opening it is the only reliable test that nobody holds it: a
        // reader that opened it without a window is invisible otherwise.
        let held = match unsafe { OpenClipboard(owner) } {
            Ok(()) => {
                let _ = unsafe { CloseClipboard() };
                false
            }
            Err(_) => true,
        };
        if now != refusals || held {
            refusals = now;
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= Duration::from_millis(150) {
            return;
        }
    }
}

fn still_offered(owner: HWND) -> bool {
    unsafe { GetClipboardOwner() }.is_ok_and(|current| current == owner)
        && unsafe { IsClipboardFormatAvailable(first_format()) }.is_ok()
}

/// Put the saved formats back.
fn restore_clipboard(owner: HWND, saved: &[(u32, Vec<u8>)]) -> bool {
    let Ok(_open) = OpenedClipboard::open(Some(owner)) else {
        return false;
    };
    if unsafe { EmptyClipboard() }.is_err() {
        return false;
    }
    let mut restored = true;
    for (format, bytes) in saved {
        restored &= global_copy(bytes)
            .is_some_and(|global| unsafe { SetClipboardData(*format, HANDLE(global.0)) }.is_ok());
    }
    restored
}

/// One paste of text through the clipboard, on this thread.
///
/// [`Paste::prepare`] copies what the clipboard holds and changes nothing;
/// [`Paste::offer`] then puts the text up for `target` alone. The caller
/// asks the target to paste (and says so with [`Paste::asked`]), waits with
/// [`Paste::consumed`], and puts the clipboard back with [`Paste::restore`],
/// or with [`Paste::abandon`] on any failure after the offer.
pub struct Paste {
    saved: Vec<(u32, Vec<u8>)>,
    owner: PasteWindow,
}

impl Paste {
    /// `target` is the process the text is for: only it is given the text.
    pub fn prepare(text: &str, target: u32) -> Result<Self> {
        Self::prepare_formats(vec![(CF_UNICODETEXT, unicode_text(text))], target)
    }

    /// A paste of several formats at once, each with its bytes as the
    /// clipboard holds them, in the order programs should prefer them.
    pub fn prepare_formats(formats: Vec<(u32, Vec<u8>)>, target: u32) -> Result<Self> {
        if formats.is_empty() {
            return Err(Error::invalid("a paste needs at least one format"));
        }
        let saved = save_clipboard()?;
        let owner = PasteWindow::create()?;
        PASTE_FORMATS.with(|slot| *slot.borrow_mut() = formats);
        PASTE_RENDERS.with(|renders| renders.set(0));
        PASTE_REFUSED.with(|refused| refused.set(0));
        PASTE_KEYS_SENT.with(|sent| sent.set(false));
        PASTE_TARGET.with(|slot| slot.set(target));
        Ok(Self { saved, owner })
    }

    /// Offers the text, lets clipboard watchers react (they are refused and
    /// let go), and offers again if one of them replaced the clipboard --
    /// clipboard sync tools re-publish what they saw. Returns only once an
    /// offer that is still ours sits on a clipboard nobody holds open.
    pub fn offer(&self) -> Result<()> {
        for _ in 0..3 {
            offer_formats(self.owner.0)?;
            settle_clipboard(self.owner.0);
            if still_offered(self.owner.0) {
                return Ok(());
            }
        }
        Err(Error::new(
            "DRIVER.CLIPBOARD_BUSY",
            "another application keeps replacing the clipboard; nothing was pasted",
        ))
    }

    /// The target has been asked to paste (keys sent, or a message): a
    /// request through a window-less open is now taken as its own.
    pub fn asked(&self) {
        PASTE_KEYS_SENT.with(|sent| sent.set(true));
    }

    /// Waits up to `timeout` for the target to ask for the text.
    pub fn consumed(&self, timeout: Duration) -> bool {
        self.consumed_cancellable(timeout, None)
    }

    pub fn consumed_cancellable(
        &self,
        timeout: Duration,
        cancellation: Option<&std::sync::atomic::AtomicBool>,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while PASTE_RENDERS.with(|renders| renders.get()) == 0 && Instant::now() < deadline {
            if cancellation.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)) {
                break;
            }
            pump_for(Duration::from_millis(10));
        }
        PASTE_RENDERS.with(|renders| renders.get()) > 0
    }

    /// How many times the target asked for one of the formats.
    pub fn renders(&self) -> u32 {
        PASTE_RENDERS.with(|renders| renders.get())
    }

    /// After the target first asked, waits until it has gone `quiet` without
    /// asking for more (a word processor asks for several formats in turn),
    /// at most `limit`.
    pub fn settled(&self, quiet: Duration, limit: Duration) {
        let until = Instant::now() + limit;
        let mut seen = self.renders();
        let mut since = Instant::now();
        while Instant::now() < until && since.elapsed() < quiet {
            pump_for(Duration::from_millis(10));
            let now = self.renders();
            if now != seen {
                seen = now;
                since = Instant::now();
            }
        }
    }

    /// How many requests for the text from other processes were refused.
    pub fn refused(&self) -> u32 {
        PASTE_REFUSED.with(|refused| refused.get())
    }

    /// How many formats the clipboard held, to be put back.
    pub fn saved_formats(&self) -> usize {
        self.saved.len()
    }

    /// Puts the clipboard back after a failure; whether that worked.
    pub fn abandon(&self) -> bool {
        let restored = restore_clipboard(self.owner.0, &self.saved);
        PASTE_FORMATS.with(|slot| slot.borrow_mut().clear());
        restored
    }

    /// Puts the clipboard back after the paste; whether that worked.
    ///
    /// The target reads while it holds the clipboard open; opening it
    /// ourselves (inside the restore) waits for it to finish. A clipboard
    /// sync tool may re-publish what it saw of the offer (its markers, never
    /// its text) after the restore: that is watched for briefly and restored
    /// once more; anything else replacing it is the user's.
    pub fn restore(&self) -> bool {
        let mut restored = self.abandon();
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let watch_until = Instant::now() + Duration::from_millis(1_000);
        let mut restored_again = false;
        while Instant::now() < watch_until {
            pump_for(Duration::from_millis(50));
            if unsafe { GetClipboardSequenceNumber() } != sequence {
                if !restored_again && echo_of_offer() {
                    restored = restore_clipboard(self.owner.0, &self.saved);
                    restored_again = true;
                } else {
                    break;
                }
            }
        }
        restored
    }
}

/// Text as CF_UNICODETEXT holds it: UTF-16, NUL-terminated.
fn unicode_text(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

const CF_DIBV5: u32 = 17;
const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;
const HTML_FORMAT: &str = "HTML Format";
const RTF_FORMAT: &str = "Rich Text Format";
const PNG_FORMAT: &str = "PNG";
const PREFERRED_DROP_EFFECT: &str = "Preferred DropEffect";
const DROPEFFECT_COPY: u32 = 1;

const FRAGMENT_START: &str = "<!--StartFragment-->";
const FRAGMENT_END: &str = "<!--EndFragment-->";

/// HTML as the "HTML Format" (CF_HTML) holds it: a header of byte offsets to
/// the document and to the fragment that was copied, then the document, in
/// UTF-8. A document that marks its fragment keeps its markers.
pub fn cf_html(html: &str) -> Vec<u8> {
    let marked = html.contains(FRAGMENT_START) && html.contains(FRAGMENT_END);
    let document = if marked {
        html.to_string()
    } else {
        let fragment = aad_platform::clip::html_fragment(html);
        format!("<html><body>\r\n{FRAGMENT_START}{fragment}{FRAGMENT_END}\r\n</body></html>")
    };
    let header_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let (start, end) = match (document.find(FRAGMENT_START), document.rfind(FRAGMENT_END)) {
        (Some(start), Some(end)) if start + FRAGMENT_START.len() <= end => {
            (start + FRAGMENT_START.len(), end)
        }
        _ => (0, document.len()),
    };
    let mut bytes = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        header_len,
        header_len + document.len(),
        header_len + start,
        header_len + end
    )
    .into_bytes();
    bytes.extend_from_slice(document.as_bytes());
    bytes.push(0);
    bytes
}

/// The fragment an "HTML Format" clipboard entry holds, by its header.
pub fn html_of_cf_html(bytes: &[u8]) -> Option<String> {
    let bytes = &bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())];
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let offset = |name: &str| -> Option<usize> {
        let at = head.find(name)? + name.len();
        head[at..]
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    let (start, end) = match (offset("StartFragment:"), offset("EndFragment:")) {
        (Some(start), Some(end)) if start <= end && end <= bytes.len() => (start, end),
        _ => match (offset("StartHTML:"), offset("EndHTML:")) {
            (Some(start), Some(end)) if start <= end && end <= bytes.len() => (start, end),
            _ => return None,
        },
    };
    Some(String::from_utf8_lossy(&bytes[start..end]).into_owned())
}

/// A PNG as CF_DIBV5: a BITMAPV5HEADER with 32-bit BGRA rows bottom-up and
/// the alpha channel's mask, which keeps transparency where programs read it.
pub fn dibv5(png: &[u8]) -> Result<Vec<u8>> {
    let image = image::load_from_memory(png)
        .map_err(|error| Error::invalid(format!("not a picture: {error}")))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    let mut bytes = Vec::with_capacity(124 + (width * height * 4) as usize);
    let u32le = |bytes: &mut Vec<u8>, value: u32| bytes.extend_from_slice(&value.to_le_bytes());
    u32le(&mut bytes, 124); // bV5Size
    u32le(&mut bytes, width);
    u32le(&mut bytes, height); // positive: bottom-up
    bytes.extend_from_slice(&1u16.to_le_bytes()); // planes
    bytes.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    u32le(&mut bytes, 3); // BI_BITFIELDS
    u32le(&mut bytes, width * height * 4);
    u32le(&mut bytes, 2835); // 72 dpi
    u32le(&mut bytes, 2835);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0x00FF_0000); // red
    u32le(&mut bytes, 0x0000_FF00); // green
    u32le(&mut bytes, 0x0000_00FF); // blue
    u32le(&mut bytes, 0xFF00_0000); // alpha
    u32le(&mut bytes, 0x7352_4742); // LCS_sRGB
    bytes.extend_from_slice(&[0u8; 36 + 12]); // endpoints, gamma
    u32le(&mut bytes, 4); // LCS_GM_IMAGES
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    for row in image.rows().rev() {
        for pixel in row {
            let [r, g, b, a] = pixel.0;
            bytes.extend_from_slice(&[b, g, r, a]);
        }
    }
    Ok(bytes)
}

/// A device-independent bitmap (CF_DIB or CF_DIBV5) as PNG bytes. Rows may
/// run either way; 24- and 32-bit pixels are read, with alpha when the
/// header has a mask for it and it is not all zero.
pub fn png_of_dib(dib: &[u8]) -> Option<Vec<u8>> {
    let u32at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(dib.get(at..at + 4)?.try_into().ok()?))
    };
    let header = u32at(0)? as usize;
    let width = u32at(4)? as i32;
    let height = u32at(8)? as i32;
    let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
    let compression = u32at(16)?;
    let colors = u32at(32)? as usize;
    if width <= 0
        || height == 0
        || !(bits == 24 || bits == 32)
        || width > 32_768
        || height.abs() > 32_768
    {
        return None;
    }
    let (width, rows) = (width as usize, height.unsigned_abs() as usize);
    // Bit fields after a 40-byte header take three more masks.
    let masks = if header == 40 && compression == 3 {
        12
    } else {
        0
    };
    let start = header + masks + colors * 4;
    let stride = (width * usize::from(bits) / 8 + 3) & !3;
    let pixels = dib.get(start..start + stride * rows)?;
    let alpha_mask = if header >= 56 { u32at(52)? } else { 0 };
    let mut image = image::RgbaImage::new(width as u32, rows as u32);
    let mut any_alpha = false;
    for y in 0..rows {
        let source = if height > 0 { rows - 1 - y } else { y };
        let row = &pixels[source * stride..];
        for x in 0..width {
            let at = x * usize::from(bits) / 8;
            let (b, g, r) = (row[at], row[at + 1], row[at + 2]);
            let a = if bits == 32 { row[at + 3] } else { 255 };
            any_alpha |= a != 0;
            image.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, a]));
        }
    }
    // Plain 32-bit bitmaps leave the fourth byte zero: that is opaque.
    if bits == 32 && (alpha_mask == 0 || !any_alpha) {
        for pixel in image.pixels_mut() {
            pixel.0[3] = 255;
        }
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// A file list as CF_HDROP holds it: a DROPFILES header, then each path in
/// UTF-16, NUL-terminated, and one more NUL.
pub fn hdrop(paths: &[std::path::PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20u32.to_le_bytes()); // pFiles
    bytes.extend_from_slice(&[0u8; 8]); // pt
    bytes.extend_from_slice(&0u32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for path in paths {
        for unit in path.as_os_str().encode_wide().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes
}

/// The paths a CF_HDROP entry lists.
pub fn paths_of_hdrop(bytes: &[u8]) -> Vec<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    if bytes.len() < 20 {
        return Vec::new();
    }
    let at = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let wide = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) != 0;
    let Some(list) = bytes.get(at..) else {
        return Vec::new();
    };
    let names: Vec<std::ffi::OsString> = if wide {
        let units: Vec<u16> = list
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        units
            .split(|unit| *unit == 0)
            .take_while(|name| !name.is_empty())
            .map(std::ffi::OsString::from_wide)
            .collect()
    } else {
        list.split(|byte| *byte == 0)
            .take_while(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned().into())
            .collect()
    };
    names.into_iter().map(std::path::PathBuf::from).collect()
}

/// Every format of a [`Clip`](aad_platform::clip::Clip), in the order
/// programs should prefer them, with the names they go by.
type Formats = (Vec<(u32, Vec<u8>)>, Vec<&'static str>);

fn formats_of(clip: &aad_platform::clip::Clip) -> Result<Formats> {
    let mut formats = Vec::new();
    let mut names = Vec::new();
    if let Some(text) = &clip.text {
        formats.push((CF_UNICODETEXT, unicode_text(text)));
        names.push("text");
    }
    if let Some(html) = &clip.html {
        formats.push((clipboard_format(HTML_FORMAT), cf_html(html)));
        names.push("html");
    }
    if let Some(rtf) = &clip.rtf {
        let mut bytes = rtf.as_bytes().to_vec();
        bytes.push(0);
        formats.push((clipboard_format(RTF_FORMAT), bytes));
        names.push("rtf");
    }
    if let Some(png) = &clip.image {
        let dib = dibv5(png)?;
        formats.push((clipboard_format(PNG_FORMAT), png.clone()));
        formats.push((CF_DIBV5, dib));
        names.push("image");
    }
    if !clip.files.is_empty() {
        formats.push((CF_HDROP, hdrop(&clip.files)));
        formats.push((
            clipboard_format(PREFERRED_DROP_EFFECT),
            DROPEFFECT_COPY.to_le_bytes().to_vec(),
        ));
        names.push("files");
    }
    Ok((formats, names))
}

/// The process of the window in front: the one a paste key goes to.
fn foreground_process() -> Option<u32> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let window = unsafe { GetForegroundWindow() };
    if window.0.is_null() {
        return None;
    }
    let mut process = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
    (process != 0).then_some(process)
}

/// How long a paste waits for the target to ask: a file manager may read
/// a file list well after the key, while copying begins.
const ASK_TIMEOUT: Duration = Duration::from_secs(3);
const ASK_TIMEOUT_FILES: Duration = Duration::from_secs(10);

/// Pastes every format of `clip` into the window in front, on a thread of
/// its own (it owns the clipboard and must answer the target's requests),
/// and puts the clipboard back afterwards. `press` presses the paste key.
pub fn paste(
    clip: aad_platform::clip::Clip,
    press: aad_platform::clip::Press,
) -> aad_platform::clip::Pasting {
    std::thread::spawn(move || {
        let (formats, names) = formats_of(&clip).map_err(|e| e.message)?;
        let target = foreground_process().ok_or("no window is in front to paste into")?;
        let paste = Paste::prepare_formats(formats, target).map_err(|e| e.message)?;
        if let Err(error) = paste.offer() {
            paste.abandon();
            return Err(error.message);
        }
        // The key is pressed elsewhere while this thread keeps answering:
        // the target asks for the formats as soon as it sees the key, and a
        // request this thread is too busy to answer comes back empty to it.
        paste.asked();
        let pressing = std::thread::spawn(press);
        while !pressing.is_finished() {
            pump_for(Duration::from_millis(5));
        }
        let pressed = pressing
            .join()
            .unwrap_or_else(|_| Err("pressing the paste key failed".into()));
        if let Err(error) = pressed {
            paste.abandon();
            return Err(error);
        }
        let files = clip.has_files();
        let taken = paste.consumed(if files {
            ASK_TIMEOUT_FILES
        } else {
            ASK_TIMEOUT
        });
        if taken {
            // A word processor asks for format after format; a file manager
            // may come back for the list while it starts copying.
            let quiet = Duration::from_millis(if files { 1_500 } else { 400 });
            paste.settled(quiet, Duration::from_secs(10));
        }
        let restored = paste.restore();
        Ok(
            serde_json::json!({"pasted": taken, "formats": names, "restored": restored,
            "refused": paste.refused()}),
        )
    })
}

/// Everything the clipboard holds that a viewer can take: text, HTML, a
/// picture (PNG, else a bitmap turned into PNG) and files. Nothing is read
/// when its source marked it as not for clipboard readers.
pub fn copied() -> Result<aad_platform::clip::Copied> {
    let _open = OpenedClipboard::open(None)?;
    let mut copied = aad_platform::clip::Copied::default();
    let has = |format: u32| unsafe { IsClipboardFormatAvailable(format) }.is_ok();
    if has(clipboard_format(EXCLUDE_FROM_MONITORS)) {
        copied.protected = true;
        return Ok(copied);
    }
    if has(CF_UNICODETEXT) {
        copied.text = clipboard_bytes(CF_UNICODETEXT).map(|bytes| utf16_text(&bytes));
    }
    let html = clipboard_format(HTML_FORMAT);
    if has(html) {
        copied.html = clipboard_bytes(html).and_then(|bytes| html_of_cf_html(&bytes));
    }
    let png = clipboard_format(PNG_FORMAT);
    if has(png) {
        copied.image = clipboard_bytes(png).filter(|bytes| bytes.starts_with(b"\x89PNG"));
    }
    if copied.image.is_none() {
        for format in [CF_DIBV5, CF_DIB] {
            if has(format) {
                copied.image = clipboard_bytes(format).and_then(|dib| png_of_dib(&dib));
                if copied.image.is_some() {
                    break;
                }
            }
        }
    }
    if has(CF_HDROP) {
        copied.files = clipboard_bytes(CF_HDROP)
            .map(|bytes| paths_of_hdrop(&bytes))
            .unwrap_or_default();
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_offsets_point_at_the_fragment() {
        for html in [
            "<b>bold</b> 中文",
            "<html><body><!--StartFragment--><i>x</i><!--EndFragment--></body></html>",
        ] {
            let bytes = cf_html(html);
            let fragment = html_of_cf_html(&bytes).unwrap();
            assert_eq!(fragment, aad_platform::clip::html_fragment(html));
            let text = String::from_utf8_lossy(&bytes);
            let start: usize = text["Version:0.9\r\nStartHTML:".len()..][..10]
                .parse()
                .unwrap();
            assert!(text[start..].starts_with("<html>"), "{text}");
        }
    }

    #[test]
    fn a_file_list_reads_back_as_it_was_written() {
        let paths = vec![
            std::path::PathBuf::from(r"C:\Temp\a b.txt"),
            std::path::PathBuf::from(r"C:\临时\图.png"),
        ];
        assert_eq!(paths_of_hdrop(&hdrop(&paths)), paths);
    }

    #[test]
    fn a_picture_becomes_a_bottom_up_dib_with_alpha_and_back() {
        let mut png = Vec::new();
        let mut image = image::RgbaImage::new(2, 2);
        image.put_pixel(0, 0, image::Rgba([255, 0, 0, 128]));
        image.put_pixel(1, 1, image::Rgba([0, 0, 255, 255]));
        image::DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let dib = dibv5(&png).unwrap();
        assert_eq!(dib.len(), 124 + 16);
        // The top-left pixel is in the last row, as BGRA.
        assert_eq!(&dib[124 + 8..124 + 12], &[0, 0, 255, 128]);
        let back = image::load_from_memory(&png_of_dib(&dib).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(back, image);
    }
}
