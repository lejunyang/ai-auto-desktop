//! The platform-independent UI model.
//!
//! Everything the driver exposes to callers is expressed with these types.
//! Keeping them free of Windows specifics means the selector logic, the
//! snapshot handle discipline and the JSON shaping are all testable on any
//! platform, and a future macOS or Linux backend can reuse them unchanged.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use sha2::{Digest, Sha256};

/// A rectangle in virtual-desktop coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }

    /// A zero-area rectangle cannot be clicked or read meaningfully.
    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn to_json(self) -> Value {
        json!({"x": self.x, "y": self.y, "width": self.width, "height": self.height})
    }
}

/// The interaction states an element can report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct States {
    pub enabled: Option<bool>,
    pub offscreen: Option<bool>,
    pub focusable: Option<bool>,
    pub focused: Option<bool>,
    pub read_only: Option<bool>,
    /// The element masks its content, as a password field does.
    ///
    /// This is a label, not a filter. The platform already withholds the value
    /// of such an element -- measured: an `ES_PASSWORD` edit reports
    /// `value: null` through UIA while an ordinary edit next to it returns its
    /// text. Recording it explicitly is what turns that null from something
    /// that looks like a driver bug into a stated fact, and it tells a caller
    /// that filling this field in is legitimate but reading it back is not
    /// possible.
    pub protected: Option<bool>,
}

impl States {
    pub fn to_json(self) -> Value {
        let mut map = Map::new();
        let entries = [
            ("enabled", self.enabled),
            ("offscreen", self.offscreen),
            ("focusable", self.focusable),
            ("focused", self.focused),
            ("read_only", self.read_only),
            ("protected", self.protected),
        ];
        for (name, value) in entries {
            map.insert(
                name.to_string(),
                value.map(Value::Bool).unwrap_or(Value::Null),
            );
        }
        Value::Object(map)
    }
}

/// The actions that operate on a single element.
pub const NODE_ACTIONS: &[&str] = &["focus", "invoke", "set_value", "type_text", "pointer_click"];

/// One element in a UI snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Stable within one snapshot revision; never reuse across revisions.
    pub node_id: String,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub automation_id: Option<String>,
    pub class_name: Option<String>,
    pub framework_id: Option<String>,
    pub bounds: Option<Bounds>,
    pub states: States,
    /// Which node actions this element supports, given its control patterns.
    pub actions: Vec<String>,
    pub depth: u32,
    pub parent_id: Option<String>,
    pub children: Vec<String>,
}

impl Node {
    pub fn to_json(&self) -> Value {
        json!({
            "node_id": self.node_id,
            "role": self.role,
            "name": self.name,
            "value": self.value,
            "automation_id": self.automation_id,
            "class_name": self.class_name,
            "framework_id": self.framework_id,
            "bounds": self.bounds.map(Bounds::to_json).unwrap_or(Value::Null),
            "states": self.states.to_json(),
            "actions": self.actions,
            "depth": self.depth,
            "parent_id": self.parent_id,
            "children": self.children,
        })
    }

    /// A one-line description for an agent choosing between candidates.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("role={}", self.role)];
        if let Some(name) = self.name.as_ref().filter(|text| !text.is_empty()) {
            parts.push(format!("name={name:?}"));
        }
        if let Some(id) = self.automation_id.as_ref().filter(|text| !text.is_empty()) {
            parts.push(format!("automation_id={id:?}"));
        }
        if let Some(value) = self.value.as_ref().filter(|text| !text.is_empty()) {
            let clipped: String = value.chars().take(40).collect();
            parts.push(format!("value={clipped:?}"));
        }
        // Says why there is no value to show, and that writing to this field is
        // still the normal way to fill it in.
        if self.states.protected == Some(true) {
            parts.push("protected".to_string());
        }
        if self.states.enabled == Some(false) {
            parts.push("disabled".to_string());
        }
        parts.join(" ")
    }
}

/// A top-level window belonging to some running application.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: String,
    pub title: String,
    pub process_id: u32,
    pub process_name: Option<String>,
    pub class_name: Option<String>,
    pub bounds: Option<Bounds>,
    pub is_foreground: bool,
    pub is_minimized: bool,
}

impl WindowInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "window_id": self.window_id,
            "title": self.title,
            "process_id": self.process_id,
            "process_name": self.process_name,
            "class_name": self.class_name,
            "bounds": self.bounds.map(Bounds::to_json).unwrap_or(Value::Null),
            "is_foreground": self.is_foreground,
            "is_minimized": self.is_minimized,
        })
    }
}

/// An immutable capture of one window's element tree.
///
/// A snapshot is the unit of addressing: an agent takes a snapshot, chooses a
/// node from it, and acts on that node by quoting the snapshot id, revision
/// and node id.  The driver refuses the action if the tree has moved on, which
/// is what stops an agent from clicking whatever happens to be under a stale
/// coordinate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub revision: u64,
    pub window: WindowInfo,
    pub nodes: Vec<Node>,
    pub root_id: Option<String>,
    pub captured_at: String,
    pub truncated: bool,
}

impl Snapshot {
    pub fn find(&self, node_id: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.node_id == node_id)
    }

    /// A digest of the structure, used to detect that the tree changed.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        for node in &self.nodes {
            hasher.update(node.node_id.as_bytes());
            hasher.update(node.role.as_bytes());
            hasher.update(node.name.as_deref().unwrap_or("").as_bytes());
            hasher.update(node.automation_id.as_deref().unwrap_or("").as_bytes());
        }
        format!("sha256:{:x}", hasher.finalize())
    }

    pub fn to_json(&self) -> Value {
        json!({
            "snapshot_id": self.snapshot_id,
            "revision": self.revision,
            "window": self.window.to_json(),
            "root_id": self.root_id,
            "captured_at": self.captured_at,
            "truncated": self.truncated,
            "node_count": self.nodes.len(),
            "digest": self.digest(),
            "nodes": self.nodes.iter().map(Node::to_json).collect::<Vec<_>>(),
        })
    }

    /// A compact outline for an agent, without the full node payloads.
    ///
    /// Elements that cannot be perceived or acted upon are omitted, because an
    /// agent choosing from this list should only see real options.
    pub fn outline(&self, limit: usize) -> Value {
        let interesting: Vec<Value> = self
            .nodes
            .iter()
            .filter(|node| {
                !node.actions.is_empty()
                    || node.name.as_ref().is_some_and(|text| !text.is_empty())
            })
            .filter(|node| node.states.offscreen != Some(true))
            .take(limit)
            .map(|node| {
                json!({
                    "node_id": node.node_id,
                    // The reference an agent needs to act on this element,
                    // so the outline is directly actionable.
                    "ref": format!("{}:{}:{}", self.snapshot_id, self.revision, node.node_id),
                    // A description that outlives this snapshot. The `ref`
                    // above stops resolving once the snapshot is gone, so
                    // anything that gets saved has to be described instead;
                    // null here means the element cannot be told apart from
                    // its siblings and so cannot be recorded.
                    "locator": Locator::synthesize(node, &self.nodes)
                        .map(|locator| locator.to_json())
                        .unwrap_or(Value::Null),
                    "depth": node.depth,
                    "summary": node.summary(),
                    "actions": node.actions,
                    // A structured flag as well as the word in `summary`, so a
                    // caller does not have to parse prose to find out that
                    // this field's content is unreadable by design.
                    "protected": node.states.protected == Some(true),
                })
            })
            .collect();

        json!({
            "snapshot_id": self.snapshot_id,
            "revision": self.revision,
            "window": self.window.to_json(),
            "node_count": self.nodes.len(),
            "shown": interesting.len(),
            "truncated": self.truncated || interesting.len() < self.nodes.len(),
            "elements": interesting,
        })
    }
}

/// A reference to one node inside a specific snapshot revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub snapshot_id: String,
    pub revision: u64,
    pub node_id: String,
}

impl Target {
    /// Parse a target from action arguments.
    ///
    /// Accepts the structured object and the compact reference string, because
    /// quoting JSON on a command line is unreliable — PowerShell in particular
    /// strips the inner quotes, so a JSON-only interface would be unusable from
    /// the shell most Windows users are in.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        if let Some(text) = value.as_str() {
            return Self::parse_ref(text);
        }
        let object = value.as_object().ok_or("target must be an object or a reference string")?;
        let snapshot_id = object
            .get("snapshot_id")
            .and_then(Value::as_str)
            .ok_or("target.snapshot_id is required")?;
        let revision = object
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or("target.revision is required")?;
        let node_id = object
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or("target.node_id is required")?;
        Ok(Self {
            snapshot_id: snapshot_id.to_string(),
            revision,
            node_id: node_id.to_string(),
        })
    }

    /// Parse the compact `snapshot:revision:node` form.
    pub fn parse_ref(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.trim().split(':').collect();
        if parts.len() != 3 {
            return Err(format!(
                "{text:?} is not a target reference; expected snapshot:revision:node"
            ));
        }
        let revision = parts[1]
            .parse::<u64>()
            .map_err(|_| format!("{:?} is not a revision number", parts[1]))?;
        if parts[0].is_empty() || parts[2].is_empty() {
            return Err("a target reference needs a snapshot and a node".to_string());
        }
        Ok(Self {
            snapshot_id: parts[0].to_string(),
            revision,
            node_id: parts[2].to_string(),
        })
    }

    /// The compact form, safe to paste into any shell without quoting.
    pub fn to_ref(&self) -> String {
        format!("{}:{}:{}", self.snapshot_id, self.revision, self.node_id)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "snapshot_id": self.snapshot_id,
            "revision": self.revision,
            "node_id": self.node_id,
        })
    }
}

/// How deeply one locator may anchor to another.
///
/// Two levels covers "the button next to the field next to Username"; beyond
/// that the description is harder to follow than a plain attribute, and each
/// level multiplies the search.
const MAX_ANCHOR_DEPTH: usize = 3;

/// How far up a parent chain to walk before giving up.
///
/// Only a guard against a malformed tree whose `parent_id`s form a cycle, which
/// would otherwise hang the search. Real hierarchies are far shallower: the
/// deepest node measured on this machine sat at depth 21, in a browser.
const MAX_ANCESTRY_DEPTH: usize = 64;

/// How many same-role siblings an ordinal is still worth using with.
///
/// Beyond this, counting stops being a description a person can verify: nobody
/// checks that a link is the sixty-fourth, and one inserted row invalidates it
/// silently. Such an element is reported as unresolved instead, which the
/// recording surfaces for a human to correct -- measured, that is 26% of the
/// cases attributes could not identify.
const MAX_COUNTABLE_SIBLINGS: usize = 10;

/// A declarative element selector.
///
/// Every populated field must match, so adding a field always narrows the
/// result.  Matching is exact by default; `contains` is opt-in per query
/// because a substring match that silently picks a different button is exactly
/// the failure mode this driver exists to prevent.
// Deliberately not `Deserialize`: `from_value` is the only way in, because it
// is where the rules live -- an ordinal of 0, an unknown direction, an anchor
// nested too deep. A derived implementation would accept all of those, and the
// checks would then apply only to whichever path happened to call the parser.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Locator {
    pub role: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
    pub automation_id: Option<String>,
    pub class_name: Option<String>,
    pub framework_id: Option<String>,
    pub states: Option<States>,
    pub actions: Option<Vec<String>>,
    /// `exact` (default) or `contains`.
    pub match_mode: Option<String>,
    /// Which of the matching elements to take, when several is expected.
    pub nth: Option<Ordinal>,
    /// Position relative to another element, itself located by a locator.
    pub near: Option<Box<Proximity>>,

    /// Search only inside the element this describes.
    ///
    /// The answer for elements that share every attribute with their siblings:
    /// seven buttons all named "Close (Ctrl+F4)" are told apart by which toolbar
    /// they sit in, not by anything they carry themselves. Measured here, of 143
    /// interactive elements no attribute combination could identify, naming an
    /// identifiable ancestor brought same-role siblings from a median of 66 down
    /// to 3, and made 23 of them unique outright.
    ///
    /// Distinct from `near`, which is geometric and mutual. This is containment,
    /// follows the accessibility tree, and scopes everything after it -- an
    /// ordinal counts inside the container rather than across the window, which
    /// is what makes an ordinal usable at all: counting across a window, the
    /// median element has 66 same-role siblings.
    pub within: Option<Box<Locator>>,
}

/// Which one of several matching elements is meant.
///
/// Attributes alone cannot express "the third button": that is a property of
/// the element's position among its peers, not of the element. Rows in a list,
/// repeated toolbar buttons and unlabelled fields often have no distinguishing
/// attribute at all, and on some toolkits the ones they do have are unstable --
/// measured on WinForms, automation_id and class_name both change between runs
/// of the same program, so a locator built from them works in the session that
/// recorded it and fails the next day.
///
/// Ordering is by position on screen (top to bottom, then left to right)
/// rather than enumeration order, because that is the order the instruction
/// "the third button" refers to. Enumeration order is an implementation detail
/// of the accessibility tree and does not have to agree with what is on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ordinal {
    /// 1-based: `first` is 1. Zero is rejected at parse time, since an
    /// off-by-one here silently acts on the wrong element.
    Index(usize),
    Last,
}

/// A spatial relationship to another element.
///
/// "The button next to Username" is how a person describes a control that has
/// no usable label of its own. The anchor is found first, then candidates are
/// ranked by distance from it.
#[derive(Clone, Debug, PartialEq)]
pub struct Proximity {
    /// How to find the anchor. Boxed via `Proximity` so an anchor can itself
    /// be described positionally, though nesting is bounded (see MAX_DEPTH).
    pub anchor: Locator,
    pub direction: Direction,
    /// Ignore anchors further than this, in pixels. Without a bound the
    /// "nearest" element can be on the far side of the window, which is not
    /// what "next to" means to anyone.
    pub within: Option<i32>,
}

/// Which way to look from the anchor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    /// Nearest in any direction.
    Any,
    Left,
    Right,
    Above,
    Below,
}

impl Locator {
    pub fn from_value(value: &Value) -> Result<Self, String> {
        Self::from_value_at(value, 0)
    }

    /// Parse, tracking how deeply anchors are nested.
    ///
    /// An anchor is itself a locator and may have its own anchor. That is
    /// occasionally useful, but unbounded nesting turns one `find` into an
    /// exponential search, so the chain is capped rather than trusted.
    fn from_value_at(value: &Value, depth: usize) -> Result<Self, String> {
        if depth > MAX_ANCHOR_DEPTH {
            return Err(format!(
                "locator.near is nested more than {MAX_ANCHOR_DEPTH} deep"
            ));
        }
        let object = value.as_object().ok_or("locator must be an object")?;
        if object.is_empty() {
            return Err("locator must constrain at least one field".to_string());
        }
        let text = |key: &str| -> Option<String> {
            object.get(key).and_then(Value::as_str).map(str::to_string)
        };
        let match_mode = text("match").unwrap_or_else(|| "exact".to_string());
        if !matches!(match_mode.as_str(), "exact" | "contains") {
            return Err("locator match must be exact or contains".to_string());
        }

        let states = match object.get("states") {
            None => None,
            Some(Value::Object(map)) => {
                let flag = |key: &str| map.get(key).and_then(Value::as_bool);
                Some(States {
                    enabled: flag("enabled"),
                    offscreen: flag("offscreen"),
                    focusable: flag("focusable"),
                    focused: flag("focused"),
                    read_only: flag("read_only"),
                    // Matchable, so "the password field in this form" is
                    // expressible without relying on its label.
                    protected: flag("protected"),
                })
            }
            Some(_) => return Err("locator.states must be an object".to_string()),
        };

        let actions = match object.get("actions") {
            None => None,
            Some(Value::Array(items)) => {
                let names: Vec<String> = items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                if let Some(unknown) = names
                    .iter()
                    .find(|name| !NODE_ACTIONS.contains(&name.as_str()))
                {
                    return Err(format!("locator.actions contains unknown action {unknown:?}"));
                }
                Some(names)
            }
            Some(_) => return Err("locator.actions must be an array".to_string()),
        };

        let nth = match object.get("nth") {
            None => None,
            Some(Value::String(word)) => match word.as_str() {
                "first" => Some(Ordinal::Index(1)),
                "last" => Some(Ordinal::Last),
                other => {
                    return Err(format!(
                        "locator.nth must be a positive number, \"first\" or \"last\", got {other:?}"
                    ))
                }
            },
            Some(Value::Number(number)) => {
                let index = number
                    .as_u64()
                    .ok_or("locator.nth must be a whole number")?;
                if index == 0 {
                    // Counting from zero here would be a silent off-by-one
                    // against every human-written instruction.
                    return Err("locator.nth counts from 1, so 0 is not a position".to_string());
                }
                Some(Ordinal::Index(index as usize))
            }
            Some(_) => return Err("locator.nth must be a number or a word".to_string()),
        };

        let near = match object.get("near") {
            None => None,
            Some(Value::Object(map)) => {
                let anchor_value = map
                    .get("anchor")
                    .ok_or("locator.near requires an anchor")?;
                let anchor = Self::from_value_at(anchor_value, depth + 1)?;
                let direction = match map.get("direction").and_then(Value::as_str) {
                    None | Some("any") => Direction::Any,
                    Some("left") => Direction::Left,
                    Some("right") => Direction::Right,
                    Some("above") => Direction::Above,
                    Some("below") => Direction::Below,
                    Some(other) => {
                        return Err(format!("locator.near.direction {other:?} is not known"))
                    }
                };
                let within = match map.get("within") {
                    None => None,
                    Some(value) => {
                        let pixels = value
                            .as_i64()
                            .ok_or("locator.near.within must be a number of pixels")?;
                        if pixels <= 0 {
                            return Err("locator.near.within must be positive".to_string());
                        }
                        Some(pixels as i32)
                    }
                };
                Some(Box::new(Proximity { anchor, direction, within }))
            }
            Some(_) => return Err("locator.near must be an object".to_string()),
        };

        // Shares the nesting budget with `near`, because both are locators that
        // can nest and both cost the same to resolve. Counting them separately
        // would double the real limit while guarding against the same thing.
        let within = match object.get("within") {
            None => None,
            Some(value @ Value::Object(_)) => {
                Some(Box::new(Self::from_value_at(value, depth + 1)?))
            }
            Some(_) => {
                return Err(
                    "locator.within must be an object describing the containing element"
                        .to_string(),
                )
            }
        };

        Ok(Self {
            role: text("role"),
            name: text("name"),
            value: text("value"),
            automation_id: text("automation_id"),
            class_name: text("class_name"),
            framework_id: text("framework_id"),
            states,
            actions,
            match_mode: Some(match_mode),
            nth,
            near,
            within,
        })
    }

    fn text_matches(&self, expected: &str, actual: Option<&str>) -> bool {
        let Some(actual) = actual else {
            return false;
        };
        if self.match_mode.as_deref() == Some("contains") {
            actual.to_lowercase().contains(&expected.to_lowercase())
        } else {
            actual == expected
        }
    }

    /// Whether this node satisfies every constraint the locator declares.
    pub fn matches(&self, node: &Node) -> bool {
        // A role is an identity, not a description, so it always compares
        // exactly even in `contains` mode.
        if let Some(role) = &self.role {
            if !role.eq_ignore_ascii_case(&node.role) {
                return false;
            }
        }
        if let Some(name) = &self.name {
            if !self.text_matches(name, node.name.as_deref()) {
                return false;
            }
        }
        if let Some(value) = &self.value {
            if !self.text_matches(value, node.value.as_deref()) {
                return false;
            }
        }
        if let Some(automation_id) = &self.automation_id {
            if !self.text_matches(automation_id, node.automation_id.as_deref()) {
                return false;
            }
        }
        if let Some(class_name) = &self.class_name {
            if !self.text_matches(class_name, node.class_name.as_deref()) {
                return false;
            }
        }
        if let Some(framework_id) = &self.framework_id {
            if !self.text_matches(framework_id, node.framework_id.as_deref()) {
                return false;
            }
        }
        if let Some(states) = &self.states {
            let pairs = [
                (states.enabled, node.states.enabled),
                (states.offscreen, node.states.offscreen),
                (states.focusable, node.states.focusable),
                (states.focused, node.states.focused),
                (states.read_only, node.states.read_only),
                (states.protected, node.states.protected),
            ];
            for (wanted, actual) in pairs {
                if let Some(wanted) = wanted {
                    if actual != Some(wanted) {
                        return false;
                    }
                }
            }
        }
        if let Some(actions) = &self.actions {
            if !actions.iter().all(|action| node.actions.contains(action)) {
                return false;
            }
        }
        true
    }

    /// Build the narrowest locator that identifies `node` uniquely in `nodes`.
    ///
    /// A target (`snapshot:revision:node`) is only valid inside the session that
    /// minted it: clearing the snapshot store makes it fail with
    /// `DRIVER.STALE_HANDLE` (verified). So anything that has to survive being
    /// saved and reopened must be described, not referenced.
    ///
    /// Fields are added in order of stability and the search stops as soon as
    /// the match is unique. Continuing past that point does not improve
    /// uniqueness but does make the locator brittle: every extra field is one
    /// more thing an unrelated UI change can invalidate.
    ///
    /// Returns `None` when no combination is unique, which is a real outcome the
    /// caller must handle rather than paper over — the recording spec requires
    /// such a step to be recorded as unresolved and left for a human.
    pub fn synthesize(node: &Node, nodes: &[Node]) -> Option<Self> {
        // Ordered by measured stability: role is always present, name is the
        // strongest single discriminator, the rest are narrowing aids.
        let mut candidate = Self {
            role: Some(node.role.clone()),
            ..Self::empty()
        };

        // Role alone is accepted only when the element has no identity of its
        // own to offer. It is often unique in the window being recorded and
        // almost never unique in the application as it grows: of the twelve
        // windows open on this machine, ten hold more than one button (one holds
        // twenty-eight), and only 66 of 141 roles are unique. Returning
        // `{"role": "edit"}` because today's window has a single edit box
        // produces a locator that works while being recorded and reports
        // AMBIGUOUS_MATCH once a second field exists -- while the durable name
        // sitting on the element goes unused.
        let has_identity = non_empty(node.name.as_deref()).is_some()
            || durable(node.automation_id.as_deref()).is_some();
        if !has_identity && candidate.unique_for(node, nodes) {
            return Some(candidate);
        }

        // Only non-empty values narrow anything; an empty string would match
        // every node that also lacks the field. And only *durable* values are
        // used: a field the toolkit regenerates per run would identify the
        // element perfectly today and match nothing tomorrow.
        //
        // automation_id comes before name because it is the more durable of the
        // two: it is written in the application's source, is never shown to a
        // user and so is never translated. A name is the label on the control,
        // and labels get localised -- this very crate had to stop reading role
        // from a localised string after watching it change from `按钮` to
        // `button` between runs. Where an author-written id exists it is the
        // better identity; name remains the fallback, since most elements have
        // no id at all.
        // An id that is really a slot number is the exception to that preference.
        // `row-0` identifies the first row rather than the row it holds, so a
        // locator built on it selects whoever moves into that slot: measured on
        // the fixture table, the same locator reported `edit:Ada` and then
        // `edit:New5` after a row was inserted above -- unique and error-free
        // both times. The row also carries `"Order for Ada"`, which keeps
        // pointing at Ada, but it was never reached because the id came first
        // and already resolved uniquely. So a sequential id yields to the name
        // and is kept only as a last resort.
        let positional_id = durable(node.automation_id.as_deref())
            .is_some_and(|id| sequential_identifier(&id, nodes));

        let refinements: [fn(&mut Self, &Node); 5] = if positional_id {
            [
                |locator, node| locator.name = non_empty(node.name.as_deref()),
                |locator, node| locator.class_name = durable(node.class_name.as_deref()),
                |locator, node| locator.framework_id = non_empty(node.framework_id.as_deref()),
                |locator, node| locator.automation_id = durable(node.automation_id.as_deref()),
                |_, _| {},
            ]
        } else {
            [
                |locator, node| locator.automation_id = durable(node.automation_id.as_deref()),
                |locator, node| locator.name = non_empty(node.name.as_deref()),
                |locator, node| locator.class_name = durable(node.class_name.as_deref()),
                |locator, node| locator.framework_id = non_empty(node.framework_id.as_deref()),
                |_, _| {},
            ]
        };

        // A refinement that added nothing must not count as an attempt. Where
        // the element has no automation_id, applying that step leaves the
        // locator exactly as it was; testing uniqueness again at that point
        // would return the unchanged `{"role": ...}` and skip the name that was
        // about to be tried -- the weakest locator, reached by accident.
        for refine in refinements {
            let before = candidate.clone();
            refine(&mut candidate, node);
            if candidate == before {
                continue;
            }
            if candidate.unique_for(node, nodes) {
                return Some(candidate);
            }
        }

        // Attributes are exhausted. What is left is where the element sits, and
        // measurement says that is worth trying: of 143 interactive elements on
        // this machine that no attribute combination could identify, naming a
        // container brought same-role siblings from a median of 66 down to 3.
        //
        // Seven buttons all named "Close (Ctrl+F4)" are a real case, and nothing
        // they carry tells them apart -- only which toolbar they are in does.
        Self::by_container(node, nodes, &candidate)
    }

    /// Identify `node` by the container it sits in, and its position inside it.
    ///
    /// The container is the nearest ancestor that can itself be identified, not
    /// simply the parent. A parent is very often an unnamed `group`, and using it
    /// would move the problem up one level rather than solve it: a locator whose
    /// container cannot be found resolves to nothing.
    /// Whether this locator leans on where something sits rather than what it is.
    ///
    /// The distinction is not cosmetic. Measured on a page whose table can gain a
    /// row at the top: `within: {automation_id: "row-1"}` selected Ada before the
    /// insertion and the newly added row afterwards -- unique both times, no
    /// error either time. A locator that names the row by its content
    /// (`"Order for Ada"`) kept selecting Ada. So a positional locator is not
    /// merely weaker; it fails in the one way that cannot be noticed.
    ///
    /// Reported for the whole chain, because a container that drifts takes the
    /// element with it.
    fn leans_on_position(&self, nodes: &[Node]) -> bool {
        if self.nth.is_some() {
            return true;
        }
        if self
            .automation_id
            .as_deref()
            .is_some_and(|id| sequential_identifier(id, nodes))
        {
            return true;
        }
        if let Some(container) = &self.within {
            if container.leans_on_position(nodes) {
                return true;
            }
        }
        if let Some(near) = &self.near {
            if near.anchor.leans_on_position(nodes) {
                return true;
            }
        }
        false
    }

    fn by_container(node: &Node, nodes: &[Node], attributes: &Self) -> Option<Self> {
        let by_id: HashMap<&str, &Node> = nodes
            .iter()
            .map(|other| (other.node_id.as_str(), other))
            .collect();

        let mut current = node.parent_id.as_deref();
        let mut steps = 0usize;
        // The best positional candidate seen so far. Something that drifts when
        // the list changes is still better than nothing, so it is held back
        // rather than thrown away while a stabler container is looked for.
        let mut fallback: Option<Self> = None;
        while let Some(id) = current {
            steps += 1;
            if steps > MAX_ANCESTRY_DEPTH {
                return fallback;
            }
            let Some(ancestor) = by_id.get(id) else {
                return fallback;
            };

            if let Some(container) = Self::synthesize(ancestor, nodes) {
                let mut candidate = attributes.clone();
                candidate.within = Some(Box::new(container));

                // The container alone may be enough. 23 of the 143 were unique
                // once scoped, and a locator without an ordinal survives a
                // sibling being added or reordered, so it is preferred.
                if candidate.unique_for(node, nodes) {
                    // A container identified by its position is worth walking
                    // past. Measured on a table that can gain a row at the top:
                    // `within: {automation_id: "row-1"}` selected Ada, then
                    // selected the newly inserted row after the insertion --
                    // unique and error-free both times, which is the one failure
                    // shape nobody notices. One level up, the row carries
                    // `"Order for Ada"` and keeps selecting Ada.
                    //
                    // Kept as a fallback rather than discarded: a positional
                    // container still beats no locator at all, and some trees
                    // offer nothing better.
                    if !candidate.leans_on_position(nodes) {
                        return Some(candidate);
                    }
                    if fallback.is_none() {
                        fallback = Some(candidate);
                    }
                    current = ancestor.parent_id.as_deref();
                    continue;
                }

                // Otherwise count inside the container. Deliberately not across
                // the window: there the median element has 66 same-role
                // siblings, and "the 66th button" is not a description anyone
                // can check or that survives a layout change.
                let inside = candidate.resolve(nodes);
                if inside.len() <= MAX_COUNTABLE_SIBLINGS {
                    let mut ordered: Vec<&Node> = inside;
                    ordered.sort_by_key(|other| reading_order(other));
                    if let Some(index) =
                        ordered.iter().position(|other| other.node_id == node.node_id)
                    {
                        candidate.nth = Some(Ordinal::Index(index + 1));
                        if candidate.unique_for(node, nodes) {
                            // An ordinal is positional by definition, so this is
                            // only ever a fallback -- but a real one: 58% of the
                            // measured sample needed it.
                            if fallback.is_none() {
                                fallback = Some(candidate);
                            }
                            current = ancestor.parent_id.as_deref();
                            continue;
                        }
                    }
                } else {
                    // Too many siblings already, and a higher ancestor holds a
                    // superset of this subtree -- so every remaining step is
                    // guaranteed to be worse. Walking on cost 41ms per element
                    // on a thousand-node page (907ms of a 917ms total spent on
                    // the 22 elements that were going to fail anyway), all of it
                    // searching a space that cannot contain an answer.
                    return fallback;
                }

                // This ancestor was identifiable but did not narrow enough.
                // Keep walking: a higher ancestor can still be the one that
                // does, and a quarter of the measured sample needed more than
                // the first.
                current = ancestor.parent_id.as_deref();
                continue;
            }
            current = ancestor.parent_id.as_deref();
        }
        // Nothing stabler turned up on the way to the root, so the positional
        // candidate is the answer after all.
        fallback
    }

    /// A locator with no constraints, for building one field at a time.
    fn empty() -> Self {
        Self {
            role: None,
            name: None,
            value: None,
            automation_id: None,
            class_name: None,
            framework_id: None,
            states: None,
            actions: None,
            match_mode: None,
            nth: None,
            near: None,
            within: None,
        }
    }

    /// Render as JSON, omitting fields this locator does not constrain.
    ///
    /// Absent and null are not the same thing here: a null `name` would be read
    /// back as "the name must be null", which matches different elements.
    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        let mut put = |key: &str, value: &Option<String>| {
            if let Some(text) = value {
                object.insert(key.to_string(), json!(text));
            }
        };
        put("role", &self.role);
        put("name", &self.name);
        put("value", &self.value);
        put("automation_id", &self.automation_id);
        put("class_name", &self.class_name);
        put("framework_id", &self.framework_id);
        put("match", &self.match_mode);
        if let Some(actions) = &self.actions {
            object.insert("actions".to_string(), json!(actions));
        }
        if let Some(states) = &self.states {
            object.insert("states".to_string(), states.to_json());
        }
        if let Some(nth) = &self.nth {
            object.insert(
                "nth".to_string(),
                match nth {
                    Ordinal::Index(index) => json!(index),
                    Ordinal::Last => json!("last"),
                },
            );
        }
        if let Some(near) = &self.near {
            let mut relation = serde_json::Map::new();
            relation.insert("anchor".to_string(), near.anchor.to_json());
            if near.direction != Direction::Any {
                relation.insert(
                    "direction".to_string(),
                    json!(match near.direction {
                        Direction::Left => "left",
                        Direction::Right => "right",
                        Direction::Above => "above",
                        Direction::Below => "below",
                        Direction::Any => unreachable!(),
                    }),
                );
            }
            if let Some(within) = near.within {
                relation.insert("within".to_string(), json!(within));
            }
            object.insert("near".to_string(), Value::Object(relation));
        }
        if let Some(container) = &self.within {
            object.insert("within".to_string(), container.to_json());
        }
        Value::Object(object)
    }

    /// Every element this locator selects, in the order it selects them.
    ///
    /// This is the real entry point: `matches` only tests one node against the
    /// attribute predicates, which cannot answer "the third button" or "the
    /// field next to Username" -- both are properties of an element's place
    /// among the others, so they need the whole set.
    ///
    /// The order of the three stages is not arbitrary. Attributes narrow to
    /// candidates, proximity filters those, and only then does an ordinal
    /// count. Counting first would make "the second button next to Username"
    /// mean "the second button overall, which happens to be near Username" --
    /// a different element, and usually none at all.
    pub fn resolve<'a>(&self, nodes: &'a [Node]) -> Vec<&'a Node> {
        let mut candidates: Vec<&Node> = nodes.iter().filter(|node| self.matches(node)).collect();

        // Before proximity and before counting: `within` is a scope, and both of
        // the others are meant to apply inside it. "The second Close button in
        // the tab bar" must count within the tab bar -- counting first and then
        // checking containment would mean "the second Close button in the
        // window, if it happens to be in the tab bar", which is a different
        // element and usually none.
        if let Some(container) = &self.within {
            candidates = container.contain(candidates, nodes);
        }

        if let Some(near) = &self.near {
            candidates = near.filter(candidates, nodes);
        }

        match self.nth {
            None => candidates,
            Some(ordinal) => {
                // Screen order, not enumeration order: "the third button" means
                // the third one a person sees, and the accessibility tree does
                // not promise to enumerate in that order.
                candidates.sort_by_key(|node| reading_order(node));
                let picked = match ordinal {
                    Ordinal::Index(index) => candidates.get(index - 1).copied(),
                    Ordinal::Last => candidates.last().copied(),
                };
                picked.into_iter().collect()
            }
        }
    }

    /// Keep only the candidates that sit inside the element this locator names.
    ///
    /// Walks `parent_id` rather than comparing rectangles. Containment in the
    /// accessibility tree is what the application actually declares; overlapping
    /// bounds are a consequence of layout and would also catch a tooltip drawn
    /// over a toolbar, or miss a scrolled-out row that is still a child.
    ///
    /// An unresolvable or ambiguous container yields nothing, matching how an
    /// anchor behaves: "inside the search panel" when there is no search panel,
    /// or two of them, is a failed lookup. Silently dropping the scope would
    /// search the whole window and act on some other element entirely.
    fn contain<'a>(&self, candidates: Vec<&'a Node>, nodes: &[Node]) -> Vec<&'a Node> {
        let containers = self.resolve(nodes);
        if containers.len() != 1 {
            return Vec::new();
        }
        let container_id = containers[0].node_id.as_str();

        let by_id: HashMap<&str, &Node> = nodes
            .iter()
            .map(|node| (node.node_id.as_str(), node))
            .collect();

        candidates
            .into_iter()
            .filter(|candidate| {
                // The container is not inside itself: "the button in the toolbar"
                // should not offer the toolbar.
                let mut current = candidate.parent_id.as_deref();
                let mut steps = 0usize;
                while let Some(id) = current {
                    // A malformed tree with a parent cycle would otherwise hang
                    // the search. The depth is generous enough that no real
                    // hierarchy reaches it.
                    steps += 1;
                    if steps > MAX_ANCESTRY_DEPTH {
                        return false;
                    }
                    if id == container_id {
                        return true;
                    }
                    current = by_id.get(id).and_then(|node| node.parent_id.as_deref());
                }
                false
            })
            .collect()
    }

    /// Whether this locator matches `node` and nothing else.
    ///
    /// Both halves matter: a locator that is unique but matches a *different*
    /// node would send the action to the wrong element.
    fn unique_for(&self, node: &Node, nodes: &[Node]) -> bool {
        // Via `resolve`, so a positional locator is judged by what it actually
        // selects. Testing the attribute predicates alone would call "the third
        // button" ambiguous whenever more than one button exists, which is
        // exactly when it is useful.
        let selected = self.resolve(nodes);
        selected.len() == 1 && selected[0].node_id == node.node_id
    }
}

impl Proximity {
    /// Keep the candidates that sit in the requested direction from the anchor,
    /// nearest first.
    ///
    /// Returns nothing when the anchor cannot be found, rather than falling
    /// back to the unfiltered candidates: "the field next to Username" with no
    /// Username on screen is a failed lookup, and quietly dropping the
    /// constraint would act on an arbitrary field instead.
    fn filter<'a>(&self, candidates: Vec<&'a Node>, nodes: &'a [Node]) -> Vec<&'a Node> {
        // The anchor is resolved with the full locator machinery, so it can
        // itself be positional.
        let anchors = self.anchor.resolve(nodes);
        // An ambiguous anchor is no anchor: picking one of several would make
        // the result depend on enumeration order.
        let [anchor] = anchors[..] else {
            return Vec::new();
        };
        let Some(origin) = anchor.bounds else {
            return Vec::new();
        };

        let mut ranked: Vec<(i64, &Node)> = candidates
            .into_iter()
            .filter(|node| node.node_id != anchor.node_id)
            .filter_map(|node| {
                let bounds = node.bounds?;
                if !self.direction.holds(&origin, &bounds) {
                    return None;
                }
                let distance = gap(&origin, &bounds);
                match self.within {
                    // Squared on this side of the comparison, because `gap`
                    // returns a squared distance to stay in integers. Comparing
                    // against the raw limit would quietly square the threshold:
                    // `within: 40` would mean 6 pixels, and a label 10 pixels
                    // from its own field would be judged too far away.
                    Some(limit) => {
                        let limit = i64::from(limit);
                        (distance <= limit * limit).then_some((distance, node))
                    }
                    None => Some((distance, node)),
                }
            })
            .collect();

        // Nearest first, with reading order breaking ties so the result does
        // not depend on enumeration order.
        ranked.sort_by_key(|(distance, node)| (*distance, reading_order(node)));
        ranked.into_iter().map(|(_, node)| node).collect()
    }
}

impl Direction {
    /// Whether `other` lies this way from `origin`.
    ///
    /// Judged by centres, and requiring genuine separation on the axis: two
    /// controls on the same row are not "above" one another just because a few
    /// pixels of rounding separate their centres.
    fn holds(&self, origin: &Bounds, other: &Bounds) -> bool {
        let (ox, oy) = origin.center();
        let (tx, ty) = other.center();
        // Overlapping on an axis means they are aligned along it, which is the
        // normal case for a label and its field.
        let vertical_overlap = other.y < origin.y + origin.height && origin.y < other.y + other.height;
        let horizontal_overlap = other.x < origin.x + origin.width && origin.x < other.x + other.width;
        match self {
            Direction::Any => true,
            // A field to the right of its label is usually on the same row, so
            // "right" means right-and-roughly-level, not merely right.
            Direction::Right => tx > ox && vertical_overlap,
            Direction::Left => tx < ox && vertical_overlap,
            Direction::Below => ty > oy && horizontal_overlap,
            Direction::Above => ty < oy && horizontal_overlap,
        }
    }
}

/// Squared distance between the nearest edges of two rectangles.
///
/// Squared, not actual: only the ordering matters for ranking, and squaring
/// avoids a square root and keeps everything in integers. Callers comparing
/// against a pixel threshold must square the threshold, not this.
///
/// Edge distance rather than centre distance: a wide text field beside a short
/// label is closer to it than centre-to-centre arithmetic suggests, and "next
/// to" is about the gap between them.
fn gap(a: &Bounds, b: &Bounds) -> i64 {
    let dx = if b.x > a.x + a.width {
        i64::from(b.x - (a.x + a.width))
    } else if a.x > b.x + b.width {
        i64::from(a.x - (b.x + b.width))
    } else {
        0
    };
    let dy = if b.y > a.y + a.height {
        i64::from(b.y - (a.y + a.height))
    } else if a.y > b.y + b.height {
        i64::from(a.y - (b.y + b.height))
    } else {
        0
    };
    dx * dx + dy * dy
}

/// A node's place in reading order: down the screen, then across.
///
/// Nodes without bounds sort last; they cannot be placed, and putting them
/// first would shift every position a person counted by eye.
fn reading_order(node: &Node) -> (i32, i32, i32) {
    match node.bounds {
        Some(bounds) => {
            let (x, y) = bounds.center();
            (0, y, x)
        }
        None => (1, 0, 0),
    }
}

/// Keep an identifier only if it will still mean the same thing after a restart.
///
/// Some toolkits mint these per run. Measured on this machine by restarting a
/// WinForms fixture: the same edit box reported automation_id 7473382 and then
/// 15534534, and class_name `...app.0.34473a7_r14_ad1` then `...376a1c9_r8_ad1`.
/// A locator built from either identifies the element perfectly in the session
/// that recorded it and matches nothing afterwards -- the worst failure shape
/// available, because it looks correct exactly while being tested.
///
/// Judged by the shape of the value rather than by which toolkit produced it.
/// Of the 64 automation ids visible across the applications open on this
/// machine, 62 are author-written names (`view_1`, `MenuBar`,
/// `FileExplorerSearchBox`) which showed no drift when re-read, and 2 are bare
/// numbers. Discarding the whole field would throw away the best identifier
/// most applications offer.
/// Whether an identifier is a position dressed up as a name.
///
/// `row-0` and `save-1` are the same shape, and the difference matters: reusing
/// the first after a row is inserted selects whoever moved into that slot, while
/// the second names a button that will still be the save button tomorrow.
///
/// Shape cannot tell them apart, so this counts instead. Sequential identifiers
/// arrive as a family -- measured on this machine, `row` had 7 members,
/// `list_id_2` had 44, `view` had 22 -- whereas an id that merely ends in a digit
/// stands alone. Two members are enough: an author writing `save-1` with no
/// `save-2` anywhere is naming a thing, not counting.
fn sequential_identifier(value: &str, nodes: &[Node]) -> bool {
    let stem = value.trim_end_matches(|character: char| character.is_ascii_digit());
    if stem.len() == value.len() {
        return false;
    }
    // A bare number (`"3"`) is a position with the stem left off.
    if stem.trim_end_matches(['-', '_']).is_empty() {
        return true;
    }
    let mut relatives = 0usize;
    for node in nodes {
        let Some(other) = node.automation_id.as_deref() else {
            continue;
        };
        if other == value || !other.starts_with(stem) {
            continue;
        }
        // The remainder has to be the number, not a longer name that happens to
        // share a prefix: `row-1` is family to `row-2`, not to `row-detail`.
        if other[stem.len()..].chars().all(|c| c.is_ascii_digit()) {
            relatives += 1;
            if relatives > 0 {
                return true;
            }
        }
    }
    false
}

fn durable(value: Option<&str>) -> Option<String> {
    let value = non_empty(value)?;
    // A bare number is a handle, not a name. Nobody writes `id="7473382"`.
    if value.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    // The WinForms per-run suffix, e.g. `WindowsForms10.EDIT.app.0.34473a7_r14_ad1`.
    if volatile_suffix(&value) {
        return None;
    }
    // A list of style classes, which a WebView reports as class_name verbatim:
    // `flex shrink-0 items-center justify-center font-[400] text-[14px] ...`.
    // Measured here, class_name has a median length of 17 -- author-written names
    // like `actions-container` -- but 153 values exceed 120 characters and the
    // longest is 937, every one of them a Tailwind list. Such a value describes
    // how the element looks, so changing a font size silently unmatches the
    // locator while the element is still there.
    if style_list(&value) {
        return None;
    }
    Some(value)
}

/// Whether this looks like a list of style classes rather than one identifier.
///
/// Judged by shape, not length: several space-separated tokens is what a class
/// attribute looks like, and a single long token is usually a control name
/// (`NonClientVerticalScrollBar`, 26 characters, perfectly usable). The
/// threshold is deliberately above what a compound name reaches -- `monaco-icon
/// -label` and `actions-container` carry no spaces at all.
fn style_list(value: &str) -> bool {
    // Two would reject legitimate two-word names; the measured style lists all
    // carry many more.
    value.split_whitespace().count() > 3
}

/// Whether a name ends in the `_r<n>_ad<n>` suffix WinForms regenerates.
///
/// Hand-written rather than a regex, to keep this crate free of a dependency
/// for one pattern. Deliberately narrow: it matches the one shape actually
/// observed changing, and of the twenty windows open here only the WinForms one
/// is caught -- `Notepad`, `Chrome_WidgetWin_1` and `XLMAIN` are all kept.
fn volatile_suffix(value: &str) -> bool {
    let Some(rest) = value.rsplit_once("_ad").and_then(|(head, tail)| {
        tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty()
    }.then_some(head)) else {
        return false;
    };
    matches!(rest.rsplit_once("_r"), Some((_, digits))
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// Treat an absent field and an empty string alike: neither narrows a search.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, role: &str, name: Option<&str>) -> Node {
        Node {
            node_id: id.to_string(),
            role: role.to_string(),
            name: name.map(str::to_string),
            value: None,
            automation_id: None,
            class_name: None,
            framework_id: None,
            bounds: Some(Bounds { x: 0, y: 0, width: 10, height: 10 }),
            states: States {
                enabled: Some(true),
                ..Default::default()
            },
            actions: vec!["invoke".to_string()],
            depth: 1,
            parent_id: None,
            children: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Containers (`within`)
    //
    // Measured on this machine: of 143 interactive elements that no attribute
    // combination could identify, naming an identifiable ancestor brought
    // same-role siblings from a median of 66 down to 3, and made 23 unique
    // outright. The motivating case was seven buttons all named
    // "Close (Ctrl+F4)", told apart only by which toolbar held them.
    // -----------------------------------------------------------------------

    /// Like `node`, but placed in a tree and positioned on screen.
    fn node_at(
        id: &str,
        role: &str,
        name: Option<&str>,
        parent: Option<&str>,
        bounds: Option<Bounds>,
    ) -> Node {
        let mut built = node(id, role, name);
        built.parent_id = parent.map(str::to_string);
        built.bounds = bounds;
        built
    }

    /// Two toolbars, each holding two identically named buttons.
    fn toolbars() -> Vec<Node> {
        let mut nodes = vec![node_at("w", "window", Some("App"), None, None)];
        for (row, (bar_id, bar_name)) in
            [("t1", "Explorer actions"), ("t2", "Terminal actions")].iter().enumerate()
        {
            nodes.push(node_at(bar_id, "tool_bar", Some(bar_name), Some("w"), None));
            for slot in 0..2i32 {
                nodes.push(node_at(
                    &format!("{bar_id}b{slot}"),
                    "button",
                    Some("Close"),
                    Some(bar_id),
                    Some(Bounds {
                        x: 100 * slot + 10,
                        y: 50 * row as i32,
                        width: 40,
                        height: 20,
                    }),
                ));
            }
        }
        nodes
    }

    #[test]
    fn a_container_scopes_the_search_to_its_subtree() {
        let nodes = toolbars();
        let locator = Locator::from_value(&json!({
            "role": "button",
            "name": "Close",
            "within": {"role": "tool_bar", "name": "Terminal actions"}
        }))
        .expect("a valid locator");

        let found = locator.resolve(&nodes);

        assert_eq!(found.len(), 2, "that toolbar's buttons, and neither of the other's");
        assert!(found.iter().all(|node| node.node_id.starts_with("t2")));
    }

    #[test]
    fn an_ordinal_counts_inside_the_container_not_across_the_window() {
        // The reason the scope is applied first. Counting across the window would
        // make this the second Close button anywhere -- which is in the *first*
        // toolbar, so the action lands on a different control while the locator
        // reads as though it were scoped.
        let nodes = toolbars();
        let locator = Locator::from_value(&json!({
            "role": "button",
            "within": {"role": "tool_bar", "name": "Terminal actions"},
            "nth": 2
        }))
        .expect("a valid locator");

        let found = locator.resolve(&nodes);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].node_id, "t2b1", "the second within that toolbar");
    }

    #[test]
    fn an_unfindable_container_selects_nothing() {
        // Not a fallback to the whole window: "the button in the search panel"
        // with no search panel present is a failed lookup, and quietly dropping
        // the scope would act on some other button entirely.
        let nodes = toolbars();
        let locator = Locator::from_value(&json!({
            "role": "button",
            "within": {"role": "tool_bar", "name": "Nothing like this"}
        }))
        .expect("a valid locator");

        assert!(locator.resolve(&nodes).is_empty());
    }

    #[test]
    fn an_ambiguous_container_selects_nothing() {
        // Two toolbars match `{"role": "tool_bar"}`. Picking one would be a coin
        // flip that reads as a definite answer.
        let nodes = toolbars();
        let locator = Locator::from_value(&json!({
            "role": "button",
            "within": {"role": "tool_bar"}
        }))
        .expect("a valid locator");

        assert!(locator.resolve(&nodes).is_empty());
    }

    #[test]
    fn containment_follows_the_tree_not_the_rectangles() {
        // A tooltip drawn over a toolbar overlaps it without being in it, and a
        // scrolled-out row is still a child. The tree is what the application
        // declares; overlapping bounds are a consequence of layout.
        let mut nodes = toolbars();
        nodes.push(node_at(
            "tip",
            "button",
            Some("Close"),
            Some("w"),
            // Sitting exactly over the first toolbar's first button.
            Some(Bounds { x: 10, y: 0, width: 40, height: 20 }),
        ));

        let locator = Locator::from_value(&json!({
            "role": "button",
            "within": {"role": "tool_bar", "name": "Explorer actions"}
        }))
        .expect("a valid locator");

        let found = locator.resolve(&nodes);

        assert_eq!(found.len(), 2, "the overlay is not one of the toolbar's buttons");
        assert!(found.iter().all(|node| node.node_id.starts_with("t1")));
    }

    #[test]
    fn the_container_itself_is_not_one_of_its_contents() {
        let nodes = toolbars();
        let locator = Locator::from_value(&json!({
            "role": "tool_bar",
            "within": {"role": "tool_bar", "name": "Explorer actions"}
        }))
        .expect("a valid locator");

        assert!(locator.resolve(&nodes).is_empty());
    }

    #[test]
    fn a_parent_cycle_does_not_hang_the_search() {
        // A malformed tree, but one that would spin for ever rather than fail.
        let mut nodes = toolbars();
        nodes[0].parent_id = Some("t1b0".to_string());

        let locator = Locator::from_value(&json!({
            "role": "button",
            "within": {"role": "window", "name": "App"}
        }))
        .expect("a valid locator");

        // That it returns at all is the assertion.
        let _ = locator.resolve(&nodes);
    }

    #[test]
    fn synthesize_falls_back_to_the_container_when_attributes_run_out() {
        let nodes = toolbars();
        let target = nodes.iter().find(|node| node.node_id == "t2b0").expect("a button");

        let locator = Locator::synthesize(target, &nodes).expect("a container makes it findable");
        let rendered = locator.to_json();

        assert!(rendered.get("within").is_some(), "got {rendered}");
        let found = locator.resolve(&nodes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].node_id, "t2b0");
    }

    #[test]
    fn an_anonymous_container_is_itself_described_rather_than_skipped() {
        // The nearest parent is very often an unnamed group, which cannot be
        // named directly. Rather than skipping to a higher ancestor, the
        // container is described the same way anything else is -- "the first
        // group in the Actions toolbar" -- because the recursion applies to
        // containers too.
        //
        // That is the better of the two: a more local description is unaffected
        // by an unrelated sibling being added to the toolbar. What must hold is
        // that the whole chain resolves and selects exactly the target; which
        // ancestor it settled on is an implementation detail, and pinning it
        // would turn a future improvement into a failure.
        let mut nodes = vec![
            node_at("w", "window", Some("App"), None, None),
            node_at("bar", "tool_bar", Some("Actions"), Some("w"), None),
            node_at("g1", "group", None, Some("bar"), None),
            node_at("g2", "group", None, Some("bar"), None),
        ];
        nodes.push(node_at(
            "b1",
            "button",
            Some("Go"),
            Some("g1"),
            Some(Bounds { x: 0, y: 0, width: 20, height: 10 }),
        ));
        nodes.push(node_at(
            "b2",
            "button",
            Some("Go"),
            Some("g2"),
            Some(Bounds { x: 0, y: 40, width: 20, height: 10 }),
        ));

        let target = nodes.iter().find(|node| node.node_id == "b1").expect("a button");
        let locator = Locator::synthesize(target, &nodes).expect("a container makes it findable");
        let rendered = locator.to_json();

        // Scoped somehow -- attributes alone cannot separate the two Go buttons.
        assert!(rendered.get("within").is_some(), "got {rendered}");

        // And the chain is not decorative: it selects the target and nothing
        // else. A container that resolves to nothing would make this empty,
        // which is the failure a nested locator can hide.
        let found = locator.resolve(&nodes);
        assert_eq!(found.len(), 1, "selected {found:?} from {rendered}");
        assert_eq!(found[0].node_id, "b1");

        // The other button must get a different locator, or one of the two
        // recordings would replay onto the wrong control.
        let other = nodes.iter().find(|node| node.node_id == "b2").expect("a button");
        let other_locator =
            Locator::synthesize(other, &nodes).expect("the sibling is findable too");
        assert_ne!(other_locator.to_json(), rendered);
        let found_other = other_locator.resolve(&nodes);
        assert_eq!(found_other.len(), 1);
        assert_eq!(found_other[0].node_id, "b2");
    }

    #[test]
    fn a_locator_with_a_container_survives_a_round_trip() {
        // Saved and reopened is the whole point; a container that does not
        // serialise makes a recording fail at replay rather than at save.
        let original = Locator::from_value(&json!({
            "role": "button",
            "within": {"role": "tool_bar", "name": "Terminal actions"},
            "nth": 2
        }))
        .expect("a valid locator");

        let reparsed = Locator::from_value(&original.to_json()).expect("still valid");

        assert_eq!(reparsed.to_json(), original.to_json());
    }

    #[test]
    fn a_container_must_be_an_object() {
        for bad in [json!("tool_bar"), json!(3), json!([{"role": "tool_bar"}])] {
            let error = Locator::from_value(&json!({"role": "button", "within": bad}))
                .expect_err("a non-object container must be rejected");
            assert!(error.contains("within"), "got {error}");
        }
    }

    #[test]
    fn a_style_class_list_is_not_an_identifier() {
        // Measured: class_name has a median length of 17 (author-written names
        // like `actions-container`), but a WebView reports the whole class
        // attribute -- 153 values over 120 characters, the longest 937, every one
        // a Tailwind list. Such a locator unmatches when a font size changes,
        // while the element is still there.
        let tailwind = "flex shrink-0 items-center justify-center font-[400] text-[14px]";
        let mut styled = node_at("b", "button", None, Some("w"), None);
        styled.class_name = Some(tailwind.to_string());
        let nodes = vec![
            node_at("w", "window", Some("App"), None, None),
            styled.clone(),
            node_at("b2", "button", None, Some("w"), None),
        ];

        if let Some(locator) = Locator::synthesize(&styled, &nodes) {
            assert!(
                locator.to_json().get("class_name").is_none(),
                "a style list must not be used as an identity: {}",
                locator.to_json()
            );
        }
    }

    #[test]
    fn a_compound_class_name_is_still_usable() {
        // The rule is about shape, not length. `monaco-icon-label` and
        // `NonClientVerticalScrollBar` are control names and carry no spaces;
        // rejecting them would throw away a working identifier.
        for name in ["actions-container", "monaco-icon-label", "NonClientVerticalScrollBar"] {
            let mut styled = node_at("b", "button", None, Some("w"), None);
            styled.class_name = Some(name.to_string());
            let nodes = vec![
                node_at("w", "window", Some("App"), None, None),
                styled.clone(),
                node_at("b2", "button", None, Some("w"), None),
            ];

            let locator =
                Locator::synthesize(&styled, &nodes).expect("the class name identifies it");

            assert_eq!(
                locator.to_json().get("class_name").and_then(Value::as_str),
                Some(name)
            );
        }
    }

    // -----------------------------------------------------------------------
    // Protected elements
    //
    // Measured against a real Win32 password box: UIA reports IsPassword and
    // withholds the value, while an ordinary edit beside it returns its text.
    // The flag exists to say which of those happened.
    // -----------------------------------------------------------------------

    #[test]
    fn a_protected_element_says_so_instead_of_looking_empty() {
        // Without the marker, a password field is indistinguishable from a field
        // the capture simply failed to read -- and a caller would waste time
        // treating a deliberate omission as a bug.
        let mut secret = node("e1", "Edit", Some("Password"));
        secret.states.protected = Some(true);
        secret.value = None;

        let summary = secret.summary();

        assert!(summary.contains("protected"), "got {summary:?}");
        assert_eq!(secret.states.to_json()["protected"], serde_json::json!(true));
    }

    #[test]
    fn an_ordinary_value_is_still_reported_in_full() {
        // The point of the narrow scope: hiding ordinary field contents would
        // remove the information a caller needs to tell whether a form is
        // filled in correctly.
        let mut ordinary = node("e2", "Edit", Some("Search"));
        ordinary.value = Some("quarterly-report-2026".into());

        let summary = ordinary.summary();

        assert!(summary.contains("quarterly-report-2026"), "got {summary:?}");
        assert!(!summary.contains("protected"), "got {summary:?}");
    }

    #[test]
    fn a_locator_can_pick_out_the_protected_field() {
        // Filling in a password has to keep working, and the field is often
        // unlabelled, so being able to match on the marking itself matters.
        let mut secret = node("e1", "Edit", Some("Password"));
        secret.states.protected = Some(true);
        let ordinary = node("e2", "Edit", Some("Password"));

        let locator = Locator::from_value(&serde_json::json!({
            "role": "Edit",
            "states": {"protected": true}
        }))
        .expect("a states constraint must parse");

        assert!(locator.matches(&secret));
        assert!(!locator.matches(&ordinary));
    }

    // -----------------------------------------------------------------------
    // Locator synthesis
    //
    // A saved recording holds a locator, never a target: a target stops
    // resolving once its snapshot is gone (verified against a cleared store).
    // -----------------------------------------------------------------------

    #[test]
    fn a_role_alone_is_not_trusted_while_the_element_has_a_name() {
        // This test used to assert the opposite, on the grounds that an
        // unnecessary name breaks when the button is relabelled. That risk is
        // real -- labels get localised. But the alternative it chose is weaker
        // still: role is unique only in the window as it looks today. Of the
        // twelve windows open on this machine ten hold more than one button, one
        // holds twenty-eight, and only 66 of 141 roles are unique. A locator of
        // `{"role": "button"}` therefore works while it is being recorded and
        // reports AMBIGUOUS_MATCH as soon as a second button exists.
        //
        // Both failures are loud, so neither clicks the wrong thing; the choice
        // is made on which one happens more often, and a second button is far
        // more common than a rename.
        let nodes = vec![
            node("e1", "Button", Some("Save")),
            node("e2", "Edit", Some("Filename")),
        ];

        let locator = Locator::synthesize(&nodes[0], &nodes).expect("must be unique");

        assert_eq!(locator.role.as_deref(), Some("Button"));
        assert_eq!(locator.name.as_deref(), Some("Save"));
    }

    #[test]
    fn role_alone_is_still_used_for_an_element_with_no_identity_at_all() {
        // The fallback has to stay. Plenty of elements carry neither a name nor
        // an id, and for those a role that happens to be unique is the only
        // description available -- better than refusing to describe them.
        let nodes = vec![node("e1", "Button", None), node("e2", "Edit", None)];

        let locator = Locator::synthesize(&nodes[0], &nodes).expect("must be unique");

        assert_eq!(locator.role.as_deref(), Some("Button"));
        assert_eq!(locator.name, None);
    }

    #[test]
    fn an_author_written_id_is_preferred_over_a_label_that_could_be_translated() {
        // Both would make this element unique, so whichever is tried first wins.
        // The id is the one that survives the application being translated.
        let mut first = node("e1", "button", Some("OK"));
        first.automation_id = Some("confirmButton".into());
        let mut second = node("e2", "button", Some("Cancel"));
        second.automation_id = Some("cancelButton".into());
        let nodes = vec![first, second];

        let locator = Locator::synthesize(&nodes[0], &nodes).expect("must be unique");

        assert_eq!(locator.automation_id.as_deref(), Some("confirmButton"));
        assert_eq!(locator.name, None, "the translatable label is not needed");
    }

    #[test]
    fn a_name_is_added_when_the_role_is_shared() {
        let nodes = vec![
            node("e1", "Button", Some("Save")),
            node("e2", "Button", Some("Cancel")),
        ];

        let locator = Locator::synthesize(&nodes[1], &nodes).expect("must be unique");

        assert_eq!(locator.role.as_deref(), Some("Button"));
        assert_eq!(locator.name.as_deref(), Some("Cancel"));
    }

    /// A table row that can be identified either by its slot or by its content.
    ///
    /// Modelled on the measured fixture: the row carries both `row-N` and an
    /// aria-label naming the customer, and the buttons inside carry nothing that
    /// tells them apart.
    fn order_rows() -> Vec<Node> {
        let mut nodes = Vec::new();
        let mut table = node("table", "table", None);
        table.depth = 1;
        nodes.push(table);

        for (index, customer) in ["Ada", "Brian", "Chen"].iter().enumerate() {
            let row_id = format!("r{index}");
            let mut row = node(&row_id, "data_item", Some(&format!("Order for {customer}")));
            row.automation_id = Some(format!("row-{index}"));
            row.parent_id = Some("table".into());
            row.depth = 2;
            row.bounds = Some(Bounds { x: 0, y: 100 + 34 * index as i32, width: 600, height: 34 });
            nodes.push(row);

            let mut edit = node(&format!("{row_id}-edit"), "button", Some("Edit"));
            edit.parent_id = Some(row_id.clone());
            edit.depth = 3;
            edit.bounds = Some(Bounds { x: 500, y: 100 + 34 * index as i32, width: 40, height: 20 });
            nodes.push(edit);
        }
        nodes
    }

    #[test]
    fn a_row_is_named_by_its_content_rather_than_its_slot() {
        let nodes = order_rows();
        let edit = nodes.iter().find(|n| n.node_id == "r0-edit").expect("Ada's Edit");

        let locator = Locator::synthesize(edit, &nodes).expect("identifiable");
        let container = locator.within.as_deref().expect("scoped to the row");

        // `row-1` would also resolve uniquely, which is exactly why uniqueness is
        // not the test: after a row is inserted above, it selects the new row.
        // Verified on the real fixture -- the slot-based locator reported
        // `edit:Ada` and then `edit:New5`, both times without an error.
        assert_eq!(container.name.as_deref(), Some("Order for Ada"));
        assert_eq!(container.automation_id, None, "the slot number must not be used: {container:?}");
    }

    #[test]
    fn a_content_named_row_survives_an_insertion_above_it() {
        let nodes = order_rows();
        let edit = nodes.iter().find(|n| n.node_id == "r0-edit").expect("Ada's Edit");
        let locator = Locator::synthesize(edit, &nodes).expect("identifiable");

        // The same tree after a row is prepended: every slot number shifts down
        // and the node ids are reassigned, as the browser does on a re-render.
        let mut shifted = Vec::new();
        let mut table = node("table", "table", None);
        table.depth = 1;
        shifted.push(table);
        for (index, customer) in ["New", "Ada", "Brian", "Chen"].iter().enumerate() {
            let row_id = format!("s{index}");
            let mut row = node(&row_id, "data_item", Some(&format!("Order for {customer}")));
            row.automation_id = Some(format!("row-{index}"));
            row.parent_id = Some("table".into());
            row.depth = 2;
            row.bounds = Some(Bounds { x: 0, y: 100 + 34 * index as i32, width: 600, height: 34 });
            shifted.push(row);
            let mut edit = node(&format!("{row_id}-edit"), "button", Some("Edit"));
            edit.parent_id = Some(row_id.clone());
            edit.depth = 3;
            edit.bounds = Some(Bounds { x: 500, y: 100 + 34 * index as i32, width: 40, height: 20 });
            shifted.push(edit);
        }

        let hits = locator.resolve(&shifted);
        assert_eq!(hits.len(), 1, "still unambiguous");
        // The node id changed; the customer did not. That is the whole point.
        assert_eq!(hits[0].node_id, "s1-edit");
        let row = shifted
            .iter()
            .find(|n| Some(n.node_id.as_str()) == hits[0].parent_id.as_deref())
            .expect("its row");
        assert_eq!(row.name.as_deref(), Some("Order for Ada"));
    }

    #[test]
    fn a_lone_numbered_id_is_a_name_not_a_position() {
        // `save-1` and `row-0` are the same shape. What separates them is that
        // sequential ids arrive as a family: on this machine `row` had 7 members
        // and `list_id_2` had 44, while an id that merely ends in a digit stands
        // alone. Shape alone would discard a perfectly good identifier.
        let mut button = node("e1", "button", Some("Save"));
        button.automation_id = Some("save-1".into());
        let nodes = vec![button, node("e2", "button", Some("Cancel"))];

        let locator = Locator::synthesize(&nodes[0], &nodes).expect("identifiable");
        assert_eq!(locator.automation_id.as_deref(), Some("save-1"));
    }

    #[test]
    fn a_numbered_id_with_siblings_is_a_position() {
        // The same shape, now with a relative present, so the digit is an index.
        let mut first = node("e1", "list_item", Some("Inbox"));
        first.automation_id = Some("item-0".into());
        let mut second = node("e2", "list_item", Some("Archive"));
        second.automation_id = Some("item-1".into());
        let nodes = vec![first, second];

        let locator = Locator::synthesize(&nodes[0], &nodes).expect("identifiable");
        assert_eq!(locator.name.as_deref(), Some("Inbox"), "the label identifies it");
        assert_eq!(locator.automation_id, None, "the index does not: {locator:?}");
    }

    #[test]
    fn narrowing_stops_as_soon_as_the_match_is_unique() {
        // Every field is populated, so a naive implementation would use them
        // all. Each extra field is another thing a UI change can invalidate.
        let mut first = node("e1", "Button", Some("Save"));
        first.class_name = Some("Btn".into());
        first.automation_id = Some("save-1".into());
        first.framework_id = Some("Win32".into());
        let nodes = vec![first, node("e2", "Edit", Some("Filename"))];

        let locator = Locator::synthesize(&nodes[0], &nodes).expect("must be unique");

        // The automation id identifies it, so nothing further is needed. Which
        // single field gets used is a separate decision; what this guards is
        // that only one of them does.
        assert_eq!(locator.automation_id.as_deref(), Some("save-1"));
        assert_eq!(locator.name, None);
        assert_eq!(locator.class_name, None);
        assert_eq!(locator.framework_id, None);
    }

    #[test]
    fn an_automation_id_settles_elements_that_look_identical() {
        // Same role, same name, same class: only the id can separate them.
        let mut first = node("e1", "Button", Some("Open"));
        first.class_name = Some("Btn".into());
        first.automation_id = Some("open-a".into());
        let mut second = node("e2", "Button", Some("Open"));
        second.class_name = Some("Btn".into());
        second.automation_id = Some("open-b".into());
        let nodes = vec![first, second];

        let locator = Locator::synthesize(&nodes[1], &nodes).expect("must be unique");

        assert_eq!(locator.automation_id.as_deref(), Some("open-b"));
        assert!(locator.unique_for(&nodes[1], &nodes));
    }

    #[test]
    fn indistinguishable_elements_yield_no_locator_rather_than_a_wrong_one() {
        // Two nodes identical in every describable field. Returning a locator
        // here would mean acting on whichever happened to be enumerated first.
        let nodes = vec![
            node("e1", "Button", Some("Item")),
            node("e2", "Button", Some("Item")),
        ];

        assert!(Locator::synthesize(&nodes[0], &nodes).is_none());
    }

    #[test]
    fn an_empty_field_is_never_used_to_narrow() {
        // An empty string matches every node that also lacks the field, so
        // adding it would not narrow anything while looking like it had.
        let mut first = node("e1", "Button", Some("Save"));
        first.class_name = Some("   ".into());
        let mut second = node("e2", "Button", Some("Save"));
        second.class_name = Some("".into());
        let nodes = vec![first, second];

        // Neither can be distinguished, so neither gets a locator.
        assert!(Locator::synthesize(&nodes[0], &nodes).is_none());
    }

    #[test]
    fn a_synthesized_locator_actually_resolves_to_its_own_node() {
        // The property that matters: whatever comes back must select exactly
        // the element it was built from, not merely something.
        let mut nodes = Vec::new();
        for index in 0..6 {
            let mut item = node(&format!("e{index}"), "Button", Some("Row"));
            item.automation_id = Some(format!("row-{index}"));
            nodes.push(item);
        }

        for target in &nodes {
            let locator = Locator::synthesize(target, &nodes)
                .unwrap_or_else(|| panic!("{} should be identifiable", target.node_id));
            let hits: Vec<&Node> = nodes.iter().filter(|n| locator.matches(n)).collect();
            assert_eq!(hits.len(), 1, "{} matched {} nodes", target.node_id, hits.len());
            assert_eq!(hits[0].node_id, target.node_id);
        }
    }

    #[test]
    fn a_locator_omits_the_fields_it_does_not_constrain() {
        // A null would be read back as "this field must be null", which selects
        // different elements than "I do not care about this field".
        // A nameless element, so the synthesised locator constrains only role --
        // this test is about how an unconstrained field is rendered, not about
        // which fields get chosen.
        let nodes = vec![node("e1", "Button", None)];
        let locator = Locator::synthesize(&nodes[0], &nodes).unwrap();

        let rendered = locator.to_json();
        let object = rendered.as_object().unwrap();
        assert!(object.contains_key("role"));
        assert!(!object.contains_key("name"), "unconstrained fields must be absent");
        assert!(!object.contains_key("class_name"));
    }

    #[test]
    fn a_rendered_locator_can_be_read_back_unchanged() {
        // Save and reopen is the whole point, so the round trip must hold.
        let mut first = node("e1", "Button", Some("Save"));
        first.class_name = Some("Btn".into());
        let nodes = vec![first, node("e2", "Button", Some("Save"))];

        let locator = Locator::synthesize(&nodes[0], &nodes).expect("must be unique");
        let reloaded = Locator::from_value(&locator.to_json()).expect("must parse");

        assert!(reloaded.unique_for(&nodes[0], &nodes));
    }

    #[test]
    fn bounds_report_their_centre_and_emptiness() {
        let bounds = Bounds { x: 10, y: 20, width: 100, height: 50 };
        assert_eq!(bounds.center(), (60, 45));
        assert!(!bounds.is_empty());
        assert!(Bounds { x: 0, y: 0, width: 0, height: 10 }.is_empty());
    }

    #[test]
    fn a_locator_requires_every_declared_field_to_match() {
        let button = node("n1", "Button", Some("Save"));
        let locator = Locator::from_value(&json!({"role": "Button", "name": "Save"})).unwrap();
        assert!(locator.matches(&button));

        let wrong_name = Locator::from_value(&json!({"role": "Button", "name": "Cancel"})).unwrap();
        assert!(!wrong_name.matches(&button));
    }

    #[test]
    fn matching_is_exact_unless_contains_is_requested() {
        let button = node("n1", "Button", Some("Save changes"));

        let exact = Locator::from_value(&json!({"name": "Save"})).unwrap();
        assert!(!exact.matches(&button), "exact matching must not match a prefix");

        let contains = Locator::from_value(&json!({"name": "Save", "match": "contains"})).unwrap();
        assert!(contains.matches(&button));
    }

    #[test]
    fn contains_matching_is_case_insensitive() {
        let button = node("n1", "Button", Some("Save Changes"));
        let locator =
            Locator::from_value(&json!({"name": "save changes", "match": "contains"})).unwrap();
        assert!(locator.matches(&button));
    }

    #[test]
    fn a_role_always_compares_exactly() {
        let button = node("n1", "Button", Some("Save"));
        let locator =
            Locator::from_value(&json!({"role": "Butt", "match": "contains"})).unwrap();
        assert!(!locator.matches(&button), "a partial role must never match");
    }

    #[test]
    fn a_locator_can_filter_on_states_and_actions() {
        let mut disabled = node("n1", "Button", Some("Save"));
        disabled.states.enabled = Some(false);

        let enabled_only = Locator::from_value(&json!({"states": {"enabled": true}})).unwrap();
        assert!(!enabled_only.matches(&disabled));
        assert!(enabled_only.matches(&node("n2", "Button", Some("Save"))));

        let invokable = Locator::from_value(&json!({"actions": ["invoke"]})).unwrap();
        assert!(invokable.matches(&disabled));
        let typeable = Locator::from_value(&json!({"actions": ["type_text"]})).unwrap();
        assert!(!typeable.matches(&disabled));
    }

    #[test]
    fn an_empty_locator_is_rejected() {
        assert!(Locator::from_value(&json!({})).is_err());
    }

    #[test]
    fn an_unknown_action_in_a_locator_is_rejected() {
        assert!(Locator::from_value(&json!({"actions": ["teleport"]})).is_err());
    }

    #[test]
    fn an_invalid_match_mode_is_rejected() {
        assert!(Locator::from_value(&json!({"name": "x", "match": "regex"})).is_err());
    }

    #[test]
    fn a_missing_attribute_never_satisfies_a_constraint() {
        let anonymous = node("n1", "Button", None);
        let locator = Locator::from_value(&json!({"name": "Save"})).unwrap();
        assert!(!locator.matches(&anonymous));
    }

    // -----------------------------------------------------------------------
    // Describing an element by where it is
    //
    // Attributes cannot say "the third button" or "the box next to Username",
    // which is how a person -- or an agent taking instructions from one --
    // actually refers to controls that have no distinguishing label. Measured
    // on WinForms, the attributes that would otherwise serve (automation_id,
    // class_name) change between runs of the same program, so a description
    // built from them holds only for the session that recorded it.
    // -----------------------------------------------------------------------

    /// A node at a given position, so layout-dependent behaviour is testable.
    fn placed(id: &str, role: &str, name: Option<&str>, x: i32, y: i32) -> Node {
        let mut node = node(id, role, name);
        node.bounds = Some(Bounds { x, y, width: 80, height: 24 });
        node
    }

    /// A form: a label with a field to its right, twice, then two buttons.
    ///
    /// Deliberately built out of order, so anything that works by accident of
    /// enumeration order fails here.
    fn form() -> Vec<Node> {
        vec![
            placed("submit", "Button", Some("OK"), 100, 300),
            placed("user_label", "Text", Some("Username"), 10, 100),
            placed("pass_field", "Edit", None, 100, 200),
            placed("cancel", "Button", Some("OK"), 200, 300),
            placed("user_field", "Edit", None, 100, 100),
            placed("pass_label", "Text", Some("Password"), 10, 200),
        ]
    }

    fn resolve_ids(locator: &Value, nodes: &[Node]) -> Vec<String> {
        Locator::from_value(locator)
            .expect("locator must parse")
            .resolve(nodes)
            .into_iter()
            .map(|node| node.node_id.clone())
            .collect()
    }

    #[test]
    fn an_ordinal_picks_the_nth_element_in_reading_order() {
        // "the second button". The nodes are listed with the second button
        // first, so enumeration order would give the wrong answer.
        let nodes = form();

        assert_eq!(
            resolve_ids(&json!({"role": "Button", "nth": 1}), &nodes),
            ["submit"]
        );
        assert_eq!(
            resolve_ids(&json!({"role": "Button", "nth": 2}), &nodes),
            ["cancel"]
        );
    }

    #[test]
    fn reading_order_goes_down_the_screen_before_across_it() {
        // Two rows of two. A left-to-right-first ordering would number these
        // 1,3,2,4 and every ordinal after the first would be wrong.
        let nodes = vec![
            placed("top_right", "Button", Some("b"), 200, 10),
            placed("bottom_left", "Button", Some("c"), 10, 200),
            placed("top_left", "Button", Some("a"), 10, 10),
            placed("bottom_right", "Button", Some("d"), 200, 200),
        ];

        let order: Vec<String> = (1..=4)
            .map(|index| resolve_ids(&json!({"role": "Button", "nth": index}), &nodes)[0].clone())
            .collect();

        assert_eq!(order, ["top_left", "top_right", "bottom_left", "bottom_right"]);
    }

    #[test]
    fn last_names_the_final_element_without_counting_them() {
        // A list whose length the agent does not know.
        let nodes = form();
        assert_eq!(
            resolve_ids(&json!({"role": "Button", "nth": "last"}), &nodes),
            ["cancel"]
        );
        assert_eq!(
            resolve_ids(&json!({"role": "Button", "nth": "first"}), &nodes),
            ["submit"]
        );
    }

    #[test]
    fn an_ordinal_past_the_end_selects_nothing() {
        // Rather than the last one. Asking for the fifth of three is a mistake
        // in the instruction, and quietly acting on a different element is how
        // automation does damage.
        let nodes = form();
        assert!(resolve_ids(&json!({"role": "Button", "nth": 5}), &nodes).is_empty());
    }

    #[test]
    fn an_ordinal_counts_only_the_matching_elements() {
        // "the second edit box" must not count the buttons and labels between
        // them.
        let nodes = form();
        assert_eq!(
            resolve_ids(&json!({"role": "Edit", "nth": 2}), &nodes),
            ["pass_field"]
        );
    }

    #[test]
    fn an_ordinal_of_zero_is_rejected_at_parse_time() {
        // Everyone writing the instruction counts from one; accepting 0 here
        // would silently act one element early.
        let error = Locator::from_value(&json!({"role": "Button", "nth": 0})).unwrap_err();
        assert!(error.contains("counts from 1"), "{error}");
    }

    #[test]
    fn an_element_can_be_found_by_what_it_sits_next_to() {
        // "the box next to Username" -- the field itself has no name at all,
        // which is exactly why this is needed.
        let nodes = form();

        let found = resolve_ids(
            &json!({
                "role": "Edit",
                "near": {"anchor": {"name": "Username"}, "direction": "right"},
            }),
            &nodes,
        );

        assert_eq!(found, ["user_field"]);
    }

    #[test]
    fn proximity_prefers_the_nearer_of_two_candidates() {
        // Both fields are to the right of Username in the loose sense; only the
        // one on its row is what a person means.
        let nodes = form();

        let found = resolve_ids(
            &json!({"role": "Edit", "near": {"anchor": {"name": "Password"}}}),
            &nodes,
        );

        assert_eq!(found[0], "pass_field", "the nearest must come first");
    }

    #[test]
    fn a_direction_excludes_what_lies_the_other_way() {
        // Without direction the nearest edit to Password could be either row.
        // "left of Password" is nothing here, and must resolve to nothing
        // rather than to the nearest in some other direction.
        let nodes = form();

        let found = resolve_ids(
            &json!({
                "role": "Edit",
                "near": {"anchor": {"name": "Password"}, "direction": "left"},
            }),
            &nodes,
        );

        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn a_distance_limit_rejects_something_across_the_window() {
        // "next to" has to mean near. The nearest edit to the OK button is two
        // hundred pixels up, which is not next to anything.
        let nodes = form();

        let unbounded = resolve_ids(
            &json!({"role": "Edit", "near": {"anchor": {"name": "OK", "nth": 1}}}),
            &nodes,
        );
        assert!(!unbounded.is_empty(), "without a limit something is found");

        let bounded = resolve_ids(
            &json!({
                "role": "Edit",
                "near": {"anchor": {"name": "OK", "nth": 1}, "within": 20},
            }),
            &nodes,
        );
        assert!(bounded.is_empty(), "got {bounded:?}");
    }

    #[test]
    fn a_distance_limit_is_measured_in_pixels() {
        // Coordinates taken from a real WinForms window: the "Name:" label ends
        // at x=646 and the field begins at x=656, so they are 10 pixels apart.
        //
        // The unit matters. Distances are kept squared internally to stay in
        // integers, and comparing a pixel threshold against a squared distance
        // silently squares the threshold -- `within: 40` would have meant 6
        // pixels, rejecting a label sitting right beside its own field. The
        // first version of this shipped with exactly that bug, and the earlier
        // tests missed it because they shared the implementation's assumption.
        let nodes = vec![
            {
                let mut label = node("label", "Text", Some("Name:"));
                label.bounds = Some(Bounds { x: 566, y: 362, width: 80, height: 22 });
                label
            },
            {
                let mut field = node("field", "Edit", None);
                field.bounds = Some(Bounds { x: 656, y: 359, width: 300, height: 21 });
                field
            },
        ];

        let generous = resolve_ids(
            &json!({"role": "Edit", "near": {"anchor": {"name": "Name:"}, "within": 40}}),
            &nodes,
        );
        assert_eq!(generous, ["field"], "10 pixels is within 40");

        // The other half: a limit that is genuinely too small must still bite,
        // or "within" would just be decoration.
        let strict = resolve_ids(
            &json!({"role": "Edit", "near": {"anchor": {"name": "Name:"}, "within": 5}}),
            &nodes,
        );
        assert!(strict.is_empty(), "10 pixels is not within 5, got {strict:?}");
    }

    #[test]
    fn a_missing_anchor_finds_nothing_rather_than_ignoring_the_constraint() {
        // The dangerous failure: if an unfindable anchor quietly dropped the
        // constraint, "the field next to Username" on a page with no Username
        // would type into whatever field came first.
        let nodes = form();

        let found = resolve_ids(
            &json!({"role": "Edit", "near": {"anchor": {"name": "Nonexistent"}}}),
            &nodes,
        );

        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn an_ambiguous_anchor_finds_nothing_rather_than_guessing() {
        // Two buttons are both named OK. Choosing one would make the result
        // depend on enumeration order, which is precisely what an anchor is
        // supposed to protect against.
        let nodes = form();

        let found = resolve_ids(
            &json!({"role": "Edit", "near": {"anchor": {"name": "OK"}}}),
            &nodes,
        );

        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn an_anchor_can_itself_be_described_positionally() {
        // "next to the second OK button" -- the anchor is ambiguous by name, so
        // it is disambiguated the same way any other element would be.
        let nodes = form();

        let found = resolve_ids(
            &json!({
                "role": "Button",
                "near": {"anchor": {"name": "OK", "nth": 1}, "direction": "right"},
            }),
            &nodes,
        );

        assert_eq!(found, ["cancel"]);
    }

    #[test]
    fn an_element_is_never_found_next_to_itself() {
        // Distance zero would otherwise make every element its own nearest
        // neighbour, and the anchor would win its own search.
        let nodes = form();

        let found = resolve_ids(
            &json!({"role": "Text", "near": {"anchor": {"name": "Username"}}}),
            &nodes,
        );

        assert!(!found.contains(&"user_label".to_string()), "got {found:?}");
    }

    #[test]
    fn narrowing_applies_before_counting() {
        // "the first edit box next to Password". Counting first would give the
        // first edit box overall (which is not near Password), then filter it
        // away, leaving nothing -- a subtly different and much less useful
        // meaning.
        let nodes = form();

        let found = resolve_ids(
            &json!({
                "role": "Edit",
                "near": {"anchor": {"name": "Password"}, "direction": "right"},
                "nth": 1,
            }),
            &nodes,
        );

        assert_eq!(found, ["pass_field"]);
    }

    #[test]
    fn the_first_focusable_input_is_expressible() {
        // A whole class of instruction -- "type into the first input" -- with
        // no reliance on any name at all.
        let mut nodes = form();
        for node in &mut nodes {
            node.states.focusable = Some(node.role == "Edit");
        }

        let found = resolve_ids(
            &json!({"role": "Edit", "states": {"focusable": true}, "nth": 1}),
            &nodes,
        );

        assert_eq!(found, ["user_field"]);
    }

    #[test]
    fn a_positional_locator_survives_being_saved_and_reopened() {
        // The point of describing rather than referencing: a recording is a
        // file, and everything in it has to mean the same thing tomorrow.
        let nodes = form();
        let original = Locator::from_value(&json!({
            "role": "Edit",
            "near": {"anchor": {"name": "Username"}, "direction": "right", "within": 100},
            "nth": 1,
        }))
        .expect("must parse");

        let reloaded = Locator::from_value(&original.to_json()).expect("must re-parse");

        assert_eq!(
            reloaded.resolve(&nodes)[0].node_id,
            original.resolve(&nodes)[0].node_id
        );
    }

    #[test]
    fn a_malformed_position_is_rejected_with_a_usable_message() {
        // These are written by an agent from a person's words, so the error has
        // to say what was wrong rather than just failing to match.
        for (locator, expected) in [
            (json!({"role": "Button", "nth": "middle"}), "first"),
            (json!({"role": "Button", "nth": -1}), "whole number"),
            (json!({"role": "Button", "near": {}}), "anchor"),
            (
                json!({"role": "Button", "near": {"anchor": {"name": "x"}, "direction": "sideways"}}),
                "direction",
            ),
            (
                json!({"role": "Button", "near": {"anchor": {"name": "x"}, "within": 0}}),
                "positive",
            ),
        ] {
            let error = Locator::from_value(&locator).unwrap_err();
            assert!(error.contains(expected), "{locator} gave {error:?}");
        }
    }

    #[test]
    fn anchors_cannot_be_nested_without_bound() {
        // Each level multiplies the search, and a description this deep is
        // harder to follow than the attribute it replaces.
        let mut locator = json!({"name": "root"});
        for _ in 0..6 {
            locator = json!({"role": "Button", "near": {"anchor": locator}});
        }

        let error = Locator::from_value(&locator).unwrap_err();
        assert!(error.contains("nested"), "{error}");
    }

    #[test]
    fn a_positional_locator_is_unique_even_when_its_attributes_are_not() {
        // `unique_for` decides whether a step can be recorded. Judged on
        // attributes alone, "the first of three buttons" looks ambiguous --
        // which would reject exactly the locators positional syntax exists to
        // make possible.
        let nodes = form();
        let locator = Locator::from_value(&json!({"role": "Button", "nth": 1})).unwrap();
        let first = nodes.iter().find(|n| n.node_id == "submit").unwrap();

        assert!(locator.unique_for(first, &nodes));
    }

    #[test]
    fn a_target_requires_all_three_addressing_fields() {
        let complete = json!({"snapshot_id": "s1", "revision": 1, "node_id": "n1"});
        assert!(Target::from_value(&complete).is_ok());

        for incomplete in [
            json!({"revision": 1, "node_id": "n1"}),
            json!({"snapshot_id": "s1", "node_id": "n1"}),
            json!({"snapshot_id": "s1", "revision": 1}),
        ] {
            assert!(Target::from_value(&incomplete).is_err());
        }
    }

    #[test]
    fn a_target_round_trips_through_its_compact_reference() {
        let target = Target {
            snapshot_id: "abc123".into(),
            revision: 4,
            node_id: "e9".into(),
        };
        let reference = target.to_ref();

        assert_eq!(reference, "abc123:4:e9");
        assert_eq!(Target::parse_ref(&reference).unwrap(), target);
        // The same string is accepted wherever a target is expected.
        assert_eq!(Target::from_value(&json!(reference)).unwrap(), target);
    }

    #[test]
    fn a_compact_reference_needs_all_three_parts() {
        for malformed in ["abc123", "abc123:4", "abc123:4:e9:extra", ":4:e9", "abc123:4:"] {
            assert!(
                Target::parse_ref(malformed).is_err(),
                "{malformed:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_compact_reference_needs_a_numeric_revision() {
        let error = Target::parse_ref("abc123:latest:e9").unwrap_err();
        assert!(error.contains("revision"), "{error}");
    }

    #[test]
    fn surrounding_whitespace_in_a_reference_is_tolerated() {
        // Shells and copy-paste routinely add it.
        let target = Target::parse_ref("  abc123:4:e9\n").unwrap();
        assert_eq!(target.node_id, "e9");
    }

    #[test]
    fn a_snapshot_digest_changes_when_the_tree_changes() {
        let base = Snapshot {
            snapshot_id: "s1".into(),
            revision: 1,
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
            nodes: vec![node("n1", "Button", Some("Save"))],
            root_id: Some("n1".into()),
            captured_at: "2026-01-01T00:00:00.000Z".into(),
            truncated: false,
        };
        let mut changed = base.clone();
        changed.nodes = vec![node("n1", "Button", Some("Cancel"))];

        assert_eq!(base.digest(), base.clone().digest());
        assert_ne!(base.digest(), changed.digest());
    }

    #[test]
    fn an_outline_hides_offscreen_and_featureless_nodes() {
        let mut offscreen = node("n2", "Text", Some("hidden"));
        offscreen.states.offscreen = Some(true);
        let mut anonymous = node("n3", "Pane", None);
        anonymous.actions.clear();

        let snapshot = Snapshot {
            snapshot_id: "s1".into(),
            revision: 1,
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
            nodes: vec![node("n1", "Button", Some("Save")), offscreen, anonymous],
            root_id: Some("n1".into()),
            captured_at: "2026-01-01T00:00:00.000Z".into(),
            truncated: false,
        };

        let outline = snapshot.outline(50);
        assert_eq!(outline["shown"], 1);
        assert_eq!(outline["elements"][0]["node_id"], "n1");
        assert_eq!(outline["node_count"], 3);
    }

    #[test]
    fn a_node_summary_describes_it_for_an_agent() {
        let mut button = node("n1", "Button", Some("Save"));
        button.automation_id = Some("saveBtn".into());
        button.states.enabled = Some(false);

        let summary = button.summary();
        assert!(summary.contains("role=Button"), "{summary}");
        assert!(summary.contains("name=\"Save\""), "{summary}");
        assert!(summary.contains("automation_id=\"saveBtn\""), "{summary}");
        assert!(summary.contains("disabled"), "{summary}");
    }
}
