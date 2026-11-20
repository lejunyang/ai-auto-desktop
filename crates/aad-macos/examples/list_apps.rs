#[cfg(target_os = "macos")]
fn main() {
    use aad_runtime::Provider;
    use serde_json::json;
    use std::time::Duration;

    let provider = aad_macos::ax::native_provider().expect("create macOS AX provider");
    match provider.invoke(
        "desktop.macos_ax.list_apps@1",
        json!({}),
        Some(Duration::from_secs(10)),
    ) {
        Ok(value) => println!("{value}"),
        Err(error) => {
            println!("{}", error.to_json());
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This example requires macos.");
    std::process::exit(1);
}
