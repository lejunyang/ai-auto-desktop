use super::*;

fn collect(session: &Session, main: &str, traversal: Value, options: Value) -> Response {
    let mut request = json!({
        "method":"collect_table","resource_id":main,"table":{"role":"table","name":"Ledger"},
        "columns":["ID","Amount"],"row_key":"ID","traversal":traversal
    });
    request
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    call_within(session, serde_json::from_value(request).unwrap(), 30_000)
}

fn success(response: Response) -> Value {
    assert!(response.error.is_none(), "{response:?}");
    response.data.unwrap()
}

#[test]
#[ignore = "requires an isolated sandboxed headless Chromium"]
fn collect_table_paginates_and_waits_for_replaced_controls() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Name", WaitCondition::Appears)
        .error
        .is_none());
    fixture.client().eval(r#"
        document.body.innerHTML = '<table aria-label="Ledger"><tr><th>ID</th><th>Amount</th></tr><tbody id="rows"></tbody></table><div id="pager"></div>';
        window.page = 0;
        window.render = () => {
          rows.innerHTML = '<tr><td>K'+page+'</td><td>'+page+'.10</td></tr>';
          pager.innerHTML = '<button '+(page===2?'disabled':'')+'>Next</button>';
          pager.firstChild.onclick = () => setTimeout(() => { ++page; render() },230);
        };
        render(); true"#).unwrap();
    let traversal = json!({"mode":"pagination","next":{"role":"button","name":"Next"}});
    let first = success(collect(
        &session,
        &main,
        traversal.clone(),
        json!({"max_pages":1}),
    ));
    assert_eq!(first["stop_reason"], "page_limit");
    assert_eq!(first["actions"], 0);
    let all = success(collect(&session, &main, traversal.clone(), json!({})));
    assert_eq!(all["complete"], true);
    assert_eq!(all["actions"], 2);
    assert_eq!(
        all["records"],
        json!([{"ID":"K0","Amount":"0.10"},{"ID":"K1","Amount":"1.10"},{"ID":"K2","Amount":"2.10"}])
    );
    fixture
        .client()
        .eval("rows.innerHTML+=rows.innerHTML; true")
        .unwrap();
    let duplicate = collect(&session, &main, traversal, json!({}));
    assert_eq!(duplicate.error.unwrap()["code"], "CONTROL.TABLE_KEY");
}

#[test]
#[ignore = "requires an isolated sandboxed headless Chromium"]
fn collect_table_scrolls_a_recycled_window_and_checks_total() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Name", WaitCondition::Appears)
        .error
        .is_none());
    fixture.client().eval(r#"
        document.body.innerHTML='<div id="box" role="region" aria-label="Viewport" style="height:200px;width:500px;overflow:auto"><div style="height:540px"><table aria-label="Ledger" aria-rowcount="19"><thead><tr><th>ID</th><th>Amount</th></tr></thead><tbody id="rows"></tbody></table></div></div>';
        window.render=()=>{
          const start = Math.min(11,Math.floor(box.scrollTop/30));
          rows.innerHTML=Array.from({length:7},(_,i)=>'<tr><td>K'+(start+i)+'</td><td>0.10</td></tr>').join('');
        };
        box.onscroll=render;render();true"#).unwrap();
    let traversal =
        json!({"mode":"virtual_scroll","container":{"role":"region","name":"Viewport"}});
    let all = success(collect(&session, &main, traversal.clone(), json!({})));
    assert_eq!(all["stop_reason"], "bottom_and_row_count", "{all}");
    assert_eq!(all["expected_rows"], 18);
    assert_eq!(
        all["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["ID"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        (0..18).map(|i| format!("K{i}")).collect::<Vec<_>>()
    );
    let partial = success(collect(&session, &main, traversal, json!({})));
    assert_eq!(partial["stop_reason"], "start_not_at_top");
    assert_eq!(partial["complete"], false);
}
