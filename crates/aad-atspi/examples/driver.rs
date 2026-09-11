use aad_runtime::Provider;
use serde_json::json;
use std::time::Duration;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "list".into());
    let provider = aad_atspi::native_provider().expect("create AT-SPI provider");
    let result = (|| -> Result<serde_json::Value, aad_core::AutomationError> {
        match command.as_str() {
            "manifest" => Ok(aad_atspi::manifest_json()),
            "inspect" => provider.invoke(
                "desktop.linux_atspi.inspect_session@1",
                json!({}),
                Some(Duration::from_secs(5)),
            ),
            "list" => provider.invoke(
                "desktop.linux_atspi.list_applications@1",
                json!({}),
                Some(Duration::from_secs(5)),
            ),
            "snapshot" | "invoke" | "set-text" | "type-text" | "pointer-click"
            | "capture" | "toggle" | "expand" | "collapse" => {
                let process_id = arguments
                    .next()
                    .expect("usage: driver ACTION PID [TEXT]")
                    .parse::<u32>()
                    .expect("PID must be an integer");
                let snapshot = provider.invoke(
                "desktop.linux_atspi.snapshot@1",
                json!({"application": {"process_id": process_id}, "max_depth": 8, "max_nodes": 96}),
                Some(Duration::from_secs(10)),
            )?;
                if command == "snapshot" {
                    Ok(snapshot)
                } else {
                    let (action, locator, extra) = if matches!(
                        command.as_str(),
                        "invoke" | "pointer-click" | "capture" | "toggle" | "expand" | "collapse"
                    ) {
                        let action = match command.as_str() {
                            "pointer-click" => "pointer_click",
                            "capture" => "capture_target",
                            "toggle" => "toggle",
                            "expand" => "expand",
                            "collapse" => "collapse",
                            _ => "invoke",
                        };
                        let extra = match command.as_str() {
                            "pointer-click" => json!({"button": "left", "position": "center"}),
                            "capture" => json!({"format": "png"}),
                            _ => json!({}),
                        };
                        let (role, name) = match action {
                            "toggle" => ("check_box", "Toggle fixture check button"),
                            "expand" | "collapse" => {
                                ("toggle_button", "Expand fixture details")
                            }
                            _ => ("push_button", "Invoke fixture button"),
                        };
                        (action, json!({"role": role, "name": name}), extra)
                    } else {
                        let text = arguments.next().unwrap_or_else(|| "Rust AT-SPI".into());
                        let action = if command == "type-text" { "type_text" } else { "set_text" };
                        let name = if command == "type-text" { "Fixture XTest text entry" } else { "Fixture text entry" };
                        (
                            action,
                            json!({"role": "text", "name": name}),
                            json!({"text": text}),
                        )
                    };
                    let found = provider.invoke(
                    "desktop.linux_atspi.find@1",
                    json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "locator": locator}),
                    Some(Duration::from_secs(5)),
                )?;
                    let mut request = json!({"target": found["target"], "locator": locator});
                    request
                        .as_object_mut()
                        .unwrap()
                        .extend(extra.as_object().unwrap().clone());
                    if action == "capture_target" {
                        let artifacts = aad_runtime::ArtifactStore::default();
                        provider.invoke_with_artifacts(
                            &format!("desktop.linux_atspi.{action}@1"),
                            request, Some(Duration::from_secs(10)), &artifacts,
                        )
                    } else {
                        provider.invoke(
                            &format!("desktop.linux_atspi.{action}@1"),
                            request, Some(Duration::from_secs(10)),
                        )
                    }
                }
            }
            _ => panic!("usage: driver [manifest|inspect|list|snapshot PID|invoke PID|set-text PID TEXT|type-text PID TEXT|pointer-click PID|capture PID|toggle PID|expand PID|collapse PID]"),
        }
    })();
    match result {
        Ok(value) => println!("{value}"),
        Err(error) => {
            println!("{}", error.to_json());
            std::process::exit(1);
        }
    }
}
