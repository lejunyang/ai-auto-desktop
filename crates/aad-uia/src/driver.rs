//! The UI Automation driver.
//!
//! The driver turns a [`Backend`] into a capability provider that the workflow
//! engine, the CLI and the MCP server all consume identically.
//!
//! Its central safety rule is the **snapshot handle discipline**: an agent can
//! only act on an element it has actually seen.  Every mutating action quotes a
//! snapshot id, a revision and a node id; the driver re-verifies that the live
//! element still matches before doing anything.  If the UI moved on, the action
//! is refused with `DRIVER.STALE_HANDLE` rather than applied to whatever now
//! occupies that position.

use crate::backend::{Backend, CaptureLimits, DriverError, Result, SnapshotStore};
use crate::model::{Locator, Node, Snapshot, Target, NODE_ACTIONS};
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::journal::now_rfc3339;
use aad_runtime::provider::Provider;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

pub const PROVIDER_NAME: &str = "desktop.windows_uia";

/// Actions that change the state of the desktop.
pub const WRITE_ACTIONS: &[&str] =
    &["focus", "invoke", "set_value", "type_text", "pointer_click"];

const MAX_TYPE_TEXT_CHARS: usize = 1024;
const MAX_CANDIDATE_SUMMARIES: usize = 10;

/// A UI Automation driver over some backend.
pub struct UiaDriver {
    backend: Arc<dyn Backend>,
    snapshots: SnapshotStore,
    manifest: CapabilityManifest,
}

impl UiaDriver {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self::with_store(backend, SnapshotStore::default())
    }

    /// Build a driver with a specific snapshot store.
    ///
    /// Useful for tests and for callers that want an isolated, non-persistent
    /// store rather than the shared one on disk.
    pub fn with_store(backend: Arc<dyn Backend>, snapshots: SnapshotStore) -> Self {
        let document = manifest::document(PROVIDER_NAME, action_contracts());
        Self {
            backend,
            snapshots,
            manifest: manifest::parse(&document).expect("the built-in manifest is valid"),
        }
    }

    pub fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// Dispatch one action by its short name.
    pub fn call(&self, action: &str, args: &Value) -> Result<Value> {
        let args = match args {
            Value::Null => Value::Object(Map::new()),
            other => other.clone(),
        };
        match action {
            "list_windows" => self.list_windows(&args),
            "snapshot" => self.snapshot(&args),
            "describe" => self.describe(&args),
            "find" => self.find(&args),
            "focus" | "invoke" | "set_value" | "type_text" | "pointer_click" => {
                self.act(action, &args)
            }
            other => Err(DriverError::invalid(format!("unknown action {other:?}"))),
        }
    }

    fn list_windows(&self, _args: &Value) -> Result<Value> {
        let windows = self.backend.list_windows()?;
        Ok(json!({
            "windows": windows.iter().map(crate::model::WindowInfo::to_json).collect::<Vec<_>>(),
            "count": windows.len(),
        }))
    }

    /// Capture a window's tree and retain it for later targeting.
    fn capture(&self, args: &Value) -> Result<Snapshot> {
        let window_id = args
            .get("window_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DriverError::invalid("window_id is required"))?;

        let limits = CaptureLimits {
            max_depth: args
                .get("max_depth")
                .and_then(Value::as_u64)
                .unwrap_or(CaptureLimits::default().max_depth as u64) as u32,
            max_nodes: args
                .get("max_nodes")
                .and_then(Value::as_u64)
                .unwrap_or(CaptureLimits::default().max_nodes as u64) as usize,
        }
        .clamp();

        let captured = self.backend.capture(window_id, limits)?;
        let snapshot = Snapshot {
            snapshot_id: uuid::Uuid::new_v4().simple().to_string(),
            revision: self.snapshots.next_revision(),
            window: captured.window,
            nodes: captured.nodes,
            root_id: captured.root_id,
            captured_at: now_rfc3339(),
            truncated: captured.truncated,
        };
        self.snapshots.insert(snapshot.clone());
        Ok(snapshot)
    }

    fn snapshot(&self, args: &Value) -> Result<Value> {
        Ok(self.capture(args)?.to_json())
    }

    /// A compact, agent-friendly description of a window.
    fn describe(&self, args: &Value) -> Result<Value> {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(80)
            .clamp(1, 500) as usize;
        Ok(self.capture(args)?.outline(limit))
    }

    fn find(&self, args: &Value) -> Result<Value> {
        let locator = args
            .get("locator")
            .ok_or_else(|| DriverError::invalid("locator is required"))
            .and_then(|value| Locator::from_value(value).map_err(DriverError::invalid))?;

        // Reuse an existing snapshot when one is quoted, so `find` can run
        // against exactly the tree the agent already reasoned about.
        let snapshot = match args.get("snapshot_id").and_then(Value::as_str) {
            Some(snapshot_id) => self.snapshots.get(snapshot_id).ok_or_else(|| {
                DriverError::stale(format!("snapshot {snapshot_id:?} is unknown or expired"))
            })?,
            None => self.capture(args)?,
        };

        let matches: Vec<&Node> = snapshot
            .nodes
            .iter()
            .filter(|node| locator.matches(node))
            .collect();

        // An ambiguous match is a genuine failure: acting on "the first one"
        // is how automation clicks the wrong button.
        if matches.len() > 1 && args.get("expect").and_then(Value::as_str) != Some("any") {
            return Err(DriverError::new(
                "DRIVER.AMBIGUOUS_MATCH",
                format!("the locator matched {} elements", matches.len()),
            )
            .with_detail("match_count", json!(matches.len()))
            .with_detail(
                "candidates",
                json!(matches
                    .iter()
                    .take(MAX_CANDIDATE_SUMMARIES)
                    .map(|node| json!({"node_id": node.node_id, "summary": node.summary()}))
                    .collect::<Vec<_>>()),
            ));
        }

        let Some(found) = matches.first() else {
            return Err(DriverError::new(
                "DRIVER.NOT_FOUND",
                "no element matched the locator",
            )
            .retryable()
            .with_detail("snapshot_id", json!(snapshot.snapshot_id))
            .with_detail("searched_nodes", json!(snapshot.nodes.len())));
        };

        let target = Target {
            snapshot_id: snapshot.snapshot_id.clone(),
            revision: snapshot.revision,
            node_id: found.node_id.clone(),
        };
        Ok(json!({
            "target": target.to_json(),
            // The compact form survives shell quoting, so it is what a caller
            // can paste straight into the next command.
            "ref": target.to_ref(),
            "node": found.to_json(),
            "match_count": matches.len(),
        }))
    }

    /// Resolve a target to a live, re-verified node.
    fn resolve(&self, target: &Target) -> Result<(String, Node)> {
        let snapshot = self.snapshots.get(&target.snapshot_id).ok_or_else(|| {
            DriverError::stale(format!(
                "snapshot {:?} is unknown or expired",
                target.snapshot_id
            ))
            .with_detail("snapshot_id", json!(target.snapshot_id))
        })?;

        // A revision mismatch means the agent is quoting an older view.
        if snapshot.revision != target.revision {
            return Err(DriverError::stale(
                "the snapshot has been superseded by a newer revision",
            )
            .with_detail("expected_revision", json!(snapshot.revision))
            .with_detail("provided_revision", json!(target.revision)));
        }

        let node = snapshot
            .find(&target.node_id)
            .ok_or_else(|| {
                DriverError::invalid(format!(
                    "node {:?} is not part of snapshot {:?}",
                    target.node_id, target.snapshot_id
                ))
            })?
            .clone();

        // Finally confirm against the live UI, not just our own record.
        if !self.backend.verify(&snapshot.window.window_id, &node)? {
            return Err(DriverError::stale(
                "the element no longer matches the captured snapshot",
            )
            .with_detail("node_id", json!(node.node_id))
            .with_detail("summary", json!(node.summary())));
        }
        Ok((snapshot.window.window_id.clone(), node))
    }

    fn act(&self, action: &str, args: &Value) -> Result<Value> {
        let target = args
            .get("target")
            .ok_or_else(|| DriverError::invalid("target is required"))
            .and_then(|value| Target::from_value(value).map_err(DriverError::invalid))?;
        let (window_id, node) = self.resolve(&target)?;

        // Refuse an action the element does not actually support, rather than
        // letting the backend fail in a less legible way.
        if !node.actions.iter().any(|name| name == action) {
            return Err(DriverError::new(
                "DRIVER.ACTION_UNSUPPORTED",
                format!("the element does not support {action:?}"),
            )
            .with_detail("node_id", json!(node.node_id))
            .with_detail("supported", json!(node.actions)));
        }

        match action {
            "focus" => self.backend.focus(&window_id, &node)?,
            "invoke" => self.backend.invoke(&window_id, &node)?,
            "pointer_click" => {
                if let Some(button) = args.get("button").and_then(Value::as_str) {
                    if button != "left" {
                        return Err(DriverError::invalid(
                            "only the left pointer button is supported",
                        ));
                    }
                }
                self.backend.pointer_click(&window_id, &node)?
            }
            "set_value" => {
                let value = args
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DriverError::invalid("value is required"))?;
                if node.states.read_only == Some(true) {
                    return Err(DriverError::invalid("the element is read-only"));
                }
                self.backend.set_value(&window_id, &node, value)?
            }
            "type_text" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DriverError::invalid("text is required"))?;
                if text.chars().count() > MAX_TYPE_TEXT_CHARS {
                    return Err(DriverError::invalid(format!(
                        "text exceeds {MAX_TYPE_TEXT_CHARS} characters"
                    )));
                }
                if node.states.read_only == Some(true) {
                    return Err(DriverError::invalid("the element is read-only"));
                }
                self.backend.type_text(&window_id, &node, text)?
            }
            other => return Err(DriverError::invalid(format!("unknown action {other:?}"))),
        }

        Ok(json!({
            "applied": true,
            "action": action,
            "node_id": node.node_id,
            "window_id": window_id,
        }))
    }
}

/// The action contracts this driver publishes in its manifest.
fn action_contracts() -> Map<String, Value> {
    let mut actions = Map::new();
    let read_only = |summary: &str| {
        json!({
            "contract_major": 1,
            "summary": summary,
            "effect": {"class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
        })
    };
    actions.insert(
        "list_windows".into(),
        read_only("List the top-level windows of running applications."),
    );
    actions.insert(
        "snapshot".into(),
        read_only("Capture the full element tree of one window."),
    );
    actions.insert(
        "describe".into(),
        read_only("Summarise a window's interactive elements for an agent."),
    );
    actions.insert(
        "find".into(),
        read_only("Find the single element matching a locator."),
    );

    let write = |summary: &str, category: &str, level: &str, class: &str| {
        json!({
            "contract_major": 1,
            "summary": summary,
            "effect": {"class": class},
            "risk": {"category": category, "level": level},
        })
    };
    actions.insert(
        "focus".into(),
        write("Give keyboard focus to an element.", "navigate", "low", "idempotent"),
    );
    actions.insert(
        "invoke".into(),
        write("Activate an element's default action.", "input", "medium", "non_idempotent"),
    );
    actions.insert(
        "set_value".into(),
        write("Replace an element's value.", "input", "medium", "idempotent"),
    );
    actions.insert(
        "type_text".into(),
        write("Type text into the focused element.", "input", "medium", "non_idempotent"),
    );
    actions.insert(
        "pointer_click".into(),
        write("Click the centre of an element.", "input", "medium", "non_idempotent"),
    );
    actions
}

impl Provider for UiaDriver {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    fn invoke(
        &self,
        action: &str,
        args: Value,
        _timeout: Option<Duration>,
    ) -> std::result::Result<Value, aad_core::AutomationError> {
        // Accept the fully qualified id as well as the bare action name.
        let short = action
            .strip_prefix(&format!("{PROVIDER_NAME}."))
            .unwrap_or(action);
        let short = short.split_once('@').map(|(head, _)| head).unwrap_or(short);

        self.call(short, &args)
            .map_err(DriverError::into_automation_error)
    }
}

/// The action ids this provider exposes, fully qualified.
pub fn action_ids() -> Vec<String> {
    let mut ids: Vec<String> = action_contracts()
        .keys()
        .map(|name| format!("{PROVIDER_NAME}.{name}@1"))
        .collect();
    ids.sort();
    ids
}

/// Whether an action changes desktop state.
pub fn is_write_action(action: &str) -> bool {
    WRITE_ACTIONS.contains(&action)
}

/// Whether an action operates on a single element.
pub fn is_node_action(action: &str) -> bool {
    NODE_ACTIONS.contains(&action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CapturedTree;
    use crate::model::{Bounds, States, WindowInfo};
    use std::sync::Mutex;

    /// A scripted backend that records what it was asked to do.
    struct FakeBackend {
        nodes: Mutex<Vec<Node>>,
        performed: Mutex<Vec<String>>,
        verify_result: Mutex<bool>,
    }

    impl FakeBackend {
        fn new(nodes: Vec<Node>) -> Arc<Self> {
            Arc::new(Self {
                nodes: Mutex::new(nodes),
                performed: Mutex::new(Vec::new()),
                verify_result: Mutex::new(true),
            })
        }

        fn performed(&self) -> Vec<String> {
            self.performed.lock().unwrap().clone()
        }

        fn set_verify(&self, value: bool) {
            *self.verify_result.lock().unwrap() = value;
        }

        fn record(&self, what: &str) {
            self.performed.lock().unwrap().push(what.to_string());
        }
    }

    impl Backend for FakeBackend {
        fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(vec![window()])
        }

        fn capture(&self, _window_id: &str, limits: CaptureLimits) -> Result<CapturedTree> {
            let nodes: Vec<Node> = self
                .nodes
                .lock()
                .unwrap()
                .iter()
                .filter(|node| node.depth <= limits.max_depth)
                .take(limits.max_nodes)
                .cloned()
                .collect();
            let total = self.nodes.lock().unwrap().len();
            Ok(CapturedTree {
                window: window(),
                root_id: nodes.first().map(|node| node.node_id.clone()),
                truncated: nodes.len() < total,
                nodes,
            })
        }

        fn verify(&self, _window_id: &str, _node: &Node) -> Result<bool> {
            Ok(*self.verify_result.lock().unwrap())
        }

        fn focus(&self, _window_id: &str, node: &Node) -> Result<()> {
            self.record(&format!("focus:{}", node.node_id));
            Ok(())
        }

        fn invoke(&self, _window_id: &str, node: &Node) -> Result<()> {
            self.record(&format!("invoke:{}", node.node_id));
            Ok(())
        }

        fn set_value(&self, _window_id: &str, node: &Node, value: &str) -> Result<()> {
            self.record(&format!("set_value:{}:{value}", node.node_id));
            Ok(())
        }

        fn type_text(&self, _window_id: &str, node: &Node, text: &str) -> Result<()> {
            self.record(&format!("type_text:{}:{text}", node.node_id));
            Ok(())
        }

        fn pointer_click(&self, _window_id: &str, node: &Node) -> Result<()> {
            self.record(&format!("pointer_click:{}", node.node_id));
            Ok(())
        }
    }

    fn window() -> WindowInfo {
        WindowInfo {
            window_id: "w1".into(),
            title: "Fixture".into(),
            process_id: 42,
            process_name: Some("fixture.exe".into()),
            class_name: None,
            bounds: Some(Bounds { x: 0, y: 0, width: 800, height: 600 }),
            is_foreground: true,
            is_minimized: false,
        }
    }

    fn node(id: &str, role: &str, name: &str, actions: &[&str]) -> Node {
        Node {
            node_id: id.into(),
            role: role.into(),
            name: Some(name.into()),
            value: None,
            automation_id: None,
            class_name: None,
            framework_id: None,
            bounds: Some(Bounds { x: 10, y: 10, width: 80, height: 24 }),
            states: States { enabled: Some(true), ..Default::default() },
            actions: actions.iter().map(|value| value.to_string()).collect(),
            depth: 1,
            parent_id: None,
            children: Vec::new(),
        }
    }

    fn driver(nodes: Vec<Node>) -> (UiaDriver, Arc<FakeBackend>) {
        let backend = FakeBackend::new(nodes);
        // An in-memory store keeps these tests independent of the shared
        // on-disk cache and of each other.
        let store = SnapshotStore::new(16, std::time::Duration::from_secs(300));
        (UiaDriver::with_store(backend.clone(), store), backend)
    }

    fn target_of(found: &Value) -> Value {
        found["target"].clone()
    }

    #[test]
    fn list_windows_returns_the_backend_windows() {
        let (driver, _) = driver(vec![]);
        let result = driver.call("list_windows", &json!({})).unwrap();

        assert_eq!(result["count"], 1);
        assert_eq!(result["windows"][0]["title"], "Fixture");
    }

    #[test]
    fn a_snapshot_carries_an_id_revision_and_nodes() {
        let (driver, _) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);
        let snapshot = driver.call("snapshot", &json!({"window_id": "w1"})).unwrap();

        assert!(snapshot["snapshot_id"].as_str().is_some());
        assert_eq!(snapshot["revision"], 1);
        assert_eq!(snapshot["node_count"], 1);
        assert!(snapshot["digest"].as_str().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn each_snapshot_gets_a_fresh_revision() {
        let (driver, _) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);
        let first = driver.call("snapshot", &json!({"window_id": "w1"})).unwrap();
        let second = driver.call("snapshot", &json!({"window_id": "w1"})).unwrap();

        assert_ne!(first["snapshot_id"], second["snapshot_id"]);
        assert!(second["revision"].as_u64() > first["revision"].as_u64());
    }

    #[test]
    fn snapshot_requires_a_window_id() {
        let (driver, _) = driver(vec![]);
        let error = driver.call("snapshot", &json!({})).unwrap_err();

        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
    }

    #[test]
    fn describe_produces_a_compact_outline() {
        let (driver, _) = driver(vec![
            node("n1", "Button", "Save", &["invoke"]),
            node("n2", "Button", "Cancel", &["invoke"]),
        ]);

        let outline = driver.call("describe", &json!({"window_id": "w1"})).unwrap();

        assert_eq!(outline["shown"], 2);
        assert!(outline["elements"][0]["summary"].as_str().unwrap().contains("Save"));
        // The outline must stay small: no full node payloads.
        assert!(outline["elements"][0].get("children").is_none());
    }

    #[test]
    fn find_locates_a_unique_element_and_returns_a_target() {
        let (driver, _) = driver(vec![
            node("n1", "Button", "Save", &["invoke"]),
            node("n2", "Button", "Cancel", &["invoke"]),
        ]);

        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .unwrap();

        assert_eq!(found["target"]["node_id"], "n1");
        assert_eq!(found["match_count"], 1);
    }

    #[test]
    fn find_refuses_an_ambiguous_locator() {
        let (driver, _) = driver(vec![
            node("n1", "Button", "Save", &["invoke"]),
            node("n2", "Button", "Save", &["invoke"]),
        ]);

        let error = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.AMBIGUOUS_MATCH");
        assert_eq!(error.details["match_count"], json!(2));
        // The candidates must be described so an agent can disambiguate.
        assert_eq!(error.details["candidates"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn find_reports_a_retryable_miss_when_nothing_matches() {
        let (driver, _) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);

        let error = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Nope"}}))
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.NOT_FOUND");
        // The element may simply not have appeared yet.
        assert!(error.retryable);
    }

    #[test]
    fn an_action_applies_to_the_addressed_element() {
        let (driver, backend) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .unwrap();

        let result = driver
            .call("invoke", &json!({"target": target_of(&found)}))
            .unwrap();

        assert_eq!(result["applied"], true);
        assert_eq!(backend.performed(), vec!["invoke:n1".to_string()]);
    }

    #[test]
    fn an_action_is_refused_when_the_element_no_longer_matches() {
        let (driver, backend) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .unwrap();

        // The UI moves on between the snapshot and the action.
        backend.set_verify(false);
        let error = driver
            .call("invoke", &json!({"target": target_of(&found)}))
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.STALE_HANDLE");
        assert!(
            backend.performed().is_empty(),
            "nothing may be clicked once the handle is known to be stale"
        );
    }

    #[test]
    fn an_action_is_refused_when_the_revision_is_superseded() {
        let (driver, backend) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .unwrap();
        let mut stale = target_of(&found);
        stale["revision"] = json!(stale["revision"].as_u64().unwrap() - 1);

        let error = driver.call("invoke", &json!({"target": stale})).unwrap_err();

        assert_eq!(error.code, "DRIVER.STALE_HANDLE");
        assert!(backend.performed().is_empty());
    }

    #[test]
    fn an_action_on_an_unknown_snapshot_is_refused() {
        let (driver, _) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);

        let error = driver
            .call(
                "invoke",
                &json!({"target": {"snapshot_id": "nope", "revision": 1, "node_id": "n1"}}),
            )
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.STALE_HANDLE");
    }

    #[test]
    fn an_unsupported_action_is_refused_before_reaching_the_backend() {
        let (driver, backend) = driver(vec![node("n1", "Text", "Label", &[])]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Label"}}))
            .unwrap();

        let error = driver
            .call("invoke", &json!({"target": target_of(&found)}))
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.ACTION_UNSUPPORTED");
        assert!(backend.performed().is_empty());
    }

    #[test]
    fn set_value_passes_the_value_through() {
        let (driver, backend) = driver(vec![node("n1", "Edit", "Name", &["set_value"])]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Name"}}))
            .unwrap();

        driver
            .call(
                "set_value",
                &json!({"target": target_of(&found), "value": "Ada"}),
            )
            .unwrap();

        assert_eq!(backend.performed(), vec!["set_value:n1:Ada".to_string()]);
    }

    #[test]
    fn writing_to_a_read_only_element_is_refused() {
        let mut field = node("n1", "Edit", "Name", &["set_value"]);
        field.states.read_only = Some(true);
        let (driver, backend) = driver(vec![field]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Name"}}))
            .unwrap();

        let error = driver
            .call("set_value", &json!({"target": target_of(&found), "value": "x"}))
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
        assert!(backend.performed().is_empty());
    }

    #[test]
    fn type_text_is_bounded() {
        let (driver, _) = driver(vec![node("n1", "Edit", "Name", &["type_text"])]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Name"}}))
            .unwrap();

        let error = driver
            .call(
                "type_text",
                &json!({"target": target_of(&found), "text": "x".repeat(2000)}),
            )
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
    }

    #[test]
    fn only_the_left_pointer_button_is_supported() {
        let (driver, _) = driver(vec![node("n1", "Button", "Save", &["pointer_click"])]);
        let found = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .unwrap();

        let error = driver
            .call(
                "pointer_click",
                &json!({"target": target_of(&found), "button": "right"}),
            )
            .unwrap_err();

        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
    }

    #[test]
    fn capture_limits_bound_the_returned_tree() {
        let nodes: Vec<Node> = (0..50)
            .map(|index| node(&format!("n{index}"), "Button", "Item", &["invoke"]))
            .collect();
        let (driver, _) = driver(nodes);

        let snapshot = driver
            .call("snapshot", &json!({"window_id": "w1", "max_nodes": 10}))
            .unwrap();

        assert_eq!(snapshot["node_count"], 10);
        assert_eq!(snapshot["truncated"], true);
    }

    #[test]
    fn find_can_reuse_an_existing_snapshot() {
        let (driver, _) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);
        let snapshot = driver.call("snapshot", &json!({"window_id": "w1"})).unwrap();
        let snapshot_id = snapshot["snapshot_id"].as_str().unwrap();

        let found = driver
            .call(
                "find",
                &json!({"snapshot_id": snapshot_id, "locator": {"name": "Save"}}),
            )
            .unwrap();

        assert_eq!(found["target"]["snapshot_id"], snapshot_id);
        assert_eq!(found["target"]["revision"], snapshot["revision"]);
    }

    #[test]
    fn the_manifest_publishes_every_action_with_its_risk() {
        let (driver, _) = driver(vec![]);
        let manifest = driver.manifest();

        for action in ["list_windows", "snapshot", "describe", "find"] {
            let contract = manifest.actions.get(action).expect(action);
            assert_eq!(contract.effect_class.as_deref(), Some("read_only"));
        }
        assert_eq!(
            manifest.actions["invoke"].effect_class.as_deref(),
            Some("non_idempotent")
        );
        assert_eq!(
            manifest.actions["invoke"].risk_level.as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn the_provider_interface_accepts_qualified_action_ids() {
        let (driver, _) = driver(vec![node("n1", "Button", "Save", &["invoke"])]);

        let result = Provider::invoke(
            &driver,
            "desktop.windows_uia.list_windows@1",
            json!({}),
            None,
        )
        .expect("a qualified id should dispatch");

        assert_eq!(result["count"], 1);
    }

    #[test]
    fn write_actions_are_classified_correctly() {
        assert!(is_write_action("invoke"));
        assert!(is_write_action("type_text"));
        assert!(!is_write_action("snapshot"));
        assert!(!is_write_action("find"));
    }

    #[test]
    fn action_ids_are_fully_qualified_and_sorted() {
        let ids = action_ids();
        assert!(ids.contains(&"desktop.windows_uia.snapshot@1".to_string()));
        assert!(ids.contains(&"desktop.windows_uia.invoke@1".to_string()));
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }
}
