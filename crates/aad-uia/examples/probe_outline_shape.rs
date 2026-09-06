// 读代码看到三个可疑点，先量清楚再改：
//
// 1. take(limit) 是**截断**不是取样 —— 截掉的是后面全部，而不是"不重要的"。
//    AI 拿到的是"窗口前 80 个可见元素"，底部的东西根本不知道存在。
// 2. truncated 的算法用 interesting.len() < nodes.len() —— 过滤掉不可交互元素也会让它
//    为 true，所以它无法区分"我筛掉了噪音"和"我砍掉了你要的东西"。
// 3. 完全平铺，depth 是唯一的层级线索。用户说的"按层级给出描述、想细看再下钻"目前不存在。
//
// 量四件事：真实窗口需要多少条、被截掉的是什么、扁平输出多大、有多少层级。

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    println!("=== 每个窗口：可交互/具名元素有多少，80 够不够 ===");
    let mut over_limit = 0usize;
    let mut totals = Vec::new();
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
        if nodes.is_empty() {
            continue;
        }
        let interesting = nodes
            .iter()
            .filter(|n| !n.actions.is_empty() || n.name.as_ref().is_some_and(|t| !t.is_empty()))
            .filter(|n| n.states.offscreen != Some(true))
            .count();
        totals.push(interesting);
        if interesting > 80 {
            over_limit += 1;
            println!(
                "  超限 {:>4} 条 (总 {:>4})  {}",
                interesting,
                nodes.len(),
                title.chars().take(42).collect::<String>()
            );
        }
    }
    totals.sort_unstable();
    println!(
        "\n  {} 个窗口，{} 个超过默认 limit=80",
        totals.len(),
        over_limit
    );
    if !totals.is_empty() {
        println!(
            "  可交互/具名元素数：中位 {}，最大 {}",
            totals[totals.len() / 2],
            totals[totals.len() - 1]
        );
    }

    // 拿最大的那个窗口细看：截断掉的是什么，输出多大
    println!("\n=== 最大的窗口：limit=80 砍掉了什么 ===");
    let mut biggest: Option<(usize, String, Vec<Node>)> = None;
    for window in list {
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
        let interesting: Vec<&Node> = nodes
            .iter()
            .filter(|n| !n.actions.is_empty() || n.name.as_ref().is_some_and(|t| !t.is_empty()))
            .filter(|n| n.states.offscreen != Some(true))
            .collect();
        let count = interesting.len();
        if biggest.as_ref().is_none_or(|(best, _, _)| count > *best) {
            biggest = Some((
                count,
                window["title"].as_str().unwrap_or_default().to_string(),
                nodes.clone(),
            ));
        }
    }

    if let Some((count, title, nodes)) = biggest {
        println!(
            "  {}（{} 条可见）",
            title.chars().take(46).collect::<String>(),
            count
        );
        let interesting: Vec<&Node> = nodes
            .iter()
            .filter(|n| !n.actions.is_empty() || n.name.as_ref().is_some_and(|t| !t.is_empty()))
            .filter(|n| n.states.offscreen != Some(true))
            .collect();

        println!("\n  第 81 条往后（被砍掉的）前 8 个:");
        for node in interesting.iter().skip(80).take(8) {
            println!(
                "    depth={} {} actions={}",
                node.depth,
                node.summary().chars().take(56).collect::<String>(),
                node.actions.len()
            );
        }

        // 输出体积
        let outline_80 = driver
            .call(
                "describe_window",
                &json!({"window_id": nodes.first().map(|_| "").unwrap_or("")}),
            )
            .ok();
        let _ = outline_80;

        println!("\n  层级分布（depth: 条数）:");
        let mut by_depth: std::collections::BTreeMap<u32, usize> = Default::default();
        for node in &interesting {
            *by_depth.entry(node.depth).or_default() += 1;
        }
        for (depth, n) in by_depth.iter().take(14) {
            println!("    {depth:>2}: {n}");
        }
        println!("  最深 {}", by_depth.keys().last().copied().unwrap_or(0));
    }
}
