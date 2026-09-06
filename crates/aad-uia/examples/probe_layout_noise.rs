// 上一轮看清两件事：
//   a. loose 里 43/45、96/98 的祖先链确实有具名容器，但那容器就是标题回声本身 ——
//      不是误杀，它们真的只被标题包着。这是真散落。
//   b. 更重要：loose 里大量是 pane 16 / group 34 这类**容器自身**。它们带 actions
//      （focus/pointer_click）所以算"可交互"，但没人要去点一个布局 pane。
//
// 若 b 成立，那 outline 和 overview 报的"可交互元素"数量本身就虚高，而这会同时让
// 概览和下钻都显得比实际更满。
//
// 量：可交互元素里有多少只有 focus/pointer_click 这类"任何东西都有"的动作，
// 且自己有子节点（说明是容器不是控件）。这些去掉后规模变多少。

use aad_uia::{native_driver, Node};
use serde_json::json;

/// 只有这些动作，说明平台没有为它提供任何专属操作。
const GENERIC: [&str; 2] = ["focus", "pointer_click"];

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    let mut before_total = 0usize;
    let mut after_total = 0usize;

    for window in list {
        let window_id = window["window_id"].as_str().unwrap_or_default();
        let title = window["title"].as_str().unwrap_or_default();
        let Ok(captured) = driver.call("snapshot", &json!({"window_id": window_id})) else {
            continue;
        };
        let nodes: Vec<Node> = captured["nodes"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        if nodes.len() < 10 {
            continue;
        }
        let root = captured["root_id"].as_str().unwrap_or_default().to_string();

        let has_children = |node: &Node| {
            nodes
                .iter()
                .any(|n| n.parent_id.as_deref() == Some(&node.node_id))
        };
        let only_generic = |node: &Node| {
            !node.actions.is_empty() && node.actions.iter().all(|a| GENERIC.contains(&a.as_str()))
        };

        let interactive: Vec<&Node> = nodes
            .iter()
            .filter(|n| {
                !n.actions.is_empty() && n.states.offscreen != Some(true) && n.node_id != root
            })
            .collect();
        // 提议的收紧：有子节点 且 只有通用动作 且 无名 → 是布局容器，不是选项
        let kept: Vec<&&Node> = interactive
            .iter()
            .filter(|n| {
                !(has_children(n) && only_generic(n) && n.name.as_deref().unwrap_or("").is_empty())
            })
            .collect();

        before_total += interactive.len();
        after_total += kept.len();

        if nodes.len() > 100 {
            // 被去掉的都是什么
            let dropped: Vec<&&Node> = interactive
                .iter()
                .filter(|n| {
                    has_children(n) && only_generic(n) && n.name.as_deref().unwrap_or("").is_empty()
                })
                .collect();
            let mut roles: std::collections::BTreeMap<&str, usize> = Default::default();
            for node in &dropped {
                *roles.entry(node.role.as_str()).or_default() += 1;
            }
            let mut sorted: Vec<(&&str, &usize)> = roles.iter().collect();
            sorted.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
            println!(
                "{:<38} {} → {}  去掉 {}: {}",
                title.chars().take(36).collect::<String>(),
                interactive.len(),
                kept.len(),
                dropped.len(),
                sorted
                    .iter()
                    .take(4)
                    .map(|(r, c)| format!("{r}×{c}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }

    println!("\n=== 汇总 ===");
    println!(
        "  可交互元素 {} → {}（减少 {:.0}%）",
        before_total,
        after_total,
        100.0 * (before_total - after_total) as f64 / before_total.max(1) as f64
    );
    println!("\n  注意：若某个无名容器是唯一能点到某处的方式，去掉它会让那里无法触达。");
    println!("  所以只去掉'有子节点'的 —— 它的子节点仍在列表里，触达性不变。");
}
