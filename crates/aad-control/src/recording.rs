//! Watching a person work and writing down what they did as saved steps.
//!
//! Each native provider that can watch offers `watch`, `collect` and
//! `release`. The Windows driver returns its own recorded steps; the AT-SPI
//! and AX providers return raw interaction events in one shared shape, which
//! [`from_events`] turns into steps:
//!
//! ```json
//! {"kind": "click", "button": "left", "node": {"role": "push_button", "name": "Save"}}
//! {"kind": "wheel", "direction": "down", "node": {...}}
//! {"kind": "key", "keys": ["ctrl", "s"], "node": {...}}
//! {"kind": "text", "node": {"role": "text", "name": "Note", "value": "hello", "protected": false}}
//! {"kind": "toggle", "checked": true, "node": {...}}
//! ```
//!
//! `node` describes the element the way that provider's snapshot does, so a
//! locator built from it finds the same element on replay.
use crate::steps::{self, App, Input, Step};
use crate::{error, Locator, Session};
use aad_core::AutomationError;
use aad_runtime::Provider;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const WINDOWS: &str = "desktop.windows_uia";
const LINUX: &str = "desktop.linux_atspi";
const MACOS: &str = "desktop.macos_ax";
/// Two clicks on one element closer than this are one double click.
const DOUBLE_CLICK_MS: u64 = 500;
/// A click this soon before a toggle or an edit of the same element was only
/// the way that change was made, not a step of its own.
const CAUSE_MS: u64 = 1_500;
const TIMEOUT: Duration = Duration::from_secs(10);

type Result<T> = std::result::Result<T, AutomationError>;

/// What one collection produced.
#[derive(Debug, Default)]
pub struct Batch {
    pub steps: Vec<Step>,
    /// Interactions that cannot become a replayable step, with the reason.
    pub skipped: Vec<Value>,
    /// Events the provider had to drop because nobody collected in time.
    pub dropped: u64,
}

impl Batch {
    pub fn to_json(&self) -> Value {
        json!({"steps": self.steps, "skipped": self.skipped, "dropped": self.dropped})
    }
}

fn same(left: &Value, right: &Value) -> bool {
    left["role"] == right["role"] && left["name"] == right["name"]
}

fn locator(node: &Value) -> Locator {
    Locator {
        role: node["role"].as_str().map(str::to_string),
        name: node["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        ..Default::default()
    }
}

fn at(event: &Value) -> u64 {
    event["at_ms"].as_u64().unwrap_or(0)
}

/// Turn raw events, oldest first, into steps for `app`. `next` numbers the
/// step ids and is advanced past every id used.
pub fn from_events(app: &App, events: &[Value], next: &mut usize) -> Batch {
    let mut batch = Batch::default();
    // Each kept event, and how many clicks or notches it stands for.
    let mut kept: Vec<(Value, u64)> = Vec::new();
    let mut last_node: Option<Value> = None;
    for event in events {
        let mut event = event.clone();
        let kind = event["kind"].as_str().unwrap_or_default().to_string();
        if kind == "key" && event["node"].is_null() {
            match &last_node {
                Some(node) => event["node"] = node.clone(),
                None => {
                    batch.skipped.push(
                        json!({"reason": "按键时不知道焦点在哪个元素上", "summary": event["keys"]}),
                    );
                    continue;
                }
            }
        }
        let node = event["node"].clone();
        if node["role"].as_str().is_none_or(str::is_empty) {
            batch
                .skipped
                .push(json!({"reason": "无法识别操作的元素", "summary": kind}));
            continue;
        }
        last_node = Some(node.clone());
        if let Some((previous, count)) = kept.last_mut() {
            let close = at(&event).saturating_sub(at(previous));
            let on_same = same(&previous["node"], &node);
            match (previous["kind"].as_str().unwrap_or_default(), kind.as_str()) {
                // Typing raises an event per keystroke: keep the final text.
                ("text", "text") if on_same => {
                    *previous = event;
                    continue;
                }
                ("click", "click")
                    if on_same
                        && previous["button"] == event["button"]
                        && *count == 1
                        && close <= DOUBLE_CLICK_MS =>
                {
                    *count = 2;
                    continue;
                }
                ("wheel", "wheel") if on_same && previous["direction"] == event["direction"] => {
                    *count = (*count + 1).min(30);
                    continue;
                }
                // The click that toggled a box or focused a field is folded
                // into the change it caused.
                ("click", "toggle" | "text")
                    if on_same
                        && *count == 1
                        && previous["button"] == "left"
                        && close <= CAUSE_MS =>
                {
                    kept.pop();
                }
                _ => {}
            }
        }
        kept.push((event, 1));
    }
    for (event, count) in kept {
        let node = &event["node"];
        let id = format!("rec{next}");
        *next += 1;
        let mut input = None;
        let action = match event["kind"].as_str().unwrap_or_default() {
            "click" => {
                let mut action = json!({"type": "click"});
                if let Some(button) = event["button"].as_str().filter(|b| *b != "left") {
                    action["button"] = json!(button);
                }
                if count == 2 {
                    action["count"] = json!(2);
                }
                action
            }
            "wheel" => json!({"type": "scroll", "direction": event["direction"], "notches": count}),
            "key" => json!({"type": "key", "keys": event["keys"]}),
            "toggle" => json!({"type": "check", "checked": event["checked"] == true}),
            "text" if node["protected"] == true => {
                input = Some(Input {
                    name: format!("{id}_text"),
                    sensitive: true,
                });
                json!({"type": "fill"})
            }
            "text" => json!({"type": "fill", "value": node["value"].as_str().unwrap_or_default()}),
            other => {
                batch
                    .skipped
                    .push(json!({"reason": format!("不认识的事件 {other:?}")}));
                continue;
            }
        };
        let step = Step {
            id,
            app: app.clone(),
            locator: locator(node),
            action: Some(action),
            to: None,
            check: None,
            input,
            note: None,
        };
        match step.validate() {
            Ok(()) => batch.steps.push(step),
            Err(problem) => batch
                .skipped
                .push(json!({"reason": problem.message, "summary": event["kind"]})),
        }
    }
    batch
}

/// A recording in progress on one window or application.
pub struct Recording {
    provider: Arc<dyn Provider>,
    name: String,
    capture: String,
    app: App,
    next: usize,
}

/// Whether the provider behind `resource` can watch a person work.
pub fn supported(session: &Session, resource: &Value) -> bool {
    provider(session, resource).is_some()
}

fn provider(session: &Session, resource: &Value) -> Option<Arc<dyn Provider>> {
    let name = resource["provider"].as_str()?;
    session
        .providers()
        .iter()
        .find(|p| p.manifest().name == name && p.manifest().actions.contains_key("watch"))
        .cloned()
}

impl Recording {
    /// Start watching `resource`, a target as the session listed it.
    pub fn start(session: &Session, resource: &Value, app: App, next: usize) -> Result<Self> {
        let provider = provider(session, resource).ok_or_else(|| {
            error(
                "RECORDING.UNSUPPORTED",
                "this window's provider cannot record what a person does",
            )
        })?;
        let name = provider.manifest().name.clone();
        let metadata = &resource["metadata"];
        let args = match name.as_str() {
            WINDOWS => json!({"window_id": metadata["window_id"]}),
            LINUX => json!({"application": {"bus_name": metadata["bus_name"]}}),
            MACOS => json!({"app": {"process_id": metadata["process_id"]}}),
            other => {
                return Err(error(
                    "RECORDING.UNSUPPORTED",
                    format!("{other} cannot record"),
                ))
            }
        };
        let started = provider.invoke(&format!("{name}.watch@1"), args, Some(TIMEOUT))?;
        let capture = started["capture_id"]
            .as_str()
            .ok_or_else(|| {
                error(
                    "RECORDING.BAD_PROVIDER_OUTPUT",
                    "watch returned no capture id",
                )
            })?
            .to_string();
        Ok(Self {
            provider,
            name,
            capture,
            app,
            next,
        })
    }

    /// The steps recorded since the last collection.
    pub fn collect(&mut self) -> Result<Batch> {
        let raw = self.provider.invoke(
            &format!("{}.collect@1", self.name),
            json!({"capture_id": self.capture}),
            Some(TIMEOUT),
        )?;
        let dropped = raw["dropped"].as_u64().unwrap_or(0);
        let mut batch = if self.name == WINDOWS {
            let mut batch = Batch::default();
            for captured in raw["steps"].as_array().cloned().unwrap_or_default() {
                let id = format!("rec{}", self.next);
                self.next += 1;
                match steps::from_windows_capture(&id, &self.app, &captured) {
                    Ok(step) => batch.steps.push(step),
                    Err(reason) => batch
                        .skipped
                        .push(json!({"reason": reason, "summary": captured["summary"]})),
                }
            }
            batch
        } else {
            let events = raw["events"].as_array().cloned().unwrap_or_default();
            from_events(&self.app, &events, &mut self.next)
        };
        batch.dropped = dropped;
        Ok(batch)
    }

    /// Collect what is left and stop watching.
    pub fn stop(mut self) -> Result<Batch> {
        let last = self.collect();
        let released = self.provider.invoke(
            &format!("{}.release@1", self.name),
            json!({"capture_id": self.capture}),
            Some(TIMEOUT),
        );
        let batch = last?;
        released?;
        Ok(batch)
    }

    /// The id the next recorded step will get.
    pub fn next_id(&self) -> usize {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, name: &str) -> Value {
        json!({"role": role, "name": name})
    }
    fn event(kind: &str, at_ms: u64, node: Value, extra: Value) -> Value {
        let mut event = json!({"kind": kind, "at_ms": at_ms, "node": node});
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        event
    }
    fn actions(batch: &Batch) -> Vec<Value> {
        batch
            .steps
            .iter()
            .map(|s| s.action.clone().unwrap())
            .collect()
    }
    fn app() -> App {
        App {
            process: Some("editor".into()),
            ..Default::default()
        }
    }

    #[test]
    fn typing_becomes_one_fill_and_the_focusing_click_disappears() {
        let field = node("text", "Note");
        let typed = |value: &str, at| {
            let mut field = field.clone();
            field["value"] = json!(value);
            event("text", at, field, json!({}))
        };
        let events = vec![
            event("click", 0, node("text", "Note"), json!({"button": "left"})),
            typed("h", 100),
            typed("hi", 200),
            event(
                "click",
                5_000,
                node("push_button", "Save"),
                json!({"button": "left"}),
            ),
        ];
        let mut next = 1;
        let batch = from_events(&app(), &events, &mut next);
        assert_eq!(
            actions(&batch),
            vec![
                json!({"type": "fill", "value": "hi"}),
                json!({"type": "click"})
            ]
        );
        assert_eq!(batch.steps[0].id, "rec1");
        assert_eq!(next, 3);
    }

    #[test]
    fn clicks_wheels_toggles_and_chords_keep_their_shape() {
        let list = node("list", "Files");
        let box_ = node("check_box", "Wrap");
        let events = vec![
            event("click", 0, list.clone(), json!({"button": "left"})),
            event("click", 200, list.clone(), json!({"button": "left"})),
            event("click", 2_000, list.clone(), json!({"button": "right"})),
            event("wheel", 3_000, list.clone(), json!({"direction": "down"})),
            event("wheel", 3_050, list.clone(), json!({"direction": "down"})),
            event("click", 4_000, box_.clone(), json!({"button": "left"})),
            event("toggle", 4_100, box_, json!({"checked": true})),
            json!({"kind": "key", "at_ms": 5_000, "keys": ["ctrl", "s"], "node": null}),
        ];
        let batch = from_events(&app(), &events, &mut 1);
        assert_eq!(
            actions(&batch),
            vec![
                json!({"type": "click", "count": 2}),
                json!({"type": "click", "button": "right"}),
                json!({"type": "scroll", "direction": "down", "notches": 2}),
                json!({"type": "check", "checked": true}),
                json!({"type": "key", "keys": ["ctrl", "s"]}),
            ]
        );
        assert_eq!(
            batch.steps[4].locator.role.as_deref(),
            Some("check_box"),
            "a key goes to the element last interacted with"
        );
    }

    #[test]
    fn secrets_become_inputs_and_unknown_elements_are_reported() {
        let mut secret = node("password_text", "Password");
        secret["protected"] = json!(true);
        secret["value"] = json!("hunter2");
        let events = vec![
            json!({"kind": "key", "keys": ["enter"], "node": null}),
            event("text", 10, secret, json!({})),
            event(
                "click",
                20,
                json!({"role": "", "name": ""}),
                json!({"button": "left"}),
            ),
        ];
        let batch = from_events(&app(), &events, &mut 1);
        assert_eq!(batch.steps.len(), 1);
        assert_eq!(batch.steps[0].action, Some(json!({"type": "fill"})));
        assert!(batch.steps[0].input.as_ref().unwrap().sensitive);
        assert!(!serde_json::to_string(&batch.steps)
            .unwrap()
            .contains("hunter2"));
        assert_eq!(batch.skipped.len(), 2);
    }
}
