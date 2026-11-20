use serde_json::{json, Value};

fn expand(raw: Value) -> Result<Value, String> {
    if raw.get("format").is_some() {
        super::program::expand(raw, None)
    } else {
        super::expand_legacy(raw)
    }
}

fn program(steps: Value) -> Value {
    json!({"kind":"Recipe","format":"recipe/2","name":"test","phases":[{"app":{"provider":"browser.cdp"},"steps":steps}]})
}

#[test]
fn rejects_unbound_names_in_every_scope_before_execution() {
    let local = json!({"set":{"hidden":"local"}});
    let use_hidden = json!({"set":{"result":"${{ hidden }}"}});
    for steps in [
        json!([{"if":"${{ True }}","then":[local],"else":[use_hidden]}]),
        json!([{"foreach":"${{ [] }}","as":"hidden","max_items":5,"steps":[local]},use_hidden]),
        json!([{"while":"${{ False }}","max_iterations":10,"steps":[local]},use_hidden]),
        json!([use_hidden, local]),
        json!([{"set":{"hidden":1,"result":"${{ hidden }}"}}]),
        json!([{"set":{"x":"${{ vars.v1 }}"}}]),
    ] {
        assert!(expand(program(steps)).is_err());
    }
}

#[test]
fn rejects_invalid_statements_even_in_empty_loops() {
    for step in [
        json!({"foreach":"${{ [] }}","as":"i","steps":[{"set":{"x":1}}]}),
        json!({"while":"${{ False }}","max_iterations":0,"steps":[{"set":{"x":1}}]}),
        json!({"if":"true","then":[{"set":{"x":1}}]}),
        json!({"foreach":"${{ [] }}","as":"i","index_as":"i","max_items":1,"steps":[{"set":{"x":1}}]}),
        json!({"foreach":"${{ [] }}","as":"i","max_items":1000,"steps":[
            {"while":"${{ False }}","max_iterations":1000,"steps":[{"set":{"x":1}}]}]}),
        json!({"foreach":"${{ [] }}","as":"i","max_items":1,"steps":[
            {"locator":{"name":"Bad"},"action":{"type":"unknown"}}]}),
        json!({"set":{"x":"${{ True }} ${{ False"}}),
        json!({"set":{"x":1},"uses":"arbitrary.provider@1"}),
    ] {
        assert!(expand(program(json!([step]))).is_err(), "{step}");
    }
}

#[test]
fn shadowed_loop_bindings_and_quoted_keys_survive_lowering() {
    let doc = expand(program(json!([
        {"set":{"row":{"row":3}}},
        {"foreach":"${{ [row] }}","as":"row","max_items":1,"steps":[
            {"set":{"x":"value=${{ row['row'] + 2e-1 }} ${{ row.row }}"}}]},
        {"set":{"after":"${{ row.row }}"}}
    ])))
    .unwrap();
    let each = &doc["steps"][1];
    assert_eq!(each["items"], "${{ [vars.v1] }}");
    assert_eq!(
        each["steps"][0]["assign"]["vars.v2"],
        "value=${{ r2_item['row'] + 2e-1 }} ${{ r2_item.row }}"
    );
    assert_eq!(doc["steps"][2]["assign"]["vars.v3"], "${{ vars.v1.row }}");
}

#[test]
fn legacy_recipes_keep_ids_permissions_and_automatic_read_outputs() {
    let mut doc = program(
        json!([{"id":"first","read":[{"as":"receipt","locator":{"name":"Receipt"},"property":"value"}]}]),
    );
    doc.as_object_mut().unwrap().remove("format");
    let old = expand(doc.clone()).unwrap();
    assert_eq!(old["steps"][0]["id"], "first");
    assert_eq!(
        old["outputs"]["receipt"]["value"],
        "${{ steps['first'].output.values['receipt'] }}"
    );
    assert_eq!(old["budgets"]["max_executed_steps"], 1);
    assert_eq!(
        old["requires"]["permissions"],
        json!(["desktop.input", "desktop.observe"])
    );
    doc["format"] = json!("recipe/999");
    assert!(expand(doc).is_err());
}
