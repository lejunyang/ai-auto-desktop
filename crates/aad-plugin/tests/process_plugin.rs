//! End-to-end tests for the plugin host, driven against real subprocesses.
//!
//! Each fixture is a small Python program written to a temporary file, so the
//! tests exercise the actual process boundary: spawning, the NDJSON handshake,
//! deadline enforcement and teardown.

use aad_plugin::{PluginSpec, ProcessPlugin};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Write a Python fixture and return the command that runs it.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str, source: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "aad-plugin-{label}-{}.py",
            uuid_like(),
        ));
        let mut file = std::fs::File::create(&path).expect("fixture file");
        file.write_all(source.as_bytes()).expect("write fixture");
        file.flush().expect("flush fixture");
        Self { path }
    }

    fn spec(&self) -> PluginSpec {
        PluginSpec::new(vec![
            python(),
            self.path.to_string_lossy().into_owned(),
        ])
        .with_timeout(Duration::from_secs(10))
        .with_name("fixture")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos:x}{:x}", std::process::id())
}

fn python() -> String {
    std::env::var("AAD_TEST_PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    })
}

/// A plugin that answers a manifest request and echoes invocations.
const RESPONSIVE: &str = r#"
import json, sys

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {
        "echo": {"contract_major": 1, "effect": {"class": "read_only"}},
        "boom": {"contract_major": 1, "effect": {"class": "read_only"}},
    },
}

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    kind = request.get("type")
    if kind == "manifest":
        response = {"id": request["id"], "result": MANIFEST}
    elif request.get("action", "").endswith("boom@1"):
        response = {
            "id": request["id"],
            "error": {
                "code": "FIXTURE.BOOM",
                "message": "deliberate failure",
                "retryable": True,
                "data": {"reason": "test"},
            },
        }
    else:
        response = {
            "id": request["id"],
            "result": {"args": request.get("args"), "deadline_ms": request.get("deadline_ms")},
        }
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#;

/// A plugin that emits its manifest proactively, before any request.
const PROACTIVE: &str = r#"
import json, sys

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"echo": {"contract_major": 1}},
}
sys.stdout.write(json.dumps(MANIFEST) + "\n")
sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    sys.stdout.write(json.dumps({"id": request["id"], "result": {"ok": True}}) + "\n")
    sys.stdout.flush()
"#;

fn start(fixture: &Fixture) -> ProcessPlugin {
    ProcessPlugin::start(fixture.spec()).expect("plugin should start")
}

#[test]
fn a_requested_manifest_completes_the_handshake() {
    let fixture = Fixture::new("responsive", RESPONSIVE);
    let plugin = start(&fixture);

    let manifest = plugin.manifest().expect("manifest is available");
    assert_eq!(manifest.name, "fixture");
    assert!(manifest.resolve("fixture.echo@1").is_some());
}

#[test]
fn a_proactive_manifest_is_accepted_without_a_request() {
    let fixture = Fixture::new("proactive", PROACTIVE);
    let plugin = start(&fixture);

    assert_eq!(plugin.manifest().expect("manifest").name, "fixture");
}

#[test]
fn invoke_round_trips_arguments_and_returns_the_result() {
    let fixture = Fixture::new("responsive", RESPONSIVE);
    let mut plugin = start(&fixture);

    let result = plugin
        .invoke("fixture.echo@1", json!({"value": 42}), None)
        .expect("invocation should succeed");

    assert_eq!(result["args"], json!({"value": 42}));
}

#[test]
fn invoke_sends_an_absolute_deadline_the_plugin_can_honour() {
    let fixture = Fixture::new("responsive", RESPONSIVE);
    let mut plugin = start(&fixture);

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let result = plugin
        .invoke("fixture.echo@1", json!({}), Some(Duration::from_secs(5)))
        .expect("invocation should succeed");

    let deadline = result["deadline_ms"].as_u64().expect("deadline is sent");
    assert!(
        deadline > before && deadline <= before + 5_100,
        "deadline {deadline} should be ~5s after {before}"
    );
}

#[test]
fn a_plugin_error_response_is_surfaced_with_its_code_and_details() {
    let fixture = Fixture::new("responsive", RESPONSIVE);
    let mut plugin = start(&fixture);

    let error = plugin
        .invoke("fixture.boom@1", json!({}), None)
        .expect_err("the fixture always fails this action");

    assert_eq!(error.code, "FIXTURE.BOOM");
    assert!(error.retryable);
    assert_eq!(error.details["reason"], json!("test"));
    // The request was written, so the outcome is ambiguous by definition.
    assert!(error.dispatched());
    assert!(!error.is_host_error());
}

#[test]
fn a_plugin_error_does_not_invalidate_the_process() {
    let fixture = Fixture::new("responsive", RESPONSIVE);
    let mut plugin = start(&fixture);

    plugin.invoke("fixture.boom@1", json!({}), None).unwrap_err();
    let result = plugin
        .invoke("fixture.echo@1", json!({"after": "error"}), None)
        .expect("the plugin is still usable after a normal error response");

    assert_eq!(result["args"], json!({"after": "error"}));
}

#[test]
fn an_undispatched_error_reports_effect_not_applied() {
    let error = aad_plugin::PluginError::new("PLUGIN.HOST_CLOSED", "closed");
    let automation = error.into_automation_error();

    assert_eq!(automation.effect, "not_applied");
}

#[test]
fn a_dispatched_error_reports_effect_unknown() {
    let fixture = Fixture::new("responsive", RESPONSIVE);
    let mut plugin = start(&fixture);

    let error = plugin.invoke("fixture.boom@1", json!({}), None).unwrap_err();
    let automation = error.into_automation_error();

    assert_eq!(automation.effect, "unknown");
    assert_eq!(automation.code, "FIXTURE.BOOM");
}

#[test]
fn a_slow_plugin_times_out_and_reports_a_retryable_host_error() {
    let fixture = Fixture::new(
        "slow",
        r#"
import json, sys, time

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"sleep": {"contract_major": 1}},
}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    if request.get("type") == "manifest":
        sys.stdout.write(json.dumps({"id": request["id"], "result": MANIFEST}) + "\n")
        sys.stdout.flush()
        continue
    time.sleep(30)
"#,
    );
    let mut plugin = start(&fixture);

    let started = Instant::now();
    let error = plugin
        .invoke("fixture.sleep@1", json!({}), Some(Duration::from_millis(300)))
        .expect_err("the invocation must time out");

    assert_eq!(error.code, "PLUGIN.HOST_TIMEOUT");
    assert!(error.retryable);
    // Ambiguous: the request was sent but no reply arrived.
    assert!(error.dispatched());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must be enforced by the host, not by the plugin"
    );
}

#[test]
fn a_plugin_that_exits_is_reported_as_eof() {
    let fixture = Fixture::new(
        "exits",
        r#"
import json, sys

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"quit": {"contract_major": 1}},
}
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({"id": request["id"], "result": MANIFEST}) + "\n")
sys.stdout.flush()
sys.exit(3)
"#,
    );
    let mut plugin = start(&fixture);

    let error = plugin
        .invoke("fixture.quit@1", json!({}), Some(Duration::from_secs(5)))
        .expect_err("the plugin exited");

    assert_eq!(error.code, "PLUGIN.HOST_EOF");
    assert!(error.is_host_error());
}

#[test]
fn invalid_json_on_stdout_is_a_protocol_error() {
    let fixture = Fixture::new(
        "garbage",
        r#"
import json, sys

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"bad": {"contract_major": 1}},
}
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({"id": request["id"], "result": MANIFEST}) + "\n")
sys.stdout.flush()
sys.stdin.readline()
sys.stdout.write("this is not json\n")
sys.stdout.flush()
import time; time.sleep(5)
"#,
    );
    let mut plugin = start(&fixture);

    let error = plugin
        .invoke("fixture.bad@1", json!({}), Some(Duration::from_secs(5)))
        .expect_err("invalid JSON must fail");

    assert_eq!(error.code, "PLUGIN.HOST_PROTOCOL_ERROR");
}

#[test]
fn a_mismatched_response_id_is_rejected() {
    let fixture = Fixture::new(
        "wrongid",
        r#"
import json, sys

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"drift": {"contract_major": 1}},
}
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({"id": request["id"], "result": MANIFEST}) + "\n")
sys.stdout.flush()
sys.stdin.readline()
sys.stdout.write(json.dumps({"id": "not-the-request-id", "result": {}}) + "\n")
sys.stdout.flush()
import time; time.sleep(5)
"#,
    );
    let mut plugin = start(&fixture);

    let error = plugin
        .invoke("fixture.drift@1", json!({}), Some(Duration::from_secs(5)))
        .expect_err("a mismatched id must fail");

    assert_eq!(error.code, "PLUGIN.HOST_PROTOCOL_ERROR");
    assert_eq!(error.details["expected_id"].is_string(), true);
}

#[test]
fn a_response_with_both_result_and_error_is_rejected() {
    let fixture = Fixture::new(
        "ambiguous",
        r#"
import json, sys

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"both": {"contract_major": 1}},
}
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({"id": request["id"], "result": MANIFEST}) + "\n")
sys.stdout.flush()
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({
    "id": request["id"], "result": {}, "error": {"code": "X.Y", "message": "z"}
}) + "\n")
sys.stdout.flush()
import time; time.sleep(5)
"#,
    );
    let mut plugin = start(&fixture);

    let error = plugin
        .invoke("fixture.both@1", json!({}), Some(Duration::from_secs(5)))
        .expect_err("an ambiguous response must fail");

    assert_eq!(error.code, "PLUGIN.HOST_PROTOCOL_ERROR");
}

#[test]
fn a_manifest_failing_validation_aborts_startup() {
    let fixture = Fixture::new(
        "badmanifest",
        r#"
import json, sys
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({"id": request["id"], "result": {"nope": True}}) + "\n")
sys.stdout.flush()
import time; time.sleep(5)
"#,
    );

    let error = ProcessPlugin::start(fixture.spec()).expect_err("startup must fail");
    assert_eq!(error.code, "PLUGIN.HOST_PROTOCOL_ERROR");
}

#[test]
fn a_missing_executable_fails_to_start() {
    let spec = PluginSpec::new(vec!["definitely-not-a-real-command-xyz".to_string()])
        .with_timeout(Duration::from_secs(2));

    let error = ProcessPlugin::start(spec).expect_err("startup must fail");
    assert_eq!(error.code, "PLUGIN.START_FAILED");
    assert!(!error.dispatched());
}

#[test]
fn an_empty_command_is_rejected_without_spawning() {
    let error = ProcessPlugin::start(PluginSpec::new(vec![])).expect_err("must be rejected");
    assert_eq!(error.code, "PLUGIN.INVALID_REQUEST");
}

#[test]
fn stderr_is_captured_and_attached_to_host_errors() {
    let fixture = Fixture::new(
        "noisy",
        r#"
import json, sys

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"noisy": {"contract_major": 1}},
}
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({"id": request["id"], "result": MANIFEST}) + "\n")
sys.stdout.flush()
sys.stderr.write("diagnostic detail from the plugin\n")
sys.stderr.flush()
sys.exit(1)
"#,
    );
    let mut plugin = start(&fixture);

    let error = plugin
        .invoke("fixture.noisy@1", json!({}), Some(Duration::from_secs(5)))
        .expect_err("the plugin exits");

    let stderr = error.details.get("stderr").and_then(Value::as_str).unwrap_or("");
    assert!(
        stderr.contains("diagnostic detail"),
        "stderr should be attached, got {stderr:?}"
    );
}

#[test]
fn close_is_idempotent_and_terminates_the_process() {
    let fixture = Fixture::new("responsive", RESPONSIVE);
    let mut plugin = start(&fixture);

    plugin.close();
    plugin.close();

    let error = plugin
        .invoke("fixture.echo@1", json!({}), Some(Duration::from_secs(1)))
        .expect_err("a closed plugin cannot be invoked");
    assert_eq!(error.code, "PLUGIN.HOST_CLOSED");
}

#[test]
fn a_long_running_child_is_reclaimed_on_close() {
    let fixture = Fixture::new(
        "sleeper",
        r#"
import json, sys, time

MANIFEST = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture"},
    "actions": {"idle": {"contract_major": 1}},
}
line = sys.stdin.readline()
request = json.loads(line)
sys.stdout.write(json.dumps({"id": request["id"], "result": MANIFEST}) + "\n")
sys.stdout.flush()
time.sleep(300)
"#,
    );
    let mut plugin = start(&fixture);
    assert!(plugin.manifest().is_some());

    let started = Instant::now();
    plugin.close();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "close must not wait for the plugin to finish on its own"
    );
}
