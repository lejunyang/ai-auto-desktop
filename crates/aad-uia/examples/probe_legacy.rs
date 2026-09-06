// Chromium 上每次交互都伴生一个 Chrome Legacy Window 的事件。量清楚它的形状，
// 再决定怎么判 —— 按名字硬编码「Chrome Legacy Window」会在换浏览器/换语言时失效。
//
// 要看的是：这个容器和真实目标在结构上差什么。

use aad_uia::{native_driver, Node};
use serde_json::json;

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

    println!("=== Chrome Legacy Window 这个节点 ===");
    for node in nodes.iter().filter(|n| {
        n.name.as_deref() == Some("Chrome Legacy Window")
            || n.automation_id.as_deref() == Some("739")
    }) {
        let children = nodes
            .iter()
            .filter(|other| other.parent_id.as_deref() == Some(node.node_id.as_str()))
            .count();
        println!(
            "  {} role={} name={:?} id={:?} depth={} 子节点={} actions={:?}",
            node.node_id,
            node.role,
            node.name,
            node.automation_id,
            node.depth,
            children,
            node.actions
        );
        println!("    states: {:?}", node.states);
        println!("    bounds: {:?}", node.bounds);
    }

    println!("\n=== 对照：一个真实的可交互目标 ===");
    for node in nodes.iter().filter(|n| {
        n.automation_id.as_deref() == Some("billing-city")
            || n.automation_id.as_deref() == Some("page-save")
    }) {
        let children = nodes
            .iter()
            .filter(|other| other.parent_id.as_deref() == Some(node.node_id.as_str()))
            .count();
        println!(
            "  {} role={} name={:?} id={:?} depth={} 子节点={} actions={:?}",
            node.node_id,
            node.role,
            node.name,
            node.automation_id,
            node.depth,
            children,
            node.actions
        );
        println!("    states: {:?}", node.states);
    }

    // 关键判据候选：容器有很多后代，叶子目标没有
    println!("\n=== 有多少节点是「有大量后代的 pane」 ===");
    let mut container_like = 0usize;
    let mut leaf_like = 0usize;
    for node in &nodes {
        let descendants = nodes
            .iter()
            .filter(|other| other.parent_id.as_deref() == Some(node.node_id.as_str()))
            .count();
        if descendants > 0 {
            container_like += 1;
        } else {
            leaf_like += 1;
        }
    }
    println!("  有子节点 {container_like}，无子节点 {leaf_like}");
}
