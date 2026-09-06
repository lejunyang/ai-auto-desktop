// 上一轮的数据推翻了「按 parent_id 逐层下钻」：顶层容器 173 个（比 limit 还多），
// 前 10 个全是单链（pane→pane→pane 串 9 层无名容器），直接子节点数中位 1。
// 「下钻一层」大多只多看到一个元素 —— 这不是给人用的结构。
//
// 单链是渲染框架的实现细节，不是界面的组织方式。真正的分区在**有多个子节点的那些节点**上。
//
// 量：若只保留「有 ≥2 个子节点」的容器（真正的分叉点），有多少个？它们有名字吗？
// 每个分叉点下有多少可交互元素？这才是「大致架构」该有的形状。

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    for window in list {
        let title = window["title"].as_str().unwrap_or_default();
        if !title.contains("one-sdk") && !title.contains("Complex Fixture") {
            continue;
        }
        let window_id = window["window_id"].as_str().unwrap_or_default();
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
        if nodes.is_empty() {
            continue;
        }

        println!(
            "\n===== {} =====",
            title.chars().take(48).collect::<String>()
        );
        println!("{} 节点", nodes.len());

        let children_of = |id: &str| -> Vec<&Node> {
            nodes
                .iter()
                .filter(|n| n.parent_id.as_deref() == Some(id))
                .collect()
        };
        let descendants = |id: &str| -> usize {
            let mut stack = vec![id.to_string()];
            let mut total = 0usize;
            while let Some(current) = stack.pop() {
                for child in children_of(&current) {
                    total += 1;
                    stack.push(child.node_id.clone());
                }
            }
            total
        };
        let actionable_under = |id: &str| -> usize {
            let mut stack = vec![id.to_string()];
            let mut total = 0usize;
            while let Some(current) = stack.pop() {
                for child in children_of(&current) {
                    if !child.actions.is_empty() && child.states.offscreen != Some(true) {
                        total += 1;
                    }
                    stack.push(child.node_id.clone());
                }
            }
            total
        };

        // 真正的分叉点：有 ≥2 个子节点
        let forks: Vec<&Node> = nodes
            .iter()
            .filter(|n| children_of(&n.node_id).len() >= 2)
            .filter(|n| n.states.offscreen != Some(true))
            .collect();
        let named_forks = forks
            .iter()
            .filter(|n| n.name.as_ref().is_some_and(|t| !t.is_empty()))
            .count();
        println!(
            "  分叉点（≥2 子节点）{} 个，有名字的 {}",
            forks.len(),
            named_forks
        );

        // 分叉点里再挑「值得作为一个区域」的：后代里有若干可交互元素
        let mut regions: Vec<(usize, &Node)> = forks
            .iter()
            .map(|node| (actionable_under(&node.node_id), *node))
            .filter(|(count, _)| *count >= 2)
            .collect();
        regions.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
        println!("  其中后代含 ≥2 个可交互元素的: {}", regions.len());

        println!("\n  前 12 个区域（可交互后代 / 全部后代 / depth / role / 名字）:");
        for (count, node) in regions.iter().take(12) {
            let label = node.name.as_deref().unwrap_or("(无名)");
            println!(
                "    {count:>4} / {:>4} / d{:<3} {:<12} {}",
                descendants(&node.node_id),
                node.depth,
                node.role,
                label.chars().take(36).collect::<String>()
            );
        }

        // 最有用的那层：跳过单链，只报「分叉点」构成的树
        println!("\n  若顶层只报最外 8 个具名分叉点:");
        let mut shown = 0usize;
        for (count, node) in regions.iter() {
            if node.name.as_ref().is_none_or(|t| t.is_empty()) {
                continue;
            }
            // 只要不是别人的祖先链上那种包住一切的
            if *count as f64 > 0.9 * (actionable_under(&nodes[0].node_id) as f64) {
                continue;
            }
            println!(
                "    {:<38} {} 个可交互元素",
                node.name
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(36)
                    .collect::<String>(),
                count
            );
            shown += 1;
            if shown >= 8 {
                break;
            }
        }
    }
}
