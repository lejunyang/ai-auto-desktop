use super::{Watch, Window, PROVIDER};
use aad_browser::recording::Result;
use aad_control::steps::Step;
use serde_json::{json, Value};

impl Watch {
    pub fn compile_action(
        &self,
        step: &Step,
        window: &Window,
        compiled: &mut Vec<Value>,
    ) -> Result<()> {
        // Linux control resources span the whole app. Restrict the fresh
        // observation to the named window before finding the action's target.
        let output =
            |part: &str, field: &str| format!("${{{{ steps.{}_{part}.output.{field} }}}}", step.id);
        let mut add = |part: &str, action: &str, args: Value| {
            let contract = self.contract(action);
            compiled.push(json!({
                "id":format!("{}_{part}",step.id),"type":"action","uses":format!("{PROVIDER}.{action}@1"),
                "with":args,"effect":{"class":contract["effect"]["default_class"]},"risk":contract["risk"]
            }));
        };
        let find = |snapshot: &str, locator: Value| {
            json!({
                "snapshot_id":output(snapshot,"snapshot_id"),"revision":output(snapshot,"revision"),"locator":locator
            })
        };
        add(
            "app",
            "snapshot",
            json!({"application":{"name":self.app_name},"max_nodes":2000}),
        );
        add("window", "find", find("app", json!(window)));
        add(
            "scope",
            "snapshot_scope",
            json!({"target":output("window","target"),"locator":window,"max_nodes":2000}),
        );
        add("control", "find", find("scope", json!(step.locator)));
        let mut args = json!({"target":output("control","target"),"locator":step.locator});
        let action = step.action.as_ref().ok_or("missing native action")?;
        let method = match action["type"].as_str() {
            Some("fill") => {
                args["text"] = action["value"].clone();
                "set_text"
            }
            Some("key") => {
                args["keys"] = action["keys"].clone();
                "key_chord"
            }
            Some("scroll") => {
                args["scroll"] =
                    json!({"direction":action["direction"],"notches":action["notches"]});
                "pointer_gesture"
            }
            Some("click") if action["button"].is_null() && action["count"].is_null() => {
                "pointer_click"
            }
            Some("click") => {
                for key in ["count", "button"] {
                    if let Some(v) = action.get(key) {
                        args[key] = v.clone();
                    }
                }
                "pointer_gesture"
            }
            _ => return Err("unsupported native action in workflow".into()),
        };
        add("act", method, args);
        Ok(())
    }
}
