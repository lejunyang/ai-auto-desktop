// 让程序自己说 by_container 在每一层看到了什么，别靠读代码推理。
use aad_uia::{native_driver, Locator, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");
    let window = list
        .iter()
        .find(|w| w["title"].as_str().unwrap_or_default().contains("Complex Fixture"))
        .expect("fixture");
    let window_id = window["window_id"].as_str().unwrap_or_default();
    let captured = driver.call("snapshot", &json!({"window_id": window_id})).expect("snap");
    let nodes: Vec<Node> = captured["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    let by_id: std::collections::HashMap<&str, &Node> =
        nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

    // 找第一个 Edit 按钮，沿祖先链逐层报告
    let edit = nodes
        .iter()
        .find(|n| n.role == "button" && n.name.as_deref() == Some("Edit"))
        .expect("an Edit button");
    println!("目标 {} {:?}\n", edit.node_id, edit.name);

    let mut current = edit.parent_id.as_deref();
    let mut level = 0;
    while let Some(id) = current {
        level += 1;
        if level > 8 {
            break;
        }
        let Some(ancestor) = by_id.get(id) else { break };
        let synthesized = Locator::synthesize(ancestor, &nodes);
        println!(
            "第{level}层祖先 {} role={} name={:?} id={:?}",
            ancestor.node_id, ancestor.role, ancestor.name, ancestor.automation_id
        );
        match &synthesized {
            None => println!("    synthesize → None"),
            Some(locator) => {
                let rendered = serde_json::to_string(&locator.to_json()).unwrap_or_default();
                println!("    synthesize → {rendered}");
            }
        }
        current = ancestor.parent_id.as_deref();
    }

    println!("\n最终合成:");
    if let Some(locator) = Locator::synthesize(edit, &nodes) {
        println!("  {}", serde_json::to_string(&locator.to_json()).unwrap_or_default());
    }
}
