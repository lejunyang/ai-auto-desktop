// 量两件事，它们决定 within 的下一步该做什么：
//
//  1. 合成出的 locator 里，有多少把「位置性容器」当成了身份？
//     实测已证明 within:{automation_id:"row-0"} 会在插入一行后指向另一个对象 ——
//     语法有效、唯一命中、零报错。这比 unresolved 危险，因为它会安静地写错对象。
//     要区分：容器是「这一行」（位置）还是「Billing 面板」（身份）。
//
//  2. 仍然 unresolved 的元素，形状到底是什么？
//     之前统计的 26% 是按 MAX_COUNTABLE_SIBLINGS=10 截断的，要看它们真实的样子。

use aad_uia::{native_driver, Locator, Node};
use serde_json::{json, Value};

fn interactive(node: &Node) -> bool {
    matches!(
        node.role.as_str(),
        "button" | "edit" | "check_box" | "radio_button" | "combo_box"
            | "list_item" | "menu_item" | "tab" | "hyperlink" | "tree_item" | "data_item"
    )
}

/// 容器是否靠「排第几」来识别，而不是靠身份。
///
/// 两种形状都算：显式的 nth，以及 automation_id 里带序号（row-0、item-3）。
fn positional(container: &Value) -> Option<String> {
    if container.get("nth").is_some() {
        return Some("nth".to_string());
    }
    if let Some(id) = container.get("automation_id").and_then(Value::as_str) {
        // row-0 / item-12 / tab_3：末尾是数字，前缀是通用词
        let trimmed = id.trim_end_matches(|c: char| c.is_ascii_digit());
        if trimmed.len() < id.len()
            && trimmed
                .trim_end_matches(['-', '_'])
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Some(format!("id尾号({id})"));
        }
    }
    // 递归：嵌套容器里任意一层是位置性的都算
    if let Some(inner) = container.get("within") {
        if let Some(reason) = positional(inner) {
            return Some(reason);
        }
    }
    None
}

fn main() {
    let driver = native_driver().expect("a Windows driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("a list");

    let mut total = 0usize;
    let mut positional_container = 0usize;
    let mut stable_container = 0usize;
    let mut no_container = 0usize;
    let mut unresolved: Vec<(String, String, usize, usize)> = Vec::new();
    let mut positional_samples: Vec<String> = Vec::new();

    for window in list.iter().take(20) {
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

        for node in nodes.iter().filter(|n| interactive(n)) {
            total += 1;
            match Locator::synthesize(node, &nodes) {
                None => {
                    // 同 role 兄弟数与最小容器内同类数
                    let siblings = nodes
                        .iter()
                        .filter(|other| other.role == node.role && other.parent_id == node.parent_id)
                        .count();
                    let same_role_in_window =
                        nodes.iter().filter(|other| other.role == node.role).count();
                    unresolved.push((
                        format!("{process}/{}", node.role),
                        format!(
                            "name={:?} id={:?} depth={}",
                            node.name.as_deref(),
                            node.automation_id.as_deref(),
                            node.depth
                        ),
                        siblings,
                        same_role_in_window,
                    ));
                }
                Some(locator) => {
                    let rendered = locator.to_json();
                    match rendered.get("within") {
                        None => no_container += 1,
                        Some(container) => match positional(container) {
                            Some(reason) => {
                                positional_container += 1;
                                if positional_samples.len() < 12 {
                                    positional_samples.push(format!(
                                        "{:<14} {:<22} [{}]\n      {}",
                                        process,
                                        format!(
                                            "{} {:?}",
                                            node.role,
                                            node.name.as_deref().unwrap_or("")
                                        )
                                        .chars()
                                        .take(22)
                                        .collect::<String>(),
                                        reason,
                                        serde_json::to_string(&rendered)
                                            .unwrap_or_default()
                                            .chars()
                                            .take(160)
                                            .collect::<String>()
                                    ));
                                }
                            }
                            None => stable_container += 1,
                        },
                    }
                }
            }
        }
    }

    println!("=== 可交互元素 {total} ===");
    let pct = |part: usize| if total == 0 { 0.0 } else { part as f64 * 100.0 / total as f64 };
    println!("  无需容器              {no_container:>4}  ({:.0}%)", pct(no_container));
    println!("  容器是稳定身份        {stable_container:>4}  ({:.0}%)", pct(stable_container));
    println!("  容器靠位置识别 ⚠      {positional_container:>4}  ({:.0}%)  ← 会安静地指向另一个对象", pct(positional_container));
    println!("  仍然 unresolved       {:>4}  ({:.0}%)", unresolved.len(), pct(unresolved.len()));

    println!("\n=== 位置性容器的样本 ===");
    for line in &positional_samples {
        println!("  {line}");
    }

    println!("\n=== unresolved 的形状（前 20）===");
    for (label, detail, siblings, in_window) in unresolved.iter().take(20) {
        println!("  {label:<22} {detail:<52} 同父兄弟{siblings:<4} 全窗口同role{in_window}");
    }

    // unresolved 的兄弟数分布
    if !unresolved.is_empty() {
        let mut sibling_counts: Vec<usize> = unresolved.iter().map(|entry| entry.2).collect();
        sibling_counts.sort_unstable();
        let median = sibling_counts[sibling_counts.len() / 2];
        let within_ten = sibling_counts.iter().filter(|count| **count <= 10).count();
        println!(
            "\n  unresolved 的同父兄弟数：中位 {median}，最多 {}，其中 ≤10 的有 {within_ten}/{}",
            sibling_counts.last().unwrap(),
            sibling_counts.len()
        );
    }
}
