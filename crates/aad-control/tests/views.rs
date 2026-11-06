//! Observation views, locator fallbacks and the gesture actions of the shared session.
use aad_control::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Fake {
    acted: Mutex<Vec<Action>>,
}
impl Backend for Fake {
    fn name(&self) -> &str {
        "fixture"
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into(), "drag".into(), "scroll".into()]
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(vec![BackendResource {
            resource: Resource {
                resource_id: "fixture:window".into(),
                provider: "fixture".into(),
                kind: "desktop".into(),
                name: "test".into(),
                capabilities: self.capabilities(),
                metadata: json!({}),
            },
            selector: json!({}),
        }])
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        Ok(json!({"truncated": false, "nodes": [
            {"node_id": "n1", "role": "button", "name": "Save", "actions": ["click", "drag"]},
            {"node_id": "n2", "role": "button", "name": "Save", "actions": ["click"]},
            {"node_id": "n3", "role": "pane", "name": "", "actions": []},
            {"node_id": "n4", "role": "list", "name": "Done", "actions": ["drag"]},
            {"node_id": "n5", "role": "button", "name": "Hidden", "actions": ["click"], "states": {"offscreen": true}},
        ]}))
    }
    fn act(&self, _: &Value, _: &Value, action: &Action, context: &Context) -> Result<Value> {
        context.commit()?;
        self.acted.lock().unwrap().push(action.clone());
        Ok(json!({"ok": true}))
    }
}

fn session() -> (Session, Arc<Fake>) {
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
    (session, backend)
}
fn call(session: &Session, operation: Operation) -> Response {
    session.execute(&Request::new(Some(session.id().into()), operation))
}
fn observe(session: &Session, view: View) -> Response {
    assert!(call(session, Operation::Targets {}).error.is_none());
    call(
        session,
        Operation::Observe {
            resource_id: "fixture:window".into(),
            max_nodes: 100,
            view,
        },
    )
}
fn code(response: &Response) -> String {
    response.error.as_ref().unwrap()["code"]
        .as_str()
        .unwrap()
        .into()
}
fn target(observation: &Value, node: &str) -> Target {
    let nodes = observation["nodes"].as_array().unwrap();
    let node = nodes.iter().find(|n| n["node_id"] == node).unwrap();
    serde_json::from_value(node["target"].clone()).unwrap()
}

#[test]
fn outline_lists_named_or_actionable_onscreen_nodes_with_session_targets() {
    let (session, _) = session();
    let outline = observe(
        &session,
        View {
            mode: ViewMode::Outline,
            ..Default::default()
        },
    )
    .data
    .unwrap();
    let shown: Vec<_> = outline["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node_id"].as_str().unwrap())
        .collect();
    assert_eq!(shown, ["n1", "n2", "n4"]);
    assert_eq!(outline["matched"], 3);
    assert_eq!(outline["node_count"], 5);
    assert_eq!(
        outline["nodes"][0]["capabilities"],
        json!(["click", "drag"])
    );
    assert_eq!(outline["nodes"][0]["target"]["session_id"], session.id());

    let limited = observe(
        &session,
        View {
            mode: ViewMode::Outline,
            limit: Some(1),
            ..Default::default()
        },
    )
    .data
    .unwrap();
    assert_eq!(limited["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(limited["stopped_by"], "limit");
    assert_eq!(limited["outline_truncated"], true);
}

#[test]
fn views_without_provider_support_or_with_misplaced_options_are_refused() {
    let (session, _) = session();
    let overview = observe(
        &session,
        View {
            mode: ViewMode::Overview,
            ..Default::default()
        },
    );
    assert_eq!(code(&overview), "CONTROL.VIEW_UNSUPPORTED");
    let text = observe(
        &session,
        View {
            mode: ViewMode::Text,
            ..Default::default()
        },
    );
    assert_eq!(code(&text), "CONTROL.VIEW_UNSUPPORTED");
    let limited_text = observe(
        &session,
        View {
            mode: ViewMode::Text,
            limit: Some(5),
            ..Default::default()
        },
    );
    assert_eq!(code(&limited_text), "CONTROL.INVALID_REQUEST");
    let misplaced = observe(
        &session,
        View {
            region: Some("toolbar".into()),
            ..Default::default()
        },
    );
    assert_eq!(code(&misplaced), "CONTROL.INVALID_REQUEST");
    let oversized = observe(
        &session,
        View {
            mode: ViewMode::Outline,
            limit: Some(501),
            ..Default::default()
        },
    );
    assert_eq!(code(&oversized), "CONTROL.INVALID_REQUEST");
}

#[test]
fn portable_find_lists_candidates_answers_optional_and_refuses_extensions() {
    let (session, _) = session();
    let observation = observe(&session, View::default()).data.unwrap();
    let id = observation["observation_id"].as_str().unwrap().to_string();
    let find = |locator: Locator, expect: Expect| {
        call(
            &session,
            Operation::Find {
                observation_id: id.clone(),
                locator: Box::new(locator),
                expect,
            },
        )
    };
    let ambiguous = find(
        Locator {
            name: Some("Save".into()),
            ..Default::default()
        },
        Expect::One,
    );
    assert_eq!(code(&ambiguous), "CONTROL.AMBIGUOUS");
    let details = &ambiguous.error.as_ref().unwrap()["details"];
    assert_eq!(details["match_count"], 2);
    assert_eq!(details["candidates"].as_array().unwrap().len(), 2);

    let missing = Locator {
        name: Some("Absent".into()),
        ..Default::default()
    };
    assert_eq!(
        code(&find(missing.clone(), Expect::One)),
        "CONTROL.NOT_FOUND"
    );
    let optional = find(missing, Expect::Optional).data.unwrap();
    assert_eq!(optional["found"], false);

    let extended = find(
        Locator {
            automation_id: Some("save".into()),
            ..Default::default()
        },
        Expect::One,
    );
    assert_eq!(code(&extended), "CONTROL.LOCATOR_UNSUPPORTED");

    let unique = find(
        Locator {
            name: Some("Done".into()),
            ..Default::default()
        },
        Expect::One,
    )
    .data
    .unwrap();
    assert_eq!(unique["found"], true);
    assert_eq!(unique["target"]["node_id"], "n4");
}

#[test]
fn drag_ends_share_one_observation_and_gestures_follow_node_capabilities() {
    let (session, backend) = session();
    let observation = observe(&session, View::default()).data.unwrap();
    let from = target(&observation, "n1");
    let to = target(&observation, "n4");
    let drag = |to: Target| Action::Drag {
        path: Vec::new(),
        to,
        button: None,
        fraction: None,
        to_fraction: None,
        modifiers: Vec::new(),
        hold_ms: None,
    };
    let act = |target: Target, action: Action| {
        call(
            &session,
            Operation::Act {
                target,
                action,
                verify: None,
            },
        )
    };

    let mut foreign = to.clone();
    foreign.observation_id = "elsewhere".into();
    assert_eq!(
        code(&act(from.clone(), drag(foreign))),
        "CONTROL.INVALID_REQUEST"
    );
    let outside = Action::Click {
        button: None,
        count: None,
        fraction: Some(Fraction { fx: 1.5, fy: 0.5 }),
        modifiers: Vec::new(),
        hold_ms: None,
    };
    assert_eq!(code(&act(from.clone(), outside)), "CONTROL.INVALID_REQUEST");
    let scroll = Action::Scroll {
        direction: Direction::Down,
        notches: None,
        fraction: None,
    };
    assert_eq!(
        code(&act(from.clone(), scroll)),
        "CONTROL.ACTION_UNSUPPORTED"
    );
    assert!(backend.acted.lock().unwrap().is_empty());

    let dragged = act(from, drag(to));
    assert!(dragged.error.is_none(), "{:?}", dragged.error);
    assert!(matches!(
        backend.acted.lock().unwrap().as_slice(),
        [Action::Drag { to, .. }] if to.node_id == "n4"
    ));
}

#[test]
fn drag_moves_between_fractions_of_one_observed_surface() {
    let (session, backend) = session();
    let observation = observe(&session, View::default()).data.unwrap();
    let surface = target(&observation, "n1");
    let drag = |to: Target, destination: Fraction| Action::Drag {
        path: Vec::new(),
        to,
        button: None,
        fraction: Some(Fraction { fx: 0.2, fy: 0.3 }),
        to_fraction: Some(destination),
        modifiers: Vec::new(),
        hold_ms: None,
    };
    let act = |action| {
        call(
            &session,
            Operation::Act {
                target: surface.clone(),
                action,
                verify: None,
            },
        )
    };
    let end = Fraction { fx: 0.8, fy: 0.7 };
    for field in ["session", "resource", "observation", "node"] {
        let mut foreign = surface.clone();
        match field {
            "session" => foreign.session_id = "elsewhere".into(),
            "resource" => foreign.resource_id = "elsewhere".into(),
            "observation" => foreign.observation_id = "elsewhere".into(),
            "node" => foreign.node_id = "unobserved".into(),
            _ => unreachable!(),
        }
        let response = act(drag(foreign, end));
        assert_eq!(
            code(&response),
            if field == "node" {
                "CONTROL.STALE_TARGET"
            } else {
                "CONTROL.INVALID_REQUEST"
            },
            "{field}"
        );
    }
    assert_eq!(
        code(&act(drag(surface.clone(), Fraction { fx: 1.1, fy: 0.7 }))),
        "CONTROL.INVALID_REQUEST"
    );
    assert!(backend.acted.lock().unwrap().is_empty());
    let action = drag(surface.clone(), end);
    let response = act(action.clone());
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        serde_json::to_value(&*backend.acted.lock().unwrap()).unwrap(),
        json!([action])
    );
    // Dispatch consumes the observation even when both ends use one node.
    assert_eq!(
        code(&act(drag(surface.clone(), end))),
        "CONTROL.STALE_TARGET"
    );
}

#[test]
fn drag_paths_reject_unobserved_or_foreign_anchors_before_dispatch() {
    let (session, backend) = session();
    let observation = observe(&session, View::default()).data.unwrap();
    let surface = target(&observation, "n1");
    let action = json!({"type":"drag", "to":surface,
        "fraction":{"fx":0.1,"fy":0.2}, "to_fraction":{"fx":0.9,"fy":0.8},
        "path":[{"target":surface,"fraction":{"fx":0.5,"fy":0.1}}]});
    let act = |value: Value| {
        call(
            &session,
            Operation::Act {
                target: surface.clone(),
                action: serde_json::from_value(value).unwrap(),
                verify: None,
            },
        )
    };
    for field in ["session_id", "resource_id", "observation_id", "node_id"] {
        let mut bad = action.clone();
        bad["path"][0]["target"][field] = json!("unobserved");
        assert_eq!(code(&act(bad)), "CONTROL.INVALID_REQUEST", "{field}");
    }
    let mut foreign = action.clone();
    foreign["to"] = json!(target(&observation, "n4"));
    assert_eq!(code(&act(foreign)), "CONTROL.INVALID_REQUEST");
    for field in ["fraction", "to_fraction"] {
        let mut bad = action.clone();
        bad.as_object_mut().unwrap().remove(field);
        assert_eq!(code(&act(bad)), "CONTROL.INVALID_REQUEST");
    }
    let mut outside = action.clone();
    outside["path"][0]["fraction"]["fx"] = json!(1.01);
    assert_eq!(code(&act(outside)), "CONTROL.INVALID_REQUEST");
    let mut too_long = action.clone();
    too_long["path"] = json!(vec![action["path"][0].clone(); 33]);
    assert_eq!(code(&act(too_long)), "CONTROL.INVALID_REQUEST");
    assert!(backend.acted.lock().unwrap().is_empty());
    let result = act(action.clone());
    assert!(result.error.is_none(), "{result:?}");
    assert_eq!(
        serde_json::to_value(&*backend.acted.lock().unwrap()).unwrap(),
        json!([action])
    );
}

#[test]
fn holds_are_bounded_single_clicks_and_cannot_outlive_the_request_budget() {
    let (session, backend) = session();
    let observation = observe(&session, View::default()).data.unwrap();
    let surface = target(&observation, "n1");
    for action in [
        json!({"type":"click", "hold_ms":0}),
        json!({"type":"click", "hold_ms":5001}),
        json!({"type":"click", "hold_ms":10, "count":2}),
        json!({"type":"drag", "to":surface, "hold_ms":5001}),
        json!({"type":"key", "keys":["a"], "hold_ms":0}),
    ] {
        let response = call(
            &session,
            Operation::Act {
                target: surface.clone(),
                action: serde_json::from_value(action).unwrap(),
                verify: None,
            },
        );
        assert_eq!(code(&response), "CONTROL.INVALID_REQUEST");
    }
    let held: Action = serde_json::from_value(json!({"type":"click", "hold_ms":500})).unwrap();
    assert!(
        !held.plain_click(),
        "a hold must not use semantic activation"
    );
    let mut request = Request::new(
        Some(session.id().into()),
        Operation::Act {
            target: surface.clone(),
            action: held,
            verify: None,
        },
    );
    request.timeout_ms = 100;
    assert_eq!(code(&session.execute(&request)), "CONTROL.TIMEOUT");
    assert!(backend.acted.lock().unwrap().is_empty());
    let held: Action =
        serde_json::from_value(json!({"type":"drag", "to":surface, "hold_ms":10})).unwrap();
    let response = call(
        &session,
        Operation::Act {
            target: surface,
            action: held,
            verify: None,
        },
    );
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(backend.acted.lock().unwrap()[0].hold_ms(), Some(10));
}

#[test]
fn pointer_modifiers_are_validated_before_dispatch() {
    for kind in ["click", "drag"] {
        let (session, backend) = session();
        let observation = observe(&session, View::default()).data.unwrap();
        let surface = target(&observation, "n1");
        let mut value = json!({"type": kind, "modifiers": ["ctrl", "ctrl"]});
        if kind == "drag" {
            value["to"] = json!(surface);
        }
        let act = |value| {
            call(
                &session,
                Operation::Act {
                    target: surface.clone(),
                    action: serde_json::from_value(value).unwrap(),
                    verify: None,
                },
            )
        };
        assert_eq!(code(&act(value.clone())), "CONTROL.INVALID_REQUEST");
        assert!(backend.acted.lock().unwrap().is_empty());
        value["modifiers"] = json!(["ctrl", "shift", "alt", "meta"]);
        let response = act(value.clone());
        assert!(response.error.is_none(), "{response:?}");
        assert_eq!(
            serde_json::to_value(&backend.acted.lock().unwrap()[0]).unwrap(),
            value
        );
    }
}

#[test]
fn actions_round_trip_with_optional_fields_and_reject_unknown_ones() {
    let click: Action = serde_json::from_value(json!({"type": "click"})).unwrap();
    assert!(click.plain_click());
    let right: Action =
        serde_json::from_value(json!({"type": "click", "button": "right", "count": 2})).unwrap();
    assert!(!right.plain_click());
    let modified: Action =
        serde_json::from_value(json!({"type":"click", "modifiers":["shift"]})).unwrap();
    assert!(!modified.plain_click());
    assert!(
        serde_json::from_value::<Action>(json!({"type":"click", "modifiers":["win"]})).is_err()
    );
    assert_eq!(
        serde_json::to_value(click).unwrap(),
        json!({"type":"click"})
    );
    assert!(serde_json::from_value::<Action>(json!({"type": "click", "x": 3})).is_err());
    assert!(serde_json::from_value::<Action>(
        json!({"type": "hover", "fraction": {"fx": 0.1, "fy": 0.2, "x": 1}})
    )
    .is_err());
}

/// A provider whose nodes name the region they sit in.
struct Regional;
impl Backend for Regional {
    fn name(&self) -> &str {
        "regional"
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into()]
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(vec![BackendResource {
            resource: Resource {
                resource_id: "regional:page".into(),
                provider: "regional".into(),
                kind: "browser".into(),
                name: "page".into(),
                capabilities: self.capabilities(),
                metadata: json!({}),
            },
            selector: json!({}),
        }])
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        Ok(json!({"truncated": false, "nodes": [
            {"node_id": "n1", "role": "link", "name": "Home", "actions": ["click"], "region": "Primary"},
            {"node_id": "n2", "role": "link", "name": "Docs", "actions": ["click"], "region": "Primary"},
            {"node_id": "n3", "role": "button", "name": "Buy", "actions": ["click"], "region": "Results"},
            {"node_id": "n4", "role": "cell", "name": "Tea", "actions": [], "region": "Results"},
            {"node_id": "n5", "role": "button", "name": "Help", "actions": ["click"], "region": null},
        ]}))
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, _: &Context) -> Result<Value> {
        Ok(json!({}))
    }
}

#[test]
fn nodes_that_name_their_region_get_an_overview_and_region_outlines() {
    let session = Session::new(vec![Arc::new(Regional)], Policy::default());
    assert!(call(&session, Operation::Targets {}).error.is_none());
    let view = |view: View| {
        call(
            &session,
            Operation::Observe {
                resource_id: "regional:page".into(),
                max_nodes: 100,
                view,
            },
        )
    };
    let overview = view(View {
        mode: ViewMode::Overview,
        ..Default::default()
    })
    .data
    .unwrap();
    assert_eq!(overview["regions"][0]["region"], "Primary");
    assert_eq!(overview["regions"][0]["elements"], 2);
    assert_eq!(
        overview["regions"][0]["holds"],
        json!([{"role": "link", "count": 2}])
    );
    assert_eq!(overview["interactive"], 4);
    assert!(overview["regions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["region"] == LOOSE_REGION));
    let results = view(View {
        mode: ViewMode::Outline,
        region: Some("Results".into()),
        ..Default::default()
    })
    .data
    .unwrap();
    let shown: Vec<_> = results["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node_id"].as_str().unwrap())
        .collect();
    assert_eq!(shown, ["n3", "n4"]);
    assert_eq!(results["region"], "Results");
    let missing = view(View {
        mode: ViewMode::Outline,
        region: Some("Nowhere".into()),
        ..Default::default()
    });
    let failure = missing.error.unwrap();
    assert_eq!(failure["code"], "CONTROL.NOT_FOUND");
    assert_eq!(failure["details"]["regions"].as_array().unwrap().len(), 3);
}
