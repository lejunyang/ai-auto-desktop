//! Run ordinary JavaScript with synchronous, page-scoped browser operations.
use crate::tools::Tool;
use aad_control::{
    Action, Expect, Fraction, Locator, Operation, Request, Session, Target, View, ViewMode,
};
use aad_runtime::CancellationToken;
use rquickjs::{CatchResultExt, Context, Function, Runtime};
use serde::Deserialize;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const NAME: &str = "browser_script";
mod download_export;
pub const SOURCE_LIMIT: usize = 64 * 1024;
const RESULT_LIMIT: usize = 512 * 1024;
const MAX_CALLS: usize = 500;
const CDP: &str = aad_control::browser::PROVIDER;
const BIDI: &str = aad_control::browser_bidi::PROVIDER;
type Result<T> = std::result::Result<T, Value>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub source: String,
    pub resource_id: Option<String>,
    pub url: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub inputs: Value,
    pub timeout_ms: Option<u64>,
}

pub fn tool() -> Tool {
    Tool {
        name: NAME,
        description: "Run synchronous JavaScript starting on one observed resource selected by resource_id or exact url. Use page.observe, present, read, fields, table, collect, wait, click, fill, key, scroll and act. In Chromium, page.readObjects(locator, options?) reads a canvas graph and page.clickObject(locator, query, options?) targets an object by label, relation or visual description. In Chromium and Firefox, page.openPopup(locator, exactUrl?) clicks and binds to one new popup opened by the bound frame; page.returnToOpener() restores that frame. Popup switching is unavailable with explicit resource grants. page.chooseFiles(locator, files) selects files through an upload button; page.upload sets a file input directly. In a session-launched Chromium, page.downloadExport({task_id,trigger,expected}) verifies a task download and returns its preserved file path and digest. Pass JSON in inputs and return JSON. Writes need the server's input grant; file selection also needs an allowed upload directory.",
        schema: json!({"type":"object","additionalProperties":false,"required":["source"],
            "oneOf":[{"required":["resource_id"]},{"required":["url"]}],
            "properties":{"source":{"type":"string","maxLength":SOURCE_LIMIT},
                "resource_id":{"type":"string"},"url":{"type":"string"},
                "provider":{"enum":[CDP,BIDI]},"inputs":{},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":120000}}}),
        mutating: true,
    }
}

fn failure(code: &str, message: impl ToString) -> Value {
    json!({"code":code, "message":message.to_string()})
}

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum PageCall {
    DownloadExport {
        spec: Box<download_export::Spec>,
    },
    Observe {
        mode: ViewMode,
    },
    Fields {
        fields: Vec<aad_control::fields::ReadField>,
    },
    ReadText {
        locator: Locator,
        language: Option<String>,
    },
    ReadObjects {
        locator: Locator,
        options: aad_control::canvas_objects::Options,
    },
    Act {
        locator: Locator,
        action: Action,
    },
    OpenPopup {
        locator: Locator,
        url: Option<String>,
    },
    ReturnToOpener {},
    Wait {
        locator: Locator,
        condition: aad_control::WaitCondition,
        states: Option<Value>,
    },
    Table {
        locator: Locator,
        columns: Vec<String>,
    },
    Collect {
        spec: Box<aad_control::table::Collection>,
    },
}

struct Page {
    session: Arc<Session>,
    resource_id: String,
    chromium: bool,
    openers: Vec<String>,
    stop: CancellationToken,
    deadline: Instant,
    calls: usize,
    writes: usize,
    error: Option<Value>,
}

impl Page {
    fn pages(&self) -> Result<Vec<Value>> {
        let provider = if self.chromium { CDP } else { BIDI };
        let listing = self.request(Operation::Targets {
            provider: Some(provider.into()),
        })?;
        if let Some(error) = listing["unavailable"].get(provider) {
            return Err(error.clone());
        }
        listing["resources"]
            .as_array()
            .cloned()
            .ok_or_else(|| failure("BROWSER.SCRIPT_ERROR", "invalid resource discovery result"))
    }

    fn open_popup(&mut self, locator: Locator, url: Option<String>) -> Result<Value> {
        if self.session.policy().resources.is_some() {
            return Err(failure(
                "POLICY.DENIED",
                "a new popup is not covered by an explicit resource grant",
            ));
        }
        if url
            .as_ref()
            .is_some_and(|url| url.is_empty() || url.len() > 4096)
        {
            return Err(failure(
                "BROWSER.SCRIPT_INVALID",
                "popup URL must be a nonempty exact URL up to 4096 bytes",
            ));
        }
        let before = self.pages()?;
        let origin = before
            .iter()
            .find(|item| item["resource_id"] == self.resource_id)
            .ok_or_else(|| {
                failure(
                    "BROWSER.PAGE_NOT_FOUND",
                    "the opener is no longer available",
                )
            })?;
        let meta = &origin["metadata"];
        if meta["page_list_complete"] != true
            || !meta["page_id"].is_string()
            || !meta["frame_id"].is_string()
        {
            return Err(failure(
                "BROWSER.SCRIPT_INVALID",
                "cannot establish a complete baseline for this opener",
            ));
        }
        let existing: BTreeSet<_> = before
            .iter()
            .filter_map(|item| item["metadata"]["page_id"].as_str())
            .collect();
        let target = self.target(locator)?;
        self.request(Operation::Act {
            target,
            action: Action::Click {
                input_timing: None,
                fraction: Some(Fraction { fx: 0.5, fy: 0.5 }),
                button: None,
                count: None,
                hold_ms: None,
                modifiers: Vec::new(),
            },
            verify: None,
        })?;
        self.writes += 1;
        let mut candidate = None;
        loop {
            let available = self.pages()?;
            let matches = available.iter().filter(|item| {
                let child = &item["metadata"];
                child["main_frame"] == true
                    && child["access_error"].is_null()
                    && child["opener_page_id"] == meta["page_id"]
                    && child["opener_frame_id"] == meta["frame_id"]
                    && child["page_id"]
                        .as_str()
                        .is_some_and(|id| !existing.contains(id))
                    && url.as_ref().is_none_or(|url| child["url"] == *url)
            });
            let mut matched = None;
            for child in matches {
                let id = child["resource_id"].as_str().ok_or_else(|| {
                    failure("BROWSER.SCRIPT_ERROR", "popup has no resource identity")
                })?;
                if !self.chromium
                    && !self
                        .session
                        .live_popup_opener(
                            id,
                            &self.resource_id,
                            url.as_deref(),
                            self.deadline,
                            &self.stop,
                        )
                        .map_err(|e| e.to_json())?
                {
                    continue;
                }
                if matched.is_some() || child["metadata"]["page_list_complete"] != true {
                    return Err(failure(
                        "BROWSER.PAGE_AMBIGUOUS",
                        "cannot identify exactly one new matching popup",
                    ));
                }
                matched = Some(id);
            }
            if let Some(id) = matched {
                if candidate.as_deref() == Some(id) {
                    self.openers
                        .push(std::mem::replace(&mut self.resource_id, id.to_owned()));
                    return Ok(Value::Null);
                }
                candidate = Some(id.to_owned());
            } else {
                candidate = None;
            }
            std::thread::sleep(
                Duration::from_millis(75)
                    .min(self.deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn return_to_opener(&mut self) -> Result<Value> {
        let origin = self.openers.last().ok_or_else(|| {
            failure(
                "BROWSER.SCRIPT_INVALID",
                "no opener was saved by this invocation",
            )
        })?;
        let available = self.pages()?;
        if !available.iter().any(|item| {
            item["resource_id"] == *origin && item["metadata"]["access_error"].is_null()
        }) {
            return Err(failure(
                "BROWSER.PAGE_NOT_FOUND",
                "the original resource is no longer available",
            ));
        }
        self.resource_id = self.openers.pop().unwrap();
        Ok(Value::Null)
    }
    fn request(&self, operation: Operation) -> Result<Value> {
        let ms = self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        if ms == 0 || self.stop.is_cancelled() {
            return Err(stopped(&self.stop));
        }
        let mut request = Request::new(Some(self.session.id().to_owned()), operation);
        request.timeout_ms = ms.min(aad_control::MAX_TIMEOUT_MS.into()) as u64;
        let reply = self
            .session
            .execute_cancellable(&request, self.stop.clone());
        if let Some(error) = reply.error {
            return Err(error);
        }
        reply
            .data
            .ok_or_else(|| failure("BROWSER.SCRIPT_ERROR", "missing session reply"))
    }

    fn observe(&self, mode: ViewMode) -> Result<Value> {
        self.request(Operation::Observe {
            resource_id: self.resource_id.clone(),
            max_nodes: aad_control::MAX_NODES,
            view: View {
                mode,
                ..View::default()
            },
        })
    }

    fn target(&self, locator: Locator) -> Result<Target> {
        if locator.is_empty() {
            return Err(failure(
                "BROWSER.SCRIPT_INVALID",
                "an element locator is required",
            ));
        }
        let seen = self.observe(ViewMode::Nodes)?;
        let found = self.request(Operation::Find {
            observation_id: seen["observation_id"].as_str().unwrap_or_default().into(),
            locator: Box::new(locator),
            expect: Expect::One,
        })?;
        serde_json::from_value(found["target"].clone())
            .map_err(|err| failure("BROWSER.SCRIPT_ERROR", err))
    }

    fn read_text(&self, locator: Locator, language: Option<String>) -> Result<Value> {
        self.request(Operation::ReadText {
            target: self.target(locator)?,
            language,
        })
    }

    fn call(&mut self, raw: &str) -> Result<Value> {
        if raw.len() > SOURCE_LIMIT || self.calls >= MAX_CALLS {
            return Err(failure(
                "BROWSER.SCRIPT_LIMIT",
                "page call count or argument size exceeded",
            ));
        }
        self.calls += 1;
        let call =
            serde_json::from_str(raw).map_err(|err| failure("BROWSER.SCRIPT_INVALID", err))?;
        match call {
            PageCall::OpenPopup { locator, url } => self.open_popup(locator, url),
            PageCall::ReturnToOpener {} => self.return_to_opener(),
            PageCall::Observe { mode } => self.observe(mode),
            PageCall::ReadText { locator, language } => self.read_text(locator, language),
            PageCall::ReadObjects { locator, options } => {
                self.request(Operation::ReadCanvasObjects {
                    target: self.target(locator)?,
                    options,
                })
            }
            PageCall::DownloadExport { spec } => self.download_export(*spec),
            PageCall::Fields { fields } => self.request(Operation::ReadFields {
                resource_id: self.resource_id.clone(),
                fields,
            }),
            PageCall::Wait {
                locator,
                condition,
                states,
            } => self.request(Operation::Wait {
                resource_id: self.resource_id.clone(),
                locator: Box::new(locator),
                condition,
                states,
                max_nodes: aad_control::MAX_NODES,
                interval_ms: None,
            }),
            PageCall::Table { locator, columns } => self.request(Operation::ReadTable {
                target: self.target(locator)?,
                columns,
                max_characters: None,
            }),
            PageCall::Collect { spec } => {
                let data = self.request(Operation::CollectTable {
                    resource_id: self.resource_id.clone(),
                    spec,
                })?;
                self.writes += data["actions"].as_u64().unwrap_or_default() as usize;
                Ok(data)
            }
            PageCall::Act { locator, action } => {
                // These actions take all destinations from this one observation.
                // Upload paths additionally go through the session's directory grant.
                if !matches!(
                    action,
                    Action::Click { .. }
                        | Action::ClickObject { .. }
                        | Action::Focus {}
                        | Action::Invoke {}
                        | Action::Fill { .. }
                        | Action::Upload { .. }
                        | Action::ChooseFiles { .. }
                        | Action::TypeText { .. }
                        | Action::Key { .. }
                        | Action::Scroll { .. }
                        | Action::Hover { .. }
                        | Action::Select { .. }
                        | Action::Check { .. }
                        | Action::Navigate { .. }
                        | Action::Back {}
                        | Action::Forward {}
                        | Action::Reload {}
                        | Action::Accept { .. }
                        | Action::Dismiss {}
                ) {
                    return Err(failure("BROWSER.SCRIPT_INVALID", "unsupported page action"));
                }
                let result = self.request(Operation::Act {
                    target: self.target(locator)?,
                    action,
                    verify: None,
                })?;
                self.writes += 1;
                Ok(result)
            }
        }
    }

    fn dispatch(&mut self, raw: String) -> String {
        if let Some(error) = &self.error {
            return json!({"error":error}).to_string();
        }
        match self.call(&raw) {
            Ok(value) => json!({"data":value}).to_string(),
            Err(error) => {
                self.error = Some(error.clone());
                json!({"error":error}).to_string()
            }
        }
    }
}

fn stopped(token: &CancellationToken) -> Value {
    if token.is_cancelled() {
        failure("CONTROL.CANCELLED", "browser script cancelled")
    } else {
        failure("CONTROL.TIMEOUT", "browser script deadline elapsed")
    }
}

pub fn run(session: Arc<Session>, script: Script, stop: CancellationToken) -> Result<Value> {
    run_with_limit(session, script, stop, 120_000)
}

pub fn run_download_export(
    session: Arc<Session>,
    resource_id: String,
    spec: Value,
    timeout_ms: u64,
    stop: CancellationToken,
) -> Result<Value> {
    run_with_limit(
        session,
        Script {
            source: "return page.downloadExport(inputs);".into(),
            inputs: spec,
            resource_id: Some(resource_id),
            url: None,
            provider: Some(CDP.into()),
            timeout_ms: Some(timeout_ms),
        },
        stop,
        1_800_000,
    )
}

pub enum PageRead {
    View(ViewMode),
    Fields(Vec<aad_control::fields::ReadField>),
    CanvasGrid(Box<aad_control::canvas_grid::Spec>),
    CanvasObjects {
        locator: Box<Locator>,
        options: aad_control::canvas_objects::Options,
    },
    CanvasText {
        locator: Box<Locator>,
        language: Option<String>,
    },
}

/// One read through the same resource selection and observation as a browser script.
pub fn read_page(
    session: Arc<Session>,
    provider: &str,
    resource_id: Option<&str>,
    url: Option<&str>,
    read: PageRead,
) -> Result<Value> {
    let page = bind_page(
        session,
        provider,
        resource_id,
        url,
        &CancellationToken::new(),
        Instant::now() + Duration::from_secs(30),
    )?;
    match read {
        PageRead::CanvasText { locator, language } => page.read_text(*locator, language),
        PageRead::CanvasObjects { locator, options } => {
            page.request(Operation::ReadCanvasObjects {
                target: page.target(*locator)?,
                options,
            })
        }
        PageRead::CanvasGrid(spec) => page.request(Operation::ReadCanvasGrid {
            resource_id: page.resource_id.clone(),
            spec,
        }),
        PageRead::View(mode) => page.observe(mode),
        PageRead::Fields(fields) => page.request(Operation::ReadFields {
            resource_id: page.resource_id.clone(),
            fields,
        }),
    }
}

fn bind_page(
    session: Arc<Session>,
    provider: &str,
    resource_id: Option<&str>,
    url: Option<&str>,
    stop: &CancellationToken,
    deadline: Instant,
) -> Result<Page> {
    if resource_id.is_some() == url.is_some() || !matches!(provider, CDP | BIDI) {
        return Err(failure(
            "BROWSER.PAGE_INVALID",
            "select one granted resource by id or exact URL",
        ));
    }
    let mut page = Page {
        session,
        resource_id: String::new(),
        chromium: provider == CDP,
        openers: Vec::new(),
        stop: stop.clone(),
        deadline,
        calls: 0,
        writes: 0,
        error: None,
    };
    let targets = page.request(Operation::Targets {
        provider: Some(provider.to_owned()),
    })?;
    let mut matches = targets["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| {
            r["provider"] == provider
                && match resource_id {
                    Some(id) => r["resource_id"].as_str() == Some(id),
                    None => r["metadata"]["url"].as_str() == url,
                }
        });
    let Some(id) = matches.next().and_then(|r| r["resource_id"].as_str()) else {
        return Err(failure(
            "BROWSER.PAGE_NOT_FOUND",
            "no granted browser page matches the selector",
        ));
    };
    if matches.next().is_some() {
        return Err(failure(
            "BROWSER.PAGE_AMBIGUOUS",
            "multiple pages match; select a resource_id from control_targets",
        ));
    }
    page.resource_id = id.to_owned();
    Ok(page)
}

fn run_with_limit(
    session: Arc<Session>,
    script: Script,
    stop: CancellationToken,
    max_timeout_ms: u64,
) -> Result<Value> {
    let timeout = script.timeout_ms.unwrap_or(30_000);
    let provider = script.provider.as_deref().unwrap_or(CDP);
    if script.source.is_empty()
        || script.source.len() > SOURCE_LIMIT
        || script.inputs.to_string().len() > SOURCE_LIMIT
        || !(1..=max_timeout_ms).contains(&timeout)
    {
        return Err(failure("BROWSER.SCRIPT_INVALID", format!("provide one page (resource_id or exact url), a script up to 65536 bytes, and timeout_ms in 1..{max_timeout_ms}")));
    }
    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout);
    let page = bind_page(
        session,
        provider,
        script.resource_id.as_deref(),
        script.url.as_deref(),
        &stop,
        deadline,
    )?;
    let page = Rc::new(RefCell::new(page));
    let vm_failure = |err| failure("BROWSER.SCRIPT_ERROR", err);
    let vm = Runtime::new().map_err(vm_failure)?;
    vm.set_memory_limit(24 * 1024 * 1024);
    vm.set_max_stack_size(256 * 1024);
    let interrupt = stop.clone();
    vm.set_interrupt_handler(Some(Box::new(move || {
        interrupt.is_cancelled() || Instant::now() >= deadline
    })));
    let ctx = Context::full(&vm).map_err(vm_failure)?;
    let result = ctx.with(|ctx| -> rquickjs::Result<String> {
        let bound = page.clone();
        let bridge = Function::new(ctx.clone(), move |raw: String| {
            bound.borrow_mut().dispatch(raw)
        })?;
        let execute: Function = ctx.eval(include_str!("browser_script.js"))?;
        execute
            .call((script.source, script.inputs.to_string(), bridge))
            .catch(&ctx)
            .map_err(|err| {
                rquickjs::Error::new_from_js_message(
                    "script",
                    "JSON",
                    err.to_string().chars().take(2048).collect::<String>(),
                )
            })
    });
    let mut page = page.borrow_mut();
    let result = if let Some(error) = page.error.take() {
        Err(error)
    } else if stop.is_cancelled() || Instant::now() >= deadline {
        Err(stopped(&stop))
    } else {
        result.map_err(vm_failure).and_then(|raw| {
            if raw.len() > RESULT_LIMIT {
                return Err(failure(
                    "BROWSER.SCRIPT_LIMIT",
                    "returned JSON exceeds 512 KiB",
                ));
            }
            serde_json::from_str::<Value>(&raw)
                .map_err(|err| failure("BROWSER.SCRIPT_INVALID", err))
        })
    };
    let mut report = json!({"page_calls":page.calls,"completed_actions":page.writes,
        "elapsed_ms": start.elapsed().as_millis() as u64});
    match result {
        Ok(value) => {
            report["result"] = value;
            Ok(report)
        }
        Err(error) => {
            report["error"] = error;
            Err(report)
        }
    }
}
