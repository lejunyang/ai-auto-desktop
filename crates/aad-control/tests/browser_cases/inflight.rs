use super::*;
use aad_control::browser_access::BrowserAccess;

const LOCAL_INPUT: &str = r#"<!doctype html><input aria-label="Local editor"><canvas aria-label="Local canvas" width="300" height="200" style="display:block"></canvas><iframe srcdoc='<input aria-label="Child editor">'></iframe>"#;

fn local_session(fixture: &Fixture) -> (Session, PathBuf, String) {
    let dir = fixture.root.join("input grant");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("main.html");
    std::fs::write(&file, LOCAL_INPUT).unwrap();
    let url = url::Url::from_file_path(&file).unwrap().to_string();
    let access = BrowserAccess::new(&[dir], &[]).unwrap();
    let session = Session::new(
        vec![Arc::new(BrowserBackend::with_access(
            fixture.port,
            access.clone(),
        ))],
        Policy {
            allow_input: true,
            browser_access: access,
            ..Default::default()
        },
    );
    fixture
        .client()
        .command("Page.navigate", json!({"url":url}))
        .unwrap();
    wait_url(fixture, &url);
    (session, file, url)
}

fn wait_url(fixture: &Fixture, url: &str) {
    let until = Instant::now() + Duration::from_secs(10);
    while page_value(fixture, "location.href") != url
        || page_value(fixture, "document.readyState") != "complete"
    {
        assert!(Instant::now() < until, "navigation never reached {url}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "real sandboxed Chromium"]
fn file_history_rewrites_preserve_or_revoke_grants_during_a_held_key() {
    let _serial = serial();
    let fixture = Fixture::with_automation(true);
    let (session, _file, url) = local_session(&fixture);
    let outside = fixture.root.join("outside.html");
    std::fs::write(&outside, "<!doctype html>outside").unwrap();
    let outside = url::Url::from_file_path(outside).unwrap().to_string();
    let value=page_value(&fixture,&format!("(()=>{{try{{history.pushState(null,'',{});return {{changed:true,url:location.href}};}}catch(e){{return {{changed:false,error:e.name,url:location.href}};}}}})()",serde_json::to_string(&outside).unwrap()));
    eprintln!("file history rewrite: {value}");
    if value["changed"] == false {
        assert_eq!(value["error"], "SecurityError");
        assert_eq!(value["url"], url);
    } else {
        assert_eq!(value["url"], outside);
    }
    // Chromium versions that forbid changing a file's path still permit a
    // fragment-only SPA change. Neither kind of rewrite replaces the loader.
    fixture
        .client()
        .command("Page.navigate", json!({"url":url}))
        .unwrap();
    wait_url(&fixture, &url);
    let rewritten = if value["changed"] == true {
        outside
    } else {
        format!("{url}#state")
    };
    let loader = fixture
        .client()
        .command("Page.getFrameTree", json!({}))
        .unwrap()["frameTree"]["frame"]["loaderId"]
        .clone();
    assert_eq!(page_value(&fixture,&format!("window.rewriteEvents=[];document.addEventListener('keydown',e=>{{window.rewriteEvents.push(['down',e.isTrusted]);history.pushState(null,'',{});}},true);document.addEventListener('keyup',e=>window.rewriteEvents.push(['up',e.isTrusted]),true);true",serde_json::to_string(&rewritten).unwrap())),true);
    let main = main_of(&session, &fixture.page_id);
    let result = act_on(
        &session,
        &main,
        "textbox",
        "Local editor",
        Action::Key {
            keys: vec!["ctrl".into(), "k".into()],
            hold_ms: Some(300),
        },
    );
    if value["changed"] == true {
        assert_eq!(result.error.as_ref().unwrap()["code"], "POLICY.DENIED");
        assert_eq!(result.error.unwrap()["details"]["key_released"], true);
    } else {
        assert!(result.error.is_none(), "{result:?}");
    }
    assert_eq!(page_value(&fixture, "location.href"), rewritten);
    assert_eq!(
        page_value(&fixture, "window.rewriteEvents"),
        json!([["down", true], ["up", true]])
    );
    assert_eq!(
        fixture
            .client()
            .command("Page.getFrameTree", json!({}))
            .unwrap()["frameTree"]["frame"]["loaderId"],
        loader
    );
}

#[test]
#[ignore = "real sandboxed Chromium"]
fn in_flight_file_grants_stop_keys_clicks_paths_typing_and_ancestor_input() {
    let _serial = serial();
    let fixture = Fixture::with_automation(true);
    let (session, file, _url) = local_session(&fixture);
    let main = main_of(&session, &fixture.page_id);
    let mut verifier = fixture.client();
    verifier.filter_events(&["Runtime.bindingCalled"], &[]);
    verifier
        .command("Runtime.addBinding", json!({"name":"aadGrantRevoked"}))
        .unwrap();
    assert_eq!(
        page_value(
            &fixture,
            r#"window.guardEvents=[];window.revokeOn='';window.signalled=false;
        for(const doc of [document,document.querySelector('iframe').contentDocument]) {
            for(const type of ['keydown','keyup','mousedown','mouseup','mousemove','input']) {
                doc.addEventListener(type,e=>{
                    if(type==='mousemove'&&!e.buttons)return;
                    window.guardEvents.push([type,e.isTrusted,e.buttons||0,e.key||'']);
                    if(type===window.revokeOn&&!window.signalled){
                        window.signalled=true;window.aadGrantRevoked(type);
                        const until=performance.now()+150;while(performance.now()<until){}
                    }
                },true);
            }
        }true"#
        ),
        true
    );
    let initial_loader = fixture
        .client()
        .command("Page.getFrameTree", json!({}))
        .unwrap()["frameTree"]["frame"]["loaderId"]
        .clone();
    let cases = [
        "key",
        "click",
        "drag",
        "path",
        "type_keys",
        "type_unicode",
        "double_click",
        "child",
    ];
    for kind in cases {
        let signal = match kind {
            "path" | "drag" => "mousemove",
            "type_unicode" => "input",
            "double_click" => "mouseup",
            _ if kind == "click" => "mousedown",
            _ => "keydown",
        };
        assert_eq!(page_value(&fixture,&format!("window.guardEvents=[];window.signalled=false;window.revokeOn={};document.querySelector('input').value='';true",serde_json::to_string(signal).unwrap())),true);
        let resource = if kind == "child" {
            targets(&session)
                .into_iter()
                .find(|r| {
                    r["metadata"]["page_id"] == fixture.page_id
                        && r["metadata"]["main_frame"] == false
                })
                .unwrap()["resource_id"]
                .as_str()
                .unwrap()
                .to_string()
        } else {
            main.clone()
        };
        let observed = observe(&session, &resource);
        let surface = matches!(kind, "click" | "drag" | "path" | "double_click");
        let target = find(
            &session,
            &observed,
            if surface { "canvas" } else { "textbox" },
            if surface {
                "Local canvas"
            } else if kind == "child" {
                "Child editor"
            } else {
                "Local editor"
            },
        );
        let action = match kind {
            "key" | "child" => Action::Key {
                keys: vec!["ctrl".into(), "k".into()],
                hold_ms: Some(1000),
            },
            "click" | "double_click" => Action::Click {
                button: None,
                count: (kind == "double_click").then_some(2),
                fraction: None,
                modifiers: Vec::new(),
                hold_ms: (kind == "click").then_some(1000),
            },
            "drag" => Action::Drag {
                to: target.clone(),
                fraction: Some(Fraction { fx: 0.1, fy: 0.1 }),
                to_fraction: Some(Fraction { fx: 0.9, fy: 0.9 }),
                button: None,
                modifiers: Vec::new(),
                hold_ms: None,
                path: Vec::new(),
            },
            "path" => surface_path(&target, &[(0.1, 0.9), (0.9, 0.9)], None),
            "type_keys" | "type_unicode" => Action::TypeText {
                text: "abc".into(),
                mode: Some(if kind == "type_keys" {
                    TypeMode::Keys
                } else {
                    TypeMode::Unicode
                }),
            },
            _ => unreachable!(),
        };
        // The browser emits this event while the handler is still executing.
        // Removing our own temporary file invalidates the canonical grant but
        // leaves the loaded DOM, loader, focus and event log unchanged.
        verifier.drain_events();
        let revoked = file.clone();
        let revoker = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                if verifier.drain_events().events.iter().any(|e| {
                    e["method"] == "Runtime.bindingCalled"
                        && e["params"]["name"] == "aadGrantRevoked"
                }) {
                    std::fs::remove_file(revoked).unwrap();
                    return verifier;
                }
                assert!(Instant::now() < until, "input never reached the fixture");
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let result = act(&session, target, action);
        verifier = revoker.join().unwrap();
        std::fs::write(&file, LOCAL_INPUT).unwrap();
        assert_eq!(
            result.error.as_ref().unwrap()["code"],
            "POLICY.DENIED",
            "{kind}: {result:?}"
        );
        let events = page_value(&fixture, "window.guardEvents.map(e=>e[0])");
        let expected = match kind {
            "key" | "child" => json!(["keydown", "keyup"]),
            "click" | "double_click" => json!(["mousedown", "mouseup"]),
            "drag" | "path" => json!(["mousedown", "mousemove", "mouseup"]),
            "type_keys" => json!(["keydown", "input", "keyup"]),
            "type_unicode" => json!(["input"]),
            _ => unreachable!(),
        };
        assert_eq!(events, expected, "{kind}: {result:?}");
        assert_eq!(
            page_value(&fixture, "window.guardEvents.every(e=>e[1])"),
            true
        );
        if matches!(kind, "key" | "child" | "type_keys") {
            assert_eq!(
                result.error.as_ref().unwrap()["details"]["key_released"],
                if kind == "type_keys" {
                    Value::Null
                } else {
                    json!(true)
                }
            );
        }
        if matches!(kind, "click" | "drag" | "path") {
            assert_eq!(
                result.error.as_ref().unwrap()["details"]["pointer_button_released"],
                true,
                "{kind}"
            );
        }
        if kind.starts_with("type_") {
            assert_eq!(
                page_value(&fixture, "document.querySelector('input').value"),
                "a"
            );
        }
        assert_eq!(
            fixture
                .client()
                .command("Page.getFrameTree", json!({}))
                .unwrap()["frameTree"]["frame"]["loaderId"],
            initial_loader
        );
    }
}

#[test]
#[ignore = "real sandboxed Chromium"]
fn history_back_and_forward_refuse_ungranted_destinations_before_navigation() {
    let _serial = serial();
    let fixture = Fixture::with_automation(true);
    // Do not replace the startup entry before its initial navigation commits.
    wait_url(&fixture, &fixture.url);
    let file = fixture.root.join("history.html");
    std::fs::write(&file, "<!doctype html><p>Private history</p>").unwrap();
    let private = url::Url::from_file_path(&file).unwrap().to_string();
    fixture
        .client()
        .command("Page.navigate", json!({"url":private}))
        .unwrap();
    wait_url(&fixture, &private);
    let current = format!("{}second", fixture.url);
    fixture
        .client()
        .command("Page.navigate", json!({"url":current}))
        .unwrap();
    wait_url(&fixture, &current);
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    let observed = observe(&session, &main);
    let denied = act(&session, document(&session, &observed), Action::Back {});
    assert_eq!(denied.error.unwrap()["code"], "POLICY.DENIED");
    assert_eq!(page_value(&fixture, "location.href"), current);
    let history = fixture
        .client()
        .command("Page.getNavigationHistory", json!({}))
        .unwrap();
    let initial = history["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["url"] == fixture.url)
        .unwrap()["id"]
        .clone();
    fixture
        .client()
        .command("Page.navigateToHistoryEntry", json!({"entryId":initial}))
        .unwrap();
    wait_url(&fixture, &fixture.url);
    let observed = observe(&session, &main);
    let denied = act(&session, document(&session, &observed), Action::Forward {});
    assert_eq!(denied.error.unwrap()["code"], "POLICY.DENIED");
    assert_eq!(page_value(&fixture, "location.href"), fixture.url);

    // History entries are existing documents, not new caller-supplied URLs.
    // Keep inert/opaque entries and long web URLs reachable through history.
    for previous in [
        "about:blank".to_string(),
        "data:text/html,History".to_string(),
        format!("{}second?{}", fixture.url, "q".repeat(2200)),
    ] {
        fixture
            .client()
            .command("Page.navigate", json!({"url":previous}))
            .unwrap();
        wait_url(&fixture, &previous);
        fixture
            .client()
            .command("Page.navigate", json!({"url":current}))
            .unwrap();
        wait_url(&fixture, &current);
        let observed = observe(&session, &main);
        let back = act(&session, document(&session, &observed), Action::Back {});
        assert!(back.error.is_none(), "{back:?}");
        wait_url(&fixture, &previous);
        let observed = observe(&session, &main);
        let forward = act(&session, document(&session, &observed), Action::Forward {});
        assert!(forward.error.is_none(), "{forward:?}");
        wait_url(&fixture, &current);
    }
}
