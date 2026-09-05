// 分叉点是可用的结构（377→50、131→35，具名的正是有意义的区域名），但前几名全是祖先链上
// 那串包住一切的无名 pane（366/362/355/350 是同一条链）—— 它们不是"区域"，是把整扇窗户
// 都算进去的外壳。
//
// 要一条判据把外壳和区域分开。两个候选：
//   A. 后代占比：超过窗口总量某个比例就是外壳
//   B. 具名 + 不是另一个候选区域的祖先
//
// A 需要拍一个阈值，B 是结构性的。先量 B 够不够用：只保留"具名分叉点里最外层的那些"
// （即它的祖先里没有别的具名分叉点），看剩下多少、覆盖多少可交互元素。
//
// 同时量：漏掉多少 —— 不在任何区域里的可交互元素有多少个。漏掉的元素 AI 永远看不到，
// 这比区域数量多几个严重得多。

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    let mut all_orphans = 0usize;
    let mut all_actionable = 0usize;
    let mut region_counts = Vec::new();

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
        let by_id: std::collections::HashMap<&str, &Node> =
            nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

        let child_count = |id: &str| nodes.iter().filter(|n| n.parent_id.as_deref() == Some(id)).count();
        let named = |node: &Node| node.name.as_ref().is_some_and(|t| !t.is_empty());

        // 候选：具名的分叉点
        let candidates: Vec<&Node> = nodes
            .iter()
            .filter(|n| child_count(&n.node_id) >= 2 && named(n) && n.states.offscreen != Some(true))
            .collect();
        let candidate_ids: std::collections::HashSet<&str> =
            candidates.iter().map(|n| n.node_id.as_str()).collect();

        // 判据 B：祖先里没有别的候选 —— 即最外层的具名分叉点
        // 但窗口自己（depth 0）总是候选且是所有人的祖先，所以从它的下一层算起
        let outermost: Vec<&&Node> = candidates
            .iter()
            .filter(|node| {
                if node.depth == 0 {
                    return false;
                }
                let mut current = node.parent_id.clone();
                while let Some(id) = current {
                    if candidate_ids.contains(id.as_str()) && by_id.get(id.as_str()).is_some_and(|n| n.depth > 0) {
                        return false;
                    }
                    current = by_id.get(id.as_str()).and_then(|n| n.parent_id.clone());
                }
                true
            })
            .collect();

        // 覆盖率：多少可交互元素落在某个 outermost 区域里
        let actionable: Vec<&Node> = nodes
            .iter()
            .filter(|n| !n.actions.is_empty() && n.states.offscreen != Some(true) && n.depth > 0)
            .collect();
        let mut covered = 0usize;
        for node in &actionable {
            let mut current = Some(node.node_id.clone());
            let mut found = false;
            while let Some(id) = current {
                if outermost.iter().any(|r| r.node_id == id) {
                    found = true;
                    break;
                }
                current = by_id.get(id.as_str()).and_then(|n| n.parent_id.clone());
            }
            if found {
                covered += 1;
            }
        }
        let orphans = actionable.len() - covered;
        all_orphans += orphans;
        all_actionable += actionable.len();
        region_counts.push(outermost.len());

        if nodes.len() > 100 {
            println!(
                "{:<44} {} 节点 → {} 个区域，覆盖 {}/{} 可交互（漏 {}）",
                title.chars().take(42).collect::<String>(),
                nodes.len(),
                outermost.len(),
                covered,
                actionable.len(),
                orphans
            );
        }
    }

    region_counts.sort_unstable();
    println!("\n=== 全机汇总 ===");
    if !region_counts.is_empty() {
        println!(
            "  每窗口区域数：中位 {}，最大 {}",
            region_counts[region_counts.len() / 2],
            region_counts[region_counts.len() - 1]
        );
    }
    println!(
        "  可交互元素 {} 个，不在任何区域里的 {} 个（{:.1}%）",
        all_actionable,
        all_orphans,
        100.0 * all_orphans as f64 / all_actionable.max(1) as f64
    );
}
