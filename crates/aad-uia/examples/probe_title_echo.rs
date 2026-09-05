// 两个 document 合并了（22+21=43），但第一个区域仍是标题。
// restates_window 应该命中它（30 字符 > 12，且是窗口标题的子串）。
//
// 不猜 —— 让程序自己报：这个区域名到底是哪个节点给的，它的名字和窗口标题分别是什么，
// restates_window 对它返回什么。

use aad_uia::native_driver;
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let window = windows["windows"]
        .as_array()
        .expect("list")
        .iter()
        .find(|w| {
            w["title"]
                .as_str()
                .unwrap_or_default()
                .contains("Complex Fixture")
        })
        .expect("fixture");
    let window_id = window["window_id"].as_str().unwrap_or_default();
    let title = window["title"].as_str().unwrap_or_default();
    println!("窗口标题: {title:?}（{} 字符）", title.chars().count());

    let captured = driver
        .call("snapshot", &json!({"window_id": window_id}))
        .expect("snap");
    // 快照里的 window.title 可能和 list_windows 的不同 —— 那会是根因
    println!(
        "快照里的 window.title: {:?}",
        captured["window"]["title"].as_str().unwrap_or("(缺)")
    );

    let nodes = captured["nodes"].as_array().expect("nodes");
    println!("\n名字含 'Complex Fixture' 的节点:");
    for node in nodes {
        let name = node["name"].as_str().unwrap_or_default();
        if name.contains("Complex Fixture") {
            println!(
                "  {} role={} depth={} name={:?}（{} 字符）",
                node["node_id"].as_str().unwrap_or(""),
                node["role"].as_str().unwrap_or(""),
                node["depth"],
                name,
                name.chars().count()
            );
            println!(
                "     是窗口标题的子串吗: {}  窗口标题是它的子串吗: {}",
                title.contains(name),
                name.contains(title)
            );
        }
    }
}
