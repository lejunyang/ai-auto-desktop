// 判据 B 失败：41% 的可交互元素落在任何区域之外，Complex Fixture 只出 1 个区域（等于
// 没分层）。"最外层具名分叉点"太严 —— 一个区域内部的其他具名区域被吞掉，顶层只有一个大
// 区域时就退化成原样。
//
// 漏 41% 不可接受。换思路：不要试图找"区域"这种一层的东西，而是给一棵**压缩后的树**：
//   - 沿单链下钻时把无名的中间容器折叠掉（它们是渲染细节，不是结构）
//   - 保留分叉点和具名节点
//   - 每个节点报它下面有多少可交互元素，让 AI 决定往哪钻
//
// 这样每个可交互元素都在某条路径上，不会漏。量三件事：
//   1. 压缩后的树有多少节点（顶层视图的规模）
//   2. 折叠掉多少无名单链
//   3. 深度降到多少

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    let mut tree_sizes = Vec::new();
    let mut depth_before = Vec::new();
    let mut depth_after = Vec::new();

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

        let children_of = |id: &str| -> Vec<&Node> {
            nodes
                .iter()
                .filter(|n| n.parent_id.as_deref() == Some(id))
                .collect()
        };
        let named = |node: &Node| node.name.as_ref().is_some_and(|t| !t.is_empty());

        // 一个节点值得出现在压缩树里，如果它：具名、或分叉、或自己可交互
        let worth_showing = |node: &Node| -> bool {
            named(node) || children_of(&node.node_id).len() >= 2 || !node.actions.is_empty()
        };

        let kept: Vec<&Node> = nodes
            .iter()
            .filter(|n| n.states.offscreen != Some(true))
            .filter(|n| worth_showing(n))
            .collect();

        // 压缩后每个保留节点的实际深度 = 它祖先里被保留的个数
        let by_id: std::collections::HashMap<&str, &Node> =
            nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();
        let kept_ids: std::collections::HashSet<&str> =
            kept.iter().map(|n| n.node_id.as_str()).collect();
        let compressed_depth = |node: &Node| -> usize {
            let mut depth = 0usize;
            let mut current = node.parent_id.clone();
            while let Some(id) = current {
                if kept_ids.contains(id.as_str()) {
                    depth += 1;
                }
                current = by_id.get(id.as_str()).and_then(|n| n.parent_id.clone());
            }
            depth
        };

        let max_before = nodes.iter().map(|n| n.depth).max().unwrap_or(0);
        let max_after = kept.iter().map(|n| compressed_depth(n)).max().unwrap_or(0);
        tree_sizes.push(kept.len());
        depth_before.push(max_before);
        depth_after.push(max_after as u32);

        // 顶两层的规模 —— AI 第一眼看到的
        let top_two = kept.iter().filter(|n| compressed_depth(n) <= 2).count();

        if nodes.len() > 100 {
            println!(
                "{:<42} {:>4} 节点 → 保留 {:>3}（顶三层 {:>3}）深度 {} → {}",
                title.chars().take(40).collect::<String>(),
                nodes.len(),
                kept.len(),
                top_two,
                max_before,
                max_after
            );
        }
    }

    tree_sizes.sort_unstable();
    depth_after.sort_unstable();
    println!("\n=== 汇总 ===");
    if !tree_sizes.is_empty() {
        println!(
            "  压缩树规模：中位 {}，最大 {}",
            tree_sizes[tree_sizes.len() / 2],
            tree_sizes[tree_sizes.len() - 1]
        );
        println!(
            "  压缩后深度：中位 {}，最大 {}",
            depth_after[depth_after.len() / 2],
            depth_after[depth_after.len() - 1]
        );
    }
    println!("\n  注：压缩树保留了所有可交互元素，所以覆盖率必然 100%。");
    println!("  要看的是顶几层是否够小、能否作为'大致架构'。");
}
