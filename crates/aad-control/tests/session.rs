use aad_control::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

struct Fake {
    calls: AtomicUsize,
    delay: Duration,
}
impl Backend for Fake {
    fn name(&self) -> &str {
        "fixture"
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into()]
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
        Ok(json!({"nodes": [{"node_id":"n", "role":"button", "name":"Save"}], "truncated":false}))
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, _: Duration) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Ok(json!({"ok":true}))
    }
}
fn call(session: &Session, op: Operation) -> Response {
    session.execute(&Request::new(Some(session.id().into()), op))
}
fn observed(session: &Session) -> Target {
    assert!(call(session, Operation::Targets {}).error.is_none());
    let observed = call(
        session,
        Operation::Observe {
            resource_id: "fixture:window".into(),
            max_nodes: 10,
        },
    )
    .data
    .unwrap();
    serde_json::from_value(observed["nodes"][0]["target"].clone()).unwrap()
}
fn fixture(input: bool, delay: Duration) -> (Arc<Session>, Arc<Fake>) {
    let backend = Arc::new(Fake {
        calls: AtomicUsize::new(0),
        delay,
    });
    (
        Arc::new(Session::new(
            vec![backend.clone()],
            Policy {
                allow_input: input,
                ..Default::default()
            },
        )),
        backend,
    )
}
#[test]
fn targets_are_session_bound_consumed_and_not_replayable() {
    let (a, backend) = fixture(true, Duration::ZERO);
    let (b, _) = fixture(true, Duration::ZERO);
    let target = observed(&a);
    assert_eq!(
        call(
            &b,
            Operation::Act {
                target: target.clone(),
                action: Action::Click {}
            }
        )
        .error
        .unwrap()["code"],
        "CONTROL.SESSION_MISMATCH"
    );
    let request = Request::new(
        Some(a.id().into()),
        Operation::Act {
            target: target.clone(),
            action: Action::Click {},
        },
    );
    assert!(a.execute(&request).error.is_none());
    assert_eq!(
        a.execute(&request).error.unwrap()["code"],
        "CONTROL.DUPLICATE_REQUEST"
    );
    assert_eq!(
        call(
            &a,
            Operation::Act {
                target,
                action: Action::Click {}
            }
        )
        .error
        .unwrap()["code"],
        "CONTROL.STALE_TARGET"
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn policy_and_unsupported_actions_do_not_dispatch() {
    let (session, backend) = fixture(false, Duration::ZERO);
    let target = observed(&session);
    assert_eq!(
        call(
            &session,
            Operation::Act {
                target,
                action: Action::Click {}
            }
        )
        .effect,
        "not_applied"
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    let (session, backend) = fixture(true, Duration::ZERO);
    let target = observed(&session);
    assert_eq!(
        call(
            &session,
            Operation::Act {
                target,
                action: Action::Key {
                    keys: vec!["Enter".into()]
                }
            }
        )
        .error
        .unwrap()["code"],
        "CONTROL.ACTION_UNSUPPORTED"
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn cancellation_after_dispatch_reports_unknown_and_does_not_retry() {
    let (session, backend) = fixture(true, Duration::from_millis(80));
    let target = observed(&session);
    let request = Request::new(
        Some(session.id().into()),
        Operation::Act {
            target,
            action: Action::Click {},
        },
    );
    let id = request.request_id.clone();
    let running = session.clone();
    let worker = std::thread::spawn(move || running.execute(&request));
    while backend.calls.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    assert!(session.cancel(&id));
    let response = worker.join().unwrap();
    assert_eq!(response.effect, "unknown");
    assert_eq!(response.error.unwrap()["code"], "CONTROL.CANCELLED");
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn unknown_fields_and_coordinate_bypasses_are_rejected() {
    assert!(serde_json::from_value::<Action>(json!({"type":"click","x":10,"y":20})).is_err());
    assert!(serde_json::from_value::<Target>(json!({"session_id":"s","resource_id":"r","observation_id":"o","node_id":"n","provider":"arbitrary"})).is_err());
}
