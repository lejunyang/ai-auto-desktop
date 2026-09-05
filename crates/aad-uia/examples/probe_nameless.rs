// invoke 是普遍存在的（131 个里 124 个都有），不能作为"这是个可点控件"的判据 ——
// 我先前把它当专属动作了，所以上一轮减少 0%。
//
// 但"有名字 90 / 无名 41"指向真判据：AI 要选一个元素，必须能**称呼**它。一个无名且有
// 子节点的 pane，AI 既没法称呼它、也不需要点它（它的子节点就在列表里）。
//
// 量这条：去掉"无名 且 有子节点"的节点后规模变多少，以及会不会让某处变得无法触达。

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    let mut before = 0usize;
    let mut after = 0usize;
    let mut unreachable_total = 0usize;

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

        let has_children =
            |id: &str| nodes.iter().any(|n| n.parent_id.as_deref() == Some(id));
        let nameless = |node: &Node| node.name.as_deref().unwrap_or("").is_empty();

        let interactive: Vec<&Node> = nodes
            .iter()
            .filter(|n| {
                !n.actions.is_empty() && n.states.offscreen != Some(true) && n.node_id != root
            })
            .collect();
        let kept: Vec<&&Node> = interactive
            .iter()
            .filter(|n| !(nameless(n) && has_children(&n.node_id)))
            .collect();

        // 触达性：被去掉的节点里，有没有"它自己无名但它的子树里也全无名"的
        // —— 那种去掉后确实少了一条路径
        let dropped: Vec<&&Node> = interactive
            .iter()
            .filter(|n| nameless(n) && has_children(&n.node_id))
            .collect();
        let mut orphaned = 0usize;
        for node in &dropped {
            // 它的子树里有没有留下来的
            let mut stack = vec![node.node_id.clone()];
            let mut survives = false;
            while let Some(id) = stack.pop() {
                for child in nodes.iter().filter(|n| n.parent_id.as_deref() == Some(&id)) {
                    if kept.iter().any(|k| k.node_id == child.node_id) {
                        survives = true;
                        break;
                    }
                    stack.push(child.node_id.clone());
                }
                if survives {
                    break;
                }
            }
            if !survives {
                orphaned += 1;
            }
        }

        before += interactive.len();
        after += kept.len();
        unreachable_total += orphaned;

        if nodes.len() > 100 {
            println!(
                "{:<38} {} → {}（去掉 {}，其中子树全空的 {}）",
                title.chars().take(36).collect::<String>(),
                interactive.len(),
                kept.len(),
                dropped.len(),
                orphaned
            );
        }
    }

    println!("\n=== 汇总 ===");
    println!(
        "  可交互 {} → {}（减少 {:.0}%）",
        before,
        after,
        100.0 * (before - after) as f64 / before.max(1) as f64
    );
    println!(
        "  去掉后子树里没有任何留存元素的容器: {}（这些是真的少了一条路径）",
        unreachable_total
    );
}
