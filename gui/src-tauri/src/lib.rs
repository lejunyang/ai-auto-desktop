//! The desktop shell's backend.
//!
//! Every command here is a thin, typed wrapper over `aad-uia`. The shell holds
//! no automation logic of its own: the same driver, the same snapshot store and
//! the same staleness rules serve the GUI, the CLI and the MCP server, so what
//! a user records in the GUI behaves identically when the CLI replays it.
//!
//! Two invariants are load-bearing:
//!
//! 1. The front end may only act on an element it has observed. Commands accept
//!    a `snapshot:revision:node` reference produced by a previous `describe`,
//!    never coordinates, so a write cannot land on an element nobody saw.
//! 2. Failures cross the boundary as the structured error contract rather than
//!    as prose, so the UI can distinguish "retry is safe" from "the UI moved".

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use aad_uia::backend::{default_snapshot_directory, DriverError, SnapshotStore};
use aad_uia::driver::UiaDriver;
use serde_json::{json, Map, Value};

pub use aad_runtime::recordings;

/// One driver action and the channel its answer goes back on.
struct Job {
    action: String,
    params: Value,
    reply: Sender<Result<Value, Value>>,
}

/// A handle to the driver, which lives on its own thread.
///
/// The thread is not an optimisation, it is a requirement, for two measured
/// reasons.
///
/// UI Automation needs a multithreaded COM apartment, while the window toolkit
/// needs a single-threaded one on the process's main thread, and an apartment
/// cannot be changed once joined. Creating the driver on the main thread makes
/// the window fail to open with `RPC_E_CHANGED_MODE`.
///
/// Second, enumerating windows reaches the application's own window, and that
/// crosses back into the main thread's apartment. If the main thread were
/// waiting for the driver at that moment the two would deadlock and the
/// interface would hang with no error. Commands are therefore `async`, which
/// keeps them off the main thread and leaves it free to pump messages.
struct Shell {
    // `Sender` is `Send` but not `Sync`, and Tauri shares state across threads.
    jobs: Mutex<Sender<Job>>,
}

impl Shell {
    fn new() -> Result<Self, DriverError> {
        let (jobs, queue) = channel::<Job>();
        // The backend is built on the worker thread, so the apartment it joins
        // is that thread's, never the main thread's.
        let (ready, started) = channel::<Result<(), DriverError>>();

        std::thread::Builder::new()
            .name("aad-uia".into())
            .spawn(move || {
                let driver = match native_backend() {
                    Ok(backend) => {
                        // The same on-disk store the CLI and MCP server use, so
                        // a reference minted here stays valid for `aad do`.
                        let store =
                            SnapshotStore::default().persisted(default_snapshot_directory());
                        let _ = ready.send(Ok(()));
                        UiaDriver::with_store(Arc::new(backend), store)
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };

                // Ends when the last sender drops, i.e. when the app closes.
                while let Ok(job) = queue.recv() {
                    let outcome = driver
                        .call(&job.action, &job.params)
                        .map_err(|error| error_payload(&error));
                    let _ = job.reply.send(outcome);
                }
            })
            .map_err(|error| {
                DriverError::unavailable(format!("cannot start the automation thread: {error}"))
            })?;

        match started.recv() {
            Ok(Ok(())) => Ok(Self {
                jobs: Mutex::new(jobs),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(DriverError::unavailable(
                "the automation thread stopped before it was ready",
            )),
        }
    }

    /// Run one driver action on the automation thread and wait for its answer.
    fn dispatch(&self, action: &str, params: Value) -> Result<Value, Value> {
        let (reply, answer) = channel();
        let job = Job {
            action: action.to_string(),
            params,
            reply,
        };
        {
            // The lock is released before waiting, so one slow read cannot
            // block every other command from being queued.
            let jobs = self.jobs.lock().map_err(|_| stopped())?;
            jobs.send(job).map_err(|_| stopped())?;
        }
        answer.recv().map_err(|_| stopped())?
    }
}

/// The automation thread is gone, so nothing can be observed or done.
fn stopped() -> Value {
    json!({
        "code": "GUI.BACKEND_STOPPED",
        "message": "The automation backend stopped responding.",
        "retryable": false,
        "effect": "unknown",
        "hint": "Restart the application.",
    })
}

/// The current platform's backend, or a clear refusal.
fn native_backend() -> Result<impl aad_uia::Backend, DriverError> {
    #[cfg(windows)]
    {
        aad_uia::windows::WindowsUiaBackend::new()
    }
    #[cfg(not(windows))]
    {
        Err::<aad_uia::backend::NullBackend, _>(DriverError::unavailable(
            "UI automation currently requires Windows",
        ))
    }
}

/// Render a driver error as the payload the front end knows how to display.
fn error_payload(error: &DriverError) -> Value {
    let mut payload = json!({
        "code": error.code,
        "message": error.message,
        "retryable": error.retryable,
        "effect": error.effect,
    });
    if !error.details.is_empty() {
        payload["details"] = Value::Object(error.details.clone());
    }
    if let Some(hint) = recovery_hint(&error.code) {
        payload["hint"] = json!(hint);
    }
    payload
}

/// What the user can actually do about a failure.
///
/// The codes are stable, so the advice belongs next to them rather than being
/// re-invented in each surface.
fn recovery_hint(code: &str) -> Option<&'static str> {
    match code {
        "DRIVER.STALE_HANDLE" => {
            Some("The interface changed after it was read. Re-read the window, then retry.")
        }
        "DRIVER.WINDOW_NOT_FOUND" => {
            Some("The window has closed. Refresh the list of running apps.")
        }
        "DRIVER.NOT_FOUND" => Some("Nothing matched. Re-read the window and check the filter."),
        "DRIVER.AMBIGUOUS_MATCH" => {
            Some("Several elements matched. Narrow the filter until one remains.")
        }
        "DRIVER.ACTION_UNSUPPORTED" => {
            Some("This element does not offer that action. Use one of the actions listed on it.")
        }
        "DRIVER.SNAPSHOT_EXPIRED" => Some("The reading expired. Re-read the window."),
        "DRIVER.INPUT_BLOCKED" => Some(
            "Another program is filtering synthetic input, so the click never reached the \
             window. Close macro or remote-control tools, or use this element's invoke or \
             set value action instead.",
        ),
        "STORE.NAME_INVALID" => Some(
            "Choose a name without slashes, colons or other path characters, and not \
             ending in a dot.",
        ),
        "STORE.NOT_FOUND" => Some("The file is no longer there. Refresh the list of recordings."),
        "STORE.PATH_REFUSED" => {
            Some("Only files in the recordings folder can be opened. Save a copy there first.")
        }
        "STORE.FILE_INVALID" => {
            Some("The file is not a readable recording. It may have been edited by hand.")
        }
        "STORE.WRITE_FAILED" => {
            Some("The recording could not be written. Check permissions on the folder.")
        }
        _ => None,
    }
}

#[tauri::command]
async fn list_apps(shell: tauri::State<'_, Shell>) -> Result<Value, Value> {
    shell.dispatch("list_windows", json!({}))
}

/// Read a window's interface and return a flat, addressable outline.
///
/// Flattening happens here rather than in the front end so the reference the UI
/// shows is exactly the one the driver will accept.
#[tauri::command]
async fn describe_window(
    shell: tauri::State<'_, Shell>,
    window_id: String,
    limit: Option<usize>,
) -> Result<Value, Value> {
    let described = shell.dispatch(
        "describe",
        json!({"window_id": window_id, "max_nodes": 1000, "max_depth": 32}),
    )?;

    let snapshot_id = described["snapshot_id"].as_str().unwrap_or_default();
    let revision = described["revision"].as_u64().unwrap_or_default();
    let nodes = described["nodes"].as_array().cloned().unwrap_or_default();

    // Only elements that can be acted on or identified are worth listing; the
    // rest would bury them.
    let limit = limit.unwrap_or(120).min(1000);
    let mut elements = Vec::new();
    for node in &nodes {
        if elements.len() >= limit {
            break;
        }
        let actions = node["actions"].as_array().cloned().unwrap_or_default();
        let name = node["name"].as_str().unwrap_or_default();
        if actions.is_empty() && name.is_empty() {
            continue;
        }
        let node_id = node["node_id"].as_str().unwrap_or_default();
        elements.push(json!({
            "node_id": node_id,
            "ref": format!("{snapshot_id}:{revision}:{node_id}"),
            "depth": node["depth"].as_u64().unwrap_or_default(),
            "summary": summarize(node),
            "actions": actions,
        }));
    }

    Ok(json!({
        "snapshot_id": snapshot_id,
        "revision": revision,
        "window": described["window"].clone(),
        "node_count": nodes.len(),
        "shown": elements.len(),
        "truncated": described["truncated"].as_bool().unwrap_or(false),
        "elements": elements,
    }))
}

/// A one-line description of an element, matching what the CLI prints.
fn summarize(node: &Value) -> String {
    let role = node["role"].as_str().unwrap_or("unknown");
    let mut summary = format!("role={role}");
    if let Some(name) = node["name"].as_str().filter(|name| !name.is_empty()) {
        summary.push_str(&format!(" name={name:?}"));
    }
    if let Some(id) = node["automation_id"]
        .as_str()
        .filter(|id| !id.is_empty())
    {
        summary.push_str(&format!(" id={id:?}"));
    }
    summary
}

/// Perform one action against a previously observed element.
#[tauri::command]
async fn act(
    shell: tauri::State<'_, Shell>,
    action: String,
    target: String,
    argument: Option<String>,
) -> Result<Value, Value> {
    let mut params = Map::new();
    params.insert("target".into(), json!(target));
    match action.as_str() {
        "set_value" => {
            params.insert("value".into(), json!(argument.unwrap_or_default()));
        }
        "type_text" => {
            params.insert("text".into(), json!(argument.unwrap_or_default()));
        }
        _ => {}
    }
    shell.dispatch(&action, Value::Object(params))
}

#[tauri::command]
async fn probe_environment() -> Value {
    aad_probe::probe().to_json()
}

/// Save the editable recording and its compiled workflow side by side.
///
/// Both are written in one command so they cannot drift apart: a workflow whose
/// recording says something else is a trap for whoever opens it next.
#[tauri::command]
async fn save_recording(
    name: String,
    document: Value,
    workflow: Value,
) -> Result<Value, Value> {
    let recording_path =
        recordings::save_recording(&name, &document).map_err(store_error)?;
    let workflow_path = recordings::save_workflow(&name, &workflow).map_err(store_error)?;
    Ok(json!({
        "recording_path": recording_path.to_string_lossy(),
        "workflow_path": workflow_path.to_string_lossy(),
    }))
}

/// Reopen a saved recording for editing.
#[tauri::command]
async fn load_recording(path: String) -> Result<Value, Value> {
    recordings::load_recording(std::path::Path::new(&path)).map_err(store_error)
}

/// The saved recordings available to open.
#[tauri::command]
async fn list_recordings() -> Result<Value, Value> {
    let found = recordings::list_recordings().map_err(store_error)?;
    Ok(json!({
        "recordings": found,
        "directory": recordings::recordings_dir().to_string_lossy(),
    }))
}

/// Render a store failure in the same shape as a driver failure.
///
/// The front end already knows how to display one structured error; giving it a
/// second shape for file problems would mean a second display path that only
/// gets exercised when something is already going wrong.
fn store_error(error: recordings::StoreError) -> Value {
    json!({
        "code": error.code,
        "message": error.message,
        "retryable": false,
        "hint": recovery_hint(error.code),
    })
}

pub fn run() {
    // Failing to construct the driver is fatal and must be visible: a window
    // that silently cannot see the desktop is worse than no window.
    let shell = match Shell::new() {
        Ok(shell) => shell,
        Err(error) => {
            eprintln!(
                "ai-auto-desktop: cannot start the UI automation backend: {}",
                error.message
            );
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .manage(shell)
        .invoke_handler(tauri::generate_handler![
            list_apps,
            describe_window,
            act,
            probe_environment,
            save_recording,
            load_recording,
            list_recordings
        ])
        .run(tauri::generate_context!())
        .expect("the desktop shell failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_names_the_element_when_it_has_a_name() {
        let node = json!({"role": "Button", "name": "Save", "automation_id": ""});
        assert_eq!(summarize(&node), r#"role=Button name="Save""#);
    }

    #[test]
    fn a_summary_survives_an_element_with_nothing_but_a_role() {
        let node = json!({"role": "Pane"});
        assert_eq!(summarize(&node), "role=Pane");
    }

    #[test]
    fn every_stale_or_missing_failure_tells_the_user_what_to_do() {
        // These are the failures a recording session actually hits, so leaving
        // any of them without advice would strand the user.
        for code in [
            "DRIVER.STALE_HANDLE",
            "DRIVER.WINDOW_NOT_FOUND",
            "DRIVER.NOT_FOUND",
            "DRIVER.AMBIGUOUS_MATCH",
            "DRIVER.ACTION_UNSUPPORTED",
            "DRIVER.SNAPSHOT_EXPIRED",
            "DRIVER.INPUT_BLOCKED",
        ] {
            assert!(recovery_hint(code).is_some(), "{code} has no hint");
        }
        assert!(recovery_hint("DRIVER.SOMETHING_NEW").is_none());
    }

    #[test]
    fn an_error_payload_carries_the_contract_fields() {
        let error = DriverError::new("DRIVER.STALE_HANDLE", "the UI moved on")
            .with_effect("not_applied");
        let payload = error_payload(&error);

        assert_eq!(payload["code"], "DRIVER.STALE_HANDLE");
        assert_eq!(payload["effect"], "not_applied");
        assert_eq!(payload["retryable"], false);
        assert!(payload["hint"].is_string());
    }
}
