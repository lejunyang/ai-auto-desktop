//! Explicit CDP page/frame resources. No arbitrary evaluation in the public API.
use crate::{
    error, native::resource_id, Action, Backend, BackendResource, Button, Direction, Resource,
    Result, PAGE_ACTIONS, TAB_ACTIONS,
};
use aad_cdp::{Client, Target};
use aad_core::AutomationError;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const PROVIDER: &str = "browser.cdp";

/// CDP `Input` and page navigation address the page, not a frame, so they are
/// offered only where the observed frame is the main one.
const MAIN_FRAME_ONLY: [&str; 10] = [
    "key",
    "scroll",
    "hover",
    "navigate",
    "back",
    "forward",
    "reload",
    "open_tab",
    "activate_page",
    "close_page",
];

/// How long a newly attached page may take to answer before it counts as blocked.
const RESPONSIVE: Duration = Duration::from_secs(2);
const DIALOG_OPENING: &str = "Page.javascriptDialogOpening";
const DOWNLOAD_BEGIN: &str = "Browser.downloadWillBegin";
const DOWNLOAD_PROGRESS: &str = "Browser.downloadProgress";
/// Downloads remembered per session; the oldest are forgotten first.
const MAX_DOWNLOADS: usize = 200;

/// Downloads go to a directory of this session's own, and are tracked from
/// the browser's events.
#[derive(Default)]
struct Downloads {
    directory: Option<std::path::PathBuf>,
    /// Why downloads could not be routed to the directory, if they could not.
    failure: Option<String>,
    items: Vec<Value>,
}

impl Downloads {
    /// Route this browser's downloads into the session directory. A browser
    /// that refuses still works; the answer then says why nothing is tracked.
    fn configure(&mut self, client: &mut Client) {
        let directory = self.directory.get_or_insert_with(|| {
            std::env::temp_dir().join(format!("aad-downloads-{}", uuid::Uuid::new_v4()))
        });
        self.failure = std::fs::create_dir_all(&*directory)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                client
                    .command(
                        "Browser.setDownloadBehavior",
                        json!({"behavior": "allow", "downloadPath": directory, "eventsEnabled": true}),
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
            .err();
    }

    fn track(&mut self, client: &Client) {
        for event in client.drain_events().events {
            self.apply(&event);
        }
    }

    fn apply(&mut self, event: &Value) {
        let params = &event["params"];
        let Some(guid) = params["guid"].as_str() else {
            return;
        };
        match event["method"].as_str() {
            Some(DOWNLOAD_BEGIN) => {
                if self.items.len() >= MAX_DOWNLOADS {
                    self.items.remove(0);
                }
                self.items.push(json!({
                    "guid": guid,
                    "url": params["url"],
                    "suggested_filename": params["suggestedFilename"],
                    "state": "in_progress",
                    "received_bytes": 0,
                    "total_bytes": null,
                    "path": null,
                }));
            }
            Some(DOWNLOAD_PROGRESS) => {
                let directory = self.directory.clone();
                let Some(item) = self.items.iter_mut().find(|item| item["guid"] == guid) else {
                    return;
                };
                item["received_bytes"] = params["receivedBytes"].clone();
                if params["totalBytes"]
                    .as_f64()
                    .is_some_and(|total| total > 0.0)
                {
                    item["total_bytes"] = params["totalBytes"].clone();
                }
                item["state"] = json!(match params["state"].as_str() {
                    Some("completed") => "completed",
                    Some("canceled") => "canceled",
                    _ => "in_progress",
                });
                if params["state"] == "completed" {
                    // Newer browsers say where; otherwise the suggested name
                    // in the directory, when that file is there.
                    let path = params["filePath"]
                        .as_str()
                        .map(std::path::PathBuf::from)
                        .or_else(|| {
                            Some(directory?.join(item["suggested_filename"].as_str()?))
                                .filter(|path| path.is_file())
                        });
                    item["path"] = json!(path);
                }
            }
            _ => {}
        }
    }

    fn answer(&self) -> Value {
        json!({
            "directory": self.directory,
            "tracking": self.failure.is_none() && self.directory.is_some(),
            "failure": self.failure,
            "downloads": self.items,
        })
    }
}
const DIALOG_CLOSED: &str = "Page.javascriptDialogClosed";

struct Page {
    client: Client,
    websocket: String,
    /// The JavaScript dialog blocking this page, as the browser announced it.
    dialog: Option<Value>,
    /// The main frame id the last frame tree reported.
    main_frame: Option<String>,
}

impl Page {
    /// Apply the dialog events received since the last look.
    fn sync_dialog(&mut self) {
        for event in self.client.drain_events().events {
            match event["method"].as_str() {
                Some(DIALOG_OPENING) => {
                    let params = &event["params"];
                    let text = |key: &str| {
                        params[key]
                            .as_str()
                            .map(|s| s.chars().take(4096).collect::<String>())
                    };
                    self.dialog = Some(json!({
                        "dialog_id": uuid::Uuid::new_v4().to_string(),
                        "type": text("type").unwrap_or_else(|| "alert".into()),
                        "message": text("message").unwrap_or_default(),
                        "default_prompt": text("defaultPrompt"),
                        "url": text("url"),
                    }));
                }
                Some(DIALOG_CLOSED) => self.dialog = None,
                _ => {}
            }
        }
    }
}

fn dialog_open(dialog: &Value) -> AutomationError {
    error(
        "CONTROL.DIALOG_OPEN",
        format!(
            "a JavaScript {} dialog blocks this page",
            dialog["type"].as_str().unwrap_or("alert")
        ),
    )
    .with_detail(
        "dialog",
        json!({"type": dialog["type"], "message": dialog["message"]}),
    )
}

/// What an observation of a page blocked by a dialog shows: the dialog alone.
fn dialog_snapshot(page_id: &str, frame_id: &str, main: bool, dialog: &Value) -> Value {
    let prompt = dialog["type"] == "prompt";
    json!({
        "snapshot_id": uuid::Uuid::new_v4().to_string(),
        "document_token": null,
        "truncated": false,
        "nodes": [{
            "node_id": "dialog",
            "role": "alertdialog",
            "name": dialog["message"],
            "tag": null,
            "value": if prompt { dialog["default_prompt"].clone() } else { Value::Null },
            "states": {"enabled": true, "modal": true, "focused": true},
            "actions": ["accept", "dismiss"],
            "dialog": {
                "type": dialog["type"],
                "message": dialog["message"],
                "default_prompt": dialog["default_prompt"],
                "url": dialog["url"],
            },
        }],
        "metadata": {
            "page_id": page_id,
            "frame_id": frame_id,
            "main_frame": main,
            "dialog_id": dialog["dialog_id"],
            "dialog": dialog["type"],
        },
    })
}
pub struct BrowserBackend {
    /// The loopback CDP port; 0 until a browser is attached.
    port: AtomicU16,
    pages: Mutex<BTreeMap<String, Page>>,
    /// The browser-level connection, for tabs and downloads; opened when first needed.
    browser: Mutex<Option<Client>>,
    downloads: Mutex<Downloads>,
    world: String,
    last_context: Mutex<Option<(String, i64)>>,
    closed: AtomicBool,
}

impl BrowserBackend {
    pub fn new(port: u16) -> Self {
        Self {
            port: AtomicU16::new(port),
            pages: Mutex::new(BTreeMap::new()),
            browser: Mutex::new(None),
            downloads: Mutex::new(Downloads::default()),
            world: format!("aad-control-{}", uuid::Uuid::new_v4()),
            last_context: Mutex::new(None),
            closed: AtomicBool::new(false),
        }
    }

    /// Work with the browser on `port` from now on, e.g. one this session started.
    pub fn attach(&self, port: u16) {
        self.pages.lock().unwrap().clear();
        *self.browser.lock().unwrap() = None;
        *self.last_context.lock().unwrap() = None;
        self.port.store(port, Ordering::SeqCst);
    }

    /// The attached CDP port, if any.
    pub fn attached_port(&self) -> Option<u16> {
        Some(self.port.load(Ordering::SeqCst)).filter(|port| *port != 0)
    }

    fn port(&self) -> Result<u16> {
        match self.port.load(Ordering::SeqCst) {
            0 => Err(error(
                "BROWSER.NOT_CONNECTED",
                "no browser is attached to this session",
            )),
            port => Ok(port),
        }
    }

    fn page<'a>(
        &self,
        pages: &'a mut BTreeMap<String, Page>,
        target: &Target,
        deadline: Instant,
    ) -> Result<&'a mut Page> {
        if pages
            .get(&target.id)
            .is_some_and(|page| page.websocket != target.webSocketDebuggerUrl)
        {
            pages.remove(&target.id);
        }
        if !pages.contains_key(&target.id) {
            if pages.len() >= 64 {
                // Never evict a page with an open dialog: reattaching to a
                // blocked page waits for the dialog to go.
                if let Some(id) = pages
                    .iter()
                    .find(|(_, page)| page.dialog.is_none())
                    .map(|(id, _)| id.clone())
                {
                    pages.remove(&id);
                }
            }
            let mut client = Client::attach_bare_until(target, deadline).map_err(cdp_error)?;
            client.filter_events(&[DIALOG_OPENING, DIALOG_CLOSED], &[DIALOG_OPENING]);
            // Dialog events need the Page domain. Enabling it waits for the
            // renderer, which a dialog opened before this connection blocks
            // for good: the browser only lets the connection that saw a
            // dialog open answer it. Fail fast rather than hang.
            let patience = deadline.min(Instant::now() + RESPONSIVE);
            client.set_deadline(patience);
            if let Err(e) = client.command("Page.enable", json!({})) {
                return Err(
                    if e.interrupted_by().is_none() && Instant::now() < deadline {
                        error(
                            "BROWSER.PAGE_UNRESPONSIVE",
                            "the page did not answer: a JavaScript dialog that opened before this \
                         session attached, or a busy script, blocks it",
                        )
                    } else {
                        cdp_error(e)
                    },
                );
            }
            client.set_deadline(deadline);
            command(&mut client, "Runtime.enable", json!({}))?;
            pages.insert(
                target.id.clone(),
                Page {
                    client,
                    websocket: target.webSocketDebuggerUrl.clone(),
                    dialog: None,
                    main_frame: None,
                },
            );
        }
        let page = pages.get_mut(&target.id).unwrap();
        page.client.set_deadline(deadline);
        page.sync_dialog();
        Ok(page)
    }

    /// Work on one page's connection with its dialog state up to date.
    fn with_page_state<T>(
        &self,
        page_id: &str,
        timeout: Duration,
        f: impl FnOnce(&mut Page) -> Result<T>,
    ) -> Result<T> {
        let deadline = Instant::now() + timeout;
        if self.closed.load(Ordering::SeqCst) {
            return Err(error("CONTROL.SESSION_CLOSED", "browser session is closed"));
        }
        // Re-discover, so a browser restart or closed tab cannot reuse an old connection.
        let targets = Client::list_targets_until(self.port()?, deadline).map_err(cdp_error)?;
        let target = targets
            .iter()
            .find(|target| target.id == page_id)
            .ok_or_else(|| error("CONTROL.STALE_TARGET", "browser tab no longer exists"))?;
        let mut pages = self.pages.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(error("CONTROL.SESSION_CLOSED", "browser session is closed"));
        }
        pages.retain(|id, _| targets.iter().any(|target| target.id == *id));
        let result = f(self.page(&mut pages, target, deadline)?);
        if self.closed.load(Ordering::SeqCst) {
            self.clear_context(&mut pages, Instant::now() + Duration::from_millis(100));
        }
        if let Some(page) = pages.get_mut(page_id) {
            page.sync_dialog();
            // A fresh connection on the next call recovers from a broken one,
            // but not while a dialog blocks the page: attaching would hang.
            if result.is_err() && page.dialog.is_none() {
                pages.remove(page_id);
            }
        }
        result
    }

    /// Work on a page that must not be blocked by a JavaScript dialog.
    fn with_page<T>(
        &self,
        page_id: &str,
        timeout: Duration,
        f: impl FnOnce(&mut Client) -> Result<T>,
    ) -> Result<T> {
        self.with_page_state(page_id, timeout, |page| match &page.dialog {
            Some(dialog) => Err(dialog_open(dialog)),
            None => f(&mut page.client),
        })
    }

    /// Run commands on the browser-level connection, opening it when needed.
    /// A failure drops it, so the next call reconnects to the current browser.
    fn with_browser<T>(
        &self,
        deadline: Instant,
        f: impl FnOnce(&mut Client) -> Result<T>,
    ) -> Result<T> {
        let port = self.port()?;
        let mut slot = self.browser.lock().unwrap();
        if slot.is_none() {
            let mut client = Client::browser_until(port, deadline).map_err(cdp_error)?;
            client.filter_events(&[DOWNLOAD_BEGIN, DOWNLOAD_PROGRESS], &[]);
            client.set_deadline(deadline);
            self.downloads.lock().unwrap().configure(&mut client);
            *slot = Some(client);
        }
        let client = slot.as_mut().unwrap();
        client.set_deadline(deadline);
        let result = f(client);
        self.downloads.lock().unwrap().track(client);
        if result.is_err() {
            *slot = None;
        }
        result
    }

    /// What the browser downloaded since this session first connected to it:
    /// each by guid, address, suggested name, state, bytes and saved path.
    pub fn downloads(&self, timeout: Duration) -> Result<Value> {
        self.with_browser(Instant::now() + timeout, |_| Ok(()))?;
        Ok(self.downloads.lock().unwrap().answer())
    }

    /// Which page opened each page, by target id, as the browser reports it.
    fn openers(&self, deadline: Instant) -> BTreeMap<String, String> {
        self.with_browser(deadline, |client| {
            command(client, "Target.getTargets", json!({}))
        })
        .map(|answer| {
            answer["targetInfos"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|info| {
                    Some((
                        info["targetId"].as_str()?.to_string(),
                        info["openerId"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// Open, front or close a tab after the observed document was confirmed.
    fn tab(&self, page_id: &str, action: &Action, deadline: Instant) -> Result<Value> {
        let unknown = |e: AutomationError| e.with_effect("unknown");
        self.with_browser(deadline, |client| match action {
            Action::OpenTab { url } => {
                let created = command(client, "Target.createTarget", json!({"url": url}))
                    .map_err(unknown)?;
                let id = created["targetId"].as_str().ok_or_else(|| {
                    error("BROWSER.BAD_RESULT", "the browser named no new page").with_effect("unknown")
                })?;
                Ok(json!({"submitted": true, "native_operation": "Target.createTarget", "page_id": id, "opener_page_id": page_id}))
            }
            Action::ActivatePage {} => {
                command(client, "Target.activateTarget", json!({"targetId": page_id}))
                    .map_err(unknown)?;
                Ok(json!({"submitted": true, "native_operation": "Target.activateTarget", "page_id": page_id}))
            }
            Action::ClosePage {} => {
                let closed = command(client, "Target.closeTarget", json!({"targetId": page_id}))
                    .map_err(unknown)?;
                // Chromium reports `success: false` when the page refused, e.g.
                // it no longer exists; older versions omit the field.
                if closed["success"] == false {
                    return Err(error("BROWSER.CLOSE_REFUSED", "the browser did not close the page")
                        .with_effect("unknown"));
                }
                Ok(json!({"submitted": true, "native_operation": "Target.closeTarget", "page_id": page_id}))
            }
            _ => unreachable!("only tab actions reach here"),
        })
    }

    fn clear_context(&self, pages: &mut BTreeMap<String, Page>, deadline: Instant) {
        if let Some((page, context)) = self.last_context.lock().unwrap().take() {
            // A page blocked by a dialog cannot evaluate; its world goes with the document.
            if let Some(page) = pages.get_mut(&page).filter(|page| page.dialog.is_none()) {
                page.client.set_deadline(deadline);
                let _ = eval(
                    &mut page.client,
                    context,
                    "delete globalThis.__aadControlV1".into(),
                );
            }
        }
    }
}

fn cdp_error(e: aad_cdp::CdpError) -> AutomationError {
    if e.interrupted_by() == Some(DIALOG_OPENING) {
        return error(
            "CONTROL.DIALOG_OPEN",
            "a JavaScript dialog opened and blocks the page",
        );
    }
    error("BROWSER.CDP_ERROR", e.to_string())
}
fn command(client: &mut Client, method: &str, params: Value) -> Result<Value> {
    client.command(method, params).map_err(cdp_error)
}
fn eval(client: &mut Client, context: i64, expression: String) -> Result<Value> {
    let raw = command(
        client,
        "Runtime.evaluate",
        json!({"expression":expression,"contextId":context,"returnByValue":true,"awaitPromise":false}),
    )?;
    if raw.get("exceptionDetails").is_some() {
        return Err(error(
            "CONTROL.STALE_TARGET",
            "browser execution context failed or was replaced",
        ));
    }
    raw.get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .ok_or_else(|| error("BROWSER.BAD_RESULT", "expression did not return a value"))
}
fn frames(tree: &Value) -> Vec<Value> {
    let mut result = Vec::new();
    let mut pending = vec![tree];
    while let Some(tree) = pending.pop() {
        result.push(tree["frame"].clone());
        if result.len() >= 4096 {
            break;
        }
        if let Some(children) = tree["childFrames"].as_array() {
            let remaining = 4096usize.saturating_sub(result.len() + pending.len());
            pending.extend(children.iter().rev().take(remaining));
        }
    }
    result
}
fn current_frame(client: &mut Client, frame_id: &str) -> Result<Value> {
    frames(&command(client, "Page.getFrameTree", json!({}))?["frameTree"])
        .into_iter()
        .find(|frame| frame["id"] == frame_id)
        .ok_or_else(|| error("CONTROL.STALE_TARGET", "browser frame was detached"))
}

impl Backend for BrowserBackend {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut pages) = self.pages.try_lock() {
            self.clear_context(&mut pages, Instant::now() + Duration::from_millis(100));
        }
    }
    fn capture(&self, snapshot: &Value, node: &Value, timeout: Duration) -> Result<Value> {
        self.capture_node(snapshot, node, timeout)
    }
    fn lease_key(&self, _snapshot: &Value) -> Option<String> {
        // UIA/AX/AT-SPI can also operate a browser. Share one cooperating
        // input owner rather than pretending these are disjoint devices.
        Some("desktop-input".into())
    }
    fn name(&self) -> &str {
        PROVIDER
    }
    fn capabilities(&self) -> Vec<String> {
        [
            "focus",
            "click",
            "fill",
            "key",
            "scroll",
            "hover",
            "select",
            "check",
            "navigate",
            "back",
            "forward",
            "reload",
            "open_tab",
            "activate_page",
            "close_page",
            "accept",
            "dismiss",
            "upload",
        ]
        .map(String::from)
        .to_vec()
    }
    fn resources(&self, timeout: Duration) -> Result<Vec<BackendResource>> {
        if self.port.load(Ordering::SeqCst) == 0 {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + timeout;
        let targets = Client::list_targets_until(self.port()?, deadline).map_err(cdp_error)?;
        // Best effort: without the browser connection pages are still listed,
        // just without who opened them.
        let openers = self.openers(deadline);
        let mut pages = self.pages.lock().unwrap();
        pages.retain(|id, _| targets.iter().any(|target| target.id == *id));
        let mut resources = Vec::new();
        for target in targets.iter().take(64) {
            let page = match self.page(&mut pages, target, deadline) {
                Ok(page) => page,
                Err(e) if e.code == "BROWSER.PAGE_UNRESPONSIVE" => {
                    // Listed, so a person can be told, but nothing is offered on it.
                    let selector = json!({"page_id": target.id, "frame_id": target.id});
                    resources.push(BackendResource {
                        resource: Resource {
                            resource_id: resource_id(PROVIDER, &selector),
                            provider: PROVIDER.into(),
                            kind: "browser".into(),
                            name: target.title.clone(),
                            capabilities: Vec::new(),
                            metadata: json!({"page_id": target.id, "frame_id": target.id,
                                "url": target.url, "main_frame": true, "responsive": false,
                                "opener_page_id": openers.get(&target.id)}),
                        },
                        selector,
                    });
                    continue;
                }
                Err(e) => return Err(e),
            };
            if let Some(dialog) = &page.dialog {
                // The renderer is blocked: list the page by its last known
                // main frame without asking it for its frames.
                let frame_id = page.main_frame.clone().unwrap_or_else(|| target.id.clone());
                let selector = json!({"page_id": target.id, "frame_id": frame_id});
                resources.push(BackendResource {
                    resource: Resource {
                        resource_id: resource_id(PROVIDER, &selector),
                        provider: PROVIDER.into(),
                        kind: "browser".into(),
                        name: target.title.clone(),
                        capabilities: vec!["accept".into(), "dismiss".into()],
                        metadata: json!({"page_id": target.id, "frame_id": frame_id, "url": target.url,
                            "main_frame": true, "opener_page_id": openers.get(&target.id),
                            "dialog": {"type": dialog["type"], "message": dialog["message"]}}),
                    },
                    selector,
                });
                continue;
            }
            let tree = command(&mut page.client, "Page.getFrameTree", json!({}))?;
            let root = tree["frameTree"]["frame"]["id"]
                .as_str()
                .ok_or_else(|| error("BROWSER.BAD_RESULT", "missing main frame"))?;
            page.main_frame = Some(root.to_string());
            for frame in frames(&tree["frameTree"]).into_iter().take(64) {
                let selector = json!({"page_id":target.id,"frame_id":frame["id"]});
                let main = frame["id"] == root;
                let mut capabilities = self.capabilities();
                // CDP Input targets the page's focused frame. Until cross-process
                // frame routing is implemented, input and navigation are main-frame only.
                if !main {
                    capabilities.retain(|s| !MAIN_FRAME_ONLY.contains(&s.as_str()));
                }
                resources.push(BackendResource {resource:Resource {resource_id:resource_id(PROVIDER,&selector),provider:PROVIDER.into(),kind:"browser".into(),
                    name:target.title.clone(),capabilities,metadata:json!({"page_id":target.id,"frame_id":frame["id"],"url":frame["url"],"main_frame":main,"opener_page_id":openers.get(&target.id),"click_semantics":"dom_activation; trusted cdp input with button, count or fraction","fill_semantics":"dom_value_and_events","key_semantics":"cdp_input_main_frame","pointer_semantics":"cdp_input_main_frame"})},selector});
            }
        }
        if self.closed.load(Ordering::SeqCst) {
            self.clear_context(&mut pages, Instant::now() + Duration::from_millis(100));
        }
        Ok(resources)
    }
    fn observe(&self, selector: &Value, max_nodes: usize, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        self.clear_context(
            &mut self.pages.lock().unwrap(),
            deadline.min(Instant::now() + Duration::from_millis(100)),
        );
        let page_id = selector["page_id"].as_str().unwrap();
        let frame_id = selector["frame_id"].as_str().unwrap();
        self.with_page_state(page_id,deadline.saturating_duration_since(Instant::now()),|page|{
            if let Some(dialog)=&page.dialog {
                let main=page.main_frame.as_deref().is_none_or(|main|main==frame_id);
                return Ok(dialog_snapshot(page_id,frame_id,main,dialog));
            }
            let client=&mut page.client;
            let frame=current_frame(client,frame_id)?;
            let context=command(client,"Page.createIsolatedWorld",json!({"frameId":frame_id,"worldName":self.world,"grantUniveralAccess":false}))?["executionContextId"].as_i64()
                .ok_or_else(||error("BROWSER.BAD_RESULT","frame did not provide an isolated context"))?;
            let nonce=serde_json::to_string(&uuid::Uuid::new_v4().to_string()).unwrap();
            eval(client,context,include_str!("browser.js").replace("NONCE",&nonce))?;
            let observation=uuid::Uuid::new_v4().to_string();
            *self.last_context.lock().unwrap()=Some((page_id.into(),context));
            let mut result=eval(client,context,format!("globalThis.__aadControlV1.observe({}, {})",serde_json::to_string(&observation).unwrap(),max_nodes))?;
            let main=frame.get("parentId").is_none();
            if !main {if let Some(nodes)=result["nodes"].as_array_mut(){for node in nodes {if let Some(actions)=node["actions"].as_array_mut(){actions.retain(|v|!MAIN_FRAME_ONLY.iter().any(|name|v==name));}}}}
            result["snapshot_id"]=json!(observation);
            result["metadata"]=json!({"page_id":page_id,"frame_id":frame_id,"loader_id":frame["loaderId"],"context_id":context,"main_frame":main});
            Ok(result)
        })
    }
    fn act(
        &self,
        snapshot: &Value,
        node: &Value,
        action: &Action,
        action_context: &crate::Context,
    ) -> Result<Value> {
        let meta = &snapshot["metadata"];
        if !node["actions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == action.name()))
        {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "node does not support this browser operation",
            ));
        }
        // A plain click is DOM activation and works in any frame; a button, a
        // count or a position needs trusted input, which addresses the page.
        let trusted = match action {
            Action::Click { .. } => !action.plain_click(),
            Action::Scroll { .. } | Action::Hover { .. } => true,
            _ => false,
        };
        if trusted && meta["main_frame"] != true {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "trusted pointer input is supported only for the main frame",
            ));
        }
        if matches!(action, Action::Accept { .. } | Action::Dismiss {}) {
            return self.answer_dialog(snapshot, action, action_context);
        }
        let page_id = meta["page_id"].as_str().unwrap();
        let frame_id = meta["frame_id"].as_str().unwrap();
        let result = self.with_page(page_id,action_context.remaining()?,|client|{
            if current_frame(client,frame_id)?["loaderId"]!=meta["loader_id"] {return Err(error("CONTROL.STALE_TARGET","browser document navigated"));}
            let context=meta["context_id"].as_i64().unwrap();
            let mut args=json!({"document_token":snapshot["document_token"],"observation":snapshot["snapshot_id"],"node_id":node["node_id"],"action":action.name()});
            match action {
                Action::Fill {value}=>args["value"]=json!(value),
                Action::Select {option}=>args["option"]=json!(option),
                Action::Check {checked}=>args["checked"]=json!(checked),
                Action::Click {fraction:Some(f),..}|Action::Scroll {fraction:Some(f),..}|Action::Hover {fraction:Some(f),..}=>args["fraction"]=json!({"fx":f.fx,"fy":f.fy}),
                _=>{}
            }
            if trusted {
                return pointer(client,context,&args,action,action_context);
            }
            if let Action::Upload {files}=action {
                return upload(client,context,&args,node,files,action_context);
            }
            if TAB_ACTIONS.contains(&action.name()) {
                // Anchored like navigation: the observed document must still be there.
                checked(eval(client,context,format!("globalThis.__aadControlV1.confirm({args})"))?)?;
                action_context.commit()?;
                return self.tab(page_id,action,Instant::now()+action_context.remaining()?);
            }
            if PAGE_ACTIONS.contains(&action.name()) {
                return page(client,context,&args,action,action_context);
            }
            if let Action::Key {keys}=action {
                if meta["main_frame"]!=true {return Err(error("CONTROL.ACTION_UNSUPPORTED","keyboard input is supported only for the main frame"));}
                let (key,code,virtual_key,modifiers)=key_chord(keys)?;
                action_context.commit()?;
                let focus=eval(client,context,format!("globalThis.__aadControlV1.act({args})")).map_err(|e|e.with_effect("unknown"))?;
                checked(focus)?;
                let verify=eval(client,context,format!("globalThis.__aadControlV1.verify({args})")).map_err(|e|e.with_effect("contextual"))?;
                if verify["ok"]!=true || current_frame(client,frame_id).map_err(|e|e.with_effect("contextual"))?["loaderId"]!=meta["loader_id"] {return Err(error("CONTROL.STALE_TARGET","keyboard focus or document changed").with_effect("contextual"));}
                // Once Input is submitted, transport/context failure is ambiguous.
                let text=if key=="Enter" && modifiers==0 {"\r"} else {""};
                action_context.commit()?;
                let down=command(client,"Input.dispatchKeyEvent",json!({"type":"keyDown","key":key,"code":code,"windowsVirtualKeyCode":virtual_key,"modifiers":modifiers,"text":text}));
                down.map_err(|e|e.with_effect("unknown"))?;
                if current_frame(client,frame_id).map_err(|e|e.with_effect("unknown"))?["loaderId"]!=meta["loader_id"] {return Err(error("CONTROL.STALE_TARGET","document changed after keydown").with_effect("unknown"));}
                command(client,"Input.dispatchKeyEvent",json!({"type":"keyUp","key":key,"code":code,"windowsVirtualKeyCode":virtual_key,"modifiers":modifiers})).map_err(|e|e.with_effect("unknown"))?;
                Ok(json!({"submitted":true,"native_operation":"Input.dispatchKeyEvent"}))
            } else {
                // Verification and DOM activation/fill happen in one JS turn in
                // the observed isolated world, preventing a navigation redirect.
                action_context.commit()?;
                let result=eval(client,context,format!("globalThis.__aadControlV1.act({args})")).map_err(|e|e.with_effect("unknown"))?;
                checked(result)
            }
        });
        match result {
            // The input was delivered and the page answered it with a dialog,
            // which now blocks it: the next observation shows the dialog.
            Err(e) if e.code == "CONTROL.DIALOG_OPEN" && e.effect != "not_applied" => Ok(
                json!({"submitted": true, "native_operation": action.name(), "dialog_opened": true}),
            ),
            other => other,
        }
    }
}

impl BrowserBackend {
    /// Accept or dismiss the dialog the observation showed, if it is still the one open.
    fn answer_dialog(
        &self,
        snapshot: &Value,
        action: &Action,
        action_context: &crate::Context,
    ) -> Result<Value> {
        let meta = &snapshot["metadata"];
        let expected = meta["dialog_id"].as_str().ok_or_else(|| {
            error(
                "CONTROL.ACTION_UNSUPPORTED",
                "only an observed dialog can be accepted or dismissed",
            )
        })?;
        let page_id = meta["page_id"].as_str().unwrap_or_default();
        self.with_page_state(page_id, action_context.remaining()?, |page| {
            let dialog = page
                .dialog
                .clone()
                .filter(|dialog| dialog["dialog_id"] == expected)
                .ok_or_else(|| {
                    error(
                        "CONTROL.STALE_TARGET",
                        "the dialog was already closed or replaced",
                    )
                })?;
            let (accept, text) = match action {
                Action::Accept { text } => (true, text.clone()),
                _ => (false, None),
            };
            if text.is_some() && dialog["type"] != "prompt" {
                return Err(error(
                    "CONTROL.INVALID_REQUEST",
                    "only a prompt dialog takes text",
                ));
            }
            let mut params = json!({"accept": accept});
            if let Some(text) = &text {
                params["promptText"] = json!(text);
            }
            action_context.commit()?;
            command(&mut page.client, "Page.handleJavaScriptDialog", params)
                .map_err(|e| e.with_effect("unknown"))?;
            page.dialog = None;
            Ok(
                json!({"submitted": true, "native_operation": "Page.handleJavaScriptDialog",
                "accepted": accept, "dialog_type": dialog["type"]}),
            )
        })
    }
}

/// The most pixels one capture returns; a long page body is cut at this height.
const MAX_CAPTURE: (f64, f64) = (4096.0, 8192.0);

impl BrowserBackend {
    /// A PNG of the observed element, scrolled into view first.
    pub(crate) fn capture_node(
        &self,
        snapshot: &Value,
        node: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let meta = &snapshot["metadata"];
        if meta["main_frame"] != true {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "screenshots are taken of the main frame's elements only",
            ));
        }
        let page_id = meta["page_id"].as_str().unwrap_or_default();
        let frame_id = meta["frame_id"].as_str().unwrap_or_default();
        self.with_page(page_id, timeout, |client| {
            if current_frame(client, frame_id)?["loaderId"] != meta["loader_id"] {
                return Err(error("CONTROL.STALE_TARGET", "browser document navigated"));
            }
            let context = meta["context_id"].as_i64().unwrap_or_default();
            let args = json!({"document_token": snapshot["document_token"], "observation": snapshot["snapshot_id"], "node_id": node["node_id"]});
            let rect = checked(eval(
                client,
                context,
                format!("globalThis.__aadControlV1.rect({args})"),
            )?)?;
            let number = |key: &str| rect[key].as_f64().unwrap_or_default();
            let (width, height) = (
                number("width").min(MAX_CAPTURE.0),
                number("height").min(MAX_CAPTURE.1),
            );
            let shot = command(
                client,
                "Page.captureScreenshot",
                json!({"format": "png", "captureBeyondViewport": true,
                       "clip": {"x": number("x"), "y": number("y"), "width": width, "height": height, "scale": 1}}),
            )?;
            let data = shot["data"]
                .as_str()
                .ok_or_else(|| error("BROWSER.BAD_RESULT", "no screenshot data"))?;
            Ok(json!({
                "format": "png",
                "data": data,
                "width": width.round(),
                "height": height.round(),
                "clipped": width < number("width") || height < number("height"),
            }))
        })
    }
}

fn mouse(client: &mut Client, params: Value) -> Result<Value> {
    command(client, "Input.dispatchMouseEvent", params).map_err(|e| e.with_effect("unknown"))
}

/// Trusted pointer input at a point the page confirmed lands on the node.
fn pointer(
    client: &mut Client,
    context: i64,
    args: &Value,
    action: &Action,
    action_context: &crate::Context,
) -> Result<Value> {
    let point = checked(eval(
        client,
        context,
        format!("globalThis.__aadControlV1.point({args})"),
    )?)?;
    let (Some(x), Some(y)) = (point["x"].as_f64(), point["y"].as_f64()) else {
        return Err(error("BROWSER.BAD_RESULT", "page did not report a point"));
    };
    action_context.commit()?;
    mouse(client, json!({"type": "mouseMoved", "x": x, "y": y}))?;
    match action {
        Action::Click { button, count, .. } => {
            let (name, mask) = match button {
                None | Some(Button::Left) => ("left", 1),
                Some(Button::Right) => ("right", 2),
                Some(Button::Middle) => ("middle", 4),
            };
            for click in 1..=count.unwrap_or(1) {
                mouse(
                    client,
                    json!({"type": "mousePressed", "x": x, "y": y, "button": name, "buttons": mask, "clickCount": click}),
                )?;
                mouse(
                    client,
                    json!({"type": "mouseReleased", "x": x, "y": y, "button": name, "buttons": 0, "clickCount": click}),
                )?;
            }
        }
        Action::Scroll {
            direction, notches, ..
        } => {
            let distance = 100.0 * f64::from(notches.unwrap_or(3));
            let (dx, dy) = match direction {
                Direction::Up => (0.0, -distance),
                Direction::Down => (0.0, distance),
                Direction::Left => (-distance, 0.0),
                Direction::Right => (distance, 0.0),
            };
            mouse(
                client,
                json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}),
            )?;
        }
        Action::Hover { ms, .. } => {
            let dwell = Duration::from_millis(u64::from(ms.unwrap_or(500)));
            std::thread::sleep(dwell.min(action_context.remaining()?));
        }
        _ => unreachable!("only pointer actions are trusted"),
    }
    Ok(
        json!({"submitted": true, "native_operation": "Input.dispatchMouseEvent", "trusted_event": true}),
    )
}

/// Files into the observed file input. The session has already resolved the
/// paths under the server's upload grant; the page never names a path.
fn upload(
    client: &mut Client,
    context: i64,
    args: &Value,
    node: &Value,
    files: &[String],
    action_context: &crate::Context,
) -> Result<Value> {
    if files.len() > 1 && node["multiple"] != true {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "this file input takes one file",
        ));
    }
    checked(eval(
        client,
        context,
        format!("globalThis.__aadControlV1.confirm({args})"),
    )?)?;
    let element = command(
        client,
        "Runtime.evaluate",
        json!({"expression": format!("globalThis.__aadControlV1.element({args})"),
               "contextId": context, "returnByValue": false}),
    )?;
    let object = element["result"]["objectId"]
        .as_str()
        .filter(|_| element["result"]["subtype"] == "node")
        .ok_or_else(|| error("CONTROL.STALE_TARGET", "the file input was replaced"))?
        .to_string();
    action_context.commit()?;
    let result = command(
        client,
        "DOM.setFileInputFiles",
        json!({"files": files, "objectId": object}),
    )
    .map_err(|e| e.with_effect("unknown"));
    let _ = command(client, "Runtime.releaseObject", json!({"objectId": object}));
    result?;
    Ok(
        json!({"submitted": true, "native_operation": "DOM.setFileInputFiles", "files": files.len()}),
    )
}

/// Navigation of the observed page, after confirming the document is the one observed.
fn page(
    client: &mut Client,
    context: i64,
    args: &Value,
    action: &Action,
    action_context: &crate::Context,
) -> Result<Value> {
    checked(eval(
        client,
        context,
        format!("globalThis.__aadControlV1.confirm({args})"),
    )?)?;
    let step = match action {
        Action::Back {} => Some(-1),
        Action::Forward {} => Some(1),
        _ => None,
    };
    let entry = match step {
        Some(step) => {
            let history = command(client, "Page.getNavigationHistory", json!({}))?;
            let index = history["currentIndex"].as_i64().unwrap_or(0) + step;
            let entry = usize::try_from(index)
                .ok()
                .and_then(|index| history["entries"].as_array()?.get(index).cloned());
            Some(entry.ok_or_else(|| {
                error(
                    "CONTROL.ACTION_UNSUPPORTED",
                    "there is no page in that direction of the history",
                )
            })?)
        }
        None => None,
    };
    action_context.commit()?;
    let unknown = |e: AutomationError| e.with_effect("unknown");
    let operation = match action {
        Action::Navigate { url } => {
            let result = command(client, "Page.navigate", json!({"url": url})).map_err(unknown)?;
            if let Some(text) = result["errorText"].as_str().filter(|t| !t.is_empty()) {
                return Err(error("BROWSER.NAVIGATION_FAILED", text).with_effect("contextual"));
            }
            "Page.navigate"
        }
        Action::Back {} | Action::Forward {} => {
            let entry = entry.expect("looked up above");
            command(
                client,
                "Page.navigateToHistoryEntry",
                json!({"entryId": entry["id"]}),
            )
            .map_err(unknown)?;
            "Page.navigateToHistoryEntry"
        }
        Action::Reload {} => {
            command(client, "Page.reload", json!({})).map_err(unknown)?;
            "Page.reload"
        }
        _ => unreachable!("only page actions reach here"),
    };
    Ok(json!({"submitted": true, "native_operation": operation}))
}

impl Drop for BrowserBackend {
    fn drop(&mut self) {
        self.close();
    }
}

fn checked(value: Value) -> Result<Value> {
    if let Some(code) = value["error"].as_str() {
        Err(error(
            code,
            "browser node was replaced, protected or no longer supports the action",
        )
        .with_effect(if value["effect"] == "contextual" {
            "contextual"
        } else {
            "not_applied"
        }))
    } else {
        Ok(value)
    }
}

fn key_chord(keys: &[String]) -> Result<(String, String, u32, u32)> {
    let mut modifiers = 0;
    let mut primary = None;
    for key in keys {
        match key.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= 2,
            "alt" => modifiers |= 1,
            "meta" | "command" => modifiers |= 4,
            "shift" => modifiers |= 8,
            _ => {
                if primary.replace(key.as_str()).is_some() {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "key requires one primary key",
                    ));
                }
            }
        }
    }
    let key =
        primary.ok_or_else(|| error("CONTROL.INVALID_REQUEST", "key requires a primary key"))?;
    // Keep browser/OS navigation shortcuts out of an element-scoped action.
    // Only editing combinations have a meaningful anchored target.
    let lower = key.to_ascii_lowercase();
    if modifiers & 1 != 0
        || (modifiers & (2 | 4) != 0
            && !matches!(
                lower.as_str(),
                "a" | "z"
                    | "y"
                    | "home"
                    | "end"
                    | "arrowleft"
                    | "left"
                    | "arrowright"
                    | "right"
                    | "arrowup"
                    | "up"
                    | "arrowdown"
                    | "down"
                    | "backspace"
                    | "delete"
            ))
    {
        return Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "browser or global shortcuts are not element-scoped editing actions",
        ));
    }
    let (name, code, vk) = match key.to_ascii_lowercase().as_str() {
        "enter" => ("Enter", "Enter", 13),
        "tab" => ("Tab", "Tab", 9),
        "escape" | "esc" => ("Escape", "Escape", 27),
        "backspace" => ("Backspace", "Backspace", 8),
        "delete" => ("Delete", "Delete", 46),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40),
        "home" => ("Home", "Home", 36),
        "end" => ("End", "End", 35),
        "pageup" => ("PageUp", "PageUp", 33),
        "pagedown" => ("PageDown", "PageDown", 34),
        _ => {
            if matches!(key.to_ascii_lowercase().as_str(), "a" | "z" | "y")
                && modifiers & (2 | 4) != 0
                && modifiers & 1 == 0
            {
                let upper = key.to_ascii_uppercase();
                return Ok((
                    key.to_ascii_lowercase(),
                    format!("Key{upper}"),
                    upper.as_bytes()[0] as u32,
                    modifiers,
                ));
            }
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "use fill for text; unsupported key chord",
            ));
        }
    };
    Ok((name.into(), code.into(), vk, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn downloads_follow_the_browser_events_and_stay_bounded() {
        let directory = std::env::temp_dir().join(format!("aad-dl-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("report.txt"), "body").unwrap();
        let mut downloads = Downloads {
            directory: Some(directory.clone()),
            ..Default::default()
        };
        downloads.apply(&json!({"method": DOWNLOAD_BEGIN, "params": {"guid": "g1",
            "url": "http://127.0.0.1/download", "suggestedFilename": "report.txt"}}));
        downloads.apply(
            &json!({"method": DOWNLOAD_PROGRESS, "params": {"guid": "g1",
            "receivedBytes": 2, "totalBytes": 4, "state": "inProgress"}}),
        );
        assert_eq!(downloads.items[0]["state"], "in_progress");
        assert!(downloads.items[0]["path"].is_null());
        downloads.apply(
            &json!({"method": DOWNLOAD_PROGRESS, "params": {"guid": "g1",
            "receivedBytes": 4, "totalBytes": 4, "state": "completed"}}),
        );
        let item = &downloads.answer()["downloads"][0];
        assert_eq!(item["state"], "completed");
        assert_eq!(item["received_bytes"], 4);
        assert_eq!(item["path"], json!(directory.join("report.txt")));
        // A progress event for an unknown download is ignored.
        downloads.apply(
            &json!({"method": DOWNLOAD_PROGRESS, "params": {"guid": "nope", "state": "completed"}}),
        );
        for n in 0..MAX_DOWNLOADS + 5 {
            downloads
                .apply(&json!({"method": DOWNLOAD_BEGIN, "params": {"guid": format!("x{n}")}}));
        }
        assert_eq!(downloads.items.len(), MAX_DOWNLOADS);
        assert_ne!(downloads.items[0]["guid"], "g1");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn a_blocked_page_shows_only_its_dialog_and_offers_only_answers() {
        let dialog = json!({"dialog_id": "d1", "type": "prompt", "message": "Your name?",
            "default_prompt": "guest", "url": "http://127.0.0.1/"});
        let snapshot = dialog_snapshot("p", "f", true, &dialog);
        let nodes = snapshot["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["role"], "alertdialog");
        assert_eq!(nodes[0]["value"], "guest");
        assert_eq!(nodes[0]["actions"], json!(["accept", "dismiss"]));
        assert_eq!(snapshot["metadata"]["dialog_id"], "d1");
        assert_eq!(snapshot["truncated"], false);
        let alert = dialog_snapshot(
            "p",
            "f",
            true,
            &json!({"dialog_id": "d2", "type": "alert", "message": "Hi", "default_prompt": ""}),
        );
        assert!(alert["nodes"][0]["value"].is_null());
        let refused = dialog_open(&dialog);
        assert_eq!(refused.code, "CONTROL.DIALOG_OPEN");
        assert_eq!(refused.effect, "not_applied");
        assert_eq!(refused.details["dialog"]["message"], "Your name?");
        assert_eq!(
            cdp_error(aad_cdp::CdpError::Interrupted(DIALOG_OPENING.into())).code,
            "CONTROL.DIALOG_OPEN"
        );
    }
    #[test]
    fn keys_are_allowlisted_without_arbitrary_cdp_parameters() {
        assert_eq!(
            key_chord(&["ctrl".into(), "a".into()]).unwrap(),
            ("a".into(), "KeyA".into(), 65, 2)
        );
        assert_eq!(key_chord(&["Enter".into()]).unwrap().2, 13);
        assert!(key_chord(&["a".into()]).is_err());
        assert!(key_chord(&["Enter".into(), "Tab".into()]).is_err());
        assert!(key_chord(&["ctrl".into(), "Tab".into()]).is_err());
        assert!(key_chord(&["alt".into(), "ArrowLeft".into()]).is_err());
    }
}
