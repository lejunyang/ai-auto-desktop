// 三个 Edit 按钮身份字段全同，same_element 的 find 总取第一个。
// 捕获节点带 bounds —— 那是位置，能区分三行。但要先确认：
//   1. 捕获事件里的 node 真的有 bounds 吗（有些来源可能没有）
//   2. 它和快照里同一元素的 bounds 一致吗（若窗口滚动过就不一致）
//
// 不确认就用 bounds 消歧，可能换来另一种静默错配。

use aad_uia::{native_driver, Node};
use serde_json::json;

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
    let window_id = window["window_id"].as_str().unwrap_or_default();
    let captured = driver
        .call("snapshot", &json!({"window_id": window_id}))
        .expect("snap");
    let nodes: Vec<Node> = captured["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();

    println!("=== 同名 Edit 按钮的 bounds ===");
    let edits: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.role == "button" && n.name.as_deref() == Some("Edit"))
        .collect();
    for node in &edits {
        match &node.bounds {
            Some(bounds) => println!(
                "  {} bounds=({}, {}) {}x{}",
                node.node_id, bounds.x, bounds.y, bounds.width, bounds.height
            ),
            None => println!("  {} bounds=None", node.node_id),
        }
    }

    // bounds 能否唯一区分它们
    let mut distinct = std::collections::HashSet::new();
    for node in &edits {
        if let Some(bounds) = &node.bounds {
            distinct.insert((bounds.x, bounds.y));
        }
    }
    println!(
        "\n  {} 个 Edit 按钮，{} 个不同位置",
        edits.len(),
        distinct.len()
    );

    // 全机统计：身份字段相同的元素有多少组，bounds 能救回多少
    println!("\n=== 全机：身份字段冲突的元素 ===");
    let mut groups: std::collections::HashMap<String, Vec<&Node>> =
        std::collections::HashMap::new();
    for node in &nodes {
        if node.actions.is_empty() {
            continue;
        }
        let key = format!(
            "{}|{:?}|{:?}|{:?}",
            node.role, node.name, node.automation_id, node.class_name
        );
        groups.entry(key).or_default().push(node);
    }
    let mut colliding = 0usize;
    let mut saved_by_bounds = 0usize;
    let mut unsaveable = 0usize;
    for (_, members) in groups.iter().filter(|(_, m)| m.len() > 1) {
        colliding += members.len();
        let positions: std::collections::HashSet<_> = members
            .iter()
            .filter_map(|n| n.bounds.as_ref().map(|b| (b.x, b.y)))
            .collect();
        if positions.len() == members.len() {
            saved_by_bounds += members.len();
        } else {
            unsaveable += members.len();
        }
    }
    println!("  身份字段与他人冲突的可交互元素: {colliding}");
    println!("  其中位置各不相同（bounds 可区分）: {saved_by_bounds}");
    println!("  位置也相同（无法区分）: {unsaveable}");
}
