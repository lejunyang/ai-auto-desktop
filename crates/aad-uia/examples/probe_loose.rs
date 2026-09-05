// (loose in the window) 是 45/98 个元素的一大坨，下钻仍 12858/28454 字符 —— 它成了新的
// "什么都看不清"的地方。
//
// 但先量它是什么，再决定要不要拆。两种可能：
//   a. 真的是散落元素（没有具名容器包着），那就该按别的维度再分 —— role 是现成的
//   b. 有具名容器但被我的判据排除了（比如容器名被当成标题回声误杀）
//
// b 是缺陷，a 是设计选择。分辨方式：看这些元素的祖先链里到底有没有具名容器。

use aad_uia::{native_driver, Node};
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
        let root = captured["root_id"].as_str().unwrap_or_default().to_string();
        let by_id: std::collections::HashMap<&str, &Node> =
            nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

        println!("\n===== {} =====", title.chars().take(46).collect::<String>());

        // 复现 region_of 的逻辑，找出归到 loose 的元素
        let mut loose = Vec::new();
        for node in nodes.iter().filter(|n| {
            !n.actions.is_empty() && n.states.offscreen != Some(true) && n.node_id != root
        }) {
            let mut current = node.parent_id.clone();
            let mut assigned = false;
            let mut chain = Vec::new();
            while let Some(id) = current {
                let Some(parent) = by_id.get(id.as_str()) else { break };
                if parent.node_id == root {
                    break;
                }
                if let Some(name) = parent.name.as_deref().filter(|t| !t.is_empty()) {
                    chain.push(format!("{}:{}", parent.role, name.chars().take(26).collect::<String>()));
                    // 判据：共同前缀
                    let shared = name
                        .chars()
                        .zip(title.chars())
                        .take_while(|(a, b)| a == b)
                        .count();
                    let shorter = name.chars().count().min(title.chars().count());
                    let echo = name == title
                        || (shorter >= 12 && shared * 100 >= shorter * 55);
                    if !echo {
                        assigned = true;
                        break;
                    }
                } else {
                    chain.push(format!("{}:(无名)", parent.role));
                }
                current = parent.parent_id.clone();
            }
            if !assigned {
                loose.push((node, chain));
            }
        }

        println!("loose 元素 {} 个", loose.len());

        // 它们的祖先链里有具名容器吗
        let with_named = loose
            .iter()
            .filter(|(_, chain)| chain.iter().any(|entry| !entry.contains("(无名)")))
            .count();
        println!("  祖先链里有具名容器的: {}（那些名字被当成标题回声排除了）", with_named);
        println!("  祖先链全是无名容器的: {}（真正散落）", loose.len() - with_named);

        println!("\n  前 8 个 loose 元素的祖先链:");
        for (node, chain) in loose.iter().take(8) {
            println!(
                "    {:<34} ← {}",
                node.summary().chars().take(32).collect::<String>(),
                chain.iter().take(3).cloned().collect::<Vec<_>>().join(" ← ")
            );
        }

        // 若按 role 再分，loose 会拆成几组
        let mut roles: std::collections::BTreeMap<&str, usize> = Default::default();
        for (node, _) in &loose {
            *roles.entry(node.role.as_str()).or_default() += 1;
        }
        println!("\n  若按 role 拆 loose：{} 组", roles.len());
        let mut sorted: Vec<(&&str, &usize)> = roles.iter().collect();
        sorted.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        for (role, count) in sorted.iter().take(6) {
            println!("    {role:<14} {count}");
        }
    }
}
