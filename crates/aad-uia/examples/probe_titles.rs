// 标题里哪部分是稳定的？不猜，先量本机所有窗口的标题形状。
//
// 已知会变的部分：文档名+修改标记、页面标题、未读数、状态串。
// 常见分隔符：" - "、" | "、" — "、"： "
//
// 要回答的问题：如果只取标题的第一段（第一个分隔符之前），是否仍足以区分窗口？
// 如果第一段就重复很多，那这条路走不通。

use aad_uia::native_driver;
use serde_json::json;
use std::collections::HashMap;

const SEPARATORS: [&str; 5] = [" - ", " | ", " — ", " – ", ": "];

/// The part before the first separator: what a user would call the application.
fn head(title: &str) -> &str {
    let mut cut = title.len();
    for separator in SEPARATORS {
        if let Some(at) = title.find(separator) {
            cut = cut.min(at);
        }
    }
    title[..cut].trim()
}

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    println!("{} 个窗口\n", list.len());

    let mut heads: HashMap<String, Vec<String>> = HashMap::new();
    let mut segmented = 0usize;

    for window in list {
        let title = window["title"].as_str().unwrap_or_default();
        let process = window["process_name"].as_str().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let first = head(title);
        if first.len() < title.trim().len() {
            segmented += 1;
        }
        heads
            .entry(format!("{process}|{first}"))
            .or_default()
            .push(title.to_string());
    }

    println!("=== 有分隔符的标题（第一段 != 全部）: {segmented} ===\n");

    println!("=== 按 process + 第一段分组 ===");
    let mut groups: Vec<_> = heads.iter().collect();
    groups.sort_by_key(|(key, _)| key.to_string());
    let mut unique_by_head = 0usize;
    let mut collides = 0usize;
    for (key, titles) in groups {
        let parts: Vec<&str> = key.split('|').collect();
        if titles.len() == 1 {
            unique_by_head += 1;
        } else {
            collides += 1;
        }
        println!(
            "  [{}] {} :: {:?}",
            titles.len(),
            parts.get(1).unwrap_or(&""),
            titles
                .iter()
                .map(|t| if t.chars().count() > 52 {
                    t.chars().take(52).collect::<String>() + "…"
                } else {
                    t.clone()
                })
                .collect::<Vec<_>>()
        );
    }

    println!("\n=== 判定 ===");
    println!("  第一段就唯一的分组: {unique_by_head}");
    println!("  第一段相同、需要更多信息的分组: {collides}");
}
