use super::*;
use aad_control::Policy;
use aad_plugin::{manifest, CapabilityManifest};
use std::collections::VecDeque;
use std::sync::Mutex;

struct Fixture {
    manifest: CapabilityManifest,
    replies: Mutex<VecDeque<(&'static str, Value)>>,
}

impl Provider for Fixture {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn invoke(
        &self,
        name: &str,
        args: Value,
        _: Option<Duration>,
    ) -> std::result::Result<Value, aad_core::AutomationError> {
        let (action, reply) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider call");
        assert_eq!(name, format!("{}.{action}@1", self.manifest.name));
        if action == "snapshot" {
            assert_eq!(args["app"], json!({"bundle_id":"org.editor"}));
        }
        if action == "collect" && self.manifest.name == WINDOWS {
            assert_eq!(args["individual_edits"], true);
        }
        Ok(reply)
    }
}

fn fixture(provider: &str, replies: Vec<(&'static str, Value)>) -> (Arc<Fixture>, Session) {
    let mut actions = serde_json::Map::new();
    for action in [
        "snapshot",
        "find",
        "pointer_click",
        "key_chord",
        "set_value",
    ] {
        let class = if matches!(action, "snapshot" | "find") {
            "read_only"
        } else {
            "non_idempotent"
        };
        actions.insert(
            action.into(),
            json!({"contract_major":1,
            "effect":{"default_class":class},"risk":{"category":"modify","level":"high"}}),
        );
    }
    let manifest = manifest::parse(&manifest::document(provider, actions)).unwrap();
    let fixture = Arc::new(Fixture {
        manifest,
        replies: Mutex::new(replies.into()),
    });
    let session = Session::new(vec![], Policy::default()).with_providers(vec![fixture.clone()]);
    (fixture, session)
}

fn window_meta() -> Value {
    json!({"process_id":25,"process_name":"editor.exe","window_id":"hwnd:10","title":"Draft"})
}

fn windows_start() -> Value {
    json!({"capture_id":"c", "window":window_meta(), "root_locator":{"role":"window","name":"Draft"},
        "sources":["uia","win_event"],"truncated":false,"started_epoch_ms":123400,"precision_ms":2})
}

#[test]
fn uia_keeps_timestamped_actions_and_gaps_and_replays_with_an_exact_window() {
    let meta = window_meta();
    let list = json!({"windows":[meta]});
    let replayable = json!({"epoch_ms":123456,"action":"invoke","replayable":true,
        "locator":{"role":"button","name":"OK","within":{"role":"pane","name":"Form"}}});
    let (f, s) = fixture(
        WINDOWS,
        vec![
            ("list_windows", list.clone()),
            ("watch", windows_start()),
            ("list_windows", list),
            (
                "collect",
                json!({"window_id":"hwnd:10","capture_id":"c",
            "raw_events":2,"dropped":1,"first_epoch_ms":123450,"steps":[replayable,
              {"epoch_ms":123455,"replayable":false,"unresolved":"ambiguous control"}]}),
            ),
            ("release", json!({"released":true})),
        ],
    );
    let mut w = Watch::start(
        &s,
        &json!({"provider":WINDOWS,"metadata":meta}),
        "Draft",
        &[],
    )
    .unwrap();
    assert_eq!(w.precision_ms(), 2);
    let rows = w.collect().unwrap();
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        [123450, 123455, 123456]
    );
    assert!(matches!(&rows[0].1, Event::Gap(s) if s.contains("dropped")));
    assert!(matches!(&rows[1].1, Event::Gap(s) if s=="ambiguous control"));
    let Event::Step(step) = &rows[2].1 else {
        panic!()
    };
    assert_eq!(step.app.title.as_deref(), Some("Draft"));
    assert_eq!(step.app.process.as_deref(), Some("editor.exe"));
    assert_eq!(
        step.locator.within.as_ref().unwrap()["within"],
        json!({"role":"window","name":"Draft"})
    );
    w.release().unwrap();
    assert!(w.collect().is_err());
    let (mut out, mut caps) = (vec![], vec![]);
    w.compile_action(step, &mut out, &mut caps).unwrap();
    assert_eq!(out[0]["uses"], "control.session.act@1");
    assert!(caps.iter().any(|c| c["name"] == WINDOWS));
    assert!(f.replies.lock().unwrap().is_empty());
}

#[test]
fn uia_refuses_a_process_title_combination_that_would_be_ambiguous_on_replay() {
    let meta = window_meta();
    let mut other = meta.clone();
    other["title"] = json!("Draft copy");
    other["process_id"] = json!(99);
    let (_, s) = fixture(
        WINDOWS,
        vec![("list_windows", json!({"windows":[meta,other]}))],
    );
    assert!(Watch::start(
        &s,
        &json!({"provider":WINDOWS,"metadata":meta}),
        "Draft",
        &[]
    )
    .is_err());
}

#[test]
fn a_late_uia_dialog_is_subscribed_by_its_own_handle_and_reports_the_blind_interval() {
    let main = window_meta();
    let mut dialog = main.clone();
    dialog["title"] = json!("Save As");
    dialog["window_id"] = json!("hwnd:20");
    let mut other = dialog.clone();
    other["process_id"] = json!(99);
    other["process_name"] = json!("other.exe");
    other["window_id"] = json!("hwnd:33");
    let mut setup = windows_start();
    setup["window"] = dialog.clone();
    setup["root_locator"]["name"] = dialog["title"].clone();
    setup["capture_id"] = json!("d");
    setup["started_epoch_ms"] = json!(123900);
    let windows = json!({"windows":[main,other,dialog]});
    let idle =
        json!({"window_id":"hwnd:10","capture_id":"c","raw_events":0,"dropped":0,"steps":[]});
    let (fixture, session) = fixture(
        WINDOWS,
        vec![
            ("list_windows", json!({"windows":[main,other]})),
            ("watch", windows_start()),
            ("list_windows", windows.clone()),
            ("collect", idle.clone()),
            ("watch", setup),
            ("list_windows", windows),
            ("collect", idle),
            (
                "collect",
                json!({"window_id":"hwnd:20","capture_id":"d",
          "raw_events":1,"dropped":0,"steps":[{"epoch_ms":124000,"replayable":true,
          "action":"invoke","locator":{"role":"button","name":"Save"}}]}),
            ),
            ("release", json!({})),
            ("release", json!({})),
        ],
    );
    let mut watch = Watch::start(
        &session,
        &json!({"provider":WINDOWS,"metadata":main}),
        "Draft",
        &["Save As".into()],
    )
    .unwrap();
    let gaps = watch.collect().unwrap();
    assert_eq!(gaps.len(), 2);
    assert!(gaps.iter().all(|(_, e)| matches!(e, Event::Gap(_))));
    let rows = watch.collect().unwrap();
    assert_eq!(rows[0].0, 124000);
    let Event::Step(step) = &rows[0].1 else {
        panic!()
    };
    assert_eq!(step.app.title.as_deref(), Some("Save As"));
    watch.release().unwrap();
    assert!(fixture.replies.lock().unwrap().is_empty());
}

#[test]
fn failure_after_ax_subscribe_releases_the_provider_handle() {
    let (f, s) = fixture(
        MACOS,
        vec![
            (
                "snapshot",
                json!({"app":{"process_id":25},"truncated":false,"nodes":[{"role":"AXWindow","name":"Draft"}]}),
            ),
            ("watch", json!({"capture_id":"ax","pointer_watch":true})),
            ("release", json!({"released":true})),
        ],
    );
    assert!(Watch::start(
        &s,
        &json!({"provider":MACOS,"metadata":{"bundle_id":"org.editor","process_id":25}}),
        "Draft",
        &[]
    )
    .is_err());
    assert!(f.replies.lock().unwrap().is_empty());
}

#[test]
fn ax_calibrates_orders_filters_and_compiles_a_globally_renamed_dialog_step() {
    let event = |at, title, unique| {
        json!({"kind":"click","button":"left","at_ms":at,
        "node":{"role":"AXButton","name":"OK","protected":false,"process_id":25,
            "window":{"role":"AXWindow","name":title,"unique":unique}}})
    };
    let (f, s) = fixture(
        MACOS,
        vec![
            (
                "snapshot",
                json!({"app":{"process_id":25},"truncated":false,"nodes":[{"role":"AXWindow","name":"Draft"}]}),
            ),
            (
                "watch",
                json!({"capture_id":"ax","pointer_watch":true,"missing_notifications":[],
            "clock":{"at_ms":100,"epoch_ms":9000,"precision_ms":3}}),
            ),
            (
                "collect",
                json!({"events":[event(210,"Confirm",true),event(195,"Confirm",false),
          event(190,"Unrelated",true), {"kind":"key","at_ms":200,"node":null}],"dropped":0}),
            ),
            ("release", json!({"released":true})),
        ],
    );
    let mut w = Watch::start(
        &s,
        &json!({"provider":MACOS,"metadata":{"bundle_id":"org.editor","process_id":25}}),
        "Draft",
        &["Confirm".into()],
    )
    .unwrap();
    let rows = w.collect().unwrap();
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        [9090, 9095, 9100, 9110]
    );
    assert!(matches!(rows[0].1, Event::Filtered("other_native_window")));
    assert!(matches!(rows[1].1, Event::Gap(_)));
    assert!(matches!(rows[2].1, Event::Gap(_)));
    let Event::Step(mut step) = rows.into_iter().last().unwrap().1 else {
        panic!()
    };
    step.id = "action42".into();
    w.release().unwrap();
    assert_eq!(w.precision_ms(), 3);
    let mut doc = aad_control::steps::workflow("mixed", std::slice::from_ref(&*step)).unwrap();
    let mut out = vec![];
    let caps = doc["requires"]["capabilities"].as_array_mut().unwrap();
    w.compile_action(&step, &mut out, caps).unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(out[0]["with"]["app"], json!({"bundle_id":"org.editor"}));
    assert_eq!(
        out[1]["with"]["revision"],
        "${{ steps.action42_snapshot.output.revision }}"
    );
    assert_eq!(
        out[2]["with"]["target"],
        "${{ steps.action42_find.output.target }}"
    );
    assert_eq!(
        out[2]["with"]["locator"]["within"],
        json!({"role":"AXWindow","name":"Confirm"})
    );
    assert_eq!(out[2]["with"]["locator"], out[1]["with"]["locator"]);
    assert_eq!(out[2]["id"], "action42");
    assert!(caps.iter().any(|c| c["name"] == MACOS));
    doc["steps"] = json!(out);
    doc["budgets"]["max_executed_steps"] = json!(3);
    aad_core::compile_descriptor(doc, None).unwrap();
    assert!(f.replies.lock().unwrap().is_empty());
}
