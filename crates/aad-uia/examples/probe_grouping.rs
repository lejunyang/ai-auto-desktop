// 三次尝试全被数据否掉：
//   逐层下钻 —— 单链，直接子节点中位 1
//   最外层具名分叉点 —— 漏 41% 可交互元素
//   压缩树 —— 深度 15→15，因为无名 pane 也带 actions
//
// 共同错误：我一直在用"树结构"找分区，而数据反复说这棵树很深很瘦、中间层是渲染细节。
//
// 顶三层只有 4-5 个节点，而 depth 11-13 挤了 245 个 —— 结构不在层级里。
//
// 换个方向：AI 要的不是"架构图"，是**能按需取到相关元素**。那么分组该按什么？
// 三个候选，全部量一遍：
//   A. 按 role 分组（有多少 button / edit / list_item …）—— 无需树
//   B. 按最近的具名祖先分组（内容锚点已经证明这个可靠，且 by_container 已经在用）
//   C. 按屏幕位置分带（上/中/下）
//
// 判据：分组数要小（能一眼看完）、每组规模要可读、且必须 100% 覆盖。

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    let mut role_groups = Vec::new();
    let mut anchor_groups = Vec::new();
    let mut anchor_orphans = 0usize;
    let mut total = 0usize;

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

        let actionable: Vec<&Node> = nodes
            .iter()
            .filter(|n| !n.actions.is_empty() && n.states.offscreen != Some(true) && n.depth > 0)
            .collect();
        if actionable.is_empty() {
            continue;
        }
        total += actionable.len();

        // A. 按 role
        let mut roles: std::collections::HashMap<&str, usize> = Default::default();
        for node in &actionable {
            *roles.entry(node.role.as_str()).or_default() += 1;
        }
        role_groups.push(roles.len());

        // B. 按最近的具名祖先（跳过窗口自己）
        let mut anchors: std::collections::HashMap<String, usize> = Default::default();
        let mut orphan = 0usize;
        for node in &actionable {
            let mut current = node.parent_id.clone();
            let mut label = None;
            while let Some(id) = current {
                let Some(parent) = by_id.get(id.as_str()) else {
                    break;
                };
                if parent.depth == 0 {
                    break;
                }
                if parent.name.as_ref().is_some_and(|t| !t.is_empty()) {
                    label = Some(parent.name.clone().unwrap());
                    break;
                }
                current = parent.parent_id.clone();
            }
            match label {
                Some(name) => *anchors.entry(name).or_default() += 1,
                None => orphan += 1,
            }
        }
        anchor_groups.push(anchors.len());
        anchor_orphans += orphan;

        if nodes.len() > 100 {
            let mut sorted: Vec<(&String, &usize)> = anchors.iter().collect();
            sorted.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
            println!(
                "{:<38} {} 可交互 → role 分 {} 组 / 具名祖先分 {} 组（无祖先 {}）",
                title.chars().take(36).collect::<String>(),
                actionable.len(),
                roles.len(),
                anchors.len(),
                orphan
            );
            for (name, count) in sorted.iter().take(5) {
                println!(
                    "      {:<34} {}",
                    name.chars().take(32).collect::<String>(),
                    count
                );
            }
        }
    }

    role_groups.sort_unstable();
    anchor_groups.sort_unstable();
    println!("\n=== 汇总 ===");
    if !role_groups.is_empty() {
        println!(
            "  role 分组数：中位 {}，最大 {}",
            role_groups[role_groups.len() / 2],
            role_groups[role_groups.len() - 1]
        );
        println!(
            "  具名祖先分组数：中位 {}，最大 {}",
            anchor_groups[anchor_groups.len() / 2],
            anchor_groups[anchor_groups.len() - 1]
        );
    }
    println!(
        "  具名祖先方案漏掉：{}/{}（{:.1}%）",
        anchor_orphans,
        total,
        100.0 * anchor_orphans as f64 / total.max(1) as f64
    );
}
