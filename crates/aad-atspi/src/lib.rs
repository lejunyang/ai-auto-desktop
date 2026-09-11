//! Native Linux AT-SPI capability provider.
//!
//! The stateful driver core is platform-neutral and accepts an injected backend
//! for contract tests. On Linux, [`native_provider`] connects directly to the
//! accessibility D-Bus with pure-Rust `zbus`; no Python worker is involved.

use aad_core::AutomationError;
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::{ArtifactStore, Provider};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PROVIDER_NAME: &str = "desktop.linux_atspi";
pub const PROVIDER_VERSION: &str = "0.1.0";
const MAX_FIELD_CHARS: usize = 4096;
const MAX_TYPE_TEXT_CHARS: usize = 1024;
const MAX_TYPE_TEXT_BYTES: usize = 4096;
const DEFAULT_MAX_DEPTH: u32 = 32;
const DEFAULT_MAX_NODES: usize = 1000;
const MAX_DEPTH: u32 = 128;
const MAX_NODES: usize = 5000;
const MAX_CANDIDATE_SUMMARIES: usize = 10;
const WRITE_ACTIONS: &[&str] = &[
    "collapse",
    "expand",
    "focus",
    "invoke",
    "pointer_click",
    "set_text",
    "toggle",
    "type_text",
];
const STATE_NAMES: &[&str] = &[
    "enabled",
    "visible",
    "showing",
    "focusable",
    "focused",
    "editable",
    "sensitive",
    "protected",
    "checked",
    "expandable",
    "expanded",
    "selectable",
    "selected",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeRef {
    pub bus_name: String,
    pub object_path: String,
}

#[derive(Clone, Debug)]
pub struct BackendNode {
    pub native: NativeRef,
    pub parent_index: Option<usize>,
    pub role: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub attributes: BTreeMap<String, String>,
    pub states: BTreeMap<String, Option<bool>>,
    pub bounds: Option<Bounds>,
    pub actions: Vec<String>,
    pub provenance: Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct BackendSnapshot {
    pub application: Map<String, Value>,
    pub nodes: Vec<BackendNode>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    fn to_json(self) -> Value {
        json!({"x": self.x, "y": self.y, "width": self.width, "height": self.height})
    }
}

pub trait AtspiBackend: Send + Sync {
    fn name(&self) -> &str;
    fn session_info(&self) -> Map<String, Value>;
    fn list_applications(
        &self,
        deadline: Instant,
    ) -> Result<Vec<Map<String, Value>>, AutomationError>;
    fn capture(
        &self,
        application: &Map<String, Value>,
        max_depth: u32,
        max_nodes: usize,
        deadline: Instant,
    ) -> Result<BackendSnapshot, AutomationError>;
    fn focus(&self, target: &NativeRef, deadline: Instant) -> Result<Value, AutomationError>;
    fn invoke(&self, target: &NativeRef, deadline: Instant) -> Result<Value, AutomationError>;
    fn set_text(
        &self,
        target: &NativeRef,
        text: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError>;
    fn named_action(
        &self,
        target: &NativeRef,
        name: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError>;
    fn type_text(
        &self,
        _target: &NativeRef,
        _text: &str,
        _process_id: u32,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "native type_text helper is unavailable",
        ))
    }
    fn pointer_click(
        &self,
        _target: &NativeRef,
        _point: (i32, i32),
        _process_id: u32,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "native pointer_click helper is unavailable",
        ))
    }
    fn accessible_at_point(
        &self,
        _root: &NativeRef,
        _point: (i32, i32),
        _deadline: Instant,
    ) -> Result<Option<NativeRef>, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "AT-SPI point lookup is unavailable",
        ))
    }
    fn capture_target(
        &self,
        _target: &NativeRef,
        _bounds: Bounds,
        _process_id: u32,
        _deadline: Instant,
    ) -> Result<(Vec<u8>, Map<String, Value>), AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "native capture_target helper is unavailable",
        ))
    }
}

#[derive(Clone)]
struct Record {
    public: Value,
    handles: BTreeMap<String, NativeRef>,
    fingerprints: BTreeMap<String, String>,
    application_selector: Map<String, Value>,
    max_depth: u32,
    max_nodes: usize,
}

#[derive(Default)]
struct DriverState {
    revision: u64,
    current: Option<Record>,
}

pub struct AtspiProvider {
    backend: Arc<dyn AtspiBackend>,
    manifest: CapabilityManifest,
    generation: String,
    state: Mutex<DriverState>,
}

impl AtspiProvider {
    pub fn new(backend: Arc<dyn AtspiBackend>) -> Result<Self, AutomationError> {
        Ok(Self {
            backend,
            manifest: manifest::parse(&manifest_document()).map_err(|reason| {
                driver_error(
                    "DRIVER.INTERNAL",
                    format!("built-in AT-SPI manifest is invalid: {reason}"),
                )
            })?,
            generation: uuid::Uuid::new_v4().simple().to_string(),
            state: Mutex::new(DriverState::default()),
        })
    }

    fn deadline(timeout: Option<Duration>) -> Result<Instant, AutomationError> {
        let duration = timeout.unwrap_or(Duration::from_secs(30));
        if duration.is_zero() {
            return Err(timeout_error(false));
        }
        Ok(Instant::now() + duration)
    }

    fn call(
        &self,
        action: &str,
        args: Value,
        deadline: Instant,
        artifacts: Option<&ArtifactStore>,
    ) -> Result<Value, AutomationError> {
        remaining(deadline, false)?;
        let short = action
            .strip_prefix(PROVIDER_NAME)
            .and_then(|value| value.strip_prefix('.'))
            .and_then(|value| value.strip_suffix("@1"))
            .ok_or_else(|| invalid("unknown AT-SPI action"))?;
        let args = args
            .as_object()
            .ok_or_else(|| invalid("args must be an object"))?;
        match short {
            "inspect_session" => {
                only_keys(args, &[])?;
                let session = self.backend.session_info();
                Ok(json!({
                    "backend": self.backend.name(),
                    "session_type": bounded_optional_string(session.get("session_type")),
                    "desktop": bounded_optional_string(session.get("desktop")),
                }))
            }
            "list_applications" => {
                only_keys(args, &[])?;
                Ok(json!({
                    "session": self.backend.session_info(),
                    "backend": self.backend.name(),
                    "applications": self.backend.list_applications(deadline)?,
                }))
            }
            "snapshot" => self.snapshot(args, deadline),
            "find" => self.find(args, deadline),
            "capture_target" => self.capture_target(args, deadline, artifacts),
            action if WRITE_ACTIONS.contains(&action) => self.write(action, args, deadline),
            _ => Err(invalid("unknown AT-SPI action")),
        }
    }

    fn snapshot(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        only_keys(args, &["application", "max_depth", "max_nodes"])?;
        let selector = application_selector(args.get("application"))?;
        let max_depth = bounded_integer(
            args.get("max_depth"),
            DEFAULT_MAX_DEPTH as u64,
            0,
            MAX_DEPTH as u64,
            "max_depth",
        )? as u32;
        let max_nodes = bounded_integer(
            args.get("max_nodes"),
            DEFAULT_MAX_NODES as u64,
            1,
            MAX_NODES as u64,
            "max_nodes",
        )? as usize;
        let record = self.capture(&selector, max_depth, max_nodes, deadline)?;
        Ok(record.public)
    }

    fn capture(
        &self,
        selector: &Map<String, Value>,
        max_depth: u32,
        max_nodes: usize,
        deadline: Instant,
    ) -> Result<Record, AutomationError> {
        let raw = self
            .backend
            .capture(selector, max_depth, max_nodes, deadline)?;
        if raw.nodes.len() > max_nodes {
            return Err(driver_error(
                "DRIVER.ACTION_FAILED",
                "backend exceeded the requested node limit",
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| driver_error("DRIVER.ACTION_FAILED", "driver state is unavailable"))?;
        state.revision += 1;
        let revision = state.revision;
        let snapshot_id = format!("{}:{revision}", self.generation);
        let mut nodes = Vec::with_capacity(raw.nodes.len());
        let mut handles = BTreeMap::new();
        let mut fingerprints = BTreeMap::new();
        for (index, item) in raw.nodes.into_iter().enumerate() {
            remaining(deadline, false)?;
            let parent_id = item
                .parent_index
                .map(|parent| {
                    if parent >= index {
                        Err(driver_error(
                            "DRIVER.ACTION_FAILED",
                            "backend returned an invalid parent relationship",
                        ))
                    } else {
                        Ok(format!("n{parent}"))
                    }
                })
                .transpose()?;
            let node_id = format!("n{index}");
            let role = normalize_role(&item.role);
            let mut states = Map::new();
            for name in STATE_NAMES {
                states.insert(
                    (*name).into(),
                    item.states
                        .get(*name)
                        .copied()
                        .flatten()
                        .map(Value::Bool)
                        .unwrap_or(Value::Null),
                );
            }
            let mut provenance = item.provenance;
            provenance.insert("backend".into(), json!(self.backend.name()));
            let protected = states.get("protected") == Some(&Value::Bool(true))
                || provenance.get("value_redacted") == Some(&Value::Bool(true))
                || matches!(role.as_str(), "password_text" | "password");
            if protected {
                states.insert("protected".into(), Value::Bool(true));
                provenance.insert("value_redacted".into(), Value::Bool(true));
            }
            let actions = item
                .actions
                .into_iter()
                .filter(|action| WRITE_ACTIONS.contains(&action.as_str()))
                .filter(|action| {
                    !protected
                        || !matches!(action.as_str(), "pointer_click" | "set_text" | "type_text")
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let node = json!({
                "node_id": node_id,
                "parent_id": parent_id,
                "role": role,
                "name": bounded_string(item.name),
                "description": bounded_string(item.description),
                "value": if protected { Value::Null } else { bounded_string(item.value) },
                "attributes": item.attributes,
                "states": states,
                "bounds": item.bounds.map(Bounds::to_json),
                "actions": actions,
                "provenance": provenance,
            });
            handles.insert(node_id.clone(), item.native);
            fingerprints.insert(node_id, fingerprint(&node));
            nodes.push(node);
        }
        let public = json!({
            "snapshot_id": snapshot_id,
            "revision": revision,
            "session": self.backend.session_info(),
            "backend": self.backend.name(),
            "application": raw.application,
            "nodes": nodes,
            "truncated": raw.truncated,
        });
        let record = Record {
            public,
            handles,
            fingerprints,
            application_selector: selector.clone(),
            max_depth,
            max_nodes,
        };
        state.current = Some(record.clone());
        Ok(record)
    }

    fn current_record(
        &self,
        snapshot_id: &Value,
        revision: &Value,
    ) -> Result<Record, AutomationError> {
        let snapshot_id = required_string(snapshot_id, "snapshot_id")?;
        let revision = revision
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("revision must be a positive integer"))?;
        let state = self
            .state
            .lock()
            .map_err(|_| driver_error("DRIVER.ACTION_FAILED", "driver state is unavailable"))?;
        let record = state
            .current
            .as_ref()
            .ok_or_else(|| stale("snapshot is not the current driver version"))?;
        if record.public["snapshot_id"] != snapshot_id || record.public["revision"] != revision {
            return Err(stale("snapshot is not the current driver version"));
        }
        Ok(record.clone())
    }

    fn find(&self, args: &Map<String, Value>, deadline: Instant) -> Result<Value, AutomationError> {
        only_keys(args, &["snapshot_id", "revision", "locator"])?;
        let record = self.current_record(
            args.get("snapshot_id").unwrap_or(&Value::Null),
            args.get("revision").unwrap_or(&Value::Null),
        )?;
        if record.public["truncated"] == true {
            return Err(driver_error(
                "DRIVER.SNAPSHOT_TRUNCATED",
                "a truncated snapshot cannot prove a unique match",
            ));
        }
        let locator = parse_locator(args.get("locator"))?;
        let node = resolve(&record, &locator, deadline)?;
        let node_id = node["node_id"].as_str().expect("normalized node id");
        Ok(json!({"target": target(&record, node_id), "node": node}))
    }

    fn fresh_target(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<(Record, String, Value, NativeRef), AutomationError> {
        let target_value = args
            .get("target")
            .ok_or_else(|| invalid("target is required"))?;
        let target_object = target_value
            .as_object()
            .ok_or_else(|| invalid("target must be an object"))?;
        only_keys(target_object, &["snapshot_id", "revision", "node_id"])?;
        let record = self.current_record(
            target_object.get("snapshot_id").unwrap_or(&Value::Null),
            target_object.get("revision").unwrap_or(&Value::Null),
        )?;
        if record.public["truncated"] == true {
            return Err(driver_error(
                "DRIVER.SNAPSHOT_TRUNCATED",
                "a truncated snapshot cannot be used for an action",
            ));
        }
        let node_id = required_string(
            target_object.get("node_id").unwrap_or(&Value::Null),
            "target.node_id",
        )?
        .to_string();
        let locator = parse_locator(args.get("locator"))?;
        let expected = resolve(&record, &locator, deadline)?;
        if expected["node_id"] != node_id || !record.handles.contains_key(&node_id) {
            return Err(stale("target does not match the snapshot locator result"));
        }
        let expected_fingerprint = record.fingerprints[&node_id].clone();
        let fresh = self.capture(
            &record.application_selector,
            record.max_depth,
            record.max_nodes,
            deadline,
        )?;
        if fresh.public["truncated"] == true {
            return Err(driver_error(
                "DRIVER.SNAPSHOT_TRUNCATED",
                "fresh action snapshot is truncated",
            ));
        }
        let resolved = resolve(&fresh, &locator, deadline).map_err(|error| {
            if matches!(error.code.as_str(), "DRIVER.NOT_FOUND" | "DRIVER.AMBIGUOUS") {
                stale("locator no longer resolves to its original unique target").with_cause(error)
            } else {
                error
            }
        })?;
        let fresh_id = resolved["node_id"]
            .as_str()
            .expect("normalized node id")
            .to_string();
        let previous_native = &record.handles[&node_id];
        let fresh_native = fresh.handles[&fresh_id].clone();
        if previous_native != &fresh_native || fresh.fingerprints[&fresh_id] != expected_fingerprint
        {
            return Err(stale(
                "locator resolved to a different native or semantic target",
            ));
        }
        Ok((fresh, fresh_id, resolved, fresh_native))
    }

    fn write(
        &self,
        action: &str,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let mut allowed = vec!["target", "locator"];
        if matches!(action, "set_text" | "type_text") {
            allowed.push("text");
        }
        if action == "pointer_click" {
            allowed.extend(["button", "position"]);
        }
        only_keys(args, &allowed)?;
        if action == "pointer_click"
            && (args.get("button").is_some_and(|value| value != "left")
                || args.get("position").is_some_and(|value| value != "center"))
        {
            return Err(invalid("pointer_click supports only left/center"));
        }
        let (fresh, fresh_id, node, native) = self.fresh_target(args, deadline)?;
        let available = node["actions"].as_array().expect("normalized action array");
        if !available.iter().any(|value| value == action) {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                format!("target does not support native {action}"),
            ));
        }
        let states = node["states"].as_object().expect("normalized states");
        if node["provenance"]["value_redacted"] == true
            || states.get("protected") == Some(&Value::Bool(true))
        {
            return Err(driver_error(
                "DRIVER.PROTECTED_ELEMENT",
                format!("protected element forbids {action}"),
            ));
        }
        if action == "toggle" && !states.get("checked").is_some_and(Value::is_boolean) {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "target has no observable checked state",
            ));
        }
        if matches!(action, "expand" | "collapse") {
            let expanded = states.get("expanded").and_then(Value::as_bool);
            if states.get("expandable") != Some(&Value::Bool(true)) || expanded.is_none() {
                return Err(driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "target has no verifiable expanded state",
                ));
            }
            let desired = action == "expand";
            if expanded == Some(desired) {
                return Ok(json!({
                    "ok": true, "action": action, "resolved": target(&fresh, &fresh_id),
                    "backend_result": {"native_interface": "Action.do_action", "dispatched": false, "no_op": true, "observed_state": {"expanded": desired}}
                }));
            }
        }
        remaining(deadline, false)?;
        let mut dispatched = false;
        let result = (|| {
            dispatched = true;
            match action {
                "focus" => self.backend.focus(&native, deadline),
                "invoke" => self.backend.invoke(&native, deadline),
                "set_text" => {
                    let text = bounded_text(args.get("text"), false)?;
                    self.backend.set_text(&native, text, deadline)
                }
                "type_text" => {
                    let text = ordinary_text(args.get("text"))?;
                    let process = process_id(&node, &fresh)?;
                    self.backend.type_text(&native, text, process, deadline)
                }
                "pointer_click" => {
                    let process = process_id(&node, &fresh)?;
                    let bounds = parse_bounds(node.get("bounds"))?;
                    if bounds.width <= 0 || bounds.height <= 0 {
                        return Err(driver_error(
                            "DRIVER.ACTION_UNSUPPORTED",
                            "target has no positive-area bounds",
                        ));
                    }
                    let point = (bounds.x + bounds.width / 2, bounds.y + bounds.height / 2);
                    self.validate_point(&fresh, &fresh_id, point, deadline)?;
                    self.backend
                        .pointer_click(&native, point, process, deadline)
                }
                "toggle" => self.backend.named_action(&native, "click", deadline),
                "expand" | "collapse" => self.backend.named_action(&native, "activate", deadline),
                _ => unreachable!(),
            }
        })();
        match result {
            Ok(backend_result) => {
                remaining(deadline, true)?;
                if let Ok(mut state) = self.state.lock() {
                    state.current = None;
                }
                Ok(
                    json!({"ok": true, "action": action, "resolved": target(&fresh, &fresh_id), "backend_result": backend_result}),
                )
            }
            Err(error)
                if dispatched
                    && matches!(
                        error.code.as_str(),
                        "DRIVER.ACTION_FAILED" | "DRIVER.TIMEOUT"
                    ) =>
            {
                if let Ok(mut state) = self.state.lock() {
                    state.current = None;
                }
                Err(driver_error(
                    "DRIVER.UNKNOWN_EFFECT",
                    "native action result is unknown after dispatch",
                )
                .with_effect("unknown")
                .with_cause(error))
            }
            Err(error) => Err(error),
        }
    }

    fn capture_target(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
        artifacts: Option<&ArtifactStore>,
    ) -> Result<Value, AutomationError> {
        only_keys(args, &["target", "locator", "format"])?;
        if args.get("format") != Some(&json!("png")) {
            return Err(invalid("capture_target.format must be png"));
        }
        let (fresh, fresh_id, node, native) = self.fresh_target(args, deadline)?;
        let artifacts = artifacts.ok_or_else(|| {
            driver_error(
                "DRIVER.ARTIFACT_IPC",
                "capture_target requires a Host artifact store",
            )
        })?;
        let states = node["states"].as_object().expect("normalized states");
        if !["visible", "showing"]
            .iter()
            .all(|name| states.get(*name) == Some(&Value::Bool(true)))
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "capture_target requires a visible, showing target",
            ));
        }
        let descendants = descendants(&fresh, &fresh_id);
        let unsafe_node = fresh.public["nodes"]
            .as_array()
            .expect("normalized nodes")
            .iter()
            .find(|candidate| {
                candidate["node_id"]
                    .as_str()
                    .is_some_and(|id| descendants.contains(id))
                    && (candidate["provenance"]["value_redacted"] != false
                        || candidate["states"]["protected"] != false
                        || matches!(
                            candidate["role"].as_str(),
                            Some("password_text" | "password")
                        ))
            });
        if unsafe_node.is_some() {
            return Err(driver_error(
                "DRIVER.PROTECTED_ELEMENT",
                "capture target is not proven free of protected content",
            ));
        }
        let bounds = parse_bounds(node.get("bounds"))?;
        if bounds.width <= 0 || bounds.height <= 0 {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "capture target has no positive-area bounds",
            ));
        }
        self.validate_point(
            &fresh,
            &fresh_id,
            (bounds.x + bounds.width / 2, bounds.y + bounds.height / 2),
            deadline,
        )?;
        for candidate in fresh.public["nodes"].as_array().expect("normalized nodes") {
            let id = candidate["node_id"].as_str().unwrap_or_default();
            if descendants.contains(id) || ancestors(&fresh, &fresh_id).contains(id) {
                continue;
            }
            if let Ok(candidate_bounds) = parse_bounds(candidate.get("bounds")) {
                let visible = candidate["states"]["visible"] != false
                    && candidate["states"]["showing"] != false;
                if visible && overlaps(bounds, candidate_bounds) {
                    return Err(driver_error(
                        "DRIVER.ACTION_UNSUPPORTED",
                        "capture target overlaps a visible node outside its subtree",
                    )
                    .with_detail("overlapping_node_id", json!(id)));
                }
            }
        }
        let process = process_id(&node, &fresh)?;
        let (png, mut provenance) = self
            .backend
            .capture_target(&native, bounds, process, deadline)?;
        let reference = artifacts
            .import_bytes(&png, Some("image/png"))
            .map_err(|cause| {
                driver_error("DRIVER.ARTIFACT_IPC", "Host artifact import failed").with_cause(cause)
            })?;
        provenance.insert("snapshot_id".into(), fresh.public["snapshot_id"].clone());
        provenance.insert("revision".into(), fresh.public["revision"].clone());
        provenance.insert("node_id".into(), json!(fresh_id));
        provenance.insert("application_process_id".into(), json!(process));
        Ok(json!({"frame": reference.to_value(), "provenance": provenance}))
    }

    fn validate_point(
        &self,
        record: &Record,
        node_id: &str,
        point: (i32, i32),
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        let allowed = descendants(record, node_id);
        let mut hit = None;
        for ancestor in ancestors(record, node_id).into_iter().rev() {
            if let Some(native) = record.handles.get(&ancestor) {
                match self.backend.accessible_at_point(native, point, deadline) {
                    Ok(Some(found)) => {
                        hit = Some(found);
                        break;
                    }
                    Ok(None) => {}
                    Err(error) if error.code == "DRIVER.ACTION_UNSUPPORTED" => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let hit = hit.ok_or_else(|| {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "AT-SPI could not prove the target at its center point",
            )
        })?;
        if allowed
            .iter()
            .filter_map(|id| record.handles.get(id))
            .any(|native| native == &hit)
        {
            Ok(())
        } else {
            Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "center point did not hit the fresh target subtree",
            ))
        }
    }
}

impl Provider for AtspiProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    fn invoke(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AutomationError> {
        self.call(action, args, Self::deadline(timeout)?, None)
    }

    fn invoke_with_artifacts(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
        artifacts: &ArtifactStore,
    ) -> Result<Value, AutomationError> {
        self.call(action, args, Self::deadline(timeout)?, Some(artifacts))
    }
}

pub fn manifest_json() -> Value {
    manifest_document()
}

#[cfg(target_os = "linux")]
pub fn native_provider() -> Result<AtspiProvider, AutomationError> {
    let backend: Arc<dyn AtspiBackend> = match linux::LinuxBackend::new() {
        Ok(backend) => Arc::new(backend),
        Err(error) => Arc::new(UnavailableBackend { error }),
    };
    AtspiProvider::new(backend)
}

#[cfg(not(target_os = "linux"))]
pub fn native_provider() -> Result<AtspiProvider, AutomationError> {
    Err(driver_error(
        "DRIVER.UNAVAILABLE",
        "AT-SPI is only available on Linux",
    ))
}

pub fn register(registry: &mut aad_runtime::ProviderRegistry) -> Result<(), AutomationError> {
    registry.insert(Arc::new(native_provider()?));
    Ok(())
}

#[cfg(target_os = "linux")]
struct UnavailableBackend {
    error: AutomationError,
}

#[cfg(target_os = "linux")]
impl UnavailableBackend {
    fn unavailable<T>(&self) -> Result<T, AutomationError> {
        Err(self.error.clone())
    }
}

#[cfg(target_os = "linux")]
impl AtspiBackend for UnavailableBackend {
    fn name(&self) -> &str {
        "unavailable"
    }

    fn session_info(&self) -> Map<String, Value> {
        Map::from_iter([
            ("session_type".into(), environment_value("XDG_SESSION_TYPE")),
            (
                "desktop".into(),
                std::env::var("XDG_CURRENT_DESKTOP")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .or_else(|| {
                        std::env::var("DESKTOP_SESSION")
                            .ok()
                            .filter(|value| !value.is_empty())
                    })
                    .map_or(Value::Null, Value::String),
            ),
        ])
    }

    fn list_applications(
        &self,
        _deadline: Instant,
    ) -> Result<Vec<Map<String, Value>>, AutomationError> {
        self.unavailable()
    }
    fn capture(
        &self,
        _application: &Map<String, Value>,
        _max_depth: u32,
        _max_nodes: usize,
        _deadline: Instant,
    ) -> Result<BackendSnapshot, AutomationError> {
        self.unavailable()
    }
    fn focus(&self, _target: &NativeRef, _deadline: Instant) -> Result<Value, AutomationError> {
        self.unavailable()
    }
    fn invoke(&self, _target: &NativeRef, _deadline: Instant) -> Result<Value, AutomationError> {
        self.unavailable()
    }
    fn set_text(
        &self,
        _target: &NativeRef,
        _text: &str,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.unavailable()
    }
    fn named_action(
        &self,
        _target: &NativeRef,
        _name: &str,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.unavailable()
    }
}

#[cfg(target_os = "linux")]
fn environment_value(name: &str) -> Value {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or(Value::Null, Value::String)
}

fn bounded_optional_string(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_str)
        .map(|text| Value::String(text.chars().take(MAX_FIELD_CHARS).collect()))
        .unwrap_or(Value::Null)
}

fn bounded_string(value: Option<String>) -> Value {
    value
        .map(|text| Value::String(text.chars().take(MAX_FIELD_CHARS).collect()))
        .unwrap_or(Value::Null)
}

fn required_string<'a>(value: &'a Value, name: &str) -> Result<&'a str, AutomationError> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && value.chars().count() <= MAX_FIELD_CHARS)
        .ok_or_else(|| invalid(format!("{name} must be a bounded non-empty string")))
}

fn bounded_text(value: Option<&Value>, non_empty: bool) -> Result<&str, AutomationError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("text must be a string"))?;
    if value.chars().count() > MAX_FIELD_CHARS
        || (non_empty && value.is_empty())
        || value.contains('\0')
    {
        return Err(invalid("text is outside the allowed range"));
    }
    Ok(value)
}

fn ordinary_text(value: Option<&Value>) -> Result<&str, AutomationError> {
    let text = bounded_text(value, true)?;
    if text.chars().count() > MAX_TYPE_TEXT_CHARS
        || text.len() > MAX_TYPE_TEXT_BYTES
        || text.chars().any(|character| {
            let code = character as u32;
            (code < 0x20 && character != '\n')
                || (0x7f..=0x9f).contains(&code)
                || (0xfdd0..=0xfdef).contains(&code)
                || matches!(code & 0xffff, 0xfffe | 0xffff)
        })
    {
        return Err(invalid(
            "type_text accepts only bounded ordinary UTF-8 text",
        ));
    }
    Ok(text)
}

fn application_selector(value: Option<&Value>) -> Result<Map<String, Value>, AutomationError> {
    let object = value
        .and_then(Value::as_object)
        .filter(|object| !object.is_empty())
        .ok_or_else(|| invalid("application must contain an exact selector"))?;
    only_keys(object, &["bus_name", "name", "process_id", "toolkit_name"])?;
    let mut result = Map::new();
    for name in ["bus_name", "name", "toolkit_name"] {
        if let Some(value) = object.get(name) {
            result.insert(
                name.into(),
                json!(required_string(value, &format!("application.{name}"))?),
            );
        }
    }
    if let Some(value) = object.get("process_id") {
        let process = value
            .as_u64()
            .filter(|value| *value <= u32::MAX as u64)
            .ok_or_else(|| invalid("application.process_id must be a non-negative integer"))?;
        result.insert("process_id".into(), json!(process));
    }
    Ok(result)
}

fn bounded_integer(
    value: Option<&Value>,
    default: u64,
    minimum: u64,
    maximum: u64,
    name: &str,
) -> Result<u64, AutomationError> {
    value
        .map_or(Some(default), Value::as_u64)
        .filter(|value| (*value >= minimum) && (*value <= maximum))
        .ok_or_else(|| invalid(format!("{name} is outside its allowed range")))
}

fn only_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), AutomationError> {
    let fields = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if fields.is_empty() {
        Ok(())
    } else {
        Err(invalid("request contains unsupported fields").with_detail("fields", json!(fields)))
    }
}

fn normalize_role(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    if value.is_empty() {
        "unknown".into()
    } else {
        value
    }
}

fn parse_locator(value: Option<&Value>) -> Result<Map<String, Value>, AutomationError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("locator must be an object"))?;
    let allowed = [
        "role",
        "name",
        "description",
        "value",
        "bus_name",
        "object_path",
        "toolkit_name",
        "attributes",
        "states",
        "actions",
        "match",
    ];
    only_keys(object, &allowed)?;
    if object.keys().all(|key| key == "match") {
        return Err(invalid("locator requires at least one selection condition"));
    }
    if object.get("match").is_some_and(|value| value != "exact") {
        return Err(invalid("only exact locators are supported"));
    }
    let mut result = Map::new();
    result.insert("match".into(), json!("exact"));
    for name in ["role", "name", "bus_name", "object_path", "toolkit_name"] {
        if let Some(value) = object.get(name) {
            result.insert(
                name.into(),
                json!(required_string(value, &format!("locator.{name}"))?),
            );
        }
    }
    for name in ["description", "value"] {
        if let Some(value) = object.get(name) {
            if value.is_null() {
                result.insert(name.into(), Value::Null);
            } else {
                result.insert(
                    name.into(),
                    json!(required_string(value, &format!("locator.{name}"))?),
                );
            }
        }
    }
    if let Some(value) = object.get("attributes") {
        let attributes = value
            .as_object()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("locator.attributes must be a non-empty object"))?;
        for (key, value) in attributes {
            if key.is_empty() || key.chars().count() > MAX_FIELD_CHARS {
                return Err(invalid("locator attribute key is invalid"));
            }
            required_string(value, "locator attribute value")?;
        }
        result.insert("attributes".into(), value.clone());
    }
    if let Some(value) = object.get("states") {
        let states = value
            .as_object()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("locator.states must be a non-empty object"))?;
        only_keys(states, STATE_NAMES)?;
        if states
            .values()
            .any(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(invalid("locator state must be boolean or null"));
        }
        result.insert("states".into(), value.clone());
    }
    if let Some(value) = object.get("actions") {
        let actions = value
            .as_array()
            .filter(|items| !items.is_empty())
            .ok_or_else(|| invalid("locator.actions must be a non-empty array"))?;
        let mut seen = BTreeSet::new();
        for action in actions {
            let action = action
                .as_str()
                .filter(|action| WRITE_ACTIONS.contains(action))
                .ok_or_else(|| invalid("locator.actions contains an unsupported action"))?;
            if !seen.insert(action) {
                return Err(invalid("locator.actions must be unique"));
            }
        }
        result.insert("actions".into(), value.clone());
    }
    Ok(result)
}

fn node_matches(node: &Value, locator: &Map<String, Value>) -> bool {
    for name in ["role", "name", "description", "value"] {
        if locator
            .get(name)
            .is_some_and(|wanted| node.get(name) != Some(wanted))
        {
            return false;
        }
    }
    for name in ["bus_name", "object_path", "toolkit_name"] {
        if locator
            .get(name)
            .is_some_and(|wanted| node["provenance"].get(name) != Some(wanted))
        {
            return false;
        }
    }
    for (name, wanted) in locator
        .get("attributes")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if node["attributes"].get(name) != Some(wanted) {
            return false;
        }
    }
    for (name, wanted) in locator
        .get("states")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if node["states"].get(name) != Some(wanted) {
            return false;
        }
    }
    let actions = node["actions"].as_array().expect("normalized actions");
    locator
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .all(|wanted| actions.contains(wanted))
}

fn resolve(
    record: &Record,
    locator: &Map<String, Value>,
    deadline: Instant,
) -> Result<Value, AutomationError> {
    let mut candidates = Vec::new();
    for node in record.public["nodes"].as_array().expect("normalized nodes") {
        remaining(deadline, false)?;
        if node_matches(node, locator) {
            candidates.push(node.clone());
        }
    }
    match candidates.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(driver_error("DRIVER.NOT_FOUND", "locator matched no node")),
        many => Err(
            driver_error("DRIVER.AMBIGUOUS", "locator matched multiple nodes")
                .with_detail("candidate_count", json!(many.len()))
                .with_detail(
                    "candidates",
                    json!(many
                        .iter()
                        .take(MAX_CANDIDATE_SUMMARIES)
                        .collect::<Vec<_>>()),
                ),
        ),
    }
}

fn target(record: &Record, node_id: &str) -> Value {
    json!({"snapshot_id": record.public["snapshot_id"], "revision": record.public["revision"], "node_id": node_id})
}

fn descendants(record: &Record, node_id: &str) -> BTreeSet<String> {
    let nodes = record.public["nodes"].as_array().expect("normalized nodes");
    let mut result = BTreeSet::from([node_id.to_string()]);
    let mut queue = VecDeque::from([node_id.to_string()]);
    while let Some(parent) = queue.pop_front() {
        for node in nodes {
            if node["parent_id"] == parent {
                if let Some(id) = node["node_id"].as_str() {
                    if result.insert(id.to_string()) {
                        queue.push_back(id.to_string());
                    }
                }
            }
        }
    }
    result
}

fn ancestors(record: &Record, node_id: &str) -> BTreeSet<String> {
    let nodes = record.public["nodes"].as_array().expect("normalized nodes");
    let parents = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node["node_id"].as_str()?.to_string(),
                node["parent_id"].as_str().map(str::to_string),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut result = BTreeSet::new();
    let mut current = Some(node_id.to_string());
    while let Some(id) = current {
        current = parents.get(&id).cloned().flatten();
        result.insert(id);
    }
    result
}

fn overlaps(left: Bounds, right: Bounds) -> bool {
    left.x < right.x.saturating_add(right.width)
        && right.x < left.x.saturating_add(left.width)
        && left.y < right.y.saturating_add(right.height)
        && right.y < left.y.saturating_add(left.height)
}

fn fingerprint(node: &Value) -> String {
    let identity = json!({
        "role": node["role"],
        "name": node["name"],
        "bus_name": node["provenance"]["bus_name"],
        "object_path": node["provenance"]["object_path"],
        "accessible_id": node["provenance"]["accessible_id"],
        "application_name": node["provenance"]["application_name"],
        "toolkit_name": node["provenance"]["toolkit_name"],
        "process_id": node["provenance"]["process_id"],
    });
    let encoded = serde_json::to_vec(&identity).expect("identity is serializable");
    format!("sha256:{:x}", Sha256::digest(encoded))
}

fn process_id(node: &Value, record: &Record) -> Result<u32, AutomationError> {
    let process = node["provenance"]["process_id"]
        .as_u64()
        .filter(|value| *value > 0 && *value <= u32::MAX as u64)
        .ok_or_else(|| stale("target application process ownership cannot be proven"))?;
    if record.public["application"]["process_id"] != process {
        return Err(stale("target application process ownership changed"));
    }
    Ok(process as u32)
}

fn parse_bounds(value: Option<&Value>) -> Result<Bounds, AutomationError> {
    let value = value
        .and_then(Value::as_object)
        .ok_or_else(|| driver_error("DRIVER.ACTION_UNSUPPORTED", "target has no valid bounds"))?;
    let integer = |name| {
        value
            .get(name)
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| driver_error("DRIVER.ACTION_UNSUPPORTED", "target has no valid bounds"))
    };
    Ok(Bounds {
        x: integer("x")?,
        y: integer("y")?,
        width: integer("width")?,
        height: integer("height")?,
    })
}

fn remaining(deadline: Instant, post_dispatch: bool) -> Result<Duration, AutomationError> {
    let value = deadline.saturating_duration_since(Instant::now());
    if value.is_zero() {
        Err(timeout_error(post_dispatch))
    } else {
        Ok(value)
    }
}

fn timeout_error(post_dispatch: bool) -> AutomationError {
    driver_error("DRIVER.TIMEOUT", "request deadline elapsed")
        .with_retryable(!post_dispatch)
        .with_effect(if post_dispatch {
            "unknown"
        } else {
            "not_applied"
        })
        .with_detail(
            "phase",
            json!(if post_dispatch {
                "post_dispatch"
            } else {
                "before_dispatch"
            }),
        )
}

fn invalid(message: impl Into<String>) -> AutomationError {
    driver_error("DRIVER.INVALID_REQUEST", message)
}

fn stale(message: impl Into<String>) -> AutomationError {
    driver_error("DRIVER.STALE_SNAPSHOT", message)
}

fn driver_error(code: &str, message: impl Into<String>) -> AutomationError {
    AutomationError::new(code, message)
        .with_category("driver")
        .with_effect("not_applied")
}

fn target_schema() -> Value {
    json!({"type": "object", "required": ["snapshot_id", "revision", "node_id"], "properties": {"snapshot_id": {"type": "string", "minLength": 1}, "revision": {"type": "integer", "minimum": 1}, "node_id": {"type": "string", "minLength": 1}}, "additionalProperties": false})
}

fn locator_schema() -> Value {
    json!({"type": "object", "properties": {
        "role": {"type": "string", "maxLength": 256}, "name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "description": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS}, "value": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS},
        "bus_name": {"type": "string", "maxLength": MAX_FIELD_CHARS}, "object_path": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "toolkit_name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "attributes": {"type": "object", "minProperties": 1, "additionalProperties": {"type": "string", "maxLength": MAX_FIELD_CHARS}},
        "states": {"type": "object", "minProperties": 1, "properties": STATE_NAMES.iter().map(|name| ((*name).to_string(), json!({"type": ["boolean", "null"]}))).collect::<Map<_,_>>(), "additionalProperties": false},
        "actions": {"type": "array", "minItems": 1, "items": {"enum": WRITE_ACTIONS}, "uniqueItems": true}, "match": {"const": "exact"}
    }, "additionalProperties": false, "minProperties": 1})
}

fn application_schema() -> Value {
    json!({"type": "object", "minProperties": 1, "properties": {"bus_name": {"type": "string", "maxLength": MAX_FIELD_CHARS}, "name": {"type": "string", "maxLength": MAX_FIELD_CHARS}, "process_id": {"type": "integer", "minimum": 0}, "toolkit_name": {"type": "string", "maxLength": MAX_FIELD_CHARS}}, "additionalProperties": false})
}

fn error_contracts(write: bool, locator: bool, capture: bool) -> Vec<Value> {
    let mut values = vec![
        (
            "DRIVER.INVALID_REQUEST",
            "动作参数无效。",
            false,
            "not_applied",
        ),
        (
            "DRIVER.UNAVAILABLE",
            "Linux AT-SPI 后端不可用。",
            false,
            "not_applied",
        ),
        (
            "DRIVER.ACTION_FAILED",
            "AT-SPI 原生操作失败。",
            false,
            "not_applied",
        ),
        ("DRIVER.TIMEOUT", "请求截止时间已到。", true, "not_applied"),
        (
            "DRIVER.OUTPUT_TOO_LARGE",
            "规范化响应超过线路限制。",
            false,
            "not_applied",
        ),
    ];
    if locator {
        values.extend([
            (
                "DRIVER.NOT_FOUND",
                "定位器没有匹配节点。",
                false,
                "not_applied",
            ),
            (
                "DRIVER.AMBIGUOUS",
                "定位器匹配多个节点。",
                false,
                "not_applied",
            ),
            (
                "DRIVER.STALE_SNAPSHOT",
                "快照目标已不再是当前版本。",
                false,
                "not_applied",
            ),
            (
                "DRIVER.SNAPSHOT_TRUNCATED",
                "有界快照无法证明唯一性。",
                false,
                "not_applied",
            ),
        ]);
    }
    if write || capture {
        values.extend([
            (
                "DRIVER.ACTION_UNSUPPORTED",
                "目标缺少所需 AT-SPI 接口。",
                false,
                "not_applied",
            ),
            (
                "DRIVER.PROTECTED_ELEMENT",
                "目标是受保护元素。",
                false,
                "not_applied",
            ),
        ]);
    }
    if write {
        values.push((
            "DRIVER.UNKNOWN_EFFECT",
            "原生动作可能已生效。",
            false,
            "unknown",
        ));
    }
    if capture {
        values.extend([
            (
                "DRIVER.CAPTURE_FAILED",
                "X11 目标截图失败。",
                false,
                "not_applied",
            ),
            (
                "DRIVER.ARTIFACT_IPC",
                "截图 artifact 传输失败。",
                false,
                "not_applied",
            ),
        ]);
    }
    values.into_iter().map(|(code, description, retryable, effect)| json!({"code": code, "description": description, "retryable": retryable, "effect": effect, "data_schema": {"type": "object"}})).collect()
}

#[allow(clippy::too_many_arguments)]
fn contract(
    description: &str,
    effect: &str,
    category: &str,
    level: &str,
    permissions: &[&str],
    input: Value,
    output: Value,
    write: bool,
    locator: bool,
) -> Value {
    json!({"contract_major": 1, "description": description, "effect": {"default_class": effect}, "risk": {"category": category, "level": level}, "permissions": permissions, "input_schema": input, "output_schema": output, "errors": error_contracts(write, locator, false)})
}

fn manifest_document() -> Value {
    let common_write = || json!({"type": "object", "required": ["target", "locator"], "properties": {"target": target_schema(), "locator": locator_schema()}, "additionalProperties": false});
    let write_output = || json!({"type": "object", "required": ["ok", "action", "resolved"], "properties": {"ok": {"const": true}, "action": {"enum": WRITE_ACTIONS}, "resolved": target_schema(), "backend_result": {}}, "additionalProperties": false});
    let mut actions = Map::new();
    actions.insert("inspect_session".into(), contract("返回不含应用内容的当前 AT-SPI backend 与桌面会话类型。", "read_only", "observe", "low", &["desktop.observe"], json!({"type": "object", "additionalProperties": false}), json!({"type": "object", "required": ["backend", "session_type", "desktop"], "properties": {"backend": {"type": "string", "maxLength": 128}, "session_type": {"type": ["string", "null"], "maxLength": 128}, "desktop": {"type": ["string", "null"], "maxLength": 256}}, "additionalProperties": false}), false, false));
    actions["inspect_session"]["sensitivity"] =
        json!({"input": "public", "output": "public", "error": "public"});
    actions["inspect_session"]["durability"] = json!({"checkpoint_fields": {
        "backend": {"pointer": "/backend", "schema": {"type": "string", "maxLength": 128}},
        "session_type": {"pointer": "/session_type", "schema": {"type": ["string", "null"], "maxLength": 128}},
        "desktop": {"pointer": "/desktop", "schema": {"type": ["string", "null"], "maxLength": 256}}
    }});
    actions.insert("list_applications".into(), contract("通过 AT-SPI desktop 根节点枚举当前会话的桌面应用。", "read_only", "observe", "low", &["desktop.observe"], json!({"type": "object", "additionalProperties": false}), json!({"type": "object", "required": ["session", "backend", "applications"], "properties": {"session": {"type": "object"}, "backend": {"type": "string"}, "applications": {"type": "array", "items": {"type": "object"}}}, "additionalProperties": false}), false, false));
    let snapshot_output = json!({"type": "object", "required": ["snapshot_id", "revision", "session", "backend", "application", "nodes", "truncated"], "properties": {"snapshot_id": {"type": "string"}, "revision": {"type": "integer", "minimum": 1}, "session": {"type": "object"}, "backend": {"type": "string"}, "application": {"type": "object"}, "nodes": {"type": "array", "items": {"type": "object"}}, "truncated": {"type": "boolean"}}, "additionalProperties": false});
    actions.insert("snapshot".into(), contract("抓取一个精确应用选择器对应的有界 AT-SPI 可访问性树。", "read_only", "observe", "low", &["desktop.observe"], json!({"type": "object", "required": ["application"], "properties": {"application": application_schema(), "max_depth": {"type": "integer", "minimum": 0, "maximum": MAX_DEPTH}, "max_nodes": {"type": "integer", "minimum": 1, "maximum": MAX_NODES}}, "additionalProperties": false}), snapshot_output, false, false));
    actions["snapshot"]["errors"].as_array_mut().unwrap().extend([
        json!({"code": "DRIVER.NOT_FOUND", "description": "定位器没有匹配节点。", "retryable": false, "effect": "not_applied", "data_schema": {"type": "object"}}),
        json!({"code": "DRIVER.AMBIGUOUS", "description": "定位器匹配多个节点。", "retryable": false, "effect": "not_applied", "data_schema": {"type": "object"}}),
    ]);
    actions.insert("find".into(), contract("在当前完整快照中解析仅支持精确匹配的定位器。", "read_only", "observe", "low", &["desktop.observe"], json!({"type": "object", "required": ["snapshot_id", "revision", "locator"], "properties": {"snapshot_id": {"type": "string", "minLength": 1}, "revision": {"type": "integer", "minimum": 1}, "locator": locator_schema()}, "additionalProperties": false}), json!({"type": "object", "required": ["target", "node"], "properties": {"target": target_schema(), "node": {"type": "object"}}, "additionalProperties": false}), false, true));
    for (name, effect, category, level, description) in [
        (
            "focus",
            "contextual",
            "navigate",
            "medium",
            "重新验证目标后调用 AT-SPI Component.grab_focus。",
        ),
        (
            "invoke",
            "non_idempotent",
            "modify",
            "high",
            "重新验证目标后调用 AT-SPI Action.do_action。",
        ),
        (
            "toggle",
            "non_idempotent",
            "modify",
            "high",
            "重新验证 GTK3 目标及 checked 状态后，精确调用名为 click 的 AT-SPI 动作。",
        ),
        (
            "expand",
            "idempotent",
            "modify",
            "medium",
            "重新验证 GTK3 目标后，在需要时精确调用名为 activate 的 AT-SPI 动作以展开。",
        ),
        (
            "collapse",
            "idempotent",
            "modify",
            "medium",
            "重新验证 GTK3 目标后，在需要时精确调用名为 activate 的 AT-SPI 动作以折叠。",
        ),
    ] {
        actions.insert(
            name.into(),
            contract(
                description,
                effect,
                category,
                level,
                &["desktop.observe", "desktop.input"],
                common_write(),
                write_output(),
                true,
                true,
            ),
        );
    }
    for (name, description, min_length) in [
        (
            "set_text",
            "重新验证目标后调用 AT-SPI EditableText.set_text_contents。",
            0,
        ),
        (
            "type_text",
            "重新验证并聚焦目标后，通过受限 KDE/X11 XTest helper 显式输入普通 UTF-8 文本。",
            1,
        ),
    ] {
        actions.insert(name.into(), contract(description, "contextual", "input", "high", &["desktop.observe", "desktop.input"], json!({"type": "object", "required": ["target", "locator", "text"], "properties": {"target": target_schema(), "locator": locator_schema(), "text": {"type": "string", "minLength": min_length, "maxLength": if name == "type_text" { MAX_TYPE_TEXT_CHARS } else { MAX_FIELD_CHARS }}}, "additionalProperties": false}), write_output(), true, true));
        if name == "set_text" {
            actions[name]["input_schema"]["properties"]["text"]
                .as_object_mut()
                .unwrap()
                .remove("minLength");
        }
    }
    actions.insert("pointer_click".into(), contract("重新验证目标后，仅在明确的 KDE/X11 会话中，通过固定路径 XTest helper 按目标 bounds 中心点显式执行左键单击。", "non_idempotent", "modify", "high", &["desktop.observe", "desktop.input"], json!({"type": "object", "required": ["target", "locator"], "properties": {"target": target_schema(), "locator": locator_schema(), "button": {"enum": ["left"]}, "position": {"enum": ["center"]}}, "additionalProperties": false}), write_output(), true, true));
    let capture_provenance = json!({"type": "object", "required": ["capture_method", "snapshot_id", "revision", "node_id", "application_process_id", "format", "mime_type", "target_process_id", "target_window", "target_top_level_window", "root_window", "bounds", "root_size", "cursor_included", "occlusion_checked", "same_euid_verified", "scene_stable"], "properties": {
        "capture_method": {"const": "x11_root_xgetimage"}, "snapshot_id": {"type": "string", "minLength": 1}, "revision": {"type": "integer", "minimum": 1}, "node_id": {"type": "string", "minLength": 1}, "application_process_id": {"type": "integer", "minimum": 1},
        "format": {"const": "png"}, "mime_type": {"const": "image/png"}, "target_process_id": {"type": "integer", "minimum": 1}, "target_window": {"type": "integer", "minimum": 1}, "target_top_level_window": {"type": "integer", "minimum": 1}, "root_window": {"type": "integer", "minimum": 1},
        "bounds": {"type": "object", "required": ["x", "y", "width", "height"], "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}, "width": {"type": "integer", "minimum": 1}, "height": {"type": "integer", "minimum": 1}}, "additionalProperties": false},
        "root_size": {"type": "object", "required": ["width", "height"], "properties": {"width": {"type": "integer", "minimum": 1}, "height": {"type": "integer", "minimum": 1}}, "additionalProperties": false},
        "cursor_included": {"const": false}, "occlusion_checked": {"const": true}, "same_euid_verified": {"const": true}, "scene_stable": {"const": true}
    }, "additionalProperties": false});
    let mut capture = contract("重新验证精确 AT-SPI 目标后，经固定 X11 helper 截取其 fresh screen bounds；仅输出 host 托管 PNG artifact 与可审计 provenance。", "read_only", "observe", "medium", &["desktop.observe", "desktop.capture"], json!({"type": "object", "required": ["target", "locator", "format"], "properties": {"target": target_schema(), "locator": locator_schema(), "format": {"const": "png"}}, "additionalProperties": false}), json!({"type": "object", "required": ["frame", "provenance"], "properties": {"frame": {"type": "object"}, "provenance": capture_provenance}, "additionalProperties": false}), false, true);
    capture["artifacts"] = json!({"outputs": {"frame": {"pointer": "/frame", "media_types": ["image/png"], "max_size_bytes": 64 * 1024 * 1024}}});
    capture["errors"] = json!(error_contracts(false, true, true));
    capture["sensitivity"] = json!({"input": "public", "output": "sensitive", "error": "public"});
    actions.insert("capture_target".into(), capture);
    json!({"apiVersion": "ai-auto-desktop.dev/v1alpha1", "kind": "CapabilityManifest", "metadata": {"name": PROVIDER_NAME, "version": PROVIDER_VERSION, "description": "Linux 原生 AT-SPI 语义桌面驱动。"}, "actions": actions, "runtime": {"kind": "builtin", "protocol": "rust-provider-v1", "platforms": ["linux"]}})
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeBackend {
        captures: AtomicUsize,
        writes: AtomicUsize,
        changed: bool,
        duplicate: bool,
        truncated: bool,
    }

    impl FakeBackend {
        fn application() -> Map<String, Value> {
            Map::from_iter([
                ("bus_name".into(), json!(":1.42")),
                ("object_path".into(), json!("/org/a11y/app")),
                ("name".into(), json!("Fixture")),
                ("process_id".into(), json!(4242)),
                ("toolkit_name".into(), json!("gtk")),
                ("toolkit_version".into(), json!("3.24")),
            ])
        }

        fn node(&self, object_path: &str) -> BackendNode {
            let changed = self.changed && self.captures.load(Ordering::SeqCst) > 1;
            BackendNode {
                native: NativeRef {
                    bus_name: ":1.42".into(),
                    object_path: object_path.into(),
                },
                parent_index: None,
                role: "push button".into(),
                name: Some(if changed { "Changed" } else { "Save" }.into()),
                description: Some("Save the document".into()),
                value: None,
                attributes: BTreeMap::from_iter([("automation-id".into(), "save".into())]),
                states: BTreeMap::from_iter([
                    ("enabled".into(), Some(true)),
                    ("visible".into(), Some(true)),
                    ("showing".into(), Some(true)),
                    ("focusable".into(), Some(true)),
                    ("sensitive".into(), Some(true)),
                    ("protected".into(), Some(false)),
                ]),
                bounds: Some(Bounds {
                    x: 10,
                    y: 20,
                    width: 40,
                    height: 20,
                }),
                actions: vec!["focus".into(), "invoke".into(), "pointer_click".into()],
                provenance: Map::from_iter([
                    ("bus_name".into(), json!(":1.42")),
                    ("object_path".into(), json!(object_path)),
                    ("accessible_id".into(), json!("save")),
                    ("application_name".into(), json!("Fixture")),
                    ("toolkit_name".into(), json!("gtk")),
                    ("process_id".into(), json!(4242)),
                    ("value_redacted".into(), json!(false)),
                    ("coordinate_space".into(), json!("screen")),
                ]),
            }
        }
    }

    impl AtspiBackend for FakeBackend {
        fn name(&self) -> &str {
            "fake_atspi"
        }
        fn session_info(&self) -> Map<String, Value> {
            Map::from_iter([
                ("session_type".into(), json!("x11")),
                ("desktop".into(), json!("KDE")),
            ])
        }
        fn list_applications(
            &self,
            _deadline: Instant,
        ) -> Result<Vec<Map<String, Value>>, AutomationError> {
            Ok(vec![Self::application()])
        }
        fn capture(
            &self,
            _application: &Map<String, Value>,
            _max_depth: u32,
            _max_nodes: usize,
            _deadline: Instant,
        ) -> Result<BackendSnapshot, AutomationError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            let mut nodes = vec![self.node("/org/a11y/save")];
            if self.duplicate {
                nodes.push(self.node("/org/a11y/save-2"));
            }
            Ok(BackendSnapshot {
                application: Self::application(),
                nodes,
                truncated: self.truncated,
            })
        }
        fn focus(&self, _target: &NativeRef, _deadline: Instant) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"accepted": true}))
        }
        fn invoke(
            &self,
            _target: &NativeRef,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"accepted": true}))
        }
        fn set_text(
            &self,
            _target: &NativeRef,
            _text: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"accepted": true}))
        }
        fn named_action(
            &self,
            _target: &NativeRef,
            _name: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"accepted": true}))
        }
        fn pointer_click(
            &self,
            _target: &NativeRef,
            point: (i32, i32),
            process_id: u32,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"submitted": true, "click_point": {"x": point.0, "y": point.1}, "expected_process_id": process_id}),
            )
        }
        fn accessible_at_point(
            &self,
            root: &NativeRef,
            _point: (i32, i32),
            _deadline: Instant,
        ) -> Result<Option<NativeRef>, AutomationError> {
            Ok(Some(root.clone()))
        }
        fn capture_target(
            &self,
            _target: &NativeRef,
            bounds: Bounds,
            process_id: u32,
            _deadline: Instant,
        ) -> Result<(Vec<u8>, Map<String, Value>), AutomationError> {
            const PNG: &[u8] = &[
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0,
                1, 8, 4, 0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100,
                248, 15, 0, 1, 5, 1, 1, 39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
                130,
            ];
            Ok((
                PNG.to_vec(),
                Map::from_iter([
                    ("capture_method".into(), json!("fake")),
                    ("format".into(), json!("png")),
                    ("mime_type".into(), json!("image/png")),
                    ("target_process_id".into(), json!(process_id)),
                    ("bounds".into(), bounds.to_json()),
                    ("cursor_included".into(), json!(false)),
                ]),
            ))
        }
    }

    fn take_snapshot(provider: &AtspiProvider) -> Value {
        provider
            .invoke(
                "desktop.linux_atspi.snapshot@1",
                json!({"application": {"name": "Fixture"}}),
                Some(Duration::from_secs(2)),
            )
            .unwrap()
    }

    fn locator() -> Value {
        json!({"role": "push_button", "name": "Save", "attributes": {"automation-id": "save"}})
    }

    #[test]
    fn manifest_keeps_the_complete_linux_action_surface() {
        let document = manifest_document();
        let parsed = manifest::parse(&document).unwrap();
        for action in [
            "inspect_session",
            "list_applications",
            "snapshot",
            "find",
            "capture_target",
            "focus",
            "invoke",
            "pointer_click",
            "set_text",
            "type_text",
            "toggle",
            "expand",
            "collapse",
        ] {
            assert!(
                parsed
                    .resolve(&format!("{PROVIDER_NAME}.{action}@1"))
                    .is_some(),
                "{action}"
            );
        }
        assert!(parsed.actions["capture_target"].has_artifacts());
    }

    #[test]
    fn snapshot_and_exact_find_preserve_the_contract() {
        let provider = AtspiProvider::new(Arc::new(FakeBackend::default())).unwrap();
        let snapshot = take_snapshot(&provider);
        assert_eq!(snapshot["backend"], "fake_atspi");
        assert_eq!(snapshot["nodes"][0]["role"], "push_button");
        let found = provider.invoke("desktop.linux_atspi.find@1", json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "locator": locator()}), Some(Duration::from_secs(2))).unwrap();
        assert_eq!(found["target"]["node_id"], "n0");
        assert_eq!(found["node"]["name"], "Save");
    }

    #[test]
    fn ambiguous_and_truncated_snapshots_fail_closed() {
        let provider = AtspiProvider::new(Arc::new(FakeBackend {
            duplicate: true,
            ..Default::default()
        }))
        .unwrap();
        let snapshot = take_snapshot(&provider);
        let error = provider.invoke("desktop.linux_atspi.find@1", json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "locator": {"role": "push_button", "name": "Save"}}), Some(Duration::from_secs(2))).unwrap_err();
        assert_eq!(error.code, "DRIVER.AMBIGUOUS");

        let provider = AtspiProvider::new(Arc::new(FakeBackend {
            truncated: true,
            ..Default::default()
        }))
        .unwrap();
        let snapshot = take_snapshot(&provider);
        let error = provider.invoke("desktop.linux_atspi.find@1", json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "locator": locator()}), Some(Duration::from_secs(2))).unwrap_err();
        assert_eq!(error.code, "DRIVER.SNAPSHOT_TRUNCATED");
    }

    #[test]
    fn write_actions_refresh_and_reject_semantic_staleness() {
        let backend = Arc::new(FakeBackend::default());
        let provider = AtspiProvider::new(backend.clone()).unwrap();
        let snapshot = take_snapshot(&provider);
        let result = provider.invoke("desktop.linux_atspi.pointer_click@1", json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"}, "locator": locator()}), Some(Duration::from_secs(2))).unwrap();
        assert_eq!(
            result["backend_result"]["click_point"],
            json!({"x": 30, "y": 30})
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);

        let backend = Arc::new(FakeBackend {
            changed: true,
            ..Default::default()
        });
        let provider = AtspiProvider::new(backend.clone()).unwrap();
        let snapshot = take_snapshot(&provider);
        let error = provider.invoke("desktop.linux_atspi.invoke@1", json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"}, "locator": locator()}), Some(Duration::from_secs(2))).unwrap_err();
        assert_eq!(error.code, "DRIVER.STALE_SNAPSHOT");
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn capture_target_returns_a_host_managed_artifact() {
        let provider = AtspiProvider::new(Arc::new(FakeBackend::default())).unwrap();
        let snapshot = take_snapshot(&provider);
        let store = ArtifactStore::default();
        let result = provider.invoke_with_artifacts("desktop.linux_atspi.capture_target@1", json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"}, "locator": locator(), "format": "png"}), Some(Duration::from_secs(2)), &store).unwrap();
        assert_eq!(result["frame"]["kind"], "ArtifactRef");
        assert!(result["frame"].get("path").is_none());
        assert!(!store.resolve(&result["frame"]).unwrap().is_empty());
    }
}
