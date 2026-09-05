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
use crate::capture::{self, CapturedEvent};
use crate::model::{Locator, Node, Snapshot, Target, NODE_ACTIONS};
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::journal::now_rfc3339;
use aad_runtime::provider::Provider;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PROVIDER_NAME: &str = "desktop.windows_uia";

/// Actions that change the state of the desktop.
pub const WRITE_ACTIONS: &[&str] =
    &["focus", "invoke", "set_value", "type_text", "pointer_click"];

const MAX_TYPE_TEXT_CHARS: usize = 1024;
const MAX_CANDIDATE_SUMMARIES: usize = 10;
/// How many captured events one `collect` will take at a time.
const MAX_COLLECTED_EVENTS: usize = 256;

/// A UI Automation driver over some backend.
pub struct UiaDriver {
    backend: Arc<dyn Backend>,
    snapshots: SnapshotStore,
    manifest: CapabilityManifest,
    /// Recording sessions in progress.
    ///
    /// Capturing is inherently stateful -- a subscription lives on a background
    /// thread -- while `call` is not, so the sessions live here and the actions
    /// are just the way in.
    captures: Mutex<HashMap<String, CaptureHandle>>,
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
            captures: Mutex::new(HashMap::new()),
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
            "overview" => self.overview(&args),
            "find" => self.find(&args),
            "focus" | "invoke" | "set_value" | "type_text" | "pointer_click" => {
                self.act(action, &args)
            }
            "watch" => self.watch(&args),
            "collect" => self.collect(&args),
            "release" => self.release(&args),
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
        // A live window id is what an interactive caller has. A saved recording
        // has no such thing -- ids are handles from a previous session -- so a
        // descriptive selector is accepted in its place and resolved against the
        // windows that exist right now.
        let window_id = match args.get("window_id").and_then(Value::as_str) {
            Some(window_id) => window_id.to_string(),
            None => match args.get("window") {
                Some(selector) => self.resolve_window(selector)?,
                None => {
                    return Err(DriverError::invalid(
                        "either window_id or window is required",
                    ))
                }
            },
        };
        let window_id = window_id.as_str();

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

    /// Find the one open window matching a descriptive selector.
    ///
    /// Fields are ANDed, and `title` matches by substring because window titles
    /// commonly carry volatile prefixes such as a modified-file marker, while
    /// `class_name` and `process_name` are compared exactly.
    ///
    /// Ambiguity is a failure, not something to resolve by picking the first
    /// match: two windows of the same application are exactly the case where
    /// guessing sends the actions to the wrong one.
    fn resolve_window(&self, selector: &Value) -> Result<String> {
        let object = selector
            .as_object()
            .ok_or_else(|| DriverError::invalid("window must be an object"))?;
        if object.is_empty() {
            return Err(DriverError::invalid(
                "window must constrain at least one of class_name, process_name or title",
            ));
        }

        let field = |key: &str| object.get(key).and_then(Value::as_str).filter(|t| !t.is_empty());
        let (want_class, want_process, want_title) =
            (field("class_name"), field("process_name"), field("title"));

        if want_class.is_none() && want_process.is_none() && want_title.is_none() {
            return Err(DriverError::invalid(
                "window must constrain at least one of class_name, process_name or title",
            ));
        }

        let windows = self.backend.list_windows()?;
        let matches: Vec<&crate::model::WindowInfo> = windows
            .iter()
            .filter(|window| {
                want_class.is_none_or(|want| window.class_name.as_deref() == Some(want))
                    && want_process.is_none_or(|want| {
                        window
                            .process_name
                            .as_deref()
                            .is_some_and(|actual| actual.eq_ignore_ascii_case(want))
                    })
                    && want_title.is_none_or(|want| window.title.contains(want))
            })
            .collect();

        match matches.as_slice() {
            [only] => Ok(only.window_id.clone()),
            [] => Err(DriverError::new(
                "DRIVER.WINDOW_NOT_FOUND",
                "no open window matched the selector",
            )
            // A window that has not opened yet is as transient as an element
            // that has not appeared yet, which is already retryable. A caller
            // waiting for a dialog needs this to mean "not yet" rather than
            // "never", or the wait cannot be expressed at all.
            .retryable()
            .with_detail("selector", selector.clone())
            .with_detail("windows_open", json!(windows.len()))),
            many => Err(DriverError::new(
                "DRIVER.AMBIGUOUS_MATCH",
                format!("the window selector matched {} windows", many.len()),
            )
            .with_detail("match_count", json!(many.len()))
            .with_detail(
                "candidates",
                json!(many
                    .iter()
                    .take(MAX_CANDIDATE_SUMMARIES)
                    .map(|window| json!({
                        "window_id": window.window_id,
                        "title": window.title,
                        "process_name": window.process_name,
                        "class_name": window.class_name,
                    }))
                    .collect::<Vec<_>>()),
            )),
        }
    }

    /// A compact, agent-friendly description of a window.
    fn describe(&self, args: &Value) -> Result<Value> {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(80)
            .clamp(1, 500) as usize;
        let region = args.get("region").and_then(Value::as_str);
        let snapshot = self.capture(args)?;
        let answer = snapshot.outline_of(limit, region);
        // A region name that matches nothing is worth reporting: an agent that
        // drilled into a misremembered name would otherwise read an empty list
        // as "that part of the window is empty", which is a different fact.
        if let Some(wanted) = region {
            if answer["matched"].as_u64() == Some(0) {
                let overview = snapshot.overview();
                return Err(DriverError::new(
                    "DRIVER.NOT_FOUND",
                    format!("no region named {wanted:?} in this window"),
                )
                .retryable()
                .with_detail("regions", overview["regions"].clone()));
            }
        }
        Ok(answer)
    }

    /// The window's regions and their sizes, without listing their contents.
    fn overview(&self, args: &Value) -> Result<Value> {
        Ok(self.capture(args)?.overview())
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

        // `resolve`, not `matches`: an ordinal or a proximity constraint is a
        // property of the element's place among the others, so it can only be
        // applied to the whole set. Filtering node by node would silently
        // ignore both and report every button as ambiguous.
        let matches: Vec<&Node> = locator.resolve(&snapshot.nodes);

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

        let expect = args.get("expect").and_then(Value::as_str);

        let Some(found) = matches.first() else {
            // Absence is a legitimate answer when the caller is asking a
            // question rather than acquiring a target: "has the dialog
            // closed?", "has the spinner gone?". Without this, such an
            // assertion cannot be written at all -- the observation fails, so
            // the condition never gets to run.
            //
            // Not the default: a caller who wants an element in order to act on
            // it is better served by a clear DRIVER.NOT_FOUND here than by an
            // empty result that fails later as a puzzling missing target.
            if expect == Some("optional") {
                return Ok(json!({
                    "found": false,
                    "match_count": 0,
                    "snapshot_id": snapshot.snapshot_id,
                    "searched_nodes": snapshot.nodes.len(),
                }));
            }
            return Err(DriverError::new(
                "DRIVER.NOT_FOUND",
                match locator
                    .near
                    .as_ref()
                    .and_then(|near| near.why_empty(&snapshot.nodes))
                {
                    // A proximity constraint that found nothing is reported as
                    // "no element matched", which sends a caller off to guess a
                    // different locator when the anchor is what needs narrowing.
                    // A page with a billing and a delivery address has two
                    // labels reading "City", so this is ordinary.
                    Some(problem) => {
                        format!("no element matched the locator: {}", problem.describe())
                    }
                    None => "no element matched the locator".to_string(),
                },
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
        let mut answer = json!({
            // Always present, so one condition shape works whether or not the
            // element turned up: a caller polling for a change should not have
            // to write the test two different ways.
            "found": true,
            "target": target.to_json(),
            // The compact form survives shell quoting, so it is what a caller
            // can paste straight into the next command.
            "ref": target.to_ref(),
            "node": found.to_json(),
            "match_count": matches.len(),
        });

        // Several matches, and the caller said to take one anyway. Knowing there
        // were five is not enough to do anything with: what narrows a locator is
        // knowing that the other four were the minimise, maximise and close
        // buttons. The ambiguity error already carries this; a caller who chose
        // to proceed needs it just as much, and has no error to read it from.
        if matches.len() > 1 {
            answer["candidates"] = json!(matches
                .iter()
                .take(MAX_CANDIDATE_SUMMARIES)
                .map(|node| json!({"node_id": node.node_id, "summary": node.summary()}))
                .collect::<Vec<_>>());
        }
        Ok(answer)
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

impl UiaDriver {
    /// Install a capture session for a window.
    ///
    /// The one platform-dependent step. Where capture is not implemented this
    /// says so rather than starting a session that would never produce a step:
    /// a recorder that appears to work and records nothing is worse than one
    /// that refuses.
    #[cfg(windows)]
    fn open_capture(&self, window_id: &str, args: &Value) -> Result<Box<dyn CaptureSource>> {
        let limit = args
            .get("buffer")
            .and_then(Value::as_u64)
            .unwrap_or(512)
            .clamp(16, 4096) as usize;
        let session = crate::windows::CaptureSession::start(window_id, limit)?;
        Ok(Box::new(session))
    }

    #[cfg(not(windows))]
    fn open_capture(&self, _window_id: &str, _args: &Value) -> Result<Box<dyn CaptureSource>> {
        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "recording interactions is only implemented on Windows so far",
        ))
    }
}

/// A recording session and the snapshot its locators are judged against.
struct CaptureHandle {
    session: Box<dyn CaptureSource>,
    /// The window as it was when recording started.
    ///
    /// Locators need the whole node table to be judged unique, and taking a
    /// fresh snapshot per event would be both expensive and racy: by the time it
    /// came back the UI would have moved on, and a dialog that was just
    /// dismissed would no longer be there to describe.
    baseline: Vec<Node>,
    window_id: String,
}

/// Something that can be watched for interactions.
///
/// A trait so the driver's own tests can drive this without a desktop; the real
/// implementation is the native capture session.
pub trait CaptureSource: Send {
    /// Take the events observed so far, and how many were dropped.
    fn drain(&self, max: usize) -> (Vec<CapturedEvent>, u64);
    /// Which capture mechanisms were installed.
    fn sources(&self) -> Vec<String>;
}

impl UiaDriver {
    /// Start recording a window's interactions.
    fn watch(&self, args: &Value) -> Result<Value> {
        // The baseline snapshot doubles as the window lookup: it accepts a live
        // id or a descriptive selector, exactly like every other action.
        let snapshot = self.capture(args)?;
        let window_id = snapshot.window.window_id.clone();
        let session = self.open_capture(&window_id, args)?;
        let sources = session.sources();

        let capture_id = uuid::Uuid::new_v4().simple().to_string();
        let baseline_nodes = snapshot.nodes.len();
        self.captures.lock().unwrap().insert(
            capture_id.clone(),
            CaptureHandle {
                session,
                baseline: snapshot.nodes,
                window_id: window_id.clone(),
            },
        );

        Ok(json!({
            "capture_id": capture_id,
            "window_id": window_id,
            // Which mechanisms are listening. Partial coverage is worth having
            // but the caller has to know: a missing source means a whole class
            // of interaction will go unrecorded rather than merely be delayed.
            "sources": sources,
            "baseline_nodes": baseline_nodes,
            "snapshot_id": snapshot.snapshot_id,
        }))
    }

    /// Take the steps recorded so far, leaving the session running.
    fn collect(&self, args: &Value) -> Result<Value> {
        let capture_id = args
            .get("capture_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DriverError::invalid("capture_id is required"))?;
        let max = args
            .get("max")
            .and_then(Value::as_u64)
            .unwrap_or(MAX_COLLECTED_EVENTS as u64)
            .clamp(1, MAX_COLLECTED_EVENTS as u64) as usize;

        let sessions = self.captures.lock().unwrap();
        let handle = sessions.get(capture_id).ok_or_else(|| {
            DriverError::new(
                "DRIVER.CAPTURE_NOT_FOUND",
                format!("no recording session {capture_id:?}"),
            )
            .with_detail("capture_id", json!(capture_id))
        })?;

        let (events, dropped) = handle.session.drain(max);
        let raw_events = events.len();
        let steps = capture::to_steps(events, &handle.baseline);

        Ok(json!({
            "capture_id": capture_id,
            "window_id": handle.window_id,
            "steps": steps.iter().map(step_to_json).collect::<Vec<_>>(),
            "count": steps.len(),
            // Reported rather than swallowed: a silently discarded event is
            // indistinguishable from the user having done nothing.
            "dropped": dropped,
            "raw_events": raw_events,
        }))
    }

    /// Stop recording and tear down the subscription.
    fn release(&self, args: &Value) -> Result<Value> {
        let capture_id = args
            .get("capture_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DriverError::invalid("capture_id is required"))?;

        // Dropping the handle stops the worker thread and removes the hooks.
        let removed = self.captures.lock().unwrap().remove(capture_id);
        match removed {
            Some(_) => Ok(json!({"released": true, "capture_id": capture_id})),
            None => Err(DriverError::new(
                "DRIVER.CAPTURE_NOT_FOUND",
                format!("no recording session {capture_id:?}"),
            )),
        }
    }
}

/// One recorded step, as JSON.
fn step_to_json(step: &capture::RecordedStep) -> Value {
    json!({
        "action": step.action,
        "locator": step.locator.as_ref().map(Locator::to_json),
        "summary": step.summary,
        "argument": step.argument,
        "protected": step.protected,
        // Both are published: `replayable` is the question a caller asks, and
        // `unresolved` is the reason, which is what a person needs to fix it.
        "replayable": step.is_replayable(),
        "unresolved": step.unresolved,
    })
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
        "overview".into(),
        read_only("Map a window's regions and their sizes, without their contents."),
    );
    actions.insert(
        "find".into(),
        read_only("Find the single element matching a locator."),
    );

    // Watching changes nothing on the desktop, but it is not `read_only`
    // either: it installs hooks and holds a session open until released, which
    // a caller reasoning about effects needs to see.
    actions.insert(
        "watch".into(),
        json!({
            "contract_major": 1,
            "summary": "Start recording a window's interactions.",
            "effect": {"class": "idempotent"},
            "risk": {"category": "observe", "level": "low"},
        }),
    );
    actions.insert(
        "collect".into(),
        read_only("Take the steps recorded so far by a watch session."),
    );
    actions.insert(
        "release".into(),
        json!({
            "contract_major": 1,
            "summary": "Stop a recording session and remove its hooks.",
            "effect": {"class": "idempotent"},
            "risk": {"category": "observe", "level": "low"},
        }),
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

    // -----------------------------------------------------------------------
    // Resolving a window from a saved selector
    //
    // A reopened recording has no live window id: ids are handles from the
    // session that recorded them. So it describes the window instead, and the
    // driver has to find it among the windows open now.
    // -----------------------------------------------------------------------

    /// A window holding two identical buttons, so ambiguity can be exercised.
    struct TwoButtons;

    impl Backend for TwoButtons {
        fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(vec![titled("w1", "Dialog", "app.exe", Some("Dialog"))])
        }

        fn capture(&self, _window_id: &str, _limits: CaptureLimits) -> Result<CapturedTree> {
            Ok(CapturedTree {
                window: titled("w1", "Dialog", "app.exe", Some("Dialog")),
                nodes: vec![
                    node("e1", "Button", "OK", &["invoke"]),
                    node("e2", "Button", "OK", &["invoke"]),
                ],
                root_id: Some("e1".into()),
                truncated: false,
            })
        }

        fn verify(&self, _window_id: &str, _node: &Node) -> Result<bool> {
            Ok(true)
        }

        fn focus(&self, _window_id: &str, _node: &Node) -> Result<()> {
            Ok(())
        }

        fn invoke(&self, _window_id: &str, _node: &Node) -> Result<()> {
            Ok(())
        }

        fn set_value(&self, _window_id: &str, _node: &Node, _value: &str) -> Result<()> {
            Ok(())
        }

        fn type_text(&self, _window_id: &str, _node: &Node, _text: &str) -> Result<()> {
            Ok(())
        }

        fn pointer_click(&self, _window_id: &str, _node: &Node) -> Result<()> {
            Ok(())
        }

        fn describe(&self) -> Value {
            json!({"backend": "two-buttons"})
        }
    }

    /// A backend with several windows, for selector tests.
    struct ManyWindows(Vec<WindowInfo>);

    impl Backend for ManyWindows {
        fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(self.0.clone())
        }

        fn capture(&self, window_id: &str, _limits: CaptureLimits) -> Result<CapturedTree> {
            let window = self
                .0
                .iter()
                .find(|candidate| candidate.window_id == window_id)
                .cloned()
                .ok_or_else(|| DriverError::new("DRIVER.WINDOW_NOT_FOUND", "no such window"))?;
            Ok(CapturedTree {
                window,
                nodes: vec![node("e1", "Button", "Save", &["invoke"])],
                root_id: Some("e1".into()),
                truncated: false,
            })
        }

        fn verify(&self, _window_id: &str, _node: &Node) -> Result<bool> {
            Ok(true)
        }

        fn focus(&self, _window_id: &str, _node: &Node) -> Result<()> {
            Ok(())
        }

        fn invoke(&self, _window_id: &str, _node: &Node) -> Result<()> {
            Ok(())
        }

        fn set_value(&self, _window_id: &str, _node: &Node, _value: &str) -> Result<()> {
            Ok(())
        }

        fn type_text(&self, _window_id: &str, _node: &Node, _text: &str) -> Result<()> {
            Ok(())
        }

        fn pointer_click(&self, _window_id: &str, _node: &Node) -> Result<()> {
            Ok(())
        }

        fn describe(&self) -> Value {
            json!({"backend": "many-windows"})
        }
    }

    fn titled(id: &str, title: &str, process: &str, class: Option<&str>) -> WindowInfo {
        WindowInfo {
            window_id: id.into(),
            title: title.into(),
            process_id: 1,
            process_name: Some(process.into()),
            class_name: class.map(str::to_string),
            bounds: Some(Bounds { x: 0, y: 0, width: 100, height: 100 }),
            is_foreground: false,
            is_minimized: false,
        }
    }

    fn many() -> UiaDriver {
        UiaDriver::new(Arc::new(ManyWindows(vec![
            titled("w1", "notes.txt - Notepad", "notepad.exe", Some("Notepad")),
            titled("w2", "Inbox - Mail", "mail.exe", Some("MailWindow")),
            titled("w3", "budget.xlsx - Excel", "excel.exe", Some("XLMAIN")),
        ])))
    }

    #[test]
    fn a_saved_recording_can_capture_by_describing_its_window() {
        let driver = many();

        let captured = driver
            .call("snapshot", &json!({"window": {"class_name": "XLMAIN"}}))
            .expect("a descriptive selector must work without a window id");

        assert_eq!(captured["window"]["window_id"], "w3");
    }

    #[test]
    fn a_window_title_matches_by_substring() {
        // Titles carry volatile parts -- a modified marker, a changing filename
        // -- so an exact comparison would break on the next edit.
        let driver = many();

        let captured = driver
            .call("snapshot", &json!({"window": {"title": "Notepad"}}))
            .expect("a partial title must resolve");

        assert_eq!(captured["window"]["window_id"], "w1");
    }

    #[test]
    fn several_matching_windows_are_refused_rather_than_guessed() {
        let driver = UiaDriver::new(Arc::new(ManyWindows(vec![
            titled("w1", "a.txt - Notepad", "notepad.exe", Some("Notepad")),
            titled("w2", "b.txt - Notepad", "notepad.exe", Some("Notepad")),
        ])));

        let error = driver
            .call("snapshot", &json!({"window": {"class_name": "Notepad"}}))
            .expect_err("two identical windows must not be silently narrowed to one");

        assert_eq!(error.code, "DRIVER.AMBIGUOUS_MATCH");
        assert_eq!(error.details["match_count"], 2);
        // The caller needs to see the options to narrow the selector.
        assert!(error.details["candidates"].as_array().unwrap().len() == 2);
    }

    #[test]
    fn taking_any_match_still_shows_what_the_others_were() {
        // A caller who proceeds past ambiguity is usually working out what a
        // locator selects. `match_count: 5` cannot be acted on; knowing that
        // three of the five were the window's own title-bar buttons is what
        // shows which constraint to add next. The ambiguity error carries this
        // already -- someone who chose to proceed has no error to read it from.
        let driver = UiaDriver::new(Arc::new(TwoButtons));

        let answer = driver
            .call(
                "find",
                &json!({
                    "window_id": "w1",
                    "locator": {"role": "Button"},
                    "expect": "any",
                }),
            )
            .expect("taking one of several is allowed when asked for");

        assert_eq!(answer["found"], json!(true));
        assert_eq!(answer["match_count"], json!(2));
        let candidates = answer["candidates"]
            .as_array()
            .expect("the competing elements must be listed, not just counted");
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .all(|entry| entry["summary"].as_str().is_some_and(|text| !text.is_empty())),
            "a candidate without a summary tells the caller nothing"
        );
    }

    #[test]
    fn a_single_match_is_not_cluttered_with_a_candidate_list() {
        // One match is unambiguous, so a list of one adds nothing and would
        // suggest a choice was made where none existed.
        let driver = many();

        let answer = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .expect("the fixture has a Save button");

        assert_eq!(answer["match_count"], json!(1));
        assert!(answer.get("candidates").is_none());
    }

    #[test]
    fn tolerating_absence_does_not_tolerate_ambiguity() {
        // Two different questions. "Is it gone?" must never quietly become
        // "here is one of the several that matched" -- that is exactly how
        // automation acts on the wrong element, and an assertion is the last
        // place it should be introduced.
        let driver = UiaDriver::new(Arc::new(TwoButtons));

        let error = driver
            .call(
                "find",
                &json!({
                    "window_id": "w1",
                    "locator": {"role": "Button"},
                    "expect": "optional",
                }),
            )
            .expect_err("several matches must still be refused");

        assert_eq!(error.code, "DRIVER.AMBIGUOUS_MATCH");
        assert_eq!(error.details["match_count"], json!(2));
    }

    #[test]
    fn an_optional_find_reports_absence_instead_of_failing() {
        // Asking "has it gone?" is not the same as asking for it. Without this
        // an absent-assertion cannot be written at all: the observation fails,
        // so the condition never runs, and since a retryable failure means
        // "not yet" the assertion polls its whole timeout and then fails.
        let driver = many();

        let answer = driver
            .call(
                "find",
                &json!({
                    "window_id": "w1",
                    "locator": {"name": "NotOnScreen"},
                    "expect": "optional",
                }),
            )
            .expect("absence is an answer, not an error");

        assert_eq!(answer["found"], json!(false));
        assert_eq!(answer["match_count"], json!(0));
        // No target: there is nothing to act on, and inventing one would be the
        // whole failure mode the reference discipline exists to prevent.
        assert!(answer.get("target").is_none());
    }

    #[test]
    fn an_optional_find_still_reports_what_it_does_find() {
        // The counterpart. An implementation that always answered "absent"
        // would satisfy the test above while making the assertion useless.
        let driver = many();

        let answer = driver
            .call(
                "find",
                &json!({
                    "window_id": "w1",
                    "locator": {"name": "Save"},
                    "expect": "optional",
                }),
            )
            .expect("a present element is still found");

        assert_eq!(answer["found"], json!(true));
        assert_eq!(answer["match_count"], json!(1));
        assert!(answer["ref"].as_str().is_some());
    }

    #[test]
    fn find_still_fails_by_default_when_nothing_matches() {
        // Erroring stays the default. A caller acquiring a target to act on is
        // better served by a clear not-found here than by an empty result that
        // fails later as a puzzling missing target.
        let driver = many();

        let error = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "NotOnScreen"}}))
            .expect_err("acquiring a missing element must fail");

        assert_eq!(error.code, "DRIVER.NOT_FOUND");
    }

    #[test]
    fn a_found_element_always_says_so() {
        // `found` is present on both paths so one condition shape works either
        // way: a caller polling for a change should not have to write the test
        // twice depending on the outcome.
        let driver = many();

        let answer = driver
            .call("find", &json!({"window_id": "w1", "locator": {"name": "Save"}}))
            .expect("the fixture has a Save button");

        assert_eq!(answer["found"], json!(true));
    }

    #[test]
    fn a_selector_that_matches_nothing_says_so() {
        let driver = many();

        let error = driver
            .call("snapshot", &json!({"window": {"class_name": "Gone"}}))
            .expect_err("a closed window must be reported, not invented");

        assert_eq!(error.code, "DRIVER.WINDOW_NOT_FOUND");
        // The window may simply not have opened yet. A caller polling for a
        // dialog needs to tell "not yet" from "never", and this flag is how:
        // without it, waiting for a window to appear cannot be expressed.
        assert!(error.retryable);
    }

    #[test]
    fn selector_fields_must_all_hold() {
        // Fields are ANDed. If they were ORed, naming a process would widen the
        // search instead of narrowing it.
        let driver = many();

        let error = driver
            .call(
                "snapshot",
                &json!({"window": {"class_name": "Notepad", "process_name": "excel.exe"}}),
            )
            .expect_err("a contradictory selector must match nothing");

        assert_eq!(error.code, "DRIVER.WINDOW_NOT_FOUND");
    }

    #[test]
    fn a_process_name_is_compared_without_case_sensitivity() {
        // Windows reports executable names inconsistently cased, and a recording
        // saved from one report must still match the other.
        let driver = many();

        let captured = driver
            .call("snapshot", &json!({"window": {"process_name": "NOTEPAD.EXE"}}))
            .expect("case must not decide whether a recording replays");

        assert_eq!(captured["window"]["window_id"], "w1");
    }

    #[test]
    fn an_empty_selector_is_rejected_instead_of_matching_everything() {
        let driver = many();

        for selector in [json!({}), json!({"class_name": ""})] {
            let error = driver
                .call("snapshot", &json!({"window": selector}))
                .expect_err("an unconstrained selector must not pick an arbitrary window");
            assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
        }
    }

    #[test]
    fn capture_still_requires_one_of_the_two_ways_to_name_a_window() {
        let driver = many();

        let error = driver
            .call("snapshot", &json!({}))
            .expect_err("naming no window at all must fail");

        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
    }

    #[test]
    fn a_window_id_still_takes_precedence_when_both_are_given() {
        // Interactive callers pass an id; it is the more specific of the two.
        let driver = many();

        let captured = driver
            .call(
                "snapshot",
                &json!({"window_id": "w2", "window": {"class_name": "Notepad"}}),
            )
            .expect("an explicit id must win");

        assert_eq!(captured["window"]["window_id"], "w2");
    }

    #[test]
    fn an_outline_carries_a_locator_for_every_element_it_can_identify() {
        // This is what the GUI saves. Without it a recording could only hold a
        // reference, which does not survive being written to a file.
        let driver = many();

        let outline = driver
            .call("describe", &json!({"window_id": "w1"}))
            .expect("describe must work");

        let element = &outline["elements"][0];
        assert!(element["ref"].is_string(), "a live reference is still offered");
        assert!(
            element["locator"].is_object(),
            "a durable locator must accompany it: {}",
            element
        );
        assert_eq!(element["locator"]["role"], "Button");
    }
}
