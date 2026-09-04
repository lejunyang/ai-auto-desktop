// 查两件事，都是 by_container 递归调 synthesize 带来的：
//   1. 深树上会不会栈溢出或慢到不可用（真实最深 21 层，但要看合成一次的耗时）
//   2. within 兜底之后，unresolved 到底降到多少 —— 这是这次改动的唯一价值指标
//
// 顺便看合成出来的 locator 长什么样，以及它们是否真的能唯一命中（unique_for 已经
// 保证，但要确认 within 在真实树上生效，而不是恰好属性就够了）。

use aad_uia::{native_driver, Locator, Node};
use serde_json::json;
use std::time::Instant;

fn interactive(node: &Node) -> bool {
    matches!(
        node.role.as_str(),
        "button" | "edit" | "check_box" | "radio_button" | "combo_box"
            | "list_item" | "menu_item" | "tab" | "hyperlink" | "tree_item"
    )
}

fn main() {
    let driver = native_driver().expect("a Windows driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("a list");

    let mut total = 0usize;
    let mut resolved_by_attributes = 0usize;
    let mut resolved_by_container = 0usize;
    let mut resolved_with_ordinal = 0usize;
    let mut still_unresolved = 0usize;
    let mut max_depth = 0usize;
    let mut slowest = (0u128, String::new());
    let mut samples: Vec<String> = Vec::new();

    for window in list.iter().take(16) {
        let window_id = window["window_id"].as_str().unwrap_or_default();
        let process = window["process_name"].as_str().unwrap_or_default();
        let Ok(captured) = driver.call("snapshot", &json!({"window_id": window_id})) else {
            continue;
        };
        let Some(raw) = captured["nodes"].as_array() else {
            continue;
        };
        let nodes: Vec<Node> = raw
            .iter()
            .filter_map(|value| serde_json::from_value(value.clone()).ok())
            .collect();
        if nodes.is_empty() {
            continue;
        }
        max_depth = max_depth.max(nodes.iter().map(|n| n.depth).max().unwrap_or(0) as usize);

        for node in nodes.iter().filter(|n| interactive(n)) {
            total += 1;
            let started = Instant::now();
            let synthesized = Locator::synthesize(node, &nodes);
            let elapsed = started.elapsed().as_micros();
            if elapsed > slowest.0 {
                slowest = (
                    elapsed,
                    format!(
                        "{process} {} (depth {})",
                        node.summary().chars().take(40).collect::<String>(),
                        node.depth
                    ),
                );
            }

            match synthesized {
                None => still_unresolved += 1,
                Some(locator) => {
                    let json_form = locator.to_json();
                    let has_within = json_form.get("within").is_some();
                    let has_nth = json_form.get("nth").is_some();
                    if !has_within {
                        resolved_by_attributes += 1;
                    } else if has_nth {
                        resolved_with_ordinal += 1;
                    } else {
                        resolved_by_container += 1;
                    }

                    // 只收集用到 within 的样本，那是新增能力
                    if has_within && samples.len() < 12 {
                        samples.push(format!(
                            "{}\n      {}",
                            node.summary().chars().take(52).collect::<String>(),
                            serde_json::to_string(&json_form)
                                .unwrap_or_default()
                                .chars()
                                .take(150)
                                .collect::<String>()
                        ));
                    }
                }
            }
        }
    }

    println!("=== 可交互元素 {total} ===");
    println!("  属性即可识别            {resolved_by_attributes:>4}  ({:.0}%)", pct(resolved_by_attributes, total));
    println!("  容器即可识别（新增）     {resolved_by_container:>4}  ({:.0}%)", pct(resolved_by_container, total));
    println!("  容器 + 序数（新增）      {resolved_with_ordinal:>4}  ({:.0}%)", pct(resolved_with_ordinal, total));
    println!("  仍然无法识别            {still_unresolved:>4}  ({:.0}%)", pct(still_unresolved, total));
    println!(
        "\nunresolved: 改动前 143 (16.3%) → 现在 {still_unresolved} ({:.1}%)",
        pct(still_unresolved, total)
    );

    println!("\n最深节点 depth {max_depth}");
    println!("最慢一次合成 {}µs  {}", slowest.0, slowest.1);

    println!("\n=== 用到 within 的样本 ===");
    for line in &samples {
        println!("  {line}");
    }
}

fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 }
}
