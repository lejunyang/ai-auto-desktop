//! Starting a browser for the session, and recording what happens in it.
//!
//! The browser is started with its own user-data directory (temporary, a named
//! working directory kept between runs, or a copy of an existing profile's
//! logins), and the session's browser provider is attached to it, so the
//! control tools observe and act on its pages. A recording writes the page
//! actions, page changes, network requests with bodies and checkpoint
//! screenshots to a directory an agent reads afterwards; turning that into a
//! skill is the agent's job, not this server's.
use crate::tools::Tool;
use aad_browser::launch::{self, Browser};
use aad_browser::recording::{Options, Recorder};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

pub const NAMES: &[&str] = &[
    "browser_status",
    "browser_profiles",
    "browser_open",
    "browser_close",
    "browser_record_start",
    "browser_record_stop",
    "browser_downloads",
    "browser_storage",
    "browser_network",
];

/// Browser tools that only read.
pub const READ_ONLY: &[&str] = &[
    "browser_status",
    "browser_profiles",
    "browser_downloads",
    "browser_storage",
    "browser_network",
];

const BROWSERS: [&str; 3] = ["chrome", "edge", "brave"];

fn record_schema() -> Value {
    json!({
        "type": "object",
        "description": "What the recording keeps.",
        "properties": {
            "redact_network": {
                "type": "boolean",
                "description": "Replace cookies, tokens, passwords and similar values in \
            recorded requests and responses. Off by default: the raw values are kept so the \
            recording shows exactly what an API needs."
            },
            "body_limit": {
                "type": "integer", "minimum": 0, "maximum": 1048576,
                "description": "Most bytes kept of each text request or response body \
            (default 32768). 0 keeps no bodies."
            },
            "screenshots": {
                "type": "boolean",
                "description": "Capture the page at key moments (default true)."
            }
        },
        "additionalProperties": false
    })
}

pub fn catalogue() -> Vec<Tool> {
    vec![
        Tool {
            name: "browser_profiles",
            description: "List the profiles of an installed browser's own user data, \
by id and display name, for browser_open's copy_profile. Read-only.",
            schema: json!({
                "type": "object",
                "properties": {"browser": {"enum": BROWSERS}},
                "required": ["browser"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "browser_status",
            description: "Read configured browser ownership, process and profile mode. Does not probe endpoint liveness or read credentials.",
            schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
            mutating: false,
        },
        Tool {
            name: "browser_open",
            description: "Start a browser for this session and attach the control tools \
to it: control_targets then lists its pages. Without a profile it starts empty and \
forgets everything when closed. profile_dir keeps a working profile between runs, so a \
login done once stays. copy_profile starts from a copy of an existing profile's logins \
(see browser_profiles); the server must allow it. An already opened browser is preserved \
and replacement is refused until browser_close. Copying does not guarantee login migration. With `record`, recording starts before the URL loads, so the first \
requests are kept.",
            schema: json!({
                "type": "object",
                "properties": {
                    "browser": {"enum": BROWSERS},
                    "url": {
                        "type": "string", "minLength": 8, "maxLength": 2048,
                        "description": "An http or https address to open."
                    },
                    "profile_dir": {
                        "type": "string",
                        "description": "A working user-data directory, created if missing \
            and kept afterwards. Not the browser's everyday directory."
                    },
                    "copy_profile": {
                        "type": "string",
                        "description": "Id of an existing profile whose login files are \
            copied into the working profile. The browser using it must be closed first."
                    },
                    "close_running": {
                        "type": "boolean",
                        "description": "If a browser is using the profile to copy or the \
            working directory, close that browser first. Only when the person agreed: it \
            closes their windows."
                    },
                    "headless": {"type": "boolean", "description": "No visible window."},
                    "record": record_schema()
                },
                "required": ["browser"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "browser_close",
            description: "Stop any recording and close the browser browser_open started. \
A temporary profile is deleted; a profile_dir is kept.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutating: true,
        },
        Tool {
            name: "browser_record_start",
            description: "Record one browser page: actions on it, how the page changed, \
network requests and responses with bodies, and screenshots at key moments. Records the \
page browser_open opened unless page_id names another page of the attached browser \
(metadata.page_id in control_targets). Stop with browser_record_stop and read the files \
it lists.",
            schema: json!({
                "type": "object",
                "properties": {
                    "page_id": {"type": "string", "maxLength": 256},
                    "record": record_schema()
                },
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "browser_record_stop",
            description: "Stop the recording and report where it was written: steps.jsonl \
(actions and the changes they caused), evidence/network.jsonl (requests and responses), \
evidence/network-streams.jsonl (WebSocket and event-stream messages) and screenshots.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutating: true,
        },
        Tool {
            name: "browser_downloads",
            description: "List what the attached browser downloaded since this session \
connected to it (control_targets connects): each download's guid, url, suggested \
filename, state (in_progress, completed or canceled), bytes received and total, and the \
path it was saved to. Only for a browser browser_open started: its downloads go to a \
directory of this session's own, given as `directory`. A browser attached by port keeps \
its own download setting, and the answer says tracking: false with the reason. \
Read-only; read the files from `path`.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutating: false,
        },
        Tool {
            name: "browser_storage",
            description: "List the cookies the browser holds for a page or frame from \
control_targets (name, domain, path, expiry, session, http_only, secure, same_site, size) and \
its origin's localStorage keys with their value lengths. Read-only. Cookie values are shown \
only when the server was started with --allow-cookie-values; storage values never. To clear \
them use control_act clear_site_data on the page's document node.",
            schema: json!({
                "type": "object",
                "properties": {"resource_id": {"type": "string", "minLength": 1, "maxLength": 256}},
                "required": ["resource_id"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "browser_network",
            description: "List the recent requests of a browser page from control_targets, \
newest last, since this session first attached to it: method, url (credentials and \
sensitive query values redacted), resource type, state (pending, finished, failed, \
redirected), status, MIME type, duration and size. Up to 500 are held per page; `dropped` \
is an alias of evicted_requests (request cache evictions); dropped_events counts lost CDP events, not requests. omitted_by_limit counts retained requests excluded from this result. With include_bodies, finished text responses carry up to 4096 \
characters of body with sensitive JSON fields redacted. Read-only.",
            schema: json!({
                "type": "object",
                "properties": {
                    "resource_id": {"type": "string", "minLength": 1, "maxLength": 256},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50},
                    "include_bodies": {"type": "boolean", "default": false}
                },
                "required": ["resource_id"],
                "additionalProperties": false
            }),
            mutating: false,
        },
    ]
}

fn session_error(e: aad_core::AutomationError) -> Value {
    let mut payload = e.to_json();
    let hint = match payload["code"].as_str() {
        Some("BROWSER.NOT_CONNECTED") => {
            Some("Start one with browser_open, or start the server with --browser-port.")
        }
        Some("CONTROL.TARGET_UNKNOWN") => {
            Some("Call control_targets and pass a browser resource_id it returned.")
        }
        _ => None,
    };
    if let Some(hint) = hint {
        payload["hint"] = json!(hint);
    }
    payload
}

struct Running {
    reason: Arc<Mutex<Option<String>>>,
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<Result<Value, String>>>>,
    result: Mutex<Option<Result<Value, String>>>,
    completed: std::sync::Condvar,
    out: PathBuf,
}

impl Running {
    fn is_finished(&self) -> bool {
        self.result.lock().unwrap().is_some()
            || self
                .thread
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|t| t.is_finished())
    }
    fn abort(&self, message: &str) {
        let mut reason = self.reason.lock().unwrap();
        if reason.is_none() {
            *reason = Some(message.to_string());
            self.cancel.store(true, Ordering::SeqCst);
        }
    }
    fn join(&self) -> Result<Value, String> {
        let thread = self.thread.lock().unwrap().take();
        if let Some(thread) = thread {
            let result = thread
                .join()
                .unwrap_or_else(|_| Err("the recorder panicked".into()));
            *self.result.lock().unwrap() = Some(result.clone());
            self.completed.notify_all();
            result
        } else {
            let mut result = self.result.lock().unwrap();
            while result.is_none() {
                result = self.completed.wait(result).unwrap();
            }
            result.clone().unwrap()
        }
    }
}

fn abort_recording(recording: &Mutex<Option<Arc<Running>>>) {
    let running = recording.lock().unwrap().clone();
    if let Some(running) = running {
        running.abort("recording cancelled by session shutdown");
        let _ = running.join();
        let mut slot = recording.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &running))
        {
            slot.take();
        }
    }
}

#[cfg(test)]
mod recording_stop_tests {
    use super::*;

    fn recorder(graceful: bool) {
        let browsers = Browsers::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            if graceful {
                // Represents the final response-body request after stop.
                assert!(!worker_cancel.load(Ordering::SeqCst));
            } else {
                while !worker_cancel.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
            }
            Ok(json!({"body_kept": graceful}))
        });
        *browsers.recording.lock().unwrap() = Some(Arc::new(Running {
            reason: Arc::default(),
            cancel,
            stop,
            thread: Mutex::new(Some(thread)),
            result: Mutex::new(None),
            completed: std::sync::Condvar::new(),
            out: PathBuf::new(),
        }));
        let result = browsers.end_recording(graceful).unwrap();
        assert_eq!(result["summary"]["body_kept"], graceful);
    }

    #[test]
    fn explicit_stop_preserves_final_command() {
        recorder(true);
    }

    #[test]
    fn abort_interrupts_final_command() {
        recorder(false);
    }

    #[test]
    fn stalled_graceful_finish_is_cancelled() {
        let browsers = Browsers::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let thread = std::thread::spawn(move || {
            while !worker_cancel.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            Ok(json!({"interrupted": true}))
        });
        *browsers.recording.lock().unwrap() = Some(Arc::new(Running {
            reason: Arc::default(),
            cancel,
            stop: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(Some(thread)),
            result: Mutex::new(None),
            completed: std::sync::Condvar::new(),
            out: PathBuf::new(),
        }));
        let started = std::time::Instant::now();
        let result = browsers.finish_recording().unwrap();
        assert_eq!(result["summary"]["interrupted"], true);
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }
}

/// The browser and recording this server started.
#[derive(Default)]
pub struct Browsers {
    // Held across preparation and publication, and before every field/session
    // lock. Internal helpers never reacquire it.
    lifecycle: Mutex<()>,
    opened: Arc<Mutex<Option<Browser>>>,
    profile_mode: Arc<Mutex<Option<String>>>,
    recording: Arc<Mutex<Option<Arc<Running>>>>,
    /// Whether copy_profile may copy an existing profile's logins.
    pub allow_profile_copy: bool,
}

fn refused(code: &str, message: impl Into<String>, hint: Option<&str>) -> Value {
    let mut payload = json!({"code": code, "message": message.into(), "effect": "not_applied"});
    if let Some(hint) = hint {
        payload["hint"] = json!(hint);
    }
    payload
}

fn failed(error: impl std::fmt::Display) -> Value {
    json!({"code": "BROWSER.FAILED", "message": error.to_string(), "effect": "unknown"})
}

fn browser_of(arguments: &Value) -> Result<String, Value> {
    arguments["browser"]
        .as_str()
        .filter(|b| BROWSERS.contains(b))
        .map(str::to_string)
        .ok_or_else(|| {
            refused(
                "MCP.INVALID_ARGUMENT",
                "browser is chrome, edge or brave",
                None,
            )
        })
}

fn options(record: &Value, port: u16, page: String) -> Result<Options, Value> {
    if record
        .get("body_limit")
        .is_some_and(|v| v.as_u64().is_none_or(|n| n > 1024 * 1024))
    {
        return Err(failed(
            "body_limit must be an integer no greater than 1 MiB",
        ));
    }
    let directory = aad_runtime::recordings::recordings_dir().join("browser");
    std::fs::create_dir_all(&directory).map_err(failed)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(Options {
        port,
        page,
        out: directory.join(format!("{stamp}-{}", uuid::Uuid::new_v4().simple())),
        seconds: 0,
        redact_network: record["redact_network"].as_bool().unwrap_or(false),
        screenshots: record["screenshots"].as_bool().unwrap_or(true),
        body_limit: record["body_limit"].as_u64().unwrap_or(32_768) as usize,
        max_bytes: 100 * 1024 * 1024,
        follow_popups: true,
    })
}

impl Browsers {
    pub fn call(
        &self,
        session: &aad_control::Session,
        context: &aad_control::Context,
        name: &str,
        arguments: &Value,
    ) -> Result<Value, Value> {
        #[cfg(feature = "lifecycle-test-hooks")]
        if name == "browser_open" {
            if let Ok(path) = std::env::var("AAD_TEST_LIFECYCLE") {
                use std::io::Write;
                let contended = self.lifecycle.try_lock().is_err();
                writeln!(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .unwrap(),
                    "{contended}"
                )
                .unwrap();
            }
        }
        let _lifecycle = if name == "browser_status" {
            self.lifecycle.try_lock().map_err(|_| json!({"code":"BROWSER.BUSY","message":"browser lifecycle transition in progress; retry later"}))?
        } else {
            loop {
                context.remaining().map_err(|e| e.to_json())?;
                match self.lifecycle.try_lock() {
                    Ok(lock) => break lock,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        panic!("browser lifecycle poisoned")
                    }
                }
            }
        };
        #[cfg(feature = "lifecycle-test-hooks")]
        if name == "browser_open" {
            if let Ok(path) = std::env::var("AAD_TEST_EFFECT_GATE") {
                session
                    .browser_effect(context, || {
                        std::fs::write(&path, "entered").unwrap();
                        while !std::path::Path::new(&format!("{path}.release")).exists() {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    })
                    .map_err(|e| e.to_json())?;
            }
        }
        if name == "browser_open" || name == "browser_record_start" {
            session
                .browser_effect(context, || ())
                .map_err(|e| e.to_json())?;
        }
        match name {
            "browser_status" => {
                let opened = self.opened.lock().unwrap();
                let port = session.browser_port();
                Ok(json!({
                    "connection": if port.is_some() { "configured" } else { "not_configured" },
                    "liveness": "not_probed",
                    "port": port,
                    "ownership": if opened.is_some() { "tool" } else if port.is_some() { "external" } else { "none" },
                    "process_id": opened.as_ref().map(Browser::process_id),
                    "profile_mode": if opened.is_some() { self.profile_mode.lock().unwrap().clone() } else { port.map(|_| "attached_unknown".to_string()) },
                    "close_impact": if opened.is_some() { "closes tool browser; deletes temporary profile only" } else { "does not close external browser" },
                    "copy_limitation": "copied profile does not guarantee login migration",
                }))
            }
            "browser_profiles" => {
                let browser = browser_of(arguments)?;
                let root = launch::real_user_data(&browser).ok_or_else(|| {
                    refused(
                        "BROWSER.NOT_FOUND",
                        format!("no {browser} user data on this machine"),
                        None,
                    )
                })?;
                let profiles = launch::list_profiles(&root).map_err(failed)?;
                Ok(json!({"browser": browser, "profiles": profiles}))
            }
            "browser_open" => self.open(session, context, arguments),
            "browser_close" => {
                let recording = self.stop_recording().ok();
                session.detach_browser();
                *self.profile_mode.lock().unwrap() = None;
                let closed = self.opened.lock().unwrap().take().is_some();
                Ok(json!({"closed": closed, "recording": recording}))
            }
            "browser_record_start" => {
                let port = session.browser_port().ok_or_else(|| {
                    refused(
                        "BROWSER.NOT_CONNECTED",
                        "no browser is attached",
                        Some("Start one with browser_open, or start the server with --cdp-port."),
                    )
                })?;
                let page = match arguments["page_id"].as_str() {
                    Some(page) => page.to_string(),
                    None => self
                        .opened
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|b| b.page.clone())
                        .ok_or_else(|| {
                            refused(
                                "MCP.INVALID_ARGUMENT",
                                "page_id is required for a browser this server did not open",
                                None,
                            )
                        })?,
                };
                let out = self.start_recording(
                    session,
                    context,
                    options(&arguments["record"], port, page)?,
                )?;
                Ok(json!({"recording": true, "out": out}))
            }
            "browser_record_stop" => {
                self.end_recording_with_context(true, Some((context, session)))
            }
            "browser_downloads" => session
                .browser_downloads(std::time::Duration::from_secs(5))
                .map_err(|e| {
                    let mut payload = e.to_json();
                    if payload["code"] == "BROWSER.NOT_CONNECTED" {
                        payload["hint"] = json!(
                            "Start one with browser_open, or start the server with --browser-port."
                        );
                    }
                    payload
                }),
            "browser_storage" | "browser_network" => {
                let resource = arguments["resource_id"].as_str().ok_or_else(|| {
                    refused("MCP.INVALID_ARGUMENT", "resource_id is required", None)
                })?;
                let timeout = std::time::Duration::from_secs(10);
                if name == "browser_storage" {
                    session.browser_storage(resource, timeout)
                } else {
                    let limit = match &arguments["limit"] {
                        Value::Null => 50,
                        limit => limit.as_u64().ok_or_else(|| {
                            refused("MCP.INVALID_ARGUMENT", "limit is a whole number", None)
                        })? as usize,
                    };
                    session.browser_network(
                        resource,
                        limit,
                        arguments["include_bodies"].as_bool().unwrap_or(false),
                        timeout,
                    )
                }
                .map_err(session_error)
            }
            other => Err(refused(
                "MCP.INVALID_ARGUMENT",
                format!("unknown tool {other}"),
                None,
            )),
        }
    }

    fn open(
        &self,
        session: &aad_control::Session,
        context: &aad_control::Context,
        arguments: &Value,
    ) -> Result<Value, Value> {
        let browser = browser_of(arguments)?;
        let url = arguments["url"].as_str();
        if let Some(url) = url {
            let parsed = url::Url::parse(url);
            if !parsed
                .as_ref()
                .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
                || url.chars().any(char::is_control)
            {
                return Err(refused(
                    "MCP.INVALID_ARGUMENT",
                    "url must be http or https",
                    None,
                ));
            }
        }
        let copy = arguments["copy_profile"].as_str();
        if copy.is_some() && !self.allow_profile_copy {
            return Err(refused(
                "POLICY.DENIED",
                "copying an existing profile's logins was not allowed by whoever started this server",
                Some("Use profile_dir and log in once instead, or restart with aad mcp --allow-profile-copy."),
            ));
        }
        let copy_from = match copy {
            Some(id) => {
                let root = launch::real_user_data(&browser).ok_or_else(|| {
                    refused(
                        "BROWSER.NOT_FOUND",
                        format!("no {browser} user data on this machine"),
                        None,
                    )
                })?;
                launch::select_profile(&root, Some(id)).map_err(failed)?;
                Some(root)
            }
            None => None,
        };
        // Replacing a live owned browser cannot be made lossless when profiles
        // or close_running overlap. Require an explicit close before switching.
        if self.opened.lock().unwrap().is_some() {
            return Err(refused(
                "BROWSER.ALREADY_OPEN",
                "a tool-owned browser is already open; it has been preserved",
                Some("Use the current session, or explicitly call browser_close before opening another browser; closing loses its temporary profile."),
            ));
        }
        if self.recording.lock().unwrap().is_some() {
            return Err(refused(
                "BROWSER.RECORDING",
                "opening another browser would interrupt the current recording; the session has been preserved",
                Some("Stop the recording explicitly before opening another browser."),
            ));
        }
        // Validate recording arguments and its destination before starting a
        // candidate process. Port and page are filled after launch.
        let record_options = arguments
            .get("record")
            .map(|record| options(record, 0, String::new()))
            .transpose()?;
        if session.browser_port().is_some() && arguments["close_running"] == true {
            return Err(refused("BROWSER.UNSAFE_REPLACEMENT",
                "close_running could terminate the attached external browser; the existing session has been preserved",
                Some("Use an independent profile without close_running.")));
        }
        struct Guard<'a>(&'a aad_control::Session, &'a aad_control::Context);
        impl launch::Authority for Guard<'_> {
            fn cancellation(&self) -> Option<Arc<AtomicBool>> {
                Some(self.1.browser_cancellation())
            }
            fn check(&self) -> aad_browser::recording::Result<()> {
                self.1.remaining().map(|_| ()).map_err(|e| e.message.into())
            }
            fn navigation(&self) -> aad_browser::recording::Result<()> {
                self.0
                    .browser_navigation(self.1)
                    .map_err(|e| e.message.into())
            }
            fn effect(
                &self,
                action: &mut dyn FnMut() -> aad_browser::recording::Result<()>,
            ) -> aad_browser::recording::Result<()> {
                self.0
                    .browser_effect(self.1, action)
                    .map_err(|e| e.message)?
            }
        }
        let started = Browser::start_guarded(launch::Options {
            browser: browser.clone(),
            executable: None,
            profile_dir: arguments["profile_dir"].as_str().map(PathBuf::from),
            copy_from,
            profile_id: copy.unwrap_or("Default").to_string(),
            kill_browser: arguments["close_running"].as_bool().unwrap_or(false),
            headless: arguments["headless"].as_bool().unwrap_or(false),
        }, &Guard(session, context))
        .map_err(|e| {
            if let Err(cancelled) = context.remaining() { return cancelled.to_json(); }
            refused(
                "BROWSER.START_FAILED",
                e.to_string(),
                Some("If the profile is in use, close that browser or pass close_running with the person's agreement."),
            )
        })?;
        let mut answer = json!({
            "browser": browser,
            "port": started.port,
            "page_id": started.page,
            "profile_dir": started.profile_dir().display().to_string(),
            "temporary_profile": arguments["profile_dir"].is_null(),
        });
        #[cfg(feature = "lifecycle-test-hooks")]
        if let Ok(path) = std::env::var("AAD_TEST_CANDIDATE") {
            std::fs::write(path, format!("{} {}", started.process_id(), started.port)).unwrap();
        }
        #[cfg(feature = "lifecycle-test-hooks")]
        if let Ok(path) = std::env::var("AAD_TEST_CANDIDATE_LOG") {
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .unwrap(),
                "{}",
                started.process_id()
            )
            .unwrap();
        }
        if let Some(mut options) = record_options {
            options.port = started.port;
            options.page = started.page.clone();
            answer["recording"] = json!({"out": self.start_recording(session, context, options)?});
        }
        #[cfg(feature = "lifecycle-test-hooks")]
        if let Ok(path) = std::env::var("AAD_TEST_BEFORE_NAVIGATION") {
            std::fs::write(&path, "entered").unwrap();
            while !std::path::Path::new(&format!("{path}.release")).exists() {
                if context.remaining().is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if let Some(url) = url {
            if let Err(error) = started.open_url_guarded(url, &Guard(session, context)) {
                let _ = self.stop_recording();
                if let Err(cancelled) = context.remaining() {
                    return Err(cancelled.to_json());
                }
                return Err(failed(error));
            }
            answer["url"] = json!(url);
        }
        // Publish only after all fallible preparation has succeeded. A failed
        // candidate is dropped without invalidating the old session's handles.
        #[cfg(feature = "lifecycle-test-hooks")]
        let inject_attach_failure = std::env::var_os("AAD_TEST_ATTACH_FAILURE").is_some();
        #[cfg(not(feature = "lifecycle-test-hooks"))]
        let inject_attach_failure = false;
        let attachment = if inject_attach_failure {
            Err(failed("injected attachment failure"))
        } else {
            session
                .publish_browser_with_context(started.port, Some(context), || {
                    *self.profile_mode.lock().unwrap() = Some(
                        if copy.is_some() {
                            "copy"
                        } else if arguments["profile_dir"].is_string() {
                            "persistent"
                        } else {
                            "temporary"
                        }
                        .into(),
                    );
                    *self.opened.lock().unwrap() = Some(started);
                    let opened = self.opened.clone();
                    let recording = self.recording.clone();
                    let profile = self.profile_mode.clone();
                    Box::new(move || {
                        abort_recording(&recording);
                        opened.lock().unwrap().take();
                        profile.lock().unwrap().take();
                    })
                })
                .map_err(|e| e.to_json())
        };
        if let Err(error) = attachment {
            let _ = self.stop_recording();
            return Err(error);
        }
        Ok(answer)
    }

    fn start_recording(
        &self,
        session: &aad_control::Session,
        context: &aad_control::Context,
        options: Options,
    ) -> Result<String, Value> {
        #[cfg(feature = "lifecycle-test-hooks")]
        let options = {
            let mut options = options;
            if std::env::var_os("AAD_TEST_RECORDER_FAILURE").is_some() {
                options.max_bytes = 0;
            }
            options
        };
        let out = options.out.clone();
        let started = session
            .register_browser_resource(context, || {
                let mut slot = self.recording.lock().unwrap();
                if slot.is_some() {
                    return Err(aad_control::error(
                        "BROWSER.RECORDING",
                        "a recording is already running",
                    ));
                }
                let cancel = Arc::new(AtomicBool::new(false));
                let graceful = Arc::new(AtomicBool::new(false));
                let graceful_thread = graceful.clone();
                let reason = Arc::new(Mutex::new(None));
                let worker_reason = reason.clone();
                let (ready, started) = mpsc::channel();
                let stop = cancel.clone();
                let thread = std::thread::spawn(move || {
                    #[cfg(feature = "lifecycle-test-hooks")]
                    if let Ok(path) = std::env::var("AAD_TEST_RECORDER_START_BARRIER") {
                        let _ = std::fs::write(&path, b"entered");
                        while !stop.load(Ordering::SeqCst)
                            && !std::path::Path::new(&format!("{path}.release")).exists()
                        {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        if stop.load(Ordering::SeqCst) {
                            let _ = ready.send(Err("recording startup cancelled".to_string()));
                            return Err("recording startup cancelled".to_string());
                        }
                    }
                    let mut recorder =
                        match Recorder::start_cancellable(options, Some(stop.clone())) {
                            Ok(recorder) => {
                                let _ = ready.send(Ok(()));
                                recorder
                            }
                            Err(e) => {
                                let _ = ready.send(Err(e.to_string()));
                                return Err("did not start".to_string());
                            }
                        };
                    recorder.set_stop_reason(worker_reason);
                    recorder.run(&graceful_thread).map_err(|e| e.to_string())
                });
                *slot = Some(Arc::new(Running {
                    reason,
                    cancel,
                    stop: graceful,
                    thread: Mutex::new(Some(thread)),
                    result: Mutex::new(None),
                    completed: std::sync::Condvar::new(),
                    out: out.clone(),
                }));
                let recording = self.recording.clone();
                Ok((
                    started,
                    Box::new(move || {
                        abort_recording(&recording);
                    }) as Box<dyn FnOnce() + Send>,
                ))
            })
            .map_err(|e| e.to_json())?;
        // Do not hold the slot/publication lock while startup is in flight:
        // close already owns a cancellation handle and can reap the recorder.
        let startup = loop {
            if let Err(error) = context.remaining() {
                let _ = self.stop_recording();
                return Err(error.to_json());
            }
            match started.recv_timeout(std::time::Duration::from_millis(10)) {
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                result => break result,
            }
        };
        match startup {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                let _ = self.stop_recording();
                return Err(failed(message));
            }
            Err(_) => {
                let _ = self.stop_recording();
                return Err(failed("the recorder stopped before starting"));
            }
        }
        if let Err(error) = session.browser_effect(context, || ()) {
            let _ = self.stop_recording();
            return Err(error.to_json());
        }
        Ok(out.display().to_string())
    }

    fn stop_recording(&self) -> Result<Value, Value> {
        self.end_recording(false)
    }

    #[cfg(test)]
    fn finish_recording(&self) -> Result<Value, Value> {
        self.end_recording(true)
    }

    fn end_recording(&self, graceful: bool) -> Result<Value, Value> {
        self.end_recording_with_context(graceful, None)
    }

    fn end_recording_with_context(
        &self,
        graceful: bool,
        context: Option<(&aad_control::Context, &aad_control::Session)>,
    ) -> Result<Value, Value> {
        let running = self
            .recording
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| refused("BROWSER.NOT_RECORDING", "no recording is running", None))?;
        running.stop.store(true, Ordering::SeqCst);
        #[cfg(feature = "lifecycle-test-hooks")]
        if let Ok(path) = std::env::var("AAD_TEST_BODY_GATE") {
            let _ = std::fs::write(format!("{path}.stop"), b"stopping");
        }
        if graceful {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !running.is_finished() && std::time::Instant::now() < deadline {
                #[cfg(feature = "lifecycle-test-hooks")]
                if let Ok(path) = std::env::var("AAD_TEST_STOP_WAITING") {
                    let _ = std::fs::write(path, b"waiting");
                }
                if context.is_some_and(|(c, _)| c.remaining().is_err()) {
                    running.abort(if context.is_some_and(|(_, s)| s.is_closed()) {
                        "recording cancelled by session shutdown"
                    } else {
                        "recording cancelled by stop request"
                    });
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        if !running.is_finished() {
            let mut reason = running.reason.lock().unwrap();
            if reason.is_none() {
                *reason = Some(
                    if context.is_some_and(|(_, s)| s.is_closed()) {
                        "recording cancelled by session shutdown"
                    } else if context.is_some_and(|(c, _)| c.remaining().is_err()) {
                        "recording cancelled by stop request"
                    } else if graceful {
                        "recording graceful stop deadline exceeded (2000 ms)"
                    } else {
                        "recording cancelled by session shutdown"
                    }
                    .to_string(),
                );
                running.cancel.store(true, Ordering::SeqCst);
            }
        }
        let summary = running.join();
        let mut slot = self.recording.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &running))
        {
            slot.take();
        }
        drop(slot);
        let files = [
            "steps.jsonl",
            "evidence/network.jsonl",
            "evidence/network-extra.jsonl",
            "evidence/network-streams.jsonl",
            "evidence/actions.jsonl",
        ]
        .iter()
        .filter(|file| running.out.join(file).exists())
        .map(|file| json!(file))
        .collect::<Vec<_>>();
        Ok(json!({
            "out": running.out.display().to_string(),
            "files": files,
            "summary": summary.unwrap_or_else(|error| json!({"error": error})),
        }))
    }
}
