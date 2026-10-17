use aad_control::{
    native::{Flavor, NativeBackend},
    *,
};
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::Provider;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Fixture {
    manifest: CapabilityManifest,
    calls: Mutex<Vec<(String, Value)>>,
    flavor: Flavor,
}
impl Fixture {
    fn new(name: &str, flavor: Flavor) -> Arc<Self> {
        let mut actions = Map::new();
        for action in [
            "list_windows",
            "list_applications",
            "list_apps",
            "snapshot",
            "focus",
            "invoke",
            "pointer_click",
            "set_value",
            "set_text",
            "type_text",
        ] {
            actions.insert(action.into(), json!({"contract_major":1}));
        }
        if matches!(flavor, Flavor::Windows) {
            actions.insert("key_chord".into(), json!({"contract_major":1}));
        }
        Arc::new(Self {
            manifest: manifest::parse(&manifest::document(name, actions)).unwrap(),
            calls: Mutex::new(Vec::new()),
            flavor,
        })
    }
}
impl Provider for Fixture {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn invoke(&self, action: &str, args: Value, _: Option<Duration>) -> Result<Value> {
        self.calls.lock().unwrap().push((action.into(), args));
        if action.contains("list_") {
            return Ok(match self.flavor {
                Flavor::Windows => json!({"windows":[{"window_id":"hwnd:1","title":"test"}]}),
                Flavor::Linux => json!({"applications":[{"bus_name":":1.2","name":"test"}]}),
                Flavor::MacOS => json!({"apps":[{"process_id":12,"name":"test"}]}),
            });
        }
        Ok(json!({"ok":true}))
    }
}
#[test]
fn platform_specific_selectors_actions_and_capabilities_are_not_conflated() {
    for (name, flavor, selector_field, write, argument) in [
        (
            "desktop.windows_uia",
            Flavor::Windows,
            "window_id",
            "set_value",
            "value",
        ),
        (
            "desktop.linux_atspi",
            Flavor::Linux,
            "application",
            "set_text",
            "text",
        ),
        (
            "desktop.macos_ax",
            Flavor::MacOS,
            "app",
            "set_value",
            "value",
        ),
    ] {
        let provider = Fixture::new(name, flavor);
        let backend = NativeBackend::new(provider.clone(), flavor);
        assert_eq!(
            backend.capabilities().contains(&"key".into()),
            matches!(flavor, Flavor::Windows)
        );
        let resources = backend.resources(Duration::from_secs(1)).unwrap();
        assert!(resources[0].selector.get(selector_field).is_some());
        backend
            .observe(&resources[0].selector, 32, Duration::from_secs(1))
            .unwrap();
        let snapshot = json!({"snapshot_id":"provider-generation:7","revision":7});
        let node = json!({"node_id":"n","role":"text","name":"Entry","actions":[write],"attributes":{"id":"entry"}});
        assert_eq!(backend.node_capabilities(&node), vec!["fill"]);
        backend
            .act(
                &snapshot,
                &node,
                &Action::Fill {
                    value: "text".into(),
                },
                &Context::new(Duration::from_secs(1), true),
            )
            .unwrap();
        let calls = provider.calls.lock().unwrap();
        let (called, args) = calls.last().unwrap();
        assert_eq!(called, &format!("{name}.{write}@1"));
        assert_eq!(args[argument], "text");
        assert_eq!(args["target"]["snapshot_id"], "provider-generation:7");
        assert_eq!(
            args.get("locator").is_some(),
            !matches!(flavor, Flavor::Windows)
        );
    }
}
