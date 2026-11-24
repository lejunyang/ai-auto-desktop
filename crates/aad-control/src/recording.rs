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
use std::collections::BTreeMap;
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
    /// Raw browser recording row kept for each normalized replay step.
    pub source_rows: BTreeMap<String, u64>,
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
    !left["capture_identity"].is_null()
        && left["capture_identity"] == right["capture_identity"]
        && left["protected"] == right["protected"]
        && left["role"] == right["role"]
        && left["name"] == right["name"]
}

fn locator(node: &Value) -> Locator {
    Locator {
        role: node["role"].as_str().map(str::to_string),
        name: node["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        within: node.get("captured_container").cloned(),
        states: node
            .get("file_hidden")
            .map(|hidden| json!({"hidden":hidden})),
        ..Default::default()
    }
}

fn at(event: &Value) -> u64 {
    event["at_ms"].as_u64().unwrap_or(0)
}

/// Turn raw events, oldest first, into steps for `app`. `next` numbers the
/// step ids and is advanced past every id used.
pub fn from_events(app: &App, events: &[Value], next: &mut usize) -> Batch {
    collect_events(app, events, next, false).0
}

fn collect_events(
    app: &App,
    events: &[Value],
    next: &mut usize,
    defer_final_edit: bool,
) -> (Batch, Option<Value>) {
    let mut batch = Batch::default();
    // Each kept event, and how many clicks or notches it stands for.
    let mut kept: Vec<(Value, u64)> = Vec::new();
    let mut last_node: Option<Value> = None;
    for event in events {
        let mut event = event.clone();
        let kind = event["kind"].as_str().unwrap_or_default().to_string();
        if kind == "focus_boundary" {
            last_node = event["node"].is_object().then(|| event["node"].clone());
            kept.push((event, 0));
            continue;
        }
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
            last_node = None;
            kept.push((json!({"kind": "focus_boundary"}), 0));
            continue;
        }
        last_node = Some(node.clone());
        if let Some((previous, count)) = kept.last_mut() {
            let close = at(&event).saturating_sub(at(previous));
            let on_same = same(&previous["node"], &node);
            match (previous["kind"].as_str().unwrap_or_default(), kind.as_str()) {
                // Typing raises an event per keystroke: keep the final text.
                ("text", "text") if on_same && close <= CAUSE_MS => {
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
    // Normalize clicks and edits before deciding what to hold for the next
    // poll. A double-click preceding an edit is a separate recorded action.
    let tail = if defer_final_edit
        && kept.last().is_some_and(|(event, _)| {
            event["kind"] == "text" && !event["node"]["capture_identity"].is_null()
        }) {
        kept.pop().map(|(event, _)| event)
    } else {
        None
    };
    for (event, count) in kept {
        if event["kind"] == "focus_boundary" {
            continue;
        }
        let node = &event["node"];
        let id = format!("rec{next}");
        *next += 1;
        let mut input = None;
        let action = match event["kind"].as_str().unwrap_or_default() {
            "canvas_click" if app.provider.as_deref() == Some(BROWSER) => {
                let Some(action) = canvas_action(&event["observed"]) else {
                    batch.skipped.push(json!({"reason":"canvas_object_unresolved","source_row":event["source_row"]}));
                    continue;
                };
                action
            }
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
            "save_as" if app.provider.as_deref() == Some(BROWSER) => {
                input = Some(Input {
                    name: format!("{id}_destination"),
                    sensitive: false,
                });
                let mut action = json!({"type":"save_as","trigger":event["trigger"]});
                if event["source"] == "download" {
                    action["source"] = json!("download");
                }
                action
            }
            "upload" if app.provider.as_deref() == Some(BROWSER) => {
                input = Some(Input {
                    name: format!("{id}_files"),
                    sensitive: false,
                });
                json!({"type":"upload"})
            }
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
            read: None,
            expect: None,
            within_ms: None,
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
            Ok(()) => {
                if let Some(row) = event["source_row"].as_u64() {
                    batch.source_rows.insert(step.id.clone(), row);
                }
                batch.steps.push(step);
            }
            Err(problem) => batch
                .skipped
                .push(json!({"reason": problem.message, "summary": event["kind"]})),
        }
    }
    (batch, tail)
}

const BROWSER: &str = "browser.cdp";

fn canvas_action(action: &Value) -> Option<Value> {
    if action["kind"] != "canvas_click"
        || action["trusted"] != true
        || action["tag"] != "canvas"
        || action["dom_event"] != "click"
    {
        return None;
    }
    let evidence = &action["canvas"];
    let fields = evidence["query"].as_object()?;
    if fields.is_empty()
        || fields.values().any(|v| {
            v.as_str()
                .is_none_or(|s| s.trim().is_empty() || s.len() > 1024 || s.contains("${{"))
        })
    {
        return None;
    }
    match evidence["source"].as_str()? {
        "editor"
            if fields.len() == 1
                && fields.get("id").and_then(Value::as_str).is_some_and(|id| {
                    id.len() <= 64
                        && id.bytes().enumerate().all(|(index, byte)| {
                            byte.is_ascii_alphanumeric()
                                || byte == b'_'
                                || index != 0 && b".:-".contains(&byte)
                        })
                }) =>
        {
            ()
        }
        "flowchart"
            if fields
                .keys()
                .all(|k| matches!(k.as_str(), "label" | "kind" | "after" | "before"))
                && (fields.contains_key("label") || fields.contains_key("kind")) =>
        {
            ()
        }
        _ => return None,
    }
    Some(json!({"type":"click_object","query":fields,"options":{"source":evidence["source"]}}))
}

/// The page a recorded browser action happened on, as a step names it: the
/// address up to its path, so a query or fragment that changes per visit
/// does not keep the step from finding its page.
fn page_of(url: &str) -> Option<String> {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    let page = &url[..cut];
    (page.starts_with("http://") || page.starts_with("https://")).then(|| page.to_string())
}

/// Steps for a browser recording's `steps.jsonl` (aad-browser's recorder), oldest
/// first. Each action names its element by `control`, the role and name the
/// control session finds it by; actions recorded without one (older
/// recordings) are skipped. A form submission is skipped as well: the click
/// or Enter that caused it is already a step.
pub fn from_browser_steps(steps_jsonl: &str, next: &mut usize) -> Batch {
    let mut batch = Batch::default();
    // Consecutive actions on one page replay against that page.
    let mut runs: Vec<(App, Vec<Value>)> = Vec::new();
    for line in steps_jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(step) = serde_json::from_str::<Value>(line) else {
            batch.skipped.push(json!({"reason": "无法读取的记录行"}));
            continue;
        };
        if step["kind"] == "file_chooser_pending" {
            batch
                .skipped
                .push(json!({"reason":"file_chooser_outcome_unobserved"}));
        }
        if step["kind"] == "file_chooser_without_dom_input" {
            batch
                .skipped
                .push(json!({"reason":"file_chooser_without_dom_input"}));
        }
        if matches!(
            step["kind"].as_str(),
            Some("save_picker_pending" | "save_picker_finished")
        ) && step["action"]["trigger_id"].is_null()
        {
            batch
                .skipped
                .push(json!({"reason":"save_picker_trigger_unobserved"}));
        }
        if step["kind"] != "action" {
            continue;
        }
        let action = &step["action"];
        let kind = action["kind"].as_str().unwrap_or_default();
        if matches!(kind, "file_activation" | "file_trigger") {
            runs.push((App::default(), Vec::new()));
            continue;
        }
        let file_reason = match kind {
            "save_pending" => Some("save_picker_outcome_unobserved"),
            "save_cancel" => Some("save_picker_cancelled"),
            "save_failed" => Some("save_picker_selection_unconfirmed"),
            "download_canceled" => Some("download_canceled_or_interrupted"),
            "save_as"
                if action["trusted"] != true
                    || !matches!(action["trigger"].as_str(), Some("click" | "enter")) =>
            {
                Some("save_picker_trigger_unobserved")
            }
            "file_cancel"
                if !(action["trusted"] == true
                    && action["dom_event"] == "cancel"
                    && action["type"] == "file"
                    && action["tag"] == "input") =>
            {
                Some("unverified_file_selection")
            }
            "file_cancel" => Some("file_selection_cancelled_or_unchanged"),
            "file_empty" => Some("file_selection_cleared"),
            "file_directory" => Some("directory_upload_unsupported"),
            "file_trigger_uncertain" => Some("custom_file_chooser_trigger_unresolved"),
            "file_target_unobserved" => Some("file_input_locator_unobserved"),
            "upload"
                if !(action["tag"] == "input"
                    && action["type"] == "file"
                    && action["trusted"] == true
                    && action["dom_event"] == "input"
                    && action["hidden"].is_boolean()) =>
            {
                Some("unverified_file_selection")
            }
            "upload" => None,
            _ if action["type"] == "file" => Some("file_selection_outcome_unknown"),
            _ => None,
        };
        if let Some(reason) = file_reason {
            let mut skipped = json!({"reason":reason});
            if matches!(
                reason,
                "file_selection_cancelled_or_unchanged" | "save_picker_cancelled"
            ) {
                skipped["noop"] = json!(true);
            }
            batch.skipped.push(skipped);
            runs.push((App::default(), Vec::new()));
            continue;
        }
        if kind == "submit" {
            continue;
        }
        let control = &action["control"];
        if control["role"].as_str().is_none_or(str::is_empty) {
            batch.skipped.push(json!({
                "reason": "记录里没有控制会话的定位（旧版录制）",
                "summary": format!("{kind} {}", action["label"].as_str().unwrap_or_default()),
            }));
            continue;
        }
        let Some(page) = action["url"].as_str().and_then(page_of) else {
            batch
                .skipped
                .push(json!({"reason": "不在网页上的操作", "summary": kind}));
            continue;
        };
        let mut node = json!({"role": control["role"], "name": control["name"],"protected":action["protected"] == true});
        if let (Some(role), Some(name)) = (
            control["within"]["role"].as_str(),
            control["within"]["name"].as_str(),
        ) {
            if !role.is_empty() && !name.is_empty() {
                node["captured_container"] = json!({"role":role,"name":name});
            }
        }
        if let (Some(doc), Some(id)) = (action["document_id"].as_str(), action["node_id"].as_str())
        {
            node["capture_identity"] = json!([doc, id]);
        }
        if kind == "input" && action["truncated"] == true {
            batch
                .skipped
                .push(json!({"reason":"输入内容未完整录下，请手工补齐","summary":"input"}));
            runs.push((App::default(), Vec::new()));
            continue;
        }
        let at_ms = action["at_ms"].clone();
        let mut event = match kind {
            "canvas_click" | "click" if kind == "canvas_click" || action["tag"] == "canvas" => {
                if node.to_string().contains("${{") || page.contains("${{") {
                    batch
                        .skipped
                        .push(json!({"reason":"canvas_object_unresolved","source_row":step["id"]}));
                    runs.push((App::default(), Vec::new()));
                    continue;
                }
                json!({"kind":"canvas_click","observed":action,"node":node,"at_ms":at_ms})
            }
            "save_as" => {
                json!({"kind":"save_as","trigger":action["trigger"],"source":action["source"],"node":node,"at_ms":at_ms})
            }
            "upload" => {
                node["file_hidden"] = action["hidden"].clone();
                json!({"kind":"upload","node":node,"at_ms":at_ms})
            }
            "click" => json!({"kind": "click", "button": "left", "node": node, "at_ms": at_ms}),
            "enter" => json!({"kind": "key", "keys": ["enter"], "node": node, "at_ms": at_ms}),
            "input" if matches!(action["type"].as_str(), Some("checkbox" | "radio")) => json!({
                "kind": "toggle", "checked": action["checked"] == true, "node": node, "at_ms": at_ms,
            }),
            "input" => {
                node["value"] = action["value"].clone();
                node["protected"] = json!(action["protected"] == true);
                json!({"kind": "text", "node": node, "at_ms": at_ms})
            }
            other => {
                batch
                    .skipped
                    .push(json!({"reason": format!("不认识的浏览器操作 {other:?}")}));
                continue;
            }
        };
        event["source_row"] = step["id"].clone();
        let app = App {
            provider: Some(BROWSER.into()),
            url: Some(page),
            ..Default::default()
        };
        match runs.last_mut() {
            Some((last, events)) if *last == app => events.push(event),
            _ => runs.push((app, vec![event])),
        }
    }
    for (app, events) in runs {
        let part = from_events(&app, &events, next);
        batch.steps.extend(part.steps);
        batch.source_rows.extend(part.source_rows);
        batch.skipped.extend(part.skipped);
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
    tail: Option<Value>,
}

fn across_polls(
    tail: &mut Option<Value>,
    mut events: Vec<Value>,
    gap: bool,
    finish: bool,
    app: &App,
    next: &mut usize,
) -> Batch {
    let mut batch = Batch::default();
    if let Some(previous) = tail.take() {
        if gap {
            batch = from_events(app, &[previous], next);
        } else {
            events.insert(0, previous);
        }
    }
    let (rest, pending) = collect_events(app, &events, next, !finish);
    *tail = pending;
    batch.steps.extend(rest.steps);
    batch.skipped.extend(rest.skipped);
    batch
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
            tail: None,
        })
    }

    /// The steps recorded since the last collection.
    pub fn collect(&mut self) -> Result<Batch> {
        self.collect_inner(false)
    }

    fn collect_inner(&mut self, finish: bool) -> Result<Batch> {
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
            let idle = events.is_empty();
            across_polls(
                &mut self.tail,
                events,
                dropped != 0,
                finish || idle,
                &self.app,
                &mut self.next,
            )
        };
        batch.dropped = dropped;
        Ok(batch)
    }

    /// Collect what is left and stop watching.
    pub fn stop(mut self) -> Result<Batch> {
        let last = self.collect_inner(true);
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

    #[test]
    fn normalized_browser_steps_keep_their_actual_evidence_rows() {
        let row = |id, kind, value| {
            json!({"id":id,"kind":"action","action":{
                "kind":kind,"value":value,"control":{"role":"textbox","name":"Order"},
                "url":"https://example.test/new","document_id":"main","node_id":"5","at_ms":id
            }})
            .to_string()
        };
        let rows = [
            row(4, "click", ""),
            row(7, "input", "a"),
            row(9, "input", "ab"),
            row(18, "enter", ""),
        ]
        .join("\n");
        let batch = from_browser_steps(&rows, &mut 1);
        assert_eq!(batch.steps.len(), 2);
        assert_eq!(batch.source_rows["rec1"], 9);
        assert_eq!(batch.source_rows["rec2"], 18);
    }

    #[test]
    fn file_selection_never_imports_a_filename_or_an_unverified_outcome() {
        let mut action = json!({"kind":"upload","url":"https://example.test/form?transient=1","control":{"role":"button","name":"Attachments","within":{"role":"form","name":"Invoice"}},"tag":"input","type":"file","hidden":true,"trusted":true,"dom_event":"input","value":"C:\\fakepath\\private.txt","files":["private.txt"]});
        let import = |action: &Value| {
            from_browser_steps(
                &json!({"kind":"action","action":action}).to_string(),
                &mut 3,
            )
        };
        let valid = import(&action);
        assert!(valid.skipped.is_empty());
        let step = serde_json::to_value(&valid.steps[0]).unwrap();
        assert_eq!(step["action"], json!({"type":"upload"}));
        assert_eq!(
            step["input"],
            json!({"name":"rec3_files","sensitive":false})
        );
        assert_eq!(
            step["locator"],
            json!({"name":"Attachments","role":"button","within":{"role":"form","name":"Invoice"},"states":{"hidden":true}})
        );
        assert!(!step.to_string().contains("private.txt"));
        action["trusted"] = json!(false);
        assert_eq!(
            import(&action).skipped[0]["reason"],
            "unverified_file_selection"
        );
        for (kind, reason) in [
            ("file_directory", "directory_upload_unsupported"),
            ("input", "file_selection_outcome_unknown"),
            ("file_empty", "file_selection_cleared"),
            ("file_cancel", "file_selection_cancelled_or_unchanged"),
        ] {
            action["trusted"] = json!(true);
            action["kind"] = json!(kind);
            if kind == "file_cancel" {
                action["dom_event"] = json!("cancel");
            }
            let batch = import(&action);
            assert!(batch.steps.is_empty());
            assert_eq!(batch.skipped[0]["reason"], reason);
            assert_eq!(batch.skipped[0]["noop"] == true, kind == "file_cancel");
        }
    }

    fn node(role: &str, name: &str) -> Value {
        json!({"role": role, "name": name,"capture_identity":[role,name]})
    }

    #[test]
    fn input_spans_polls_but_never_merges_distinct_rows_or_crosses_a_gap() {
        let edit = |id, value| json!({"kind":"text","at_ms":20,"node":{"role":"text","name":"Qty","capture_identity":id,"value":value}});
        let mut tail = None;
        let mut next = 1;
        let mut poll =
            |events, gap, finish| across_polls(&mut tail, events, gap, finish, &app(), &mut next);
        assert!(poll(vec![edit(1, "1")], false, false).steps.is_empty());
        let first = poll(vec![edit(1, "12"), edit(2, "3")], false, false);
        assert_eq!(actions(&first), vec![json!({"type":"fill","value":"12"})]);
        let last = poll(vec![edit(2, "34")], true, true);
        assert_eq!(
            actions(&last),
            vec![
                json!({"type":"fill","value":"3"}),
                json!({"type":"fill","value":"34"})
            ]
        );
        assert!(!last.to_json().to_string().contains("capture_identity"));
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
    fn deferring_an_edit_preserves_double_clicks_and_unknown_target_boundaries() {
        let field = node("text", "Note");
        let click = |ms| event("click", ms, field.clone(), json!({"button":"left"}));
        let edit = |ms, value: &str| {
            let mut node = field.clone();
            node["value"] = json!(value);
            event("text", ms, node, json!({}))
        };
        let mut next = 1;
        let mut tail = None;
        let first = across_polls(
            &mut tail,
            vec![click(0), click(50), edit(75, "a")],
            false,
            false,
            &app(),
            &mut next,
        );
        assert_eq!(actions(&first), vec![json!({"type":"click","count":2})]);
        let next_poll = vec![
            event("click", 80, Value::Null, json!({})),
            event("key", 85, Value::Null, json!({"keys":["enter"]})),
            edit(95, "b"),
        ];
        let last = across_polls(&mut tail, next_poll, false, true, &app(), &mut next);
        assert_eq!(
            actions(&last),
            vec![
                json!({"type":"fill","value":"a"}),
                json!({"type":"fill","value":"b"})
            ]
        );
        assert_eq!(last.skipped.len(), 2);
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
