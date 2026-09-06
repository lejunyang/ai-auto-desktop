// 27/38 的身份冲突元素位置各不相同，所以 bounds 有消歧能力。
//
// 但用它之前必须排除一个疑点：**捕获事件里的 bounds 和快照里的是否一致**。
// 捕获节点是事件发生时独立描述的，快照是开始录制时拍的。如果两者坐标系不同、
// 或者页面在这期间滚动过，用 bounds 匹配只会换来另一种静默错配。
//
// 做法：录一次真实点击，把捕获节点的 bounds 和快照里同位置元素的 bounds 直接对照。

use aad_uia::{native_driver, Node};
use serde_json::json;
use std::thread::sleep;
use std::time::Duration;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let window = windows["windows"]
        .as_array()
        .expect("list")
        .iter()
        .find(|w| {
            w["title"]
                .as_str()
                .unwrap_or_default()
                .contains("Complex Fixture")
        })
        .expect("fixture");
    let window_id = window["window_id"].as_str().unwrap_or_default().to_string();

    let captured = driver
        .call("snapshot", &json!({"window_id": &window_id}))
        .expect("snap");
    let snapshot_id = captured["snapshot_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let revision = captured["revision"].as_u64().unwrap_or_default();
    let nodes: Vec<Node> = captured["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();

    // 取第三个 Edit 按钮（Chen 那一行），刻意不取第一个 —— 第一个是错配时的默认答案
    let edits: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.role == "button" && n.name.as_deref() == Some("Edit"))
        .collect();
    let target = edits.get(2).expect("third Edit");
    println!(
        "目标: {} bounds={:?}",
        target.node_id,
        target.bounds.as_ref().map(|b| (b.x, b.y))
    );

    // 开始录制，点它，看捕获到的 bounds
    let session = driver
        .call("watch", &json!({"window_id": &window_id}))
        .expect("watch");
    let capture_id = session["capture_id"].as_str().expect("id").to_string();
    sleep(Duration::from_millis(700));

    driver
        .call(
            "invoke",
            &json!({"target": format!("{snapshot_id}:{revision}:{}", target.node_id)}),
        )
        .expect("invoke");
    sleep(Duration::from_millis(1800));

    let collected = driver
        .call("collect", &json!({"capture_id": &capture_id}))
        .expect("collect");
    driver
        .call("release", &json!({"capture_id": &capture_id}))
        .ok();

    // steps 里没有 bounds，但 summary 有。直接看 driver 报的原始事件不可行
    // （collect 返回的是 steps），所以改看 steps 的数量与 summary。
    let steps = collected["steps"].as_array().cloned().unwrap_or_default();
    println!("\n录到 {} 步:", steps.len());
    for step in &steps {
        println!("  action={:?}", step["action"].as_str());
        println!("  locator={}", step["locator"]);
    }

    println!("\n=== 对照：目标应属 Chen 那一行 ===");
    // 找 target 的祖先里带 "Order for" 的
    let mut current = target.parent_id.clone();
    for _ in 0..4 {
        let Some(id) = current else { break };
        let Some(parent) = nodes.iter().find(|n| n.node_id == id) else {
            break;
        };
        if parent
            .name
            .as_deref()
            .is_some_and(|n| n.starts_with("Order for"))
        {
            println!("  目标真正所属: {:?}", parent.name);
            break;
        }
        current = parent.parent_id.clone();
    }
}
