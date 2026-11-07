#![cfg(target_os = "linux")]
use aad_atspi::{AtspiBackend, AtspiProvider, BackendNode, BackendSnapshot, NativeRef};
use aad_control::{
    native::{Flavor, NativeBackend},
    *,
};
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
    captures: AtomicUsize,
}
impl Tree {
    fn app() -> Map<String, Value> {
        json!({"bus_name":":1.42","name":"Fixture","process_id":4242})
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
        Map::new()
    }
    fn list_applications(&self, _: Instant) -> Result<Vec<Map<String, Value>>> {
        Ok(vec![Self::app()])
    }
    fn capture(
        &self,
        _: &Map<String, Value>,
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
        let nodes = (0..count)
            .map(|i| {
                let path = if self.distinct && i == 1 {
                    "/other"
                } else {
                    "/allow"
                };
                BackendNode {
                    native: NativeRef {
                        bus_name: ":1.42".into(),
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
                    provenance: json!({"bus_name":":1.42","object_path":path})
                        .as_object()
                        .unwrap()
                        .clone(),
                }
            })
            .collect();
        Ok(BackendSnapshot {
            application: Self::app(),
            nodes,
            truncated: false,
            truncation_reasons: vec![],
        })
    }
    fn focus(
        &self,
        _: &NativeRef,
        _: Option<&aad_atspi::WindowAnchor>,
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
        let targets = call(Operation::Targets {}).data.unwrap();
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
                Operation::Targets {},
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
