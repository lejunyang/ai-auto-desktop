#![cfg(target_os = "linux")]
use aad_control::{
    native::{Flavor, NativeBackend},
    *,
};
use aad_linux::atspi::{AtspiBackend, AtspiProvider, BackendNode, BackendSnapshot, NativeRef};
use serde_json::{json, Map, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

#[derive(Default)]
struct Tree {
    distinct: bool,
    after_first: Option<usize>,
    legacy_x11_helper: bool,
    incompatible_handshake: bool,
    two_apps: bool,
    context_sibling: bool,
    captures: AtomicUsize,
}
impl Tree {
    fn app() -> Map<String, Value> {
        json!({"bus_name":":1.42","name":"Fixture","process_id":4242})
            .as_object()
            .unwrap()
            .clone()
    }

    fn other_app() -> Map<String, Value> {
        json!({"bus_name":":1.43","name":"Other Fixture","process_id":4243})
            .as_object()
            .unwrap()
            .clone()
    }
}
impl AtspiBackend for Tree {
    fn name(&self) -> &str {
        "fixture"
    }
    fn session_info(&self) -> Map<String, Value> {
        if self.incompatible_handshake {
            Map::from_iter([(
                "input_helper".into(),
                json!({"ready":false,"code":"DRIVER.HELPER_INCOMPATIBLE","message":"fixture rejected describe"}),
            )])
        } else if self.legacy_x11_helper {
            Map::from_iter([(
                "input_helper".into(),
                json!({"ready":true,"protocol":1,"features":["type-text","window-focus","anchored-drag-between"]}),
            )])
        } else {
            Map::new()
        }
    }
    fn list_applications(&self, _: Instant) -> Result<Vec<Map<String, Value>>> {
        let mut applications = vec![Self::app()];
        if self.two_apps {
            applications.push(Self::other_app());
        }
        Ok(applications)
    }
    fn retains_resource_observations(&self) -> bool {
        self.two_apps
    }
    fn capture(
        &self,
        application: &Map<String, Value>,
        _: u32,
        _: usize,
        _: Instant,
    ) -> Result<BackendSnapshot> {
        let capture = self.captures.fetch_add(1, Ordering::SeqCst);
        let count = if capture > 0 {
            self.after_first.unwrap_or(2)
        } else {
            2
        };
        let bus_name = application["bus_name"].as_str().unwrap();
        let nodes = (0..count)
            .map(|i| {
                let path = if self.distinct && i == 1 {
                    "/other"
                } else {
                    "/allow"
                };
                BackendNode {
                    native: NativeRef {
                        bus_name: bus_name.into(),
                        object_path: path.into(),
                    },
                    parent_index: None,
                    role: "push_button".into(),
                    name: Some("允许".into()),
                    description: None,
                    value: None,
                    attributes: Default::default(),
                    states: Default::default(),
                    bounds: None,
                    actions: vec![],
                    provenance: json!({"bus_name":bus_name,"object_path":path,
                        "focus_context": self.context_sibling && i == 1})
                    .as_object()
                    .unwrap()
                    .clone(),
                }
            })
            .collect();
        Ok(BackendSnapshot {
            application: application.clone(),
            nodes,
            truncated: false,
            truncation_reasons: vec![],
            qt_x11_windows: None,
        })
    }
    fn focus(
        &self,
        _: &NativeRef,
        _: Option<&aad_linux::atspi::WindowAnchor>,
        _: Instant,
    ) -> Result<Value> {
        unreachable!()
    }
    fn invoke(&self, _: &NativeRef, _: Instant) -> Result<Value> {
        unreachable!()
    }
    fn set_text(&self, _: &NativeRef, _: &str, _: Instant) -> Result<Value> {
        unreachable!()
    }
    fn named_action(&self, _: &NativeRef, _: &str, _: Instant) -> Result<Value> {
        unreachable!()
    }
}

#[test]
fn session_find_uses_native_identity_and_preserves_optional_and_node_id() {
    for distinct in [false, true] {
        let provider = Arc::new(
            AtspiProvider::new(Arc::new(Tree {
                distinct,
                ..Default::default()
            }))
            .unwrap(),
        );
        let session = Session::new(
            vec![Arc::new(NativeBackend::new(provider, Flavor::Linux))],
            Policy::default(),
        );
        let call = |op| session.execute(&Request::new(Some(session.id().into()), op));
        let targets = call(Operation::Targets { provider: None }).data.unwrap();
        let resource = targets["resources"][0]["resource_id"].as_str().unwrap();
        let observation = call(Operation::Observe {
            resource_id: resource.into(),
            max_nodes: 10,
            view: Default::default(),
        })
        .data
        .unwrap();
        let find = |locator, expect| {
            call(Operation::Find {
                observation_id: observation["observation_id"].as_str().unwrap().into(),
                locator: Box::new(locator),
                expect,
            })
        };
        let named = || Locator {
            name: Some("允许".into()),
            ..Default::default()
        };
        for expect in [Expect::One, Expect::Optional] {
            let result = find(named(), expect);
            if distinct {
                assert_eq!(result.error.unwrap()["code"], "DRIVER.AMBIGUOUS");
            } else {
                assert_eq!(result.data.unwrap()["match_count"], 1);
            }
        }
        let result = find(
            Locator {
                node_id: Some("n1".into()),
                ..named()
            },
            Expect::One,
        );
        assert_eq!(result.data.unwrap()["target"]["node_id"], "n1");
        let missing = || Locator {
            name: Some("missing".into()),
            ..Default::default()
        };
        assert_eq!(
            find(missing(), Expect::Optional).data.unwrap()["found"],
            false
        );
        assert_eq!(
            find(missing(), Expect::One).error.unwrap()["code"],
            "DRIVER.NOT_FOUND"
        );
    }
}

#[test]
fn session_wait_reobserves_native_ambiguity_until_unique_or_absent() {
    for (count, condition) in [(1, "appears"), (0, "disappears")] {
        let tree = Arc::new(Tree {
            distinct: true,
            after_first: Some(count),
            ..Default::default()
        });
        let provider = Arc::new(AtspiProvider::new(tree.clone()).unwrap());
        let session = Session::new(
            vec![Arc::new(NativeBackend::new(provider, Flavor::Linux))],
            Policy::default(),
        );
        let targets = session
            .execute(&Request::new(
                Some(session.id().into()),
                Operation::Targets { provider: None },
            ))
            .data
            .unwrap();
        let op = serde_json::from_value(json!({
            "method":"wait", "resource_id":targets["resources"][0]["resource_id"],
            "locator":{"name":"允许"}, "condition":condition
        }))
        .unwrap();
        let result = session.execute(&Request::new(Some(session.id().into()), op));
        assert!(result.error.is_none(), "{result:?}");
        assert!(tree.captures.load(Ordering::SeqCst) >= 2);
    }
}

#[test]
fn explicit_x11_actions_report_missing_geometry_feature_before_capability_rejection() {
    assert_x11_helper_rejection(false);
}

#[test]
fn explicit_x11_actions_report_incompatible_handshake_before_capability_rejection() {
    assert_x11_helper_rejection(true);
}

fn assert_x11_helper_rejection(incompatible_handshake: bool) {
    let tree = Arc::new(Tree {
        distinct: true,
        legacy_x11_helper: true,
        incompatible_handshake,
        context_sibling: true,
        ..Default::default()
    });
    let provider = Arc::new(AtspiProvider::new(tree.clone()).unwrap());
    let session = Session::new(
        vec![Arc::new(NativeBackend::new(provider, Flavor::Linux))],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    let call = |operation| session.execute(&Request::new(Some(session.id().into()), operation));
    let targets = call(Operation::Targets { provider: None }).data.unwrap();
    let observed = call(Operation::Observe {
        resource_id: targets["resources"][0]["resource_id"]
            .as_str()
            .unwrap()
            .into(),
        max_nodes: 10,
        view: Default::default(),
    })
    .data
    .unwrap();
    let node = &observed["nodes"][0];
    assert!(!node["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "type_text"));
    for (action, code) in [
        (
            json!({"type":"type_text","text":"seed"}),
            "DRIVER.HELPER_INCOMPATIBLE",
        ),
        (json!({"type":"close_window"}), "DRIVER.HELPER_INCOMPATIBLE"),
        (
            json!({"type":"drag","to":node["target"]}),
            "DRIVER.HELPER_INCOMPATIBLE",
        ),
        (
            json!({"type":"click"}),
            if incompatible_handshake {
                "DRIVER.HELPER_INCOMPATIBLE"
            } else {
                "CONTROL.ACTION_UNSUPPORTED"
            },
        ),
        (
            json!({"type":"fill","value":"seed"}),
            "CONTROL.ACTION_UNSUPPORTED",
        ),
    ] {
        let op =
            serde_json::from_value(json!({"method":"act","target":node["target"],"action":action}))
                .unwrap();
        let response = call(op);
        assert_eq!(response.effect, "not_applied");
        let failure = response.error.unwrap();
        assert_eq!(failure["code"], code);
        if incompatible_handshake && code == "DRIVER.HELPER_INCOMPATIBLE" {
            assert_eq!(failure["message"], "fixture rejected describe");
        }
    }
    let context = &observed["nodes"][1];
    assert_eq!(context["provenance"]["scope_context"], true);
    for (target, action) in [
        (&context["target"], json!({"type":"close_window"})),
        (
            &node["target"],
            json!({"type":"drag","to":context["target"],"modifiers":["shift"]}),
        ),
    ] {
        let op = serde_json::from_value(json!({"method":"act","target":target,"action":action}))
            .unwrap();
        let response = call(op);
        assert_eq!(response.effect, "not_applied");
        assert_eq!(response.error.unwrap()["code"], "CONTROL.STALE_TARGET");
    }
    assert_eq!(tree.captures.load(Ordering::SeqCst), 1);
}

#[test]
fn legacy_x11_helper_retains_both_drag_observations_for_authorization() {
    for allow_input in [true, false] {
        let tree = Arc::new(Tree {
            distinct: true,
            legacy_x11_helper: true,
            two_apps: true,
            context_sibling: true,
            ..Default::default()
        });
        let provider = Arc::new(AtspiProvider::new(tree.clone()).unwrap());
        let session = Session::new(
            vec![Arc::new(NativeBackend::new(provider, Flavor::Linux))],
            Policy {
                allow_input,
                ..Default::default()
            },
        );
        let call = |op| session.execute(&Request::new(Some(session.id().into()), op));
        let caps = call(Operation::Capabilities {}).data.unwrap();
        assert_eq!(caps["providers"][0]["cross_resource_drag"], false);
        assert_eq!(caps["providers"][0]["max_retained_observations"], 16);
        let targets = call(Operation::Targets { provider: None }).data.unwrap();
        let observe = |index: usize| {
            call(Operation::Observe {
                resource_id: targets["resources"][index]["resource_id"]
                    .as_str()
                    .unwrap()
                    .into(),
                max_nodes: 10,
                view: Default::default(),
            })
            .data
            .unwrap()
        };
        let source = observe(0);
        let destination = observe(1);
        let drag = |source: &Value, destination: &Value, code: &str| {
            let op = serde_json::from_value(json!({
                "method":"act", "target":source["target"],
                "action":{"type":"drag","to":destination["target"]}
            }))
            .unwrap();
            let response = call(op);
            assert_eq!(response.effect, "not_applied");
            assert_eq!(response.error.unwrap()["code"], code);
        };
        if allow_input {
            drag(
                &source["nodes"][0],
                &destination["nodes"][0],
                "DRIVER.HELPER_INCOMPATIBLE",
            );
            drag(
                &destination["nodes"][0],
                &source["nodes"][0],
                "DRIVER.HELPER_INCOMPATIBLE",
            );
            drag(
                &source["nodes"][0],
                &destination["nodes"][1],
                "CONTROL.STALE_TARGET",
            );
            drag(
                &source["nodes"][1],
                &destination["nodes"][0],
                "CONTROL.STALE_TARGET",
            );
        } else {
            drag(
                &source["nodes"][0],
                &destination["nodes"][0],
                "POLICY.DENIED",
            );
        }
        assert_eq!(tree.captures.load(Ordering::SeqCst), 2);
    }
}
