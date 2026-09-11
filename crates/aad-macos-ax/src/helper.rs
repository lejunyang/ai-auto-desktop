use super::{
    driver_error, remaining, AutomationError, AxBackend, BackendNode, BackendSnapshot, Bounds, Map,
    Value,
};
use serde_json::json;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024 - 1;
const HELPER_BUNDLE_ID: &str = "dev.ai-auto-desktop.macos-ax-helper";
const HELPER_BUNDLE_NAME: &str = "MacOSAXHelper.app";
const HELPER_EXECUTABLE_NAME: &str = "MacOSAXHelper";
const HELPER_PROTOCOL_VERSION: u64 = 2;

struct HelperState {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    buffer: Vec<u8>,
    request_number: u64,
}

pub struct SwiftHelperBackend {
    state: Mutex<Option<HelperState>>,
    helper_source: String,
}

impl SwiftHelperBackend {
    pub fn new() -> Result<Self, AutomationError> {
        let configured = std::env::var_os("AI_AUTO_DESKTOP_MACOS_AX_HELPER").map(PathBuf::from);
        let helper_source = if configured.is_some() {
            "custom_untrusted"
        } else {
            "default_build"
        };
        let helper = configured.or_else(find_default_helper).ok_or_else(|| {
            driver_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX Swift helper has not been built",
            )
            .with_detail("reason", json!("helper_missing"))
        })?;
        let bundle = helper_bundle(&helper)?;
        validate_bundle(&bundle)?;
        verify_signature(&bundle)?;
        let mut child = Command::new(&helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                driver_error(
                    "DRIVER.UNAVAILABLE",
                    "signed macOS AX helper could not be started",
                )
                .with_detail("cause", json!(error.to_string()))
            })?;
        let stdin = child.stdin.take().expect("helper stdin is piped");
        let stdout = child.stdout.take().expect("helper stdout is piped");
        let backend = Self {
            state: Mutex::new(Some(HelperState {
                child,
                stdin,
                stdout,
                buffer: Vec::new(),
                request_number: 0,
            })),
            helper_source: helper_source.into(),
        };
        let status = backend.rpc(
            "status",
            json!({}),
            Instant::now() + Duration::from_secs(5),
            false,
        )?;
        if status["protocol_version"] != HELPER_PROTOCOL_VERSION
            || status["implementation"] != "native_accessibility_api"
        {
            backend.close();
            return Err(driver_error(
                "DRIVER.ACTION_FAILED",
                "macOS AX helper handshake is incompatible",
            ));
        }
        Ok(backend)
    }

    fn rpc(
        &self,
        operation: &str,
        args: Value,
        deadline: Instant,
        track_dispatch: bool,
    ) -> Result<Value, AutomationError> {
        remaining(deadline, false)?;
        let mut state = self.state.lock().map_err(|_| {
            channel_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper state is unavailable",
                false,
                false,
                false,
            )
        })?;
        let state = state.as_mut().ok_or_else(|| {
            channel_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper is not running",
                false,
                false,
                false,
            )
        })?;
        if state.child.try_wait().ok().flatten().is_some() {
            return Err(channel_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper exited",
                false,
                false,
                false,
            ));
        }
        state.request_number += 1;
        let id = format!("h{}", state.request_number);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let deadline_ms = epoch
            .as_millis()
            .saturating_add(remaining(deadline, false)?.as_millis())
            .min(u128::from(u64::MAX)) as u64;
        let mut encoded = serde_json::to_vec(
            &json!({"id": id, "operation": operation, "args": args, "deadline_ms": deadline_ms}),
        )
        .map_err(|_| invalid("macOS AX helper request could not be encoded"))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err(invalid("macOS AX helper request exceeds the frame limit"));
        }
        state
            .stdin
            .write_all(&encoded)
            .and_then(|()| state.stdin.flush())
            .map_err(|error| {
                channel_error(
                    "DRIVER.UNAVAILABLE",
                    &format!("macOS AX helper write failed: {error}"),
                    false,
                    false,
                    false,
                )
            })?;
        let mut dispatch_started = false;
        let mut focus_changed = false;
        loop {
            let line = read_line(state, deadline).map_err(|error| {
                let effect = dispatch_started || focus_changed;
                if effect {
                    terminate_state(state, true);
                }
                channel_error(
                    &error.code,
                    &error.message,
                    true,
                    dispatch_started,
                    focus_changed,
                )
            })?;
            let response: Value = serde_json::from_slice(&line).map_err(|_| {
                terminate_state(state, true);
                channel_error(
                    "DRIVER.ACTION_FAILED",
                    "macOS AX helper returned invalid JSON",
                    true,
                    dispatch_started,
                    focus_changed,
                )
            })?;
            let object = response.as_object().ok_or_else(|| {
                channel_error(
                    "DRIVER.ACTION_FAILED",
                    "macOS AX helper response must be an object",
                    true,
                    dispatch_started,
                    focus_changed,
                )
            })?;
            if object.get("id") != Some(&json!(id)) {
                terminate_state(state, true);
                return Err(channel_error(
                    "DRIVER.ACTION_FAILED",
                    "macOS AX helper response id mismatch",
                    true,
                    dispatch_started,
                    focus_changed,
                ));
            }
            if let Some(progress) = object.get("progress").and_then(Value::as_object) {
                dispatch_started |= progress
                    .get("keyboard_dispatch_started")
                    .and_then(Value::as_bool)
                    == Some(true)
                    || progress
                        .get("pointer_dispatch_started")
                        .and_then(Value::as_bool)
                        == Some(true);
                focus_changed |=
                    progress.get("focus_changed").and_then(Value::as_bool) == Some(true);
                continue;
            }
            if let Some(error) = object.get("error").and_then(Value::as_object) {
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .filter(|code| code.starts_with("DRIVER."))
                    .unwrap_or("DRIVER.ACTION_FAILED");
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("macOS AX helper failed");
                let retryable = error
                    .get("retryable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let details = error
                    .get("data")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                dispatch_started |= details
                    .get("keyboard_dispatch_started")
                    .and_then(Value::as_bool)
                    == Some(true)
                    || details
                        .get("pointer_dispatch_started")
                        .and_then(Value::as_bool)
                        == Some(true);
                focus_changed |=
                    details.get("focus_changed").and_then(Value::as_bool) == Some(true);
                if track_dispatch && dispatch_started {
                    terminate_state(state, true);
                    return Err(driver_error(
                        "DRIVER.UNKNOWN_EFFECT",
                        "macOS input result is unknown after native dispatch",
                    )
                    .with_effect("unknown")
                    .with_details(details));
                }
                let effect = if track_dispatch && focus_changed {
                    "contextual"
                } else {
                    "not_applied"
                };
                return Err(driver_error(code, message)
                    .with_retryable(retryable)
                    .with_effect(effect)
                    .with_details(details));
            }
            let result = object.get("result").cloned().ok_or_else(|| {
                channel_error(
                    "DRIVER.ACTION_FAILED",
                    "macOS AX helper response omitted result and error",
                    true,
                    dispatch_started,
                    focus_changed,
                )
            })?;
            if track_dispatch {
                let declared = result
                    .get("keyboard_dispatch_started")
                    .and_then(Value::as_bool)
                    .or_else(|| {
                        result
                            .get("pointer_dispatch_started")
                            .and_then(Value::as_bool)
                    });
                if declared != Some(dispatch_started) {
                    terminate_state(state, true);
                    return Err(channel_error(
                        "DRIVER.UNKNOWN_EFFECT",
                        "macOS AX helper dispatch state is inconsistent",
                        true,
                        dispatch_started,
                        focus_changed,
                    ));
                }
            }
            return Ok(result);
        }
    }

    fn write(
        &self,
        operation: &str,
        args: Value,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let track = matches!(operation, "type_text" | "pointer_click");
        let result = self.rpc(operation, args, deadline, track)?;
        let expected = if track { "submitted" } else { "accepted" };
        if result.get(expected) != Some(&Value::Bool(true))
            || !result.get("native_operation").is_some_and(Value::is_string)
        {
            return Err(channel_error(
                "DRIVER.ACTION_FAILED",
                "macOS AX helper returned an invalid write result",
                true,
                false,
                false,
            ));
        }
        Ok(result)
    }
}

impl AxBackend for SwiftHelperBackend {
    fn name(&self) -> &str {
        "macos_ax_swift_helper"
    }
    fn security_info(&self) -> Option<Map<String, Value>> {
        Some(Map::from_iter([
            ("source".into(), json!(self.helper_source)),
            ("integrity_verified".into(), json!(true)),
            ("source_authenticated".into(), json!(false)),
        ]))
    }
    fn list_apps(
        &self,
        deadline: Instant,
    ) -> Result<(bool, Vec<Map<String, Value>>), AutomationError> {
        let result = self.rpc("list_apps", json!({}), deadline, false)?;
        let trusted = result["accessibility_trusted"]
            .as_bool()
            .ok_or_else(|| protocol_error("invalid list_apps response"))?;
        let apps = result["apps"]
            .as_array()
            .ok_or_else(|| protocol_error("invalid list_apps response"))?
            .iter()
            .map(|value| {
                value
                    .as_object()
                    .cloned()
                    .ok_or_else(|| protocol_error("invalid app response"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((trusted, apps))
    }
    fn capture(
        &self,
        app: &Map<String, Value>,
        max_depth: u32,
        max_nodes: usize,
        deadline: Instant,
    ) -> Result<BackendSnapshot, AutomationError> {
        let result = self.rpc(
            "snapshot",
            json!({"app": app, "max_depth": max_depth, "max_nodes": max_nodes}),
            deadline,
            false,
        )?;
        let app = result["app"]
            .as_object()
            .cloned()
            .ok_or_else(|| protocol_error("invalid snapshot app"))?;
        let truncated = result["truncated"]
            .as_bool()
            .ok_or_else(|| protocol_error("invalid snapshot truncation"))?;
        let nodes = result["nodes"]
            .as_array()
            .ok_or_else(|| protocol_error("invalid snapshot nodes"))?
            .iter()
            .map(parse_node)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BackendSnapshot {
            app,
            nodes,
            truncated,
        })
    }
    fn same_element(
        &self,
        previous: &str,
        current: &str,
        deadline: Instant,
    ) -> Result<bool, AutomationError> {
        let value = self.rpc(
            "same_element",
            json!({"previous_token": previous, "current_token": current}),
            deadline,
            false,
        )?;
        value["same"]
            .as_bool()
            .ok_or_else(|| protocol_error("invalid same_element response"))
    }
    fn focus(&self, token: &str, deadline: Instant) -> Result<Value, AutomationError> {
        self.write("focus", json!({"native_token": token}), deadline)
    }
    fn invoke(&self, token: &str, deadline: Instant) -> Result<Value, AutomationError> {
        self.write("invoke", json!({"native_token": token}), deadline)
    }
    fn pointer_click(
        &self,
        token: &str,
        button: &str,
        position: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.write(
            "pointer_click",
            json!({"native_token": token, "button": button, "position": position}),
            deadline,
        )
    }
    fn set_value(
        &self,
        token: &str,
        value: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.write(
            "set_value",
            json!({"native_token": token, "value": value}),
            deadline,
        )
    }
    fn type_text(
        &self,
        token: &str,
        text: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.write(
            "type_text",
            json!({"native_token": token, "text": text}),
            deadline,
        )
    }
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                terminate_state(state, false);
            }
            *state = None;
        }
    }
}

fn parse_node(value: &Value) -> Result<BackendNode, AutomationError> {
    let object = value
        .as_object()
        .ok_or_else(|| protocol_error("helper node must be an object"))?;
    let token = object
        .get("native_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| protocol_error("helper node has no token"))?
        .to_string();
    let parent_index = match object.get("parent_index") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| protocol_error("invalid parent index"))?,
        ),
    };
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("helper node has no role"))?
        .to_string();
    let optional = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_string);
    let states = object
        .get("states")
        .and_then(Value::as_object)
        .map(|values| {
            values
                .iter()
                .map(|(name, value)| (name.clone(), value.as_bool()))
                .collect()
        })
        .unwrap_or_default();
    let bounds = object
        .get("bounds")
        .and_then(Value::as_object)
        .and_then(|bounds| {
            Some(Bounds {
                x: i32::try_from(bounds.get("x")?.as_i64()?).ok()?,
                y: i32::try_from(bounds.get("y")?.as_i64()?).ok()?,
                width: i32::try_from(bounds.get("width")?.as_i64()?).ok()?,
                height: i32::try_from(bounds.get("height")?.as_i64()?).ok()?,
            })
        });
    let actions = object
        .get("actions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let provenance = object
        .get("provenance")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Ok(BackendNode {
        native_token: token,
        parent_index,
        role,
        subrole: optional("subrole"),
        name: optional("name"),
        description: optional("description"),
        value: optional("value"),
        states,
        bounds,
        actions,
        provenance,
    })
}

fn read_line(state: &mut HelperState, deadline: Instant) -> Result<Vec<u8>, AutomationError> {
    loop {
        if state.buffer.len() > MAX_RESPONSE_BYTES {
            return Err(driver_error(
                "DRIVER.OUTPUT_TOO_LARGE",
                "macOS AX helper response exceeded the frame limit",
            ));
        }
        if let Some(index) = state.buffer.iter().position(|byte| *byte == b'\n') {
            let line = state.buffer[..index].to_vec();
            state.buffer.drain(..=index);
            return Ok(line);
        }
        let timeout = remaining(deadline, false)?;
        let mut poll = libc::pollfd {
            fd: state.stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(&mut poll, 1, milliseconds) };
        if ready == 0 {
            return Err(super::timeout_error(false));
        }
        if ready < 0 {
            return Err(driver_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper poll failed",
            ));
        }
        let mut bytes = [0u8; 65_536];
        let count = state.stdout.read(&mut bytes).map_err(|error| {
            driver_error(
                "DRIVER.UNAVAILABLE",
                format!("macOS AX helper read failed: {error}"),
            )
        })?;
        if count == 0 {
            return Err(driver_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper exited before responding",
            ));
        }
        state.buffer.extend_from_slice(&bytes[..count]);
    }
}

fn terminate_state(state: &mut HelperState, force: bool) {
    if force {
        let _ = state.child.kill();
    } else {
        unsafe {
            libc::kill(state.child.id() as i32, libc::SIGTERM);
        }
    }
    let _ = state.child.wait();
}
fn channel_error(
    code: &str,
    message: &str,
    dispatched: bool,
    native_dispatch: bool,
    focus: bool,
) -> AutomationError {
    driver_error(code, message)
        .with_effect(if native_dispatch {
            "unknown"
        } else if focus {
            "contextual"
        } else {
            "not_applied"
        })
        .with_detail("helper_channel_failure", json!(true))
        .with_detail("helper_request_dispatched", json!(dispatched))
        .with_detail("dispatch_started", json!(native_dispatch))
        .with_detail("focus_changed", json!(focus))
}
fn invalid(message: impl Into<String>) -> AutomationError {
    driver_error("DRIVER.INVALID_REQUEST", message)
}
fn protocol_error(message: &str) -> AutomationError {
    driver_error("DRIVER.ACTION_FAILED", message)
        .with_detail("reason", json!("helper_protocol_error"))
}

fn find_default_helper() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(
                parent
                    .join(HELPER_BUNDLE_NAME)
                    .join("Contents/MacOS")
                    .join(HELPER_EXECUTABLE_NAME),
            );
        }
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/macos_ax/.build")
            .join(HELPER_BUNDLE_NAME)
            .join("Contents/MacOS")
            .join(HELPER_EXECUTABLE_NAME),
    );
    candidates.into_iter().find(|path| path.is_file())
}
fn helper_bundle(executable: &Path) -> Result<PathBuf, AutomationError> {
    let resolved = executable
        .canonicalize()
        .map_err(|_| driver_error("DRIVER.UNAVAILABLE", "macOS AX helper path is invalid"))?;
    let bundle = resolved
        .ancestors()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name == HELPER_BUNDLE_NAME)
        })
        .ok_or_else(|| {
            driver_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper must reside in the expected app bundle",
            )
        })?;
    let expected = bundle
        .join("Contents/MacOS")
        .join(HELPER_EXECUTABLE_NAME)
        .canonicalize()
        .map_err(|_| {
            driver_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper bundle layout is invalid",
            )
        })?;
    if resolved != expected {
        Err(driver_error(
            "DRIVER.UNAVAILABLE",
            "macOS AX helper bundle layout is invalid",
        ))
    } else {
        Ok(bundle.to_path_buf())
    }
}
fn validate_bundle(bundle: &Path) -> Result<(), AutomationError> {
    let plist = bundle.join("Contents/Info.plist");
    for (key, expected) in [
        ("CFBundleIdentifier", HELPER_BUNDLE_ID),
        ("CFBundleExecutable", HELPER_EXECUTABLE_NAME),
        ("CFBundlePackageType", "APPL"),
    ] {
        let output = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &format!("Print :{key}")])
            .arg(&plist)
            .output()
            .map_err(|_| {
                driver_error(
                    "DRIVER.UNAVAILABLE",
                    "macOS AX helper Info.plist could not be inspected",
                )
            })?;
        if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != expected {
            return Err(driver_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper bundle identity does not match",
            ));
        }
    }
    Ok(())
}
fn verify_signature(bundle: &Path) -> Result<(), AutomationError> {
    let status = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--deep", "--verbose=2"])
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| {
            driver_error(
                "DRIVER.UNAVAILABLE",
                "macOS AX helper signature could not be verified",
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(driver_error(
            "DRIVER.UNAVAILABLE",
            "macOS AX helper signature verification failed",
        ))
    }
}
