// (loose in the window) 里 group×28 仍是主体 —— 那些 group 有名字所以逃过筛选。
// 看它们叫什么：若名字有意义，它们该自成区域；若是重复噪音，说明还有一类要处理。

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let window = windows["windows"]
        .as_array()
        .expect("list")
        .iter()
        .find(|w| w["title"].as_str().unwrap_or_default().contains("one-sdk"))
        .expect("vscode");
    let window_id = window["window_id"].as_str().unwrap_or_default();
    let title = window["title"].as_str().unwrap_or_default();
    let captured = driver
        .call("snapshot", &json!({"window_id": window_id}))
        .expect("snap");
    let nodes: Vec<Node> = captured["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    let root = captured["root_id"].as_str().unwrap_or_default().to_string();
    let by_id: std::collections::HashMap<&str, &Node> =
        nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

    println!("=== 归到 loose 的 group，它们叫什么 ===");
    let mut shown = 0usize;
    for node in nodes
        .iter()
        .filter(|n| n.role == "group" && !n.actions.is_empty())
    {
        // 复现 region_of
        let mut current = node.parent_id.clone();
        let mut region = "(loose)".to_string();
        while let Some(id) = current {
            let Some(parent) = by_id.get(id.as_str()) else {
                break;
            };
            if parent.node_id == root {
                break;
            }
            if let Some(name) = parent.name.as_deref().filter(|t| !t.is_empty()) {
                let shared = name
                    .chars()
                    .zip(title.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
                let shorter = name.chars().count().min(title.chars().count());
                if !(name == title || (shorter >= 12 && shared * 100 >= shorter * 55)) {
                    region = name.to_string();
                    break;
                }
            }
            current = parent.parent_id.clone();
        }
        if region != "(loose)" {
            continue;
        }
        let children = nodes
            .iter()
            .filter(|n| n.parent_id.as_deref() == Some(&node.node_id))
            .count();
        println!(
            "  {} name={:?} 子节点={} depth={}",
            node.node_id,
            node.name
                .as_deref()
                .unwrap_or("(无名)")
                .chars()
                .take(30)
                .collect::<String>(),
            children,
            node.depth
        );
        shown += 1;
        if shown >= 14 {
            break;
        }
    }

    // 这些 group 自己是不是别人的区域名？
    println!("\n=== 若 group 自己成为区域，会多出多少区域 ===");
    let named_groups: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.role == "group" && !n.name.as_deref().unwrap_or("").is_empty())
        .collect();
    println!("  具名 group {} 个", named_groups.len());
    let mut names: std::collections::BTreeMap<&str, usize> = Default::default();
    for node in &named_groups {
        *names.entry(node.name.as_deref().unwrap_or("")).or_default() += 1;
    }
    println!("  不同名字 {} 个", names.len());
    let mut sorted: Vec<(&&str, &usize)> = names.iter().collect();
    sorted.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (name, count) in sorted.iter().take(8) {
        println!(
            "    {:<34} ×{}",
            name.chars().take(32).collect::<String>(),
            count
        );
    }
}
