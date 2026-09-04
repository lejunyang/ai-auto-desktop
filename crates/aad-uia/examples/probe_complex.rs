// 量这个页面上合成出的 locator 长什么样，以及能否唯一命中。
//
// 重点看 Edit/Delete 各 5 个的那些行：无 automation_id、同名同 role，只有所在行不同。
// 这是 within 该救的形状，也是电商/后台最常见的形状。

use aad_uia::{native_driver, Locator, Node};
use serde_json::json;

fn interactive(node: &Node) -> bool {
    matches!(
        node.role.as_str(),
        "button" | "edit" | "check_box" | "radio_button" | "combo_box"
            | "list_item" | "menu_item" | "tab" | "hyperlink" | "tree_item" | "data_item"
    )
}

fn main() {
    let driver = native_driver().expect("a Windows driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("a list");
    let target = list.iter().find(|window| {
        window["title"].as_str().unwrap_or_default().contains("Complex Fixture")
    });
    let Some(window) = target else {
        println!("找不到 Complex Fixture 窗口");
        return;
    };

    let window_id = window["window_id"].as_str().unwrap_or_default();
    let captured = driver
        .call("snapshot", &json!({"window_id": window_id}))
        .expect("a snapshot");
    let nodes: Vec<Node> = captured["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect();

    println!("{} 个节点\n", nodes.len());

    // 先看页面上那些重名按钮的原始形状
    println!("=== Edit / Delete 的原始节点 ===");
    for node in nodes.iter().filter(|n| {
        n.role == "button" && matches!(n.name.as_deref(), Some("Edit") | Some("Delete"))
    }) {
        let parent = node
            .parent_id
            .as_deref()
            .and_then(|id| nodes.iter().find(|other| other.node_id == id));
        println!(
            "  {:<5} {:<8} automation_id={:<8?} depth={:<3} 父={:?}/{:?}",
            node.node_id,
            node.name.as_deref().unwrap_or(""),
            node.automation_id.as_deref(),
            node.depth,
            parent.map(|p| p.role.as_str()),
            parent.and_then(|p| p.name.as_deref()),
        );
    }

    // 合成并检查
    let mut resolved = 0usize;
    let mut with_container = 0usize;
    let mut failed: Vec<&Node> = Vec::new();
    let mut samples: Vec<String> = Vec::new();

    for node in nodes.iter().filter(|n| interactive(n)) {
        match Locator::synthesize(node, &nodes) {
            None => failed.push(node),
            Some(locator) => {
                resolved += 1;
                let rendered = locator.to_json();
                if rendered.get("within").is_some() {
                    with_container += 1;
                }
                // 收集页面上那些重名按钮的样本
                if matches!(node.name.as_deref(), Some("Edit") | Some("Delete") | Some("Apply") | Some("Save"))
                    && samples.len() < 10
                {
                    let hits = locator.resolve(&nodes);
                    samples.push(format!(
                        "{:<5} {:<8} → {}  [命中 {} 个{}]",
                        node.node_id,
                        node.name.as_deref().unwrap_or(""),
                        serde_json::to_string(&rendered).unwrap_or_default(),
                        hits.len(),
                        if hits.len() == 1 && hits[0].node_id == node.node_id {
                            ", 正确"
                        } else {
                            ", ✗"
                        }
                    ));
                }
            }
        }
    }

    let total = resolved + failed.len();
    println!("\n=== 合成结果 ===");
    println!("  可交互元素 {total}");
    println!("  成功 {resolved}（其中用到容器 {with_container}）");
    println!("  失败 {}", failed.len());

    println!("\n=== 重名按钮的合成样本 ===");
    for line in &samples {
        println!("  {line}");
    }

    println!("\n=== 失败的元素是什么形状 ===");
    for node in failed.iter().take(20) {
        let ancestry: Vec<String> = {
            let mut chain = Vec::new();
            let mut current = node.parent_id.as_deref();
            let mut budget = 8;
            while let Some(id) = current {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let Some(ancestor) = nodes.iter().find(|other| other.node_id == id) else {
                    break;
                };
                chain.push(format!(
                    "{}{}",
                    ancestor.role,
                    ancestor
                        .name
                        .as_deref()
                        .map(|n| format!("({n})"))
                        .unwrap_or_default()
                ));
                current = ancestor.parent_id.as_deref();
            }
            chain
        };
        // 同 role 兄弟数
        let siblings = nodes
            .iter()
            .filter(|other| other.role == node.role && other.parent_id == node.parent_id)
            .count();
        println!(
            "  {:<5} {:<10} name={:<20?} id={:?} 兄弟{} 祖先链: {}",
            node.node_id,
            node.role,
            node.name.as_deref(),
            node.automation_id.as_deref(),
            siblings,
            ancestry.join(" < ")
        );
    }
}
