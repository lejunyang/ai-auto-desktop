//! The backend boundary and the snapshot store.
//!
//! A [`Backend`] is the only part of the driver that touches the operating
//! system.  Everything above it — handle validation, staleness checks, action
//! dispatch and JSON shaping — is platform independent and covered by tests
//! that run everywhere.

use crate::model::{Bounds, Node, Snapshot, WindowInfo};
pub use aad_platform::{
    ImageBox, Pixels, PointerButton, Recognition, ScrollDirection, ScrollSpec, TextSpan,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A structured driver failure.
#[derive(Clone, Debug)]
pub struct DriverError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    /// One of `none`, `not_applied`, `applied`, `unknown`.
    pub effect: String,
    pub details: serde_json::Map<String, serde_json::Value>,
}

impl DriverError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            retryable: false,
            effect: "not_applied".to_string(),
            details: serde_json::Map::new(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("DRIVER.INVALID_REQUEST", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new("DRIVER.UNAVAILABLE", message).retryable()
    }

    /// The addressed snapshot no longer describes the live UI.
    pub fn stale(message: impl Into<String>) -> Self {
        Self::new("DRIVER.STALE_HANDLE", message)
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn with_effect(mut self, effect: &str) -> Self {
        self.effect = effect.to_string();
        self
    }

    pub fn with_detail(mut self, key: &str, value: serde_json::Value) -> Self {
        self.details.insert(key.to_string(), value);
        self
    }

    pub fn into_automation_error(self) -> aad_core::AutomationError {
        aad_core::AutomationError::new(self.code, self.message)
            .with_category("driver")
            .with_retryable(self.retryable)
            .with_effect(self.effect)
            .with_details(self.details)
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DriverError {}

/// A platform crate's failure, as the driver reports it.
impl From<aad_platform::Error> for DriverError {
    fn from(error: aad_platform::Error) -> Self {
        Self {
            code: error.code.to_string(),
            message: error.message,
            retryable: error.retryable,
            effect: error.effect.to_string(),
            details: error.details,
        }
    }
}

pub type Result<T> = std::result::Result<T, DriverError>;

pub fn ensure_input_active(deadline: Instant, cancellation: Option<&AtomicBool>) -> Result<()> {
    if cancellation.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err(DriverError::new("DRIVER.CANCELLED", "input was cancelled"));
    }
    ensure_input_deadline(deadline)
}

/// Internal execution control; never accepted from serialized action arguments.
pub struct InputControl {
    pub deadline: Instant,
    pub cancellation: Option<Arc<AtomicBool>>,
}
impl InputControl {
    pub fn check(&self) -> Result<()> {
        ensure_input_active(self.deadline, self.cancellation.as_deref())
    }
}
impl Default for InputControl {
    fn default() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(30),
            cancellation: None,
        }
    }
}

/// Already validated element operations, with borrowed text kept out of Debug.
pub enum ElementInput<'a> {
    Focus,
    Invoke,
    PointerClick,
    SetValue(&'a str),
    SecretInput(&'a str),
    TypeText { text: &'a str, keys: bool },
    Paste(&'a str),
    WindowAction(&'a str),
}

pub fn ensure_input_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(DriverError::new(
            "DRIVER.TIMEOUT",
            "input deadline expired before dispatch",
        ))
    } else {
        Ok(())
    }
}

/// Validate the bounded dwell before any key or button goes down.
pub fn ensure_hold_budget(hold_ms: u64, deadline: Instant) -> Result<()> {
    if !(1..=5_000).contains(&hold_ms) {
        return Err(DriverError::invalid("hold_ms must be between 1 and 5000"));
    }
    if deadline.saturating_duration_since(Instant::now()) <= Duration::from_millis(hold_ms) {
        return Err(DriverError::new(
            "DRIVER.TIMEOUT",
            "hold exceeds the remaining input deadline",
        ));
    }
    Ok(())
}

/// Synchronous dwell: callers retain their release guard until this returns.
/// Verification never refocuses or activates a changed target.
pub fn wait_input_hold(
    hold_ms: u64,
    deadline: Instant,
    mut verify: impl FnMut() -> Result<()>,
) -> Result<()> {
    let end = Instant::now() + Duration::from_millis(hold_ms);
    loop {
        ensure_input_deadline(deadline)?;
        verify()?;
        ensure_input_deadline(deadline)?;
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        std::thread::sleep(
            (end - now)
                .min(deadline - now)
                .min(Duration::from_millis(20)),
        );
    }
}

/// A window plus the element tree captured from it.
#[derive(Clone, Debug)]
pub struct CapturedTree {
    pub window: WindowInfo,
    pub nodes: Vec<Node>,
    pub root_id: Option<String>,
    pub truncated: bool,
}

/// How deep and how wide a capture is allowed to go.
#[derive(Clone, Copy, Debug)]
pub struct CaptureLimits {
    pub max_depth: u32,
    pub max_nodes: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        // Deep enough for real applications, bounded enough that a pathological
        // tree cannot stall the agent or exhaust memory.
        Self {
            max_depth: 32,
            max_nodes: 1_000,
        }
    }
}

impl CaptureLimits {
    pub const MAX_DEPTH: u32 = 128;
    pub const MAX_NODES: usize = 5_000;

    pub fn clamp(self) -> Self {
        Self {
            max_depth: self.max_depth.clamp(1, Self::MAX_DEPTH),
            max_nodes: self.max_nodes.clamp(1, Self::MAX_NODES),
        }
    }
}

/// Everything a pointer gesture beyond a plain left click needs.
///
/// The point acted on is always derived inside the backend from the element's
/// bounds and `fraction`; a caller never supplies a screen coordinate, which is
/// what keeps the "observe first" rule intact for pointer actions too.
#[derive(Clone, Debug, Default)]
pub struct PointerGesture {
    /// Internal cooperative cancellation. Never deserialized from action arguments.
    pub cancellation: Option<Arc<AtomicBool>>,
    /// Internal execution deadline, derived by the provider from its timeout.
    /// This is never accepted as a caller-supplied pointer field.
    pub deadline: Option<Instant>,
    /// Dwell after button down, before release or the first drag movement.
    pub hold_ms: Option<u64>,
    /// Which button. Defaults to the left button.
    pub button: PointerButton,
    /// 1 for a single click, 2 for a double click.
    pub count: u8,
    /// Modifier keys held for a click or drag: ctrl, shift, alt, meta.
    pub modifiers: Vec<String>,
    /// Where inside the first (or only) element to act, as (fx, fy) each in
    /// 0.0..=1.0. `None` means the centre, matching a plain click.
    pub fraction: Option<(f64, f64)>,
    /// Where inside the destination element to release a drag. `None` means its
    /// centre. Ignored by single-element gestures.
    pub to_fraction: Option<(f64, f64)>,
    /// Intermediate fractions of the same observed surface. The provider has
    /// checked every caller-supplied anchor against the source snapshot/node.
    pub path: Vec<(f64, f64)>,
    /// When set, the gesture is a scroll rather than a click.
    pub scroll: Option<ScrollSpec>,
    /// When set, the gesture hovers: move to the point and dwell this long.
    pub hover_ms: Option<u64>,
}

/// The operations a platform backend must provide.
pub trait Backend: Send + Sync {
    /// Defaults only bracket synchronous operations. Native implementations
    /// also check immediately before OS dispatch and inside bounded waits.
    fn element_input_controlled(
        &self,
        window_id: &str,
        node: &Node,
        input: ElementInput<'_>,
        control: &InputControl,
    ) -> Result<serde_json::Value> {
        control.check()?;
        let result = match input {
            ElementInput::Focus => self
                .focus(window_id, node)
                .map(|()| serde_json::Value::Null),
            ElementInput::Invoke => self
                .invoke(window_id, node)
                .map(|()| serde_json::Value::Null),
            ElementInput::PointerClick => self
                .pointer_click(window_id, node)
                .map(|()| serde_json::Value::Null),
            ElementInput::SetValue(text) => self
                .set_value(window_id, node, text)
                .map(|()| serde_json::Value::Null),
            ElementInput::SecretInput(text) => self
                .secret_input(window_id, node, text)
                .map(|()| serde_json::Value::Null),
            ElementInput::TypeText { text, keys: true } => self
                .type_text_keys(window_id, node, text)
                .map(|()| serde_json::Value::Null),
            ElementInput::TypeText { text, keys: false } => self
                .type_text(window_id, node, text)
                .map(|()| serde_json::Value::Null),
            ElementInput::Paste(text) => self.paste(window_id, node, text),
            ElementInput::WindowAction(action) => self.window_action(window_id, node, action),
        };
        let result = result?;
        control
            .check()
            .map_err(|error| error.with_effect("unknown"))?;
        Ok(result)
    }
    fn global_key_controlled(
        &self,
        keys: &[String],
        control: &InputControl,
    ) -> Result<serde_json::Value> {
        control.check()?;
        let result = self.global_key(keys);
        let result = result?;
        control
            .check()
            .map_err(|error| error.with_effect("unknown"))?;
        Ok(result)
    }
    /// Replace an observed protected field without reading it or using the clipboard.
    fn secret_input(&self, _window_id: &str, _node: &Node, _text: &str) -> Result<()> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "secret input is not implemented on this platform",
        ))
    }
    /// Start recording a window's interactions. Where capture is not
    /// implemented this says so rather than starting a session that would
    /// never produce a step: a recorder that appears to work and records
    /// nothing is worse than one that refuses.
    fn open_capture(
        &self,
        _window_id: &str,
        _limit: usize,
    ) -> Result<Box<dyn crate::driver::CaptureSource>> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "recording interactions is only implemented on Windows so far",
        ))
    }

    /// Start a desktop-wide observation session, subscribed at the desktop
    /// root, that reports foreground switches itself.
    fn open_observation(
        &self,
        _limit: usize,
    ) -> Result<Box<dyn crate::observe_session::ObserveSource>> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "ambient observation is only implemented on Windows so far",
        ))
    }

    /// Enumerate top-level windows that a user could interact with.
    fn list_windows(&self) -> Result<Vec<WindowInfo>>;

    /// Capture the element tree of one window.
    fn capture(&self, window_id: &str, limits: CaptureLimits) -> Result<CapturedTree>;

    /// Confirm the live element still matches what the snapshot recorded.
    ///
    /// This is the check that makes stale handles safe: if the UI moved on,
    /// the action is refused instead of hitting the wrong element.
    fn verify(&self, window_id: &str, node: &Node) -> Result<bool>;

    fn focus(&self, window_id: &str, node: &Node) -> Result<()>;
    fn invoke(&self, window_id: &str, node: &Node) -> Result<()>;
    fn set_value(&self, window_id: &str, node: &Node, value: &str) -> Result<()>;
    fn type_text(&self, window_id: &str, node: &Node, text: &str) -> Result<()>;

    /// Type text as layout-aware virtual-key presses rather than Unicode packets.
    ///
    /// This is visible to Raw Input consumers, but can represent only characters
    /// the target window's active keyboard layout maps to physical keys. Backends
    /// without such a path fail explicitly; they must not silently fall back to
    /// Unicode because that would change the visibility the caller requested.
    fn type_text_keys(&self, _window_id: &str, _node: &Node, _text: &str) -> Result<()> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "layout-aware key typing is not implemented on this platform yet",
        ))
    }
    fn pointer_click(&self, window_id: &str, node: &Node) -> Result<()>;

    /// Press a key combination while the element holds focus.
    ///
    /// Unlike [`type_text`], which can only deliver characters, this expresses
    /// shortcuts and function keys — Ctrl+C, F5, Shift+End — that no amount of
    /// text can. `keys` is a chord: every modifier is held down, the main key is
    /// pressed and released, then the modifiers are released in reverse. The
    /// element is focused first, exactly like `type_text`, so the chord lands
    /// where the caller aimed it rather than wherever focus happened to be.
    ///
    /// Defaulted so a backend that has not implemented it degrades to a clear
    /// refusal rather than failing to compile.
    fn key_chord(&self, _window_id: &str, _node: &Node, _keys: &[String]) -> Result<()> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "sending key combinations is not implemented on this platform yet",
        ))
    }

    /// Timed dispatch hook. Backends with multi-step input must keep checking
    /// this deadline and release their own input before returning on failure.
    fn key_chord_with_deadline(
        &self,
        window_id: &str,
        node: &Node,
        keys: &[String],
        deadline: Instant,
    ) -> Result<()> {
        ensure_input_deadline(deadline)?;
        self.key_chord(window_id, node, keys)
    }

    fn key_chord_held(
        &self,
        window_id: &str,
        node: &Node,
        keys: &[String],
        hold_ms: Option<u64>,
        deadline: Instant,
    ) -> Result<()> {
        if hold_ms.is_some() {
            return Err(DriverError::new(
                "DRIVER.ACTION_UNSUPPORTED",
                "bounded key holds are not implemented on this platform",
            ));
        }
        self.key_chord_with_deadline(window_id, node, keys, deadline)
    }

    fn key_chord_controlled(
        &self,
        window_id: &str,
        node: &Node,
        keys: &[String],
        hold_ms: Option<u64>,
        deadline: Instant,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        ensure_input_active(deadline, cancellation.as_deref())?;
        self.key_chord_held(window_id, node, keys, hold_ms, deadline)
    }

    /// Perform a pointer gesture on a single element: a right or middle click, a
    /// double click, a scroll, a hover, or a click at a fraction of the element
    /// rather than its centre. The point is computed from the element's bounds,
    /// never given by the caller.
    ///
    /// Defaulted to a clear refusal so a backend that lacks it still compiles.
    fn pointer_gesture(
        &self,
        _window_id: &str,
        _node: &Node,
        _gesture: &PointerGesture,
    ) -> Result<()> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "pointer gestures are not implemented on this platform yet",
        ))
    }

    /// Drag from one element to another: press at the first, move across, and
    /// release at the second. Both ends are observed elements; the path between
    /// them is interpolated, so no leg of the drag is a caller-supplied point.
    ///
    /// Defaulted to a clear refusal so a backend that lacks it still compiles.
    fn drag(
        &self,
        _window_id: &str,
        _from: &Node,
        _to: &Node,
        _gesture: &PointerGesture,
    ) -> Result<()> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "drag is not implemented on this platform yet",
        ))
    }

    /// The pixels the live element shows, cut to its bounds; the whole window
    /// (without its invisible resize border) when `whole_window` is set.
    ///
    /// Read-only. The element must still match the snapshot; the backend reads
    /// its bounds live rather than trusting the captured ones.
    fn capture_pixels(
        &self,
        _window_id: &str,
        _node: &Node,
        _whole_window: bool,
    ) -> Result<Pixels> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "screenshots are not implemented on this platform yet",
        ))
    }

    /// Minimize, maximize, restore, close or activate the observed window,
    /// `node` being its own (root) node. Answers the window's state after.
    fn window_action(
        &self,
        _window_id: &str,
        _node: &Node,
        _action: &str,
    ) -> Result<serde_json::Value> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "window actions are not implemented on this platform yet",
        ))
    }

    /// The clipboard's text, at most `max_chars` characters of it, with the
    /// full length. `None` when the clipboard holds no text. Content that its
    /// source marked as excluded from clipboard monitors (password managers do)
    /// is refused rather than read.
    fn clipboard_text(&self, _max_chars: usize) -> Result<(Option<String>, usize)> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "reading the clipboard is not implemented on this platform yet",
        ))
    }

    /// Press a key chord wherever keyboard focus is, anchored on no element.
    /// The one un-anchored input; hosts gate it behind a grant of its own.
    /// Answers what little can be said: where focus was when it was sent.
    fn global_key(&self, _keys: &[String]) -> Result<serde_json::Value> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "global keys are not implemented on this platform yet",
        ))
    }

    /// Paste `text` into the element through the clipboard (focus, Ctrl+V),
    /// leaving the clipboard as it was. Answers whether the element took it.
    fn paste(&self, _window_id: &str, _node: &Node, _text: &str) -> Result<serde_json::Value> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "pasting is not implemented on this platform yet",
        ))
    }

    /// Recognise the text an image shows, with the platform's own OCR.
    /// `language` is a BCP-47 tag; `None` takes the user's languages.
    fn recognize_text(&self, _pixels: &Pixels, _language: Option<&str>) -> Result<Recognition> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "text recognition is not implemented on this platform yet",
        ))
    }

    /// A human-readable description of this backend, for diagnostics.
    fn describe(&self) -> serde_json::Value {
        serde_json::json!({"backend": "unknown"})
    }
}

/// Retains recent snapshots so targets can be validated against them.
///
/// Snapshots are also written to disk. A CLI invocation is a whole process, so
/// `aad find` and `aad do` are different processes: without persistence a
/// target could never be used by the command that follows the one that
/// produced it, which is the normal way both people and agents drive the CLI.
pub struct SnapshotStore {
    entries: Mutex<HashMap<String, (Snapshot, Instant)>>,
    revision: AtomicU64,
    capacity: usize,
    ttl: Duration,
    directory: Option<std::path::PathBuf>,
}

impl SnapshotStore {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            revision: AtomicU64::new(0),
            capacity,
            ttl,
            directory: None,
        }
    }

    /// Also persist snapshots under `directory`, so other processes can use them.
    pub fn persisted(mut self, directory: std::path::PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&directory);
        self.directory = Some(directory);
        self
    }

    /// Revisions must keep increasing across processes, or a target minted by
    /// one invocation would collide with a different tree captured by another.
    pub fn next_revision(&self) -> u64 {
        let local = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let floor = self
            .directory
            .as_ref()
            .map(|directory| Self::highest_persisted_revision(directory))
            .unwrap_or(0);
        let next = local.max(floor + 1);
        // Keep the in-process counter ahead of what is already on disk.
        self.revision.fetch_max(next, Ordering::SeqCst);
        next
    }

    fn highest_persisted_revision(directory: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return 0;
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .filter_map(|text| serde_json::from_str::<Snapshot>(&text).ok())
            .map(|snapshot| snapshot.revision)
            .max()
            .unwrap_or(0)
    }

    fn path_for(&self, snapshot_id: &str) -> Option<std::path::PathBuf> {
        // Reject anything that is not a plain identifier before it reaches the
        // filesystem: a snapshot id arrives from the caller.
        if snapshot_id.is_empty()
            || !snapshot_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return None;
        }
        self.directory
            .as_ref()
            .map(|directory| directory.join(format!("{snapshot_id}.json")))
    }

    pub fn insert(&self, snapshot: Snapshot) {
        if let Some(path) = self.path_for(&snapshot.snapshot_id) {
            if let Ok(text) = serde_json::to_string(&snapshot) {
                let _ = std::fs::write(path, text);
            }
            self.evict_persisted();
        }
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.insert(snapshot.snapshot_id.clone(), (snapshot, Instant::now()));
        self.evict(&mut entries);
    }

    /// Delete persisted snapshots that have outlived the store's TTL.
    fn evict_persisted(&self) {
        let Some(directory) = self.directory.as_ref() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for entry in entries.filter_map(|entry| entry.ok()) {
            let expired = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > self.ttl);
            if expired {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Drop expired snapshots, then the oldest ones if still over capacity.
    fn evict(&self, entries: &mut HashMap<String, (Snapshot, Instant)>) {
        let now = Instant::now();
        entries.retain(|_, (_, stored)| now.duration_since(*stored) < self.ttl);
        while entries.len() > self.capacity {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, stored))| *stored)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
    }

    pub fn get(&self, snapshot_id: &str) -> Option<Snapshot> {
        if let Ok(entries) = self.entries.lock() {
            if let Some((snapshot, stored)) = entries.get(snapshot_id) {
                if Instant::now().duration_since(*stored) < self.ttl {
                    return Some(snapshot.clone());
                }
            }
        }

        // Fall back to disk, so a target minted by an earlier process works.
        let path = self.path_for(snapshot_id)?;
        let age = std::fs::metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())?;
        if age > self.ttl {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SnapshotStore {
    fn default() -> Self {
        // A handle older than five minutes almost certainly refers to a UI
        // that has moved on, so expiring it produces a clearer failure than
        // letting the staleness check reject it later.
        Self::new(16, Duration::from_secs(300)).persisted(default_snapshot_directory())
    }
}

/// Where snapshots are cached so separate CLI invocations can share them.
pub fn default_snapshot_directory() -> std::path::PathBuf {
    std::env::var_os("AAD_SNAPSHOT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("ai-auto-desktop")
                .join("snapshots")
        })
}

/// Compute a bounding rectangle's centre, refusing empty rectangles.
pub fn click_point(bounds: Option<Bounds>) -> Result<(i32, i32)> {
    let bounds = bounds
        .ok_or_else(|| DriverError::invalid("the element has no bounding rectangle to click"))?;
    if bounds.is_empty() {
        return Err(DriverError::invalid(
            "the element's bounding rectangle has no area",
        ));
    }
    Ok(bounds.center())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::States;

    fn snapshot(id: &str, revision: u64) -> Snapshot {
        Snapshot {
            snapshot_id: id.to_string(),
            revision,
            window: WindowInfo {
                window_id: "w1".into(),
                title: "Test".into(),
                process_id: 1,
                process_name: None,
                class_name: None,
                bounds: None,
                is_foreground: true,
                is_minimized: false,
            },
            nodes: Vec::new(),
            root_id: None,
            captured_at: "2026-01-01T00:00:00.000Z".into(),
            truncated: false,
        }
    }

    #[test]
    fn revisions_increase_monotonically() {
        let store = SnapshotStore::new(8, Duration::from_secs(60));
        let first = store.next_revision();
        let second = store.next_revision();

        assert!(second > first);
    }

    #[test]
    fn a_stored_snapshot_can_be_retrieved() {
        let store = SnapshotStore::new(8, Duration::from_secs(60));
        store.insert(snapshot("s1", 1));

        assert_eq!(store.get("s1").map(|found| found.revision), Some(1));
        assert!(store.get("missing").is_none());
    }

    /// A private directory, so persistence tests never see each other's data.
    fn scratch(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("aad-store-test-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn a_snapshot_survives_into_another_store() {
        // This is what makes `aad find` followed by `aad do` work at all: the
        // two commands are separate processes.
        let directory = scratch("cross-process");
        let first = SnapshotStore::new(8, Duration::from_secs(60)).persisted(directory.clone());
        first.insert(snapshot("abc123", 7));
        drop(first);

        let second = SnapshotStore::new(8, Duration::from_secs(60)).persisted(directory.clone());
        let found = second
            .get("abc123")
            .expect("the snapshot is still readable");

        assert_eq!(found.revision, 7);
        assert_eq!(found.window.window_id, "w1");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn revisions_keep_climbing_past_what_another_process_wrote() {
        // Two processes must not mint the same revision for different trees.
        let directory = scratch("revisions");
        let first = SnapshotStore::new(8, Duration::from_secs(60)).persisted(directory.clone());
        first.insert(snapshot("aaa", 5));
        drop(first);

        let second = SnapshotStore::new(8, Duration::from_secs(60)).persisted(directory.clone());

        assert!(
            second.next_revision() > 5,
            "a fresh process must not reuse a revision already on disk"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_persisted_snapshot_expires_with_the_ttl() {
        let directory = scratch("expiry");
        let store = SnapshotStore::new(8, Duration::from_millis(20)).persisted(directory.clone());
        store.insert(snapshot("bbb", 1));
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            store.get("bbb").is_none(),
            "an expired handle must not resolve"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_snapshot_id_can_never_escape_its_directory() {
        // Ids arrive from the caller, so they must not reach the filesystem raw.
        let directory = scratch("traversal");
        let store = SnapshotStore::new(8, Duration::from_secs(60)).persisted(directory.clone());

        for hostile in [
            "../escape",
            "..\\escape",
            "a/b",
            "a\\b",
            "",
            "with space",
            "a:b",
        ] {
            assert!(
                store.path_for(hostile).is_none(),
                "{hostile:?} must be rejected"
            );
            assert!(store.get(hostile).is_none());
        }
        // A normal id is still accepted.
        assert!(store.path_for("abc123").is_some());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_store_evicts_beyond_its_capacity() {
        let store = SnapshotStore::new(2, Duration::from_secs(60));
        store.insert(snapshot("s1", 1));
        std::thread::sleep(Duration::from_millis(2));
        store.insert(snapshot("s2", 2));
        std::thread::sleep(Duration::from_millis(2));
        store.insert(snapshot("s3", 3));

        assert_eq!(store.len(), 2);
        assert!(
            store.get("s1").is_none(),
            "the oldest entry is evicted first"
        );
        assert!(store.get("s3").is_some());
    }

    #[test]
    fn an_expired_snapshot_is_not_returned() {
        let store = SnapshotStore::new(8, Duration::from_millis(20));
        store.insert(snapshot("s1", 1));
        std::thread::sleep(Duration::from_millis(40));

        assert!(store.get("s1").is_none());
    }

    #[test]
    fn capture_limits_are_clamped_to_safe_bounds() {
        let clamped = CaptureLimits {
            max_depth: 9_999,
            max_nodes: 999_999,
        }
        .clamp();
        assert_eq!(clamped.max_depth, CaptureLimits::MAX_DEPTH);
        assert_eq!(clamped.max_nodes, CaptureLimits::MAX_NODES);

        let raised = CaptureLimits {
            max_depth: 0,
            max_nodes: 0,
        }
        .clamp();
        assert_eq!(raised.max_depth, 1);
        assert_eq!(raised.max_nodes, 1);
    }

    #[test]
    fn bounded_hold_refuses_insufficient_budget_and_stops_on_changed_target() {
        assert!(ensure_hold_budget(0, Instant::now() + Duration::from_secs(1)).is_err());
        assert!(ensure_hold_budget(5001, Instant::now() + Duration::from_secs(10)).is_err());
        assert!(ensure_hold_budget(200, Instant::now() + Duration::from_millis(20)).is_err());
        let started = Instant::now();
        let mut checked = 0;
        let error = wait_input_hold(500, started + Duration::from_secs(1), || {
            checked += 1;
            Err(DriverError::stale("target changed"))
        })
        .unwrap_err();
        assert_eq!(checked, 1);
        assert_eq!(error.code, "DRIVER.STALE_HANDLE");
        assert!(started.elapsed() < Duration::from_millis(400));
        let error = wait_input_hold(100, Instant::now() + Duration::from_millis(10), || Ok(()))
            .unwrap_err();
        assert_eq!(error.code, "DRIVER.TIMEOUT");
    }

    #[test]
    fn a_click_point_is_the_centre_of_a_non_empty_rectangle() {
        let point = click_point(Some(Bounds {
            x: 100,
            y: 200,
            width: 40,
            height: 20,
        }))
        .unwrap();
        assert_eq!(point, (120, 210));
    }

    #[test]
    fn an_empty_or_missing_rectangle_cannot_be_clicked() {
        assert!(click_point(None).is_err());
        assert!(click_point(Some(Bounds {
            x: 0,
            y: 0,
            width: 0,
            height: 5
        }))
        .is_err());
    }

    #[test]
    fn a_driver_error_maps_onto_the_workflow_error_contract() {
        let error = DriverError::stale("the snapshot is out of date")
            .with_detail("snapshot_id", serde_json::json!("s1"))
            .into_automation_error();

        assert_eq!(error.code, "DRIVER.STALE_HANDLE");
        assert_eq!(error.category, "driver");
        // Refusing a stale handle happens before anything is touched.
        assert_eq!(error.effect, "not_applied");
        assert_eq!(error.details["snapshot_id"], serde_json::json!("s1"));
    }

    #[test]
    fn states_serialize_every_field_including_unknowns() {
        let value = States {
            enabled: Some(true),
            ..Default::default()
        }
        .to_json();

        assert_eq!(value["enabled"], serde_json::json!(true));
        assert_eq!(value["focused"], serde_json::Value::Null);
    }
}
