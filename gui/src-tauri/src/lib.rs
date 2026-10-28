//! The desktop shell's backend.
//!
//! Every command goes through one control session, the same one `aad control
//! serve`, the CLI and the MCP tools use, so the shell works wherever the
//! session does: Windows UIA, Linux AT-SPI, macOS AX and an explicitly
//! connected browser. The shell holds no automation logic of its own.
//!
//! Two invariants are load-bearing:
//!
//! 1. Steps are saved in the platform-neutral format of `aad_control::steps`:
//!    an application named as a person would, a locator and a control action.
//!    Nothing session-bound is stored, so a recording replays tomorrow, on
//!    another machine, through `aad run` as well as here.
//! 2. Failures cross the boundary as structured errors with a hint, so the UI
//!    can tell "retry is safe" from "the interface moved".

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aad_control::recording::{self, Recording};
use aad_control::steps::{self, App, Step};
use aad_control::{Expect, Locator, Operation, Policy, Request, Session, View};
use aad_core::AutomationError;
use serde_json::{json, Map, Value};

pub use aad_runtime::recordings;

const STEP_TIMEOUT: Duration = Duration::from_secs(30);

type Reply = Result<Value, Value>;
type Job = Box<dyn FnOnce(&mut Desk) + Send>;

/// What the automation thread owns.
struct Desk {
    session: Arc<Session>,
    browser_port: Option<u16>,
    capture: Option<Recording>,
    /// Numbers the ids of recorded steps, across recordings in one run.
    recorded: usize,
}

/// A handle to the automation thread.
///
/// The thread is a requirement, not an optimisation. UI Automation needs a
/// multithreaded COM apartment while the window toolkit needs a
/// single-threaded one on the main thread, and an apartment cannot be changed
/// once joined; the session is therefore built, and always used, on its own
/// thread. Enumerating windows also reaches this application's own window,
/// which crosses back into the main thread: commands are `async` so the main
/// thread stays free to pump messages while it waits.
struct Shell {
    // `Sender` is `Send` but not `Sync`, and Tauri shares state across threads.
    jobs: Mutex<Sender<Job>>,
}

fn open(browser_port: Option<u16>) -> Arc<Session> {
    // A person at the window is the one asking for input.
    Arc::new(Session::native_with_browser(
        Policy {
            allow_input: true,
            ..Default::default()
        },
        browser_port,
    ))
}

impl Shell {
    fn new() -> Self {
        let (jobs, queue) = channel::<Job>();
        std::thread::Builder::new()
            .name("aad-session".into())
            .spawn(move || {
                let mut desk = Desk {
                    session: open(None),
                    browser_port: None,
                    capture: None,
                    recorded: 0,
                };
                // Ends when the last sender drops, i.e. when the app closes.
                while let Ok(job) = queue.recv() {
                    job(&mut desk);
                }
                let _ = desk.stop_capture();
            })
            .expect("the automation thread can be started");
        Self {
            jobs: Mutex::new(jobs),
        }
    }

    /// Run `work` on the automation thread and wait for its answer.
    fn run(&self, work: impl FnOnce(&mut Desk) -> Reply + Send + 'static) -> Reply {
        let (reply, answer) = channel();
        let job: Job = Box::new(move |desk| {
            let _ = reply.send(work(desk));
        });
        {
            // Released before waiting, so one slow read does not block queuing.
            let jobs = self.jobs.lock().map_err(|_| stopped())?;
            jobs.send(job).map_err(|_| stopped())?;
        }
        answer.recv().map_err(|_| stopped())?
    }
}

fn stopped() -> Value {
    json!({
        "code": "GUI.BACKEND_STOPPED",
        "message": "自动化后台已停止响应。",
        "retryable": false,
        "effect": "unknown",
        "hint": "重新启动应用。",
    })
}

/// What a person can do about a failure. Codes are stable, so the advice
/// lives next to them rather than in each component.
fn hint(code: &str) -> Option<&'static str> {
    Some(match code {
        "CONTROL.STALE_TARGET" | "DRIVER.STALE_HANDLE" | "DRIVER.STALE_SNAPSHOT" => {
            "界面在读取之后变了。重新读取窗口再试。"
        }
        "CONTROL.AMBIGUOUS" | "DRIVER.AMBIGUOUS_MATCH" | "DRIVER.AMBIGUOUS" => {
            "定位条件匹配到多个元素。用“所在容器”或“旁边的文字”收窄，最后才用“第几个”。"
        }
        "CONTROL.NOT_FOUND" | "DRIVER.NOT_FOUND" => "没有匹配的元素。重新读取窗口，检查定位条件。",
        "CONTROL.ACTION_UNSUPPORTED" | "DRIVER.ACTION_UNSUPPORTED" => {
            "这个元素不支持这个操作，换一个它列出的操作。"
        }
        "CONTROL.LOCATOR_UNSUPPORTED" => "这个平台只按角色和名称精确匹配，去掉其他条件。",
        "CONTROL.VIEW_UNSUPPORTED" => "只有 Windows 会划分区域，直接看元素列表。",
        "CONTROL.OCCLUDED" => "有别的东西盖在元素上（对话框、横幅、菜单），先处理它。",
        "CONTROL.OBSERVATION_TRUNCATED" => "窗口元素太多，没法确认唯一。换一个更小的窗口。",
        "DRIVER.PROTECTED_ELEMENT" => "这是受保护的输入框（如密码），值不会被读取或保存。",
        "DRIVER.INPUT_BLOCKED" => {
            "有程序在拦截模拟输入。关掉宏或远程控制工具，或改用这个元素的“触发”或“填写”。"
        }
        "STEP.APP_NOT_FOUND" => "找不到这个步骤的窗口。先打开它，或在步骤里改应用条件。",
        "STEP.APP_AMBIGUOUS" => "有多个窗口符合步骤的应用条件。给步骤加上窗口标题。",
        "STEP.CHECK_FAILED" => "等待的元素状态没有出现。延长等待时间，或检查定位条件。",
        "STEP.INVALID" => "步骤本身不完整，按提示修改后再试。",
        "RECORDING.UNSUPPORTED" => {
            "这个窗口不能自动录制（浏览器页面用 aad browser-record），可以用“添加步骤”逐步搭建。"
        }
        "DRIVER.UNAVAILABLE" => {
            "需要的系统组件不可用：Linux 要 KDE/X11 和 XTest helper，macOS 要辅助功能授权。"
        }
        "STORE.NAME_INVALID" => "换一个名字：不要斜杠、冒号等路径字符，也不要以点结尾。",
        "STORE.NOT_FOUND" => "文件已经不在了，刷新列表。",
        "STORE.PATH_REFUSED" => "只能打开录制文件夹里的文件。",
        "STORE.FILE_INVALID" => "这不是可读的录制文件，可能被手工改坏了。",
        "STORE.WRITE_FAILED" => "写不进录制文件夹，检查它的权限。",
        _ => return None,
    })
}

fn payload(code: &str, message: &str, effect: &str, details: Option<Value>) -> Value {
    let mut payload = json!({"code": code, "message": message, "effect": effect});
    if let Some(details) = details.filter(|d| d.as_object().is_some_and(|d| !d.is_empty())) {
        payload["details"] = details;
    }
    if let Some(hint) = hint(code) {
        payload["hint"] = json!(hint);
    }
    payload
}

fn failure(error: &AutomationError) -> Value {
    let mut value = payload(
        &error.code,
        &error.message,
        &error.effect,
        Some(Value::Object(error.details.clone())),
    );
    value["retryable"] = json!(error.retryable);
    value
}

fn store_error(error: recordings::StoreError) -> Value {
    payload(error.code, &error.message, "not_applied", None)
}

fn invalid(message: impl AsRef<str>) -> Value {
    payload("GUI.INVALID_REQUEST", message.as_ref(), "not_applied", None)
}

impl Desk {
    fn call(&self, operation: Operation) -> Reply {
        let mut request = Request::new(Some(self.session.id().to_string()), operation);
        request.timeout_ms = aad_control::MAX_TIMEOUT_MS;
        let response = self.session.execute(&request);
        match (response.data, response.error) {
            (Some(data), _) => Ok(data),
            (None, error) => {
                let error = error.unwrap_or_default();
                Err(payload(
                    error["code"].as_str().unwrap_or("CONTROL.FAILED"),
                    error["message"].as_str().unwrap_or("操作被拒绝"),
                    &response.effect,
                    error.get("details").cloned(),
                ))
            }
        }
    }

    /// Every window and page, each with the name a saved step would use.
    fn targets(&self) -> Reply {
        let listed = self.call(Operation::Targets {})?;
        let all = listed["resources"].as_array().cloned().unwrap_or_default();
        let resources: Vec<Value> = all
            .iter()
            .map(|resource| {
                let mut resource = resource.clone();
                resource["app"] = json!(steps::describe(&resource, &all));
                resource["recordable"] = json!(recording::supported(&self.session, &resource));
                resource
            })
            .collect();
        Ok(json!({
            "resources": resources,
            "unavailable": listed["unavailable"],
            "recording_supported": resources.iter().any(|r| r["recordable"] == true),
            "browser_port": self.browser_port,
        }))
    }

    fn resource(&self, resource_id: &str) -> Reply {
        let listed = self.targets()?;
        listed["resources"]
            .as_array()
            .and_then(|all| all.iter().find(|r| r["resource_id"] == resource_id))
            .cloned()
            .ok_or_else(|| {
                payload(
                    "STEP.APP_NOT_FOUND",
                    "这个窗口已经关闭了",
                    "not_applied",
                    None,
                )
            })
    }

    fn observe(&self, resource_id: &str, view: View) -> Reply {
        let resource = self.resource(resource_id)?;
        let mut observed = self.call(Operation::Observe {
            resource_id: resource_id.into(),
            max_nodes: aad_control::MAX_NODES,
            view,
        })?;
        // The locator a step would save, worked out once by the backend so the
        // front end never invents one.
        if let Some(nodes) = observed["nodes"].as_array_mut() {
            for node in nodes {
                node["step_locator"] = json!(steps::locator_for(node));
                if let Some(object) = node.as_object_mut() {
                    object.remove("target");
                }
            }
        }
        observed["app"] = resource["app"].clone();
        Ok(observed)
    }

    /// What a locator selects in a window right now. A locator that matches
    /// nothing is a state to show while it is being edited, not an error.
    fn try_locator(&self, resource_id: &str, locator: Locator) -> Reply {
        self.resource(resource_id)?;
        let observed = self.call(Operation::Observe {
            resource_id: resource_id.into(),
            max_nodes: aad_control::MAX_NODES,
            view: View::default(),
        })?;
        let mut found = self.call(Operation::Find {
            observation_id: observed["observation_id"]
                .as_str()
                .unwrap_or_default()
                .into(),
            locator: Box::new(locator),
            expect: Expect::Optional,
        })?;
        if let Some(node) = found.get_mut("node").and_then(Value::as_object_mut) {
            node.remove("target");
        }
        if let Some(object) = found.as_object_mut() {
            object.remove("target");
            object.remove("observation_id");
        }
        Ok(found)
    }

    fn start_capture(&mut self, resource_id: &str) -> Reply {
        let _ = self.stop_capture();
        let resource = self.resource(resource_id)?;
        let app: App = serde_json::from_value(resource["app"].clone()).unwrap_or_default();
        let started = Recording::start(&self.session, &resource, app, self.recorded + 1)
            .map_err(|e| failure(&e))?;
        self.capture = Some(started);
        Ok(json!({"recording": true}))
    }

    /// The steps recorded since the last call, already in the saved format.
    fn collect(&mut self) -> Reply {
        let Some(capture) = self.capture.as_mut() else {
            return Ok(json!({"steps": [], "skipped": [], "recording": false}));
        };
        let batch = capture.collect().map_err(|e| failure(&e))?;
        self.recorded = capture.next_id() - 1;
        let mut answer = batch.to_json();
        answer["recording"] = json!(true);
        Ok(answer)
    }

    /// Stop recording, with whatever was recorded since the last collection.
    fn stop_capture(&mut self) -> Reply {
        let Some(capture) = self.capture.take() else {
            return Ok(json!({"steps": [], "skipped": [], "recording": false}));
        };
        self.recorded = capture.next_id() - 1;
        let batch = capture.stop().map_err(|e| failure(&e))?;
        let mut answer = batch.to_json();
        answer["recording"] = json!(false);
        Ok(answer)
    }

    fn perform(&self, step: &Step, inputs: &Map<String, Value>) -> Reply {
        steps::perform(&self.session, step, inputs, STEP_TIMEOUT).map_err(|e| failure(&e))
    }

    /// Replay steps through the workflow engine, exactly as `aad run` would.
    fn replay(&self, name: &str, steps: &[Step], inputs: Map<String, Value>) -> Reply {
        let document = steps::workflow(name, steps).map_err(|e| failure(&e))?;
        let descriptor = aad_core::compile_descriptor(document, None).map_err(|e| failure(&e))?;
        let mut providers = aad_runtime::ProviderRegistry::new();
        providers.insert(Arc::new(steps::StepProvider::new(self.session.clone())));
        let result = aad_runtime::engine::run(
            &descriptor,
            aad_runtime::RunOptions::default()
                .with_providers(providers)
                .with_inputs(inputs.into_iter().collect())
                // Pressing Run is the person's grant.
                .with_granted_permissions(["desktop.input", "desktop.observe"]),
        );
        let outcomes: Vec<Value> = result
            .events
            .iter()
            .filter(|event| event.event_type == "step.finished")
            .map(|event| {
                let error = event.payload.get("error").cloned().filter(|e| !e.is_null());
                json!({
                    "id": event.payload.get("id"),
                    "status": event.payload.get("status"),
                    "error": error.map(|e| {
                        let code = e["code"].as_str().unwrap_or("STEP.FAILED").to_string();
                        payload(
                            &code,
                            e["message"].as_str().unwrap_or_default(),
                            e["effect"].as_str().unwrap_or("unknown"),
                            e.get("details").cloned(),
                        )
                    }),
                })
            })
            .collect();
        Ok(json!({
            "status": result.status.as_str(),
            "steps": outcomes,
            "error": result.error.as_ref().map(failure),
            "duration_seconds": result.duration_seconds,
        }))
    }
}

fn parse_steps(value: Value) -> Result<Vec<Step>, Value> {
    serde_json::from_value(value).map_err(|e| invalid(format!("步骤无法读取：{e}")))
}

fn save(name: &str, steps: &[Step]) -> Reply {
    for step in steps {
        step.validate().map_err(|e| failure(&e))?;
    }
    let workflow = steps::workflow(name, steps).map_err(|e| failure(&e))?;
    let recording_path =
        recordings::save_recording(name, &steps::recording(name, steps)).map_err(store_error)?;
    let workflow_path = recordings::save_workflow(name, &workflow).map_err(store_error)?;
    Ok(json!({
        "recording_path": recording_path.to_string_lossy(),
        "workflow_path": workflow_path.to_string_lossy(),
    }))
}

fn open_saved(path: &str) -> Reply {
    let document = recordings::load_recording(std::path::Path::new(path)).map_err(store_error)?;
    let steps = steps::steps_of(&document).map_err(|e| failure(&e))?;
    Ok(json!({"name": document["metadata"]["name"], "steps": steps}))
}

#[tauri::command]
async fn list_targets(shell: tauri::State<'_, Shell>) -> Reply {
    shell.run(|desk| desk.targets())
}

#[tauri::command]
async fn observe(shell: tauri::State<'_, Shell>, resource_id: String, view: Value) -> Reply {
    let view: View = serde_json::from_value(view).map_err(|e| invalid(e.to_string()))?;
    shell.run(move |desk| desk.observe(&resource_id, view))
}

#[tauri::command]
async fn try_locator(shell: tauri::State<'_, Shell>, resource_id: String, locator: Value) -> Reply {
    let locator: Locator =
        serde_json::from_value(locator).map_err(|e| invalid(format!("定位条件无法读取：{e}")))?;
    shell.run(move |desk| desk.try_locator(&resource_id, locator))
}

#[tauri::command]
async fn perform_step(shell: tauri::State<'_, Shell>, step: Value, inputs: Option<Value>) -> Reply {
    let step: Step =
        serde_json::from_value(step).map_err(|e| invalid(format!("步骤无法读取：{e}")))?;
    let inputs = inputs
        .and_then(|i| i.as_object().cloned())
        .unwrap_or_default();
    shell.run(move |desk| desk.perform(&step, &inputs))
}

#[tauri::command]
async fn run_steps(
    shell: tauri::State<'_, Shell>,
    name: String,
    steps: Value,
    inputs: Option<Value>,
) -> Reply {
    let steps = parse_steps(steps)?;
    let inputs = inputs
        .and_then(|i| i.as_object().cloned())
        .unwrap_or_default();
    shell.run(move |desk| desk.replay(&name, &steps, inputs))
}

#[tauri::command]
async fn start_recording(shell: tauri::State<'_, Shell>, resource_id: String) -> Reply {
    shell.run(move |desk| desk.start_capture(&resource_id))
}

#[tauri::command]
async fn collect_recording(shell: tauri::State<'_, Shell>) -> Reply {
    shell.run(|desk| desk.collect())
}

#[tauri::command]
async fn stop_recording(shell: tauri::State<'_, Shell>) -> Reply {
    shell.run(|desk| desk.stop_capture())
}

#[tauri::command]
async fn connect_browser(shell: tauri::State<'_, Shell>, port: Option<u16>) -> Reply {
    shell.run(move |desk| {
        let _ = desk.stop_capture();
        desk.session.close();
        desk.session = open(port);
        desk.browser_port = port;
        desk.targets()
    })
}

#[tauri::command]
async fn save_recording(name: String, steps: Value) -> Reply {
    save(&name, &parse_steps(steps)?)
}

#[tauri::command]
async fn open_recording(path: String) -> Reply {
    open_saved(&path)
}

#[tauri::command]
async fn list_recordings() -> Reply {
    let found = recordings::list_recordings().map_err(store_error)?;
    Ok(json!({
        "recordings": found,
        "directory": recordings::recordings_dir().to_string_lossy(),
    }))
}

#[tauri::command]
async fn probe_environment() -> Value {
    aad_probe::probe().to_json()
}

pub fn run() {
    tauri::Builder::default()
        .manage(Shell::new())
        .invoke_handler(tauri::generate_handler![
            list_targets,
            observe,
            try_locator,
            perform_step,
            run_steps,
            start_recording,
            collect_recording,
            stop_recording,
            connect_browser,
            save_recording,
            open_recording,
            list_recordings,
            probe_environment
        ])
        .run(tauri::generate_context!())
        .expect("the desktop shell failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;
    use aad_control::steps::Check;

    fn step(id: &str) -> Step {
        Step {
            id: id.into(),
            app: App {
                process: Some("notepad.exe".into()),
                ..Default::default()
            },
            locator: Locator {
                role: Some("Button".into()),
                name: Some("Save".into()),
                ..Default::default()
            },
            action: Some(json!({"type": "invoke"})),
            to: None,
            check: None,
            input: None,
            note: None,
        }
    }

    #[test]
    fn failures_carry_a_hint_a_person_can_act_on() {
        let error = AutomationError::new("STEP.APP_AMBIGUOUS", "two windows")
            .with_effect("not_applied")
            .with_detail("candidates", json!([{"name": "a"}]));
        let value = failure(&error);
        assert_eq!(value["code"], "STEP.APP_AMBIGUOUS");
        assert!(value["hint"].as_str().unwrap().contains("窗口标题"));
        assert_eq!(value["details"]["candidates"][0]["name"], "a");
        assert!(payload("SOMETHING.ELSE", "x", "unknown", None)
            .get("hint")
            .is_none());
    }

    #[test]
    fn invalid_steps_are_refused_before_anything_is_written() {
        let mut broken = step("bad");
        broken.action = None;
        let refused = save("never-written", &[broken]).unwrap_err();
        assert_eq!(refused["code"], "STEP.INVALID");
        let mut check = step("wait");
        check.action = None;
        check.check = Some(Check {
            present: true,
            within_ms: 1000,
        });
        assert!(steps::workflow("ok", &[step("one"), check]).is_ok());
    }

    #[test]
    fn steps_from_the_front_end_parse_or_fail_as_usage_errors() {
        let parsed = parse_steps(json!([step("one")])).unwrap();
        assert_eq!(parsed[0].id, "one");
        let refused = parse_steps(json!([{"id": "x"}])).unwrap_err();
        assert_eq!(refused["code"], "GUI.INVALID_REQUEST");
    }
}
