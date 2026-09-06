// 两个设计问题要先量，不能猜：
//
// 1. 上限用字符还是字节？中文在 UTF-8 里占 3 字节，而区域名和元素名大量是中文。
//    若两者差很多，选错单位会让"限 20000"在中文界面上意外地严格或宽松。
//
// 2. 截断粒度：按元素整个丢，还是可以切进元素内部？切进去会产生非法 JSON，
//    但整个丢会让"限 5000"在单条 331 字符的情况下只剩 15 条。量一下单条的分布。

use aad_uia::{native_driver, Node};
use serde_json::json;

fn main() {
    let driver = native_driver().expect("driver");
    let windows = driver.call("list_windows", &json!({})).expect("windows");
    let list = windows["windows"].as_array().expect("list");

    println!("{:<40} {:>8} {:>8} {:>6}", "窗口", "字符", "字节", "比值");
    let mut all_element_chars: Vec<usize> = Vec::new();

    for window in list {
        let window_id = window["window_id"].as_str().unwrap_or_default();
        let title = window["title"].as_str().unwrap_or_default();
        let Ok(answer) = driver.call("describe", &json!({"window_id": window_id, "limit": 500}))
        else {
            continue;
        };
        let text = serde_json::to_string(&answer).unwrap_or_default();
        if text.len() < 400 {
            continue;
        }
        let chars = text.chars().count();
        let bytes = text.len();
        println!(
            "{:<40} {:>8} {:>8} {:>6.2}",
            title.chars().take(38).collect::<String>(),
            chars,
            bytes,
            bytes as f64 / chars.max(1) as f64
        );

        if let Some(elements) = answer["elements"].as_array() {
            for element in elements {
                all_element_chars.push(
                    serde_json::to_string(element)
                        .unwrap_or_default()
                        .chars()
                        .count(),
                );
            }
        }
    }

    all_element_chars.sort_unstable();
    if all_element_chars.is_empty() {
        return;
    }
    let pick = |p: f64| all_element_chars[((all_element_chars.len() - 1) as f64 * p) as usize];
    println!(
        "\n单条元素的 JSON 字符数（{} 条样本）:",
        all_element_chars.len()
    );
    println!(
        "  最小 {}  p25 {}  中位 {}  p75 {}  p90 {}  最大 {}",
        all_element_chars[0],
        pick(0.25),
        pick(0.50),
        pick(0.75),
        pick(0.90),
        all_element_chars[all_element_chars.len() - 1]
    );
    let total: usize = all_element_chars.iter().sum();
    println!("  平均 {}", total / all_element_chars.len());

    // 几个候选默认值下能装多少条
    println!("\n若按元素整个丢弃，各上限装得下多少条（用中位值估）:");
    let median = pick(0.50).max(1);
    for cap in [4000usize, 8000, 16000, 20000, 40000] {
        println!("  上限 {cap:>6} 字符 → 约 {} 条", cap / median);
    }

    // overview 的体积：它是"永远不该被截断"的那一层
    println!("\noverview 的体积（决定它是否需要上限）:");
    for window in list.iter().take(20) {
        let window_id = window["window_id"].as_str().unwrap_or_default();
        let Ok(answer) = driver.call("overview", &json!({"window_id": window_id})) else {
            continue;
        };
        let text = serde_json::to_string(&answer).unwrap_or_default();
        if text.chars().count() > 1200 {
            println!(
                "  {:<38} {} 字符",
                window["title"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(36)
                    .collect::<String>(),
                text.chars().count()
            );
        }
    }
    let _ = std::marker::PhantomData::<Node>;
}
