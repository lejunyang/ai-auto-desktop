// 减少 0% —— "只有 focus/pointer_click" 的假设完全不成立。
// 直接看那些 pane/group 到底带什么动作，别再猜。

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

    println!("=== 无名 pane / group 的动作集 ===");
    let mut shown = 0usize;
    for node in nodes.iter().filter(|n| {
        matches!(n.role.as_str(), "pane" | "group")
            && n.name.as_deref().unwrap_or("").is_empty()
            && !n.actions.is_empty()
    }) {
        let children = nodes
            .iter()
            .filter(|n| n.parent_id.as_deref() == Some(&node.node_id))
            .count();
        println!(
            "  {} {} 子节点={} actions={:?}",
            node.node_id, node.role, children, node.actions
        );
        shown += 1;
        if shown >= 10 {
            break;
        }
    }

    // 全窗口的动作集分布
    println!("\n=== 全窗口：不同动作集各有多少节点 ===");
    let mut sets: std::collections::BTreeMap<String, usize> = Default::default();
    for node in nodes.iter().filter(|n| !n.actions.is_empty()) {
        *sets.entry(node.actions.join("+")).or_default() += 1;
    }
    let mut sorted: Vec<(&String, &usize)> = sets.iter().collect();
    sorted.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    for (set, count) in sorted.iter().take(10) {
        println!("  {count:>4}  {set}");
    }

    // 真实问题：一个 AI 想点东西时，哪些节点是它真正的"选项"
    println!("\n=== 有名字 vs 无名字 ===");
    let actionable: Vec<&Node> = nodes.iter().filter(|n| !n.actions.is_empty()).collect();
    let named = actionable
        .iter()
        .filter(|n| !n.name.as_deref().unwrap_or("").is_empty())
        .count();
    println!("  可交互 {} 个，其中有名字 {}，无名 {}",
             actionable.len(), named, actionable.len() - named);
    let leaf_named = actionable
        .iter()
        .filter(|n| {
            !n.name.as_deref().unwrap_or("").is_empty()
                && !nodes.iter().any(|c| c.parent_id.as_deref() == Some(&n.node_id))
        })
        .count();
    println!("  其中无子节点且有名字（最像'一个控件'）: {leaf_named}");
}
