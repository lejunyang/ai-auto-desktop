//! 打印一次真实运行里每个事件的 type 与 payload 顶层键。

fn main() {
    let path = std::path::Path::new(&std::env::var("USERPROFILE").unwrap())
        .join(".ai-auto-desktop")
        .join("recordings")
        .join("agent_fills_billing.workflow.json");
    let document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let descriptor =
        aad_core::compiler::compile_descriptor(document, path.parent().map(Into::into)).unwrap();

    let driver = std::sync::Arc::new(aad_uia::native_driver().unwrap());
    let mut providers = aad_runtime::ProviderRegistry::new();
    providers.insert(driver);

    let result = aad_runtime::run(
        &descriptor,
        aad_runtime::RunOptions::default().with_providers(providers),
    );

    println!("status = {}", result.status.as_str());
    println!("events = {}", result.events.len());
    for event in &result.events {
        let keys: Vec<&str> = event
            .payload
            .as_object()
            .map(|map| map.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let id = event
            .payload
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no id)");
        let status = event
            .payload
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        println!(
            "  {:<18} id={:<18} status={:<10} keys={:?}",
            event.event_type, id, status, keys
        );
    }
}
