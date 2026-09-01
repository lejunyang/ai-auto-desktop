//! Subprocess-backed NDJSON plugin host.
//!
//! A plugin is an ordinary process that reads one JSON object per line from
//! stdin and writes one JSON object per line to stdout.  The host owns the
//! lifetime of that process: it starts it, performs the manifest handshake,
//! serializes invocations, enforces deadlines, and reclaims the whole process
//! tree on close.
//!
//! The most important property here is the `dispatched` flag on every error.
//! It records whether a request reached a successful stdin flush.  A caller
//! must not blindly retry a non-idempotent action after an ambiguous timeout,
//! and this flag is what makes that decision possible.

use aad_core::AutomationError;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod artifact;
pub mod manifest;

pub use artifact::{Artifact, ArtifactError, Frame, Receiver as ArtifactReceiver, Transfer};
pub use manifest::{ActionContract, CapabilityManifest};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A bounded queue stops a misbehaving plugin from making the host retain an
/// unlimited number of unsolicited stdout messages.
const STDOUT_QUEUE_SIZE: usize = 128;
const MAX_STDOUT_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Reserve most of a short startup budget for the request-based handshake.
const MANIFEST_PROBE: Duration = Duration::from_millis(100);
#[cfg(unix)]
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// A structured error raised by a plugin or by its host.
#[derive(Clone, Debug)]
pub struct PluginError {
    pub code: String,
    pub message: String,
    pub details: Map<String, Value>,
    pub retryable: bool,
}

impl PluginError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Map::new(),
            retryable: false,
        }
    }

    fn with_dispatched(mut self, dispatched: bool) -> Self {
        self.details
            .insert("dispatched".into(), Value::Bool(dispatched));
        self
    }

    fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    fn with_detail(mut self, key: &str, value: Value) -> Self {
        self.details.insert(key.to_string(), value);
        self
    }

    /// Whether the request reached a successful stdin flush boundary.
    pub fn dispatched(&self) -> bool {
        self.details.get("dispatched") == Some(&Value::Bool(true))
    }

    /// Whether this is a host/transport failure rather than a plugin response.
    pub fn is_host_error(&self) -> bool {
        self.code.starts_with("PLUGIN.HOST_")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "message": self.message,
            "details": Value::Object(self.details.clone()),
            "retryable": self.retryable,
        })
    }

    /// Convert into the workflow-level error contract.
    pub fn into_automation_error(self) -> AutomationError {
        let mut error = AutomationError::new(self.code.clone(), self.message.clone())
            .with_retryable(self.retryable)
            .with_details(self.details.clone());
        // An undispatched request provably never reached the plugin, so the
        // action cannot have been applied.
        error.effect = if self.dispatched() {
            "unknown".to_string()
        } else {
            "not_applied".to_string()
        };
        error
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PluginError {}

type Result<T> = std::result::Result<T, PluginError>;

/// An event observed on the plugin's stdout.
#[derive(Clone, Debug)]
enum StreamEvent {
    Line(String),
    Eof,
    Error(String),
    Closed,
}

/// Configuration for spawning a plugin process.
#[derive(Clone, Debug)]
pub struct PluginSpec {
    pub command: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: Option<Vec<(String, String)>>,
    pub timeout: Duration,
    pub name: Option<String>,
}

impl PluginSpec {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            cwd: None,
            env: None,
            timeout: DEFAULT_TIMEOUT,
            name: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

struct Shared {
    stderr: Mutex<String>,
    closed: AtomicBool,
    reader_stop: AtomicBool,
}

/// A live plugin process.
pub struct ProcessPlugin {
    name: String,
    timeout: Duration,
    manifest: Option<CapabilityManifest>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    events: Option<Receiver<StreamEvent>>,
    event_sender: Option<SyncSender<StreamEvent>>,
    readers: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
    /// Response ids that are paired but must be ignored, so a late proactive
    /// manifest cannot poison the first real invocation.
    discard_ids: HashSet<String>,
    #[cfg(windows)]
    job: Option<windows_job::Job>,
    #[cfg(windows)]
    tree_bounded: bool,
}

impl ProcessPlugin {
    /// Spawn the plugin and complete the manifest handshake.
    pub fn start(spec: PluginSpec) -> Result<Self> {
        if spec.command.is_empty() || spec.command.iter().any(String::is_empty) {
            return Err(PluginError::new(
                "PLUGIN.INVALID_REQUEST",
                "command must be a non-empty list of non-empty strings",
            )
            .with_dispatched(false));
        }
        let name = spec.name.clone().unwrap_or_else(|| {
            std::path::Path::new(&spec.command[0])
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| spec.command[0].clone())
        });

        let mut command = Command::new(&spec.command[0]);
        command
            .args(&spec.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = &spec.env {
            command.env_clear();
            for (key, value) in env {
                command.env(key, value);
            }
        }

        // On Windows the worker starts suspended so it can be placed in a Job
        // Object before it is able to spawn anything; on POSIX a new session
        // makes the process group the unit of cancellation.
        #[cfg(windows)]
        let job = windows_job::Job::create();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_SUSPENDED: u32 = 0x0000_0004;
            let mut flags = CREATE_NEW_PROCESS_GROUP;
            if job.is_some() {
                flags |= CREATE_SUSPENDED;
            }
            command.creation_flags(flags);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    // Detach into a new session so the whole tree can be
                    // signalled as one process group.
                    if libc_setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = command.spawn().map_err(|error| {
            PluginError::new(
                "PLUGIN.START_FAILED",
                format!("could not start plugin {name:?}: {error}"),
            )
            .with_dispatched(false)
            .with_retryable(true)
        })?;

        #[cfg(windows)]
        let mut tree_bounded = false;
        #[cfg(windows)]
        let job = {
            let mut job = job;
            if let Some(handle) = &job {
                if handle.assign(&child) {
                    tree_bounded = true;
                } else {
                    // The kernel refused assignment. The worker is still
                    // suspended, so nothing has been spawned; degrade to
                    // direct termination rather than failing the run.
                    job = None;
                }
            }
            if job.is_some() || tree_bounded {
                windows_job::resume(&child);
            } else {
                windows_job::resume(&child);
            }
            job
        };

        let shared = Arc::new(Shared {
            stderr: Mutex::new(String::new()),
            closed: AtomicBool::new(false),
            reader_stop: AtomicBool::new(false),
        });
        let (sender, receiver) = mpsc::sync_channel(STDOUT_QUEUE_SIZE);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let mut readers = Vec::new();
        {
            let sender = sender.clone();
            let shared = Arc::clone(&shared);
            readers.push(std::thread::spawn(move || {
                read_stdout(stdout, sender, shared);
            }));
        }
        {
            let shared = Arc::clone(&shared);
            readers.push(std::thread::spawn(move || {
                read_stderr(stderr, shared);
            }));
        }

        let mut plugin = Self {
            name,
            timeout: spec.timeout,
            manifest: None,
            child: Some(child),
            stdin,
            events: Some(receiver),
            event_sender: Some(sender),
            readers,
            shared,
            discard_ids: HashSet::new(),
            #[cfg(windows)]
            job,
            #[cfg(windows)]
            tree_bounded,
        };

        match plugin.handshake(Instant::now() + spec.timeout) {
            Ok(manifest) => {
                plugin.manifest = Some(manifest);
                Ok(plugin)
            }
            Err(error) => {
                plugin.close();
                Err(error)
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn manifest(&self) -> Option<&CapabilityManifest> {
        self.manifest.as_ref()
    }

    /// The bounded tail of stderr captured so far.
    pub fn stderr(&self) -> String {
        self.shared.stderr.lock().map(|text| text.clone()).unwrap_or_default()
    }

    /// Whether cancelling this worker provably reclaims its descendants.
    pub fn process_tree_bounded(&self) -> bool {
        #[cfg(unix)]
        {
            self.child.is_some()
        }
        #[cfg(windows)]
        {
            self.child.is_some() && self.tree_bounded
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    /// The manifest handshake: accept a proactive manifest, else request one.
    fn handshake(&mut self, deadline: Instant) -> Result<CapabilityManifest> {
        let probe_budget = deadline
            .saturating_duration_since(Instant::now())
            .div_f64(10.0)
            .min(MANIFEST_PROBE);
        let probe_deadline = (Instant::now() + probe_budget).min(deadline);

        if let Some(message) = self.read_message(probe_deadline, true)? {
            return self.manifest_from_message(message, None, false);
        }
        if Instant::now() >= deadline {
            return Err(self.host_error(
                "PLUGIN.HOST_TIMEOUT",
                format!("plugin {:?} did not respond before the deadline", self.name),
                true,
            ));
        }

        let request_id = new_request_id();
        let dispatched = self.write_request(&json!({"type": "manifest", "id": request_id}))?;
        let message = self
            .read_message(deadline, false)?
            .expect("a blocking read returns a message or fails");

        // A late proactive manifest is valid; remember the request id so its
        // eventual response can be paired and discarded.
        if message.get("id").is_none() {
            let manifest = self.manifest_from_message(message, None, dispatched)?;
            self.discard_ids.insert(request_id);
            Ok(manifest)
        } else {
            self.manifest_from_message(message, Some(&request_id), dispatched)
        }
    }

    /// Invoke an action and return its JSON result.
    ///
    /// The request shape is `{type, id, action, args, deadline_ms}`, where
    /// `deadline_ms` is an absolute Unix timestamp in milliseconds so the
    /// plugin can enforce the same budget the host is enforcing.
    pub fn invoke(&mut self, action: &str, args: Value, timeout: Option<Duration>) -> Result<Value> {
        if action.is_empty() {
            return Err(
                PluginError::new("PLUGIN.INVALID_REQUEST", "action must be a non-empty string")
                    .with_dispatched(false),
            );
        }
        let deadline = Instant::now() + timeout.unwrap_or(self.timeout);
        let request_id = new_request_id();

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(self.host_error(
                "PLUGIN.HOST_TIMEOUT",
                format!("plugin {:?} did not respond before the deadline", self.name),
                true,
            ));
        }
        let deadline_ms = epoch_millis() + remaining.as_millis() as u64;
        let request = json!({
            "type": "invoke",
            "id": request_id,
            "action": action,
            "args": args,
            "deadline_ms": deadline_ms,
        });

        let dispatched = match self.write_request(&request) {
            Ok(value) => value,
            Err(error) => return Err(self.maybe_abort(error)),
        };

        loop {
            let message = match self.read_message(deadline, false) {
                Ok(Some(message)) => message,
                Ok(None) => unreachable!("a blocking read returns a message or fails"),
                Err(error) => {
                    return Err(self.maybe_abort(error.with_dispatched(dispatched)));
                }
            };
            if let Some(Value::String(id)) = message.get("id") {
                if self.discard_ids.contains(id) {
                    let id = id.clone();
                    self.validate_discarded(&message, &id)?;
                    self.discard_ids.remove(&id);
                    continue;
                }
            }
            return match self.result_from_message(message, &request_id, dispatched) {
                Ok(value) => Ok(value),
                Err(error) => Err(self.maybe_abort(error)),
            };
        }
    }

    /// A host/transport failure invalidates the process; a plugin error does not.
    fn maybe_abort(&mut self, error: PluginError) -> PluginError {
        if error.is_host_error() {
            self.close();
        }
        error
    }

    fn result_from_message(
        &self,
        message: Map<String, Value>,
        expected_id: &str,
        dispatched: bool,
    ) -> Result<Value> {
        self.require_response_id(&message, expected_id)?;
        let has_result = message.contains_key("result");
        let has_error = message.contains_key("error");
        if has_result == has_error {
            return Err(self.host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin response must contain exactly one of result or error",
                false,
            ));
        }
        if has_error {
            return Err(self.plugin_error_from_message(&message, dispatched));
        }
        Ok(message["result"].clone())
    }

    fn manifest_from_message(
        &self,
        message: Map<String, Value>,
        expected_id: Option<&str>,
        dispatched: bool,
    ) -> Result<CapabilityManifest> {
        match expected_id {
            Some(id) => self.require_response_id(&message, id)?,
            None => {
                if message.contains_key("id") {
                    return Err(self.host_error(
                        "PLUGIN.HOST_PROTOCOL_ERROR",
                        "proactive manifest must not contain a request id",
                        false,
                    ));
                }
            }
        }
        if message.contains_key("error") {
            return Err(self.plugin_error_from_message(&message, dispatched));
        }

        // Plugins may wrap the manifest, or emit the bare canonical document.
        let raw = if let Some(value) = message.get("manifest") {
            value.clone()
        } else if let Some(value) = message.get("result") {
            value.clone()
        } else if message.get("type") == Some(&Value::String("manifest".into())) {
            Value::Object(
                message
                    .iter()
                    .filter(|(key, _)| key.as_str() != "id" && key.as_str() != "type")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            )
        } else if expected_id.is_none() {
            Value::Object(message.clone())
        } else {
            return Err(self.host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "expected a manifest response",
                false,
            ));
        };

        manifest::parse(&raw).map_err(|reason| {
            self.host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                format!("plugin manifest is invalid: {reason}"),
                false,
            )
        })
    }

    fn validate_discarded(&self, message: &Map<String, Value>, response_id: &str) -> Result<()> {
        self.require_response_id(message, response_id)?;
        if message.contains_key("error") {
            let error = self.plugin_error_from_message(message, true);
            if error.is_host_error() {
                return Err(error);
            }
            return Ok(());
        }
        self.manifest_from_message(message.clone(), Some(response_id), true)
            .map(|_| ())
    }

    fn require_response_id(&self, message: &Map<String, Value>, expected: &str) -> Result<()> {
        if message.get("id").and_then(Value::as_str) != Some(expected) {
            return Err(self
                .host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    "plugin response id does not match its request",
                    false,
                )
                .with_detail("expected_id", Value::String(expected.to_string()))
                .with_detail(
                    "actual_id",
                    message.get("id").cloned().unwrap_or(Value::Null),
                ));
        }
        Ok(())
    }

    fn plugin_error_from_message(
        &self,
        message: &Map<String, Value>,
        dispatched: bool,
    ) -> PluginError {
        let Some(Value::Object(payload)) = message.get("error") else {
            return self.host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin error payload must be a JSON object",
                false,
            );
        };
        let code = payload.get("code").and_then(Value::as_str);
        let text = payload.get("message").and_then(Value::as_str);
        if payload.contains_key("code") && code.is_none()
            || payload.contains_key("message") && text.is_none()
        {
            return self.host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin error code and message must be strings",
                false,
            );
        }
        let retryable = match payload.get("retryable") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return self.host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    "plugin error retryable field must be a boolean",
                    false,
                )
            }
        };

        // `data` is the canonical wire spelling; `details` is accepted from
        // early plugins and maps onto the same host field.
        let raw = payload.get("details").or_else(|| payload.get("data"));
        let mut details = match raw {
            None => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(other) => {
                let mut map = Map::new();
                map.insert("data".into(), other.clone());
                map
            }
        };
        if dispatched {
            details.insert("dispatched".into(), Value::Bool(true));
        }

        PluginError {
            code: code.unwrap_or("PLUGIN.ERROR").to_string(),
            message: text.unwrap_or("plugin invocation failed").to_string(),
            details,
            retryable,
        }
    }

    fn write_request(&mut self, request: &Value) -> Result<bool> {
        let encoded = format!("{}\n", serde_json::to_string(request).map_err(|error| {
            PluginError::new(
                "PLUGIN.INVALID_REQUEST",
                format!("request is not JSON serializable: {error}"),
            )
            .with_dispatched(false)
        })?);

        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(self.host_error(
                "PLUGIN.HOST_CLOSED",
                format!("plugin {:?} is closed", self.name),
                false,
            ));
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(self.host_error(
                "PLUGIN.HOST_CLOSED",
                format!("plugin {:?} is not running", self.name),
                false,
            ));
        };

        match stdin.write_all(encoded.as_bytes()).and_then(|()| stdin.flush()) {
            Ok(()) => Ok(true),
            Err(error) => Err(self
                .host_error(
                    "PLUGIN.HOST_IO_ERROR",
                    format!("could not write to plugin {:?}: {error}", self.name),
                    true,
                )
                .with_dispatched(false)),
        }
    }

    /// Read one JSON object, or `None` when `allow_timeout` and time ran out.
    fn read_message(
        &mut self,
        deadline: Instant,
        allow_timeout: bool,
    ) -> Result<Option<Map<String, Value>>> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if allow_timeout {
                    return Ok(None);
                }
                return Err(self.host_error(
                    "PLUGIN.HOST_TIMEOUT",
                    format!("plugin {:?} did not respond before the deadline", self.name),
                    true,
                ));
            }

            let Some(events) = self.events.as_ref() else {
                return Err(self.host_error(
                    "PLUGIN.HOST_CLOSED",
                    format!("plugin {:?} was closed", self.name),
                    false,
                ));
            };
            let event = match events.recv_timeout(remaining) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    if allow_timeout {
                        return Ok(None);
                    }
                    return Err(self.host_error(
                        "PLUGIN.HOST_TIMEOUT",
                        format!("plugin {:?} did not respond before the deadline", self.name),
                        true,
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => StreamEvent::Eof,
            };

            let line = match event {
                StreamEvent::Closed => {
                    return Err(self.host_error(
                        "PLUGIN.HOST_CLOSED",
                        format!("plugin {:?} was closed", self.name),
                        false,
                    ))
                }
                StreamEvent::Eof => {
                    let returncode = self
                        .child
                        .as_mut()
                        .and_then(|child| child.try_wait().ok().flatten())
                        .and_then(|status| status.code());
                    return Err(self
                        .host_error(
                            "PLUGIN.HOST_EOF",
                            format!("plugin {:?} closed stdout unexpectedly", self.name),
                            true,
                        )
                        .with_detail(
                            "returncode",
                            returncode.map(Value::from).unwrap_or(Value::Null),
                        ));
                }
                StreamEvent::Error(reason) => {
                    return Err(self.host_error("PLUGIN.HOST_PROTOCOL_ERROR", reason, false))
                }
                StreamEvent::Line(line) => line,
            };

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.trim().is_empty() {
                return Err(self.host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    "plugin emitted an empty stdout line",
                    false,
                ));
            }
            return match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Object(message)) => Ok(Some(message)),
                Ok(_) => Err(self.host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    "plugin response must be a JSON object",
                    false,
                )),
                Err(error) => Err(self
                    .host_error(
                        "PLUGIN.HOST_PROTOCOL_ERROR",
                        format!("plugin emitted invalid JSON: {error}"),
                        false,
                    )
                    .with_detail(
                        "line",
                        Value::String(trimmed.chars().take(500).collect()),
                    )),
            };
        }
    }

    fn host_error(&self, code: &str, message: impl Into<String>, retryable: bool) -> PluginError {
        let mut error = PluginError::new(code, message).with_retryable(retryable);
        let stderr = self.stderr();
        if !stderr.is_empty() {
            error = error.with_detail("stderr", Value::String(stderr));
        }
        error
    }

    /// Terminate the plugin process tree.  Idempotent.
    pub fn close(&mut self) {
        if self.shared.closed.swap(true, Ordering::SeqCst) && self.child.is_none() {
            return;
        }
        // Wake a request blocked on stdout even if the queue is full.
        if let Some(sender) = &self.event_sender {
            if let Err(TrySendError::Full(event)) = sender.try_send(StreamEvent::Closed) {
                let _ = event;
            }
        }
        self.stdin = None;

        if let Some(mut child) = self.child.take() {
            self.terminate(&mut child);
        }
        self.shared.reader_stop.store(true, Ordering::SeqCst);
        self.events = None;
        self.event_sender = None;
        for reader in std::mem::take(&mut self.readers) {
            let _ = reader.join();
        }
    }

    fn terminate(&mut self, child: &mut Child) {
        #[cfg(windows)]
        {
            // TerminateJobObject ends every member at once, including
            // descendants the host never saw.
            if let Some(job) = &self.job {
                if job.terminate() {
                    let _ = child.wait();
                    self.job = None;
                    return;
                }
            }
            self.job = None;
        }
        #[cfg(unix)]
        {
            // Signal the whole process group, not just the direct child.
            let pid = child.id() as i32;
            unsafe {
                libc_killpg(pid, 15);
            }
            let deadline = Instant::now() + TERMINATE_GRACE;
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            unsafe {
                libc_killpg(pid, 9);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for ProcessPlugin {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for ProcessPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessPlugin")
            .field("name", &self.name)
            .field("running", &self.child.is_some())
            .field(
                "manifest",
                &self.manifest.as_ref().map(|manifest| &manifest.name),
            )
            .finish()
    }
}

fn read_stdout(
    stdout: std::process::ChildStdout,
    sender: SyncSender<StreamEvent>,
    shared: Arc<Shared>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    loop {
        if shared.reader_stop.load(Ordering::SeqCst) {
            return;
        }
        buffer.clear();
        // Bound each line so one plugin cannot exhaust host memory.
        let mut limited = (&mut reader).take((MAX_STDOUT_LINE_BYTES + 1) as u64);
        match limited.read_until(b'\n', &mut buffer) {
            Ok(0) => {
                let _ = sender.send(StreamEvent::Eof);
                return;
            }
            Ok(_) => {
                if buffer.len() > MAX_STDOUT_LINE_BYTES {
                    let _ = sender.send(StreamEvent::Error(format!(
                        "stdout line exceeds {MAX_STDOUT_LINE_BYTES} bytes"
                    )));
                    return;
                }
                match String::from_utf8(buffer.clone()) {
                    Ok(line) => {
                        if sender.send(StreamEvent::Line(line)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender
                            .send(StreamEvent::Error(format!("stdout is not UTF-8: {error}")));
                        return;
                    }
                }
            }
            Err(error) => {
                if !shared.reader_stop.load(Ordering::SeqCst) {
                    let _ = sender
                        .send(StreamEvent::Error(format!("could not read stdout: {error}")));
                }
                return;
            }
        }
    }
}

fn read_stderr(mut stderr: std::process::ChildStderr, shared: Arc<Shared>) {
    let mut chunk = [0u8; 4096];
    loop {
        if shared.reader_stop.load(Ordering::SeqCst) {
            return;
        }
        match stderr.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let text = String::from_utf8_lossy(&chunk[..count]);
                if let Ok(mut buffer) = shared.stderr.lock() {
                    buffer.push_str(&text);
                    // Keep only the bounded tail.
                    if buffer.len() > MAX_STDERR_BYTES {
                        let excess = buffer.len() - MAX_STDERR_BYTES;
                        let boundary = buffer
                            .char_indices()
                            .map(|(index, _)| index)
                            .find(|index| *index >= excess)
                            .unwrap_or(buffer.len());
                        buffer.drain(..boundary);
                    }
                }
            }
        }
    }
}

fn new_request_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
    #[link_name = "killpg"]
    fn libc_killpg(pgrp: i32, sig: i32) -> i32;
}

#[cfg(windows)]
pub mod windows_job {
    //! A Job Object is the only Windows mechanism that reclaims descendants
    //! the host never saw, so it is the primary cancellation path.

    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenThread, ResumeThread, PROCESS_ALL_ACCESS,
    };

    pub struct Job(HANDLE);

    // The handle is owned exclusively by this struct.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn create() -> Option<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                // Any survivor is killed when the last handle closes.
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(handle);
                    return None;
                }
                Some(Self(handle))
            }
        }

        /// Apply kernel-enforced ceilings on memory, CPU time and process count.
        ///
        /// These are what make a runaway script survivable: the kernel stops it
        /// even if the script ignores every signal we send.
        pub fn set_limits(
            &self,
            memory_bytes: u64,
            cpu_seconds: u64,
            active_processes: u32,
        ) -> bool {
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                    | JOB_OBJECT_LIMIT_JOB_MEMORY
                    | JOB_OBJECT_LIMIT_JOB_TIME
                    | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.JobMemoryLimit = memory_bytes as usize;
                info.BasicLimitInformation.ActiveProcessLimit = active_processes;
                // PerJobUserTimeLimit counts in 100-nanosecond units.
                info.BasicLimitInformation.PerJobUserTimeLimit =
                    (cpu_seconds as i64).saturating_mul(10_000_000);
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0
            }
        }

        pub fn assign(&self, child: &Child) -> bool {
            unsafe {
                let process = OpenProcess(PROCESS_ALL_ACCESS, 0, child.id());
                if process.is_null() {
                    return false;
                }
                let ok = AssignProcessToJobObject(self.0, process);
                CloseHandle(process);
                ok != 0
            }
        }

        pub fn terminate(&self) -> bool {
            unsafe { TerminateJobObject(self.0, 1) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Resume a worker created with CREATE_SUSPENDED.
    pub fn resume(child: &Child) {
        // The main thread id is not exposed by std, so walk the process's
        // threads via the toolhelp snapshot the loader already provides.
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        const THREAD_SUSPEND_RESUME: u32 = 0x0002;
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot.is_null() {
                return;
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            if Thread32First(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32OwnerProcessID == child.id() {
                        let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                        if !thread.is_null() {
                            ResumeThread(thread);
                            CloseHandle(thread);
                        }
                    }
                    if Thread32Next(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }
    }
}
