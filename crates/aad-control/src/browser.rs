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
const MAIN_FRAME_ONLY: [&str; 7] = PAGE_ACTIONS;

/// How long a newly attached page may take to answer before it counts as blocked.
const RESPONSIVE: Duration = Duration::from_secs(2);
const DIALOG_OPENING: &str = "Page.javascriptDialogOpening";
const DOWNLOAD_BEGIN: &str = "Browser.downloadWillBegin";
const DOWNLOAD_PROGRESS: &str = "Browser.downloadProgress";
/// Downloads remembered per session; the oldest are forgotten first.
const MAX_DOWNLOADS: usize = 200;

/// Downloads go to a directory of this session's own, and are tracked from
/// the browser's events -- only for a browser this server launched. A
/// person's own browser, attached by port, keeps its download settings.
#[derive(Default)]
struct Downloads {
    /// Whether the attached browser is one this server launched.
    launched: bool,
    /// Whether the current browser connection redirected downloads.
    routed: bool,
    /// Downloads were handed back, as the session or the browser closes.
    released: bool,
    directory: Option<std::path::PathBuf>,
    /// Why downloads could not be routed to the directory, if they could not.
    failure: Option<String>,
    items: Vec<Value>,
}

impl Downloads {
    /// Route this browser's downloads into the session directory. A browser
    /// that refuses still works; the answer then says why nothing is tracked.
    fn configure(&mut self, client: &mut Client) {
        if !self.launched || self.released {
            return;
        }
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
        self.routed = self.failure.is_none();
    }

    /// Hand downloads back to the browser's own setting. Best effort: the
    /// browser may already be gone.
    fn reset(&mut self, client: &mut Client) {
        self.released = self.launched;
        if self.routed {
            client.set_deadline(Instant::now() + Duration::from_secs(1));
            let _ = client.command(
                "Browser.setDownloadBehavior",
                json!({"behavior": "default", "eventsEnabled": false}),
            );
            self.routed = false;
        }
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
        let reason = if !self.launched {
            Some("attached browser: downloads are not redirected".to_string())
        } else if self.released {
            Some("downloads were handed back to the browser's own setting".to_string())
        } else {
            self.failure.clone()
        };
        json!({
            "directory": self.directory,
            "tracking": self.launched && self.routed && !self.released,
            "reason": reason,
            "downloads": self.items,
        })
    }
}
const DIALOG_CLOSED: &str = "Page.javascriptDialogClosed";
const DRAG_INTERCEPTED: &str = "Input.dragIntercepted";
const ATTACHED: &str = "Target.attachedToTarget";
const DETACHED: &str = "Target.detachedFromTarget";
/// Out-of-process frames followed per page.
const MAX_CHILD_FRAMES: usize = 64;

struct Page {
    client: Client,
    websocket: String,
    /// The JavaScript dialog blocking this page, as the browser announced it.
    dialog: Option<Value>,
    /// The main frame id the last frame tree reported.
    main_frame: Option<String>,
    /// The drag data of an HTML5 drag the browser intercepted, while one is in flight.
    drag: Option<Value>,
    /// Out-of-process frames attached to this connection: CDP session id to frame id.
    children: BTreeMap<String, String>,
    /// Children not yet asked to attach their own out-of-process frames.
    fresh: Vec<String>,
}

static AUTO_ATTACH: std::sync::LazyLock<Value> = std::sync::LazyLock::new(
    || json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
);

impl Page {
    /// Let newly attached out-of-process frames attach their own. Best effort:
    /// a frame that went away meanwhile is dropped by its detach event.
    fn adopt(&mut self) {
        for session in std::mem::take(&mut self.fresh) {
            if self.children.contains_key(&session) {
                let _ = self.client.session_command(
                    Some(&session),
                    "Target.setAutoAttach",
                    AUTO_ATTACH.clone(),
                );
            }
        }
        self.sync_dialog();
    }

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
                Some(DRAG_INTERCEPTED) => self.drag = Some(event["params"]["data"].clone()),
                Some(ATTACHED) if event["params"]["targetInfo"]["type"] == "iframe" => {
                    let params = &event["params"];
                    if let (Some(session), Some(frame)) = (
                        params["sessionId"].as_str(),
                        params["targetInfo"]["targetId"].as_str(),
                    ) {
                        if self.children.len() < MAX_CHILD_FRAMES {
                            self.children.insert(session.into(), frame.into());
                            self.fresh.push(session.into());
                        }
                    }
                }
                Some(DETACHED) => {
                    if let Some(session) = event["params"]["sessionId"].as_str() {
                        self.children.remove(session);
                    }
                }
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
    last_context: Mutex<Option<(String, Ctx)>>,
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

    /// A provider for a browser this server launched on `port`: its
    /// downloads go to the session's directory and are listed.
    pub fn launched(port: u16) -> Self {
        let backend = Self::new(port);
        backend.downloads.lock().unwrap().launched = true;
        backend
    }

    /// Work with a browser someone else started on `port` from now on. Its
    /// download settings are left alone.
    pub fn attach(&self, port: u16) {
        self.switch(port, false);
    }

    /// Work with a browser this server launched on `port` from now on.
    pub fn attach_launched(&self, port: u16) {
        self.switch(port, true);
    }

    /// Give the current launched browser its own download setting back.
    pub fn release_downloads(&self) {
        if let Ok(mut slot) = self.browser.try_lock() {
            if let Some(client) = slot.as_mut() {
                self.downloads.lock().unwrap().reset(client);
            }
        }
    }

    fn switch(&self, port: u16, launched: bool) {
        self.release_downloads();
        {
            let mut downloads = self.downloads.lock().unwrap();
            downloads.launched = launched;
            downloads.released = false;
        }
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
            client.filter_events(
                &[
                    DIALOG_OPENING,
                    DIALOG_CLOSED,
                    DRAG_INTERCEPTED,
                    ATTACHED,
                    DETACHED,
                ],
                &[DIALOG_OPENING],
            );
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
            // Cross-origin frames run in their own renderer: follow them as
            // flattened child sessions on this connection.
            command(&mut client, "Target.setAutoAttach", AUTO_ATTACH.clone())?;
            pages.insert(
                target.id.clone(),
                Page {
                    client,
                    websocket: target.webSocketDebuggerUrl.clone(),
                    dialog: None,
                    main_frame: None,
                    drag: None,
                    children: BTreeMap::new(),
                    fresh: Vec::new(),
                },
            );
        }
        let page = pages.get_mut(&target.id).unwrap();
        page.client.set_deadline(deadline);
        page.sync_dialog();
        page.adopt();
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
            let client = Client::browser_until(port, deadline).map_err(cdp_error)?;
            client.filter_events(&[DOWNLOAD_BEGIN, DOWNLOAD_PROGRESS], &[]);
            *slot = Some(client);
        }
        let client = slot.as_mut().unwrap();
        client.set_deadline(deadline);
        // Chromium drops the download setting whenever any browser-level
        // DevTools client detaches, so a launched browser gets it every time.
        self.downloads.lock().unwrap().configure(client);
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
                    &context,
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
fn eval(client: &mut Client, context: &Ctx, expression: String) -> Result<Value> {
    let raw = run(
        client,
        context.session.as_deref(),
        "Runtime.evaluate",
        json!({"expression":expression,"contextId":context.id,"returnByValue":true,"awaitPromise":false}),
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
/// The frame as the session running it reports it now.
fn current_frame(client: &mut Client, session: Option<&str>, frame_id: &str) -> Result<Value> {
    frames(&run(client, session, "Page.getFrameTree", json!({}))?["frameTree"])
        .into_iter()
        .find(|frame| frame["id"] == frame_id)
        .ok_or_else(|| error("CONTROL.STALE_TARGET", "browser frame was detached"))
}

/// A command on the page's own session (`None`) or an out-of-process frame's.
fn run(client: &mut Client, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
    client
        .session_command(session, method, params)
        .map_err(cdp_error)
}

/// An isolated world: the CDP session whose renderer runs the frame, and the
/// world's execution context there.
#[derive(Clone, Debug)]
struct Ctx {
    session: Option<String>,
    id: i64,
}

/// Where a frame lives: the CDP session that runs it (`None`: the page's
/// own renderer), its frame record, its parent, and whether it is the root of
/// that session -- an out-of-process frame, or the main frame.
#[derive(Clone, Debug)]
struct FrameEntry {
    session: Option<String>,
    frame: Value,
    parent: Option<String>,
    root: bool,
}

impl FrameEntry {
    fn main(&self) -> bool {
        self.session.is_none() && self.parent.is_none()
    }
}

fn tree_entries(tree: &Value, session: Option<&str>, out: &mut Vec<(String, FrameEntry)>) {
    let mut pending = vec![(tree, None::<String>, true)];
    while let Some((tree, parent, root)) = pending.pop() {
        let Some(id) = tree["frame"]["id"].as_str().map(str::to_string) else {
            continue;
        };
        if out.len() >= 4096 {
            break;
        }
        let parent = parent.or_else(|| tree["frame"]["parentId"].as_str().map(str::to_string));
        for child in tree["childFrames"].as_array().into_iter().flatten().rev() {
            pending.push((child, Some(id.clone()), false));
        }
        out.push((
            id,
            FrameEntry {
                session: session.map(str::to_string),
                frame: tree["frame"].clone(),
                parent,
                root,
            },
        ));
    }
}

/// Every frame of a page, in tree order, each with the session that runs it.
/// A frame an out-of-process session runs is taken from that session, keeping
/// the parent the page's own tree knows it by.
fn frame_map(page: &mut Page) -> Result<Vec<(String, FrameEntry)>> {
    let mut all = Vec::new();
    let tree = run(&mut page.client, None, "Page.getFrameTree", json!({}))?;
    tree_entries(&tree["frameTree"], None, &mut all);
    let sessions: Vec<String> = page.children.keys().cloned().collect();
    for session in sessions {
        // A frame that is going away answers with an error; its detach event follows.
        let Ok(tree) = run(
            &mut page.client,
            Some(&session),
            "Page.getFrameTree",
            json!({}),
        ) else {
            continue;
        };
        let mut own = Vec::new();
        tree_entries(&tree["frameTree"], Some(&session), &mut own);
        merge_frames(&mut all, own);
    }
    Ok(all)
}

/// Fold an out-of-process session's frames into the page's: the session
/// runs them, so its records win, keeping the parent the page knew.
fn merge_frames(all: &mut Vec<(String, FrameEntry)>, own: Vec<(String, FrameEntry)>) {
    for (id, mut entry) in own {
        match all.iter_mut().find(|(known, _)| *known == id) {
            Some((_, existing)) => {
                if entry.parent.is_none() {
                    entry.parent = existing.parent.take();
                }
                *existing = entry;
            }
            None => all.push((id, entry)),
        }
    }
}

fn locate(page: &mut Page, frame_id: &str) -> Result<FrameEntry> {
    frame_map(page)?
        .into_iter()
        .find(|(id, _)| id == frame_id)
        .map(|(_, entry)| entry)
        .ok_or_else(|| error("CONTROL.STALE_TARGET", "browser frame was detached"))
}

/// Captures larger than this keep their heuristic roles and names: the
/// browser's accessibility tree is asked once per node.
const MAX_ACCESSIBLE_NODES: usize = 2_000;
/// Accessibility queries sent before their replies are awaited.
const AX_PIPELINE: usize = 200;

/// Roles and names as the browser's accessibility tree has them, for each
/// observed element. An element the tree ignores, an internal role
/// (`StaticText`, `LabelText`, ...) or an empty name keeps the heuristic
/// value; the page body stays `document`, where page actions live.
fn accessibility(client: &mut Client, context: &Ctx, nodes: &mut [Value]) -> Result<Value> {
    let started = Instant::now();
    if nodes.len() > MAX_ACCESSIBLE_NODES {
        return Ok(json!({"applied": false, "reason": "too many nodes", "nodes": nodes.len()}));
    }
    let session = context.session.as_deref();
    const GROUP: &str = "aad-control-accessibility";
    let list = run(
        client,
        session,
        "Runtime.evaluate",
        json!({"expression": "globalThis.__aadControlV1.elements()", "contextId": context.id,
            "returnByValue": false, "objectGroup": GROUP}),
    )?;
    let result = (|| -> Result<usize> {
        let array = list["result"]["objectId"]
            .as_str()
            .ok_or_else(|| error("BROWSER.BAD_RESULT", "no element list"))?;
        let properties = run(
            client,
            session,
            "Runtime.getProperties",
            json!({"objectId": array, "ownProperties": true}),
        )?;
        let elements: Vec<(usize, String)> = properties["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|property| {
                Some((
                    property["name"].as_str()?.parse::<usize>().ok()?,
                    property["value"]["objectId"].as_str()?.to_string(),
                ))
            })
            .filter(|(index, _)| *index < nodes.len())
            .collect();
        let mut applied = 0;
        // One element per call, many calls in flight: the round trips, not
        // the browser's work, dominate a sequential pass.
        for chunk in elements.chunks(AX_PIPELINE) {
            let calls: Vec<(&str, Value)> = chunk
                .iter()
                .map(|(_, object)| {
                    (
                        "Accessibility.getPartialAXTree",
                        json!({"objectId": object, "fetchRelatives": false}),
                    )
                })
                .collect();
            for ((index, _), tree) in chunk.iter().zip(client.session_pipeline(session, &calls)) {
                let tree = tree.map_err(cdp_error)?;
                let Some(ax) = tree["nodes"].as_array().and_then(|n| n.first()) else {
                    continue;
                };
                if apply_accessibility(&mut nodes[*index], ax) {
                    applied += 1;
                }
            }
        }
        Ok(applied)
    })();
    let _ = run(
        client,
        session,
        "Runtime.releaseObjectGroup",
        json!({"objectGroup": GROUP}),
    );
    Ok(
        json!({"applied": true, "nodes": nodes.len(), "from_tree": result?,
        "ms": started.elapsed().as_millis() as u64}),
    )
}

/// Take the accessibility tree's role and name where it has meaningful ones.
fn apply_accessibility(node: &mut Value, ax: &Value) -> bool {
    if ax["ignored"] == true || node["role"] == "document" {
        return false;
    }
    let mut changed = false;
    if let Some(role) = ax["role"]["value"].as_str().filter(|role| {
        role.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && *role != "none"
    }) {
        changed |= node["role"] != role;
        node["role"] = json!(role);
    }
    if let Some(name) = ax["name"]["value"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let name: String = name.chars().take(256).collect();
        changed |= node["name"] != name.as_str();
        node["name"] = json!(name);
    }
    changed
}

/// Name every landmark's region -- its name, else its role, numbered when
/// repeated -- and give each node the region it sits in (null outside any).
fn regions(nodes: &mut [Value]) {
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    let mut used: BTreeMap<String, usize> = BTreeMap::new();
    for node in nodes.iter_mut() {
        if node["landmark"] != true {
            continue;
        }
        let base = node["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .or_else(|| node["role"].as_str())
            .unwrap_or("region")
            .to_string();
        let count = used.entry(base.clone()).or_default();
        *count += 1;
        let label = if *count == 1 {
            base
        } else {
            format!("{base} ({count})")
        };
        node["landmark"] = json!(label);
        labels.insert(node["node_id"].as_str().unwrap_or_default().into(), label);
    }
    let parents: BTreeMap<String, Option<String>> = nodes
        .iter()
        .map(|n| {
            (
                n["node_id"].as_str().unwrap_or_default().to_string(),
                n["parent"].as_str().map(str::to_string),
            )
        })
        .collect();
    for node in nodes.iter_mut() {
        let mut at = node["parent"].as_str().map(str::to_string);
        let mut region = Value::Null;
        for _ in 0..parents.len() {
            let Some(id) = at else { break };
            if let Some(label) = labels.get(&id) {
                region = json!(label);
                break;
            }
            at = parents.get(&id).cloned().flatten();
        }
        node["region"] = region;
    }
}

/// Resolve a locator in a browser capture: role, name, node_id, value and
/// states, `match: contains` (case-insensitive, for name and value), `within`
/// a container another locator names (an overview's region name works as its
/// `name`), and `nth` counted top to bottom, then left to right.
fn browser_find(
    snapshot: &Value,
    locator: &crate::Locator,
    expect: crate::Expect,
) -> Result<Value> {
    let nodes = snapshot["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let candidates = matching(nodes, locator, 0)?;
    let order: Vec<&str> = snapshot["order"]
        .as_array()
        .map(|o| o.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut candidates = candidates;
    candidates.sort_by_key(|n| {
        let id = n["node_id"].as_str().unwrap_or_default();
        order.iter().position(|o| *o == id).unwrap_or(usize::MAX)
    });
    let total = candidates.len();
    if let Some(nth) = &locator.nth {
        let index = match nth {
            Value::String(word) if word == "first" => Some(0),
            Value::String(word) if word == "last" => total.checked_sub(1),
            Value::Number(n) => match n.as_u64() {
                Some(0) | None => {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "nth counts from 1, or is \"first\" or \"last\"",
                    ))
                }
                Some(n) => Some(n as usize - 1).filter(|i| *i < total),
            },
            _ => {
                return Err(error(
                    "CONTROL.INVALID_REQUEST",
                    "nth is a positive number, \"first\" or \"last\"",
                ))
            }
        };
        candidates = index.map(|i| vec![candidates[i]]).unwrap_or_default();
    }
    match candidates.as_slice() {
        [] if expect == crate::Expect::Optional => Ok(json!({"found": false})),
        [] => Err(error(
            "CONTROL.NOT_FOUND",
            "find must resolve exactly one observed node",
        )
        .with_retryable(true)),
        [only] => Ok(json!({"found": true, "node_id": only["node_id"], "match_count": total})),
        many => Err(error(
            "CONTROL.AMBIGUOUS",
            format!("the locator matched {} observed nodes", many.len()),
        )
        .with_detail("match_count", json!(many.len()))
        .with_detail(
            "candidates",
            json!(many
                .iter()
                .take(10)
                .map(
                    |n| json!({"node_id": n["node_id"], "role": n["role"], "name": n["name"],
                    "region": n["region"]})
                )
                .collect::<Vec<_>>()),
        )),
    }
}

fn matching<'a>(
    nodes: &'a [Value],
    locator: &crate::Locator,
    depth: usize,
) -> Result<Vec<&'a Value>> {
    let unsupported: Vec<&str> = [
        ("automation_id", locator.automation_id.is_some()),
        ("class_name", locator.class_name.is_some()),
        ("framework_id", locator.framework_id.is_some()),
        ("near", locator.near.is_some()),
    ]
    .iter()
    .filter(|(_, present)| *present)
    .map(|(name, _)| *name)
    .collect();
    if !unsupported.is_empty() {
        return Err(error(
            "CONTROL.LOCATOR_UNSUPPORTED",
            format!(
                "browser pages do not match {}; use role, name, value, states, match, within or nth",
                unsupported.join(", ")
            ),
        ));
    }
    if depth > 4 {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "within is nested too deeply",
        ));
    }
    let contains = match locator.match_mode.as_deref() {
        None | Some("exact") => false,
        Some("contains") => true,
        Some(_) => {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "match is exact or contains",
            ))
        }
    };
    let text = |expected: &str, actual: &Value| {
        actual.as_str().is_some_and(|actual| {
            if contains {
                actual.to_lowercase().contains(&expected.to_lowercase())
            } else {
                actual == expected
            }
        })
    };
    let states = match &locator.states {
        None => serde_json::Map::new(),
        Some(Value::Object(states)) => states.clone(),
        Some(_) => {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "states is an object of state names and values",
            ))
        }
    };
    let container = match &locator.within {
        None => None,
        Some(within) => {
            let within: crate::Locator = serde_json::from_value(within.clone()).map_err(|e| {
                error(
                    "CONTROL.INVALID_REQUEST",
                    format!("within is a locator: {e}"),
                )
            })?;
            let found = browser_find_in(nodes, &within, depth + 1)?;
            Some(found)
        }
    };
    let parents: BTreeMap<&str, &str> = nodes
        .iter()
        .filter_map(|n| Some((n["node_id"].as_str()?, n["parent"].as_str()?)))
        .collect();
    let inside = |node: &Value, container: &str| {
        let mut at = node["parent"].as_str();
        for _ in 0..nodes.len() {
            match at {
                Some(id) if id == container => return true,
                Some(id) => at = parents.get(id).copied(),
                None => return false,
            }
        }
        false
    };
    Ok(nodes
        .iter()
        .filter(|n| {
            locator.role.as_ref().is_none_or(|v| n["role"] == *v)
                && locator.name.as_ref().is_none_or(|v| text(v, &n["name"]))
                && locator.node_id.as_ref().is_none_or(|v| n["node_id"] == *v)
                && locator.value.as_ref().is_none_or(|v| text(v, &n["value"]))
                && states.iter().all(|(key, value)| n["states"][key] == *value)
                && container.as_deref().is_none_or(|c| inside(n, c))
        })
        .collect())
}

/// The one container a `within` names.
fn browser_find_in(nodes: &[Value], locator: &crate::Locator, depth: usize) -> Result<String> {
    let found = matching(nodes, locator, depth)?;
    match found.as_slice() {
        [only] => Ok(only["node_id"].as_str().unwrap_or_default().to_string()),
        [] => Err(error(
            "CONTROL.NOT_FOUND",
            "the container `within` names is not in this observation",
        )
        .with_retryable(true)),
        many => Err(error(
            "CONTROL.AMBIGUOUS",
            format!("`within` matched {} containers; name one", many.len()),
        )
        .with_detail("match_count", json!(many.len()))),
    }
}

/// What a browser-level hit test must find at one step out of a frame.
#[derive(Clone, Debug)]
enum Expect {
    /// A node of this frame: no same-process frame or element covers it.
    Frame(String),
    /// The element that hosts the out-of-process frame below, or that frame:
    /// hit-testing in the parent renderer reports the remote frame's node.
    Owner(i64, String),
}

/// One step from a frame towards the main frame: the frame owner element in
/// the session that holds it, and what hit-testing there must find.
#[derive(Clone, Debug)]
struct Level {
    session: Option<String>,
    owner: i64,
    expect: Expect,
}

/// How a point in a frame's viewport reaches the page. Trusted input goes to
/// the page's own session in its main-frame coordinates; the browser routes
/// it into same-process and out-of-process frames alike.
#[derive(Clone, Debug, Default)]
struct Route {
    levels: Vec<Level>,
}

impl Route {
    /// The owner elements between `frame_id` and the main frame, scrolled
    /// into view from the outermost in.
    fn to(page: &mut Page, frame_id: &str) -> Result<Self> {
        let map = frame_map(page)?;
        let entry = |id: &str| {
            map.iter()
                .find(|(known, _)| known == id)
                .map(|(_, entry)| entry.clone())
                .ok_or_else(|| error("CONTROL.STALE_TARGET", "browser frame was detached"))
        };
        let owner = |client: &mut Client, session: Option<&str>, frame: &str| -> Result<i64> {
            run(
                client,
                session,
                "DOM.getFrameOwner",
                json!({"frameId": frame}),
            )?["backendNodeId"]
                .as_i64()
                .ok_or_else(|| error("BROWSER.BAD_RESULT", "frame owner has no node"))
        };
        let mut levels = Vec::new();
        let mut current = entry(frame_id)?;
        if !current.root {
            let session = current.session.clone();
            levels.push(Level {
                owner: owner(&mut page.client, session.as_deref(), frame_id)?,
                session,
                expect: Expect::Frame(frame_id.to_string()),
            });
        }
        while let Some(session) = current.session.clone() {
            if levels.len() >= 16 {
                return Err(error(
                    "CONTROL.ACTION_UNSUPPORTED",
                    "the frame is nested too deeply for trusted input",
                ));
            }
            let root = page
                .children
                .get(&session)
                .cloned()
                .ok_or_else(|| error("CONTROL.STALE_TARGET", "browser frame was detached"))?;
            let parent = entry(&root)?
                .parent
                .ok_or_else(|| error("BROWSER.BAD_RESULT", "out-of-process frame has no parent"))?;
            current = entry(&parent)?;
            let id = owner(&mut page.client, current.session.as_deref(), &root)?;
            levels.push(Level {
                session: current.session.clone(),
                owner: id,
                expect: Expect::Owner(id, root.clone()),
            });
        }
        for level in levels.iter().rev() {
            run(
                &mut page.client,
                level.session.as_deref(),
                "DOM.scrollIntoViewIfNeeded",
                json!({"backendNodeId": level.owner}),
            )?;
        }
        Ok(Self { levels })
    }

    /// The frame point in main-frame coordinates, after hit-testing every step
    /// out: an element or frame laid over the way in makes it `CONTROL.OCCLUDED`.
    fn page_point(&self, client: &mut Client, point: (f64, f64)) -> Result<(f64, f64)> {
        let (mut x, mut y) = point;
        for level in &self.levels {
            let model = run(
                client,
                level.session.as_deref(),
                "DOM.getBoxModel",
                json!({"backendNodeId": level.owner}),
            )?;
            let content = &model["model"]["content"];
            let (Some(left), Some(top)) = (content[0].as_f64(), content[1].as_f64()) else {
                return Err(error("CONTROL.OCCLUDED", "the frame is not laid out"));
            };
            x += left;
            y += top;
            // Box models are in viewport coordinates; hit-testing takes document
            // coordinates, so the scroll offset of that renderer's root is added.
            let metrics = run(
                client,
                level.session.as_deref(),
                "Page.getLayoutMetrics",
                json!({}),
            )?;
            let scrolled = &metrics["cssLayoutViewport"];
            let (sx, sy) = (
                scrolled["pageX"].as_f64().unwrap_or_default(),
                scrolled["pageY"].as_f64().unwrap_or_default(),
            );
            let hit = run(
                client,
                level.session.as_deref(),
                "DOM.getNodeForLocation",
                json!({"x": (x + sx).round() as i64, "y": (y + sy).round() as i64,
                    "includeUserAgentShadowDOM": false, "ignorePointerEventsNone": false}),
            )
            .map_err(|_| error("CONTROL.OCCLUDED", "nothing of the frame is at that point"))?;
            let found = match &level.expect {
                Expect::Frame(frame) => hit["frameId"] == frame.as_str(),
                Expect::Owner(owner, frame) => {
                    hit["backendNodeId"].as_i64() == Some(*owner)
                        || hit["frameId"] == frame.as_str()
                }
            };
            if !found {
                return Err(error(
                    "CONTROL.OCCLUDED",
                    "something in an enclosing frame covers the element",
                ));
            }
        }
        Ok((x, y))
    }
}

impl Backend for BrowserBackend {
    fn close(&self) {
        self.release_downloads();
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut pages) = self.pages.try_lock() {
            self.clear_context(&mut pages, Instant::now() + Duration::from_millis(100));
        }
    }
    fn capture(&self, snapshot: &Value, node: &Value, timeout: Duration) -> Result<Value> {
        self.capture_node(snapshot, node, timeout)
    }
    fn view(
        &self,
        snapshot: &Value,
        view: &crate::View,
        timeout: Duration,
    ) -> Result<Option<Value>> {
        if view.mode != crate::ViewMode::Text {
            // Outline and overview come from the capture's regions.
            return Ok(None);
        }
        if snapshot["metadata"]["dialog_id"].is_string() {
            return Err(error(
                "CONTROL.DIALOG_OPEN",
                "a JavaScript dialog blocks this page; its message is in the observation",
            ));
        }
        self.text(snapshot, view, timeout).map(Some)
    }
    fn find(
        &self,
        snapshot: &Value,
        locator: &crate::Locator,
        expect: crate::Expect,
        _timeout: Duration,
    ) -> Result<Option<Value>> {
        browser_find(snapshot, locator, expect).map(Some)
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
            "type_text",
            "key",
            "scroll",
            "hover",
            "drag",
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
            let map = frame_map(page)?;
            let root = map
                .iter()
                .find(|(_, entry)| entry.main())
                .map(|(id, _)| id.clone())
                .ok_or_else(|| error("BROWSER.BAD_RESULT", "missing main frame"))?;
            page.main_frame = Some(root.clone());
            for (id, entry) in map.into_iter().take(64) {
                let selector = json!({"page_id": target.id, "frame_id": id});
                let main = id == root;
                let mut capabilities = self.capabilities();
                // Navigation and tabs address the page, not a frame.
                if !main {
                    capabilities.retain(|s| !MAIN_FRAME_ONLY.contains(&s.as_str()));
                }
                resources.push(BackendResource {
                    resource: Resource {
                        resource_id: resource_id(PROVIDER, &selector),
                        provider: PROVIDER.into(),
                        kind: "browser".into(),
                        name: target.title.clone(),
                        capabilities,
                        metadata: json!({"page_id": target.id, "frame_id": id,
                            "url": entry.frame["url"], "main_frame": main,
                            "parent_frame_id": entry.parent,
                            "out_of_process": entry.session.is_some(),
                            "opener_page_id": openers.get(&target.id),
                            "click_semantics": "dom_activation; trusted cdp input with button, count or fraction",
                            "fill_semantics": "dom_value_and_events",
                            "key_semantics": "cdp_input_to_the_focused_element",
                            "type_text_semantics": "cdp_insert_text_or_us_key_events",
                            "pointer_semantics": "cdp_input_in_page_coordinates_hit_tested_through_frames"}),
                    },
                    selector,
                });
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
            let entry=locate(page,frame_id)?;
            let frame=entry.frame.clone();
            let session=entry.session.clone();
            let client=&mut page.client;
            let context=run(client,session.as_deref(),"Page.createIsolatedWorld",json!({"frameId":frame_id,"worldName":self.world,"grantUniveralAccess":false}))?["executionContextId"].as_i64()
                .ok_or_else(||error("BROWSER.BAD_RESULT","frame did not provide an isolated context"))?;
            let context=Ctx{session:session.clone(),id:context};
            let nonce=serde_json::to_string(&uuid::Uuid::new_v4().to_string()).unwrap();
            eval(client,&context,include_str!("browser.js").replace("NONCE",&nonce))?;
            let observation=uuid::Uuid::new_v4().to_string();
            *self.last_context.lock().unwrap()=Some((page_id.into(),context.clone()));
            let mut result=eval(client,&context,format!("globalThis.__aadControlV1.observe({}, {})",serde_json::to_string(&observation).unwrap(),max_nodes))?;
            let main=entry.main();
            if !main {if let Some(nodes)=result["nodes"].as_array_mut(){for node in nodes {if let Some(actions)=node["actions"].as_array_mut(){actions.retain(|v|!MAIN_FRAME_ONLY.iter().any(|name|v==name));}}}}
            result["snapshot_id"]=json!(observation);
            // Roles and names as the browser's accessibility tree has them; a
            // failure there leaves the heuristic ones rather than failing the look.
            let mut nodes=result["nodes"].as_array().cloned().unwrap_or_default();
            let accessible=accessibility(client,&context,&mut nodes).unwrap_or_else(|e|json!({"applied":false,"reason":e.message}));
            regions(&mut nodes);
            result["nodes"]=json!(nodes);
            result["metadata"]=json!({"page_id":page_id,"frame_id":frame_id,"loader_id":frame["loaderId"],"context_id":context.id,"main_frame":main,"parent_frame_id":entry.parent,"out_of_process":session.is_some(),"accessibility":accessible});
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
            Action::Scroll { .. } | Action::Hover { .. } | Action::Drag { .. } => true,
            _ => false,
        };
        if self.downloads.lock().unwrap().launched {
            // A click may start a download: make sure it lands in the session directory.
            let _ = self.with_browser(Instant::now() + action_context.remaining()?, |_| Ok(()));
        }
        if matches!(action, Action::Accept { .. } | Action::Dismiss {}) {
            return self.answer_dialog(snapshot, action, action_context);
        }
        let page_id = meta["page_id"].as_str().unwrap();
        let frame_id = meta["frame_id"].as_str().unwrap();
        let result = self.with_page_state(page_id,action_context.remaining()?,|state|{
            if let Some(dialog)=&state.dialog {return Err(dialog_open(dialog));}
            let entry=locate(state,frame_id)?;
            if entry.frame["loaderId"]!=meta["loader_id"] {return Err(error("CONTROL.STALE_TARGET","browser document navigated"));}
            let context=Ctx{session:entry.session.clone(),id:meta["context_id"].as_i64().unwrap_or_default()};
            // Trusted pointer input is addressed to the page; the owner
            // elements of enclosing frames are brought into view first.
            let route=if trusted {Route::to(state,frame_id)?} else {Route::default()};
            if let Action::Drag {to,fraction,to_fraction,button}=action {
                let observed=|node_id:&str,fraction:&Option<crate::Fraction>,action:Value|{let mut args=json!({"document_token":snapshot["document_token"],"observation":snapshot["snapshot_id"],"node_id":node_id,"action":action});if let Some(f)=fraction{args["fraction"]=json!({"fx":f.fx,"fy":f.fy});}args};
                let from=observed(node["node_id"].as_str().unwrap_or_default(),fraction,json!("drag"));
                let onto=observed(&to.node_id,to_fraction,Value::Null);
                return drag(state,&context,&route,&from,&onto,*button,action_context);
            }
            let client=&mut state.client;
            let mut args=json!({"document_token":snapshot["document_token"],"observation":snapshot["snapshot_id"],"node_id":node["node_id"],"action":action.name()});
            match action {
                Action::Fill {value}=>args["value"]=json!(value),
                Action::Select {option}=>args["option"]=json!(option),
                Action::Check {checked}=>args["checked"]=json!(checked),
                Action::Click {fraction:Some(f),..}|Action::Scroll {fraction:Some(f),..}|Action::Hover {fraction:Some(f),..}=>args["fraction"]=json!({"fx":f.fx,"fy":f.fy}),
                _=>{}
            }
            if trusted {
                return pointer(client,&context,&route,&args,action,action_context);
            }
            if let Action::Upload {files}=action {
                return upload(client,&context,&args,node,files,action_context);
            }
            if TAB_ACTIONS.contains(&action.name()) {
                // Anchored like navigation: the observed document must still be there.
                checked(eval(client,&context,format!("globalThis.__aadControlV1.confirm({args})"))?)?;
                action_context.commit()?;
                return self.tab(page_id,action,Instant::now()+action_context.remaining()?);
            }
            if PAGE_ACTIONS.contains(&action.name()) {
                return page(client,&context,&args,action,action_context);
            }
            if matches!(action,Action::Key {..}|Action::TypeText {..}) {
                keyboard(client,&context,&args,frame_id,&meta["loader_id"],action,action_context)
            } else {
                // Verification and DOM activation/fill happen in one JS turn in
                // the observed isolated world, preventing a navigation redirect.
                action_context.commit()?;
                let result=eval(client,&context,format!("globalThis.__aadControlV1.act({args})")).map_err(|e|e.with_effect("unknown"))?;
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

impl BrowserBackend {
    /// The observed page's or region's visible text, with its tables as rows.
    fn text(&self, snapshot: &Value, view: &crate::View, timeout: Duration) -> Result<Value> {
        let meta = &snapshot["metadata"];
        let page_id = meta["page_id"].as_str().unwrap_or_default();
        let frame_id = meta["frame_id"].as_str().unwrap_or_default();
        let nodes = snapshot["nodes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let region = match &view.region {
            None => Value::Null,
            Some(region) => nodes
                .iter()
                .find(|n| n["landmark"] == region.as_str())
                .map(|n| n["node_id"].clone())
                .ok_or_else(|| {
                    let names: Vec<&Value> = nodes
                        .iter()
                        .filter(|n| n["landmark"].is_string())
                        .map(|n| &n["landmark"])
                        .collect();
                    error(
                        "CONTROL.NOT_FOUND",
                        format!("no region named {region:?} on this page"),
                    )
                    .with_detail("regions", json!(names))
                })?,
        };
        let max = view
            .max_characters
            .unwrap_or(crate::DEFAULT_OUTLINE_CHARACTERS);
        self.with_page_state(page_id, timeout, |page| {
            if let Some(dialog) = &page.dialog {
                return Err(dialog_open(dialog));
            }
            let entry = locate(page, frame_id)?;
            if entry.frame["loaderId"] != meta["loader_id"] {
                return Err(error("CONTROL.STALE_TARGET", "browser document navigated"));
            }
            let context = Ctx {
                session: entry.session,
                id: meta["context_id"].as_i64().unwrap_or_default(),
            };
            let args = json!({"document_token": snapshot["document_token"],
                "observation": snapshot["snapshot_id"], "node_id": region, "max_characters": max});
            let mut answer = checked(eval(
                &mut page.client,
                &context,
                format!("globalThis.__aadControlV1.text({args})"),
            )?)?;
            if let Some(region) = &view.region {
                answer["region"] = json!(region);
            }
            Ok(answer)
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
            if current_frame(client, None, frame_id)?["loaderId"] != meta["loader_id"] {
                return Err(error("CONTROL.STALE_TARGET", "browser document navigated"));
            }
            let context = Ctx {
                session: None,
                id: meta["context_id"].as_i64().unwrap_or_default(),
            };
            let args = json!({"document_token": snapshot["document_token"], "observation": snapshot["snapshot_id"], "node_id": node["node_id"]});
            let rect = checked(eval(
                client,
                &context,
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
    context: &Ctx,
    route: &Route,
    args: &Value,
    action: &Action,
    action_context: &crate::Context,
) -> Result<Value> {
    let point = landing(client, context, args)?;
    let (x, y) = route.page_point(client, point)?;
    action_context.commit()?;
    arrive(client, context, route, args, (x, y), action_context)?;
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

/// Where the page confirmed pointer input lands on an observed node.
fn landing(client: &mut Client, context: &Ctx, args: &Value) -> Result<(f64, f64)> {
    let point = checked(eval(
        client,
        context,
        format!("globalThis.__aadControlV1.point({args})"),
    )?)?;
    match (point["x"].as_f64(), point["y"].as_f64()) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(error("BROWSER.BAD_RESULT", "page did not report a point")),
    }
}

/// How long the browser may take to route the pointer into a frame.
const ROUTING: Duration = Duration::from_millis(1500);

/// Move the pointer onto the point. Inside a frame, the browser routes input
/// by compositor hit-test data that lags behind scrolling, so the move is
/// repeated until the frame itself reports the element under the pointer
/// (`:hover`); only then may a button go down.
fn arrive(
    client: &mut Client,
    context: &Ctx,
    route: &Route,
    args: &Value,
    (x, y): (f64, f64),
    action_context: &crate::Context,
) -> Result<()> {
    mouse(client, json!({"type": "mouseMoved", "x": x, "y": y}))?;
    if route.levels.is_empty() {
        return Ok(());
    }
    let until = Instant::now() + ROUTING.min(action_context.remaining()?);
    loop {
        let hovered = eval(
            client,
            context,
            format!("globalThis.__aadControlV1.hovered({args})"),
        )
        .map_err(|e| e.with_effect("contextual"))?;
        if hovered["ok"] == true {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(error(
                "CONTROL.OCCLUDED",
                "the browser did not route the pointer into the frame's element",
            )
            .with_effect("contextual"));
        }
        std::thread::sleep(Duration::from_millis(25));
        // A move to the same point is not delivered again; nudge and return.
        mouse(client, json!({"type": "mouseMoved", "x": x + 0.5, "y": y}))?;
        mouse(client, json!({"type": "mouseMoved", "x": x, "y": y}))?;
    }
}

/// Mouse moves between press and release, so pointer handlers see a glide.
const DRAG_STEPS: u32 = 12;
/// How long an HTML5 drag may take to reach the browser after the last move.
const DRAG_SETTLE: Duration = Duration::from_millis(150);

/// Press on one observed node, glide, release on another. Both ends are hit
/// tested while both are in view. An HTML5 drag (draggable, dragstart/drop)
/// is intercepted by the browser and finished with drag events at the
/// destination, since headless Chrome has no OS drag loop to run it.
#[allow(clippy::too_many_arguments)]
fn drag(
    page: &mut Page,
    context: &Ctx,
    route: &Route,
    from: &Value,
    onto: &Value,
    button: Option<Button>,
    action_context: &crate::Context,
) -> Result<Value> {
    let client = &mut page.client;
    landing(client, context, from)?;
    landing(client, context, onto)?;
    // Bringing the destination into view may have moved the source: both
    // must be hit where they are now, without scrolling again.
    let still = |args: &Value| {
        let mut args = args.clone();
        args["noscroll"] = json!(true);
        args
    };
    let both = |e: AutomationError| {
        if e.code == "CONTROL.OCCLUDED" {
            error(
                "CONTROL.OCCLUDED",
                "the drag's source and destination cannot both be in view, or something covers one",
            )
        } else {
            e
        }
    };
    let start = landing(client, context, &still(from)).map_err(both)?;
    let start = route.page_point(client, start)?;
    let end = landing(client, context, &still(onto))?;
    let end = route.page_point(client, end)?;
    let (name, mask) = match button {
        None | Some(Button::Left) => ("left", 1),
        Some(Button::Right) => ("right", 2),
        Some(Button::Middle) => ("middle", 4),
    };
    page.sync_dialog();
    page.drag = None;
    action_context.commit()?;
    let client = &mut page.client;
    command(
        client,
        "Input.setInterceptDrags",
        json!({"enabled": name == "left"}),
    )
    .map_err(|e| e.with_effect("unknown"))?;
    let mut result = || -> Result<Value> {
        arrive(
            &mut page.client,
            context,
            route,
            from,
            start,
            action_context,
        )?;
        mouse(
            &mut page.client,
            json!({"type": "mousePressed", "x": start.0, "y": start.1, "button": name, "buttons": mask, "clickCount": 1}),
        )?;
        let mut intercepted = None;
        for step in 1..=DRAG_STEPS {
            let f = f64::from(step) / f64::from(DRAG_STEPS);
            let (x, y) = (
                start.0 + (end.0 - start.0) * f,
                start.1 + (end.1 - start.1) * f,
            );
            mouse(
                &mut page.client,
                json!({"type": "mouseMoved", "x": x, "y": y, "button": name, "buttons": mask}),
            )?;
            page.sync_dialog();
            if let Some(data) = page.drag.take() {
                intercepted = Some(data);
                break;
            }
        }
        let settle = Instant::now() + DRAG_SETTLE;
        while intercepted.is_none() && name == "left" && Instant::now() < settle {
            std::thread::sleep(Duration::from_millis(10));
            page.sync_dialog();
            intercepted = page.drag.take();
        }
        let client = &mut page.client;
        let operation = match &intercepted {
            Some(data) => {
                for kind in ["dragEnter", "dragOver", "drop"] {
                    command(
                        client,
                        "Input.dispatchDragEvent",
                        json!({"type": kind, "x": end.0, "y": end.1, "data": data}),
                    )
                    .map_err(|e| e.with_effect("unknown"))?;
                }
                "Input.dispatchDragEvent"
            }
            None => "Input.dispatchMouseEvent",
        };
        mouse(
            client,
            json!({"type": "mouseReleased", "x": end.0, "y": end.1, "button": name, "buttons": 0, "clickCount": 1}),
        )?;
        Ok(
            json!({"submitted": true, "native_operation": operation, "trusted_event": true,
                "html5_drag": intercepted.is_some(), "steps": DRAG_STEPS}),
        )
    };
    let result = result();
    let _ = command(
        &mut page.client,
        "Input.setInterceptDrags",
        json!({"enabled": false}),
    );
    result
}

/// Files into the observed file input. The session has already resolved the
/// paths under the server's upload grant; the page never names a path.
fn upload(
    client: &mut Client,
    context: &Ctx,
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
    let session = context.session.as_deref();
    let element = run(
        client,
        session,
        "Runtime.evaluate",
        json!({"expression": format!("globalThis.__aadControlV1.element({args})"),
               "contextId": context.id, "returnByValue": false}),
    )?;
    let object = element["result"]["objectId"]
        .as_str()
        .filter(|_| element["result"]["subtype"] == "node")
        .ok_or_else(|| error("CONTROL.STALE_TARGET", "the file input was replaced"))?
        .to_string();
    action_context.commit()?;
    let result = run(
        client,
        session,
        "DOM.setFileInputFiles",
        json!({"files": files, "objectId": object}),
    )
    .map_err(|e| e.with_effect("unknown"));
    let _ = run(
        client,
        session,
        "Runtime.releaseObject",
        json!({"objectId": object}),
    );
    result?;
    Ok(
        json!({"submitted": true, "native_operation": "DOM.setFileInputFiles", "files": files.len()}),
    )
}

/// Navigation of the observed page, after confirming the document is the one observed.
fn page(
    client: &mut Client,
    context: &Ctx,
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

const ALT: u32 = 1;
const CTRL: u32 = 2;
const META: u32 = 4;
const SHIFT: u32 = 8;

/// One key press as CDP dispatches it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyPress {
    key: String,
    code: String,
    virtual_key: u32,
    modifiers: u32,
    /// What the key types, if anything: a keypress and an input event follow.
    text: Option<String>,
}

/// The key a US layout types `character` with, and whether it needs Shift.
fn us_key(character: char) -> Option<(String, String, u32, bool)> {
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    const PUNCTUATION: [(char, char, &str, u32); 11] = [
        ('-', '_', "Minus", 189),
        ('=', '+', "Equal", 187),
        ('[', '{', "BracketLeft", 219),
        (']', '}', "BracketRight", 221),
        ('\\', '|', "Backslash", 220),
        (';', ':', "Semicolon", 186),
        ('\'', '"', "Quote", 222),
        (',', '<', "Comma", 188),
        ('.', '>', "Period", 190),
        ('/', '?', "Slash", 191),
        ('`', '~', "Backquote", 192),
    ];
    let named = |key: &str, code: &str, vk: u32, shift: bool| {
        Some((key.to_string(), code.to_string(), vk, shift))
    };
    match character {
        'a'..='z' => {
            let upper = character.to_ascii_uppercase();
            named(
                &character.to_string(),
                &format!("Key{upper}"),
                upper as u32,
                false,
            )
        }
        'A'..='Z' => named(
            &character.to_string(),
            &format!("Key{character}"),
            character as u32,
            true,
        ),
        '0'..='9' => named(
            &character.to_string(),
            &format!("Digit{character}"),
            character as u32,
            false,
        ),
        ' ' => named(" ", "Space", 32, false),
        '\n' | '\r' => named("Enter", "Enter", 13, false),
        '\t' => named("Tab", "Tab", 9, false),
        _ => {
            if let Some(digit) = SHIFTED_DIGITS.find(character) {
                return named(
                    &character.to_string(),
                    &format!("Digit{digit}"),
                    '0' as u32 + digit as u32,
                    true,
                );
            }
            PUNCTUATION
                .iter()
                .find(|(plain, shifted, _, _)| *plain == character || *shifted == character)
                .and_then(|(plain, _, code, vk)| {
                    named(&character.to_string(), code, *vk, *plain != character)
                })
        }
    }
}

/// The press that types `character` on a US layout, with its text.
fn typed_key(character: char) -> Option<KeyPress> {
    let (key, code, virtual_key, shift) = us_key(character)?;
    let text = match character {
        '\n' | '\r' => Some("\r".to_string()),
        '\t' => None,
        other => Some(other.to_string()),
    };
    Some(KeyPress {
        key,
        code,
        virtual_key,
        modifiers: if shift { SHIFT } else { 0 },
        text,
    })
}

/// A chord for the focused element. Shortcuts the browser itself acts on --
/// new or closed tabs and windows, history, devtools, find, reload, zoom --
/// are refused: CDP hands keys the page does not consume back to the browser.
fn key_chord(keys: &[String]) -> Result<KeyPress> {
    let mut modifiers: u32 = 0;
    let mut primary = None;
    for key in keys {
        match key.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= CTRL,
            "alt" | "option" => modifiers |= ALT,
            "meta" | "command" | "cmd" | "win" | "super" => modifiers |= META,
            "shift" => modifiers |= SHIFT,
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
    let lower = key.to_ascii_lowercase();
    let named: Option<(&str, u32)> = match lower.as_str() {
        "enter" | "return" => Some(("Enter", 13)),
        "tab" => Some(("Tab", 9)),
        "escape" | "esc" => Some(("Escape", 27)),
        "backspace" => Some(("Backspace", 8)),
        "delete" | "del" => Some(("Delete", 46)),
        "insert" => Some(("Insert", 45)),
        "arrowleft" | "left" => Some(("ArrowLeft", 37)),
        "arrowright" | "right" => Some(("ArrowRight", 39)),
        "arrowup" | "up" => Some(("ArrowUp", 38)),
        "arrowdown" | "down" => Some(("ArrowDown", 40)),
        "home" => Some(("Home", 36)),
        "end" => Some(("End", 35)),
        "pageup" => Some(("PageUp", 33)),
        "pagedown" => Some(("PageDown", 34)),
        "space" | "spacebar" => Some((" ", 32)),
        // F2 edits a grid cell; every other function key is the browser's.
        "f2" => Some(("F2", 113)),
        _ => None,
    };
    let character = (key.chars().count() == 1)
        .then(|| key.chars().next().unwrap())
        .filter(|c| us_key(*c).is_some() && !c.is_control());
    let refuse = || {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            format!(
                "{} is a browser-global shortcut or not an element-scoped key; only editing \
                 and navigation keys for the focused element are sent",
                keys.join("+")
            ),
        ))
    };
    if modifiers & (ALT | META) != 0 {
        return refuse();
    }
    let navigation = matches!(
        named.map(|(k, _)| k),
        Some("ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End")
    );
    let allowed = match (modifiers & CTRL != 0, modifiers & SHIFT != 0) {
        (false, _) => named.is_some() || character.is_some(),
        // Ctrl+Tab and Ctrl+PageUp/PageDown switch tabs.
        (true, false) => {
            navigation
                || matches!(
                    named.map(|(k, _)| k),
                    Some("Enter" | "Backspace" | "Delete" | " ")
                )
                || matches!(
                    lower.as_str(),
                    "a" | "c" | "v" | "x" | "z" | "y" | "b" | "i"
                )
        }
        // Ctrl+Shift opens windows, devtools and history; selection and redo stay.
        (true, true) => navigation || lower == "z",
    };
    if !allowed {
        return refuse();
    }
    if let Some((name, vk)) = named {
        let code = if name == " " { "Space" } else { name };
        let text = (modifiers & CTRL == 0)
            .then(|| match name {
                "Enter" => Some("\r".to_string()),
                " " => Some(" ".to_string()),
                _ => None,
            })
            .flatten();
        return Ok(KeyPress {
            key: name.into(),
            code: code.into(),
            virtual_key: vk,
            modifiers,
            text,
        });
    }
    let character = character.expect("allowed keys are named or typed");
    // With Ctrl, a letter names its key: Ctrl+A is Ctrl+a, not Ctrl+Shift+a.
    let character = if modifiers & CTRL != 0 {
        character.to_ascii_lowercase()
    } else {
        character
    };
    let (mut key, code, virtual_key, shift) = us_key(character).unwrap();
    if shift {
        modifiers |= SHIFT;
    }
    if modifiers & SHIFT != 0 && !shift {
        // Shift+1 types "!": the key's shifted character, as the layout has it.
        if let Some(shifted) = (' '..='~').find(|c| {
            us_key(*c).is_some_and(|(_, other, _, needs_shift)| other == code && needs_shift)
        }) {
            key = shifted.to_string();
        }
    }
    let text = (modifiers & CTRL == 0).then(|| key.clone());
    Ok(KeyPress {
        key,
        code,
        virtual_key,
        modifiers,
        text,
    })
}

/// Press and release one key. The document must still be the observed one
/// between the two halves.
fn press(client: &mut Client, press: &KeyPress) -> Result<()> {
    let mut down = json!({"type": if press.text.is_some() {"keyDown"} else {"rawKeyDown"},
        "key": press.key, "code": press.code, "windowsVirtualKeyCode": press.virtual_key,
        "nativeVirtualKeyCode": press.virtual_key, "modifiers": press.modifiers});
    if let Some(text) = &press.text {
        down["text"] = json!(text);
        down["unmodifiedText"] = json!(text);
    }
    command(client, "Input.dispatchKeyEvent", down).map_err(|e| e.with_effect("unknown"))?;
    command(
        client,
        "Input.dispatchKeyEvent",
        json!({"type": "keyUp", "key": press.key, "code": press.code,
            "windowsVirtualKeyCode": press.virtual_key, "nativeVirtualKeyCode": press.virtual_key,
            "modifiers": press.modifiers}),
    )
    .map_err(|e| e.with_effect("unknown"))?;
    Ok(())
}

/// What a typing request sends, character by character, checked before
/// anything is sent: a character a US layout cannot type refuses the whole text.
enum Keystrokes {
    Presses(Vec<KeyPress>),
    Text(Vec<String>),
}

fn keystrokes(text: &str, mode: Option<crate::TypeMode>) -> Result<Keystrokes> {
    match mode {
        None | Some(crate::TypeMode::Unicode) => Ok(Keystrokes::Text(
            text.chars().map(|c| c.to_string()).collect(),
        )),
        Some(crate::TypeMode::Keys) => text
            .chars()
            .map(|character| {
                typed_key(character).ok_or_else(|| {
                    error(
                        "CONTROL.INVALID_REQUEST",
                        format!(
                            "character {character:?} has no key on a US layout; use mode \
                             unicode or fill"
                        ),
                    )
                    .with_detail("character", json!(character.to_string()))
                })
            })
            .collect::<Result<Vec<_>>>()
            .map(Keystrokes::Presses),
    }
}

/// Focus the observed element, confirm the keystrokes would reach it, then
/// send a chord or type text, re-confirming focus between characters.
fn keyboard(
    client: &mut Client,
    context: &Ctx,
    args: &Value,
    frame_id: &str,
    loader: &Value,
    action: &Action,
    action_context: &crate::Context,
) -> Result<Value> {
    let strokes = match action {
        Action::Key { keys } => Keystrokes::Presses(vec![key_chord(keys)?]),
        Action::TypeText { text, mode } => keystrokes(text, *mode)?,
        _ => unreachable!("only keyboard actions reach here"),
    };
    action_context.commit()?;
    checked(
        eval(
            client,
            context,
            format!("globalThis.__aadControlV1.act({args})"),
        )
        .map_err(|e| e.with_effect("unknown"))?,
    )?;
    let focused = |client: &mut Client, effect: &str| -> Result<()> {
        let verify = eval(
            client,
            context,
            format!("globalThis.__aadControlV1.verify({args})"),
        )
        .map_err(|e| e.with_effect(effect))?;
        if verify["ok"] != true
            || current_frame(client, context.session.as_deref(), frame_id)
                .map_err(|e| e.with_effect(effect))?["loaderId"]
                != *loader
        {
            return Err(error(
                "CONTROL.FOCUS_LOST",
                "keyboard focus left the observed element, or its document changed",
            )
            .with_effect(effect));
        }
        Ok(())
    };
    focused(client, "contextual")?;
    let (count, operation) = match &strokes {
        Keystrokes::Presses(presses) => (presses.len(), "Input.dispatchKeyEvent"),
        Keystrokes::Text(chunks) => (chunks.len(), "Input.insertText"),
    };
    for index in 0..count {
        if index > 0 {
            // Typing stops where focus moved: later keys would land elsewhere.
            focused(client, "contextual").map_err(|e| e.with_detail("typed", json!(index)))?;
        }
        action_context.commit()?;
        match &strokes {
            Keystrokes::Presses(presses) => press(client, &presses[index])?,
            Keystrokes::Text(chunks) => {
                command(client, "Input.insertText", json!({"text": chunks[index]}))
                    .map_err(|e| e.with_effect("unknown"))?;
            }
        }
    }
    Ok(
        json!({"submitted": true, "native_operation": operation, "trusted_event": true,
        "keystrokes": count}),
    )
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
            launched: true,
            routed: true,
            directory: Some(directory.clone()),
            ..Default::default()
        };
        assert_eq!(downloads.answer()["tracking"], true);
        let attached = Downloads::default();
        assert_eq!(attached.answer()["tracking"], false);
        assert_eq!(
            attached.answer()["reason"],
            "attached browser: downloads are not redirected"
        );
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
        let chord =
            |keys: &[&str]| key_chord(&keys.iter().map(|k| k.to_string()).collect::<Vec<_>>());
        assert_eq!(
            chord(&["ctrl", "a"]).unwrap(),
            KeyPress {
                key: "a".into(),
                code: "KeyA".into(),
                virtual_key: 65,
                modifiers: CTRL,
                text: None
            }
        );
        let enter = chord(&["Enter"]).unwrap();
        assert_eq!((enter.virtual_key, enter.text.as_deref()), (13, Some("\r")));
        // Editing shortcuts scoped to the focused element.
        for keys in [
            &["ctrl", "c"][..],
            &["ctrl", "v"],
            &["ctrl", "x"],
            &["ctrl", "z"],
            &["ctrl", "shift", "z"],
            &["shift", "ArrowLeft"],
            &["ctrl", "shift", "End"],
            &["shift", "Tab"],
            &["a"],
            &["shift", "1"],
            &["F2"],
        ] {
            assert!(chord(keys).is_ok(), "{keys:?}");
        }
        let bang = chord(&["shift", "1"]).unwrap();
        assert_eq!((bang.key.as_str(), bang.text.as_deref()), ("!", Some("!")));
        let shifted = chord(&["shift", "a"]).unwrap();
        assert_eq!((shifted.key.as_str(), shifted.modifiers), ("A", SHIFT));
        // Shortcuts the browser itself acts on are refused.
        for keys in [
            &["ctrl", "t"][..],
            &["ctrl", "w"],
            &["ctrl", "n"],
            &["ctrl", "l"],
            &["ctrl", "u"],
            &["ctrl", "Tab"],
            &["ctrl", "PageDown"],
            &["ctrl", "shift", "t"],
            &["ctrl", "shift", "i"],
            &["F12"],
            &["F5"],
            &["alt", "F4"],
            &["alt", "ArrowLeft"],
            &["meta", "a"],
        ] {
            let refused = chord(keys).unwrap_err();
            assert_eq!(refused.code, "CONTROL.ACTION_UNSUPPORTED", "{keys:?}");
            assert!(refused.message.contains("browser-global"), "{keys:?}");
        }
        assert!(chord(&["Enter", "Tab"]).is_err());
        assert!(chord(&["ctrl"]).is_err());
    }

    #[test]
    fn frames_of_child_sessions_join_the_page_tree_with_their_parents() {
        let tree = json!({"frame": {"id": "main", "loaderId": "l"}, "childFrames": [
            {"frame": {"id": "a", "parentId": "main"}, "childFrames": [{"frame": {"id": "a1", "parentId": "a"}}]},
            {"frame": {"id": "b", "parentId": "main"}},
        ]});
        let mut all = Vec::new();
        tree_entries(&tree, None, &mut all);
        let ids: Vec<_> = all.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["main", "a", "a1", "b"]);
        assert!(all[0].1.main() && all[0].1.root);
        assert_eq!(all[2].1.parent.as_deref(), Some("a"));
        assert!(!all[1].1.root);
        // An out-of-process frame's own tree names no parent for its root;
        // the page's record of it does. Its child is new to the page.
        let child = json!({"frame": {"id": "b", "url": "http://other.test/"}, "childFrames": [
            {"frame": {"id": "b1", "parentId": "b"}}]});
        let mut own = Vec::new();
        tree_entries(&child, Some("S1"), &mut own);
        merge_frames(&mut all, own);
        let b = &all.iter().find(|(id, _)| id == "b").unwrap().1;
        assert_eq!(b.session.as_deref(), Some("S1"));
        assert_eq!(b.parent.as_deref(), Some("main"));
        assert!(b.root && !b.main());
        let b1 = &all.iter().find(|(id, _)| id == "b1").unwrap().1;
        assert_eq!(
            (b1.session.as_deref(), b1.parent.as_deref(), b1.root),
            (Some("S1"), Some("b"), false)
        );
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn accessible_roles_and_names_win_unless_internal_ignored_or_empty() {
        let mut node = json!({"role": "textbox", "name": "q"});
        assert!(apply_accessibility(
            &mut node,
            &json!({"role": {"value": "searchbox"}, "name": {"value": "Search"}})
        ));
        assert_eq!(node, json!({"role": "searchbox", "name": "Search"}));
        let mut label = json!({"role": "generic", "name": "Query"});
        assert!(!apply_accessibility(
            &mut label,
            &json!({"role": {"value": "LabelText"}, "name": {"value": ""}})
        ));
        assert_eq!(label, json!({"role": "generic", "name": "Query"}));
        let mut hidden = json!({"role": "button", "name": "x"});
        assert!(!apply_accessibility(
            &mut hidden,
            &json!({"ignored": true, "role": {"value": "none"}})
        ));
        let mut body = json!({"role": "document", "name": ""});
        apply_accessibility(&mut body, &json!({"role": {"value": "generic"}}));
        assert_eq!(body["role"], "document");
    }

    fn capture() -> Value {
        let mut nodes = vec![
            json!({"node_id": "n0", "role": "document", "name": "", "parent": null}),
            json!({"node_id": "n1", "role": "navigation", "name": "", "parent": "n0", "landmark": true}),
            json!({"node_id": "n2", "role": "link", "name": "Home", "parent": "n1"}),
            json!({"node_id": "n3", "role": "region", "name": "Results", "parent": "n0", "landmark": true}),
            json!({"node_id": "n4", "role": "generic", "name": "", "parent": "n3"}),
            json!({"node_id": "n5", "role": "button", "name": "Buy now", "parent": "n4", "states": {"enabled": true}}),
            json!({"node_id": "n6", "role": "region", "name": "Other", "parent": "n0", "landmark": true}),
            json!({"node_id": "n7", "role": "button", "name": "Buy now", "parent": "n6", "states": {"enabled": false}}),
            json!({"node_id": "n8", "role": "navigation", "name": "", "parent": "n0", "landmark": true}),
        ];
        regions(&mut nodes);
        // Screen order puts Other's button above Results'.
        json!({"nodes": nodes, "order": ["n0", "n1", "n2", "n6", "n7", "n3", "n4", "n5", "n8"]})
    }

    #[test]
    fn landmarks_name_regions_and_nodes_know_theirs() {
        let snapshot = capture();
        let nodes = snapshot["nodes"].as_array().unwrap();
        assert_eq!(nodes[1]["landmark"], "navigation");
        assert_eq!(nodes[8]["landmark"], "navigation (2)");
        assert_eq!(nodes[2]["region"], "navigation");
        assert_eq!(nodes[5]["region"], "Results");
        assert_eq!(nodes[7]["region"], "Other");
        assert!(nodes[0]["region"].is_null() && nodes[3]["region"].is_null());
    }

    #[test]
    fn browser_locators_narrow_by_container_position_text_and_state() {
        let snapshot = capture();
        let find = |locator: Value, expect: crate::Expect| {
            browser_find(&snapshot, &serde_json::from_value(locator).unwrap(), expect)
        };
        let one = crate::Expect::One;
        let ambiguous = find(json!({"role": "button", "name": "Buy now"}), one).unwrap_err();
        assert_eq!(ambiguous.code, "CONTROL.AMBIGUOUS");
        assert_eq!(ambiguous.details["candidates"][0]["region"], "Other");
        let within = find(
            json!({"role": "button", "within": {"name": "Results"}}),
            one,
        )
        .unwrap();
        assert_eq!(within["node_id"], "n5");
        // nth counts top to bottom, so the first is Other's.
        let first = find(json!({"role": "button", "nth": 1}), one).unwrap();
        assert_eq!(
            (first["node_id"].as_str(), first["match_count"].as_u64()),
            (Some("n7"), Some(2))
        );
        assert_eq!(
            find(json!({"role": "button", "nth": "last"}), one).unwrap()["node_id"],
            "n5"
        );
        assert_eq!(
            find(json!({"role": "button", "nth": 3}), crate::Expect::Optional).unwrap()["found"],
            false
        );
        assert_eq!(
            find(json!({"role": "button", "nth": 0}), one)
                .unwrap_err()
                .code,
            "CONTROL.INVALID_REQUEST"
        );
        assert_eq!(
            find(
                json!({"name": "buy", "match": "contains", "states": {"enabled": false}}),
                one
            )
            .unwrap()["node_id"],
            "n7"
        );
        assert_eq!(
            find(json!({"name": "buy"}), crate::Expect::Optional).unwrap()["found"],
            false
        );
        assert_eq!(
            find(
                json!({"role": "link", "within": {"role": "navigation"}}),
                one
            )
            .unwrap_err()
            .code,
            "CONTROL.AMBIGUOUS",
            "two navigation landmarks: within must name one"
        );
        for field in [
            json!({"automation_id": "x"}),
            json!({"near": {"anchor": {"name": "a"}}}),
        ] {
            assert_eq!(
                find(field, one).unwrap_err().code,
                "CONTROL.LOCATOR_UNSUPPORTED"
            );
        }
    }

    #[test]
    fn typing_in_keys_mode_needs_a_us_key_for_every_character() {
        let Keystrokes::Presses(presses) =
            keystrokes("Hi, 9!\n", Some(crate::TypeMode::Keys)).unwrap()
        else {
            panic!("keys mode presses keys")
        };
        let summary: Vec<_> = presses
            .iter()
            .map(|p| (p.code.as_str(), p.modifiers, p.text.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                ("KeyH", SHIFT, Some("H")),
                ("KeyI", 0, Some("i")),
                ("Comma", 0, Some(",")),
                ("Space", 0, Some(" ")),
                ("Digit9", 0, Some("9")),
                ("Digit1", SHIFT, Some("!")),
                ("Enter", 0, Some("\r")),
            ]
        );
        let refused = keystrokes("café", Some(crate::TypeMode::Keys))
            .err()
            .unwrap();
        assert_eq!(refused.code, "CONTROL.INVALID_REQUEST");
        assert_eq!(refused.details["character"], "é");
        // Unicode mode inserts any text, one character at a time.
        let Keystrokes::Text(chunks) = keystrokes("日本é", None).unwrap() else {
            panic!("unicode mode inserts text")
        };
        assert_eq!(chunks, ["日", "本", "é"]);
    }
}
