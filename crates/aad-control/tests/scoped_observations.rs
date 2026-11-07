//! A partial tree may anchor a new read, but never grant input by itself.
use aad_control::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Default)]
struct Scoped {
    scoped_reads: AtomicUsize,
    writes: AtomicUsize,
}

fn subtree() -> Value {
    json!({"scope_id":"opaque-native-scope", "scope_revision":2, "truncated":false,
        "nodes":[{"node_id":"n0","role":"pane","name":"Editor","actions":[]},
                 {"node_id":"n1","role":"button","name":"Save","actions":["click"]},
                 {"node_id":"context","role":"pane","name":"Save","actions":[],"provenance":{"scope_context":true}}]})
}

impl Backend for Scoped {
    fn name(&self) -> &str {
        "scoped-fixture"
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into()]
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(["first", "second"]
            .into_iter()
            .map(|id| BackendResource {
                resource: Resource {
                    resource_id: id.into(),
                    provider: self.name().into(),
                    kind: "desktop".into(),
                    name: id.into(),
                    capabilities: self.capabilities(),
                    metadata: json!({}),
                },
                selector: json!({"application":id}),
            })
            .collect())
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        let mut snapshot = subtree();
        snapshot.as_object_mut().unwrap().remove("scope_id");
        snapshot["truncated"] = json!(true);
        snapshot["truncation_reasons"] = json!([{"reason":"max_depth","node_id":"n0"}]);
        Ok(snapshot)
    }
    fn observe_target_scope(
        &self,
        snapshot: &Value,
        root: &Value,
        _: usize,
        _: Duration,
    ) -> Result<Value> {
        assert_eq!(snapshot["truncated"], true);
        assert_eq!(root["node_id"], "n0");
        self.scoped_reads.fetch_add(1, Ordering::SeqCst);
        Ok(subtree())
    }
    fn observe_scope(&self, selector: &Value, scope: &str, _: usize, _: Duration) -> Result<Value> {
        assert_eq!(selector["application"], "first");
        assert_eq!(scope, "opaque-native-scope");
        self.scoped_reads.fetch_add(1, Ordering::SeqCst);
        Ok(subtree())
    }
    fn act(&self, snapshot: &Value, _: &Value, _: &Action, context: &Context) -> Result<Value> {
        assert_eq!(snapshot["scope_id"], "opaque-native-scope");
        assert_eq!(snapshot["truncated"], false);
        context.commit()?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok":true}))
    }
}

fn call(session: &Session, operation: Operation) -> Response {
    session.execute(&Request::new(Some(session.id().into()), operation))
}
fn observe(session: &Session, resource: &str, view: View) -> Response {
    call(
        session,
        Operation::Observe {
            resource_id: resource.into(),
            max_nodes: 100,
            view,
        },
    )
}
fn setup(input: bool) -> (Session, Arc<Scoped>, Value) {
    let backend = Arc::new(Scoped::default());
    let session = Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: input,
            ..Default::default()
        },
    );
    assert!(call(&session, Operation::Targets {}).error.is_none());
    let initial = observe(&session, "first", View::default()).data.unwrap();
    (session, backend, initial)
}
fn target(snapshot: &Value, index: usize) -> Target {
    serde_json::from_value(snapshot["nodes"][index]["target"].clone()).unwrap()
}
fn code(response: Response) -> String {
    response.error.unwrap()["code"].as_str().unwrap().into()
}

#[test]
fn incomplete_tree_can_be_recaptured_at_an_observed_root_and_verified_after_input() {
    let (session, backend, initial) = setup(true);
    assert_eq!(initial["truncation_reasons"][0]["reason"], "max_depth");
    assert_eq!(
        code(call(
            &session,
            Operation::Act {
                target: target(&initial, 1),
                action: Action::click(),
                verify: None,
            }
        )),
        "CONTROL.OBSERVATION_TRUNCATED"
    );
    let scoped = observe(
        &session,
        "first",
        View {
            scope_target: Some(target(&initial, 0)),
            ..Default::default()
        },
    )
    .data
    .unwrap();
    assert_eq!(scoped["scope"]["complete"], true);
    let outside = call(
        &session,
        Operation::Find {
            observation_id: scoped["observation_id"].as_str().unwrap().into(),
            locator: Box::new(Locator {
                node_id: Some("context".into()),
                ..Default::default()
            }),
            expect: Expect::Optional,
        },
    )
    .data
    .unwrap();
    assert_eq!(outside["found"], false);
    let outcome = call(
        &session,
        Operation::Act {
            target: target(&scoped, 1),
            action: Action::click(),
            verify: Some(Box::new(Locator {
                name: Some("Save".into()),
                ..Default::default()
            })),
        },
    )
    .data
    .unwrap();
    assert_eq!(outcome["verified"], true, "{outcome}");
    assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    assert_eq!(backend.scoped_reads.load(Ordering::SeqCst), 2);
    // The parent capture cannot be reused after drilling into its subtree.
    assert_eq!(
        code(observe(
            &session,
            "first",
            View {
                scope_target: Some(target(&initial, 0)),
                ..Default::default()
            }
        )),
        "CONTROL.STALE_TARGET"
    );
}

#[test]
fn scope_reads_reject_forged_session_resource_node_and_conflicting_options() {
    let (session, backend, initial) = setup(false);
    let anchor = target(&initial, 0);
    let mut wrong = anchor.clone();
    wrong.session_id = "other-session".into();
    assert_eq!(
        code(observe(
            &session,
            "first",
            View {
                scope_target: Some(wrong),
                ..Default::default()
            }
        )),
        "CONTROL.SESSION_MISMATCH"
    );
    assert_eq!(
        code(observe(
            &session,
            "second",
            View {
                scope_target: Some(anchor.clone()),
                ..Default::default()
            }
        )),
        "POLICY.DENIED"
    );
    let mut wrong = anchor.clone();
    wrong.node_id = "never-observed".into();
    assert_eq!(
        code(observe(
            &session,
            "first",
            View {
                scope_target: Some(wrong),
                ..Default::default()
            }
        )),
        "CONTROL.STALE_TARGET"
    );
    assert_eq!(
        code(observe(
            &session,
            "first",
            View {
                scope_target: Some(anchor.clone()),
                scope_id: Some("ambiguous".into()),
                ..Default::default()
            }
        )),
        "CONTROL.INVALID_REQUEST"
    );
    assert_eq!(backend.scoped_reads.load(Ordering::SeqCst), 0);
    // Read-only grants may recover observations, but gain no write permission.
    let scoped = observe(
        &session,
        "first",
        View {
            scope_target: Some(anchor),
            ..Default::default()
        },
    )
    .data
    .unwrap();
    assert_eq!(
        code(call(
            &session,
            Operation::Act {
                target: target(&scoped, 1),
                action: Action::click(),
                verify: None,
            }
        )),
        "POLICY.DENIED"
    );
    assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
}
