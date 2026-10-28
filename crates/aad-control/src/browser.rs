//! Explicit CDP page/frame resources. No arbitrary evaluation in the public API.
use crate::{
    error, native::resource_id, Action, Backend, BackendResource, Button, Direction, Resource,
    Result, PAGE_ACTIONS,
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
const MAIN_FRAME_ONLY: [&str; 7] = [
    "key", "scroll", "hover", "navigate", "back", "forward", "reload",
];

struct Page {
    client: Client,
    websocket: String,
}
pub struct BrowserBackend {
    /// The loopback CDP port; 0 until a browser is attached.
    port: AtomicU16,
    pages: Mutex<BTreeMap<String, Page>>,
    world: String,
    last_context: Mutex<Option<(String, i64)>>,
    closed: AtomicBool,
}

impl BrowserBackend {
    pub fn new(port: u16) -> Self {
        Self {
            port: AtomicU16::new(port),
            pages: Mutex::new(BTreeMap::new()),
            world: format!("aad-control-{}", uuid::Uuid::new_v4()),
            last_context: Mutex::new(None),
            closed: AtomicBool::new(false),
        }
    }

    /// Work with the browser on `port` from now on, e.g. one this session started.
    pub fn attach(&self, port: u16) {
        self.pages.lock().unwrap().clear();
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
    ) -> Result<&'a mut Client> {
        if pages
            .get(&target.id)
            .is_some_and(|page| page.websocket != target.webSocketDebuggerUrl)
        {
            pages.remove(&target.id);
        }
        if !pages.contains_key(&target.id) {
            if pages.len() >= 64 {
                if let Some(id) = pages.keys().next().cloned() {
                    pages.remove(&id);
                }
            }
            let client = Client::attach_until(target, deadline).map_err(cdp_error)?;
            pages.insert(
                target.id.clone(),
                Page {
                    client,
                    websocket: target.webSocketDebuggerUrl.clone(),
                },
            );
        }
        let client = &mut pages.get_mut(&target.id).unwrap().client;
        client.set_deadline(deadline);
        Ok(client)
    }

    fn with_page<T>(
        &self,
        page_id: &str,
        timeout: Duration,
        f: impl FnOnce(&mut Client) -> Result<T>,
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
        if result.is_err() {
            pages.remove(page_id);
        }
        result
    }

    fn clear_context(&self, pages: &mut BTreeMap<String, Page>, deadline: Instant) {
        if let Some((page, context)) = self.last_context.lock().unwrap().take() {
            if let Some(page) = pages.get_mut(&page) {
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
            "focus", "click", "fill", "key", "scroll", "hover", "select", "check", "navigate",
            "back", "forward", "reload",
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
        let mut pages = self.pages.lock().unwrap();
        pages.retain(|id, _| targets.iter().any(|target| target.id == *id));
        let mut resources = Vec::new();
        for target in targets.iter().take(64) {
            let client = self.page(&mut pages, target, deadline)?;
            let tree = command(client, "Page.getFrameTree", json!({}))?;
            let root = tree["frameTree"]["frame"]["id"]
                .as_str()
                .ok_or_else(|| error("BROWSER.BAD_RESULT", "missing main frame"))?;
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
                    name:target.title.clone(),capabilities,metadata:json!({"page_id":target.id,"frame_id":frame["id"],"url":frame["url"],"main_frame":main,"click_semantics":"dom_activation; trusted cdp input with button, count or fraction","fill_semantics":"dom_value_and_events","key_semantics":"cdp_input_main_frame","pointer_semantics":"cdp_input_main_frame"})},selector});
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
        self.with_page(page_id,deadline.saturating_duration_since(Instant::now()),|client|{
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
        let page_id = meta["page_id"].as_str().unwrap();
        let frame_id = meta["frame_id"].as_str().unwrap();
        self.with_page(page_id,action_context.remaining()?,|client|{
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
