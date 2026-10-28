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
        to,
        button: None,
        fraction: None,
        to_fraction: None,
    };
    let act = |target: Target, action: Action| call(&session, Operation::Act { target, action });

    assert_eq!(
        code(&act(from.clone(), drag(from.clone()))),
        "CONTROL.INVALID_REQUEST"
    );
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
fn actions_round_trip_with_optional_fields_and_reject_unknown_ones() {
    let click: Action = serde_json::from_value(json!({"type": "click"})).unwrap();
    assert!(click.plain_click());
    let right: Action =
        serde_json::from_value(json!({"type": "click", "button": "right", "count": 2})).unwrap();
    assert!(!right.plain_click());
    assert!(serde_json::from_value::<Action>(json!({"type": "click", "x": 3})).is_err());
    assert!(serde_json::from_value::<Action>(
        json!({"type": "hover", "fraction": {"fx": 0.1, "fy": 0.2, "x": 1}})
    )
    .is_err());
}
