//! End-to-end tests for the plugin host against a real Rust subprocess.

use aad_plugin::{PluginSpec, ProcessPlugin};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

struct Fixture {
    mode: &'static str,
}

impl Fixture {
    fn new(mode: &'static str) -> Self {
        Self { mode }
    }

    fn spec(&self) -> PluginSpec {
        PluginSpec::new(vec![
            env!("CARGO_BIN_EXE_aad-plugin-fixture").to_string(),
            self.mode.to_string(),
        ])
        .with_timeout(Duration::from_secs(10))
        .with_name("fixture")
    }
}

fn start(mode: &'static str) -> ProcessPlugin {
    ProcessPlugin::start(Fixture::new(mode).spec()).expect("plugin should start")
}

#[test]
fn a_requested_manifest_completes_the_handshake() {
    let plugin = start("responsive");
    let manifest = plugin.manifest().expect("manifest is available");
    assert_eq!(manifest.name, "fixture");
    assert!(manifest.resolve("fixture.echo@1").is_some());
}

#[test]
fn a_proactive_manifest_is_accepted_without_a_request() {
    let plugin = start("proactive");
    assert_eq!(plugin.manifest().expect("manifest").name, "fixture");
}

#[test]
fn invoke_round_trips_arguments_and_returns_the_result() {
    let mut plugin = start("responsive");
    let result = plugin
        .invoke("fixture.echo@1", json!({"value": 42}), None)
        .unwrap();
    assert_eq!(result["args"], json!({"value": 42}));
}

#[test]
fn invoke_sends_an_absolute_deadline_the_plugin_can_honour() {
    let mut plugin = start("responsive");
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let result = plugin
        .invoke("fixture.echo@1", json!({}), Some(Duration::from_secs(5)))
        .unwrap();
    let deadline = result["deadline_ms"].as_u64().unwrap();
    assert!(deadline > before && deadline <= before + 5_100);
}

#[test]
fn a_plugin_error_response_is_surfaced_and_process_remains_usable() {
    let mut plugin = start("responsive");
    let error = plugin
        .invoke("fixture.boom@1", json!({}), None)
        .unwrap_err();
    assert_eq!(error.code, "FIXTURE.BOOM");
    assert!(error.retryable && error.dispatched() && !error.is_host_error());
    assert_eq!(error.details["reason"], "test");
    let result = plugin
        .invoke("fixture.echo@1", json!({"after": "error"}), None)
        .unwrap();
    assert_eq!(result["args"], json!({"after": "error"}));
}

#[test]
fn error_effect_tracks_dispatch_boundary() {
    let error = aad_plugin::PluginError::new("PLUGIN.HOST_CLOSED", "closed");
    assert_eq!(error.into_automation_error().effect, "not_applied");
    let mut plugin = start("responsive");
    let error = plugin
        .invoke("fixture.boom@1", json!({}), None)
        .unwrap_err();
    assert_eq!(error.into_automation_error().effect, "unknown");
}

#[test]
fn a_slow_plugin_times_out_and_reports_a_retryable_host_error() {
    let mut plugin = start("slow");
    let started = Instant::now();
    let error = plugin
        .invoke(
            "fixture.sleep@1",
            json!({}),
            Some(Duration::from_millis(300)),
        )
        .unwrap_err();
    assert_eq!(error.code, "PLUGIN.HOST_TIMEOUT");
    assert!(error.retryable && error.dispatched());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_plugin_that_exits_is_reported_as_eof() {
    let mut plugin = start("exits");
    let error = plugin
        .invoke("fixture.quit@1", json!({}), Some(Duration::from_secs(5)))
        .unwrap_err();
    assert_eq!(error.code, "PLUGIN.HOST_EOF");
    assert!(error.is_host_error());
}

#[test]
fn malformed_responses_are_protocol_errors() {
    for mode in ["garbage", "wrongid", "both"] {
        let mut plugin = start(mode);
        let error = plugin
            .invoke(
                &format!("fixture.{mode}@1"),
                json!({}),
                Some(Duration::from_secs(5)),
            )
            .unwrap_err();
        assert_eq!(error.code, "PLUGIN.HOST_PROTOCOL_ERROR", "mode={mode}");
        if mode == "wrongid" {
            assert!(error.details["expected_id"].is_string());
        }
    }
}

#[test]
fn a_manifest_failing_validation_aborts_startup() {
    let error =
        ProcessPlugin::start(Fixture::new("badmanifest").spec()).expect_err("startup must fail");
    assert_eq!(error.code, "PLUGIN.HOST_PROTOCOL_ERROR");
}

#[test]
fn invalid_commands_are_rejected_without_dispatch() {
    let missing = PluginSpec::new(vec!["definitely-not-a-real-command-xyz".to_string()])
        .with_timeout(Duration::from_secs(2));
    let error = ProcessPlugin::start(missing).expect_err("startup must fail");
    assert_eq!(error.code, "PLUGIN.START_FAILED");
    assert!(!error.dispatched());
    let error = ProcessPlugin::start(PluginSpec::new(vec![])).expect_err("empty command");
    assert_eq!(error.code, "PLUGIN.INVALID_REQUEST");
}

#[test]
fn stderr_is_captured_and_attached_to_host_errors() {
    let mut plugin = start("noisy");
    let error = plugin
        .invoke("fixture.noisy@1", json!({}), Some(Duration::from_secs(5)))
        .expect_err("fixture exits");
    let stderr = error
        .details
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(stderr.contains("diagnostic detail"));
}

#[test]
fn close_is_idempotent_and_terminates_the_process() {
    let mut plugin = start("responsive");
    plugin.close();
    plugin.close();
    let error = plugin
        .invoke("fixture.echo@1", json!({}), Some(Duration::from_secs(1)))
        .unwrap_err();
    assert_eq!(error.code, "PLUGIN.HOST_CLOSED");
}

#[test]
fn a_long_running_child_is_reclaimed_on_close() {
    let mut plugin = start("sleeper");
    assert!(plugin.manifest().is_some());
    let started = Instant::now();
    plugin.close();
    assert!(started.elapsed() < Duration::from_secs(5));
}
