// Real-machine probe for the product ambient observation session.
//
// Unlike spike_root_observe (a raw API investigation), this drives the actual
// driver actions -- observe_start / observe_poll / observe_stop -- so it
// verifies the shipped contract (filtering, buffer, drop accounting, json
// shape), not just the native calls. Usage: observe_session_probe [seconds]

use aad_uia::UiaDriver;
use serde_json::json;
use std::sync::Arc;

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);

    let backend = aad_uia::windows::WindowsUiaBackend::new()
        .expect("a Windows UIA backend must be constructible");
    let driver = UiaDriver::new(Arc::new(backend));

    let started = driver
        .call("observe_start", &json!({}))
        .expect("observe_start must install the subscriptions");
    println!("START {started}");
    let observe_id = started["observe_id"]
        .as_str()
        .expect("observe_start returns an observe_id")
        .to_string();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut total = 0u64;
    let mut dropped = 0u64;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let polled = driver
            .call("observe_poll", &json!({"observe_id": observe_id}))
            .expect("observe_poll must read the session");
        let count = polled["count"].as_u64().unwrap_or(0);
        dropped += polled["dropped_events"].as_u64().unwrap_or(0);
        for event in polled["events"].as_array().into_iter().flatten() {
            println!("EVENT {event}");
            total += 1;
        }
        if count > 0 {
            println!("-- batch of {count} (total {total}, dropped {dropped})");
        }
    }

    let stopped = driver
        .call("observe_stop", &json!({"observe_id": observe_id}))
        .expect("observe_stop must tear the session down");
    println!("STOP {stopped}");
    println!("SUMMARY total={total} dropped={dropped}");

    // Polling after stop must be a clear, structured failure.
    let after = driver.call("observe_poll", &json!({"observe_id": observe_id}));
    assert!(after.is_err(), "a stopped session must not be pollable");
    println!("POLL_AFTER_STOP_REJECTED");
}
