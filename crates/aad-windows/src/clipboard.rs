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
    /// The text this thread's paste window offers, and how often it was asked for.
    static PASTE_TEXT: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
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
    let render = || {
        let text = PASTE_TEXT.with(|text| {
            let text = text.borrow();
            text.iter()
                .chain(std::iter::once(&0u16))
                .flat_map(|unit| unit.to_le_bytes())
                .collect::<Vec<u8>>()
        });
        if let Some(global) = global_copy(&text) {
            let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(global.0));
        }
        PASTE_RENDERS.with(|renders| renders.set(renders.get() + 1));
    };
    match message {
        // The target asked for the text: it is pasting now. Anyone else is
        // refused, so the offer stays pending for the target.
        WM_RENDERFORMAT if wparam.0 as u32 == CF_UNICODETEXT => {
            if requester_is_target() {
                render();
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

/// Put a delayed-rendering text offer on the clipboard, owned by `owner`.
fn offer_text(owner: HWND) -> Result<()> {
    let _open = OpenedClipboard::open(Some(owner))?;
    unsafe { EmptyClipboard() }
        .map_err(|error| Error::new("DRIVER.CLIPBOARD_BUSY", format!("clipboard: {error}")))?;
    // SetClipboardData returns NULL for a delayed offer by design, so
    // success is read back rather than taken from its result.
    let _ = unsafe { SetClipboardData(CF_UNICODETEXT, HANDLE::default()) };
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
        return Err(Error::new(
            "DRIVER.CLIPBOARD_BUSY",
            "the clipboard did not accept the text offer",
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
            && IsClipboardFormatAvailable(CF_UNICODETEXT).is_err()
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
        && unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_ok()
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
        let saved = save_clipboard()?;
        let owner = PasteWindow::create()?;
        PASTE_TEXT.with(|slot| *slot.borrow_mut() = text.encode_utf16().collect());
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
            offer_text(self.owner.0)?;
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
        PASTE_TEXT.with(|slot| slot.borrow_mut().clear());
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
