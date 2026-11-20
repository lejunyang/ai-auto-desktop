use super::scope::Scope;
use super::{resolve_alias, App, Locator};
use aad_control::table::Collection;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableStep {
    collect_table: Collection,
}

/// Lower the typed table action using its provider's validation and contract.
pub(super) fn collect_table(
    mut raw: Value,
    id: &str,
    app: &App,
    targets: &BTreeMap<String, Locator>,
    scope: &Scope,
) -> Result<Value, String> {
    if *app == App::default() {
        return Err("table collection requires an application selector".into());
    }
    let spec = &mut raw["collect_table"];
    if let Some(table) = spec.get_mut("table") {
        resolve_alias(table, targets)?;
    }
    if let Some(traversal) = spec.get_mut("traversal") {
        for field in ["next", "first", "container"] {
            if let Some(locator) = traversal.get_mut(field) {
                resolve_alias(locator, targets)?;
            }
        }
    }
    let step: TableStep = serde_json::from_value(raw).map_err(|err| err.to_string())?;
    step.collect_table.validate().map_err(|err| err.message)?;
    let mut args = json!(step.collect_table);
    if args.to_string().contains("${{") {
        return Err("collect_table requires static columns, bounds and locators".into());
    }
    let mut selector = json!(app);
    scope.resolve(&mut selector)?;
    args["app"] = selector;
    Ok(json!({
        "type":"action","id":id,"uses":"control.session.collect_table@1","with":args,
        "effect":{"class":"non_idempotent"},"risk":{"category":"modify","level":"high"}
    }))
}
