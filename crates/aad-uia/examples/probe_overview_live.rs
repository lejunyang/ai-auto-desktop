// 先真机看概览有没有实用价值，再决定接 CLI/MCP 的形状。
// 光看单测过不了这一关 —— 概览的价值全在"名字是否说得通、规模是否合理"。

use aad_uia::native_driver;
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    for window in list {
        let title = window["title"].as_str().unwrap_or_default();
        if !title.contains("one-sdk") && !title.contains("Complex Fixture") {
            continue;
        }
        let window_id = window["window_id"].as_str().unwrap_or_default();
        let Ok(overview) = driver.call("overview", &json!({"window_id": window_id})) else {
            continue;
        };

        println!("\n===== {} =====", title.chars().take(50).collect::<String>());
        println!(
            "{} 节点，{} 可交互",
            overview["node_count"], overview["interactive"]
        );
        let text = overview.to_string();
        println!("概览输出 {} 字符（对比 outline limit=80 的约 26000）", text.len());

        println!("\n区域:");
        for region in overview["regions"].as_array().unwrap_or(&vec![]) {
            let holds: Vec<String> = region["holds"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .map(|entry| {
                    format!(
                        "{}×{}",
                        entry["role"].as_str().unwrap_or(""),
                        entry["count"]
                    )
                })
                .collect();
            println!(
                "  {:<38} {:>3} 个  {}",
                region["region"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(36)
                    .collect::<String>(),
                region["elements"],
                holds.join(" ")
            );
        }
        println!(
            "  折叠 {} 个区域，共 {} 个元素",
            overview["folded_regions"], overview["folded_elements"]
        );

        // 下钻一个区域
        let first = overview["regions"][0]["region"].as_str().unwrap_or("");
        if !first.is_empty() {
            match driver.call(
                "describe",
                &json!({"window_id": window_id, "region": first, "limit": 200}),
            ) {
                Ok(drilled) => println!(
                    "\n下钻 {:?}: shown={} matched={} truncated={} 输出 {} 字符",
                    first.chars().take(30).collect::<String>(),
                    drilled["shown"],
                    drilled["matched"],
                    drilled["truncated"],
                    drilled.to_string().len()
                ),
                Err(error) => println!("\n下钻失败: {}", error.code),
            }
        }

        // 错的区域名
        match driver.call(
            "describe",
            &json!({"window_id": window_id, "region": "No Such Region"}),
        ) {
            Ok(_) => println!("错区域名: 竟然成功了（不该）"),
            Err(error) => println!(
                "错区域名: {} — {}",
                error.code,
                error.message.chars().take(60).collect::<String>()
            ),
        }
    }
}
