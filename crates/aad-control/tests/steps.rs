//! Saved steps: application matching, replay through the session, compilation.
use aad_control::steps::{self, App, Check, Input, Step};
use aad_control::*;
use aad_runtime::Provider;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Two windows of one editor and a browser page; a button appears once clicked.
struct Fake {
    acted: Mutex<Vec<Action>>,
}
fn resource(id: &str, name: &str, metadata: Value) -> BackendResource {
    BackendResource {
        resource: Resource {
            resource_id: id.into(),
            provider: "fixture".into(),
            kind: "desktop".into(),
            name: name.into(),
            capabilities: vec!["click".into(), "fill".into(), "drag".into()],
            metadata,
        },
        selector: json!({"id": id}),
    }
}
impl Backend for Fake {
    fn name(&self) -> &str {
        "fixture"
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into(), "fill".into(), "drag".into()]
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(vec![
            resource(
                "fixture:a",
                "notes.txt - Editor",
                json!({"process_name": "Editor.exe"}),
            ),
            resource(
                "fixture:b",
                "todo.txt - Editor",
                json!({"process_name": "Editor.exe"}),
            ),
            resource(
                "fixture:c",
                "Calculator",
                json!({"process_name": "calc.exe"}),
            ),
        ])
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        let clicked = !self.acted.lock().unwrap().is_empty();
        let mut nodes = vec![
            json!({"node_id": "n1", "role": "button", "name": "Save", "actions": ["click", "drag"]}),
            json!({"node_id": "n2", "role": "edit", "name": "Text", "actions": ["fill"]}),
            json!({"node_id": "n3", "role": "list", "name": "Bin", "actions": ["drag"]}),
        ];
        if clicked {
            nodes.push(json!({"node_id": "n4", "role": "text", "name": "Saved", "actions": []}));
        }
        Ok(json!({"truncated": false, "nodes": nodes}))
    }
    fn act(&self, _: &Value, _: &Value, action: &Action, context: &Context) -> Result<Value> {
        context.commit()?;
        self.acted.lock().unwrap().push(action.clone());
        Ok(json!({"ok": true}))
    }
}

fn session() -> (Arc<Session>, Arc<Fake>) {
    let backend = Arc::new(Fake {
        acted: Mutex::new(Vec::new()),
    });
    let session = Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    (Arc::new(session), backend)
}

fn named(role: &str, name: &str) -> Locator {
    Locator {
        role: Some(role.into()),
        name: Some(name.into()),
        ..Default::default()
    }
}

fn app(title: Option<&str>) -> App {
    App {
        process: Some("editor.exe".into()),
        title: title.map(str::to_string),
        ..Default::default()
    }
}

fn act(id: &str, title: Option<&str>, locator: Locator, action: Value) -> Step {
    Step {
        id: id.into(),
        app: app(title),
        locator,
        action: Some(action),
        to: None,
        check: None,
        input: None,
        note: None,
    }
}

fn targets(session: &Session) -> Vec<Value> {
    let response = session.execute(&Request::new(
        Some(session.id().into()),
        Operation::Targets {},
    ));
    response.data.unwrap()["resources"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn an_application_is_named_by_process_and_only_then_by_title() {
    let (session, _) = session();
    let all = targets(&session);
    let calculator = all.iter().find(|r| r["name"] == "Calculator").unwrap();
    let named = steps::describe(calculator, &all);
    assert_eq!(named.process.as_deref(), Some("calc.exe"));
    assert_eq!(named.title, None, "a unique process needs no title");
    let notes = all
        .iter()
        .find(|r| r["name"] == "notes.txt - Editor")
        .unwrap();
    let named = steps::describe(notes, &all);
    assert_eq!(named.title.as_deref(), Some("notes.txt - Editor"));
    assert_eq!(all.iter().filter(|r| steps::matches(&named, r)).count(), 1);
}

#[test]
fn a_step_replays_by_finding_its_window_and_element_again() {
    let (session, backend) = session();
    let ambiguous = act(
        "save",
        None,
        named("button", "Save"),
        json!({"type": "click"}),
    );
    let refused =
        steps::perform(&session, &ambiguous, &Map::new(), Duration::from_secs(5)).unwrap_err();
    assert_eq!(refused.code, "STEP.APP_AMBIGUOUS");
    assert!(backend.acted.lock().unwrap().is_empty());

    let step = act(
        "save",
        Some("notes"),
        named("button", "Save"),
        json!({"type": "click"}),
    );
    let done = steps::perform(&session, &step, &Map::new(), Duration::from_secs(5)).unwrap();
    assert_eq!(done["element"]["name"], "Save");
    assert!(
        done["element"].get("target").is_none(),
        "nothing session-bound leaves"
    );

    let mut drag = act(
        "move",
        Some("todo"),
        named("button", "Save"),
        json!({"type": "drag"}),
    );
    drag.to = Some(named("list", "Bin"));
    steps::perform(&session, &drag, &Map::new(), Duration::from_secs(5)).unwrap();
    let acted = backend.acted.lock().unwrap();
    assert!(matches!(acted.last(), Some(Action::Drag { .. })));
}

#[test]
fn an_input_supplies_text_that_is_never_stored() {
    let (session, backend) = session();
    let mut step = act(
        "secret",
        Some("notes"),
        named("edit", "Text"),
        json!({"type": "fill"}),
    );
    step.input = Some(Input {
        name: "password".into(),
        sensitive: true,
    });
    let missing = steps::perform(&session, &step, &Map::new(), Duration::from_secs(5));
    assert_eq!(missing.unwrap_err().code, "STEP.INVALID");
    let inputs = Map::from_iter([("password".to_string(), json!("hunter2"))]);
    steps::perform(&session, &step, &inputs, Duration::from_secs(5)).unwrap();
    assert!(matches!(
        backend.acted.lock().unwrap().last(),
        Some(Action::Fill { value }) if value == "hunter2"
    ));
    let workflow = steps::workflow("login", &[step]).unwrap();
    assert!(!workflow.to_string().contains("hunter2"));
    assert_eq!(workflow["inputs"]["password"]["sensitive"], true);
    assert_eq!(
        workflow["steps"][0]["with"]["inputs"]["password"],
        "${{ inputs.password }}"
    );
}

#[test]
fn a_check_waits_for_an_element_to_appear() {
    let (session, _) = session();
    let check = Step {
        id: "saved".into(),
        app: app(Some("notes")),
        locator: named("text", "Saved"),
        action: None,
        to: None,
        check: Some(Check {
            present: true,
            within_ms: 300,
        }),
        input: None,
        note: None,
    };
    let early = steps::perform(&session, &check, &Map::new(), Duration::from_secs(5));
    assert_eq!(early.unwrap_err().code, "STEP.CHECK_FAILED");
    let click = act(
        "save",
        Some("notes"),
        named("button", "Save"),
        json!({"type": "click"}),
    );
    steps::perform(&session, &click, &Map::new(), Duration::from_secs(5)).unwrap();
    assert_eq!(
        steps::perform(&session, &check, &Map::new(), Duration::from_secs(5)).unwrap()["present"],
        true
    );
}

#[test]
fn malformed_steps_are_refused_before_any_desktop_call() {
    let base = act(
        "ok",
        Some("notes"),
        named("button", "Save"),
        json!({"type": "click"}),
    );
    let mut cases = Vec::new();
    let mut bad = base.clone();
    bad.id = "two__parts".into();
    cases.push(bad);
    let mut bad = base.clone();
    bad.action = Some(json!({"type": "teleport"}));
    cases.push(bad);
    let mut bad = base.clone();
    bad.action = Some(json!({"type": "drag"}));
    cases.push(bad);
    let mut bad = base.clone();
    bad.app = App::default();
    cases.push(bad);
    let mut bad = base.clone();
    bad.input = Some(Input {
        name: "x".into(),
        sensitive: false,
    });
    cases.push(bad);
    let mut bad = base.clone();
    bad.action = Some(json!({"type": "click", "button": "left", "x": 10}));
    cases.push(bad);
    for step in cases {
        assert_eq!(
            step.validate().unwrap_err().code,
            "STEP.INVALID",
            "{step:?}"
        );
    }
    assert!(steps::workflow("dup", &[base.clone(), base]).is_err());
}

#[test]
fn compiled_steps_run_through_the_engine() {
    let (session, backend) = session();
    let steps = vec![
        act(
            "save",
            Some("notes"),
            named("button", "Save"),
            json!({"type": "click"}),
        ),
        Step {
            id: "saved".into(),
            app: app(Some("notes")),
            locator: named("text", "Saved"),
            action: None,
            to: None,
            check: Some(Check {
                present: true,
                within_ms: 1000,
            }),
            input: None,
            note: None,
        },
    ];
    let document = steps::workflow("save-notes", &steps).unwrap();
    let descriptor = aad_core::compile_descriptor(document, None).unwrap();
    let mut providers = aad_runtime::ProviderRegistry::new();
    let provider = Arc::new(steps::StepProvider::new(session));
    assert!(provider.manifest().actions.contains_key("act"));
    providers.insert(provider);
    let result = aad_runtime::engine::run(
        &descriptor,
        aad_runtime::RunOptions::default()
            .with_providers(providers)
            .with_granted_permissions(["desktop.input", "desktop.observe"]),
    );
    assert_eq!(result.status.as_str(), "succeeded", "{:?}", result.error);
    assert_eq!(backend.acted.lock().unwrap().len(), 1);

    let saved = steps::recording("save-notes", &steps);
    assert_eq!(steps::steps_of(&saved).unwrap(), steps);
    assert!(steps::steps_of(&json!({"kind": "Recording", "steps": []})).is_err());
}

#[test]
fn windows_captures_convert_and_unreplayable_ones_say_why() {
    let app = app(Some("notes"));
    let typed = json!({"action": "set_value", "locator": {"role": "edit", "name": "Text"}, "argument": "hello", "protected": false, "replayable": true, "summary": "edit Text"});
    let step = steps::from_windows_capture("s1", &app, &typed).unwrap();
    assert_eq!(step.action, Some(json!({"type": "fill", "value": "hello"})));
    let secret = json!({"action": "set_value", "locator": {"role": "edit", "name": "Password"}, "argument": null, "protected": true, "replayable": true});
    let step = steps::from_windows_capture("s2", &app, &secret).unwrap();
    assert_eq!(step.input.unwrap().name, "s2_text");
    let lost = json!({"action": "invoke", "locator": null, "replayable": false, "unresolved": "not in the snapshot"});
    assert_eq!(
        steps::from_windows_capture("s3", &app, &lost).unwrap_err(),
        "not in the snapshot"
    );
}
