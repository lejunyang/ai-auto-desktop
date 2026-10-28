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

#[test]
fn windows_gestures_views_and_rich_locators_route_to_the_driver() {
    struct Windows {
        manifest: CapabilityManifest,
        calls: Mutex<Vec<(String, Value)>>,
    }
    impl Provider for Windows {
        fn manifest(&self) -> &CapabilityManifest {
            &self.manifest
        }
        fn invoke(&self, action: &str, args: Value, _: Option<Duration>) -> Result<Value> {
            self.calls.lock().unwrap().push((action.into(), args));
            Ok(match action.rsplit('.').next().unwrap_or_default() {
                "describe@1" => {
                    json!({"elements":[{"node_id":"n","ref":"s:1:n","summary":"Button Save"}],"shown":1,"matched":1,"truncated":false})
                }
                "overview@1" => json!({"regions":[{"name":"toolbar","elements":4}]}),
                "find@1" => json!({"found":true,"target":{"node_id":"n"},"match_count":1}),
                _ => json!({"ok":true}),
            })
        }
    }
    let mut actions = Map::new();
    for action in ["snapshot", "pointer_click", "pointer_gesture", "drag"] {
        actions.insert(action.into(), json!({"contract_major":1}));
    }
    let provider = Arc::new(Windows {
        manifest: manifest::parse(&manifest::document("desktop.windows_uia", actions)).unwrap(),
        calls: Mutex::new(Vec::new()),
    });
    let backend = NativeBackend::new(provider.clone(), Flavor::Windows);
    assert_eq!(
        backend.capabilities(),
        vec!["click", "scroll", "hover", "drag"]
    );
    let node = json!({"node_id":"n","actions":["pointer_click","pointer_gesture","drag"]});
    assert_eq!(
        backend.node_capabilities(&node),
        vec!["click", "scroll", "hover", "drag"]
    );
    let snapshot = json!({"snapshot_id":"s","revision":1});
    let context = || Context::new(Duration::from_secs(1), true);
    let last = || provider.calls.lock().unwrap().last().cloned().unwrap();

    backend
        .act(&snapshot, &node, &Action::click(), &context())
        .unwrap();
    assert_eq!(last().0, "desktop.windows_uia.pointer_click@1");
    let right = Action::Click {
        button: Some(Button::Right),
        count: None,
        fraction: Some(Fraction { fx: 0.25, fy: 0.5 }),
    };
    backend.act(&snapshot, &node, &right, &context()).unwrap();
    let (called, args) = last();
    assert_eq!(called, "desktop.windows_uia.pointer_gesture@1");
    assert_eq!(args["button"], "right");
    assert_eq!(args["fraction"], json!({"fx":0.25,"fy":0.5}));
    assert!(args.get("count").is_none());
    let scroll = Action::Scroll {
        direction: Direction::Down,
        notches: Some(5),
        fraction: None,
    };
    backend.act(&snapshot, &node, &scroll, &context()).unwrap();
    assert_eq!(last().1["scroll"], json!({"direction":"down","notches":5}));
    let drag = Action::Drag {
        to: Target {
            session_id: "x".into(),
            resource_id: "r".into(),
            observation_id: "o".into(),
            node_id: "m".into(),
        },
        button: None,
        fraction: None,
        to_fraction: None,
    };
    backend.act(&snapshot, &node, &drag, &context()).unwrap();
    let (called, args) = last();
    assert_eq!(called, "desktop.windows_uia.drag@1");
    assert_eq!(
        args["to"],
        json!({"snapshot_id":"s","revision":1,"node_id":"m"})
    );

    let outline = backend
        .view(
            &snapshot,
            &View {
                mode: ViewMode::Outline,
                region: Some("toolbar".into()),
                ..Default::default()
            },
            Duration::from_secs(1),
        )
        .unwrap()
        .unwrap();
    let (called, args) = last();
    assert_eq!(called, "desktop.windows_uia.describe@1");
    assert_eq!(args["snapshot_id"], "s");
    assert_eq!(args["region"], "toolbar");
    assert_eq!(outline["nodes"][0]["summary"], "Button Save");
    assert!(outline["nodes"][0].get("ref").is_none());
    let overview = backend
        .view(
            &snapshot,
            &View {
                mode: ViewMode::Overview,
                ..Default::default()
            },
            Duration::from_secs(1),
        )
        .unwrap()
        .unwrap();
    assert_eq!(overview["regions"][0]["name"], "toolbar");

    let found = backend
        .find(
            &snapshot,
            &Locator {
                automation_id: Some("save".into()),
                ..Default::default()
            },
            Expect::Optional,
            Duration::from_secs(1),
        )
        .unwrap()
        .unwrap();
    let (called, args) = last();
    assert_eq!(called, "desktop.windows_uia.find@1");
    assert_eq!(args["locator"], json!({"automation_id":"save"}));
    assert_eq!(args["expect"], "optional");
    assert_eq!(found["node_id"], "n");

    let linux = NativeBackend::new(
        Fixture::new("desktop.linux_atspi", Flavor::Linux),
        Flavor::Linux,
    );
    assert!(linux
        .view(
            &snapshot,
            &View {
                mode: ViewMode::Outline,
                ..Default::default()
            },
            Duration::from_secs(1)
        )
        .unwrap()
        .is_none());
}
