// 位置性 id 的判据该怎么定：`row-0` 是槽位，`save-1` 是名字，两者形状相同。
// 先看真机上带尾号的 id 到底长什么样，别凭直觉划线。

use aad_uia::{native_driver, Node};
use serde_json::json;
use std::collections::HashMap;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    // stem -> 该 stem 下出现过的完整 id
    let mut families: HashMap<String, Vec<String>> = HashMap::new();
    let mut plain: Vec<String> = Vec::new();

    for window in list.iter().take(20) {
        let window_id = window["window_id"].as_str().unwrap_or_default();
        let Ok(captured) = driver.call("snapshot", &json!({"window_id": window_id})) else {
            continue;
        };
        let Some(raw) = captured["nodes"].as_array() else {
            continue;
        };
        let nodes: Vec<Node> = raw
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        for node in &nodes {
            let Some(id) = node.automation_id.as_deref() else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            let stem = id.trim_end_matches(|c: char| c.is_ascii_digit());
            if stem.len() == id.len() {
                plain.push(id.to_string());
                continue;
            }
            let key = stem.trim_end_matches(['-', '_']).to_string();
            families.entry(key).or_default().push(id.to_string());
        }
    }

    println!("=== 带尾号的 id，按 stem 分组 ===");
    let mut groups: Vec<(String, Vec<String>)> = families.into_iter().collect();
    groups.sort_by_key(|(_, ids)| std::cmp::Reverse(ids.len()));
    for (stem, mut ids) in groups.into_iter().take(28) {
        ids.sort();
        ids.dedup();
        // 关键判据候选：同一 stem 是否出现多个不同编号
        let siblings = ids.len();
        let sample: Vec<&str> = ids.iter().take(5).map(String::as_str).collect();
        println!(
            "  stem={:<22} 不同编号 {:<4} {}{}",
            stem,
            siblings,
            sample.join(", "),
            if ids.len() > 5 { ", …" } else { "" }
        );
    }

    println!("\n=== 不带尾号的 id（对照，前 20）===");
    plain.sort();
    plain.dedup();
    for id in plain.iter().take(20) {
        println!("  {id}");
    }
    println!("  共 {} 个不同的无尾号 id", plain.len());
}
