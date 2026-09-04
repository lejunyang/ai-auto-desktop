// 两个问题要分别量：
//   1. class_name 是整串 Tailwind CSS —— durable() 没拦住。要看这种值有多长、
//      多常见，以及它到底稳不稳定（长不等于不稳，但 CSS 类名随样式改动而变）。
//   2. 合成耗时 45.8ms。要定位是递归深度还是 resolve 的全量扫描。

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

    println!("=== class_name 的长度分布 ===");
    let mut lengths: Vec<(usize, String, String)> = Vec::new();
    let mut per_window_timing: Vec<(String, usize, u128, u128)> = Vec::new();

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

        for node in &nodes {
            if let Some(class) = node.class_name.as_deref() {
                if !class.is_empty() {
                    lengths.push((
                        class.chars().count(),
                        process.to_string(),
                        class.chars().take(48).collect(),
                    ));
                }
            }
        }

        // 该窗口内合成的总耗时与最慢一次
        let started = Instant::now();
        let mut worst = 0u128;
        let interactive_nodes: Vec<&Node> = nodes.iter().filter(|n| interactive(n)).collect();
        for node in &interactive_nodes {
            let one = Instant::now();
            let _ = Locator::synthesize(node, &nodes);
            worst = worst.max(one.elapsed().as_micros());
        }
        if !interactive_nodes.is_empty() {
            per_window_timing.push((
                format!("{process} ({} nodes)", nodes.len()),
                interactive_nodes.len(),
                started.elapsed().as_millis(),
                worst,
            ));
        }
    }

    lengths.sort_by(|a, b| b.0.cmp(&a.0));
    println!("class_name 非空的节点 {} 个", lengths.len());
    if !lengths.is_empty() {
        let median = lengths[lengths.len() / 2].0;
        let over_60 = lengths.iter().filter(|(len, _, _)| *len > 60).count();
        let over_120 = lengths.iter().filter(|(len, _, _)| *len > 120).count();
        println!("  长度中位 {median}，>60 字符 {over_60} 个，>120 字符 {over_120} 个");
        println!("\n  最长的 6 个：");
        for (length, process, sample) in lengths.iter().take(6) {
            println!("    {length:>5} 字符  {process:<14} {sample}");
        }
        println!("\n  正常长度的样本（中位附近）：");
        let middle = lengths.len() / 2;
        for (length, process, sample) in lengths.iter().skip(middle).take(5) {
            println!("    {length:>5} 字符  {process:<14} {sample}");
        }
    }

    println!("\n=== 合成耗时（每窗口）===");
    per_window_timing.sort_by(|a, b| b.2.cmp(&a.2));
    for (label, count, total_ms, worst_us) in &per_window_timing {
        println!(
            "  {total_ms:>6}ms 合成 {count:>4} 个，最慢一次 {:>7}µs   {label}",
            worst_us
        );
    }
}
