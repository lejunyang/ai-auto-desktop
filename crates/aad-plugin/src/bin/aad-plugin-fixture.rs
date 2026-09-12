//! Test-only NDJSON subprocess used by `tests/process_plugin.rs`.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::time::Duration;

const MANIFEST: &str = r#"{"apiVersion":"ai-auto-desktop.dev/v1alpha1","kind":"CapabilityManifest","metadata":{"name":"fixture"},"actions":{"echo":{"contract_major":1,"effect":{"default_class":"read_only"}},"boom":{"contract_major":1,"effect":{"default_class":"read_only"}},"sleep":{"contract_major":1},"quit":{"contract_major":1},"bad":{"contract_major":1},"drift":{"contract_major":1},"both":{"contract_major":1},"noisy":{"contract_major":1},"idle":{"contract_major":1}}}"#;

fn emit(value: &Value) {
    println!("{}", serde_json::to_string(value).expect("fixture JSON"));
    std::io::stdout().flush().expect("flush fixture response");
}

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "responsive".into());
    if mode == "proactive" {
        println!("{MANIFEST}");
        std::io::stdout().flush().expect("flush manifest");
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let request: Value =
            serde_json::from_str(&line.expect("read request")).expect("valid fixture request");
        if request["type"] == "manifest" {
            if mode == "badmanifest" {
                emit(&json!({"id": request["id"], "result": {"nope": true}}));
            } else {
                emit(
                    &json!({"id": request["id"], "result": serde_json::from_str::<Value>(MANIFEST).unwrap()}),
                );
            }
            continue;
        }
        match mode.as_str() {
            "slow" | "sleeper" => std::thread::sleep(Duration::from_secs(300)),
            "garbage" => {
                println!("this is not json");
                std::io::stdout().flush().expect("flush garbage");
                std::thread::sleep(Duration::from_secs(300));
            }
            "wrongid" => {
                emit(&json!({"id": "not-the-request-id", "result": {}}));
                std::thread::sleep(Duration::from_secs(300));
            }
            "exits" => std::process::exit(3),
            "noisy" => {
                eprintln!("diagnostic detail from the plugin");
                std::process::exit(1);
            }
            "both" => emit(
                &json!({"id": request["id"], "result": {}, "error": {"code": "X.Y", "message": "z"}}),
            ),
            _ if request["action"]
                .as_str()
                .is_some_and(|action| action.ends_with("boom@1")) =>
            {
                emit(
                    &json!({"id": request["id"], "error": {"code": "FIXTURE.BOOM", "message": "deliberate failure", "retryable": true, "data": {"reason": "test"}}}),
                )
            }
            _ => emit(
                &json!({"id": request["id"], "result": {"args": request["args"], "deadline_ms": request["deadline_ms"]}}),
            ),
        }
    }
}
