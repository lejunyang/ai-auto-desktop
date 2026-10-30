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
use aad_cdp::launch::{self, Browser};
use aad_cdp::recording::{Options, Recorder};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

pub const NAMES: &[&str] = &[
    "browser_profiles",
    "browser_open",
    "browser_close",
    "browser_record_start",
    "browser_record_stop",
    "browser_downloads",
];

/// Browser tools that only read.
pub const READ_ONLY: &[&str] = &["browser_profiles", "browser_downloads"];

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
            name: "browser_open",
            description: "Start a browser for this session and attach the control tools \
to it: control_targets then lists its pages. Without a profile it starts empty and \
forgets everything when closed. profile_dir keeps a working profile between runs, so a \
login done once stays. copy_profile starts from a copy of an existing profile's logins \
(see browser_profiles); the server must allow it. Opening a new browser closes the one \
opened before. With `record`, recording starts before the URL loads, so the first \
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
    ]
}

struct Running {
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<Result<Value, String>>,
    out: PathBuf,
}

/// The browser and recording this server started.
#[derive(Default)]
pub struct Browsers {
    opened: Mutex<Option<Browser>>,
    recording: Mutex<Option<Running>>,
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
        name: &str,
        arguments: &Value,
    ) -> Result<Value, Value> {
        match name {
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
            "browser_open" => self.open(session, arguments),
            "browser_close" => {
                let recording = self.stop_recording().ok();
                session.release_browser_downloads();
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
                let out = self.start_recording(options(&arguments["record"], port, page)?)?;
                Ok(json!({"recording": true, "out": out}))
            }
            "browser_record_stop" => self.stop_recording(),
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
            other => Err(refused(
                "MCP.INVALID_ARGUMENT",
                format!("unknown tool {other}"),
                None,
            )),
        }
    }

    fn open(&self, session: &aad_control::Session, arguments: &Value) -> Result<Value, Value> {
        let browser = browser_of(arguments)?;
        let url = arguments["url"].as_str();
        if let Some(url) = url {
            let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
            if !matches!(scheme.as_deref(), Some("http" | "https"))
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
        // One browser per server: the session works with one at a time.
        let _ = self.stop_recording();
        session.release_browser_downloads();
        self.opened.lock().unwrap().take();
        let started = Browser::start(launch::Options {
            browser: browser.clone(),
            executable: None,
            profile_dir: arguments["profile_dir"].as_str().map(PathBuf::from),
            copy_from,
            profile_id: copy.unwrap_or("Default").to_string(),
            kill_browser: arguments["close_running"].as_bool().unwrap_or(false),
            headless: arguments["headless"].as_bool().unwrap_or(false),
        })
        .map_err(|e| {
            refused(
                "BROWSER.START_FAILED",
                e.to_string(),
                Some("If the profile is in use, close that browser or pass close_running with the person's agreement."),
            )
        })?;
        session
            .attach_launched_browser(started.port)
            .map_err(|e| e.to_json())?;
        let mut answer = json!({
            "browser": browser,
            "port": started.port,
            "page_id": started.page,
            "profile_dir": started.profile_dir().display().to_string(),
            "temporary_profile": arguments["profile_dir"].is_null(),
        });
        if arguments.get("record").is_some() {
            let options = options(&arguments["record"], started.port, started.page.clone())?;
            answer["recording"] = json!({"out": self.start_recording(options)?});
        }
        if let Some(url) = url {
            started.open_url(url).map_err(failed)?;
            answer["url"] = json!(url);
        }
        *self.opened.lock().unwrap() = Some(started);
        Ok(answer)
    }

    fn start_recording(&self, options: Options) -> Result<String, Value> {
        let mut slot = self.recording.lock().unwrap();
        if slot.is_some() {
            return Err(refused(
                "BROWSER.RECORDING",
                "a recording is already running",
                Some("Stop it with browser_record_stop first."),
            ));
        }
        let out = options.out.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let (ready, started) = mpsc::channel();
        let stop = cancel.clone();
        let thread = std::thread::spawn(move || {
            let recorder = match Recorder::start(options) {
                Ok(recorder) => {
                    let _ = ready.send(Ok(()));
                    recorder
                }
                Err(e) => {
                    let _ = ready.send(Err(e.to_string()));
                    return Err("did not start".to_string());
                }
            };
            recorder.run(&stop).map_err(|e| e.to_string())
        });
        match started.recv() {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                let _ = thread.join();
                return Err(failed(message));
            }
            Err(_) => return Err(failed("the recorder stopped before starting")),
        }
        *slot = Some(Running {
            cancel,
            thread,
            out: out.clone(),
        });
        Ok(out.display().to_string())
    }

    fn stop_recording(&self) -> Result<Value, Value> {
        let running = self
            .recording
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| refused("BROWSER.NOT_RECORDING", "no recording is running", None))?;
        running.cancel.store(true, Ordering::SeqCst);
        let summary = running
            .thread
            .join()
            .map_err(|_| failed("the recorder panicked"))?;
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
