// 三个问题量化清楚了：9/20 窗口超限、truncated 分不清两种情况、单条 331 字符
// （limit=500 就 127KB）。
//
// 单条太重是关键 —— 提高 limit 会撑爆上下文，所以分层下钻是唯一可行的方案。
//
// 下钻要靠层级，先量它到底可不可用：
//   1. 若只给"有子节点的容器"作为顶层视图，有多少条？
//   2. 这些容器有名字吗？没名字的容器 AI 无从选择
//   3. 每个容器下面有多少元素 —— 下钻一层是否就落到可读的规模
//   4. summary 和 locator 有多少重复，能省多少

use aad_uia::{native_driver, Locator, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    // 挑最大的窗口做主样本
    let mut sample: Option<(String, Vec<Node>)> = None;
    let mut best = 0usize;
    for window in list {
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
        let visible = nodes
            .iter()
            .filter(|n| {
                (!n.actions.is_empty() || n.name.as_ref().is_some_and(|t| !t.is_empty()))
                    && n.states.offscreen != Some(true)
            })
            .count();
        if visible > best {
            best = visible;
            sample = Some((
                window["title"].as_str().unwrap_or_default().to_string(),
                nodes,
            ));
        }
    }

    let Some((title, nodes)) = sample else {
        println!("没有样本");
        return;
    };
    println!(
        "样本: {}（{} 节点，{} 可见）\n",
        title.chars().take(44).collect::<String>(),
        nodes.len(),
        best
    );

    let has_children = |node: &Node| nodes.iter().any(|n| n.parent_id.as_deref() == Some(&node.node_id));

    // 1&2. 容器视图
    let containers: Vec<&Node> = nodes
        .iter()
        .filter(|n| has_children(n) && n.states.offscreen != Some(true))
        .collect();
    let named: Vec<&&Node> = containers
        .iter()
        .filter(|n| n.name.as_ref().is_some_and(|t| !t.is_empty()))
        .collect();
    println!("=== 若顶层只给容器 ===");
    println!("  容器 {} 个，其中有名字的 {} 个", containers.len(), named.len());

    // 3. 每个容器下面多少个（直接子节点 + 全部后代）
    let mut spans: Vec<(usize, usize, &Node)> = containers
        .iter()
        .map(|node| {
            let direct = nodes
                .iter()
                .filter(|n| n.parent_id.as_deref() == Some(&node.node_id))
                .count();
            // 全部后代
            let mut stack = vec![node.node_id.clone()];
            let mut total = 0usize;
            while let Some(id) = stack.pop() {
                for child in nodes.iter().filter(|n| n.parent_id.as_deref() == Some(&id)) {
                    total += 1;
                    stack.push(child.node_id.clone());
                }
            }
            (total, direct, *node)
        })
        .collect();
    spans.sort_by_key(|(total, _, _)| std::cmp::Reverse(*total));

    println!("\n  最大的 10 个容器（后代数 / 直接子节点 / 名字）:");
    for (total, direct, node) in spans.iter().take(10) {
        let label = node.name.as_deref().unwrap_or("(无名)");
        println!(
            "    {total:>4} / {direct:>3}  depth={:<3} {} {}",
            node.depth,
            node.role,
            label.chars().take(34).collect::<String>()
        );
    }

    let mut direct_counts: Vec<usize> = spans.iter().map(|(_, d, _)| *d).collect();
    direct_counts.sort_unstable();
    if !direct_counts.is_empty() {
        println!(
            "\n  直接子节点数：中位 {}，最大 {}",
            direct_counts[direct_counts.len() / 2],
            direct_counts[direct_counts.len() - 1]
        );
    }

    // 4. summary 与 locator 的重复
    println!("\n=== 单条 331 字符花在哪 ===");
    let mut summary_bytes = 0usize;
    let mut locator_bytes = 0usize;
    let mut ref_bytes = 0usize;
    let mut actions_bytes = 0usize;
    let mut counted = 0usize;
    for node in nodes.iter().filter(|n| {
        (!n.actions.is_empty() || n.name.as_ref().is_some_and(|t| !t.is_empty()))
            && n.states.offscreen != Some(true)
    }) {
        summary_bytes += node.summary().len();
        locator_bytes += Locator::synthesize(node, &nodes)
            .map(|l| l.to_json().to_string().len())
            .unwrap_or(4);
        ref_bytes += 32 + 1 + 4 + 1 + node.node_id.len();
        actions_bytes += json!(node.actions).to_string().len();
        counted += 1;
    }
    if counted > 0 {
        println!("  summary  平均 {} 字符", summary_bytes / counted);
        println!("  locator  平均 {} 字符", locator_bytes / counted);
        println!("  ref      平均 {} 字符", ref_bytes / counted);
        println!("  actions  平均 {} 字符", actions_bytes / counted);
    }
}
