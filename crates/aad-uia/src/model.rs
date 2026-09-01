//! The platform-independent UI model.
//!
//! Everything the driver exposes to callers is expressed with these types.
//! Keeping them free of Windows specifics means the selector logic, the
//! snapshot handle discipline and the JSON shaping are all testable on any
//! platform, and a future macOS or Linux backend can reuse them unchanged.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
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
                    "depth": node.depth,
                    "summary": node.summary(),
                    "actions": node.actions,
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

/// A declarative element selector.
///
/// Every populated field must match, so adding a field always narrows the
/// result.  Matching is exact by default; `contains` is opt-in per query
/// because a substring match that silently picks a different button is exactly
/// the failure mode this driver exists to prevent.
#[derive(Clone, Debug, Default, Deserialize)]
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
}

impl Locator {
    pub fn from_value(value: &Value) -> Result<Self, String> {
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
