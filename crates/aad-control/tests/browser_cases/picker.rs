use super::*;

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled"]
fn choose_files_reads_bytes_only_into_the_owned_input() {
    let _serial = serial();
    let fixture = Fixture::new();
    let grant = fixture.root.join("granted");
    std::fs::create_dir(&grant).unwrap();
    let first = grant.join("first.txt");
    let second = grant.join("second.txt");
    let denied = fixture.root.join("outside.txt");
    for (path, contents) in [
        (&first, "first payload"),
        (&second, "second payload"),
        (&denied, "not granted"),
    ] {
        std::fs::write(path, contents).unwrap();
    }
    let policy = Policy {
        allow_input: true,
        upload_roots: vec![grant],
        ..Default::default()
    };
    let session = Session::new(
        vec![Arc::new(BrowserBackend::new(fixture.port))],
        policy.clone(),
    );
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Name", WaitCondition::Appears)
        .error
        .is_none());
    page_value(
        &fixture,
        r#"(() => {
      const button = document.createElement('button'); button.textContent='Choose attachment'; document.body.prepend(button);
      window.pickerClicks=0; window.pickerInputs=[]; window.mode='single'; window.trustedEvents=[]; window.cancels=0;
      document.querySelector('[aria-label="Hidden upload"]').onclick=()=>pickerClicks++;
      window.newInput = (doc=document) => {
        const input=doc.createElement('input'); input.type='file'; input.hidden=true; doc.body.append(input);
        input.oninput=input.onchange=event=>{if(event.isTrusted) trustedEvents.push(event.type)};
        input.oncancel=()=>{cancels++; input.remove()}; pickerInputs.push(input); return input;
      };
      button.onclick=()=>{
        pickerClicks++;
        const input=newInput(); input.multiple=mode==='multi';
        if(mode==='directory') input.webkitdirectory=true;
        if(mode==='unrelated') { queueMicrotask(()=>newInput().click()); return; }
        if(mode==='wrong-frame') { newInput(document.querySelector('iframe').contentDocument).click(); return; }
        if(mode==='wrong-page') { newInput(peer.document).click(); return; }
        if(mode==='wrong-node') { input.onclick=()=>{input.type='text'; newInput().showPicker(); queueMicrotask(()=>input.type='file')}; }
        input.click();
        if(mode==='cancel') {const wait=new XMLHttpRequest(); wait.open('GET',cancelUrl,false); wait.send();}
        if(mode==='two') newInput().click();
        if(mode==='replaced') input.replaceWith(newInput());
        if(mode==='changed-mode') input.multiple=!input.multiple;
        if(mode==='adopted') document.querySelector('iframe').contentDocument.body.append(input);
        if(mode==='navigation') history.pushState(null,'','#new-document-location');
        if(mode==='reload') { peer.held=input; location.assign('/second'); }
      };
      return true;
    })()"#,
    );
    let choose = |session: &Session, name: &str, files: &[&std::path::Path]| {
        let observed = observe(session, &main);
        let target = find(session, &observed, "button", name);
        act(
            session,
            target,
            serde_json::from_value(json!({"type":"choose_files","files":files})).unwrap(),
        )
    };
    let button = "Choose attachment";
    let refused = choose(&session, button, &[&first, &denied]);
    assert_eq!(refused.error.unwrap()["code"], "POLICY.DENIED");
    assert_eq!(refused.commit_state, CommitState::NotCommitted);
    assert_eq!(page_value(&fixture, "pickerClicks"), 0);
    let selected = choose(&session, button, &[&first]);
    assert!(selected.error.is_none(), "{selected:?}");
    let contents = "Promise.all(Array.from(pickerInputs.at(-1).files, file=>file.text()))";
    assert_eq!(page_value(&fixture, contents), json!(["first payload"]));
    assert_eq!(
        page_value(&fixture, "cancels===0 && pickerInputs.at(-1).isConnected"),
        true
    );
    let competitor = Session::new(vec![Arc::new(BrowserBackend::new(fixture.port))], policy);
    main_of(&competitor, &fixture.page_id);
    let busy = choose(&competitor, button, &[&first]);
    assert_eq!(busy.error.unwrap()["code"], "CONTROL.RESOURCE_BUSY");
    assert_eq!(page_value(&fixture, "pickerClicks"), 1);
    drop(competitor);
    page_value(&fixture, "mode='multi'");
    let selected = choose(&session, button, &[&second, &first]);
    assert!(selected.error.is_none(), "{selected:?}");
    assert_eq!(
        page_value(&fixture, contents),
        json!(["second payload", "first payload"])
    );
    assert_eq!(
        page_value(&fixture, "trustedEvents"),
        json!(["input", "change", "input", "change"])
    );
    assert_eq!(
        page_value(&fixture, "cancels===0 && pickerInputs.at(-1).isConnected"),
        true
    );
    let hidden = choose(&session, "Hidden upload", &[&second, &first]);
    assert!(hidden.error.is_none(), "{hidden:?}");
    assert_eq!(
        page_value(
            &fixture,
            r#"Promise.all(Array.from(document.querySelector('[aria-label="Hidden upload"]').files, file=>file.text()))"#
        ),
        json!(["second payload", "first payload"])
    );
    assert_eq!(page_value(&fixture, "pickerClicks"), 2);
    fixture.client().command("Runtime.evaluate", json!({"expression":"window.peer=open('about:blank','picker-peer'); !!peer","userGesture":true})).unwrap();
    let peer_target = aad_browser::cdp::Client::list_targets(fixture.port)
        .unwrap()
        .into_iter()
        .find(|tab| tab.kind == "page" && tab.url == "about:blank" && tab.id != fixture.page_id)
        .unwrap();
    let mut peer = aad_browser::cdp::Client::attach(&peer_target).unwrap();
    peer.filter_events(&["Page.fileChooserOpened"], &[]);
    peer.command("Page.enable", json!({})).unwrap();
    peer.command(
        "Page.setInterceptFileChooserDialog",
        json!({"enabled":true,"cancel":true}),
    )
    .unwrap();
    for mode in [
        "single",
        "directory",
        "wrong-frame",
        "wrong-node",
        "wrong-page",
        "two",
        "unrelated",
        "changed-mode",
        "replaced",
        "adopted",
        "navigation",
    ] {
        page_value(&fixture, &format!("pickerInputs=[]; mode='{mode}'"));
        if mode == "wrong-page" {
            peer.command(
                "Runtime.evaluate",
                json!({"expression":"true","userGesture":true}),
            )
            .unwrap();
        }
        let paths = if mode == "single" {
            vec![first.as_path(), second.as_path()]
        } else {
            vec![first.as_path()]
        };
        let rejected = choose(&session, button, &paths);
        assert!(rejected.error.is_some(), "{mode}: {rejected:?}");
        if mode == "single" {
            assert_eq!(rejected.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
        }
        assert_eq!(page_value(&fixture,"pickerInputs.length>0 && pickerInputs.every(input=>!input.matches(':open') && input.files.length===0) && trustedEvents.length===4"),true,"{mode}");
        if mode == "wrong-page" {
            assert!(peer
                .drain_events()
                .events
                .iter()
                .any(|event| event["params"]["backendNodeId"].is_number()));
        }
    }
    let signal = TcpListener::bind("127.0.0.1:0").unwrap();
    signal.set_nonblocking(true).unwrap();
    page_value(
        &fixture,
        &format!(
            "window.cancelUrl='http://{}/'; pickerInputs=[]; mode='cancel'",
            signal.local_addr().unwrap()
        ),
    );
    let target = find(&session, &observe(&session, &main), "button", button);
    let request = Request::new(
        Some(session.id().into()),
        Operation::Act {
            target,
            action: Action::ChooseFiles {
                files: vec![first.display().to_string()],
            },
            verify: None,
        },
    );
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| session.execute(&request));
        let until = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            if let Ok((stream, _)) = signal.accept() {
                break stream;
            }
            assert!(
                Instant::now() < until,
                "the picker callback did not reach its barrier"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(session.cancel(&request.request_id));
        stream.write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n").unwrap();
        drop(stream);
        let cancelled = pending.join().unwrap();
        assert_eq!(cancelled.error.unwrap()["code"], "CONTROL.CANCELLED");
        assert_eq!(cancelled.commit_state, CommitState::Unknown);
    });
    assert_eq!(
        page_value(
            &fixture,
            "pickerInputs.length===1 && !pickerInputs[0].matches(':open') && pickerInputs[0].files.length===0"
        ),
        true
    );
    page_value(&fixture, "mode='single'");
    let recovered = choose(&session, button, &[&second]);
    assert!(recovered.error.is_none(), "{recovered:?}");
    assert_eq!(page_value(&fixture, contents), json!(["second payload"]));
    page_value(&fixture, "mode='reload'");
    let navigated = choose(&session, button, &[&first]);
    assert!(navigated.error.is_some(), "{navigated:?}");
    assert_eq!(
        peer.eval("held.files.length").unwrap()["result"]["value"],
        0
    );
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled"]
fn choose_files_intercepts_the_observed_child_renderer() {
    let _serial = serial();
    let fixture = Fixture::new();
    let file = fixture.root.join("child.txt");
    std::fs::write(&file, "child bytes").unwrap();
    let session = Session::new(
        vec![Arc::new(BrowserBackend::new(fixture.port))],
        Policy {
            allow_input: true,
            upload_roots: vec![fixture.root.clone()],
            ..Default::default()
        },
    );
    let child = frame_of(&session, |meta| meta["out_of_process"] == true);
    let resource = child["resource_id"].as_str().unwrap();
    assert!(
        wait_until(&session, resource, "CrossButton", WaitCondition::Appears)
            .error
            .is_none()
    );
    let mut verifier = fixture.client();
    let attached = verifier
        .command(
            "Target.attachToTarget",
            json!({"targetId":child["metadata"]["frame_id"],"flatten":true}),
        )
        .unwrap();
    let child_session = attached["sessionId"].as_str();
    let mut eval = |expression: &str| {
        verifier
            .session_command(
                child_session,
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
            )
            .unwrap()["result"]["value"]
            .clone()
    };
    assert_eq!(eval("document.querySelector('button').onclick=()=>{window.field=document.createElement('input');field.type='file';field.hidden=true;document.body.append(field);field.click()};true"),true);
    let selected = act_on(
        &session,
        resource,
        "button",
        "CrossButton",
        Action::ChooseFiles {
            files: vec![file.display().to_string()],
        },
    );
    assert!(selected.error.is_none(), "{selected:?}");
    assert_eq!(eval("field.files[0].text()"), "child bytes");
}
