// 真相：那些 group 全都无名、且大多子节点=0 —— 它们是 VS Code 编辑器里的代码行/token，
// 不是控件。我的 worth_offering 按"无名+无子节点=只此一个"保留了它们。
//
// 所以 loose 的主体不是布局噪音，而是**文档内容**。这是另一类东西：一个 AI 想操作界面时，
// 编辑器里的每一行代码都不是"选项"。
//
// 但不能一律按 role=group 排除 —— 别的应用里 group 可能是真控件容器。
// 量三个可能的判据：
//   A. 在 document / edit 这类"内容宿主"的子树里
//   B. 无名 且 无子节点 且 role 属于结构性角色（group/text）
//   C. 同一个父节点下有大量同 role 无名兄弟（内容的特征是重复）
//
// 判据要满足：在 VS Code 里去掉代码行，在 complex fixture 里不去掉真控件。

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
        let by_id: std::collections::HashMap<&str, &Node> =
            nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

        println!("\n===== {} =====", title.chars().take(44).collect::<String>());

        let usable: Vec<&Node> = nodes
            .iter()
            .filter(|n| !n.actions.is_empty() && n.states.offscreen != Some(true))
            .collect();
        println!("可交互 {}", usable.len());

        // A. 在 document/edit 子树里
        let inside_content = |node: &Node| -> bool {
            let mut current = node.parent_id.clone();
            let mut hops = 0;
            while let Some(id) = current {
                if hops > 30 {
                    break;
                }
                hops += 1;
                let Some(parent) = by_id.get(id.as_str()) else { break };
                if matches!(parent.role.as_str(), "document" | "edit") {
                    return true;
                }
                current = parent.parent_id.clone();
            }
            false
        };
        let a = usable
            .iter()
            .filter(|n| inside_content(n) && n.name.as_deref().unwrap_or("").is_empty())
            .count();
        println!("  A. 在 document/edit 子树里且无名: {a}");

        // C. 同父下同 role 无名兄弟很多
        let mut sibling_groups: std::collections::HashMap<(String, String), usize> =
            Default::default();
        for node in &usable {
            if node.name.as_deref().unwrap_or("").is_empty() {
                let key = (
                    node.parent_id.clone().unwrap_or_default(),
                    node.role.clone(),
                );
                *sibling_groups.entry(key).or_default() += 1;
            }
        }
        let crowded: usize = sibling_groups.values().filter(|c| **c >= 5).sum();
        println!("  C. 同父同 role 无名兄弟 ≥5 的总数: {crowded}");

        // 交集与差集 —— 两个判据是否指向同一批
        let by_a: std::collections::HashSet<&str> = usable
            .iter()
            .filter(|n| inside_content(n) && n.name.as_deref().unwrap_or("").is_empty())
            .map(|n| n.node_id.as_str())
            .collect();
        let by_c: std::collections::HashSet<&str> = usable
            .iter()
            .filter(|n| {
                n.name.as_deref().unwrap_or("").is_empty()
                    && sibling_groups
                        .get(&(n.parent_id.clone().unwrap_or_default(), n.role.clone()))
                        .is_some_and(|c| *c >= 5)
            })
            .map(|n| n.node_id.as_str())
            .collect();
        println!(
            "  A∩C {}  仅A {}  仅C {}",
            by_a.intersection(&by_c).count(),
            by_a.difference(&by_c).count(),
            by_c.difference(&by_a).count()
        );

        // 关键检查：complex fixture 的真控件会不会被误伤
        for probe in ["billing-city", "page-save", "bare"] {
            if let Some(node) = nodes.iter().find(|n| n.automation_id.as_deref() == Some(probe)) {
                println!(
                    "  控件 {probe}: A={} C={}",
                    by_a.contains(node.node_id.as_str()),
                    by_c.contains(node.node_id.as_str())
                );
            }
        }
    }
}
