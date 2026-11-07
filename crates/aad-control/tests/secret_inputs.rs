use aad_control::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

const SECRET: &str = "synthetic-private-value-测试";

struct PasswordBackend {
    writes: AtomicUsize,
    fail: bool,
    protected: bool,
}
impl Backend for PasswordBackend {
    fn name(&self) -> &str {
        "password-fixture"
    }
    fn capabilities(&self) -> Vec<String> {
        ["secret_input", "fill", "type_text", "paste", "key"]
            .map(str::to_owned)
            .to_vec()
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(vec![BackendResource {
            resource: Resource {
                resource_id: "password-field".into(),
                provider: self.name().into(),
                kind: "desktop".into(),
                name: "fixture".into(),
                capabilities: self.capabilities(),
                metadata: json!({}),
            },
            selector: json!({}),
        }])
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        Ok(
            json!({"nodes":[{"node_id":"field", "role":"edit", "name":"Password", "value":SECRET,
            "states":{"protected":self.protected,"enabled":true,"editable":true},
            "actions":self.capabilities()}],"truncated":false}),
        )
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, _: &Context) -> Result<Value> {
        panic!("protected input must not use the ordinary action dispatch")
    }
    fn secret_input(&self, _: &Value, _: &Value, text: &str, context: &Context) -> Result<Value> {
        assert_eq!(text, SECRET);
        context.commit()?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(error("PLATFORM.SECRET_ECHO", SECRET)
                .with_effect("unknown")
                .with_detail("value", json!(SECRET)))
        } else {
            // Session must not trust even a successful provider to redact.
            Ok(json!({"submitted":true,"value":SECRET,"length":SECRET.len()}))
        }
    }
}

fn setup(grant: bool, fail: bool, protected: bool) -> (Session, Arc<PasswordBackend>, Target) {
    let backend = Arc::new(PasswordBackend {
        writes: AtomicUsize::new(0),
        fail,
        protected,
    });
    let session = Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: true,
            allow_secret_input: grant,
            secret_bindings: SecretBindings::new([("login".into(), SECRET.into())]).unwrap(),
            ..Default::default()
        },
    );
    assert!(call(&session, Operation::Targets {}).error.is_none());
    let observed = call(
        &session,
        Operation::Observe {
            resource_id: "password-field".into(),
            max_nodes: 10,
            view: View::default(),
        },
    );
    let observed = observed.data.unwrap();
    if protected {
        assert!(observed["nodes"][0]["value"].is_null());
    }
    let target = serde_json::from_value(observed["nodes"][0]["target"].clone()).unwrap();
    (session, backend, target)
}
fn call(session: &Session, operation: Operation) -> Response {
    session.execute(&Request::new(Some(session.id().into()), operation))
}
fn act(session: &Session, target: Target, action: Action) -> Response {
    call(
        session,
        Operation::Act {
            target,
            action,
            verify: None,
        },
    )
}

#[test]
fn bound_secret_uses_the_dedicated_dispatch_and_redacts_success_and_failure() {
    for fail in [false, true] {
        let (session, backend, target) = setup(true, fail, true);
        let response = act(
            &session,
            target,
            Action::SecretInput {
                secret_ref: "login".into(),
            },
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
        let public = serde_json::to_string(&response).unwrap();
        assert!(!public.contains(SECRET) && !public.contains("length"));
        if fail {
            assert_eq!(response.effect, "unknown");
            assert_eq!(
                response.error.unwrap()["code"],
                "CONTROL.SECRET_INPUT_FAILED"
            );
        } else {
            assert_eq!(response.effect, "applied");
            assert_eq!(
                response.data.unwrap()["backend_result"],
                json!({"submitted":true,"redacted":true})
            );
        }
        let capabilities =
            serde_json::to_string(&call(&session, Operation::Capabilities {})).unwrap();
        assert!(!capabilities.contains(SECRET));
    }
}

#[test]
fn secret_input_needs_a_grant_a_known_binding_and_a_protected_observed_target() {
    for (grant, protected, name, expected) in [
        (false, true, "login", "POLICY.DENIED"),
        (true, false, "login", "CONTROL.ACTION_UNSUPPORTED"),
        (true, true, "unknown", "CONTROL.SECRET_BINDING_MISSING"),
        (true, true, "../login", "CONTROL.INVALID_REQUEST"),
    ] {
        let (session, backend, target) = setup(grant, false, protected);
        let response = act(
            &session,
            target,
            Action::SecretInput {
                secret_ref: name.into(),
            },
        );
        assert_eq!(response.error.unwrap()["code"], expected);
        assert_eq!(response.effect, "not_applied");
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn ordinary_text_and_key_actions_cannot_bypass_the_secret_grant() {
    let (session, backend, target) = setup(true, false, true);
    for action in [
        Action::Fill {
            value: "public".into(),
        },
        Action::TypeText {
            text: "public".into(),
            mode: None,
        },
        Action::Paste {
            text: "public".into(),
        },
        Action::Key {
            hold_ms: None,
            keys: vec!["ctrl".into(), "v".into()],
        },
    ] {
        let response = act(&session, target.clone(), action);
        assert_eq!(response.error.unwrap()["code"], "POLICY.DENIED");
        assert_eq!(response.effect, "not_applied");
    }
    assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
}
