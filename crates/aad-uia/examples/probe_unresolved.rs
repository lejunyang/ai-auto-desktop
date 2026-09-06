// 容器把兄弟数从中位 66 降到 3，但样本里有 7 个 name="关闭 (Ctrl+F4)" 全在同名的
// tool_bar "选项卡操作" 里 —— 容器名一样，仍分不开。
//
// 这一轮要分清三类，因为它们需要的方案不同：
//   A. 容器内唯一，且容器本身可识别      → 只说容器就够
//   B. 容器内不唯一，但容器可识别        → 容器 + 容器内序数
//   C. 容器本身也不可识别（同名兄弟容器）  → 上述两种都无效，得看别的
//
// C 类占比决定这件事能做到什么程度。不预设它小。

use aad_uia::{native_driver, Locator, Node};
use serde_json::json;

fn interactive(node: &Node) -> bool {
    matches!(
        node.role.as_str(),
        "button"
            | "edit"
            | "check_box"
            | "radio_button"
            | "combo_box"
            | "list_item"
            | "menu_item"
            | "tab"
            | "hyperlink"
            | "tree_item"
    )
}

fn main() {
    let driver = native_driver().expect("a Windows driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("a list");

    let mut total = 0usize;
    let mut class_a = 0usize; // 容器可识别 + 容器内唯一
    let mut class_b = 0usize; // 容器可识别 + 需要序数（容器内 ≤10）
    let mut class_b_long = 0usize; // 容器可识别但容器内 >10（序数不实用）
    let mut class_c = 0usize; // 容器本身不可识别
    let mut no_parent = 0usize;
    let mut samples_c: Vec<String> = Vec::new();

    for window in list.iter().take(16) {
        let window_id = window["window_id"].as_str().unwrap_or_default();
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

        let by_id: std::collections::HashMap<&str, &Node> = nodes
            .iter()
            .map(|node| (node.node_id.as_str(), node))
            .collect();

        for node in nodes.iter().filter(|n| interactive(n)) {
            if Locator::synthesize(node, &nodes).is_some() {
                continue;
            }
            total += 1;

            // 沿 parent_id 往上，找第一个「本身可被 synthesize 唯一识别」的祖先。
            // 这才是能写进 locator 的容器 —— 找到一个自己都描述不出来的容器没有用。
            let mut current = node.parent_id.as_deref();
            let mut identifiable_ancestor: Option<&Node> = None;
            let mut depth_walked = 0;
            while let Some(id) = current {
                depth_walked += 1;
                if depth_walked > 8 {
                    break;
                }
                let Some(ancestor) = by_id.get(id) else { break };
                if Locator::synthesize(ancestor, &nodes).is_some() {
                    identifiable_ancestor = Some(ancestor);
                    break;
                }
                current = ancestor.parent_id.as_deref();
            }

            if node.parent_id.is_none() {
                no_parent += 1;
                continue;
            }

            match identifiable_ancestor {
                None => {
                    class_c += 1;
                    if samples_c.len() < 12 {
                        let parent = node
                            .parent_id
                            .as_deref()
                            .and_then(|id| by_id.get(id))
                            .map(|p| {
                                format!(
                                    "{}{}",
                                    p.role,
                                    p.name
                                        .as_deref()
                                        .map(|n| format!(
                                            " {:?}",
                                            n.chars().take(14).collect::<String>()
                                        ))
                                        .unwrap_or_default()
                                )
                            })
                            .unwrap_or_else(|| "?".into());
                        samples_c.push(format!(
                            "{:<34} 父 {}",
                            node.summary().chars().take(34).collect::<String>(),
                            parent
                        ));
                    }
                }
                Some(ancestor) => {
                    // 在这个祖先的子树里，同 role 的有几个？
                    let mut inside = 0usize;
                    for other in nodes.iter().filter(|other| other.role == node.role) {
                        // 沿父链判断 other 是否在 ancestor 下
                        let mut walker = other.parent_id.as_deref();
                        let mut steps = 0;
                        let mut under = false;
                        while let Some(id) = walker {
                            steps += 1;
                            if steps > 10 {
                                break;
                            }
                            if id == ancestor.node_id {
                                under = true;
                                break;
                            }
                            walker = by_id.get(id).and_then(|n| n.parent_id.as_deref());
                        }
                        if under {
                            inside += 1;
                        }
                    }
                    if inside <= 1 {
                        class_a += 1;
                    } else if inside <= 10 {
                        class_b += 1;
                    } else {
                        class_b_long += 1;
                    }
                }
            }
        }
    }

    println!("=== 无法用属性识别的可交互元素: {total} ===\n");
    println!(
        "A 祖先可识别 + 子树内唯一        {class_a:>4}  ({:.0}%)",
        pct(class_a, total)
    );
    println!(
        "B 祖先可识别 + 子树内 ≤10 个     {class_b:>4}  ({:.0}%)  需要序数",
        pct(class_b, total)
    );
    println!(
        "  祖先可识别 + 子树内 >10 个     {class_b_long:>4}  ({:.0}%)  序数不实用",
        pct(class_b_long, total)
    );
    println!(
        "C 没有可识别的祖先              {class_c:>4}  ({:.0}%)",
        pct(class_c, total)
    );
    println!("  根节点（无父）                {no_parent:>4}",);
    println!(
        "\nA+B 可以靠祖先解决: {} / {total}  ({:.0}%)",
        class_a + class_b,
        pct(class_a + class_b, total)
    );

    println!("\n=== C 类样本（祖先也不可识别）===");
    for line in &samples_c {
        println!("  {line}");
    }
}

fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}
