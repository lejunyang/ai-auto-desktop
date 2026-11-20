use super::{
    driver_error, remaining, AutomationError, AxBackend, AxInput, BackendNode, BackendSnapshot,
    Bounds, Map, Value,
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
/// 3 added capture, window_action, paste, clipboard_read and global_key.
const HELPER_PROTOCOL_VERSION: u64 = 3;

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
        let mut command = Command::new(&helper);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
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
        self.rpc_controlled(operation, args, deadline, track_dispatch, None)
    }

    fn rpc_controlled(
        &self,
        operation: &str,
        args: Value,
        deadline: Instant,
        track_dispatch: bool,
        cancellation: Option<&aad_runtime::CancellationToken>,
    ) -> Result<Value, AutomationError> {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        remaining(deadline, false)?;
        let mut state = loop {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            remaining(deadline, false)?;
            match self.state.try_lock() {
                Ok(state) => break state,
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(channel_error(
                        "DRIVER.UNAVAILABLE",
                        "macOS AX helper state is unavailable",
                        false,
                        false,
                        false,
                    ))
                }
            }
        };
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
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        write_request(state, &encoded, deadline, cancellation)?;
        let mut dispatch_started = false;
        let mut focus_changed = false;
        loop {
            let line = read_line(state, deadline, cancellation).map_err(|error| {
                // SIGTERM gives the helper's independent release guard a
                // bounded cleanup window. SIGKILL would bypass owned key-up.
                // Even without progress, an in-flight request must be stopped.
                // Paste can be unwinding a bounded AX call before restoring
                // the clipboard. Its cleanup budget includes that 2 s call.
                let grace = if operation == "paste" {
                    Duration::from_millis(2500)
                } else {
                    Duration::from_millis(500)
                };
                terminate_state_with_grace(state, grace);
                let failure = channel_error(
                    &error.code,
                    &error.message,
                    true,
                    dispatch_started,
                    focus_changed,
                );
                // Cancellation can race a helper progress frame. Preserve the
                // observed progress flag, but do not infer absence of effects
                // from a missing frame after a complete write crossed the pipe.
                if error.code == "DRIVER.CANCELLED" {
                    failure.with_effect("unknown")
                } else {
                    failure
                }
            })?;
            let response: Value = serde_json::from_slice(&line).map_err(|_| {
                terminate_state(state);
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
                terminate_state(state);
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
                        == Some(true)
                    || progress
                        .get("native_dispatch_started")
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
                        == Some(true)
                    || details
                        .get("native_dispatch_started")
                        .and_then(Value::as_bool)
                        == Some(true);
                focus_changed |=
                    details.get("focus_changed").and_then(Value::as_bool) == Some(true);
                if track_dispatch && dispatch_started {
                    terminate_state(state);
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
                    })
                    .or_else(|| {
                        result
                            .get("native_dispatch_started")
                            .and_then(Value::as_bool)
                    });
                if declared != Some(dispatch_started) {
                    terminate_state(state);
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
        self.write_controlled(operation, args, deadline, None)
    }

    fn write_controlled(
        &self,
        operation: &str,
        args: Value,
        deadline: Instant,
        cancellation: Option<&aad_runtime::CancellationToken>,
    ) -> Result<Value, AutomationError> {
        let track = matches!(
            operation,
            "type_text"
                | "secret_input"
                | "pointer_click"
                | "pointer_gesture"
                | "drag"
                | "key_chord"
                | "paste"
                | "global_key"
        );
        let result = self.rpc_controlled(operation, args, deadline, track, cancellation)?;
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

impl Drop for SwiftHelperBackend {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                terminate_state(state);
            }
            *state = None;
        }
    }
}

impl AxBackend for SwiftHelperBackend {
    fn input_controlled(
        &self,
        token: &str,
        input: AxInput<'_>,
        deadline: Instant,
        cancellation: &aad_runtime::CancellationToken,
    ) -> Result<Value, AutomationError> {
        let mut args = json!({"native_token": token});
        let operation = match input {
            AxInput::Focus => "focus",
            AxInput::Invoke => "invoke",
            AxInput::PointerClick { button, position } => {
                args["button"] = json!(button);
                args["position"] = json!(position);
                "pointer_click"
            }
            AxInput::SetValue(text) => {
                args["value"] = json!(text);
                "set_value"
            }
            AxInput::TypeText(text) => {
                args["text"] = json!(text);
                "type_text"
            }
            AxInput::SecretInput(text) => {
                args["text"] = json!(text);
                "secret_input"
            }
            AxInput::Paste(text) => {
                args["text"] = json!(text);
                "paste"
            }
            AxInput::WindowAction(action) => {
                args["action"] = json!(action);
                "window_action"
            }
        };
        let result = self.write_controlled(operation, args, deadline, Some(cancellation))?;
        if operation == "paste" && result["consumed"] != true {
            return Err(
                driver_error("DRIVER.PASTE_NOT_CONSUMED", "paste input was not consumed")
                    .with_effect("unknown")
                    .with_detail("clipboard_restored", result["restored"].clone()),
            );
        }
        if operation == "window_action" && result["verified"] != true {
            return Err(
                driver_error("DRIVER.UNKNOWN_EFFECT", "window state was not verified")
                    .with_effect("unknown")
                    .with_detail("observed_state", result["observed_state"].clone()),
            );
        }
        Ok(result)
    }
    fn global_key_controlled(
        &self,
        keys: &[String],
        deadline: Instant,
        cancellation: &aad_runtime::CancellationToken,
    ) -> Result<Value, AutomationError> {
        self.write_controlled(
            "global_key",
            json!({"keys": keys}),
            deadline,
            Some(cancellation),
        )
    }
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
    fn secret_input(
        &self,
        token: &str,
        text: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.write(
            "secret_input",
            json!({"native_token": token, "text": text}),
            deadline,
        )
    }
    fn start_recording(
        &self,
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let started = self.rpc(
            "record_start",
            json!({"process_id": process_id}),
            deadline,
            false,
        )?;
        if !started["recording_id"].is_string() {
            return Err(protocol_error("record_start returned no recording id"));
        }
        Ok(started)
    }
    fn collect_recording(&self, id: &str, deadline: Instant) -> Result<Value, AutomationError> {
        let batch = self.rpc(
            "record_collect",
            json!({"recording_id": id}),
            deadline,
            false,
        )?;
        if !batch["events"].is_array() {
            return Err(protocol_error("record_collect returned no events"));
        }
        Ok(batch)
    }
    fn stop_recording(&self, id: &str, deadline: Instant) -> Result<(), AutomationError> {
        self.rpc("record_stop", json!({"recording_id": id}), deadline, false)?;
        Ok(())
    }
    fn pointer_gesture(
        &self,
        token: &str,
        gesture: &Value,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let mut args = gesture.clone();
        args["native_token"] = json!(token);
        self.write("pointer_gesture", args, deadline)
    }
    fn pointer_gesture_controlled(
        &self,
        token: &str,
        gesture: &Value,
        deadline: Instant,
        cancellation: &aad_runtime::CancellationToken,
    ) -> Result<Value, AutomationError> {
        let mut args = gesture.clone();
        args["native_token"] = json!(token);
        self.write_controlled("pointer_gesture", args, deadline, Some(cancellation))
    }
    fn drag(
        &self,
        token: &str,
        to_token: &str,
        request: &Value,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let mut args = request.clone();
        args["native_token"] = json!(token);
        args["to_token"] = json!(to_token);
        self.write("drag", args, deadline)
    }
    fn drag_controlled(
        &self,
        token: &str,
        to_token: &str,
        request: &Value,
        deadline: Instant,
        cancellation: &aad_runtime::CancellationToken,
    ) -> Result<Value, AutomationError> {
        let mut args = request.clone();
        args["native_token"] = json!(token);
        args["to_token"] = json!(to_token);
        self.write_controlled("drag", args, deadline, Some(cancellation))
    }
    fn key_chord(
        &self,
        token: &str,
        keys: &[String],
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.write(
            "key_chord",
            json!({"native_token": token, "keys": keys}),
            deadline,
        )
    }
    fn key_chord_held(
        &self,
        token: &str,
        keys: &[String],
        hold_ms: Option<u64>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let mut args = json!({"native_token": token, "keys": keys});
        if let Some(ms) = hold_ms {
            args["hold_ms"] = json!(ms);
        }
        self.write("key_chord", args, deadline)
    }
    fn key_chord_controlled(
        &self,
        token: &str,
        keys: &[String],
        hold_ms: Option<u64>,
        deadline: Instant,
        cancellation: &aad_runtime::CancellationToken,
    ) -> Result<Value, AutomationError> {
        let mut args = json!({"native_token": token, "keys": keys});
        if let Some(ms) = hold_ms {
            args["hold_ms"] = json!(ms);
        }
        self.write_controlled("key_chord", args, deadline, Some(cancellation))
    }
    fn window_action(
        &self,
        token: &str,
        action: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let result = self.write(
            "window_action",
            json!({"native_token": token, "action": action}),
            deadline,
        )?;
        if result["verified"] != true {
            return Err(driver_error(
                "DRIVER.UNKNOWN_EFFECT",
                "the window did not report the requested state in time",
            )
            .with_effect("unknown")
            .with_detail("observed_state", result["observed_state"].clone()));
        }
        Ok(result)
    }
    fn paste(&self, token: &str, text: &str, deadline: Instant) -> Result<Value, AutomationError> {
        let result = self.write(
            "paste",
            json!({"native_token": token, "text": text}),
            deadline,
        )?;
        if result["consumed"] != true {
            return Err(driver_error(
                "DRIVER.PASTE_NOT_CONSUMED",
                "Cmd+V reached the element but nothing asked the pasteboard for the text",
            )
            .with_effect("unknown")
            .with_detail("clipboard_restored", result["restored"].clone()));
        }
        Ok(result)
    }
    fn capture_element(&self, token: &str, deadline: Instant) -> Result<Value, AutomationError> {
        self.rpc("capture", json!({"native_token": token}), deadline, false)
    }
    fn clipboard_text(
        &self,
        max_characters: usize,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.rpc(
            "clipboard_read",
            json!({"max_characters": max_characters}),
            deadline,
            false,
        )
    }
    fn global_key(&self, keys: &[String], deadline: Instant) -> Result<Value, AutomationError> {
        self.write("global_key", json!({"keys": keys}), deadline)
    }
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                terminate_state(state);
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

/// A stalled helper must not trap cancellation while the request pipe is full.
/// Partial frames cannot be reused: terminate before another command is sent.
fn write_request(
    state: &mut HelperState,
    bytes: &[u8],
    deadline: Instant,
    cancellation: Option<&aad_runtime::CancellationToken>,
) -> Result<(), AutomationError> {
    let fd = state.stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(driver_error(
            "DRIVER.UNAVAILABLE",
            "helper request pipe cannot be made nonblocking",
        ));
    }
    let mut offset = 0;
    let result = (|| {
        while offset < bytes.len() {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            let timeout = remaining(deadline, false)?.min(Duration::from_millis(10));
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis().max(1) as i32) };
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(driver_error(
                    "DRIVER.UNAVAILABLE",
                    "helper request pipe poll failed",
                ));
            }
            match state.stdin.write(&bytes[offset..]) {
                Ok(0) => {
                    return Err(driver_error(
                        "DRIVER.UNAVAILABLE",
                        "helper request pipe closed",
                    ))
                }
                Ok(written) => offset += written,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    return Err(driver_error(
                        "DRIVER.UNAVAILABLE",
                        "helper request pipe write failed",
                    ))
                }
            }
        }
        Ok(())
    })();
    result.map_err(|error: AutomationError| {
        if offset > 0 {
            terminate_state(state);
        }
        channel_error(&error.code, &error.message, offset > 0, false, false)
    })
}

fn read_line(
    state: &mut HelperState,
    deadline: Instant,
    cancellation: Option<&aad_runtime::CancellationToken>,
) -> Result<Vec<u8>, AutomationError> {
    loop {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
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
        let mut timeout = remaining(deadline, false)?;
        if cancellation.is_some() {
            timeout = timeout.min(Duration::from_millis(10));
        }
        let mut poll = libc::pollfd {
            fd: state.stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(&mut poll, 1, milliseconds) };
        if ready == 0 {
            if cancellation.is_some() {
                continue;
            }
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

fn terminate_state(state: &mut HelperState) {
    terminate_state_with_grace(state, Duration::from_millis(500));
}

fn terminate_state_with_grace(state: &mut HelperState, cleanup: Duration) {
    // All shutdown paths first allow a held-input watchdog to release. The
    // cleanup budget is independent of the expired request's timeout.
    unsafe {
        libc::kill(-(state.child.id() as i32), libc::SIGTERM);
    }
    let grace = Instant::now() + cleanup;
    while Instant::now() < grace {
        if state.child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(-(state.child.id() as i32), libc::SIGKILL);
    }
    let _ = state.child.kill();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        root: PathBuf,
        executable: PathBuf,
    }

    impl Fixture {
        fn new(body: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "aad-macos-helper-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            let executable = root
                .join(HELPER_BUNDLE_NAME)
                .join("Contents/MacOS")
                .join(HELPER_EXECUTABLE_NAME);
            std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
            std::fs::write(&executable, body).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self { root, executable }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn backend(fixture: &Fixture) -> SwiftHelperBackend {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sh");
        command
            .arg(&fixture.executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        SwiftHelperBackend {
            state: Mutex::new(Some(HelperState {
                child,
                stdin,
                stdout,
                buffer: Vec::new(),
                request_number: 0,
            })),
            helper_source: "fixture".into(),
        }
    }

    #[test]
    fn controlled_non_hold_writes_use_the_explicit_rpc_path() {
        let fixture = Fixture::new(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  operation=$(printf '%s' "$line" | sed -n 's/.*"operation":"\([^"]*\)".*/\1/p')
  printf '{"id":"%s","progress":{"native_dispatch_started":true}}\n' "$id"
  printf '{"id":"%s","result":{"native_operation":"%s","native_dispatch_started":true,"submitted":true,"accepted":true,"verified":true,"consumed":true,"restored":true}}\n' "$id" "$operation"
done
"#,
        );
        let backend = backend(&fixture);
        let cancellation = aad_runtime::CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        for (input, operation) in [
            (AxInput::Focus, "focus"),
            (AxInput::Invoke, "invoke"),
            (AxInput::SetValue("fixture"), "set_value"),
            (AxInput::TypeText("fixture"), "type_text"),
            (AxInput::SecretInput("fixture"), "secret_input"),
            (AxInput::Paste("fixture"), "paste"),
            (AxInput::WindowAction("activate"), "window_action"),
            (
                AxInput::PointerClick {
                    button: "left",
                    position: "center",
                },
                "pointer_click",
            ),
        ] {
            let value = backend
                .input_controlled("fixture", input, deadline, &cancellation)
                .unwrap();
            assert_eq!(value["native_operation"], operation);
        }
        assert_eq!(
            backend
                .global_key_controlled(&["a".into()], deadline, &cancellation)
                .unwrap()["native_operation"],
            "global_key"
        );
    }

    #[test]
    fn cancelled_paste_keeps_cleanup_grace_for_a_bounded_ax_call() {
        let fixture = Fixture::new(
            r#"#!/bin/sh
trap 'sleep 0.6; printf restored > "$0.restored"; exit 0' TERM
IFS= read -r line
printf offered > "$0.offered"
while :; do sleep 0.01; done
"#,
        );
        let backend = backend(&fixture);
        let cancellation = aad_runtime::CancellationToken::new();
        let cancelling = cancellation.clone();
        let offered = PathBuf::from(format!("{}.offered", fixture.executable.display()));
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !offered.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let ready = offered.exists();
            cancelling.cancel();
            assert!(ready);
        });
        let started = Instant::now();
        let error = backend
            .input_controlled(
                "fixture",
                AxInput::Paste("fixture"),
                started + Duration::from_secs(5),
                &cancellation,
            )
            .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code, "DRIVER.CANCELLED");
        assert_eq!(error.effect, "unknown");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            std::fs::read_to_string(format!("{}.restored", fixture.executable.display())).unwrap(),
            "restored"
        );
        assert!(backend
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_some());
    }

    #[test]
    fn cancellation_interrupts_a_full_helper_request_pipe() {
        let fixture = Fixture::new(
            r#"#!/bin/sh
trap 'exit 0' TERM
dd bs=1 count=1 of=/dev/null 2>/dev/null
printf blocked > "$0.partial"
while :; do sleep 0.01; done
"#,
        );
        let backend = backend(&fixture);
        let cancellation = aad_runtime::CancellationToken::new();
        let cancelling = cancellation.clone();
        let partial = PathBuf::from(format!("{}.partial", fixture.executable.display()));
        let worker = std::thread::spawn(move || {
            let limit = Instant::now() + Duration::from_secs(2);
            while !partial.exists() && Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(1));
            }
            let dispatched = partial.exists();
            cancelling.cancel();
            assert!(
                dispatched,
                "fixture must consume a prefix before cancellation"
            );
        });
        let started = Instant::now();
        let error = backend
            .rpc_controlled(
                "type_text",
                json!({"text":"a".repeat(256 * 1024)}),
                started + Duration::from_secs(5),
                true,
                Some(&cancellation),
            )
            .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code, "DRIVER.CANCELLED");
        assert_eq!(
            error.effect, "not_applied",
            "an incomplete JSON frame cannot dispatch"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(backend
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_some());
    }

    #[test]
    fn cancellation_reaps_inflight_helper_only_after_cleanup_and_preserves_preflight() {
        let fixture = Fixture::new(
            r#"#!/bin/sh
trap 'printf released > "$0.release"; exit 0' TERM
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf held > "$0.held"
printf '{"id":"%s","progress":{"keyboard_dispatch_started":true}}\n' "$id"
while :; do sleep 0.01; done
"#,
        );
        let backend = backend(&fixture);
        let cancelled = aad_runtime::CancellationToken::new();
        cancelled.cancel();
        let error = backend
            .key_chord_controlled(
                "fixture",
                &["a".into()],
                Some(5000),
                Instant::now() + Duration::from_secs(10),
                &cancelled,
            )
            .unwrap_err();
        assert_eq!(error.code, "DRIVER.CANCELLED");
        assert_eq!(error.effect, "not_applied");
        assert_eq!(
            backend
                .state
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .request_number,
            0
        );

        let cancellation = aad_runtime::CancellationToken::new();
        let cancelling = cancellation.clone();
        let held_marker = PathBuf::from(format!("{}.held", fixture.executable.display()));
        let worker = std::thread::spawn(move || {
            let limit = Instant::now() + Duration::from_secs(2);
            while !held_marker.exists() && Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(1));
            }
            let held = held_marker.exists();
            cancelling.cancel();
            assert!(held, "fixture must receive the request before cancellation");
            Instant::now()
        });
        let error = backend
            .key_chord_controlled(
                "fixture",
                &["a".into()],
                Some(5000),
                Instant::now() + Duration::from_secs(10),
                &cancellation,
            )
            .unwrap_err();
        let cancelled_at = worker.join().unwrap();
        assert!(cancelled_at.elapsed() < Duration::from_secs(1));
        assert_eq!(error.code, "DRIVER.CANCELLED");
        assert_eq!(error.effect, "unknown");
        assert_eq!(
            std::fs::read_to_string(format!("{}.release", fixture.executable.display())).unwrap(),
            "released"
        );
        assert!(backend
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_some());
    }

    #[test]
    fn expired_input_request_gets_cleanup_grace_before_process_exit() {
        let fixture = Fixture::new(
            r#"#!/bin/sh
trap 'printf released > "$0.release"; exit 0' TERM
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"id":"%s","progress":{"keyboard_dispatch_started":true}}\n' "$id"
while :; do sleep 0.01; done
"#,
        );
        let backend = backend(&fixture);
        let started = Instant::now();
        let error = backend
            .key_chord_held(
                "fixture",
                &["a".into()],
                Some(20),
                started + Duration::from_millis(100),
            )
            .unwrap_err();
        assert_eq!(error.effect, "unknown");
        assert!(started.elapsed() < Duration::from_secs(2));
        let marker = PathBuf::from(format!("{}.release", fixture.executable.display()));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "released");
        assert!(backend
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_some());
    }

    #[test]
    fn protocol_round_trips_results_and_progress() {
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{"id":"%s","progress":{"phase":"keyboard_dispatch","keyboard_dispatch_started":true,"focus_changed":true}}\n' "$id"
  printf '{"id":"%s","result":{"native_operation":"CGEventKeyboardSetUnicodeString","submitted":true,"keyboard_dispatch_started":true,"focus_changed":true,"phase":"submitted"}}\n' "$id"
done
"#;
        let fixture = Fixture::new(script);
        let backend = backend(&fixture);
        let result = backend
            .rpc(
                "type_text",
                json!({"native_token": "n", "text": "hi"}),
                Instant::now() + Duration::from_secs(2),
                true,
            )
            .unwrap();
        assert_eq!(result["submitted"], true);
    }

    #[test]
    fn helper_errors_keep_pre_dispatch_effect() {
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{"id":"%s","error":{"code":"DRIVER.PROTECTED_ELEMENT","message":"blocked","retryable":false,"data":{"keyboard_dispatch_started":false,"focus_changed":false}}}\n' "$id"
done
"#;
        let fixture = Fixture::new(script);
        let backend = backend(&fixture);
        let error = backend
            .rpc(
                "type_text",
                json!({}),
                Instant::now() + Duration::from_secs(2),
                true,
            )
            .unwrap_err();
        assert_eq!(error.code, "DRIVER.PROTECTED_ELEMENT");
        assert_eq!(error.effect, "not_applied");
    }

    #[test]
    fn secret_native_write_progress_preserves_unknown_effect_on_channel_loss() {
        let script = r#"#!/bin/sh
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"id":"%s","progress":{"native_dispatch_started":true}}\n' "$id"
exit 0
"#;
        let fixture = Fixture::new(script);
        let error = backend(&fixture)
            .secret_input(
                "n",
                "synthetic-fixture",
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap_err();
        assert_eq!(error.effect, "unknown");
        assert!(!format!("{error:?}").contains("synthetic-fixture"));
    }

    /// A helper that answers every request with `result`, after a keyboard
    /// dispatch progress marker when `keyboard` is set.
    fn answering(result: &str, keyboard: bool) -> Fixture {
        let progress = if keyboard {
            r#"  printf '{"id":"%s","progress":{"phase":"keyboard_dispatch","keyboard_dispatch_started":true,"focus_changed":true}}\n' "$id"
"#
        } else {
            ""
        };
        Fixture::new(&format!(
            "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n{progress}  printf '{{\"id\":\"%s\",\"result\":{result}}}\\n' \"$id\"\ndone\n"
        ))
    }

    #[test]
    fn paste_that_nothing_consumed_is_an_unknown_effect() {
        let fixture = answering(
            r#"{"native_operation":"NSPasteboard+CGEventCmdV","submitted":true,"keyboard_dispatch_started":true,"focus_changed":true,"phase":"submitted","consumed":false,"restored":"restored"}"#,
            true,
        );
        let helper = backend(&fixture);
        let error = helper
            .paste("n", "text", Instant::now() + Duration::from_secs(2))
            .unwrap_err();
        assert_eq!(error.code, "DRIVER.PASTE_NOT_CONSUMED");
        assert_eq!(error.effect, "unknown");
        assert_eq!(error.details["clipboard_restored"], "restored");

        let fixture = answering(
            r#"{"native_operation":"NSPasteboard+CGEventCmdV","submitted":true,"keyboard_dispatch_started":true,"focus_changed":true,"phase":"submitted","consumed":true,"restored":"restored"}"#,
            true,
        );
        let helper = backend(&fixture);
        let pasted = helper
            .paste("n", "text", Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(pasted["consumed"], true);
    }

    #[test]
    fn unverified_window_state_is_an_unknown_effect() {
        let fixture = answering(
            r#"{"native_operation":"AXWindow.minimize","accepted":true,"verified":false,"observed_state":{"minimized":false}}"#,
            false,
        );
        let helper = backend(&fixture);
        let error = helper
            .window_action("w", "minimize", Instant::now() + Duration::from_secs(2))
            .unwrap_err();
        assert_eq!(error.code, "DRIVER.UNKNOWN_EFFECT");
        assert_eq!(error.effect, "unknown");
        assert_eq!(error.details["observed_state"]["minimized"], false);

        let fixture = answering(
            r#"{"native_operation":"AXWindow.minimize","accepted":true,"verified":true,"observed_state":{"minimized":true}}"#,
            false,
        );
        let helper = backend(&fixture);
        assert_eq!(
            helper
                .window_action("w", "minimize", Instant::now() + Duration::from_secs(2))
                .unwrap()["observed_state"]["minimized"],
            true
        );
    }

    #[test]
    fn global_keys_must_declare_their_dispatch() {
        // A helper that claims a submitted chord without the progress marker is inconsistent.
        let fixture = answering(
            r#"{"native_operation":"CGEventGlobalKeyChord","submitted":true,"keyboard_dispatch_started":true,"focus_changed":false,"phase":"submitted"}"#,
            false,
        );
        let helper = backend(&fixture);
        let error = helper
            .global_key(
                &["cmd".into(), "tab".into()],
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap_err();
        assert_eq!(error.code, "DRIVER.UNKNOWN_EFFECT");

        let fixture = answering(
            r#"{"native_operation":"CGEventGlobalKeyChord","submitted":true,"keyboard_dispatch_started":true,"focus_changed":false,"phase":"submitted"}"#,
            true,
        );
        let helper = backend(&fixture);
        assert_eq!(
            helper
                .global_key(
                    &["cmd".into(), "tab".into()],
                    Instant::now() + Duration::from_secs(2)
                )
                .unwrap()["submitted"],
            true
        );
    }

    #[test]
    fn capture_and_pasteboard_reads_pass_through_the_helper() {
        let fixture = answering(
            r#"{"format":"png","data":"iVBORw0KGgo=","width":2,"height":1,"clipped":false,"window_id":7}"#,
            false,
        );
        let helper = backend(&fixture);
        let shot = helper
            .capture_element("n", Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(shot["window_id"], 7);
        let read = helper
            .clipboard_text(5, Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            read["format"], "png",
            "the fixture answers one shape to every request"
        );
    }

    #[test]
    fn native_dispatch_progress_turns_channel_loss_into_unknown_effect() {
        let script = r#"#!/bin/sh
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"id":"%s","progress":{"phase":"pointer_dispatch","pointer_dispatch_started":true}}\n' "$id"
exit 1
"#;
        let fixture = Fixture::new(script);
        let backend = backend(&fixture);
        let error = backend
            .rpc(
                "pointer_click",
                json!({}),
                Instant::now() + Duration::from_secs(2),
                true,
            )
            .unwrap_err();
        assert_eq!(error.code, "DRIVER.UNAVAILABLE");
        assert_eq!(error.effect, "unknown");
    }
}
