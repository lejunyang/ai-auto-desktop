// 定位 989ms 花在哪一层。三个候选：
//   a. by_container 沿祖先链每层调 synthesize（递归，每层又是完整一轮属性尝试）
//   b. unique_for 每次都对全部节点跑 resolve
//   c. contain 每次重建 HashMap（1000 节点建一次表，被调用多次）
//
// 分别计时，不猜。

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

    // 只挑最大的那个窗口
    let mut biggest: Option<(String, Vec<Node>)> = None;
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
        if biggest.as_ref().is_none_or(|(_, existing)| nodes.len() > existing.len()) {
            biggest = Some((process.to_string(), nodes));
        }
    }

    let Some((process, nodes)) = biggest else {
        println!("没有窗口");
        return;
    };
    println!("最大窗口: {process}，{} 个节点\n", nodes.len());

    // 单独测 resolve 一次的成本（属性 locator vs 带 within 的）
    let plain = Locator::from_value(&json!({"role": "button"})).expect("valid");
    let started = Instant::now();
    for _ in 0..100 {
        let _ = plain.resolve(&nodes);
    }
    println!("resolve（纯属性）100 次: {}µs", started.elapsed().as_micros());

    let scoped = Locator::from_value(&json!({
        "role": "button",
        "within": {"role": "document"}
    }))
    .expect("valid");
    let started = Instant::now();
    for _ in 0..100 {
        let _ = scoped.resolve(&nodes);
    }
    println!("resolve（带 within）100 次: {}µs  ← 每次要建 HashMap", started.elapsed().as_micros());

    // 逐个元素测 synthesize，并按耗时排序看分布
    let mut timings: Vec<(u128, String, u32)> = Vec::new();
    for node in nodes.iter().filter(|n| interactive(n)) {
        let started = Instant::now();
        let result = Locator::synthesize(node, &nodes);
        timings.push((
            started.elapsed().as_micros(),
            format!(
                "{} {}",
                if result.as_ref().is_some_and(|l| l.to_json().get("within").is_some()) {
                    "within"
                } else if result.is_some() {
                    "attrs "
                } else {
                    "FAILED"
                },
                node.summary().chars().take(40).collect::<String>()
            ),
            node.depth,
        ));
    }
    timings.sort_by(|a, b| b.0.cmp(&a.0));

    let total: u128 = timings.iter().map(|(time, _, _)| time).sum();
    println!("\n合成 {} 个元素，共 {}ms", timings.len(), total / 1000);
    println!("中位 {}µs", timings[timings.len() / 2].0);

    println!("\n最慢的 10 个：");
    for (time, label, depth) in timings.iter().take(10) {
        println!("  {time:>7}µs  depth {depth:>2}  {label}");
    }

    println!("\n最快的 5 个（对照）：");
    for (time, label, depth) in timings.iter().rev().take(5) {
        println!("  {time:>7}µs  depth {depth:>2}  {label}");
    }

    // 用到 within 的 vs 没用到的，平均耗时
    let with_within: Vec<u128> = timings
        .iter()
        .filter(|(_, label, _)| label.starts_with("within"))
        .map(|(time, _, _)| *time)
        .collect();
    let without: Vec<u128> = timings
        .iter()
        .filter(|(_, label, _)| label.starts_with("attrs"))
        .map(|(time, _, _)| *time)
        .collect();
    let failed: Vec<u128> = timings
        .iter()
        .filter(|(_, label, _)| label.starts_with("FAILED"))
        .map(|(time, _, _)| *time)
        .collect();

    for (name, group) in [("属性即可", &without), ("用到 within", &with_within), ("失败", &failed)] {
        if group.is_empty() {
            continue;
        }
        let sum: u128 = group.iter().sum();
        println!(
            "\n{name}: {} 个，平均 {}µs，合计 {}ms",
            group.len(),
            sum / group.len() as u128,
            sum / 1000
        );
    }
}
