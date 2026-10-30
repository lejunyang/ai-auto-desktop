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
    before_commit: Duration,
    entered: std::sync::atomic::AtomicBool,
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
    fn act(&self, _: &Value, _: &Value, _: &Action, context: &Context) -> Result<Value> {
        self.entered.store(true, Ordering::SeqCst);
        std::thread::sleep(self.before_commit);
        context.commit()?;
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
            view: Default::default(),
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
        before_commit: Duration::ZERO,
        entered: std::sync::atomic::AtomicBool::new(false),
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
                action: Action::click()
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
            action: Action::click(),
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
                action: Action::click()
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
                action: Action::click()
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
            action: Action::click(),
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

#[test]
fn cancellation_before_worker_admission_prevents_dispatch() {
    let (session, backend) = fixture(true, Duration::ZERO);
    let target = observed(&session);
    let request = Request::new(
        Some(session.id().into()),
        Operation::Act {
            target,
            action: Action::click(),
        },
    );
    assert!(session.cancel(&request.request_id));
    let response = session.execute(&request);
    assert_eq!(response.commit_state, CommitState::NotCommitted);
    assert_eq!(response.error.unwrap()["code"], "CONTROL.CANCELLED");
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn cancellation_during_preflight_wins_before_the_atomic_commit_boundary() {
    let backend = Arc::new(Fake {
        calls: AtomicUsize::new(0),
        delay: Duration::ZERO,
        before_commit: Duration::from_millis(100),
        entered: std::sync::atomic::AtomicBool::new(false),
    });
    let session = Arc::new(Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    ));
    let target = observed(&session);
    let request = Request::new(
        Some(session.id().into()),
        Operation::Act {
            target,
            action: Action::click(),
        },
    );
    let id = request.request_id.clone();
    let running = session.clone();
    let worker = std::thread::spawn(move || running.execute(&request));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !backend.entered.load(Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(session.cancel(&id));
    let response = worker.join().unwrap();
    assert_eq!(response.commit_state, CommitState::NotCommitted);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn deadline_before_and_after_commit_has_distinct_effects() {
    for before in [true, false] {
        let backend = Arc::new(Fake {
            calls: AtomicUsize::new(0),
            delay: if before {
                Duration::ZERO
            } else {
                Duration::from_millis(180)
            },
            before_commit: if before {
                Duration::from_millis(180)
            } else {
                Duration::ZERO
            },
            entered: std::sync::atomic::AtomicBool::new(false),
        });
        let session = Session::new(
            vec![backend.clone()],
            Policy {
                allow_input: true,
                ..Default::default()
            },
        );
        let target = observed(&session);
        let mut request = Request::new(
            Some(session.id().into()),
            Operation::Act {
                target,
                action: Action::click(),
            },
        );
        request.timeout_ms = 100;
        let response = session.execute(&request);
        assert_eq!(response.error.unwrap()["code"], "CONTROL.TIMEOUT");
        assert_eq!(
            response.commit_state,
            if before {
                CommitState::NotCommitted
            } else {
                CommitState::Unknown
            }
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), usize::from(!before));
    }
}

#[test]
fn close_expiry_and_bounded_replay_guard_revoke_observations() {
    let (session, backend) = fixture(true, Duration::ZERO);
    let target = observed(&session);
    session.close();
    let response = call(
        &session,
        Operation::Act {
            target,
            action: Action::click(),
        },
    );
    assert_eq!(response.error.unwrap()["code"], "CONTROL.SESSION_CLOSED");
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    let (session, _) = fixture(false, Duration::ZERO);
    let session = Arc::try_unwrap(session)
        .ok()
        .unwrap()
        .with_limits(Duration::from_secs(5), Duration::from_millis(1), 3)
        .unwrap();
    let target = observed(&session);
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(
        call(
            &session,
            Operation::Find {
                observation_id: target.observation_id,
                locator: Box::new(Locator {
                    node_id: Some("n".into()),
                    ..Default::default()
                }),
                expect: Default::default(),
            }
        )
        .error
        .unwrap()["code"],
        "CONTROL.STALE_TARGET"
    );
    assert_eq!(
        call(&session, Operation::Capabilities {}).error.unwrap()["code"],
        "CONTROL.SESSION_LIMIT"
    );
    assert_eq!(
        call(&session, Operation::Capabilities {}).error.unwrap()["code"],
        "CONTROL.SESSION_CLOSED"
    );
    let (session, _) = fixture(false, Duration::ZERO);
    let session = Arc::try_unwrap(session)
        .ok()
        .unwrap()
        .with_limits(Duration::from_millis(1), Duration::from_secs(1), 8)
        .unwrap();
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(
        call(&session, Operation::Targets {}).error.unwrap()["code"],
        "CONTROL.SESSION_CLOSED"
    );
}

struct Reader;
impl Backend for Reader {
    fn name(&self) -> &str {
        "fixture"
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into()]
    }
    fn reads(&self) -> Vec<String> {
        vec!["capture".into(), "read_text".into()]
    }
    fn resources(&self, timeout: Duration) -> Result<Vec<BackendResource>> {
        Fake {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            before_commit: Duration::ZERO,
            entered: std::sync::atomic::AtomicBool::new(false),
        }
        .resources(timeout)
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        Ok(json!({"nodes": [{"node_id":"n", "role":"pane", "name":"Canvas"}], "truncated":false}))
    }
    fn capture(&self, _: &Value, _: &Value, _: Duration) -> Result<Value> {
        Ok(json!({"format":"png","data":"cGl4ZWxz","width":200,"height":50}))
    }
    fn read_text(
        &self,
        _: &Value,
        node: &Value,
        language: Option<&str>,
        _: Duration,
    ) -> Result<Value> {
        assert_eq!(node["node_id"], "n");
        Ok(json!({
            "engine": "fixture-ocr",
            "language": language.unwrap_or("en-US"),
            "format": "png", "data": "cGl4ZWxz", "width": 200, "height": 50,
            "lines": [{"text": "Total 42", "bounds": {"x": 20, "y": 10, "width": 100, "height": 25},
                "words": [{"text": "Total", "bounds": {"x": 20, "y": 10, "width": 60, "height": 25}},
                          {"text": "42", "bounds": {"x": 90, "y": 10, "width": 30, "height": 25}}]}],
        }))
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, _: &Context) -> Result<Value> {
        unreachable!("reading never acts")
    }
}

#[test]
fn recognised_text_comes_back_as_element_fractions_with_the_capture_digest() {
    // Read-only: no input grant needed, and the observation stays usable.
    let session = Session::new(vec![Arc::new(Reader)], Policy::default());
    let target = observed(&session);
    let read = call(
        &session,
        Operation::ReadText {
            target: target.clone(),
            language: None,
        },
    );
    assert_eq!(read.commit_state, CommitState::NotCommitted);
    let read = read.data.unwrap();
    assert_eq!(read["text"], "Total 42");
    assert_eq!(read["read_only"], true);
    assert_eq!(
        read["lines"][0]["box"],
        json!({"fx": 0.1, "fy": 0.2, "fw": 0.5, "fh": 0.5})
    );
    assert_eq!(read["lines"][0]["words"][1]["box"]["fx"], 0.45);
    // Neither the image nor any pixel or screen coordinate is handed out.
    let text = read.to_string();
    for absent in ["\"data\"", "\"x\"", "\"y\"", "\"bounds\""] {
        assert!(!text.contains(absent), "{absent} in {text}");
    }
    // The digest is the one a capture of the same pixels carries.
    let shot = call(
        &session,
        Operation::Capture {
            target: target.clone(),
        },
    )
    .data
    .unwrap();
    assert_eq!(read["digest"], shot["digest"]);

    let refused = call(
        &session,
        Operation::ReadText {
            target,
            language: Some("en US; rm".into()),
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
}

#[test]
fn a_provider_without_ocr_refuses_to_read_text() {
    let (session, _) = fixture(true, Duration::ZERO);
    let target = observed(&session);
    let refused = call(
        &session,
        Operation::ReadText {
            target,
            language: None,
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "CONTROL.ACTION_UNSUPPORTED");
}

#[test]
fn the_clipboard_is_read_only_with_its_own_grant() {
    struct Clipboard;
    impl Backend for Clipboard {
        fn name(&self) -> &str {
            "fixture"
        }
        fn capabilities(&self) -> Vec<String> {
            vec!["paste".into()]
        }
        fn clipboard_read(&self, max: Option<usize>, _: Duration) -> Result<Value> {
            Ok(
                json!({"has_text": true, "text": "copied", "characters": 6, "truncated": max == Some(1)}),
            )
        }
        fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
            Ok(Vec::new())
        }
        fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
            unreachable!()
        }
        fn act(&self, _: &Value, _: &Value, _: &Action, _: &Context) -> Result<Value> {
            unreachable!()
        }
    }
    let read = Operation::ClipboardRead {
        max_characters: None,
    };
    // Input alone does not grant it, and it is not advertised without the grant.
    let input_only = Session::new(
        vec![Arc::new(Clipboard)],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    assert_eq!(
        call(&input_only, read.clone()).error.unwrap()["code"],
        "POLICY.DENIED"
    );
    assert!(call(&input_only, Operation::Capabilities {})
        .data
        .unwrap()
        .get("clipboard_read")
        .is_none());
    let granted = Session::new(
        vec![Arc::new(Clipboard)],
        Policy {
            allow_clipboard_read: true,
            ..Default::default()
        },
    );
    assert_eq!(
        call(&granted, Operation::Capabilities {}).data.unwrap()["clipboard_read"],
        true
    );
    let answer = call(&granted, read);
    assert_eq!(answer.commit_state, CommitState::NotCommitted);
    assert_eq!(answer.data.unwrap()["text"], "copied");
    let bounded = call(
        &granted,
        Operation::ClipboardRead {
            max_characters: Some(0),
        },
    );
    assert_eq!(bounded.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
}

#[test]
fn global_keys_need_both_grants_and_never_claim_a_verified_effect() {
    struct Keys(AtomicUsize);
    impl Backend for Keys {
        fn name(&self) -> &str {
            "fixture"
        }
        fn capabilities(&self) -> Vec<String> {
            vec!["key".into()]
        }
        fn offers_global_keys(&self) -> bool {
            true
        }
        fn global_key(&self, keys: &[String], context: &Context) -> Result<Value> {
            context.commit()?;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"keys": keys}))
        }
        fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
            Ok(Vec::new())
        }
        fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
            unreachable!()
        }
        fn act(&self, _: &Value, _: &Value, _: &Action, _: &Context) -> Result<Value> {
            unreachable!()
        }
    }
    let press = || Operation::GlobalKey {
        keys: vec!["win".into(), "d".into()],
    };
    for policy in [
        Policy::default(),
        Policy {
            allow_input: true,
            ..Default::default()
        },
        Policy {
            allow_global_keys: true,
            ..Default::default()
        },
    ] {
        let backend = Arc::new(Keys(AtomicUsize::new(0)));
        let session = Session::new(vec![backend.clone()], policy);
        let refused = call(&session, press());
        assert_eq!(refused.error.unwrap()["code"], "POLICY.DENIED");
        assert_eq!(refused.commit_state, CommitState::NotCommitted);
        assert_eq!(backend.0.load(Ordering::SeqCst), 0);
        assert!(call(&session, Operation::Capabilities {})
            .data
            .unwrap()
            .get("global_keys")
            .is_none());
    }
    let backend = Arc::new(Keys(AtomicUsize::new(0)));
    let session = Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: true,
            allow_global_keys: true,
            ..Default::default()
        },
    );
    assert_eq!(
        call(&session, Operation::Capabilities {}).data.unwrap()["global_keys"],
        true
    );
    let pressed = call(&session, press());
    assert!(pressed.error.is_none(), "{:?}", pressed.error);
    assert_eq!(pressed.effect, "dispatched");
    assert_eq!(pressed.commit_state, CommitState::Unknown);
    assert_eq!(pressed.data.unwrap()["verified"], false);
    assert_eq!(backend.0.load(Ordering::SeqCst), 1);
    let malformed = call(
        &session,
        Operation::GlobalKey {
            keys: vec!["x".into(); 7],
        },
    );
    assert_eq!(malformed.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
}
