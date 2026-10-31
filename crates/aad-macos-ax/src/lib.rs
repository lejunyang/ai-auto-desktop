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

pub mod result_verifier;

pub const PROVIDER_NAME: &str = "desktop.macos_ax";
pub const PROVIDER_VERSION: &str = "0.1.0";
const MAX_FIELD_CHARS: usize = 4096;
const MAX_TYPE_TEXT_CHARS: usize = 1024;
const MAX_TYPE_TEXT_UTF16_UNITS: usize = 2048;
const DEFAULT_MAX_DEPTH: u32 = 32;
const DEFAULT_MAX_NODES: usize = 1000;
const MAX_DEPTH: u32 = 128;
const MAX_NODES: usize = 5000;
const WRITE_ACTIONS: &[&str] = &[
    "focus",
    "invoke",
    "pointer_click",
    "pointer_gesture",
    "drag",
    "key_chord",
    "set_value",
    "type_text",
    "paste",
    "minimize",
    "maximize",
    "restore",
    "close_window",
    "activate",
];
/// Writes that send synthetic input and so are never offered on protected elements.
const INPUT_ACTIONS: &[&str] = &[
    "pointer_click",
    "pointer_gesture",
    "drag",
    "key_chord",
    "set_value",
    "type_text",
    "paste",
];
/// Actions on an AXWindow node, carried out through its AX attributes and
/// buttons and verified by reading the window back.
pub const WINDOW_ACTIONS: &[&str] = &[
    "minimize",
    "maximize",
    "restore",
    "close_window",
    "activate",
];
const MAX_PASTE_CHARS: usize = 4096;
const DEFAULT_CLIPBOARD_CHARS: usize = 16_384;
const MAX_CLIPBOARD_CHARS: usize = 65_536;
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
    /// `gesture` is already validated: `mode` click, wheel or hover with its
    /// fields, plus `fx`/`fy` inside the element.
    fn pointer_gesture(
        &self,
        _token: &str,
        _gesture: &Value,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend has no pointer gestures",
        ))
    }
    /// `request` holds `button`, `fx`, `fy`, `to_fx` and `to_fy`.
    fn drag(
        &self,
        _token: &str,
        _to_token: &str,
        _request: &Value,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend has no drag",
        ))
    }
    fn key_chord(
        &self,
        _token: &str,
        _keys: &[String],
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend has no key chords",
        ))
    }
    /// Start watching what a person does in the application with this process id.
    fn start_recording(
        &self,
        _process_id: u32,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend cannot record",
        ))
    }
    /// The raw events seen since the last call: `{events, dropped}`.
    fn collect_recording(&self, _id: &str, _deadline: Instant) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend cannot record",
        ))
    }
    fn stop_recording(&self, _id: &str, _deadline: Instant) -> Result<(), AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend cannot record",
        ))
    }
    /// One of [`WINDOW_ACTIONS`] on an AXWindow, answered once the window
    /// reports the requested state.
    fn window_action(
        &self,
        _token: &str,
        _action: &str,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend has no window actions",
        ))
    }
    /// Paste `text` into the focused element through the general pasteboard,
    /// putting the previous contents back afterwards.
    fn paste(
        &self,
        _token: &str,
        _text: &str,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend cannot paste",
        ))
    }
    /// A PNG of the element's window content at the element's bounds:
    /// `{format, data, width, height, clipped}`.
    fn capture_element(&self, _token: &str, _deadline: Instant) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend cannot capture",
        ))
    }
    /// The general pasteboard's text: `{has_text, text, characters, truncated}`.
    fn clipboard_text(
        &self,
        _max_characters: usize,
        _deadline: Instant,
    ) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend cannot read the pasteboard",
        ))
    }
    /// A key chord posted to the session, anchored on no element.
    fn global_key(&self, _keys: &[String], _deadline: Instant) -> Result<Value, AutomationError> {
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "this backend has no global keys",
        ))
    }
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
            "capture" => self.capture_inline(args, deadline),
            "clipboard_read" => {
                only_keys(args, &["max_characters"])?;
                let max = bounded_integer(
                    args.get("max_characters"),
                    DEFAULT_CLIPBOARD_CHARS as u64,
                    1,
                    MAX_CLIPBOARD_CHARS as u64,
                    "max_characters",
                )? as usize;
                let read = self.backend.clipboard_text(max, deadline)?;
                if !read["has_text"].is_boolean()
                    || !read["characters"].is_u64()
                    || !read["truncated"].is_boolean()
                    || !(read["text"].is_string() || read["text"].is_null())
                {
                    return Err(driver_error(
                        "DRIVER.ACTION_FAILED",
                        "pasteboard read returned an invalid result",
                    ));
                }
                let text = read["text"]
                    .as_str()
                    .map(|text| json!(text.chars().take(max).collect::<String>()))
                    .unwrap_or(Value::Null);
                Ok(
                    json!({"has_text": read["has_text"], "text": text, "characters": read["characters"], "truncated": read["truncated"]}),
                )
            }
            "global_key" => {
                only_keys(args, &["keys"])?;
                let keys = key_names(args.get("keys"))?;
                refuse_reserved_chord(&keys)?;
                self.backend.global_key(&keys, deadline)
            }
            "watch" => {
                only_keys(args, &["app"])?;
                let app = app_selector(args.get("app"))?;
                let process_id = app
                    .get("process_id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("watch.app needs a process_id"))?;
                let started = self.backend.start_recording(process_id as u32, deadline)?;
                Ok(
                    json!({"capture_id": started["recording_id"], "pointer_watch": started["pointer_watch"]}),
                )
            }
            "collect" | "release" => {
                only_keys(args, &["capture_id"])?;
                let id = args
                    .get("capture_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 64)
                    .ok_or_else(|| invalid("capture_id is required"))?;
                if short == "collect" {
                    self.backend.collect_recording(id, deadline)
                } else {
                    self.backend.stop_recording(id, deadline)?;
                    Ok(json!({"released": true}))
                }
            }
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
                .filter(|action| !protected || !INPUT_ACTIONS.contains(&action.as_str()))
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

    /// A PNG of one observed element, after the same revalidation as a write.
    /// Refuses elements with protected content in their subtree. The caller's
    /// observation stays current: a read does not spend it.
    fn capture_inline(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        only_keys(args, &["target", "locator"])?;
        let observed = {
            let target = args
                .get("target")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid("target must be an object"))?;
            self.current(
                target.get("snapshot_id").unwrap_or(&Value::Null),
                target.get("revision").unwrap_or(&Value::Null),
            )?
        };
        let checked = (|| {
            let (fresh, fresh_id, node, token) = self.fresh_target(args, deadline)?;
            let inside = descendants(&fresh, &fresh_id);
            let protected =
                fresh.public["nodes"]
                    .as_array()
                    .expect("nodes")
                    .iter()
                    .any(|candidate| {
                        candidate["node_id"]
                            .as_str()
                            .is_some_and(|id| inside.contains(id))
                            && (candidate["states"]["protected"] != false
                                || candidate["provenance"]["value_redacted"] == true)
                    });
            if protected {
                return Err(driver_error(
                    "DRIVER.PROTECTED_ELEMENT",
                    "capture target is not proven free of protected content",
                ));
            }
            parse_bounds(node.get("bounds")).map_err(|_| {
                driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "capture requires positive-area bounds",
                )
            })?;
            let shot = self.backend.capture_element(&token, deadline)?;
            let valid = shot["format"] == "png"
                && shot["data"].as_str().is_some_and(|data| !data.is_empty())
                && shot["width"].as_u64().is_some_and(|w| w > 0)
                && shot["height"].as_u64().is_some_and(|h| h > 0)
                && shot["clipped"].is_boolean();
            if !valid {
                return Err(driver_error(
                    "DRIVER.CAPTURE_FAILED",
                    "macOS AX helper returned an invalid capture",
                ));
            }
            Ok(json!({
                "format": "png",
                "data": shot["data"],
                "width": shot["width"],
                "height": shot["height"],
                "clipped": shot["clipped"],
                "node_id": fresh_id,
            }))
        })();
        if let Ok(mut state) = self.state.lock() {
            state.current = Some(observed);
        }
        checked
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
        if matches!(action, "type_text" | "paste") {
            allowed.push("text");
        }
        match action {
            "pointer_click" => allowed.extend(["button", "position"]),
            "pointer_gesture" => {
                allowed.extend(["button", "count", "fraction", "scroll", "hover_ms"])
            }
            "drag" => allowed.extend(["to", "to_locator", "button", "fraction", "to_fraction"]),
            "key_chord" => allowed.push("keys"),
            _ => {}
        }
        only_keys(args, &allowed)?;
        // Validated before anything is captured, so a malformed request costs nothing.
        let gesture = match action {
            "pointer_gesture" => Some(gesture_request(args)?),
            "drag" => Some(drag_request(args)?),
            _ => None,
        };
        let keys = if action == "key_chord" {
            Some(key_names(args.get("keys"))?)
        } else {
            None
        };
        let value = if action == "set_value" {
            Some(bounded_text(args.get("value"), false)?)
        } else {
            None
        };
        let text = match action {
            "type_text" => Some(keyboard_text(args.get("text"))?),
            "paste" => Some(paste_text(args.get("text"))?),
            _ => None,
        };
        let (button, position) = if action == "pointer_click" {
            (
                enum_value(args.get("button"), "left", "button")?,
                enum_value(args.get("position"), "center", "position")?,
            )
        } else {
            ("left", "center")
        };
        let (fresh, fresh_id, node, token) = self.fresh_target(args, deadline)?;
        if INPUT_ACTIONS.contains(&action) && node["states"]["protected"] == true {
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
        if action == "paste" && node["states"]["editable"] != true {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "paste requires an editable, non-protected element",
            ));
        }
        if WINDOW_ACTIONS.contains(&action) && node["role"] != "AXWindow" {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                format!("{action} is offered only on an AXWindow node"),
            ));
        }
        if matches!(action, "pointer_click" | "pointer_gesture" | "drag")
            && parse_bounds(node.get("bounds")).is_err()
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                format!("{action} requires positive-area bounds"),
            ));
        }
        // The destination of a drag is re-verified exactly like the source.
        let destination = if action == "drag" {
            let mut to_args = Map::new();
            to_args.insert(
                "target".into(),
                args.get("to").cloned().unwrap_or(Value::Null),
            );
            to_args.insert(
                "locator".into(),
                args.get("to_locator").cloned().unwrap_or(Value::Null),
            );
            let (_, to_id, to_node, to_token) = self.fresh_target(&to_args, deadline)?;
            if to_id == fresh_id {
                return Err(invalid("drag needs a destination other than its source"));
            }
            if to_node["states"]["protected"] == true
                || parse_bounds(to_node.get("bounds")).is_err()
            {
                return Err(driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "drag destination must be unprotected with positive-area bounds",
                ));
            }
            Some(to_token)
        } else {
            None
        };
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
            "pointer_gesture" => self.backend.pointer_gesture(
                &token,
                gesture.as_ref().expect("gesture validated"),
                deadline,
            ),
            "drag" => self.backend.drag(
                &token,
                destination.as_deref().expect("destination verified"),
                gesture.as_ref().expect("drag validated"),
                deadline,
            ),
            "key_chord" => {
                self.backend
                    .key_chord(&token, keys.as_deref().expect("keys validated"), deadline)
            }
            "paste" => self
                .backend
                .paste(&token, text.expect("text validated"), deadline),
            window if WINDOW_ACTIONS.contains(&window) => {
                self.backend.window_action(&token, window, deadline)
            }
            _ => unreachable!(),
        };
        let backend_result = match result {
            Ok(value) => {
                remaining(deadline, true)?;
                value
            }
            Err(error) if error.code == "DRIVER.UNKNOWN_EFFECT" => return Err(error),
            // The helper reports how far input got, so its own effect stands.
            Err(error)
                if matches!(
                    action,
                    "type_text"
                        | "pointer_click"
                        | "pointer_gesture"
                        | "drag"
                        | "key_chord"
                        | "paste"
                ) =>
            {
                return Err(error)
            }
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

fn fraction_pair(value: Option<&Value>) -> Result<(f64, f64), AutomationError> {
    match value {
        None | Some(Value::Null) => Ok((0.5, 0.5)),
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("fraction must be an object"))?;
            only_keys(object, &["fx", "fy"])?;
            let part = |name: &str| {
                object
                    .get(name)
                    .and_then(Value::as_f64)
                    .filter(|f| (0.0..=1.0).contains(f))
                    .ok_or_else(|| invalid("fraction.fx and fraction.fy must lie in 0..1"))
            };
            Ok((part("fx")?, part("fy")?))
        }
    }
}

fn button_name(value: Option<&Value>) -> Result<&'static str, AutomationError> {
    match value {
        None | Some(Value::Null) => Ok("left"),
        Some(value) => match value.as_str() {
            Some("left") => Ok("left"),
            Some("right") => Ok("right"),
            Some("middle") => Ok("middle"),
            _ => Err(invalid("button must be left, right or middle")),
        },
    }
}

/// The Windows driver's request shape, normalised for the Swift helper.
fn gesture_request(args: &Map<String, Value>) -> Result<Value, AutomationError> {
    let given = |name: &str| args.get(name).is_some_and(|value| !value.is_null());
    let (fx, fy) = fraction_pair(args.get("fraction"))?;
    let mut request = if let Some(scroll) = args.get("scroll").filter(|v| !v.is_null()) {
        if given("button") || given("count") || given("hover_ms") {
            return Err(invalid("a scroll takes no button, count or hover_ms"));
        }
        let scroll = scroll
            .as_object()
            .ok_or_else(|| invalid("scroll must be an object"))?;
        only_keys(scroll, &["direction", "notches"])?;
        let direction = scroll
            .get("direction")
            .and_then(Value::as_str)
            .filter(|d| matches!(*d, "up" | "down" | "left" | "right"))
            .ok_or_else(|| invalid("scroll.direction must be up, down, left or right"))?;
        let notches = match scroll.get("notches") {
            None | Some(Value::Null) => 3,
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=30).contains(n))
                .ok_or_else(|| invalid("scroll.notches must be 1..30"))?,
        };
        json!({"mode": "wheel", "direction": direction, "notches": notches})
    } else if let Some(ms) = args.get("hover_ms").filter(|v| !v.is_null()) {
        if given("button") || given("count") {
            return Err(invalid("a hover takes no button or count"));
        }
        let ms = ms
            .as_u64()
            .filter(|ms| (1..=5000).contains(ms))
            .ok_or_else(|| invalid("hover_ms must be 1..5000"))?;
        json!({"mode": "hover", "hover_ms": ms})
    } else {
        let count = match args.get("count") {
            None | Some(Value::Null) => 1,
            Some(value) => value
                .as_u64()
                .filter(|n| matches!(n, 1 | 2))
                .ok_or_else(|| invalid("count must be 1 or 2"))?,
        };
        json!({"mode": "click", "button": button_name(args.get("button"))?, "count": count})
    };
    request["fx"] = json!(fx);
    request["fy"] = json!(fy);
    Ok(request)
}

fn drag_request(args: &Map<String, Value>) -> Result<Value, AutomationError> {
    let (fx, fy) = fraction_pair(args.get("fraction"))?;
    let (to_fx, to_fy) = fraction_pair(args.get("to_fraction"))?;
    Ok(
        json!({"button": button_name(args.get("button"))?, "fx": fx, "fy": fy, "to_fx": to_fx, "to_fy": to_fy}),
    )
}

fn key_names(value: Option<&Value>) -> Result<Vec<String>, AutomationError> {
    let keys = value
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("keys must be an array"))?;
    if keys.is_empty() || keys.len() > 6 {
        return Err(invalid("keys must hold 1..6 names"));
    }
    keys.iter()
        .map(|key| {
            key.as_str()
                .filter(|key| {
                    !key.is_empty()
                        && key.len() <= 32
                        && key.chars().all(|c| c.is_ascii_alphanumeric())
                })
                .map(str::to_string)
                .ok_or_else(|| invalid("each key is a short alphanumeric name"))
        })
        .collect()
}

/// Text for `paste`: 1..4096 characters, no NUL.
fn paste_text(value: Option<&Value>) -> Result<&str, AutomationError> {
    let text = bounded_text(value, true)?;
    if text.chars().count() > MAX_PASTE_CHARS {
        return Err(invalid("paste text is 1..4096 characters"));
    }
    Ok(text)
}

/// Chords a global key never presses: the lock screen (Ctrl+Cmd+Q), log out
/// (Shift+Cmd+Q, with or without Option) and Force Quit (Option+Cmd+Esc).
fn refuse_reserved_chord(keys: &[String]) -> Result<(), AutomationError> {
    let lower: Vec<String> = keys.iter().map(|key| key.to_ascii_lowercase()).collect();
    let has = |names: &[&str]| lower.iter().any(|key| names.contains(&key.as_str()));
    let main = lower.last().map(String::as_str).unwrap_or_default();
    let cmd = has(&["cmd", "command", "meta", "super", "win"]);
    let reserved = cmd
        && ((main == "q" && (has(&["ctrl", "control"]) || has(&["shift"])))
            || (matches!(main, "escape" | "esc") && has(&["alt", "option", "opt"])));
    if reserved {
        return Err(driver_error(
            "DRIVER.KEY_RESERVED",
            "Ctrl+Cmd+Q, Shift+Cmd+Q and Option+Cmd+Esc are never pressed",
        ));
    }
    Ok(())
}

/// The node and everything under it in one snapshot.
fn descendants(record: &Record, node_id: &str) -> BTreeSet<String> {
    let nodes = record.public["nodes"].as_array().expect("nodes");
    let mut result = BTreeSet::from([node_id.to_string()]);
    let mut grew = true;
    while grew {
        grew = false;
        for node in nodes {
            if node["parent_id"]
                .as_str()
                .is_some_and(|parent| result.contains(parent))
            {
                if let Some(id) = node["node_id"].as_str() {
                    grew |= result.insert(id.to_string());
                }
            }
        }
    }
    result
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
    let point_input = |extra: Value, required: &[&str]| {
        let mut properties = json!({"target": target_schema(), "locator": locator_schema()});
        properties
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut all = vec!["target", "locator"];
        all.extend_from_slice(required);
        json!({"type": "object", "required": all, "properties": properties, "additionalProperties": false})
    };
    let fraction = json!({"type": "object", "required": ["fx", "fy"], "properties": {"fx": {"type": "number", "minimum": 0, "maximum": 1}, "fy": {"type": "number", "minimum": 0, "maximum": 1}}, "additionalProperties": false});
    let button = json!({"enum": ["left", "middle", "right"]});
    actions.insert("pointer_gesture".into(), contract("重新验证目标后，在元素 bounds 内的比例位置、经 AX 命中检查后向前台应用派发右键/中键/双击、滚轮或悬停。", "non_idempotent", "modify", "high", &["desktop.observe", "desktop.input"], point_input(json!({"button": button, "count": {"enum": [1, 2]}, "fraction": fraction, "scroll": {"type": "object", "required": ["direction"], "properties": {"direction": {"enum": ["up", "down", "left", "right"]}, "notches": {"type": "integer", "minimum": 1, "maximum": 30}}, "additionalProperties": false}, "hover_ms": {"type": "integer", "minimum": 1, "maximum": 5000}}), &[]), write_output(), true, true));
    actions.insert("drag".into(), contract("分别重新验证起点与终点并做 AX 命中检查后，在同一前台应用内按下、分步拖动并在终点释放。", "non_idempotent", "modify", "high", &["desktop.observe", "desktop.input"], point_input(json!({"to": target_schema(), "to_locator": locator_schema(), "button": button, "fraction": fraction, "to_fraction": fraction}), &["to", "to_locator"]), write_output(), true, true));
    actions.insert("key_chord".into(), contract("重新验证并聚焦非受保护目标后，按下修饰键（ctrl、shift、alt、cmd）加一个主键；名称按字面映射，不在平台间互换。", "non_idempotent", "input", "high", &["desktop.observe", "desktop.input"], point_input(json!({"keys": {"type": "array", "minItems": 1, "maxItems": 6, "items": {"type": "string", "minLength": 1, "maxLength": 32}}}), &["keys"]), write_output(), true, true));
    for (name, description) in [
        ("minimize", "重新验证 AXWindow 后设置 AXMinimized=true，等待（有上限）读回为 true。"),
        ("maximize", "重新验证 AXWindow 后设置 AXFullScreen=true（macOS 没有与 Windows 相同的最大化），等待读回。"),
        ("restore", "最小化时设 AXMinimized=false，全屏时设 AXFullScreen=false，等待读回。"),
        ("close_window", "按下 AXWindow 的 AXCloseButton（AXPress），等待窗口从应用的 AXWindows 中消失；应用先询问时 effect unknown。"),
        ("activate", "激活目标应用并对 AXWindow 执行 AXRaise，等待它成为前台应用的主窗口。"),
    ] {
        actions.insert(
            name.into(),
            contract(
                description,
                if name == "close_window" { "non_idempotent" } else { "idempotent" },
                "modify",
                "high",
                &["desktop.observe", "desktop.input"],
                write_input(),
                write_output(),
                true,
                true,
            ),
        );
    }
    actions.insert("paste".into(), contract("重新验证并聚焦可编辑、非受保护元素后，经通用剪贴板（标记 Concealed/Transient）提供文字并向目标进程发送 Cmd+V，之后恢复原剪贴板内容。", "contextual", "input", "high", &["desktop.observe", "desktop.input"], json!({"type": "object", "required": ["target", "locator", "text"], "properties": {"target": target_schema(), "locator": locator_schema(), "text": {"type": "string", "minLength": 1, "maxLength": MAX_PASTE_CHARS}}, "additionalProperties": false}), write_output(), true, true));
    let mut capture = contract("与写动作相同的重新验证后，只截取目标所在窗口自身的内容（CGWindowListCreateImage + 该窗口 ID，不含遮挡它的其他窗口），裁剪到元素 bounds；需要“屏幕录制”权限，未授予时拒绝。", "read_only", "observe", "medium", &["desktop.observe", "desktop.capture"], json!({"type": "object", "required": ["target", "locator"], "properties": {"target": target_schema(), "locator": locator_schema()}, "additionalProperties": false}), json!({"type": "object", "required": ["format", "data", "width", "height", "clipped", "node_id"], "properties": {"format": {"const": "png"}, "data": {"type": "string"}, "width": {"type": "integer", "minimum": 1}, "height": {"type": "integer", "minimum": 1}, "clipped": {"type": "boolean"}, "node_id": {"type": "string"}}, "additionalProperties": false}), true, true);
    capture["errors"].as_array_mut().unwrap().extend([
        json!({"code": "DRIVER.CAPTURE_FAILED", "description": "窗口截图失败。", "retryable": false, "effect": "not_applied", "data_schema": {"type": "object"}}),
        json!({"code": "DRIVER.SCREEN_RECORDING_DENIED", "description": "MacOSAXHelper.app 没有屏幕录制权限。", "retryable": false, "effect": "not_applied", "data_schema": {"type": "object"}}),
        json!({"code": "DRIVER.WINDOW_MINIMIZED", "description": "窗口已最小化，没有画面。", "retryable": false, "effect": "not_applied", "data_schema": {"type": "object"}}),
    ]);
    capture["sensitivity"] = json!({"input": "public", "output": "sensitive", "error": "public"});
    actions.insert("capture".into(), capture);
    let mut clipboard = contract(
        "读取通用剪贴板的文字（只读、有上限）；带 org.nspasteboard.ConcealedType 的内容拒绝。",
        "read_only",
        "observe",
        "high",
        &["desktop.clipboard_read"],
        json!({"type": "object", "properties": {"max_characters": {"type": "integer", "minimum": 1, "maximum": MAX_CLIPBOARD_CHARS}}, "additionalProperties": false}),
        json!({"type": "object", "required": ["has_text", "text", "characters", "truncated"], "properties": {"has_text": {"type": "boolean"}, "text": {"type": ["string", "null"]}, "characters": {"type": "integer", "minimum": 0}, "truncated": {"type": "boolean"}}, "additionalProperties": false}),
        false,
        false,
    );
    clipboard["sensitivity"] = json!({"input": "public", "output": "sensitive", "error": "public"});
    actions.insert("clipboard_read".into(), clipboard);
    actions.insert("global_key".into(), contract("不锚定任何元素，以 CGEvent 向会话发送组合键；拒绝 Ctrl+Cmd+Q、Shift+Cmd+Q、Option+Cmd+Esc 与 Secure Event Input 期间的输入。", "non_idempotent", "input", "high", &["desktop.input"], json!({"type": "object", "required": ["keys"], "properties": {"keys": {"type": "array", "minItems": 1, "maxItems": 6, "items": {"type": "string", "minLength": 1, "maxLength": 32}}}, "additionalProperties": false}), json!({"type": "object"}), false, true));
    let capture_id = json!({"type": "object", "required": ["capture_id"], "properties": {"capture_id": {"type": "string", "minLength": 1, "maxLength": 64}}, "additionalProperties": false});
    actions.insert("watch".into(), contract("开始记录本人在一个应用里的点击、滚轮、组合键、文本修改和勾选（AXObserver + 只监听的事件 tap），不注入输入。", "idempotent", "observe", "medium", &["desktop.observe"], json!({"type": "object", "required": ["app"], "properties": {"app": app_schema()}, "additionalProperties": false}), json!({"type": "object"}), false, false));
    actions.insert(
        "collect".into(),
        contract(
            "取走上次以来记录到的原始交互事件。",
            "read_only",
            "observe",
            "low",
            &["desktop.observe"],
            capture_id.clone(),
            json!({"type": "object"}),
            false,
            false,
        ),
    );
    actions.insert(
        "release".into(),
        contract(
            "停止记录，移除观察者和事件 tap。",
            "idempotent",
            "observe",
            "low",
            &["desktop.observe"],
            capture_id,
            json!({"type": "object"}),
            false,
            false,
        ),
    );
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
                actions: vec![
                    "focus".into(),
                    "invoke".into(),
                    "pointer_click".into(),
                    "pointer_gesture".into(),
                    "key_chord".into(),
                ],
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
        fn pointer_gesture(
            &self,
            _token: &str,
            gesture: &Value,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"native_operation": "CGEventPointerGesture", "submitted": true, "sent": gesture}),
            )
        }
        fn key_chord(
            &self,
            _token: &str,
            keys: &[String],
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"native_operation": "CGEventKeyChord", "submitted": true, "sent": keys}))
        }
        fn start_recording(
            &self,
            process_id: u32,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            assert_eq!(process_id, 4242);
            Ok(json!({"recording_id": "r1", "pointer_watch": false}))
        }
        fn collect_recording(
            &self,
            id: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            assert_eq!(id, "r1");
            Ok(
                json!({"events": [{"kind": "text", "node": {"role": "AXTextField", "name": "Note", "value": "hi"}}], "dropped": 0}),
            )
        }
        fn stop_recording(&self, _id: &str, _deadline: Instant) -> Result<(), AutomationError> {
            Ok(())
        }
        fn window_action(
            &self,
            token: &str,
            action: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"native_operation": format!("AXWindow.{action}"), "accepted": true, "token": token}),
            )
        }
        fn paste(
            &self,
            _token: &str,
            text: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"native_operation": "NSPasteboard+CGEventCmdV", "submitted": true, "pasted": text}),
            )
        }
        fn capture_element(
            &self,
            token: &str,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            Ok(
                json!({"format": "png", "data": "iVBORw0KGgo=", "width": 240, "height": 60, "clipped": false, "token": token}),
            )
        }
        fn clipboard_text(
            &self,
            max_characters: usize,
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            let text = "pasteboard text";
            Ok(
                json!({"has_text": true, "text": text.chars().take(max_characters).collect::<String>(), "characters": text.chars().count(), "truncated": text.chars().count() > max_characters}),
            )
        }
        fn global_key(
            &self,
            keys: &[String],
            _deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"native_operation": "CGEventGlobalKeyChord", "submitted": true, "sent": keys}),
            )
        }
    }

    /// A one-window app: an AXWindow with an editable field and a secure field.
    struct WindowBackend {
        inner: FakeBackend,
        secure: bool,
    }
    impl WindowBackend {
        fn nodes(&self) -> Vec<BackendNode> {
            let mut window = self.inner.node("window");
            window.role = "AXWindow".into();
            window.name = Some("Editor".into());
            window.description = None;
            window.actions = vec!["minimize".into(), "close_window".into()];
            window.provenance.remove("identifier");
            let mut field = self.inner.node("field");
            field.parent_index = Some(0);
            field.role = if self.secure {
                "AXSecureTextField"
            } else {
                "AXTextField"
            }
            .into();
            field.name = Some("Note".into());
            field.description = None;
            field.provenance.remove("identifier");
            field.states.insert("editable".into(), Some(!self.secure));
            field.states.insert("protected".into(), Some(self.secure));
            field.actions = vec!["focus".into(), "set_value".into(), "paste".into()];
            vec![window, field]
        }
    }
    impl AxBackend for WindowBackend {
        fn name(&self) -> &str {
            "fake_macos_ax"
        }
        fn list_apps(
            &self,
            deadline: Instant,
        ) -> Result<(bool, Vec<Map<String, Value>>), AutomationError> {
            self.inner.list_apps(deadline)
        }
        fn capture(
            &self,
            _app: &Map<String, Value>,
            _max_depth: u32,
            _max_nodes: usize,
            _deadline: Instant,
        ) -> Result<BackendSnapshot, AutomationError> {
            Ok(BackendSnapshot {
                app: FakeBackend::app(),
                nodes: self.nodes(),
                truncated: false,
            })
        }
        fn same_element(&self, a: &str, b: &str, _: Instant) -> Result<bool, AutomationError> {
            Ok(a == b)
        }
        fn focus(&self, token: &str, deadline: Instant) -> Result<Value, AutomationError> {
            self.inner.focus(token, deadline)
        }
        fn invoke(&self, token: &str, deadline: Instant) -> Result<Value, AutomationError> {
            self.inner.invoke(token, deadline)
        }
        fn pointer_click(
            &self,
            token: &str,
            button: &str,
            position: &str,
            deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.inner.pointer_click(token, button, position, deadline)
        }
        fn set_value(
            &self,
            token: &str,
            value: &str,
            deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.inner.set_value(token, value, deadline)
        }
        fn type_text(
            &self,
            token: &str,
            text: &str,
            deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.inner.type_text(token, text, deadline)
        }
        fn window_action(
            &self,
            token: &str,
            action: &str,
            deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.inner.window_action(token, action, deadline)
        }
        fn paste(
            &self,
            token: &str,
            text: &str,
            deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.inner.paste(token, text, deadline)
        }
        fn capture_element(
            &self,
            token: &str,
            deadline: Instant,
        ) -> Result<Value, AutomationError> {
            self.inner.capture_element(token, deadline)
        }
    }

    #[test]
    fn window_actions_paste_and_capture_are_revalidated_and_routed() {
        let backend = Arc::new(WindowBackend {
            inner: FakeBackend::default(),
            secure: false,
        });
        let provider = MacOSAxProvider::new(backend.clone()).unwrap();
        let call = |action: &str, node: &str, locator: Value, extra: Value| {
            let snapshot = take_snapshot(&provider);
            let mut args = json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": node}, "locator": locator});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            provider.invoke(
                &format!("desktop.macos_ax.{action}@1"),
                args,
                Some(Duration::from_secs(2)),
            )
        };
        let window = json!({"role": "AXWindow", "name": "Editor"});
        let field = json!({"role": "AXTextField", "name": "Note"});
        let minimized = call("minimize", "n0", window.clone(), json!({})).unwrap();
        assert_eq!(
            minimized["backend_result"]["native_operation"],
            "AXWindow.minimize"
        );
        assert_eq!(minimized["backend_result"]["token"], "window");
        // Not offered on this window, so refused before anything is sent.
        assert_eq!(
            call("maximize", "n0", window.clone(), json!({}))
                .unwrap_err()
                .code,
            "DRIVER.ACTION_UNSUPPORTED"
        );
        // Window actions only on the window; paste only on the field.
        assert!(call("close_window", "n1", field.clone(), json!({})).is_err());
        assert!(call("paste", "n0", window.clone(), json!({"text": "x"})).is_err());
        let pasted = call("paste", "n1", field.clone(), json!({"text": "Grüße"})).unwrap();
        assert_eq!(pasted["backend_result"]["pasted"], "Grüße");
        assert!(call("paste", "n1", field.clone(), json!({"text": ""})).is_err());
        assert_eq!(backend.inner.writes.load(Ordering::SeqCst), 2);

        // Capture answers the PNG and leaves the observation usable.
        let snapshot = take_snapshot(&provider);
        let target = json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"});
        let shot = provider
            .invoke(
                "desktop.macos_ax.capture@1",
                json!({"target": target, "locator": window}),
                Some(Duration::from_secs(2)),
            )
            .unwrap();
        assert_eq!(shot["width"], 240);
        assert_eq!(shot["node_id"], "n0");
        provider
            .invoke(
                "desktop.macos_ax.minimize@1",
                json!({"target": target, "locator": window}),
                Some(Duration::from_secs(2)),
            )
            .unwrap();

        // A window holding a secure field is never captured.
        let secure = MacOSAxProvider::new(Arc::new(WindowBackend {
            inner: FakeBackend::default(),
            secure: true,
        }))
        .unwrap();
        let snapshot = take_snapshot(&secure);
        let refused = secure
            .invoke(
                "desktop.macos_ax.capture@1",
                json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"}, "locator": {"role": "AXWindow", "name": "Editor"}}),
                Some(Duration::from_secs(2)),
            )
            .unwrap_err();
        assert_eq!(refused.code, "DRIVER.PROTECTED_ELEMENT");
        // Nor pasted into.
        let snapshot = take_snapshot(&secure);
        assert!(secure
            .invoke(
                "desktop.macos_ax.paste@1",
                json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n1"}, "locator": {"role": "AXSecureTextField", "name": "Note"}, "text": "x"}),
                Some(Duration::from_secs(2)),
            )
            .is_err());
    }

    #[test]
    fn pasteboard_reads_and_global_keys_are_bounded() {
        let backend = Arc::new(FakeBackend::default());
        let provider = MacOSAxProvider::new(backend.clone()).unwrap();
        let call = |action: &str, args: Value| {
            provider.invoke(
                &format!("desktop.macos_ax.{action}@1"),
                args,
                Some(Duration::from_secs(2)),
            )
        };
        let read = call("clipboard_read", json!({"max_characters": 10})).unwrap();
        assert_eq!(read["text"], "pasteboard");
        assert_eq!(read["characters"], 15);
        assert_eq!(read["truncated"], true);
        assert!(call("clipboard_read", json!({"max_characters": 0})).is_err());
        assert!(call("clipboard_read", json!({"other": 1})).is_err());
        assert_eq!(
            call("global_key", json!({"keys": ["cmd", "tab"]})).unwrap()["sent"],
            json!(["cmd", "tab"])
        );
        for reserved in [
            json!(["ctrl", "cmd", "q"]),
            json!(["shift", "cmd", "q"]),
            json!(["alt", "cmd", "escape"]),
        ] {
            assert_eq!(
                call("global_key", json!({"keys": reserved}))
                    .unwrap_err()
                    .code,
                "DRIVER.KEY_RESERVED"
            );
        }
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
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
            "pointer_gesture",
            "drag",
            "key_chord",
            "set_value",
            "type_text",
            "paste",
            "capture",
            "minimize",
            "maximize",
            "restore",
            "close_window",
            "activate",
            "clipboard_read",
            "global_key",
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
    fn gestures_and_chords_are_normalised_for_the_helper() {
        let backend = Arc::new(FakeBackend::default());
        let provider = MacOSAxProvider::new(backend.clone()).unwrap();
        let invoke = |action: &str, extra: Value| {
            let snapshot = take_snapshot(&provider);
            let mut args = json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": "n0"}, "locator": locator()});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            provider.invoke(
                &format!("desktop.macos_ax.{action}@1"),
                args,
                Some(Duration::from_secs(2)),
            )
        };
        let sent = |result: Value| result["backend_result"]["sent"].clone();
        assert_eq!(
            sent(invoke("pointer_gesture", json!({"button": "right", "count": 2})).unwrap()),
            json!({"mode": "click", "button": "right", "count": 2, "fx": 0.5, "fy": 0.5})
        );
        assert_eq!(
            sent(
                invoke(
                    "pointer_gesture",
                    json!({"scroll": {"direction": "down"}, "fraction": {"fx": 0.1, "fy": 0.9}})
                )
                .unwrap()
            ),
            json!({"mode": "wheel", "direction": "down", "notches": 3, "fx": 0.1, "fy": 0.9})
        );
        assert_eq!(
            sent(invoke("pointer_gesture", json!({"hover_ms": 250})).unwrap()),
            json!({"mode": "hover", "hover_ms": 250, "fx": 0.5, "fy": 0.5})
        );
        assert_eq!(
            sent(invoke("key_chord", json!({"keys": ["cmd", "a"]})).unwrap()),
            json!(["cmd", "a"])
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 4);
        for (action, extra) in [
            (
                "pointer_gesture",
                json!({"scroll": {"direction": "down"}, "button": "left"}),
            ),
            ("pointer_gesture", json!({"count": 3})),
            (
                "pointer_gesture",
                json!({"fraction": {"fx": -0.1, "fy": 0.5}}),
            ),
            ("key_chord", json!({"keys": ["cmd", "a b"]})),
            (
                "drag",
                json!({"to": {"snapshot_id": "x", "revision": 1, "node_id": "n0"}, "to_locator": locator(), "button": "side"}),
            ),
        ] {
            assert!(
                invoke(action, extra.clone()).is_err(),
                "{action} {extra} must be refused"
            );
        }
        assert_eq!(
            backend.writes.load(Ordering::SeqCst),
            4,
            "nothing refused reached the helper"
        );
    }

    #[test]
    fn watching_reports_whether_pointer_input_is_seen() {
        let provider = MacOSAxProvider::new(Arc::new(FakeBackend::default())).unwrap();
        let call = |action: &str, args: Value| {
            provider.invoke(
                &format!("desktop.macos_ax.{action}@1"),
                args,
                Some(Duration::from_secs(2)),
            )
        };
        let started = call("watch", json!({"app": {"process_id": 4242}})).unwrap();
        assert_eq!(started["capture_id"], "r1");
        assert_eq!(
            started["pointer_watch"], false,
            "no Input Monitoring: say so"
        );
        assert_eq!(
            call("collect", json!({"capture_id": "r1"})).unwrap()["events"][0]["kind"],
            "text"
        );
        assert_eq!(
            call("release", json!({"capture_id": "r1"})).unwrap()["released"],
            true
        );
        assert!(
            call("watch", json!({"app": {"name": "Notes"}})).is_err(),
            "a process id is required"
        );
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
