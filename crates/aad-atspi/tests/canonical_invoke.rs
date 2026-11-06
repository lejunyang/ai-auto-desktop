use aad_atspi::{
    AtspiBackend, AtspiProvider, BackendNode, BackendSnapshot, NativeRef, WindowAnchor,
};
use aad_core::AutomationError;
use aad_runtime::Provider;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Default)]
struct CanonicalBackend {
    captures: AtomicUsize,
}

impl AtspiBackend for CanonicalBackend {
    fn name(&self) -> &str {
        "canonical fixture"
    }
    fn session_info(&self) -> Map<String, Value> {
        Map::new()
    }
    fn list_applications(&self, _: Instant) -> Result<Vec<Map<String, Value>>, AutomationError> {
        unreachable!()
    }
    fn capture(
        &self,
        _: &Map<String, Value>,
        _: u32,
        _: usize,
        _: Instant,
    ) -> Result<BackendSnapshot, AutomationError> {
        let fresh = self.captures.fetch_add(1, Ordering::SeqCst) > 0;
        let application =
            json!({"bus_name":":1.42","object_path":"/app","process_id":42,"name":"Fixture"});
        Ok(BackendSnapshot {
            application: application.as_object().unwrap().clone(),
            nodes: vec![BackendNode {
                native: NativeRef {
                    bus_name: ":1.42".into(),
                    object_path: "/button".into(),
                },
                parent_index: None,
                role: "push_button".into(),
                name: Some("Close".into()),
                description: None,
                value: None,
                attributes: BTreeMap::new(),
                states: BTreeMap::from_iter([("protected".into(), Some(false))]),
                bounds: None,
                actions: vec!["invoke".into()],
                provenance: json!({"bus_name":":1.42","object_path":"/button",
                    "process_id":42,"native_action_name":"click",
                    "native_invoke_name":if fresh {"Click"} else {"press"}})
                .as_object()
                .unwrap()
                .clone(),
            }],
            truncated: false,
            truncation_reasons: vec![],
        })
    }
    fn focus(
        &self,
        _: &NativeRef,
        _: Option<&WindowAnchor>,
        _: Instant,
    ) -> Result<Value, AutomationError> {
        unreachable!()
    }
    fn invoke(&self, _: &NativeRef, _: Instant) -> Result<Value, AutomationError> {
        panic!("fresh canonical action must use the checked named dispatch")
    }
    fn set_text(&self, _: &NativeRef, _: &str, _: Instant) -> Result<Value, AutomationError> {
        unreachable!()
    }
    fn named_action(
        &self,
        _: &NativeRef,
        name: &str,
        _: Instant,
    ) -> Result<Value, AutomationError> {
        assert_eq!(
            name, "Click",
            "the fresh action, not the older observed action, must be bound"
        );
        Ok(json!({"canonical":name}))
    }
}

#[test]
fn provider_binds_the_fresh_canonical_name_into_final_native_dispatch() {
    let provider = AtspiProvider::new(Arc::new(CanonicalBackend::default())).unwrap();
    let observed = provider
        .invoke(
            "desktop.linux_atspi.snapshot@1",
            json!({"application":{"process_id":42}}),
            Some(Duration::from_secs(2)),
        )
        .unwrap();
    let result = provider.invoke("desktop.linux_atspi.invoke@1", json!({
        "target":{"snapshot_id":observed["snapshot_id"],"revision":observed["revision"],"node_id":"n0"},
        "locator":{"name":"Close"}
    }), Some(Duration::from_secs(2))).unwrap();
    assert_eq!(result["backend_result"]["canonical"], "Click");
}
