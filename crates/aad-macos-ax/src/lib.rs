//! Rust adapter and snapshot discipline for the native macOS AX helper.
//!
//! The Swift helper remains the code-signed TCC identity and the only process
//! that imports AppKit/ApplicationServices. This crate replaces the Python
//! protocol, validation, locator and stale-target layers.

use aad_core::AutomationError;
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::Provider;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PROVIDER_NAME: &str = "desktop.macos_ax";
pub const PROVIDER_VERSION: &str = "0.1.0";
const MAX_FIELD_CHARS: usize = 4096;
const MAX_TYPE_TEXT_CHARS: usize = 1024;
const MAX_TYPE_TEXT_UTF16_UNITS: usize = 2048;
const DEFAULT_MAX_DEPTH: u32 = 32;
const DEFAULT_MAX_NODES: usize = 1000;
const MAX_DEPTH: u32 = 128;
const MAX_NODES: usize = 5000;
const WRITE_ACTIONS: &[&str] = &["focus", "invoke", "pointer_click", "set_value", "type_text"];
const STATE_NAMES: &[&str] = &["enabled", "focused", "focusable", "editable", "protected"];
const TYPE_TEXT_ROLES: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox"];

#[derive(Clone, Debug)]
pub struct BackendNode {
    pub native_token: String,
    pub parent_index: Option<usize>,
    pub role: String,
    pub subrole: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub states: BTreeMap<String, Option<bool>>,
    pub bounds: Option<Bounds>,
    pub actions: Vec<String>,
    pub provenance: Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct BackendSnapshot {
    pub app: Map<String, Value>,
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

pub trait AxBackend: Send + Sync {
    fn name(&self) -> &str;
    fn security_info(&self) -> Option<Map<String, Value>> {
        None
    }
    fn list_apps(
        &self,
        deadline: Instant,
    ) -> Result<(bool, Vec<Map<String, Value>>), AutomationError>;
    fn capture(
        &self,
        app: &Map<String, Value>,
        max_depth: u32,
        max_nodes: usize,
        deadline: Instant,
    ) -> Result<BackendSnapshot, AutomationError>;
    fn same_element(
        &self,
        previous: &str,
        current: &str,
        deadline: Instant,
    ) -> Result<bool, AutomationError>;
    fn focus(&self, token: &str, deadline: Instant) -> Result<Value, AutomationError>;
    fn invoke(&self, token: &str, deadline: Instant) -> Result<Value, AutomationError>;
    fn pointer_click(
        &self,
        token: &str,
        button: &str,
        position: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError>;
    fn set_value(
        &self,
        token: &str,
        value: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError>;
    fn type_text(
        &self,
        token: &str,
        text: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError>;
    fn close(&self) {}
}

#[derive(Clone)]
struct Record {
    public: Value,
    handles: BTreeMap<String, String>,
    fingerprints: BTreeMap<String, String>,
    app_selector: Map<String, Value>,
    max_depth: u32,
    max_nodes: usize,
}

#[derive(Default)]
struct DriverState {
    revision: u64,
    current: Option<Record>,
}

pub struct MacOSAxProvider {
    backend: Arc<dyn AxBackend>,
    manifest: CapabilityManifest,
    generation: String,
    state: Mutex<DriverState>,
}

impl MacOSAxProvider {
    pub fn new(backend: Arc<dyn AxBackend>) -> Result<Self, AutomationError> {
        Ok(Self {
            backend,
            manifest: manifest::parse(&manifest_document()).map_err(|reason| {
                driver_error(
                    "DRIVER.INTERNAL",
                    format!("built-in macOS AX manifest is invalid: {reason}"),
                )
            })?,
            generation: uuid::Uuid::new_v4().simple().to_string(),
            state: Mutex::new(DriverState::default()),
        })
    }

    fn deadline(timeout: Option<Duration>) -> Result<Instant, AutomationError> {
        let timeout = timeout.unwrap_or(Duration::from_secs(30));
        if timeout.is_zero() {
            return Err(timeout_error(false));
        }
        Ok(Instant::now() + timeout)
    }

    fn call(&self, action: &str, args: Value, deadline: Instant) -> Result<Value, AutomationError> {
        remaining(deadline, false)?;
        let short = action
            .strip_prefix(PROVIDER_NAME)
            .and_then(|value| value.strip_prefix('.'))
            .and_then(|value| value.strip_suffix("@1"))
            .ok_or_else(|| invalid("unknown macOS AX action"))?;
        let args = args
            .as_object()
            .ok_or_else(|| invalid("args must be an object"))?;
        match short {
            "list_apps" => self.list_apps(args, deadline),
            "snapshot" => self.snapshot(args, deadline),
            "find" => self.find(args, deadline),
            action if WRITE_ACTIONS.contains(&action) => self.write(action, args, deadline),
            _ => Err(invalid("unknown macOS AX action")),
        }
    }

    fn list_apps(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        only_keys(args, &[])?;
        let (trusted, apps) = self.backend.list_apps(deadline)?;
        let mut result = Map::from_iter([
            ("backend".into(), json!(self.backend.name())),
            ("accessibility_trusted".into(), json!(trusted)),
            ("apps".into(), json!(apps)),
        ]);
        if let Some(security) = self.backend.security_info() {
            result.insert("helper_security".into(), Value::Object(security));
        }
        Ok(Value::Object(result))
    }

    fn snapshot(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        only_keys(args, &["app", "max_depth", "max_nodes"])?;
        let app = app_selector(args.get("app"))?;
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
        Ok(self.capture(&app, max_depth, max_nodes, deadline)?.public)
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
            if item.native_token.is_empty() {
                return Err(driver_error(
                    "DRIVER.ACTION_FAILED",
                    "backend node omitted its native token",
                ));
            }
            let node_id = format!("n{index}");
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
            let protected = states.get("protected") == Some(&Value::Bool(true));
            let mut provenance = item.provenance;
            provenance.insert("backend".into(), json!(self.backend.name()));
            if protected {
                provenance.insert("value_redacted".into(), json!(true));
            }
            let actions = item
                .actions
                .into_iter()
                .filter(|action| WRITE_ACTIONS.contains(&action.as_str()))
                .filter(|action| {
                    !protected
                        || !matches!(action.as_str(), "pointer_click" | "set_value" | "type_text")
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let node = json!({
                "node_id": node_id, "parent_id": parent_id, "role": bounded_string(Some(item.role)),
                "subrole": bounded_string(item.subrole), "name": bounded_string(item.name),
                "description": bounded_string(item.description), "value": if protected { Value::Null } else { bounded_string(item.value) },
                "states": states, "bounds": item.bounds.map(Bounds::to_json), "actions": actions, "provenance": provenance
            });
            handles.insert(node_id.clone(), item.native_token);
            fingerprints.insert(node_id, fingerprint(&node));
            nodes.push(node);
        }
        let mut public = json!({"snapshot_id": snapshot_id, "revision": revision, "backend": self.backend.name(), "app": raw.app, "nodes": nodes, "truncated": raw.truncated});
        if let Some(security) = self.backend.security_info() {
            public["helper_security"] = Value::Object(security);
        }
        let record = Record {
            public,
            handles,
            fingerprints,
            app_selector: selector.clone(),
            max_depth,
            max_nodes,
        };
        state.current = Some(record.clone());
        Ok(record)
    }

    fn current(&self, snapshot: &Value, revision: &Value) -> Result<Record, AutomationError> {
        let snapshot = required_string(snapshot, "snapshot_id")?;
        let revision = revision
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("revision must be positive"))?;
        let state = self
            .state
            .lock()
            .map_err(|_| driver_error("DRIVER.ACTION_FAILED", "driver state is unavailable"))?;
        let record = state
            .current
            .as_ref()
            .ok_or_else(|| stale("snapshot is not current"))?;
        if record.public["snapshot_id"] != snapshot || record.public["revision"] != revision {
            return Err(stale("snapshot is not current"));
        }
        Ok(record.clone())
    }

    fn find(&self, args: &Map<String, Value>, deadline: Instant) -> Result<Value, AutomationError> {
        only_keys(args, &["snapshot_id", "revision", "locator"])?;
        let record = self.current(
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
        let id = node["node_id"].as_str().expect("node id");
        Ok(json!({"target": target(&record, id), "node": node}))
    }

    fn fresh_target(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<(Record, String, Value, String), AutomationError> {
        let target_value = args
            .get("target")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("target must be an object"))?;
        only_keys(target_value, &["snapshot_id", "revision", "node_id"])?;
        let record = self.current(
            target_value.get("snapshot_id").unwrap_or(&Value::Null),
            target_value.get("revision").unwrap_or(&Value::Null),
        )?;
        if record.public["truncated"] == true {
            return Err(driver_error(
                "DRIVER.SNAPSHOT_TRUNCATED",
                "truncated snapshots cannot authorize writes",
            ));
        }
        let id = required_string(
            target_value.get("node_id").unwrap_or(&Value::Null),
            "target.node_id",
        )?
        .to_string();
        let locator = parse_locator(args.get("locator"))?;
        let expected = resolve(&record, &locator, deadline)?;
        if expected["node_id"] != id || !record.handles.contains_key(&id) {
            return Err(stale("target does not match the snapshot locator result"));
        }
        let fingerprint = record.fingerprints[&id].clone();
        let old_token = record.handles[&id].clone();
        let fresh = self.capture(
            &record.app_selector,
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
        let node = resolve(&fresh, &locator, deadline).map_err(|error| {
            if matches!(error.code.as_str(), "DRIVER.NOT_FOUND" | "DRIVER.AMBIGUOUS") {
                stale("locator no longer resolves to its original target").with_cause(error)
            } else {
                error
            }
        })?;
        let fresh_id = node["node_id"].as_str().expect("node id").to_string();
        let token = fresh.handles[&fresh_id].clone();
        if !self
            .backend
            .same_element(&old_token, &token, deadline)
            .map_err(|error| {
                stale("native AX target identity could not be verified").with_cause(error)
            })?
            || fresh.fingerprints[&fresh_id] != fingerprint
        {
            return Err(stale(
                "locator resolved to a different native or semantic target",
            ));
        }
        Ok((fresh, fresh_id, node, token))
    }

    fn write(
        &self,
        action: &str,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let mut allowed = vec!["target", "locator"];
        if action == "set_value" {
            allowed.push("value");
        }
        if action == "type_text" {
            allowed.push("text");
        }
        if action == "pointer_click" {
            allowed.extend(["button", "position"]);
        }
        only_keys(args, &allowed)?;
        let value = if action == "set_value" {
            Some(bounded_text(args.get("value"), false)?)
        } else {
            None
        };
        let text = if action == "type_text" {
            Some(keyboard_text(args.get("text"))?)
        } else {
            None
        };
        let button = enum_value(args.get("button"), "left", "button")?;
        let position = enum_value(args.get("position"), "center", "position")?;
        let (fresh, fresh_id, node, token) = self.fresh_target(args, deadline)?;
        if matches!(action, "pointer_click" | "set_value" | "type_text")
            && node["states"]["protected"] == true
        {
            return Err(driver_error(
                "DRIVER.PROTECTED_ELEMENT",
                format!("protected element forbids {action}"),
            ));
        }
        if action == "type_text"
            && (!TYPE_TEXT_ROLES.contains(&node["role"].as_str().unwrap_or_default())
                || node["states"]["protected"] != false)
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "type_text requires a proven non-protected text target",
            ));
        }
        if action == "pointer_click" && parse_bounds(node.get("bounds")).is_err() {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "pointer_click requires positive-area bounds",
            ));
        }
        if !node["actions"]
            .as_array()
            .expect("actions")
            .iter()
            .any(|item| item == action)
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                format!("target does not support native {action}"),
            ));
        }
        remaining(deadline, false)?;
        let result = match action {
            "focus" => self.backend.focus(&token, deadline),
            "invoke" => self.backend.invoke(&token, deadline),
            "pointer_click" => self
                .backend
                .pointer_click(&token, button, position, deadline),
            "set_value" => {
                self.backend
                    .set_value(&token, value.expect("value validated"), deadline)
            }
            "type_text" => self
                .backend
                .type_text(&token, text.expect("text validated"), deadline),
            _ => unreachable!(),
        };
        let backend_result = match result {
            Ok(value) => {
                remaining(deadline, true)?;
                value
            }
            Err(error) if error.code == "DRIVER.UNKNOWN_EFFECT" => return Err(error),
            Err(error) if matches!(action, "type_text" | "pointer_click") => return Err(error),
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "DRIVER.ACTION_FAILED" | "DRIVER.TIMEOUT" | "DRIVER.UNAVAILABLE"
                ) =>
            {
                return Err(driver_error(
                    "DRIVER.UNKNOWN_EFFECT",
                    "native AX action result is unknown after dispatch",
                )
                .with_effect("unknown")
                .with_cause(error))
            }
            Err(error) => return Err(error),
        };
        if let Ok(mut state) = self.state.lock() {
            state.current = None;
        }
        Ok(
            json!({"ok": true, "action": action, "resolved": target(&fresh, &fresh_id), "backend_result": backend_result}),
        )
    }
}

impl Provider for MacOSAxProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn invoke(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AutomationError> {
        self.call(action, args, Self::deadline(timeout)?)
    }
    fn close(&self) {
        self.backend.close();
    }
}

pub fn manifest_json() -> Value {
    manifest_document()
}

#[cfg(target_os = "macos")]
pub fn native_provider() -> Result<MacOSAxProvider, AutomationError> {
    let backend: Arc<dyn AxBackend> = match helper::SwiftHelperBackend::new() {
        Ok(value) => Arc::new(value),
        Err(error) => Arc::new(UnavailableBackend { error }),
    };
    MacOSAxProvider::new(backend)
}

#[cfg(not(target_os = "macos"))]
pub fn native_provider() -> Result<MacOSAxProvider, AutomationError> {
    Err(driver_error(
        "DRIVER.UNAVAILABLE",
        "macOS AX is only available on macOS",
    ))
}

pub fn register(registry: &mut aad_runtime::ProviderRegistry) -> Result<(), AutomationError> {
    registry.insert(Arc::new(native_provider()?));
    Ok(())
}

#[cfg(target_os = "macos")]
struct UnavailableBackend {
    error: AutomationError,
}
#[cfg(target_os = "macos")]
impl UnavailableBackend {
    fn unavailable<T>(&self) -> Result<T, AutomationError> {
        Err(self.error.clone())
    }
}
#[cfg(target_os = "macos")]
impl AxBackend for UnavailableBackend {
    fn name(&self) -> &str {
        "macos_ax_unavailable"
    }
    fn list_apps(
        &self,
        _deadline: Instant,
    ) -> Result<(bool, Vec<Map<String, Value>>), AutomationError> {
        self.unavailable()
    }
    fn capture(
        &self,
        _app: &Map<String, Value>,
        _max_depth: u32,
        _max_nodes: usize,
        _deadline: Instant,
    ) -> Result<BackendSnapshot, AutomationError> {
        self.unavailable()
    }
    fn same_element(
        &self,
        _previous: &str,
        _current: &str,
        _deadline: Instant,
    ) -> Result<bool, AutomationError> {
        self.unavailable()
    }
    fn focus(&self, _token: &str, _deadline: Instant) -> Result<Value, AutomationError> {
        self.unavailable()
    }
    fn invoke(&self, _token: &str, _deadline: Instant) -> Result<Value, AutomationError> {
        self.unavailable()
    }
    fn pointer_click(
        &self,
        _token: &str,
        _button: &str,
        _position: &str,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.unavailable()
    }
    fn set_value(
        &self,
        _token: &str,
        _value: &str,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.unavailable()
    }
    fn type_text(
        &self,
        _token: &str,
        _text: &str,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.unavailable()
    }
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
        Err(invalid("text is outside the allowed range"))
    } else {
        Ok(value)
    }
}
fn keyboard_text(value: Option<&Value>) -> Result<&str, AutomationError> {
    let value = bounded_text(value, true)?;
    let utf16 = value.encode_utf16().count();
    if value.chars().count() > MAX_TYPE_TEXT_CHARS
        || utf16 > MAX_TYPE_TEXT_UTF16_UNITS
        || value.chars().any(char::is_control)
    {
        Err(invalid(
            "type_text accepts only bounded Unicode text without controls",
        ))
    } else {
        Ok(value)
    }
}
fn enum_value<'a>(
    value: Option<&'a Value>,
    default: &'a str,
    name: &str,
) -> Result<&'a str, AutomationError> {
    match value {
        None => Ok(default),
        Some(Value::String(value)) if value == default => Ok(value),
        _ => Err(invalid(format!("{name} supports only {default}"))),
    }
}
fn bounded_integer(
    value: Option<&Value>,
    default: u64,
    min: u64,
    max: u64,
    name: &str,
) -> Result<u64, AutomationError> {
    value
        .map_or(Some(default), Value::as_u64)
        .filter(|value| *value >= min && *value <= max)
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

fn app_selector(value: Option<&Value>) -> Result<Map<String, Value>, AutomationError> {
    let object = value
        .and_then(Value::as_object)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("app must contain an exact selector"))?;
    only_keys(object, &["process_id", "bundle_id", "name"])?;
    let mut result = Map::new();
    if let Some(value) = object.get("process_id") {
        let value = value
            .as_u64()
            .filter(|value| *value > 0 && *value <= i32::MAX as u64)
            .ok_or_else(|| invalid("app.process_id must be positive"))?;
        result.insert("process_id".into(), json!(value));
    }
    for name in ["bundle_id", "name"] {
        if let Some(value) = object.get(name) {
            result.insert(
                name.into(),
                json!(required_string(value, &format!("app.{name}"))?),
            );
        }
    }
    Ok(result)
}

fn parse_locator(value: Option<&Value>) -> Result<Map<String, Value>, AutomationError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("locator must be an object"))?;
    only_keys(
        object,
        &[
            "role",
            "subrole",
            "name",
            "description",
            "value",
            "identifier",
            "states",
            "actions",
            "match",
        ],
    )?;
    if object.keys().all(|key| key == "match") {
        return Err(invalid("locator requires at least one condition"));
    }
    if object.get("match").is_some_and(|value| value != "exact") {
        return Err(invalid("only exact locators are supported"));
    }
    let mut result = Map::from_iter([("match".into(), json!("exact"))]);
    for name in ["role", "identifier"] {
        if let Some(value) = object.get(name) {
            result.insert(
                name.into(),
                json!(required_string(value, &format!("locator.{name}"))?),
            );
        }
    }
    for name in ["subrole", "name", "description", "value"] {
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
            return Err(invalid("locator states must be boolean or null"));
        }
        result.insert("states".into(), value.clone());
    }
    if let Some(value) = object.get("actions") {
        let actions = value
            .as_array()
            .filter(|value| !value.is_empty())
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
    for name in ["role", "subrole", "name", "description", "value"] {
        if locator
            .get(name)
            .is_some_and(|wanted| node.get(name) != Some(wanted))
        {
            return false;
        }
    }
    if locator
        .get("identifier")
        .is_some_and(|wanted| node["provenance"].get("identifier") != Some(wanted))
    {
        return false;
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
    let actions = node["actions"].as_array().expect("actions");
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
    let mut found = Vec::new();
    for node in record.public["nodes"].as_array().expect("nodes") {
        remaining(deadline, false)?;
        if node_matches(node, locator) {
            found.push(node.clone());
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(driver_error("DRIVER.NOT_FOUND", "locator matched no node")),
        many => Err(
            driver_error("DRIVER.AMBIGUOUS", "locator matched multiple nodes")
                .with_detail("candidate_count", json!(many.len())),
        ),
    }
}
fn target(record: &Record, id: &str) -> Value {
    json!({"snapshot_id": record.public["snapshot_id"], "revision": record.public["revision"], "node_id": id})
}
fn fingerprint(node: &Value) -> String {
    let identity = json!({"role": node["role"], "subrole": node["subrole"], "name": node["name"], "identifier": node["provenance"]["identifier"], "process_id": node["provenance"]["process_id"], "bundle_id": node["provenance"]["bundle_id"]});
    format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&identity).expect("identity JSON"))
    )
}
fn parse_bounds(value: Option<&Value>) -> Result<Bounds, AutomationError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| driver_error("DRIVER.ACTION_UNSUPPORTED", "target has no bounds"))?;
    let get = |name| {
        object
            .get(name)
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| driver_error("DRIVER.ACTION_UNSUPPORTED", "target bounds are invalid"))
    };
    let bounds = Bounds {
        x: get("x")?,
        y: get("y")?,
        width: get("width")?,
        height: get("height")?,
    };
    if bounds.width <= 0 || bounds.height <= 0 {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "target bounds have no positive area",
        ))
    } else {
        Ok(bounds)
    }
}
fn remaining(deadline: Instant, post: bool) -> Result<Duration, AutomationError> {
    let value = deadline.saturating_duration_since(Instant::now());
    if value.is_zero() {
        Err(timeout_error(post))
    } else {
        Ok(value)
    }
}
fn timeout_error(post: bool) -> AutomationError {
    driver_error("DRIVER.TIMEOUT", "request deadline elapsed")
        .with_retryable(!post)
        .with_effect(if post { "unknown" } else { "not_applied" })
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
    json!({"type": "object", "properties": {"role": {"type": "string", "maxLength": 256}, "subrole": {"type": ["string", "null"], "maxLength": 256}, "name": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS}, "description": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS}, "value": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS}, "identifier": {"type": "string", "maxLength": MAX_FIELD_CHARS}, "states": {"type": "object", "minProperties": 1, "properties": STATE_NAMES.iter().map(|name| ((*name).into(), json!({"type": ["boolean", "null"]}))).collect::<Map<_,_>>(), "additionalProperties": false}, "actions": {"type": "array", "minItems": 1, "items": {"enum": WRITE_ACTIONS}, "uniqueItems": true}, "match": {"const": "exact"}}, "additionalProperties": false, "minProperties": 1})
}
fn app_schema() -> Value {
    json!({"type": "object", "minProperties": 1, "properties": {"process_id": {"type": "integer", "minimum": 1}, "bundle_id": {"type": "string", "minLength": 1, "maxLength": MAX_FIELD_CHARS}, "name": {"type": "string", "minLength": 1, "maxLength": MAX_FIELD_CHARS}}, "additionalProperties": false})
}
fn error_contracts(locator: bool, write: bool) -> Vec<Value> {
    let mut values = vec![
        (
            "DRIVER.INVALID_REQUEST",
            "动作参数无效。",
            false,
            "not_applied",
        ),
        (
            "DRIVER.UNAVAILABLE",
            "macOS AX helper 或 Accessibility 权限不可用。",
            false,
            "not_applied",
        ),
        (
            "DRIVER.ACTION_FAILED",
            "原生 AX 观察操作失败。",
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
    if write {
        values.extend([
            (
                "DRIVER.ACTION_UNSUPPORTED",
                "目标不支持所需原生 AX 属性或动作。",
                false,
                "not_applied",
            ),
            (
                "DRIVER.PROTECTED_ELEMENT",
                "目标暴露受保护内容。",
                false,
                "not_applied",
            ),
            (
                "DRIVER.UNKNOWN_EFFECT",
                "原生动作可能已生效。",
                false,
                "unknown",
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
    locator: bool,
    write: bool,
) -> Value {
    json!({"contract_major": 1, "description": description, "effect": {"default_class": effect}, "risk": {"category": category, "level": level}, "permissions": permissions, "input_schema": input, "output_schema": output, "errors": error_contracts(locator, write)})
}

fn manifest_document() -> Value {
    let write_input = || json!({"type": "object", "required": ["target", "locator"], "properties": {"target": target_schema(), "locator": locator_schema()}, "additionalProperties": false});
    let write_output = || json!({"type": "object", "required": ["ok", "action", "resolved"], "properties": {"ok": {"const": true}, "action": {"enum": WRITE_ACTIONS}, "resolved": target_schema(), "backend_result": {}}, "additionalProperties": false});
    let snapshot_output = json!({"type": "object", "required": ["snapshot_id", "revision", "backend", "app", "nodes", "truncated"], "properties": {"snapshot_id": {"type": "string"}, "revision": {"type": "integer", "minimum": 1}, "backend": {"type": "string"}, "app": {"type": "object"}, "nodes": {"type": "array", "items": {"type": "object"}}, "truncated": {"type": "boolean"}, "helper_security": {"type": "object"}}, "additionalProperties": false});
    let mut actions = Map::new();
    actions.insert("list_apps".into(), contract("通过完整性校验后的 Swift helper 枚举当前 Aqua 会话的运行中应用。", "read_only", "observe", "low", &["desktop.observe"], json!({"type": "object", "additionalProperties": false}), json!({"type": "object", "required": ["backend", "accessibility_trusted", "apps"], "properties": {"backend": {"type": "string"}, "accessibility_trusted": {"type": "boolean"}, "apps": {"type": "array", "items": {"type": "object"}}, "helper_security": {"type": "object"}}, "additionalProperties": false}), false, false));
    actions.insert("snapshot".into(), contract("抓取一个精确应用选择器对应的有界 macOS AX 树。", "read_only", "observe", "low", &["desktop.observe"], json!({"type": "object", "required": ["app"], "properties": {"app": app_schema(), "max_depth": {"type": "integer", "minimum": 0, "maximum": MAX_DEPTH}, "max_nodes": {"type": "integer", "minimum": 1, "maximum": MAX_NODES}}, "additionalProperties": false}), snapshot_output, false, false));
    actions["snapshot"]["errors"]
        .as_array_mut()
        .unwrap()
        .extend(error_contracts(true, false).into_iter().skip(5).take(2));
    actions.insert("find".into(), contract("在当前完整快照中解析仅支持精确匹配的定位器。", "read_only", "observe", "low", &["desktop.observe"], json!({"type": "object", "required": ["snapshot_id", "revision", "locator"], "properties": {"snapshot_id": {"type": "string", "minLength": 1}, "revision": {"type": "integer", "minimum": 1}, "locator": locator_schema()}, "additionalProperties": false}), json!({"type": "object", "required": ["target", "node"], "properties": {"target": target_schema(), "node": {"type": "object"}}, "additionalProperties": false}), true, false));
    for (name, effect, category, level, description) in [
        (
            "focus",
            "contextual",
            "navigate",
            "medium",
            "重新验证目标后设置原生 AXFocused 属性。",
        ),
        (
            "invoke",
            "non_idempotent",
            "modify",
            "high",
            "重新验证目标后执行原生 AXPress 动作。",
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
                write_input(),
                write_output(),
                true,
                true,
            ),
        );
    }
    actions.insert("pointer_click".into(), contract("重新验证目标后，仅按元素正面积 bounds 的中心点显式派发左键 pointer click。", "non_idempotent", "modify", "high", &["desktop.observe", "desktop.input"], json!({"type": "object", "required": ["target", "locator"], "properties": {"target": target_schema(), "locator": locator_schema(), "button": {"enum": ["left"]}, "position": {"enum": ["center"]}}, "additionalProperties": false}), write_output(), true, true));
    for (name, field, description) in [
        (
            "set_value",
            "value",
            "重新验证目标后设置原生 AXValue 属性。",
        ),
        (
            "type_text",
            "text",
            "重新验证并聚焦非受保护文本目标后，显式发送有界 Unicode 键盘输入。",
        ),
    ] {
        let mut property = json!({"type": "string", "maxLength": if name == "type_text" { MAX_TYPE_TEXT_CHARS } else { MAX_FIELD_CHARS }});
        if name == "type_text" {
            property["minLength"] = json!(1);
        }
        actions.insert(name.into(), contract(description, "contextual", "input", "high", &["desktop.observe", "desktop.input"], json!({"type": "object", "required": ["target", "locator", field], "properties": {"target": target_schema(), "locator": locator_schema(), field: property}, "additionalProperties": false}), write_output(), true, true));
    }
    json!({"apiVersion": "ai-auto-desktop.dev/v1alpha1", "kind": "CapabilityManifest", "metadata": {"name": PROVIDER_NAME, "version": PROVIDER_VERSION, "description": "macOS 原生 Accessibility API 进程驱动（完整性校验的 Swift helper）。"}, "actions": actions, "runtime": {"kind": "builtin", "protocol": "rust-provider-v1", "platforms": ["macos"]}})
}

// The adapter itself is ordinary Unix process I/O. Compile it in tests and on
// Linux too so CI catches protocol regressions without pretending Linux can
// provide AX. Only `native_provider` constructs it on macOS.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod helper;

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
        fn app() -> Map<String, Value> {
            Map::from_iter([
                ("process_id".into(), json!(7)),
                ("bundle_id".into(), json!("dev.example.Editor")),
                ("name".into(), json!("Editor")),
            ])
        }

        fn node(&self, token: &str) -> BackendNode {
            let changed = self.changed && self.captures.load(Ordering::SeqCst) > 1;
            BackendNode {
                native_token: token.into(),
                parent_index: None,
                role: "AXButton".into(),
                subrole: None,
                name: Some(if changed { "Changed" } else { "Save" }.into()),
                description: Some("Save".into()),
                value: None,
                states: BTreeMap::from_iter([
                    ("enabled".into(), Some(true)),
                    ("focused".into(), Some(false)),
                    ("focusable".into(), Some(true)),
                    ("editable".into(), Some(false)),
                    ("protected".into(), Some(false)),
                ]),
                bounds: Some(Bounds {
                    x: 10,
                    y: 20,
                    width: 120,
                    height: 30,
                }),
                actions: vec!["focus".into(), "invoke".into(), "pointer_click".into()],
                provenance: Map::from_iter([
                    ("identifier".into(), json!("save")),
                    ("process_id".into(), json!(7)),
                    ("bundle_id".into(), json!("dev.example.Editor")),
                    ("coordinate_space".into(), json!("screen_points")),
                    ("value_redacted".into(), json!(false)),
                ]),
            }
        }
    }

    impl AxBackend for FakeBackend {
        fn name(&self) -> &str {
            "fake_macos_ax"
        }
        fn security_info(&self) -> Option<Map<String, Value>> {
            Some(Map::from_iter([("integrity_verified".into(), json!(true))]))
        }
        fn list_apps(
            &self,
            _deadline: Instant,
        ) -> Result<(bool, Vec<Map<String, Value>>), AutomationError> {
            Ok((true, vec![Self::app()]))
        }
        fn capture(
            &self,
            _app: &Map<String, Value>,
            _max_depth: u32,
            _max_nodes: usize,
            _deadline: Instant,
        ) -> Result<BackendSnapshot, AutomationError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            let mut nodes = vec![self.node("save")];
            if self.duplicate {
                nodes.push(self.node("save2"));
            }
            Ok(BackendSnapshot {
                app: Self::app(),
                nodes,
                truncated: self.truncated,
            })
        }
        fn same_element(
            &self,
            previous: &str,
            current: &str,
            _deadline: Instant,
        ) -> Result<bool, AutomationError> {
            Ok(previous == current)
        }
        fn focus(&self, _token: &str, _deadline: Instant) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"native_operation": "AXFocused", "accepted": true}))
        }
        fn invoke(&self, _token: &str, _deadline: Instant) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"native_operation": "AXPress", "accepted": true}))
        }
        fn pointer_click(
            &self,
            _token: &str,
            _button: &str,
            _position: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"native_operation": "CGEventLeftClick", "submitted": true}))
        }
        fn set_value(
            &self,
            _token: &str,
            _value: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"native_operation": "AXValue", "accepted": true}))
        }
        fn type_text(
            &self,
            _token: &str,
            _text: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"native_operation": "CGEventKeyboardSetUnicodeString", "submitted": true}))
        }
    }

    fn take_snapshot(provider: &MacOSAxProvider) -> Value {
        provider
            .invoke(
                "desktop.macos_ax.snapshot@1",
                json!({"app": {"process_id": 7}, "max_depth": 8, "max_nodes": 32}),
                Some(Duration::from_secs(2)),
            )
            .unwrap()
    }
    fn locator() -> Value {
        json!({"role": "AXButton", "name": "Save", "identifier": "save"})
    }

    #[test]
    fn manifest_exposes_the_complete_ax_surface() {
        let parsed = manifest::parse(&manifest_document()).unwrap();
        for action in [
            "list_apps",
            "snapshot",
            "find",
            "focus",
            "invoke",
            "pointer_click",
            "set_value",
            "type_text",
        ] {
            assert!(
                parsed
                    .resolve(&format!("{PROVIDER_NAME}.{action}@1"))
                    .is_some(),
                "{action}"
            );
        }
    }

    #[test]
    fn list_snapshot_and_find_preserve_contract_shape() {
        let provider = MacOSAxProvider::new(Arc::new(FakeBackend::default())).unwrap();
        let apps = provider
            .invoke(
                "desktop.macos_ax.list_apps@1",
                json!({}),
                Some(Duration::from_secs(2)),
            )
            .unwrap();
        assert_eq!(apps["accessibility_trusted"], true);
        assert_eq!(apps["helper_security"]["integrity_verified"], true);
        let snapshot = take_snapshot(&provider);
        let found = provider.invoke("desktop.macos_ax.find@1", json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "locator": locator()}), Some(Duration::from_secs(2))).unwrap();
        assert_eq!(found["target"]["node_id"], "n0");
        assert!(found["node"].get("native_token").is_none());
    }

    #[test]
    fn writes_refresh_and_reject_semantic_staleness() {
        let backend = Arc::new(FakeBackend::default());
        let provider = MacOSAxProvider::new(backend.clone()).unwrap();
        let snapshot = take_snapshot(&provider);
        let result = provider.invoke("desktop.macos_ax.invoke@1", json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"}, "locator": locator()}), Some(Duration::from_secs(2))).unwrap();
        assert_eq!(result["backend_result"]["native_operation"], "AXPress");
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);

        let backend = Arc::new(FakeBackend {
            changed: true,
            ..Default::default()
        });
        let provider = MacOSAxProvider::new(backend.clone()).unwrap();
        let snapshot = take_snapshot(&provider);
        let error = provider.invoke("desktop.macos_ax.invoke@1", json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"}, "locator": locator()}), Some(Duration::from_secs(2))).unwrap_err();
        assert_eq!(error.code, "DRIVER.STALE_SNAPSHOT");
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ambiguity_truncation_and_bad_keyboard_text_fail_closed() {
        let provider = MacOSAxProvider::new(Arc::new(FakeBackend {
            duplicate: true,
            ..Default::default()
        }))
        .unwrap();
        let snapshot = take_snapshot(&provider);
        assert_eq!(provider.invoke("desktop.macos_ax.find@1", json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "locator": locator()}), Some(Duration::from_secs(2))).unwrap_err().code, "DRIVER.AMBIGUOUS");
        let provider = MacOSAxProvider::new(Arc::new(FakeBackend {
            truncated: true,
            ..Default::default()
        }))
        .unwrap();
        let snapshot = take_snapshot(&provider);
        assert_eq!(provider.invoke("desktop.macos_ax.find@1", json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "locator": locator()}), Some(Duration::from_secs(2))).unwrap_err().code, "DRIVER.SNAPSHOT_TRUNCATED");
        for value in [json!(""), json!("bad\0text"), json!("😀".repeat(1025))] {
            assert_eq!(
                keyboard_text(Some(&value)).unwrap_err().code,
                "DRIVER.INVALID_REQUEST"
            );
        }
    }
}
