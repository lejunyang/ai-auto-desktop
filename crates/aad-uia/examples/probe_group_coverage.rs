// 上一轮的可用发现：具名祖先分组的**头部质量很高**（文件资源管理器 35、活动视图切换器 23、
// 编辑器操作 13、标签页栏 42 —— 全是真实区域名），但总组数最多 116，长尾全是单元素组。
//
// 关键：头部几组覆盖了大部分元素（lib.rs 366 个元素，前 5 组占 183）。
//
// 所以 describe 的分层应该是：
//   顶层 = 按具名祖先分组，报每组名字和元素数，长尾折叠成"其他"
//   下钻 = 给一个组名，列出该组内的元素
//
// 这不需要新语法 —— 下钻就是已有的 within，AI 已经会用。
//
// 量三件事定参数：
//   1. 前 N 组覆盖多少（N 取几合适）
//   2. 长尾（单元素组）占多少组、多少元素
//   3. 每组元素数的分布 —— 下钻一次是否落到可读规模

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    let mut coverage_at = [0usize; 5]; // 前 4/8/12/16/20 组的累计覆盖
    let mut totals = 0usize;
    let mut singleton_groups = 0usize;
    let mut singleton_elements = 0usize;
    let mut all_groups = 0usize;
    let mut group_sizes = Vec::new();
    let mut windows_seen = 0usize;

    for window in list {
        let window_id = window["window_id"].as_str().unwrap_or_default();
        let title = window["title"].as_str().unwrap_or_default();
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
        if nodes.len() < 10 {
            continue;
        }
        let by_id: std::collections::HashMap<&str, &Node> =
            nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();
        let actionable: Vec<&Node> = nodes
            .iter()
            .filter(|n| !n.actions.is_empty() && n.states.offscreen != Some(true) && n.depth > 0)
            .collect();
        if actionable.len() < 20 {
            continue;
        }
        windows_seen += 1;
        totals += actionable.len();

        let mut groups: std::collections::HashMap<String, usize> = Default::default();
        for node in &actionable {
            let mut current = node.parent_id.clone();
            let mut label = "(窗口本身)".to_string();
            while let Some(id) = current {
                let Some(parent) = by_id.get(id.as_str()) else { break };
                if parent.depth == 0 {
                    break;
                }
                if parent.name.as_ref().is_some_and(|t| !t.is_empty()) {
                    label = parent.name.clone().unwrap();
                    break;
                }
                current = parent.parent_id.clone();
            }
            *groups.entry(label).or_default() += 1;
        }

        all_groups += groups.len();
        for (_, count) in groups.iter() {
            group_sizes.push(*count);
            if *count == 1 {
                singleton_groups += 1;
                singleton_elements += 1;
            }
        }

        let mut sorted: Vec<usize> = groups.values().copied().collect();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        for (index, n) in [4usize, 8, 12, 16, 20].iter().enumerate() {
            coverage_at[index] += sorted.iter().take(*n).sum::<usize>();
        }

        let top: usize = sorted.iter().take(8).sum();
        println!(
            "{:<36} {:>4} 元素 / {:>3} 组，前 8 组覆盖 {:>3}（{:.0}%），最大组 {}",
            title.chars().take(34).collect::<String>(),
            actionable.len(),
            groups.len(),
            top,
            100.0 * top as f64 / actionable.len() as f64,
            sorted.first().copied().unwrap_or(0)
        );
    }

    println!("\n=== 汇总（{} 个窗口，{} 个可交互元素）===", windows_seen, totals);
    for (index, n) in [4usize, 8, 12, 16, 20].iter().enumerate() {
        println!(
            "  前 {:>2} 组覆盖 {:.0}%",
            n,
            100.0 * coverage_at[index] as f64 / totals.max(1) as f64
        );
    }
    println!(
        "\n  单元素组：{}/{} 组（{:.0}%），只含 {} 个元素（{:.1}%）",
        singleton_groups,
        all_groups,
        100.0 * singleton_groups as f64 / all_groups.max(1) as f64,
        singleton_elements,
        100.0 * singleton_elements as f64 / totals.max(1) as f64
    );
    group_sizes.sort_unstable();
    if !group_sizes.is_empty() {
        println!(
            "  每组元素数：中位 {}，p90 {}，最大 {}",
            group_sizes[group_sizes.len() / 2],
            group_sizes[group_sizes.len() * 9 / 10],
            group_sizes[group_sizes.len() - 1]
        );
    }
}
