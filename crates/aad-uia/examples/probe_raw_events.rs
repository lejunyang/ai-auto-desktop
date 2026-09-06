// 直接看原始捕获事件里 Chrome Legacy Window 的 Node 长什么样。
// 步骤 JSON 不含 actions，所以只能从 watch/collect 层拿。

use aad_uia::native_driver;
use serde_json::json;
use std::thread::sleep;
use std::time::Duration;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");
    let window = list
        .iter()
        .find(|w| {
            w["title"]
                .as_str()
                .unwrap_or_default()
                .contains("Complex Fixture")
        })
        .expect("fixture");
    let window_id = window["window_id"].as_str().unwrap_or_default().to_string();

    // 找 billing-city
    let captured = driver
        .call("snapshot", &json!({"window_id": &window_id}))
        .expect("snap");
    let snapshot_id = captured["snapshot_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let revision = captured["revision"].as_u64().unwrap_or_default();
    let node_id = captured["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["automation_id"].as_str() == Some("billing-city"))
        .and_then(|n| n["node_id"].as_str())
        .expect("billing-city")
        .to_string();

    let session = driver
        .call("watch", &json!({"window_id": &window_id}))
        .expect("watch");
    let capture_id = session["capture_id"]
        .as_str()
        .expect("capture id")
        .to_string();
    println!("watching {capture_id}");

    sleep(Duration::from_millis(600));
    let target = format!("{snapshot_id}:{revision}:{node_id}");
    let applied = driver
        .call("set_value", &json!({"target": &target, "value": "Oslo"}))
        .expect("set_value");
    println!("set_value applied={:?}", applied["applied"]);

    sleep(Duration::from_millis(1500));
    let collected = driver
        .call("collect", &json!({"capture_id": &capture_id}))
        .expect("collect");
    driver
        .call("release", &json!({"capture_id": &capture_id}))
        .ok();

    let events = collected["events"].as_array().cloned().unwrap_or_default();
    println!("\n{} 个事件:", events.len());
    for event in &events {
        let node = &event["node"];
        println!(
            "\n  kind={:?} source={:?}",
            event["kind"].as_str(),
            event["source"].as_str()
        );
        if node.is_null() {
            println!("    node: null");
            continue;
        }
        println!(
            "    role={:?} name={:?} id={:?} actions={:?}",
            node["role"].as_str(),
            node["name"].as_str(),
            node["automation_id"].as_str(),
            node["actions"]
        );
        println!(
            "    children={:?} depth={:?}",
            node["children"], node["depth"]
        );
    }
}
