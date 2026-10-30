//! Saved steps that replay on any provider the session offers.
//!
//! A step names its application the way a person would -- the provider, the
//! process or app name, part of the window title, the start of a page's
//! address -- and then an element by locator and a control action. Replaying
//! a step lists the session's targets, picks the one application that matches,
//! observes it, finds the element and acts, so nothing session-bound is ever
//! stored. The same step runs on Windows UIA, Linux AT-SPI, macOS AX and CDP.
use crate::{error, Action, Expect, Locator, Operation, Request, Session, View, ACTION_NAMES};
use aad_core::AutomationError;
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::Provider;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const PROVIDER: &str = "control.session";
/// The `format` a saved recording of these steps carries.
pub const FORMAT: &str = "control-steps/1";
const API_VERSION: &str = "ai-auto-desktop.dev/v1alpha1";
const MAX_STEPS: usize = 500;
const MAX_CHECK_MS: u64 = 30_000;

type Result<T> = std::result::Result<T, AutomationError>;

/// Which application or page a step works in. Every given field must match,
/// and exactly one target may match.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct App {
    /// The provider, e.g. `desktop.windows_uia` or `browser.cdp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Process, application or bundle name, compared case-insensitively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    /// Text the window or page title contains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What a page's address starts with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Where a step's text comes from when it must not be stored.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: String,
    #[serde(default)]
    pub sensitive: bool,
}

/// Wait until an element is there, or gone.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub present: bool,
    #[serde(default = "default_within")]
    pub within_ms: u64,
}

fn default_within() -> u64 {
    5_000
}

/// One saved step: an action on an element, or a check that it is there.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub app: App,
    pub locator: Locator,
    /// A control action without its session-bound parts: a drag names its
    /// destination in `to` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Locator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Check>,
    /// For fill, type_text and paste: take the text from this workflow input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Input>,
    /// What a person wrote about the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn invalid(message: impl Into<String>) -> AutomationError {
    error("STEP.INVALID", message)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !id.contains("__")
}

impl Step {
    /// Everything that can be checked without a desktop.
    pub fn validate(&self) -> Result<()> {
        if !valid_id(&self.id) {
            return Err(invalid(format!(
                "step id {:?} must be 1..64 letters, digits, _ or -",
                self.id
            )));
        }
        if self.locator.is_empty() {
            return Err(invalid(format!("step {} has an empty locator", self.id)));
        }
        if self.app == App::default() {
            return Err(invalid(format!("step {} names no application", self.id)));
        }
        match (&self.action, &self.check) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(invalid(format!(
                    "step {} needs exactly one of action and check",
                    self.id
                )))
            }
            (None, Some(check)) if check.within_ms == 0 || check.within_ms > MAX_CHECK_MS => {
                return Err(invalid(format!(
                    "step {} waits 1..{MAX_CHECK_MS} ms",
                    self.id
                )))
            }
            _ => {}
        }
        if let Some(action) = &self.action {
            let name = action["type"].as_str().unwrap_or_default();
            if !ACTION_NAMES.contains(&name) {
                return Err(invalid(format!(
                    "step {} has no known action type",
                    self.id
                )));
            }
            if (name == "drag") != self.to.is_some() {
                return Err(invalid(format!(
                    "step {}: a drag, and only a drag, names a destination in `to`",
                    self.id
                )));
            }
            if self.input.is_some() && !matches!(name, "fill" | "type_text" | "paste") {
                return Err(invalid(format!(
                    "step {}: only fill, type_text and paste take an input",
                    self.id
                )));
            }
            self.typed(None, None)?;
        }
        if let Some(input) = &self.input {
            if !valid_id(&input.name) {
                return Err(invalid(format!("step {} names an invalid input", self.id)));
            }
        }
        Ok(())
    }

    /// The action, with the input's value and the drag destination filled in.
    fn typed(&self, value: Option<&str>, to: Option<&Value>) -> Result<Action> {
        let mut action = self.action.clone().unwrap_or(Value::Null);
        if let Some(value) = value {
            let field = if action["type"] == "fill" {
                "value"
            } else {
                "text"
            };
            action[field] = json!(value);
        } else if self.input.is_some() {
            // Placeholder so the shape can be checked before the value exists.
            let field = if action["type"] == "fill" {
                "value"
            } else {
                "text"
            };
            action[field] = json!("");
        }
        if self.to.is_some() {
            action["to"] = to.cloned().unwrap_or_else(|| {
                json!({"session_id": "", "resource_id": "", "observation_id": "", "node_id": ""})
            });
        }
        serde_json::from_value(action)
            .map_err(|e| invalid(format!("step {} has an unusable action: {e}", self.id)))
    }
}

fn lower(value: &Value) -> Option<String> {
    value.as_str().map(str::to_lowercase)
}

/// Whether a target listed by the session is the application a step names.
pub fn matches(app: &App, resource: &Value) -> bool {
    let metadata = &resource["metadata"];
    let process_names = ["process_name", "name", "bundle_id", "application_name"]
        .iter()
        .filter_map(|key| lower(&metadata[*key]))
        .collect::<Vec<_>>();
    app.provider
        .as_ref()
        .is_none_or(|p| resource["provider"] == p.as_str())
        && app
            .process
            .as_ref()
            .is_none_or(|p| process_names.contains(&p.to_lowercase()))
        && app.title.as_ref().is_none_or(|t| {
            lower(&resource["name"]).is_some_and(|name| name.contains(&t.to_lowercase()))
        })
        && app.url.as_ref().is_none_or(|u| {
            metadata["url"]
                .as_str()
                .is_some_and(|url| url.starts_with(u.as_str()))
        })
}

/// How to name `resource` so that, among `all`, it alone matches: its
/// provider and process, then its title or page address only when needed.
/// Titles often carry a document name, so they are left out when they can be.
pub fn describe(resource: &Value, all: &[Value]) -> App {
    let metadata = &resource["metadata"];
    let process = ["process_name", "bundle_id", "name"]
        .iter()
        .find_map(|key| metadata[*key].as_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let mut app = App {
        provider: resource["provider"].as_str().map(str::to_string),
        process: if resource["provider"] == crate::browser::PROVIDER {
            None
        } else {
            process
        },
        ..Default::default()
    };
    let unique = |app: &App| all.iter().filter(|r| matches(app, r)).count() == 1;
    if unique(&app) {
        return app;
    }
    if let Some(url) = metadata["url"].as_str() {
        app.url = Some(url.to_string());
        if unique(&app) {
            return app;
        }
    }
    app.title = resource["name"].as_str().map(str::to_string);
    app
}

/// A locator for a node, from the driver's own when it offers one.
pub fn locator_for(node: &Value) -> Locator {
    if let Ok(locator) = serde_json::from_value::<Locator>(node["locator"].clone()) {
        if !locator.is_empty() {
            return locator;
        }
    }
    Locator {
        role: node["role"].as_str().map(str::to_string),
        name: node["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        ..Default::default()
    }
}

fn request(session: &Session, operation: Operation, deadline: Instant) -> Result<Value> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(error("STEP.TIMEOUT", "the step ran out of time"));
    }
    let mut request = Request::new(Some(session.id().to_string()), operation);
    request.timeout_ms = (remaining.as_millis() as u64).clamp(1, crate::MAX_TIMEOUT_MS);
    let response = session.execute(&request);
    match response.data {
        Some(data) => Ok(data),
        None => {
            let failure = response.error.unwrap_or_default();
            let mut error = AutomationError::new(
                failure["code"].as_str().unwrap_or("CONTROL.FAILED"),
                failure["message"]
                    .as_str()
                    .unwrap_or("the session refused the request"),
            )
            .with_effect(response.effect);
            if let Some(details) = failure["details"].as_object() {
                error = error.with_details(details.clone());
            }
            Err(error)
        }
    }
}

/// The one resource a step's application names.
fn resource(session: &Session, app: &App, deadline: Instant) -> Result<Option<String>> {
    let listed = request(session, Operation::Targets {}, deadline)?;
    let found: Vec<&Value> = listed["resources"]
        .as_array()
        .map(|all| all.iter().filter(|r| matches(app, r)).collect())
        .unwrap_or_default();
    match found.as_slice() {
        [] => Ok(None),
        [one] => Ok(one["resource_id"].as_str().map(str::to_string)),
        several => Err(error(
            "STEP.APP_AMBIGUOUS",
            format!("{} windows match the step's application", several.len()),
        )
        .with_detail(
            "candidates",
            json!(several
                .iter()
                .take(10)
                .map(|r| json!({"name": r["name"], "provider": r["provider"]}))
                .collect::<Vec<_>>()),
        )),
    }
}

fn observe(session: &Session, resource_id: String, deadline: Instant) -> Result<Value> {
    request(
        session,
        Operation::Observe {
            resource_id,
            max_nodes: crate::MAX_NODES,
            view: View::default(),
        },
        deadline,
    )
}

fn find(
    session: &Session,
    observation: &Value,
    locator: &Locator,
    expect: Expect,
    deadline: Instant,
) -> Result<Value> {
    request(
        session,
        Operation::Find {
            observation_id: observation["observation_id"]
                .as_str()
                .unwrap_or_default()
                .into(),
            locator: Box::new(locator.clone()),
            expect,
        },
        deadline,
    )
}

/// Run one step now. `inputs` supplies values for steps that take one.
pub fn perform(
    session: &Session,
    step: &Step,
    inputs: &Map<String, Value>,
    timeout: Duration,
) -> Result<Value> {
    step.validate()?;
    let deadline = Instant::now() + timeout;
    if let Some(check) = &step.check {
        return wait(session, step, check, deadline);
    }
    let value = match &step.input {
        Some(input) => Some(
            inputs
                .get(&input.name)
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(format!("input {} is required", input.name)))?
                .to_string(),
        ),
        None => None,
    };
    let resource_id = resource(session, &step.app, deadline)?.ok_or_else(|| {
        error(
            "STEP.APP_NOT_FOUND",
            "no open window matches the step's application",
        )
    })?;
    let observation = observe(session, resource_id, deadline)?;
    let found = find(session, &observation, &step.locator, Expect::One, deadline)?;
    let to = match &step.to {
        Some(to) => Some(find(session, &observation, to, Expect::One, deadline)?["target"].clone()),
        None => None,
    };
    let action = step.typed(value.as_deref(), to.as_ref())?;
    let target = serde_json::from_value(found["target"].clone())
        .map_err(|e| error("STEP.BAD_TARGET", e.to_string()))?;
    let result = request(session, Operation::Act { target, action }, deadline)?;
    Ok(json!({"step": step.id, "element": public(&found["node"]), "result": result}))
}

fn wait(session: &Session, step: &Step, check: &Check, deadline: Instant) -> Result<Value> {
    let until = Instant::now() + Duration::from_millis(check.within_ms);
    let deadline = deadline.min(until + Duration::from_secs(5));
    loop {
        let seen = match resource(session, &step.app, deadline)? {
            None => false,
            Some(resource_id) => {
                let observation = observe(session, resource_id, deadline)?;
                find(
                    session,
                    &observation,
                    &step.locator,
                    Expect::Optional,
                    deadline,
                )?["found"]
                    == true
            }
        };
        if seen == check.present {
            return Ok(json!({"step": step.id, "present": seen}));
        }
        if Instant::now() >= until {
            return Err(error(
                "STEP.CHECK_FAILED",
                if check.present {
                    "the element did not appear in time"
                } else {
                    "the element was still there"
                },
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn public(node: &Value) -> Value {
    let mut node = node.clone();
    if let Some(object) = node.as_object_mut() {
        object.remove("target");
    }
    node
}

/// A step recorded by the Windows driver's capture, in this format.
/// `app` names the window that was being recorded. Steps that cannot replay
/// come back as `None` with the reason.
pub fn from_windows_capture(
    id: &str,
    app: &App,
    captured: &Value,
) -> std::result::Result<Step, String> {
    if captured["replayable"] != true {
        return Err(captured["unresolved"]
            .as_str()
            .unwrap_or("the element could not be described")
            .to_string());
    }
    let raw = captured["locator"].as_object().cloned().unwrap_or_default();
    let locator: Locator = serde_json::from_value(Value::Object(raw.clone())).unwrap_or(Locator {
        role: raw.get("role").and_then(Value::as_str).map(str::to_string),
        name: raw.get("name").and_then(Value::as_str).map(str::to_string),
        automation_id: raw
            .get("automation_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        class_name: raw
            .get("class_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    });
    let argument = captured["argument"].as_str().unwrap_or_default();
    let protected = captured["protected"] == true;
    let (action, input) = match captured["action"].as_str().unwrap_or_default() {
        "invoke" => (json!({"type": "invoke"}), None),
        "focus" => (json!({"type": "focus"}), None),
        "pointer_click" => (json!({"type": "click"}), None),
        "set_value" if protected => (
            json!({"type": "fill"}),
            Some(Input {
                name: format!("{id}_text"),
                sensitive: true,
            }),
        ),
        "set_value" => (json!({"type": "fill", "value": argument}), None),
        "type_text" if protected => (
            json!({"type": "type_text"}),
            Some(Input {
                name: format!("{id}_text"),
                sensitive: true,
            }),
        ),
        "type_text" => (json!({"type": "type_text", "text": argument}), None),
        other => return Err(format!("the recorder produced an unknown action {other:?}")),
    };
    Ok(Step {
        id: id.to_string(),
        app: app.clone(),
        locator,
        action: Some(action),
        to: None,
        check: None,
        input,
        note: captured["summary"].as_str().map(str::to_string),
    })
}

/// The saved recording document for `steps`.
pub fn recording(name: &str, steps: &[Step]) -> Value {
    json!({
        "apiVersion": API_VERSION,
        "kind": "Recording",
        "format": FORMAT,
        "metadata": {"name": name},
        "steps": steps,
    })
}

/// The steps of a saved recording, refusing other formats.
pub fn steps_of(document: &Value) -> Result<Vec<Step>> {
    if document["format"] != FORMAT {
        return Err(invalid(format!(
            "this recording is not in the {FORMAT} format; record it again"
        )));
    }
    serde_json::from_value(document["steps"].clone())
        .map_err(|e| invalid(format!("the recording's steps are unreadable: {e}")))
}

/// A workflow the engine runs through [`StepProvider`].
pub fn workflow(name: &str, steps: &[Step]) -> Result<Value> {
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(invalid(format!("a workflow holds 1..{MAX_STEPS} steps")));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut inputs = BTreeMap::new();
    let mut compiled = Vec::new();
    for step in steps {
        step.validate()?;
        if !seen.insert(step.id.clone()) {
            return Err(invalid(format!("step id {} is used twice", step.id)));
        }
        let mut with = json!({"step": step});
        if let Some(input) = &step.input {
            inputs.insert(
                input.name.clone(),
                json!({"schema": {"type": "string"}, "required": true, "sensitive": input.sensitive}),
            );
            with["inputs"] = json!({ &input.name: format!("${{{{ inputs.{} }}}}", input.name) });
        }
        let (uses, effect, risk) = if step.check.is_some() {
            (
                "check",
                "read_only",
                json!({"category": "observe", "level": "low"}),
            )
        } else {
            (
                "act",
                "non_idempotent",
                json!({"category": "modify", "level": "high"}),
            )
        };
        compiled.push(json!({
            "id": step.id,
            "type": "action",
            "uses": format!("{PROVIDER}.{uses}@1"),
            "with": with,
            "effect": {"class": effect},
            "risk": risk,
        }));
    }
    let mut document = json!({
        "apiVersion": API_VERSION,
        "kind": "Workflow",
        "metadata": {"name": name, "version": "0.1.0"},
        "requires": {
            "capabilities": [{"name": PROVIDER, "version": ">=0.1.0"}],
            "permissions": ["desktop.input", "desktop.observe"],
        },
        "budgets": {"max_duration": "10m", "max_executed_steps": steps.len()},
        "steps": compiled,
    });
    if !inputs.is_empty() {
        document["inputs"] = json!(inputs);
    }
    Ok(document)
}

/// Runs compiled steps for the workflow engine through one session.
pub struct StepProvider {
    session: Arc<Session>,
    manifest: CapabilityManifest,
}

impl StepProvider {
    pub fn new(session: Arc<Session>) -> Self {
        let mut actions = Map::new();
        let contract = |summary: &str, effect: &str, category: &str, level: &str| {
            json!({
                "contract_major": 1,
                "summary": summary,
                "effect": {"default_class": effect},
                "risk": {"category": category, "level": level},
            })
        };
        actions.insert(
            "act".into(),
            contract(
                "Find the step's element in its application and act on it.",
                "non_idempotent",
                "modify",
                "high",
            ),
        );
        actions.insert(
            "check".into(),
            contract(
                "Wait until the step's element is present, or absent.",
                "read_only",
                "observe",
                "low",
            ),
        );
        let mut document = manifest::document(PROVIDER, actions);
        document["metadata"]["version"] = json!("0.1.0");
        Self {
            session,
            manifest: manifest::parse(&document).expect("the built-in manifest is valid"),
        }
    }
}

impl Provider for StepProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn invoke(&self, action: &str, args: Value, timeout: Option<Duration>) -> Result<Value> {
        let name = action
            .strip_prefix(PROVIDER)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.split('@').next())
            .unwrap_or(action);
        let step: Step = serde_json::from_value(args["step"].clone())
            .map_err(|e| invalid(format!("unreadable step: {e}")))?;
        if (name == "check") != step.check.is_some() || !matches!(name, "act" | "check") {
            return Err(invalid(format!("{action} does not run this step")));
        }
        let inputs = args["inputs"].as_object().cloned().unwrap_or_default();
        perform(
            &self.session,
            &step,
            &inputs,
            timeout.unwrap_or(Duration::from_secs(30)),
        )
    }
}
