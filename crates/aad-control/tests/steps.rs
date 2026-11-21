//! Saved steps: application matching, replay through the session, compilation.
use aad_control::steps::{self, App, Check, Input, Step};
use aad_control::*;
use aad_runtime::Provider;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Two windows of one editor and a browser page; a button appears once clicked.
struct Fake {
    acted: Mutex<Vec<Action>>,
    /// How many more observations show the "Saved" text twice, as an
    /// accessibility tree can while it changes.
    twins: Mutex<usize>,
    observations: Mutex<VecDeque<ObservationReply>>,
}
enum ObservationReply {
    Snapshot,
    Fail(&'static str),
    FailAtDeadline(&'static str),
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
    fn observe(&self, _: &Value, _: usize, timeout: Duration) -> Result<Value> {
        let reply = self.observations.lock().unwrap().pop_front();
        match reply {
            Some(ObservationReply::FailAtDeadline(code)) => {
                std::thread::sleep(timeout);
                return Err(error(code, "fixture observation failed"));
            }
            Some(ObservationReply::Fail(code)) => {
                return Err(error(code, "fixture observation failed"))
            }
            _ => {}
        }
        let clicked = !self.acted.lock().unwrap().is_empty();
        let mut nodes = vec![
            json!({"node_id": "n1", "role": "button", "name": "Save", "actions": ["click", "drag"]}),
            json!({"node_id": "n2", "role": "edit", "name": "Text", "actions": ["fill"]}),
            json!({"node_id": "n3", "role": "list", "name": "Bin", "actions": ["drag"]}),
        ];
        if clicked {
            nodes.push(json!({"node_id": "n4", "role": "text", "name": "Saved", "actions": []}));
            let mut twins = self.twins.lock().unwrap();
            if *twins > 0 {
                *twins -= 1;
                nodes
                    .push(json!({"node_id": "n5", "role": "text", "name": "Saved", "actions": []}));
            }
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
        twins: Mutex::new(0),
        observations: Mutex::default(),
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

#[test]
fn reads_named_fields_in_a_read_only_session() {
    let (_, backend) = session();
    let session = Session::new(vec![backend], Policy::default());
    let request = |op| session.execute(&Request::new(Some(session.id().into()), op));
    request(Operation::Targets { provider: None });
    let read = |fields: Value| {
        request(
            serde_json::from_value(json!({
                "method":"read_fields","resource_id":"fixture:a","fields":fields
            }))
            .unwrap(),
        )
    };
    let label = json!({"as":"label","locator":{"name":"Save"},"property":"name"});
    let absent = json!({"as":"saved","locator":{"name":"Saved"},"property":"present"});
    assert_eq!(
        read(json!([label, absent])).data.unwrap(),
        json!({"values":{"label":"Save","saved":false}})
    );
    for fields in [
        json!([]),
        json!([label, label]),
        json!(vec![label.clone(); 33]),
    ] {
        assert_eq!(
            read(fields).error.unwrap()["code"],
            "CONTROL.INVALID_REQUEST"
        );
    }
    assert_eq!(
        read(json!([{"as":"price","locator":{"name":"Save"},"property":"value"}]))
            .error
            .unwrap()["code"],
        "CONTROL.FIELD_UNAVAILABLE"
    );
}

#[test]
fn read_deadline_preserves_the_last_observed_mismatch() {
    use ObservationReply::*;

    let step: Step = serde_json::from_value(json!({
        "id":"receipt","app":{"provider":"fixture","title":"notes.txt"},
        "read":[{"as":"billed","locator":{"name":"Save"},"property":"name"}],
        "expect":{"billed":"expected"},"within_ms":300
    }))
    .unwrap();
    let run = |replies: Vec<ObservationReply>, timeout| {
        let (session, backend) = session();
        *backend.observations.lock().unwrap() = replies.into();
        let err = steps::perform(&session, &step, &Map::new(), timeout).unwrap_err();
        assert!(backend.observations.lock().unwrap().is_empty());
        err
    };
    let timeout = Duration::from_secs(2);
    let mismatch = run(vec![Snapshot, FailAtDeadline("CONTROL.TIMEOUT")], timeout);
    assert_eq!(mismatch.code, "STEP.CHECK_FAILED");
    assert!(mismatch.message.contains("billed"));
    assert_eq!(
        run(vec![FailAtDeadline("CONTROL.TIMEOUT")], timeout).code,
        "CONTROL.TIMEOUT"
    );
    assert_eq!(
        run(vec![Snapshot, Fail("CONTROL.TIMEOUT")], timeout).code,
        "CONTROL.TIMEOUT"
    );
    assert_eq!(
        run(vec![Snapshot, FailAtDeadline("BROWSER.CDP_ERROR")], timeout).code,
        "BROWSER.CDP_ERROR"
    );
    assert_eq!(
        run(
            vec![Snapshot, FailAtDeadline("CONTROL.TIMEOUT")],
            Duration::from_millis(225)
        )
        .code,
        "CONTROL.TIMEOUT"
    );
}

#[test]
fn read_steps_preserve_their_contract_when_saved() {
    let read: Step = serde_json::from_value(json!({
        "app":{"provider":"fixture"},"id":"receipt",
        "read":[{"as":"amount","locator":{"name":"Total"},"property":"value"}],
        "expect":{"amount":"20.00"}
    }))
    .unwrap();
    let saved = steps::recording("receipt", std::slice::from_ref(&read));
    assert!(saved["steps"][0].get("locator").is_none());
    assert_eq!(steps::steps_of(&saved).unwrap()[0], read);
    let document = steps::workflow("receipt", std::slice::from_ref(&read)).unwrap();
    assert_eq!(document["steps"][0]["effect"]["class"], "read_only");
    let mut duplicate = read.clone();
    duplicate.id = "again".into();
    assert!(steps::workflow("receipt", &[read.clone(), duplicate]).is_err());
    for extra in [
        json!({"action":{"type":"click"}}),
        json!({"expect":{"typo":true}}),
        json!({"within_ms":0}),
        json!({"expect":{"amount":[]}}),
        json!({"within_ms":5,"expect":null}),
    ] {
        let mut raw = serde_json::to_value(&read).unwrap();
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(serde_json::from_value::<Step>(raw)
            .unwrap()
            .validate()
            .is_err());
    }
}

fn act(id: &str, title: Option<&str>, locator: Locator, action: Value) -> Step {
    Step {
        id: id.into(),
        read: None,
        expect: None,
        within_ms: None,
        app: app(title),
        locator,
        action: Some(action),
        to: None,
        check: None,
        input: None,
        note: None,
    }
}

#[test]
fn saved_steps_and_compiled_workflows_refuse_secret_input() {
    let step = act(
        "password",
        None,
        Locator::default(),
        json!({"type":"secret_input","secret_ref":"password"}),
    );
    assert_eq!(step.validate().unwrap_err().code, "STEP.INVALID");
    assert_eq!(
        steps::workflow("login", &[step]).unwrap_err().code,
        "STEP.INVALID"
    );
}

fn targets(session: &Session) -> Vec<Value> {
    let response = session.execute(&Request::new(
        Some(session.id().into()),
        Operation::Targets { provider: None },
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
        workflow["inputs"]["password"]["schema"],
        json!({"type":"string"})
    );
    assert_eq!(
        workflow["steps"][0]["with"]["inputs"]["password"],
        "${{ inputs.password }}"
    );
}

#[test]
fn a_saved_step_rejects_session_bound_path_anchors_before_replay() {
    let (session, backend) = session();
    let mut step = act(
        "path",
        Some("todo"),
        named("button", "Save"),
        json!({"type":"drag", "path":[{
            "target":{"session_id":"old","resource_id":"old","observation_id":"old","node_id":"n1"},
            "fraction":{"fx":0.5,"fy":0.5}
        }]}),
    );
    step.to = Some(named("list", "Bin"));
    assert_eq!(step.validate().unwrap_err().code, "STEP.INVALID");
    assert_eq!(
        steps::perform(&session, &step, &Map::new(), Duration::from_secs(5))
            .unwrap_err()
            .code,
        "STEP.INVALID"
    );
    assert!(backend.acted.lock().unwrap().is_empty());
    step.action = Some(json!({"type":"drag", "path":[]}));
    step.validate().unwrap();
}

#[test]
fn a_check_waits_for_an_element_to_appear() {
    let (session, _) = session();
    let check = Step {
        read: None,
        expect: None,
        within_ms: None,
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
fn a_check_waits_out_a_moment_of_two_matches() {
    let (session, backend) = session();
    let click = act(
        "save",
        Some("notes"),
        named("button", "Save"),
        json!({"type": "click"}),
    );
    steps::perform(&session, &click, &Map::new(), Duration::from_secs(5)).unwrap();
    let check = |present: bool, within_ms: u64| Step {
        read: None,
        expect: None,
        within_ms: None,
        id: "saved".into(),
        app: app(Some("notes")),
        locator: named("text", "Saved"),
        action: None,
        to: None,
        check: Some(Check { present, within_ms }),
        input: None,
        note: None,
    };
    *backend.twins.lock().unwrap() = 2;
    let passed = steps::perform(
        &session,
        &check(true, 3_000),
        &Map::new(),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(passed["present"], true);
    assert_eq!(*backend.twins.lock().unwrap(), 0);
    // Two that stay are reported as two, whether one or none was expected.
    *backend.twins.lock().unwrap() = usize::MAX;
    for present in [true, false] {
        let refused = steps::perform(
            &session,
            &check(present, 600),
            &Map::new(),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(refused.code, "CONTROL.AMBIGUOUS");
    }
}

#[test]
fn a_recorded_file_selection_requires_paths_from_the_run() {
    let (session, backend) = session();
    let mut step = act(
        "rec1",
        None,
        named("button", "Attachments"),
        json!({"type":"upload"}),
    );
    step.input = Some(Input {
        name: "rec1_files".into(),
        sensitive: false,
    });
    let recorded = steps::recording("attach", std::slice::from_ref(&step));
    let restored = steps::steps_of(&recorded).unwrap();
    let document = steps::workflow("attach", &restored).unwrap();
    assert_eq!(
        document["inputs"]["rec1_files"],
        json!({"schema":{"type":"array","items":{"type":"string"}},"sensitive":false,"required":true})
    );
    assert_eq!(
        document["steps"][0]["with"]["inputs"]["rec1_files"],
        "${{ inputs.rec1_files }}"
    );
    assert_eq!(
        document["steps"][0]["with"]["step"]["action"],
        json!({"type":"upload"})
    );
    for inputs in [
        json!({}),
        json!({"rec1_files":null}),
        json!({"rec1_files":"file.txt"}),
        json!({"rec1_files":[]}),
        json!({"rec1_files":[""]}),
        json!({"rec1_files":["ok", 2]}),
    ] {
        let error = steps::perform(
            &session,
            &step,
            inputs.as_object().unwrap(),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.code, "STEP.INVALID");
        assert!(error.message.contains("rec1_files"));
    }
    assert!(backend.acted.lock().unwrap().is_empty());
    let mut text = step.clone();
    text.id = "other".into();
    text.action = Some(json!({"type":"paste"}));
    assert!(steps::workflow("attach", &[step.clone(), text]).is_err());
    step.action = Some(json!({"type":"upload","files":["recorded.txt"]}));
    assert!(step.validate().is_err());
    step.input = None;
    step.validate().expect("legacy literal upload still works");
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
            read: None,
            expect: None,
            within_ms: None,
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
