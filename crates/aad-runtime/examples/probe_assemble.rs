// 组装测试全过，但那只证明形状符合我的预期。要害在于引擎是否真的接受 ——
// 这正是我上一轮手写时被 validate 指出六处错误的地方。
//
// 所以：组装 → 落盘 → 跑真实的 validate。让编译器自己说话，而不是我断言它会满意。

use aad_runtime::assemble::{assemble, PerformedStep};
use serde_json::json;

fn main() {
    let cases: Vec<(&str, Vec<PerformedStep>)> = vec![
        (
            "single_click",
            vec![PerformedStep {
                action: "invoke".into(),
                locator: json!({"role": "button", "name": "Save"}),
                window: json!({"process_name": "msedge.exe", "title": "AAD Complex Fixture"}),
                argument: None,
                protected: false,
            }],
        ),
        (
            "fill_then_submit",
            vec![
                PerformedStep {
                    action: "set_value".into(),
                    locator: json!({"automation_id": "billing-city"}),
                    window: json!({"process_name": "msedge.exe", "title": "AAD Complex Fixture"}),
                    argument: Some("Beijing".into()),
                    protected: false,
                },
                PerformedStep {
                    action: "invoke".into(),
                    locator: json!({"automation_id": "page-save"}),
                    window: json!({"process_name": "msedge.exe", "title": "AAD Complex Fixture"}),
                    argument: None,
                    protected: false,
                },
            ],
        ),
        (
            "with_credential",
            vec![PerformedStep {
                action: "set_value".into(),
                locator: json!({"role": "edit", "protected": true}),
                window: json!({"process_name": "app.exe", "title": "Login"}),
                argument: Some("should not appear".into()),
                protected: true,
            }],
        ),
        (
            "descriptive_locator",
            vec![PerformedStep {
                action: "invoke".into(),
                locator: json!({
                    "role": "button",
                    "name": "Edit",
                    "within": {"role": "data_item", "name": "Order for Ada"}
                }),
                window: json!({"process_name": "msedge.exe", "title": "AAD Complex Fixture"}),
                argument: None,
                protected: false,
            }],
        ),
    ];

    for (name, steps) in cases {
        match assemble(name, &steps) {
            Ok(descriptor) => {
                let path = std::env::temp_dir().join(format!("assembled_{name}.json"));
                let text = serde_json::to_string_pretty(&descriptor).expect("render");
                std::fs::write(&path, &text).expect("write");
                println!(
                    "{name}: {} 个执行步骤，{} 字符 → {}",
                    descriptor["steps"].as_array().map(Vec::len).unwrap_or(0),
                    text.chars().count(),
                    path.display()
                );
            }
            Err(problem) => println!("{name}: 组装被拒 {} — {}", problem.code, problem.message),
        }
    }
}
