use super::*;
use aad_control::browser_access::BrowserAccess;

fn granted_session(fixture: &Fixture, access: BrowserAccess) -> Session {
    Session::new(
        vec![Arc::new(BrowserBackend::with_access(
            fixture.port,
            access.clone(),
        ))],
        Policy {
            allow_input: true,
            browser_access: access,
            ..Default::default()
        },
    )
}

fn wait_for_url(fixture: &Fixture, url: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if page_value(fixture, "location.href") == url
            && page_value(fixture, "document.readyState") == "complete"
        {
            return;
        }
        assert!(Instant::now() < deadline, "did not reach {url}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "real sandboxed Chromium"]
fn file_document_grants_cover_navigation_actual_click_and_fresh_access_checks() {
    let _serial = serial();
    let fixture = Fixture::with_automation(true);
    let dir = fixture.root.join("granted documents");
    std::fs::create_dir(&dir).unwrap();
    let inside = dir.join("测试 page.html");
    let outside = fixture.root.join("outside.html");
    std::fs::write(&inside, "<!doctype html><title>AAD local file</title><button onclick=\"document.body.dataset.clicked='yes'\">Local fixture button</button>").unwrap();
    std::fs::write(
        &outside,
        "<!doctype html><title>Outside fixture</title><p>Must not be observed without a grant</p>",
    )
    .unwrap();
    let url = url::Url::from_file_path(&inside).unwrap().to_string();
    let outside_url = url::Url::from_file_path(&outside).unwrap().to_string();
    let (default, _) = session_for(&fixture);
    let main = main_of(&default, &fixture.page_id);
    let observed = observe(&default, &main);
    assert_eq!(
        act(
            &default,
            document(&default, &observed),
            Action::Navigate { url: url.clone() }
        )
        .error
        .unwrap()["code"],
        "POLICY.DENIED"
    );
    default.close();

    let session = granted_session(&fixture, BrowserAccess::new(&[dir], &[]).unwrap());
    let main = main_of(&session, &fixture.page_id);
    let observed = observe(&session, &main);
    let navigated = act(
        &session,
        document(&session, &observed),
        Action::Navigate { url: url.clone() },
    );
    assert!(navigated.error.is_none(), "{navigated:?}");
    wait_for_url(&fixture, &url);
    let observed = observe(&session, &main);
    let clicked = act(
        &session,
        find(&session, &observed, "button", "Local fixture button"),
        Action::click(),
    );
    assert!(clicked.error.is_none(), "{clicked:?}");
    assert_eq!(page_value(&fixture, "document.body.dataset.clicked"), "yes");

    let observed = observe(&session, &main);
    let denied = act(
        &session,
        document(&session, &observed),
        Action::OpenTab {
            url: outside_url.clone(),
        },
    );
    assert_eq!(denied.error.unwrap()["code"], "POLICY.DENIED");
    let observed = observe(&session, &main);
    let old_button = find(&session, &observed, "button", "Local fixture button");
    // Independent verifier simulates a user/external navigation between calls.
    fixture
        .client()
        .command("Page.navigate", json!({"url":outside_url}))
        .unwrap();
    wait_for_url(&fixture, &outside_url);
    assert_eq!(
        act(&session, old_button, Action::click()).error.unwrap()["code"],
        "POLICY.DENIED"
    );
    let listed = targets(&session);
    let denied = listed
        .iter()
        .find(|r| r["metadata"]["page_id"] == fixture.page_id)
        .unwrap();
    assert!(denied["capabilities"].as_array().unwrap().is_empty());
    assert_eq!(denied["metadata"]["access_error"]["code"], "POLICY.DENIED");
    let refused = call(
        &session,
        Operation::Observe {
            resource_id: denied["resource_id"].as_str().unwrap().into(),
            max_nodes: 100,
            view: Default::default(),
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "POLICY.DENIED");
}

#[test]
#[ignore = "real sandboxed Chromium"]
fn internal_origin_grant_opens_actual_browser_ui_and_refuses_other_origins() {
    let _serial = serial();
    let fixture = Fixture::with_automation(true);
    let session = granted_session(
        &fixture,
        BrowserAccess::new(
            &[],
            &["chrome://version".into(), "chrome://settings".into()],
        )
        .unwrap(),
    );
    let main = main_of(&session, &fixture.page_id);
    let observed = observe(&session, &main);
    let result = act(
        &session,
        document(&session, &observed),
        Action::Navigate {
            url: "chrome://version".into(),
        },
    );
    assert!(result.error.is_none(), "{result:?}");
    wait_for_url(&fixture, "chrome://version/");
    let observed = observe(&session, &main);
    assert!(observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["name"]
            .as_str()
            .is_some_and(|name| name.contains("Chromium") || name.contains("Chrome"))));
    let result = act(
        &session,
        document(&session, &observed),
        Action::Navigate {
            url: "chrome://extensions".into(),
        },
    );
    assert_eq!(result.error.unwrap()["code"], "POLICY.DENIED");
    let observed = observe(&session, &main);
    let result = act(
        &session,
        document(&session, &observed),
        Action::Navigate {
            url: "chrome://settings".into(),
        },
    );
    assert!(result.error.is_none(), "{result:?}");
    wait_for_url(&fixture, "chrome://settings/");
    // Narrow windows collapse Settings' search into a button. Exercise that
    // real UI transition before looking for its editable field.
    let observed = observe(&session, &main);
    if let Some(search) = observed["nodes"].as_array().unwrap().iter().find(|n| {
        n["role"] == "button" && matches!(n["name"].as_str(), Some("搜索设置" | "Search settings"))
    }) {
        let target = serde_json::from_value(search["target"].clone()).unwrap();
        let result = act(&session, target, Action::click());
        assert!(result.error.is_none(), "{result:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let observed = loop {
        let response = call(
            &session,
            Operation::Observe {
                resource_id: main.clone(),
                max_nodes: 2000,
                view: Default::default(),
            },
        );
        assert!(response.error.is_none(), "{response:?}");
        let observed = response.data.unwrap();
        if observed["nodes"].as_array().unwrap().iter().any(|n| {
            matches!(n["role"].as_str(), Some("searchbox" | "textbox"))
                && n["actions"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a == "fill"))
        }) {
            break observed;
        }
        assert!(
            Instant::now() < deadline,
            "settings search was not exposed after opening the search UI"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let search = observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| {
            matches!(n["role"].as_str(), Some("searchbox" | "textbox"))
                && n["actions"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a == "fill"))
        })
        .unwrap();
    let target = serde_json::from_value(search["target"].clone()).unwrap();
    let result = act(
        &session,
        target,
        Action::Fill {
            value: "appearance".into(),
        },
    );
    assert!(result.error.is_none(), "{result:?}");
    assert_eq!(page_value(&fixture, "(()=>{const todo=[document];while(todo.length){const root=todo.pop();for(const el of root.querySelectorAll('*')){if(el.tagName==='INPUT'&&el.value==='appearance')return true;if(el.shadowRoot)todo.push(el.shadowRoot);}}return false;})()"), true);
    if let Some(directory) = std::env::var_os("AAD_BROWSER_ACCESS_ARTIFACTS") {
        use base64::Engine;
        let observed = observe(&session, &main);
        let captured = call(
            &session,
            Operation::Capture {
                target: document(&session, &observed),
            },
        );
        assert!(captured.error.is_none(), "{captured:?}");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(captured.data.unwrap()["data"].as_str().unwrap())
            .unwrap();
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("chromium-settings-search.png"), bytes).unwrap();
    }
}
