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
            "pointer_gesture",
            "drag",
            "set_value",
            "set_text",
            "type_text",
            "secret_input",
        ] {
            actions.insert(action.into(), json!({"contract_major":1}));
        }
        if matches!(flavor, Flavor::Windows) {
            actions.insert("key_chord".into(), json!({"contract_major":1}));
        }
        if matches!(flavor, Flavor::Linux) {
            actions.insert("snapshot_scope".into(), json!({"contract_major":1}));
        }
        Arc::new(Self {
            manifest: manifest::parse(&manifest::document(name, actions)).unwrap(),
            calls: Mutex::new(Vec::new()),
            flavor,
        })
    }
}

#[test]
fn secret_input_uses_only_the_dedicated_hook_and_cannot_enter_workflows() {
    for (name, flavor) in [
        ("desktop.windows_uia", Flavor::Windows),
        ("desktop.linux_atspi", Flavor::Linux),
        ("desktop.macos_ax", Flavor::MacOS),
    ] {
        let provider = Fixture::new(name, flavor);
        let backend = NativeBackend::new(provider.clone(), flavor);
        let node = json!({"node_id":"n", "role":"edit", "states":{"protected":true}, "actions":["secret_input"]});
        let snapshot = json!({"snapshot_id":"s", "revision":1, "nodes":[node]});
        backend
            .secret_input(
                &snapshot,
                &node,
                "synthetic-fixture",
                &Context::new(Duration::from_secs(1), true),
            )
            .unwrap();
        {
            let calls = provider.calls.lock().unwrap();
            assert_eq!(calls[0].0, format!("{name}.secret_input@1"));
            assert_eq!(
                calls[0].1["target"],
                json!({"snapshot_id":"s","revision":1,"node_id":"n"})
            );
            assert_eq!(calls[0].1["text"], "synthetic-fixture");
        }
        assert!(backend
            .act(
                &snapshot,
                &node,
                &Action::SecretInput {
                    secret_ref: "password".into()
                },
                &Context::new(Duration::from_secs(1), true)
            )
            .is_err());
        let workflow_provider = backend.provider();
        assert!(!workflow_provider
            .manifest()
            .actions
            .contains_key("secret_input"));
        assert!(workflow_provider.manifest().raw["actions"]
            .get("secret_input")
            .is_none());
        let uses = format!("{name}.secret_input@1");
        assert_eq!(
            workflow_provider
                .invoke(&uses, json!({"text":"synthetic-fixture"}), None)
                .unwrap_err()
                .code,
            "CONTROL.SECRET_WORKFLOW_DENIED"
        );
        assert_eq!(
            workflow_provider
                .invoke_with_artifacts(
                    &uses,
                    json!({"text":"synthetic-fixture"}),
                    None,
                    &aad_runtime::ArtifactStore::default()
                )
                .unwrap_err()
                .code,
            "CONTROL.SECRET_WORKFLOW_DENIED"
        );
        let mut registry = aad_runtime::ProviderRegistry::new();
        registry.insert(workflow_provider.clone());
        assert!(
            registry.resolve(&uses).is_none(),
            "handwritten workflow action must not resolve"
        );
        // Hosts may register raw providers explicitly; the Session boundary
        // must apply the same filter as NativeBackend::provider().
        let session =
            Session::new(Vec::new(), Policy::default()).with_providers(vec![provider.clone()]);
        let registered = &session.providers()[0];
        assert!(!registered.manifest().actions.contains_key("secret_input"));
        assert_eq!(
            registered
                .invoke(&uses, json!({"text":"synthetic-fixture"}), None)
                .unwrap_err()
                .code,
            "CONTROL.SECRET_WORKFLOW_DENIED"
        );
        assert_eq!(provider.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn native_pointer_modifiers_route_to_gestures_without_losing_target_identity() {
    for (name, flavor) in [
        ("desktop.windows_uia", Flavor::Windows),
        ("desktop.linux_atspi", Flavor::Linux),
        ("desktop.macos_ax", Flavor::MacOS),
    ] {
        let provider = Fixture::new(name, flavor);
        let backend = NativeBackend::new(provider.clone(), flavor);
        let node = json!({"node_id":"n", "role":"canvas", "name":"Surface", "actions":["pointer_click","pointer_gesture","drag"],
            "provenance":{"bus_name":":1.2","object_path":"/org/a11y/surface"}});
        let snapshot = json!({"snapshot_id":"s", "revision":1, "nodes":[node]});
        for kind in ["click", "drag"] {
            let mut value = json!({"type":kind, "modifiers":["ctrl","shift"]});
            if kind == "drag" {
                value["to"] = json!({"session_id":"s", "resource_id":"r", "observation_id":"o", "node_id":"n"});
            }
            let action = serde_json::from_value(value).unwrap();
            let result = backend.act(
                &snapshot,
                &node,
                &action,
                &Context::new(Duration::from_secs(1), true),
            );
            {
                result.unwrap();
                let calls = provider.calls.lock().unwrap();
                let (called, args) = calls.last().unwrap();
                assert_eq!(
                    *called,
                    format!(
                        "{name}.{}@1",
                        if kind == "click" {
                            "pointer_gesture"
                        } else {
                            "drag"
                        }
                    )
                );
                assert_eq!(args["modifiers"], json!(["ctrl", "shift"]));
                assert_eq!(
                    args["target"],
                    json!({"snapshot_id":"s", "revision":1, "node_id":"n"})
                );
                if matches!(flavor, Flavor::Linux) {
                    assert_eq!(args["locator"]["object_path"], "/org/a11y/surface");
                }
            }
        }
    }
}

#[test]
fn native_scope_keeps_the_observed_identity_and_resource_selector() {
    let provider = Fixture::new("desktop.linux_atspi", Flavor::Linux);
    let backend = NativeBackend::new(provider.clone(), Flavor::Linux);
    let snapshot = json!({"snapshot_id":"generation:1","revision":1,"truncated":true});
    let node = json!({"node_id":"n20","role":"pane","name":"Editor",
        "provenance":{"bus_name":":1.2","object_path":"/org/a11y/editor"}});
    backend
        .observe_target_scope(&snapshot, &node, 300, Duration::from_secs(1))
        .unwrap();
    backend
        .observe_scope(
            &json!({"application":{"bus_name":":1.2"}}),
            "opaque-scope",
            300,
            Duration::from_secs(1),
        )
        .unwrap();
    let calls = provider.calls.lock().unwrap();
    assert_eq!(calls[0].0, "desktop.linux_atspi.snapshot_scope@1");
    assert_eq!(
        calls[0].1["target"],
        json!({"snapshot_id":"generation:1","revision":1,"node_id":"n20"})
    );
    assert_eq!(calls[0].1["locator"]["object_path"], "/org/a11y/editor");
    assert_eq!(calls[0].1["max_nodes"], 300);
    assert_eq!(calls[1].1["scope_id"], "opaque-scope");
    assert_eq!(calls[1].1["application"], json!({"bus_name":":1.2"}));
    let unsupported = NativeBackend::new(
        Fixture::new("desktop.macos_ax", Flavor::MacOS),
        Flavor::MacOS,
    );
    assert_eq!(
        unsupported
            .observe_target_scope(&snapshot, &node, 300, Duration::from_secs(1))
            .unwrap_err()
            .code,
        "CONTROL.SCOPE_UNSUPPORTED"
    );
}

#[test]
fn held_input_preserves_duration_and_uses_physical_native_operations() {
    for (name, flavor) in [
        ("desktop.windows_uia", Flavor::Windows),
        ("desktop.linux_atspi", Flavor::Linux),
        ("desktop.macos_ax", Flavor::MacOS),
    ] {
        let provider = Fixture::new(name, flavor);
        let backend = NativeBackend::new(provider.clone(), flavor);
        let node = json!({"node_id":"n", "actions":["pointer_click","pointer_gesture","drag","key_chord"],
            "provenance":{"bus_name":":1.2","object_path":"/org/a11y/surface"}});
        let snapshot = json!({"snapshot_id":"s", "revision":1, "nodes":[node]});
        for (kind, native) in [
            ("click", "pointer_gesture"),
            ("drag", "drag"),
            ("key", "key_chord"),
        ] {
            let mut value = json!({"type":kind,"hold_ms":250});
            if kind == "drag" {
                value["to"] =
                    json!({"session_id":"s","resource_id":"r","observation_id":"o","node_id":"n"});
            } else if kind == "key" {
                value["keys"] = json!(["a"]);
            }
            let action: Action = serde_json::from_value(value).unwrap();
            backend
                .act(
                    &snapshot,
                    &node,
                    &action,
                    &Context::new(Duration::from_secs(1), true),
                )
                .unwrap();
            let calls = provider.calls.lock().unwrap();
            let (called, args) = calls.last().unwrap();
            assert_eq!(called, &format!("{name}.{native}@1"));
            assert_eq!(args["hold_ms"], 250);
            assert_eq!(
                args["target"],
                json!({"snapshot_id":"s","revision":1,"node_id":"n"})
            );
        }
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
fn drag_path_adapters_preserve_each_observed_anchor() {
    for (name, flavor) in [
        ("desktop.windows_uia", Flavor::Windows),
        ("desktop.linux_atspi", Flavor::Linux),
        ("desktop.macos_ax", Flavor::MacOS),
    ] {
        let provider = Fixture::new(name, flavor);
        let backend = NativeBackend::new(provider.clone(), flavor);
        let node = json!({"node_id":"n","role":"pane","name":"surface","actions":["drag"],
            "provenance":{"bus_name":":1.2","object_path":"/surface"}});
        let snapshot = json!({"snapshot_id":"s","revision":1,"nodes":[node]});
        let target = json!({"session_id":"s","resource_id":"r","observation_id":"o","node_id":"n"});
        let action = serde_json::from_value(json!({"type":"drag","to":target,
            "fraction":{"fx":0.1,"fy":0.2},"to_fraction":{"fx":0.9,"fy":0.8},
            "path":[{"target":target,"fraction":{"fx":0.3,"fy":0.7}},
                    {"target":target,"fraction":{"fx":0.6,"fy":0.1}}]}))
        .unwrap();
        backend
            .act(
                &snapshot,
                &node,
                &action,
                &Context::new(Duration::from_secs(1), true),
            )
            .unwrap();
        let calls = provider.calls.lock().unwrap();
        let (called, args) = calls.last().unwrap();
        assert_eq!(called, &format!("{name}.drag@1"));
        assert_eq!(args["path"].as_array().unwrap().len(), 2);
        for (anchor, fraction) in args["path"]
            .as_array()
            .unwrap()
            .iter()
            .zip([json!({"fx":0.3,"fy":0.7}), json!({"fx":0.6,"fy":0.1})])
        {
            assert_eq!(
                anchor["target"],
                json!({"snapshot_id":"s","revision":1,"node_id":"n"})
            );
            assert_eq!(anchor["fraction"], fraction);
            assert_eq!(
                anchor.get("locator").is_some(),
                !matches!(flavor, Flavor::Windows)
            );
            if matches!(flavor, Flavor::Linux) {
                assert_eq!(anchor["locator"]["object_path"], "/surface");
            }
        }
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
        let node = json!({"node_id":"n","role":"text","name":"Entry","actions":[write],"attributes":{"id":"entry"},"provenance":{"bus_name":":1.2","object_path":"/org/a11y/button"}});
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
        if matches!(flavor, Flavor::Linux) {
            assert_eq!(args["locator"]["bus_name"], ":1.2");
            assert_eq!(args["locator"]["object_path"], "/org/a11y/button");
        } else {
            assert!(args["locator"].get("bus_name").is_none());
            assert!(args["locator"].get("object_path").is_none());
        }
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
                "capture@1" => {
                    json!({"format":"png","data":"aGk=","width":2,"height":1,"clipped":false,"node_id":"n"})
                }
                _ => json!({"ok":true}),
            })
        }
    }
    let mut actions = Map::new();
    for action in [
        "snapshot",
        "pointer_click",
        "pointer_gesture",
        "drag",
        "capture",
    ] {
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
        modifiers: vec![PointerModifier::Ctrl, PointerModifier::Shift],
        hold_ms: None,
    };
    backend.act(&snapshot, &node, &right, &context()).unwrap();
    let (called, args) = last();
    assert_eq!(called, "desktop.windows_uia.pointer_gesture@1");
    assert_eq!(args["button"], "right");
    assert_eq!(args["modifiers"], json!(["ctrl", "shift"]));
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
        path: Vec::new(),
        to: Target {
            session_id: "x".into(),
            resource_id: "r".into(),
            observation_id: "o".into(),
            node_id: "m".into(),
        },
        button: None,
        fraction: None,
        to_fraction: None,
        modifiers: vec![PointerModifier::Alt],
        hold_ms: None,
    };
    backend.act(&snapshot, &node, &drag, &context()).unwrap();
    let (called, args) = last();
    assert_eq!(called, "desktop.windows_uia.drag@1");
    assert_eq!(args["modifiers"], json!(["alt"]));
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

    // Window actions are typed only where the driver labelled the node.
    let window = json!({"node_id":"root","actions":["activate","close_window","minimize"]});
    let mut windowed = Map::new();
    for action in [
        "snapshot",
        "activate",
        "close_window",
        "minimize",
        "maximize",
        "restore",
    ] {
        windowed.insert(action.into(), json!({"contract_major":1}));
    }
    let provider_with_windows = Arc::new(Windows {
        manifest: manifest::parse(&manifest::document("desktop.windows_uia", windowed)).unwrap(),
        calls: Mutex::new(Vec::new()),
    });
    let window_backend = NativeBackend::new(provider_with_windows.clone(), Flavor::Windows);
    assert_eq!(
        window_backend.node_capabilities(&window),
        vec!["minimize", "close_window", "activate"]
    );
    window_backend
        .act(&snapshot, &window, &Action::Activate {}, &context())
        .unwrap();
    let calls = provider_with_windows.calls.lock().unwrap();
    let (called, args) = calls.last().unwrap();
    assert_eq!(called, "desktop.windows_uia.activate@1");
    assert_eq!(args["target"]["node_id"], "root");
    drop(calls);

    // A screenshot is of the quoted, revalidated node, in the browser's shape.
    assert_eq!(backend.reads(), vec!["capture"]);
    let shot = backend
        .capture(&snapshot, &node, Duration::from_secs(1))
        .unwrap();
    let (called, args) = last();
    assert_eq!(called, "desktop.windows_uia.capture@1");
    assert_eq!(
        args["target"],
        json!({"snapshot_id":"s","revision":1,"node_id":"n"})
    );
    assert_eq!(
        shot,
        json!({"format":"png","data":"aGk=","width":2,"height":1,"clipped":false})
    );

    let linux = NativeBackend::new(
        Fixture::new("desktop.linux_atspi", Flavor::Linux),
        Flavor::Linux,
    );
    assert!(linux.reads().is_empty());
    assert_eq!(
        linux
            .capture(&snapshot, &node, Duration::from_secs(1))
            .unwrap_err()
            .code,
        "CONTROL.ACTION_UNSUPPORTED"
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
