//! Ambient observation: a long-running, system-wide view of what is happening
//! on the desktop.
//!
//! This is deliberately separate from [`crate::capture`]. The recorder watches
//! one window for a fixed time, deliberately drops values, treats focus as
//! context only, and produces replayable steps. An ambient observer instead
//! runs for as long as asked across all windows, keeps values, reports
//! foreground switches as first-class events, and produces a chronological
//! activity log. The engineering lessons are shared (bounded buffer, drop
//! accounting, callbacks that never panic into COM); the event contract is not.
//!
//! The pipeline is "events wake, snapshots tell the truth": native events
//! ([`ObservedEvent`]) say *when and roughly where* to look again; the caller
//! re-captures the foreground window and [`diff_trees`] says what actually
//! changed. Events alone are not the content, because measurement showed each
//! native mechanism is blind where the other sees (see
//! `docs/research/ambient-observation-monitor.md` §8).

use crate::model::{Node, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What the observer noticed, at the granularity an ambient log needs.
///
/// Smaller than the raw platform vocabulary: Win32 and UI Automation each emit
/// dozens of notification ids, many of them repaint noise. Only the families
/// that answer "what is the user doing / what changed" survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// The foreground window changed to another top-level window.
    ForegroundChanged,
    /// Keyboard focus moved inside the (foreground) window.
    FocusChanged,
    /// A selectable thing became selected: a browser tab, a list row, …
    SelectionChanged,
    /// A button/menu/link was invoked.
    Invoked,
    /// An element's value (text, check state) changed.
    ValueChanged,
    /// The element tree gained or lost nodes.
    StructureChanged,
    /// Something changed that did not fit the families above. Kept rather than
    /// silently dropped, so a gap in coverage is diagnosable.
    Other,
}

impl ObservationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ObservationKind::ForegroundChanged => "foreground_changed",
            ObservationKind::FocusChanged => "focus_changed",
            ObservationKind::SelectionChanged => "selection_changed",
            ObservationKind::Invoked => "invoked",
            ObservationKind::ValueChanged => "value_changed",
            ObservationKind::StructureChanged => "structure_changed",
            ObservationKind::Other => "other",
        }
    }
}

/// Identity of the window an event concerns, resolved cheaply at callback time.
///
/// The event thread must not walk the UI tree (that is what snapshotting is
/// for), so this carries only what the native callback already knows: the
/// window handle as an opaque id plus pid/title. Resolution into a full
/// [`WindowInfo`] happens when the event is consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventWindow {
    /// The backend's window id (`hwnd:<isize>` on Windows). `None` when the
    /// event could not be attributed to a window.
    pub window_id: Option<String>,
    pub process_id: u32,
    pub process_name: Option<String>,
    pub title: Option<String>,
}

impl EventWindow {
    pub fn unknown() -> Self {
        Self {
            window_id: None,
            process_id: 0,
            process_name: None,
            title: None,
        }
    }
}

/// A minimal description of the element an event concerns.
///
/// Like [`EventWindow`], this is deliberately not a full [`Node`]: resolving
/// every property inside a callback is expensive and racy. The fields here are
/// the ones the native event can report without a tree walk; the authoritative
/// element state comes from the follow-up snapshot diff.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EventElement {
    pub role: Option<String>,
    pub name: Option<String>,
    pub automation_id: Option<String>,
    pub class_name: Option<String>,
}

/// One ambient observation, as a log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedEvent {
    /// Position in observed order, unique for the session. Gaps mean events
    /// were dropped under pressure (reported separately via the drop count).
    pub sequence: u64,
    pub kind: ObservationKind,
    /// Which native mechanism saw it (`winevent` / `uia`), for gap diagnosis.
    pub source: String,
    /// Window attribution. For UIA foreground-filtered events this is
    /// deliberately left as the process id only: the sender's native handle can
    /// be an internal renderer/popup ("Chrome Legacy Window"), and resolving
    /// the title against the foreground window at *poll* time is wrong too (the
    /// foreground may already have moved on). The consumer attributes the event
    /// to the foreground window of its batch, keyed on process id.
    pub window: EventWindow,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub element: Option<EventElement>,
    /// Monotonic session time; not serialised across processes. A value
    /// reconstructed on the consumer side is stamped with the current time —
    /// the consumer folds bursts over wall-clock gaps anyway.
    #[serde(skip, default = "Instant::now")]
    pub observed_at: Instant,
}

impl ObservedEvent {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::json!({}))
    }
}

/// Bounded, thread-safe queue between native callback threads and the reader.
///
/// Mirrors the guarantees of the recorder's buffer, because the failure mode
/// is identical and already measured here: an overflow that is not counted is
/// indistinguishable from the user doing nothing.
#[derive(Debug)]
pub struct ObservationBuffer {
    inner: Mutex<BufferState>,
    limit: usize,
}

#[derive(Debug, Default)]
struct BufferState {
    events: VecDeque<ObservedEvent>,
    dropped: u64,
    next_sequence: AtomicU64,
}

impl ObservationBuffer {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BufferState::default()),
            limit: limit.max(1),
        })
    }

    /// Record one event from a callback thread.
    ///
    /// A poisoned lock is recovered, not propagated: panicking across a COM or
    /// hook callback can tear down the whole subscription.
    pub fn push(
        &self,
        kind: ObservationKind,
        source: impl Into<String>,
        window: EventWindow,
        element: Option<EventElement>,
    ) {
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let sequence = state.next_sequence.fetch_add(1, Ordering::SeqCst);
        state.events.push_back(ObservedEvent {
            sequence,
            kind,
            source: source.into(),
            window,
            element,
            observed_at: Instant::now(),
        });
        while state.events.len() > self.limit {
            state.events.pop_front();
            state.dropped += 1;
        }
    }

    /// Take up to `max` events, oldest first, plus drops since the last drain.
    pub fn drain(&self, max: usize) -> (Vec<ObservedEvent>, u64) {
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let take = max.min(state.events.len());
        let events = state.events.drain(..take).collect();
        let dropped = state.dropped;
        state.dropped = 0;
        (events, dropped)
    }

    pub fn pending(&self) -> usize {
        match self.inner.lock() {
            Ok(state) => state.events.len(),
            Err(poisoned) => poisoned.into_inner().events.len(),
        }
    }
}

/// Collapse a burst of events for the same window into a smaller set of
/// "reasons to re-snapshot".
///
/// The observer does not try to faithfully replay every notification: a click
/// can raise several, typing raises one value event per keystroke, and a
/// foreground switch drags a trail of focus/structure noise behind it. What
/// the consumer needs is a short list of *(window, kind)* triggers, each with
/// the time of its most recent occurrence. Order is preserved by first
/// occurrence.
///
/// `quiet` defines how long the same *(window, kind)* must be silent before it
/// counts as a new trigger: back-to-back value events while typing collapse
/// into one; a second edit seconds later is a new trigger.
pub fn triggers(events: &[ObservedEvent], quiet: std::time::Duration) -> Vec<Trigger> {
    // Indexed by (window_id, kind) so the same signal from two windows of one
    // process (two editor windows, two browser windows) stays distinct.
    let mut by_key: HashMap<(Option<String>, ObservationKind), usize> = HashMap::new();
    let mut out: Vec<Trigger> = Vec::new();

    for event in events {
        let key = (event.window.window_id.clone(), event.kind);
        let now = event.observed_at;
        if let Some(&index) = by_key.get(&key) {
            let trigger = &mut out[index];
            if now.duration_since(trigger.last_at) <= quiet {
                trigger.count += 1;
                trigger.last_at = now;
            } else {
                // The quiet window elapsed: this is a fresh trigger.
                by_key.insert(key, out.len());
                out.push(Trigger::from_event(event));
            }
        } else {
            by_key.insert(key, out.len());
            out.push(Trigger::from_event(event));
        }
    }
    out
}

/// A reason to re-capture a window.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub kind: ObservationKind,
    pub window: EventWindow,
    /// How many raw events were folded into this trigger.
    pub count: u64,
    pub source: String,
    pub last_at: Instant,
}

impl Trigger {
    fn from_event(event: &ObservedEvent) -> Self {
        Self {
            kind: event.kind,
            window: event.window.clone(),
            count: 1,
            source: event.source.clone(),
            last_at: event.observed_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Incremental tree diff
// ---------------------------------------------------------------------------

/// One node's cross-snapshot identity.
///
/// `node_id` is only stable within one snapshot. To follow the same element
/// across two captures we build an identity from content instead, in priority
/// order (see `docs/research/ambient-observation-monitor.md` §4.3):
///
/// 1. a non-empty automation id (measured 100% cross-session stable);
/// 2. role + class + ancestor chain + ordinal among identical siblings;
/// 3. role + parent + name for leaf/text nodes.
///
/// `value` is deliberately NOT part of identity, so typing in a field is a
/// change to one node rather than "old removed + new added".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeIdentity {
    pub key: String,
}

impl NodeIdentity {
    /// Build the identity for `node` within its snapshot.
    pub fn of(node: &Node, nodes: &[Node]) -> Self {
        let parent_key = node
            .parent_id
            .as_deref()
            .and_then(|pid| nodes.iter().find(|n| n.node_id == pid))
            .map(|parent| Self::of(parent, nodes).key)
            .unwrap_or_default();

        if let Some(id) = non_empty(&node.automation_id) {
            return Self {
                key: format!("aid:{}:{id}", role_key(node)),
            };
        }

        // Ordinal among siblings that share role+class, so repeated identical
        // controls (rows, toolbar buttons) keep a stable position when their
        // neighbours do not change.
        let ordinal = node
            .parent_id
            .as_deref()
            .and_then(|pid| nodes.iter().find(|n| n.node_id == pid))
            .map(|parent| {
                parent
                    .children
                    .iter()
                    .filter_map(|cid| nodes.iter().find(|n| &n.node_id == cid))
                    .filter(|sibling| {
                        sibling.role == node.role && sibling.class_name == node.class_name
                    })
                    .position(|sibling| sibling.node_id == node.node_id)
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        let class = non_empty(&node.class_name).unwrap_or("");
        let name = non_empty(&node.name).unwrap_or("");
        Self {
            key: format!(
                "{}|p={parent_key}|c={class}|n={name}#{ordinal}",
                role_key(node)
            ),
        }
    }
}

fn role_key(node: &Node) -> String {
    let framework = non_empty(&node.framework_id).unwrap_or("");
    format!("{}:{}", framework, node.role)
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.is_empty())
}

/// What changed for one matched node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeChange {
    pub identity: String,
    pub role: String,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub value: Vec<FieldChange<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<FieldChange<bool>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub name_changes: Vec<FieldChange<String>>,
}

/// One field's before/after values.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldChange<T: Serialize> {
    pub field: String,
    pub from: Option<T>,
    pub to: Option<T>,
}

/// The incremental difference between two snapshots of the same window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ChangeSet {
    /// Identities present now but not in the baseline.
    pub added: Vec<AddedNode>,
    /// Identities in the baseline but not present now.
    pub removed: Vec<RemovedNode>,
    /// Identities in both whose value/states/name changed.
    pub changed: Vec<NodeChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddedNode {
    pub identity: String,
    pub role: String,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemovedNode {
    pub identity: String,
    pub role: String,
    pub name: Option<String>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        (self.added.len(), self.removed.len(), self.changed.len())
    }
}

/// How an ambient log presents a [`ChangeSet`].
///
/// Raw diffs are faithful but noisy: an Electron workbench built from
/// anonymous panes reports hundreds of add/remove pairs for one click, none
/// carrying the text or labels a reader cares about. The digest keeps what
/// reconstructs *what was read, typed or activated* and counts the rest. It
/// never invents content; it only folds presentation noise.
#[derive(Clone, Debug)]
pub struct DigestOptions {
    /// Per-category cap on individual items; overflow is folded away.
    pub max_items: usize,
    /// Report focus/enabled/read_only transitions on named controls.
    pub include_states: bool,
}

impl Default for DigestOptions {
    fn default() -> Self {
        Self {
            max_items: 40,
            include_states: true,
        }
    }
}

/// One text-bearing thing that appeared or disappeared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContentItem {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Stable fingerprint of the content, used to cancel an item that appears
    /// and disappears within the same batch (a menu opened and dismissed).
    #[serde(skip)]
    pub identity: String,
}

fn item_fingerprint(role: &str, name: &Option<String>, text: &Option<String>) -> String {
    // A unit-separator-free composite key built without an escape-prone
    // literal: role, name and text joined by a character that UI text never
    // contains (the ASCII unit separator).
    let sep = char::from(31);
    format!(
        "{role}{sep}{}{sep}{}",
        name.as_deref().unwrap_or(""),
        text.as_deref().unwrap_or("")
    )
}

impl ContentItem {
    fn has_content(&self) -> bool {
        self.name.as_deref().is_some_and(|s| !s.is_empty())
            || self.text.as_deref().is_some_and(|s| !s.is_empty())
    }
}

/// A field value change -- the ambient signal for typing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextChange {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// Content-first, filtered view of one window's changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ChangeDigest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub appeared: Vec<ContentItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disappeared: Vec<ContentItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub text_changes: Vec<TextChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub state_changes: Vec<serde_json::Value>,
    /// Anonymous structural add/remove pairs folded away as scaffolding.
    pub structural_noise: usize,
    /// Whether the per-category caps truncated the detail lists.
    pub truncated: bool,
}

impl ChangeDigest {
    pub fn is_empty(&self) -> bool {
        self.appeared.is_empty()
            && self.disappeared.is_empty()
            && self.text_changes.is_empty()
            && self.state_changes.is_empty()
    }
}

/// Reduce a raw [`ChangeSet`] to content worth logging.
///
/// Rules, each driven by a measured failure on VS Code / Edge:
/// - added/removed nodes with neither name nor value are scaffolding, counted
///   not printed;
/// - value/name changes between empty strings (anonymous pane flicker) are not
///   text changes;
/// - state transitions on unnamed elements are dropped;
/// - only focused/enabled/read_only states survive -- offscreen churn and the
///   protected marker are presentation, not activity.
pub fn digest(changes: &ChangeSet, options: &DigestOptions) -> ChangeDigest {
    let cap = options.max_items.max(1);
    let mut structural = 0usize;

    let mut appeared: Vec<ContentItem> = Vec::new();
    for node in &changes.added {
        let item = ContentItem {
            identity: item_fingerprint(&node.role, &node.name, &node.value),
            role: node.role.clone(),
            name: node.name.clone(),
            text: node.value.clone(),
        };
        if item.has_content() {
            appeared.push(item);
        } else {
            structural += 1;
        }
    }

    let mut disappeared: Vec<ContentItem> = Vec::new();
    for node in &changes.removed {
        let item = ContentItem {
            identity: item_fingerprint(&node.role, &node.name, &None),
            role: node.role.clone(),
            name: node.name.clone(),
            text: None,
        };
        if item.has_content() {
            disappeared.push(item);
        } else {
            structural += 1;
        }
    }

    let mut text_changes: Vec<TextChange> = Vec::new();
    let mut state_changes: Vec<serde_json::Value> = Vec::new();
    for node in &changes.changed {
        let named = node.name.clone().filter(|s| !s.is_empty());

        for field in node.value.iter().chain(node.name_changes.iter()) {
            let from = field.from.clone().filter(|s| !s.is_empty());
            let to = field.to.clone().filter(|s| !s.is_empty());
            if from.is_none() && to.is_none() {
                continue;
            }
            text_changes.push(TextChange {
                role: node.role.clone(),
                name: named.clone(),
                from,
                to,
            });
        }

        if options.include_states {
            for state in &node.states {
                if !matches!(state.field.as_str(), "focused" | "enabled" | "read_only") {
                    continue;
                }
                if named.is_none() {
                    continue;
                }
                state_changes.push(serde_json::json!({
                    "role": node.role,
                    "name": named,
                    "field": state.field,
                    "from": state.from,
                    "to": state.to,
                }));
            }
        }
    }

    // Cancel content that both appeared and disappeared within this batch:
    // measured on a right-click dismissed 800ms later, the same menu items
    // landed in both lists and were printed twice. What survives the batch is
    // what actually changed.
    let gone: std::collections::HashSet<&str> = disappeared
        .iter()
        .map(|item| item.identity.as_str())
        .collect();
    let appeared: Vec<ContentItem> = appeared
        .into_iter()
        .filter(|item| !gone.contains(item.identity.as_str()))
        .collect();
    let appeared_now: std::collections::HashSet<&str> =
        appeared.iter().map(|item| item.identity.as_str()).collect();
    let disappeared: Vec<ContentItem> = disappeared
        .into_iter()
        .filter(|item| !appeared_now.contains(item.identity.as_str()))
        .collect();

    let mut appeared = appeared;
    let mut disappeared = disappeared;
    appeared.sort_by(|a, b| a.identity.cmp(&b.identity));
    disappeared.sort_by(|a, b| a.identity.cmp(&b.identity));
    text_changes.sort_by(|a, b| a.name.cmp(&b.name));

    let truncated = text_changes.len() > cap
        || appeared.len() > cap
        || disappeared.len() > cap
        || state_changes.len() > cap;
    ChangeDigest {
        text_changes: text_changes[..cap.min(text_changes.len())].to_vec(),
        appeared: appeared[..cap.min(appeared.len())].to_vec(),
        disappeared: disappeared[..cap.min(disappeared.len())].to_vec(),
        state_changes: state_changes[..cap.min(state_changes.len())].to_vec(),
        structural_noise: structural,
        truncated,
    }
}

/// Diff `current` against the `baseline` snapshot.
///
/// Unmatched nodes are reported honestly as added/removed; this never invents
/// a "moved" relationship it cannot prove. Pure and allocation-light enough to
/// run on every triggered re-capture.
pub fn diff_trees(baseline: &Snapshot, current: &Snapshot) -> ChangeSet {
    let baseline_index = index_nodes(&baseline.nodes);
    let current_index = index_nodes(&current.nodes);

    let mut changes = ChangeSet::default();

    for (identity, node) in &current_index {
        match baseline_index.get(identity) {
            None => changes.added.push(AddedNode {
                identity: identity.key.clone(),
                role: node.role.clone(),
                name: node.name.clone(),
                value: non_empty(&node.value).map(str::to_string),
            }),
            Some(old) => {
                if let Some(change) = diff_node(identity, old, node) {
                    changes.changed.push(change);
                }
            }
        }
    }

    for (identity, node) in &baseline_index {
        if !current_index.contains_key(identity) {
            changes.removed.push(RemovedNode {
                identity: identity.key.clone(),
                role: node.role.clone(),
                name: node.name.clone(),
            });
        }
    }

    // Stable order makes output diffable across runs.
    changes.added.sort_by(|a, b| a.identity.cmp(&b.identity));
    changes.removed.sort_by(|a, b| a.identity.cmp(&b.identity));
    changes.changed.sort_by(|a, b| a.identity.cmp(&b.identity));
    changes
}

fn index_nodes(nodes: &[Node]) -> HashMap<NodeIdentity, &Node> {
    // When two nodes compute the same identity (ordinal-based keys on a
    // reordered list), keep the first rather than panicking: the second is
    // reported as an add/remove pair, which is the honest outcome.
    let mut map = HashMap::with_capacity(nodes.len());
    for node in nodes {
        map.entry(NodeIdentity::of(node, nodes)).or_insert(node);
    }
    map
}

fn diff_node(identity: &NodeIdentity, old: &Node, now: &Node) -> Option<NodeChange> {
    let mut value = Vec::new();
    if old.value != now.value {
        value.push(FieldChange {
            field: "value".into(),
            from: old.value.clone(),
            to: now.value.clone(),
        });
    }

    let mut name_changes = Vec::new();
    if old.name != now.name {
        name_changes.push(FieldChange {
            field: "name".into(),
            from: old.name.clone(),
            to: now.name.clone(),
        });
    }

    let mut states = Vec::new();
    diff_state(
        &mut states,
        "enabled",
        old.states.enabled,
        now.states.enabled,
    );
    diff_state(
        &mut states,
        "focused",
        old.states.focused,
        now.states.focused,
    );
    diff_state(
        &mut states,
        "focusable",
        old.states.focusable,
        now.states.focusable,
    );
    diff_state(
        &mut states,
        "offscreen",
        old.states.offscreen,
        now.states.offscreen,
    );
    diff_state(
        &mut states,
        "read_only",
        old.states.read_only,
        now.states.read_only,
    );
    diff_state(
        &mut states,
        "protected",
        old.states.protected,
        now.states.protected,
    );

    if value.is_empty() && states.is_empty() && name_changes.is_empty() {
        return None;
    }
    Some(NodeChange {
        identity: identity.key.clone(),
        role: now.role.clone(),
        name: now.name.clone(),
        value,
        states,
        name_changes,
    })
}

fn diff_state(out: &mut Vec<FieldChange<bool>>, field: &str, old: Option<bool>, now: Option<bool>) {
    if old != now {
        out.push(FieldChange {
            field: field.to_string(),
            from: old,
            to: now,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bounds, States, WindowInfo};
    use std::time::Duration;

    fn window(title: &str) -> WindowInfo {
        WindowInfo {
            window_id: "hwnd:1".into(),
            title: title.into(),
            process_id: 1,
            process_name: Some("app.exe".into()),
            class_name: Some("App".into()),
            bounds: None,
            is_foreground: true,
            is_minimized: false,
        }
    }

    fn snapshot(nodes: Vec<Node>) -> Snapshot {
        Snapshot {
            snapshot_id: "s".into(),
            revision: 1,
            window: window("App"),
            nodes,
            root_id: None,
            captured_at: "2026-01-01T00:00:00.000Z".into(),
            truncated: false,
        }
    }

    fn node(id: &str, role: &str, parent: Option<&str>) -> Node {
        Node {
            node_id: id.into(),
            role: role.into(),
            name: None,
            value: None,
            automation_id: None,
            class_name: None,
            framework_id: Some("Win32".into()),
            bounds: Some(Bounds {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }),
            states: States::default(),
            actions: Vec::new(),
            depth: 0,
            parent_id: parent.map(str::to_string),
            children: Vec::new(),
        }
    }

    fn event(kind: ObservationKind, window: Option<&str>, age_ms: u64) -> ObservedEvent {
        ObservedEvent {
            sequence: 0,
            kind,
            source: "test".to_string(),
            window: EventWindow {
                window_id: window.map(str::to_string),
                process_id: 1,
                process_name: None,
                title: None,
            },
            element: None,
            observed_at: Instant::now() + std::time::Duration::from_millis(age_ms),
        }
    }

    #[test]
    fn buffer_keeps_sequence_and_counts_drops() {
        let buffer = ObservationBuffer::new(2);
        for _ in 0..3 {
            buffer.push(
                ObservationKind::FocusChanged,
                "test".to_string(),
                EventWindow::unknown(),
                None,
            );
        }
        let (events, dropped) = buffer.drain(10);
        assert_eq!(events.len(), 2);
        assert_eq!(dropped, 1);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].sequence, 2);
    }

    #[test]
    fn triggers_fold_a_burst_but_split_after_quiet() {
        let g1 = event(ObservationKind::ValueChanged, Some("w"), 0);
        let g2 = event(ObservationKind::ValueChanged, Some("w"), 10);
        let later = event(ObservationKind::ValueChanged, Some("w"), 1000);
        let other_window = event(ObservationKind::ValueChanged, Some("w2"), 20);
        let folded = triggers(&[g1, g2, other_window, later], Duration::from_millis(200));
        // w burst (2), w2 (1), w after quiet (1).
        assert_eq!(folded.len(), 3);
        assert_eq!(folded[0].count, 2);
        assert_eq!(folded[1].window.window_id.as_deref(), Some("w2"));
        assert_eq!(folded[2].count, 1);
    }

    #[test]
    fn foreground_events_never_fold_with_window_events() {
        let events = vec![
            event(ObservationKind::ForegroundChanged, Some("a"), 0),
            event(ObservationKind::ForegroundChanged, Some("b"), 5),
            event(ObservationKind::FocusChanged, Some("b"), 6),
        ];
        let folded = triggers(&events, Duration::from_secs(10));
        assert_eq!(folded.len(), 3);
    }

    #[test]
    fn diff_detects_add_remove_and_value_change_without_identity_churn() {
        let mut edit = node("edit", "edit", None);
        edit.automation_id = Some("search".into());
        edit.value = Some("hello".into());

        let mut button = node("btn", "button", None);
        button.automation_id = Some("ok".into());

        let baseline = snapshot(vec![edit.clone(), button.clone()]);

        let mut edit2 = edit.clone();
        edit2.value = Some("hello world".into());
        let mut extra = node("list", "list", None);
        extra.automation_id = Some("results".into());
        let current = snapshot(vec![edit2, extra]);

        let changes = diff_trees(&baseline, &current);
        let (added, removed, changed) = changes.counts();
        assert_eq!(added, 1, "the list is new: {changes:?}");
        assert_eq!(removed, 1, "the button is gone");
        assert_eq!(changed, 1, "typing is a value change, not add/remove");
        assert_eq!(
            changes.changed[0].value[0].to.as_deref(),
            Some("hello world")
        );
        assert_eq!(changes.added[0].identity, "aid:Win32:list:results");
    }

    #[test]
    fn identical_trees_produce_an_empty_changeset() {
        let mut a = node("a", "button", None);
        a.automation_id = Some("x".into());
        let baseline = snapshot(vec![a.clone()]);
        let current = snapshot(vec![a]);
        assert!(diff_trees(&baseline, &current).is_empty());
    }

    #[test]
    fn digest_folds_anonymous_scaffolding_but_keeps_text_and_labels() {
        // One click in an Electron workbench: hundreds of anonymous panes plus
        // a real menu of named items and one typed value.
        let baseline = snapshot(vec![]);
        let mut current = snapshot(vec![]);
        let mut nodes: Vec<Node> = Vec::new();
        // Distinct ancestry gives each pane a distinct identity, as in a real
        // tree; identical-identity duplicates would (correctly) be deduplicated
        // by the index before diffing.
        let mut parent: Option<String> = None;
        for n in 0..120 {
            let id = format!("p{n}");
            let mut pane = node(&id, "pane", parent.as_deref());
            pane.class_name = Some("".into());
            nodes.push(pane);
            parent = Some(id);
        }
        let mut menu = node("m1", "menu_item", None);
        menu.name = Some("为此页面创建 QR 代码".into());
        nodes.push(menu);
        let mut edit = node("e1", "edit", None);
        edit.automation_id = Some("addr".into());
        edit.name = Some("地址和搜索栏".into());
        edit.value = Some("example.com".into());
        nodes.push(edit);
        current.nodes = nodes;

        let changes = diff_trees(&baseline, &current);
        let digest = digest(
            &changes,
            &DigestOptions {
                max_items: 40,
                include_states: true,
            },
        );

        // Named menu and the typed value survive...
        assert!(
            digest
                .appeared
                .iter()
                .any(|item| item.name.as_deref() == Some("为此页面创建 QR 代码")),
            "named controls must survive filtering"
        );
        assert!(
            digest
                .appeared
                .iter()
                .any(|item| item.text.as_deref() == Some("example.com")),
            "text values must survive filtering"
        );
        // ...while the 120 anonymous panes are folded into one number.
        assert!(
            digest.structural_noise >= 120,
            "noise = {}",
            digest.structural_noise
        );
        assert!(!digest
            .appeared
            .iter()
            .any(|item| item.role == "pane" && item.name.is_none()));
    }

    #[test]
    fn digest_drops_empty_value_repaint_and_unnamed_state_churn() {
        let mut a = node("a", "edit", None);
        a.automation_id = Some("f".into());
        a.name = Some("搜索".into());
        a.value = Some("q".into());
        let mut anon = node("b", "pane", None);
        anon.states.offscreen = Some(false);

        let baseline = snapshot(vec![a.clone(), anon.clone()]);

        let mut a2 = a.clone();
        a2.value = Some("query".into()); // real typing
        let mut anon2 = anon.clone();
        anon2.states.offscreen = Some(true); // offscreen is not reported
        let current = snapshot(vec![a2, anon2]);

        let changes = diff_trees(&baseline, &current);
        let digest = digest(&changes, &DigestOptions::default());

        assert_eq!(digest.text_changes.len(), 1);
        assert_eq!(digest.text_changes[0].to.as_deref(), Some("query"));
        assert!(
            digest.state_changes.is_empty(),
            "offscreen churn is dropped"
        );
    }

    #[test]
    fn digest_cancels_content_open_and_closed_in_one_batch() {
        // A context menu that appears and is dismissed before the next snapshot
        // must not show up as both appeared and disappeared.
        let baseline = snapshot(vec![]);
        let menu = |id: &str| {
            let mut n = node(id, "menu_item", None);
            n.name = Some("复制(C) Ctrl+C".into());
            n
        };
        // Mid-batch: menu visible. End-batch: gone. Simulate by diffing against
        // a tree that never contained it (so it appears in added of a transient
        // snapshot); the pairing contract is exercised directly here.
        let transient = snapshot(vec![menu("m")]);
        let added_changes = diff_trees(&baseline, &transient);
        let digest_open = digest(&added_changes, &DigestOptions::default());
        assert_eq!(digest_open.appeared.len(), 1);
        // Feed the closing batch: same content disappeared, no added.
        let removed: Vec<RemovedNode> = added_changes
            .added
            .iter()
            .map(|n| RemovedNode {
                identity: n.identity.clone(),
                role: n.role.clone(),
                name: n.name.clone(),
            })
            .collect();
        let closed_changes = ChangeSet {
            added: vec![],
            removed,
            changed: vec![],
        };
        let digest_closed = digest(&closed_changes, &DigestOptions::default());
        assert_eq!(digest_closed.disappeared.len(), 1);
        // The CLI's within-batch pairing relies on identical fingerprints on
        // both sides: assert that contract directly.
        assert_eq!(
            digest_open.appeared[0].identity,
            digest_closed.disappeared[0].identity
        );
    }

    #[test]
    fn digest_caps_each_category_and_marks_truncation() {
        let baseline = snapshot(vec![]);
        let mut nodes = Vec::new();
        for n in 0..10 {
            let mut b = node(&format!("b{n}"), "button", None);
            b.name = Some(format!("按钮{n}"));
            nodes.push(b);
        }
        let mut current = snapshot(vec![]);
        current.nodes = nodes;
        let changes = diff_trees(&baseline, &current);
        let digest = digest(
            &changes,
            &DigestOptions {
                max_items: 3,
                include_states: true,
            },
        );
        assert_eq!(digest.appeared.len(), 3);
        assert!(digest.truncated);
    }

    #[test]
    fn identity_survives_node_id_renumbering() {
        // Same content, different snapshot-local node ids: the element must
        // still match.
        let mut first = node("n1", "button", None);
        first.automation_id = Some("save".into());
        let mut second = node("zz-different-id", "button", None);
        second.automation_id = Some("save".into());
        assert_eq!(
            NodeIdentity::of(&first, &[first.clone()]),
            NodeIdentity::of(&second, &[second.clone()])
        );
    }
}
