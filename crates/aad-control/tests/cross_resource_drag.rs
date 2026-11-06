//! Admission only: native providers must opt in and implement their own fresh
//! endpoint checks before any real cross-resource drag becomes available.
use aad_control::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

struct Fake {
    name: String,
    enabled: bool,
    leases: bool,
    serial: AtomicUsize,
    stale: AtomicBool,
    overrides: Mutex<BTreeMap<String, Value>>,
    calls: Mutex<Vec<Value>>,
    lease_prefix: String,
}

impl Fake {
    fn new(name: &str, enabled: bool, leases: bool) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            enabled,
            leases,
            serial: AtomicUsize::new(0),
            stale: AtomicBool::new(false),
            overrides: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            lease_prefix: uuid::Uuid::new_v4().to_string(),
        })
    }
    fn resource(&self, index: usize) -> String {
        format!("{}:{index}", self.name)
    }
    fn key(&self, resource: &str) -> String {
        format!("{}:{resource}", self.lease_prefix)
    }
}

impl Backend for Fake {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["drag".into()]
    }
    fn supports_cross_resource_drag(&self) -> bool {
        self.enabled
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok((0..20)
            .map(|index| BackendResource {
                resource: Resource {
                    resource_id: self.resource(index),
                    provider: self.name.clone(),
                    kind: "desktop".into(),
                    name: format!("window {index}"),
                    capabilities: self.capabilities(),
                    metadata: json!({}),
                },
                selector: json!({"resource": self.resource(index)}),
            })
            .collect())
    }
    fn observe(&self, selector: &Value, _: usize, _: Duration) -> Result<Value> {
        let resource = selector["resource"].as_str().unwrap();
        let mut snapshot = json!({
            "resource": resource,
            "snapshot": self.serial.fetch_add(1, Ordering::SeqCst),
            "revision": "original-revision",
            "scope_id": format!("scope-{resource}"),
            "truncated": false,
            "nodes": [{"node_id": "surface", "name": "surface", "actions": ["drag"]}]
        });
        if let Some(overrides) = self.overrides.lock().unwrap().get(resource) {
            for (key, value) in overrides.as_object().unwrap() {
                snapshot[key] = value.clone();
            }
        }
        Ok(snapshot)
    }
    fn lease_keys(&self, snapshot: &Value) -> Result<Vec<String>> {
        Ok(if self.leases {
            // Shared key occurs twice in the union and must be deduplicated.
            vec![
                self.key("shared"),
                self.key(snapshot["resource"].as_str().unwrap()),
            ]
        } else {
            Vec::new()
        })
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, context: &Context) -> Result<Value> {
        context.commit()?;
        self.calls.lock().unwrap().push(json!({"ordinary": true}));
        Ok(json!({"ok": true}))
    }
    fn drag_between(
        &self,
        source: DragEndpoint<'_>,
        destination: DragEndpoint<'_>,
        action: &Action,
        context: &Context,
    ) -> Result<Value> {
        if self.stale.load(Ordering::SeqCst) {
            return Err(error(
                "DRIVER.STALE_TARGET",
                "fresh destination identity changed",
            ));
        }
        if self.leases {
            for snapshot in [source.snapshot, destination.snapshot] {
                for key in self.lease_keys(snapshot)? {
                    assert!(
                        lease::InputLease::acquire(&key).is_err(),
                        "lease missing: {key}"
                    );
                }
            }
        }
        context.commit()?;
        self.calls.lock().unwrap().push(json!({
            "source": source.snapshot, "source_node": source.node,
            "destination": destination.snapshot, "destination_node": destination.node,
            "action": action
        }));
        Ok(json!({"ok": true}))
    }
}

fn fixture(enabled: bool, leases: bool) -> (Session, Arc<Fake>) {
    let backend = Fake::new("fixture", enabled, leases);
    let session = Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    assert!(call(&session, Operation::Targets {}).error.is_none());
    (session, backend)
}

fn call(session: &Session, operation: Operation) -> Response {
    session.execute(&Request::new(Some(session.id().into()), operation))
}

fn observe(session: &Session, resource: &str) -> Target {
    let response = call(
        session,
        Operation::Observe {
            resource_id: resource.into(),
            max_nodes: 100,
            view: View::default(),
        },
    );
    assert!(response.error.is_none(), "{:?}", response.error);
    serde_json::from_value(response.data.unwrap()["nodes"][0]["target"].clone()).unwrap()
}

fn drag(session: &Session, source: &Target, destination: &Target) -> Response {
    call(
        session,
        serde_json::from_value(json!({
            "method": "act", "target": source,
            "action": {"type": "drag", "to": destination,
                "fraction": {"fx": 0.1, "fy": 0.2},
                "to_fraction": {"fx": 0.8, "fy": 0.9}}
        }))
        .unwrap(),
    )
}

fn assert_code(response: Response, expected: &str) {
    assert_eq!(
        response.error.as_ref().map(|e| e["code"].as_str().unwrap()),
        Some(expected),
        "{response:?}"
    );
}

fn find(session: &Session, target: &Target) -> Response {
    call(
        session,
        serde_json::from_value(json!({
            "method": "find", "observation_id": target.observation_id,
            "locator": {"name": "surface"}
        }))
        .unwrap(),
    )
}

#[test]
fn opted_in_drag_receives_both_original_snapshots_and_leases_then_expires_provider() {
    let (session, backend) = fixture(true, true);
    let source = observe(&session, "fixture:0");
    let destination = observe(&session, "fixture:1");
    let other = observe(&session, "fixture:2");
    assert!(drag(&session, &source, &destination).error.is_none());
    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["source"]["snapshot"], 0);
    assert_eq!(calls[0]["destination"]["snapshot"], 1);
    assert_eq!(calls[0]["source"]["scope_id"], "scope-fixture:0");
    assert_eq!(calls[0]["destination"]["scope_id"], "scope-fixture:1");
    assert_eq!(calls[0]["action"]["fraction"]["fx"], 0.1);
    assert_eq!(calls[0]["action"]["to_fraction"]["fx"], 0.8);
    for target in [&source, &destination, &other] {
        assert_code(find(&session, target), "CONTROL.STALE_TARGET");
    }
    // Leases belong to the session, as they do for ordinary input.
    assert!(lease::InputLease::acquire(&backend.key("fixture:0")).is_err());
    session.close();
    assert!(lease::InputLease::acquire(&backend.key("fixture:0")).is_ok());
    assert!(lease::InputLease::acquire(&backend.key("fixture:1")).is_ok());
}

#[test]
fn production_default_keeps_old_retention_and_does_not_route_cross_resource_drag() {
    let (session, backend) = fixture(false, false);
    let source = observe(&session, "fixture:0");
    let mut forged = source.clone();
    forged.resource_id = "fixture:1".into();
    assert_code(drag(&session, &source, &forged), "CONTROL.INVALID_REQUEST");
    let _destination = observe(&session, "fixture:1");
    assert_code(find(&session, &source), "CONTROL.STALE_TARGET");
    assert!(backend.calls.lock().unwrap().is_empty());
    let capabilities = call(&session, Operation::Capabilities {}).data.unwrap();
    assert_eq!(
        capabilities["observation_retention"],
        "until_next_observation_or_input_attempt"
    );
    assert_eq!(capabilities["providers"][0]["cross_resource_drag"], false);
    assert_eq!(capabilities["providers"][0]["max_retained_observations"], 1);
}

#[test]
fn opt_in_retention_is_latest_per_resource_and_bounded() {
    let (session, _) = fixture(true, false);
    let first = observe(&session, "fixture:0");
    let replacement = observe(&session, "fixture:0");
    assert_code(find(&session, &first), "CONTROL.STALE_TARGET");
    assert!(find(&session, &replacement).error.is_none());
    let mut retained = vec![replacement.clone()];
    for index in 1..17 {
        retained.push(observe(&session, &format!("fixture:{index}")));
    }
    assert_code(find(&session, &replacement), "CONTROL.STALE_TARGET");
    for target in &retained[1..] {
        assert!(find(&session, target).error.is_none());
    }
    let capabilities = call(&session, Operation::Capabilities {}).data.unwrap();
    assert_eq!(capabilities["providers"][0]["cross_resource_drag"], true);
    assert_eq!(
        capabilities["providers"][0]["max_retained_observations"],
        16
    );
}

#[test]
fn destination_is_checked_for_completeness_scope_membership_and_capability() {
    for (resource, overrides, expected) in [
        (
            "fixture:0",
            json!({"truncated": true}),
            "CONTROL.OBSERVATION_TRUNCATED",
        ),
        (
            "fixture:1",
            json!({"truncated": true}),
            "CONTROL.OBSERVATION_TRUNCATED",
        ),
        (
            "fixture:1",
            json!({"nodes": [{"node_id": "surface", "actions": []}]}),
            "CONTROL.ACTION_UNSUPPORTED",
        ),
        (
            "fixture:1",
            json!({"nodes": [{"node_id": "surface", "actions": ["drag"], "provenance": {"scope_context": true}}]}),
            "CONTROL.STALE_TARGET",
        ),
        (
            "fixture:0",
            json!({"nodes": [{"node_id": "surface", "actions": ["drag"], "provenance": {"scope_context": true}}]}),
            "CONTROL.STALE_TARGET",
        ),
    ] {
        let (session, backend) = fixture(true, false);
        backend
            .overrides
            .lock()
            .unwrap()
            .insert(resource.into(), overrides);
        let source = observe(&session, "fixture:0");
        let destination = observe(&session, "fixture:1");
        assert_code(drag(&session, &source, &destination), expected);
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn destination_identity_grant_and_session_cannot_be_substituted() {
    let (session, backend) = fixture(true, false);
    let source = observe(&session, "fixture:0");
    let destination = observe(&session, "fixture:1");
    let mut changed = destination.clone();
    changed.resource_id = "fixture:denied".into();
    assert_code(drag(&session, &source, &changed), "POLICY.DENIED");
    changed = destination.clone();
    changed.session_id = "another-session".into();
    assert_code(
        drag(&session, &source, &changed),
        "CONTROL.SESSION_MISMATCH",
    );
    changed = destination.clone();
    changed.node_id = "unobserved".into();
    assert_code(drag(&session, &source, &changed), "CONTROL.STALE_TARGET");
    changed = destination;
    changed.observation_id = "expired".into();
    assert_code(drag(&session, &source, &changed), "CONTROL.STALE_TARGET");
    assert!(backend.calls.lock().unwrap().is_empty());
}

#[test]
fn opt_in_does_not_enable_different_providers_or_cross_surface_paths() {
    let first = Fake::new("first", true, false);
    let second = Fake::new("second", true, false);
    let session = Session::new(
        vec![first.clone(), second.clone()],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    assert!(call(&session, Operation::Targets {}).error.is_none());
    let source = observe(&session, "first:0");
    let foreign = observe(&session, "second:0");
    assert_code(
        drag(&session, &source, &foreign),
        "CONTROL.ACTION_UNSUPPORTED",
    );
    let destination = observe(&session, "first:1");
    let response = call(
        &session,
        serde_json::from_value(json!({
            "method": "act", "target": source,
            "action": {"type": "drag", "to": destination,
                "fraction": {"fx": 0.1, "fy": 0.2}, "to_fraction": {"fx": 0.8, "fy": 0.9},
                "path": [{"target": source, "fraction": {"fx": 0.5, "fy": 0.5}}]}
        }))
        .unwrap(),
    );
    assert_code(response, "CONTROL.INVALID_REQUEST");
    assert!(first.calls.lock().unwrap().is_empty());
    assert!(second.calls.lock().unwrap().is_empty());
}

#[test]
fn failed_backend_fresh_check_still_invalidates_both_observations() {
    let (session, backend) = fixture(true, false);
    let source = observe(&session, "fixture:0");
    let destination = observe(&session, "fixture:1");
    backend.stale.store(true, Ordering::SeqCst);
    assert_code(drag(&session, &source, &destination), "DRIVER.STALE_TARGET");
    assert_code(find(&session, &source), "CONTROL.STALE_TARGET");
    assert_code(find(&session, &destination), "CONTROL.STALE_TARGET");
    assert!(backend.calls.lock().unwrap().is_empty());
}

#[test]
fn busy_destination_lease_rejects_before_dispatch_and_rolls_back_source_lease() {
    let (session, backend) = fixture(true, true);
    let source = observe(&session, "fixture:0");
    let destination = observe(&session, "fixture:1");
    let busy = lease::InputLease::acquire(&backend.key("fixture:1")).unwrap();
    assert_code(
        drag(&session, &source, &destination),
        "CONTROL.RESOURCE_BUSY",
    );
    assert!(lease::InputLease::acquire(&backend.key("fixture:0")).is_ok());
    assert!(backend.calls.lock().unwrap().is_empty());
    drop(busy);
    assert!(drag(&session, &source, &destination).error.is_none());
}

#[test]
fn retained_resource_observations_still_expire_at_the_session_ttl() {
    let (session, _) = fixture(true, false);
    let session = session
        .with_limits(Duration::from_secs(60), Duration::from_millis(5), 100)
        .unwrap();
    let source = observe(&session, "fixture:0");
    std::thread::sleep(Duration::from_millis(10));
    let destination = observe(&session, "fixture:1");
    assert_code(
        drag(&session, &source, &destination),
        "CONTROL.STALE_TARGET",
    );
}
