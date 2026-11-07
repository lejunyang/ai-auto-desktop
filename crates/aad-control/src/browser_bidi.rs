//! Firefox BiDi resources. All script is server-owned and runs in a named,
//! isolated realm. There is no public evaluate tool or CDP fallback.
use crate::{
    browser_access::BrowserAccess, error, native::resource_id, Action, Backend, BackendResource,
    Button, Context, Direction, Expect, Locator, Resource, Result, TypeMode,
};
use aad_browser::bidi::Client;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

pub const PROVIDER: &str = "browser.firefox_bidi";
const ACTIONS: &[&str] = &[
    "focus",
    "invoke",
    "fill",
    "click",
    "hover",
    "scroll",
    "key",
    "type_text",
    "select",
    "check",
    "navigate",
    "back",
    "forward",
    "reload",
    "open_tab",
    "activate_page",
    "close_page",
];
const MAX_FRAMES: usize = 256;

#[derive(Clone)]
struct Frame {
    id: String,
    parent: Option<String>,
    url: String,
}

pub struct FirefoxBackend {
    port: u16,
    access: BrowserAccess,
    world: String,
    client: Mutex<Option<Client>>,
}

fn unsupported() -> aad_core::AutomationError {
    error(
        "CONTROL.ACTION_UNSUPPORTED",
        "this Firefox BiDi action or parameter is not implemented",
    )
}

fn protocol_error(_: aad_browser::cdp::CdpError) -> aad_core::AutomationError {
    // Protocol exceptions may contain a page's text, arguments or script source.
    // Keep them private rather than copying an exception into a control result.
    error(
        "BROWSER.BIDI_FAILED",
        "Firefox BiDi request failed or its session is unavailable",
    )
}

struct Request<'a> {
    deadline: Instant,
    action: Option<&'a Context>,
}
impl Request<'_> {
    fn hold_budget(&self, hold_ms: Option<u64>) -> Result<()> {
        self.remaining()?;
        if hold_ms.is_some_and(|ms| {
            self.deadline.saturating_duration_since(Instant::now())
                < Duration::from_millis(ms.saturating_add(50))
        }) {
            return Err(error(
                "CONTROL.TIMEOUT",
                "Firefox hold exceeds the remaining request budget",
            ));
        }
        Ok(())
    }
    fn remaining(&self) -> Result<()> {
        if let Some(context) = self.action {
            context.remaining()?;
        }
        if Instant::now() >= self.deadline {
            return Err(error("CONTROL.TIMEOUT", "Firefox request deadline elapsed"));
        }
        Ok(())
    }
    fn command(&self, client: &mut Client, method: &str, params: Value) -> Result<Value> {
        self.remaining()?;
        let result = client.command(
            method,
            params,
            self.deadline,
            self.action.map(|c| c.cancellation().shared_flag()),
        );
        self.remaining()?;
        result.map_err(protocol_error)
    }
    fn script(&self, client: &mut Client, target: Value, expression: &str) -> Result<Value> {
        let result = self.command(client, "script.evaluate", json!({"target":target,
            "expression":expression,"awaitPromise":false,"serializationOptions":{"maxObjectDepth":0,"maxDomDepth":0}}))?;
        if result["type"] != "success" {
            return Err(error(
                "CONTROL.STALE_TARGET",
                "Firefox isolated document is unavailable",
            ));
        }
        Ok(result)
    }
    fn value(&self, client: &mut Client, realm: &str, expression: &str) -> Result<Value> {
        let result = self.script(
            client,
            json!({"realm":realm}),
            &format!("JSON.stringify({expression})"),
        )?;
        let value: Value = serde_json::from_str(
            result["result"]["value"].as_str().unwrap_or("null"),
        )
        .map_err(|_| {
            error(
                "BROWSER.BAD_RESULT",
                "Firefox returned an invalid private result",
            )
        })?;
        if let Some(code) = value["error"].as_str() {
            return Err(error(
                code,
                "Firefox observed element no longer admits this operation",
            ));
        }
        Ok(value)
    }
    fn commit(&self) -> Result<()> {
        self.remaining()?;
        if let Some(action) = self.action {
            action.commit()?;
        }
        Ok(())
    }
}

fn frames(client: &mut Client, request: &Request<'_>) -> Result<BTreeMap<String, Frame>> {
    let tree = request.command(client, "browsingContext.getTree", json!({}))?;
    let mut frames = BTreeMap::new();
    let mut pending: VecDeque<(Value, Option<String>)> = tree["contexts"]
        .as_array()
        .into_iter()
        .flatten()
        .cloned()
        .map(|v| (v, None))
        .collect();
    while let Some((entry, parent)) = pending.pop_front() {
        if frames.len() >= MAX_FRAMES {
            return Err(unsupported());
        }
        let id = entry["context"]
            .as_str()
            .ok_or_else(unsupported)?
            .to_owned();
        for child in entry["children"].as_array().into_iter().flatten() {
            pending.push_back((child.clone(), Some(id.clone())));
        }
        if frames
            .insert(
                id.clone(),
                Frame {
                    id,
                    parent,
                    url: entry["url"].as_str().unwrap_or_default().into(),
                },
            )
            .is_some()
        {
            return Err(error(
                "BROWSER.BAD_RESULT",
                "Firefox frame identities are ambiguous",
            ));
        }
    }
    Ok(frames)
}

impl FirefoxBackend {
    pub fn new(port: u16, access: BrowserAccess) -> Self {
        Self {
            port,
            access,
            world: format!("aad-bidi-{}", uuid::Uuid::new_v4()),
            client: Mutex::new(None),
        }
    }
    fn lock(&self, request: &Request<'_>) -> Result<MutexGuard<'_, Option<Client>>> {
        loop {
            request.remaining()?;
            match self.client.try_lock() {
                Ok(mut state) => {
                    if state.is_none() {
                        *state = Some(
                            Client::connect(self.port, request.deadline).map_err(protocol_error)?,
                        );
                    }
                    return Ok(state);
                }
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(error("BROWSER.BIDI_FAILED", "Firefox session state failed"))
                }
            }
        }
    }
    fn authorized(&self, tree: &BTreeMap<String, Frame>, id: &str) -> Result<Frame> {
        let frame = tree.get(id).ok_or_else(|| {
            error(
                "CONTROL.STALE_TARGET",
                "Firefox browsing context disappeared",
            )
        })?;
        let mut current = Some(frame);
        for _ in 0..MAX_FRAMES {
            let Some(entry) = current else {
                return Ok(frame.clone());
            };
            self.access.check_document(&entry.url)?;
            current = entry
                .parent
                .as_ref()
                .map(|parent| {
                    tree.get(parent).ok_or_else(|| {
                        error("CONTROL.STALE_TARGET", "Firefox parent frame disappeared")
                    })
                })
                .transpose()?;
        }
        Err(error(
            "CONTROL.STALE_TARGET",
            "Firefox frame ancestry exceeded its bound",
        ))
    }
    fn realm(&self, client: &mut Client, frame: &Frame, request: &Request<'_>) -> Result<String> {
        let source = include_str!("browser.js")
            .replace("/*AAD_NAMING*/", include_str!("browser_bidi/naming.js"))
            .replace(
                "NONCE",
                &serde_json::to_string(&uuid::Uuid::new_v4().to_string()).unwrap(),
            );
        let result = request.script(
            client,
            json!({"context":frame.id,"sandbox":self.world}),
            "true",
        )?;
        let realm = result["realm"].as_str().ok_or_else(unsupported)?.to_owned();
        // Creating a sandbox can race a navigation. Authorize its current
        // document before any private DOM scan, then address this exact realm.
        let current = self.authorized(&frames(client, request)?, &frame.id)?;
        if current.parent != frame.parent {
            return Err(error(
                "CONTROL.STALE_TARGET",
                "Firefox frame ancestry changed",
            ));
        }
        request.script(client, json!({"realm":realm}), &source)?;
        request.script(
            client,
            json!({"realm":realm}),
            include_str!("browser_bidi/private.js"),
        )?;
        self.protection(client, &realm, request)?;
        Ok(realm)
    }
    fn protection(&self, client: &mut Client, realm: &str, request: &Request<'_>) -> Result<()> {
        request.value(
            client,
            realm,
            "globalThis.__aadControlV1.reset_closed_roots()",
        )?;
        let mut roots = VecDeque::from([None]);
        let mut seen = BTreeSet::new();
        let mut count = 0usize;
        while let Some(root) = roots.pop_front() {
            let args: Vec<Value> = root.map(|id| json!({"sharedId":id})).into_iter().collect();
            let result = request.command(client, "script.callFunction", json!({
                "target":{"realm":realm},"functionDeclaration":"root => { if(root)globalThis.__aadControlV1.mark_closed_root(root);return globalThis.__aadBidiV1.scan(root||document); }",
                "arguments":args,"awaitPromise":false,"serializationOptions":{"maxObjectDepth":1,"maxDomDepth":0,"includeShadowTree":"all"}}))?;
            if result["type"] != "success" {
                return Err(unsupported());
            }
            let nodes = result["result"]["value"]
                .as_array()
                .ok_or_else(unsupported)?;
            count += nodes.len();
            if count > 20000 {
                return Err(unsupported());
            }
            for node in nodes {
                if let Some(id) = node["value"]["shadowRoot"]["sharedId"].as_str() {
                    if node["value"]["shadowRoot"]["value"]["mode"] == "closed"
                        && seen.insert(id.to_owned())
                    {
                        roots.push_back(Some(id.to_owned()));
                    }
                }
            }
        }
        request.value(
            client,
            realm,
            "globalThis.__aadControlV1.record_closed_protection()",
        )?;
        Ok(())
    }
    fn fresh(
        &self,
        client: &mut Client,
        snapshot: &Value,
        request: &Request<'_>,
    ) -> Result<(String, String)> {
        let id = snapshot["metadata"]["context"]
            .as_str()
            .ok_or_else(unsupported)?;
        let tree = frames(client, request)?;
        let frame = self.authorized(&tree, id)?;
        if json!(frame.parent) != snapshot["metadata"]["parent"] {
            return Err(error(
                "CONTROL.STALE_TARGET",
                "Firefox frame ancestry changed",
            ));
        }
        let realm = snapshot["metadata"]["realm"]
            .as_str()
            .ok_or_else(unsupported)?;
        let active = request.script(client, json!({"context":id,"sandbox":self.world}), "true")?;
        if active["realm"] != realm {
            return Err(error(
                "CONTROL.STALE_TARGET",
                "Firefox isolated document was replaced",
            ));
        }
        // A realm reference, unlike a context+name lookup, cannot silently bind
        // to a newly navigated document with an identical-looking element.
        self.protection(client, realm, request)?;
        request.remaining()?;
        Ok((id.to_owned(), realm.to_owned()))
    }
    fn observe_inner(
        &self,
        selector: &Value,
        scope: Option<&str>,
        root: Option<(&Value, &Value)>,
        max_nodes: usize,
        timeout: Duration,
    ) -> Result<Value> {
        let request = Request {
            deadline: Instant::now() + timeout,
            action: None,
        };
        let mut state = self.lock(&request)?;
        let client = state.as_mut().unwrap();
        let id = selector["context"].as_str().ok_or_else(unsupported)?;
        let frame = self.authorized(&frames(client, &request)?, id)?;
        let realm = self.realm(client, &frame, &request)?;
        let observation = uuid::Uuid::new_v4().to_string();
        let expression = if let Some((snapshot, node)) = root {
            if snapshot["metadata"]["realm"] != realm {
                return Err(error(
                    "CONTROL.STALE_TARGET",
                    "Firefox scope document changed",
                ));
            }
            let args = json!({"document_token":snapshot["document_token"],"observation":snapshot["snapshot_id"],"node_id":node["node_id"]});
            format!("(()=>{{const root=globalThis.__aadControlV1.scope_element({args});return root?globalThis.__aadControlV1.observe({}, {max_nodes}, {}, root):{{error:'CONTROL.STALE_TARGET'}};}})()",json!(observation),json!(format!("aad:bidi-scope:{}",uuid::Uuid::new_v4())))
        } else {
            format!(
                "globalThis.__aadControlV1.observe({}, {max_nodes}, {})",
                json!(observation),
                json!(scope)
            )
        };
        let mut result = request.value(client, &realm, &expression)?;
        let history = request.value(client, &realm, "globalThis.__aadBidiV1.history_targets()")?;
        for node in result["nodes"].as_array_mut().into_iter().flatten() {
            let protected = node["states"]["protected"] == true;
            if protected {
                node["name"] = json!("");
                node["value"] = Value::Null;
            }
            if let Some(actions) = node["actions"].as_array_mut() {
                actions.retain(|a| {
                    frame.parent.is_none()
                        && ACTIONS.iter().any(|name| a == name)
                        && (!matches!(a.as_str(), Some("back" | "forward"))
                            || history[a.as_str().unwrap()]["destination"]["url"]
                                .as_str()
                                .is_some_and(|url| self.access.check_document(url).is_ok()))
                });
                if actions.iter().any(|a| a == "click") {
                    actions.push(json!("invoke"));
                }
            }
        }
        result["snapshot_id"] = json!(observation);
        result["metadata"] = json!({"context":id,"realm":realm,"parent":frame.parent,"url":frame.url,
            "main_frame":frame.parent.is_none(),"accessibility":{"applied":false,"source":"bounded DOM semantics"},"closed_shadow_inspection":{"complete":true}});
        let current = self.authorized(&frames(client, &request)?, id)?;
        let active = request.script(client, json!({"context":id,"sandbox":self.world}), "true")?;
        if current.parent != frame.parent || active["realm"] != realm {
            return Err(error(
                "CONTROL.STALE_TARGET",
                "Firefox document changed during observation",
            ));
        }
        result["metadata"]["url"] = json!(current.url);
        Ok(result)
    }
    fn input_check(
        &self,
        client: &mut Client,
        snapshot: &Value,
        args: &Value,
        request: &Request<'_>,
    ) -> Result<(String, String)> {
        let (id, realm) = self.fresh(client, snapshot, request)?;
        if snapshot["metadata"]["main_frame"] != true {
            return Err(unsupported());
        }
        request.value(
            client,
            &realm,
            &format!("globalThis.__aadControlV1.confirm({args})"),
        )?;
        self.authorized(&frames(client, request)?, &id)?;
        request.remaining()?;
        Ok((id, realm))
    }
    fn release(client: &mut Client, id: &str) -> bool {
        client
            .command(
                "input.releaseActions",
                json!({"context":id}),
                Instant::now() + Duration::from_secs(1),
                None,
            )
            .is_ok()
    }
    fn hold(
        &self,
        client: &mut Client,
        snapshot: &Value,
        args: &Value,
        point: Option<(i64, i64)>,
        request: &Request<'_>,
        hold_ms: Option<u64>,
    ) -> Result<()> {
        let Some(ms) = hold_ms else {
            return Ok(());
        };
        let until = Instant::now() + Duration::from_millis(ms);
        loop {
            let (_, realm) = self.input_check(client, snapshot, args, request)?;
            if let Some((x, y)) = point {
                let current = request.value(
                    client,
                    &realm,
                    &format!("globalThis.__aadBidiV1.point({args})"),
                )?;
                if current["x"] != x || current["y"] != y {
                    return Err(error(
                        "CONTROL.STALE_TARGET",
                        "Firefox held pointer anchor moved",
                    ));
                }
            } else if request.value(
                client,
                &realm,
                &format!("globalThis.__aadControlV1.verify({args})"),
            )?["ok"]
                != true
            {
                return Err(error(
                    "CONTROL.FOCUS_FAILED",
                    "Firefox focus left the held key anchor",
                ));
            }
            request.remaining()?;
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(10)));
        }
    }
    fn navigate(
        &self,
        client: &mut Client,
        id: &str,
        url: &str,
        request: &Request<'_>,
    ) -> Result<Value> {
        let url = self.access.navigation_url(url)?;
        request.commit()?;
        request.command(
            client,
            "browsingContext.navigate",
            json!({"context":id,"url":url,"wait":"interactive"}),
        )?;
        // A redirect can leave the requested grant. Do not read the destination
        // DOM or claim success under the source URL's authorization.
        let current = self.authorized(&frames(client, request)?, id)?;
        Ok(
            json!({"submitted":true,"native_operation":"WebDriver BiDi browsingContext.navigate","page_id":id,"url":current.url}),
        )
    }
    fn history(
        &self,
        client: &mut Client,
        snapshot: &Value,
        args: &Value,
        request: &Request<'_>,
    ) -> Result<Value> {
        let (id, realm) = self.input_check(client, snapshot, args, request)?;
        let targets = request.value(client, &realm, "globalThis.__aadBidiV1.history_targets()")?;
        let target = &targets[args["action"].as_str().ok_or_else(unsupported)?];
        let url = target["destination"]["url"]
            .as_str()
            .ok_or_else(unsupported)?;
        let entry = target["destination"]["id"]
            .as_str()
            .ok_or_else(unsupported)?;
        self.access.check_document(url)?;
        self.input_check(client, snapshot, args, request)?;
        request.commit()?;
        request.value(
            client,
            &realm,
            &format!("globalThis.__aadBidiV1.history({args},{target})"),
        )?;
        loop {
            self.authorized(&frames(client, request)?, &id)?;
            // Traversal can replace the realm. Read only browser-owned history
            // identity from the current context until the authorized key lands.
            let current=request.script(client,json!({"context":id,"sandbox":self.world}),
                "JSON.stringify({id:globalThis.navigation?.currentEntry?.id,failed:globalThis.__aadBidiHistoryFailed===true})")?;
            let current: Value =
                serde_json::from_str(current["result"]["value"].as_str().unwrap_or("null"))
                    .map_err(|_| unsupported())?;
            if current["failed"] == true {
                return Err(error(
                    "BROWSER.NAVIGATION_FAILED",
                    "Firefox history traversal did not commit",
                ));
            }
            if current["id"] == entry {
                let current = self.authorized(&frames(client, request)?, &id)?;
                return Ok(
                    json!({"submitted":true,"native_operation":"DOM Navigation.traverseTo","page_id":id,"url":current.url}),
                );
            }
            request.remaining()?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn open_tab(
        &self,
        client: &mut Client,
        snapshot: &Value,
        args: &Value,
        url: &str,
        request: &Request<'_>,
    ) -> Result<Value> {
        let url = self.access.navigation_url(url)?;
        let (source, _) = self.input_check(client, snapshot, args, request)?;
        request.commit()?;
        let created = request.command(
            client,
            "browsingContext.create",
            json!({"type":"tab","referenceContext":source,"background":false}),
        )?;
        let id = created["context"].as_str().ok_or_else(unsupported)?;
        let result = (|| {
            // Creating a tab and navigating it are separate BiDi commands.
            // Recheck the observed source and observe/fresh-check the new blank
            // document before the second write, rather than trusting its ID.
            self.input_check(client, snapshot, args, request)?;
            let frame = self.authorized(&frames(client, request)?, id)?;
            if frame.parent.is_some() || frame.url != "about:blank" {
                return Err(error(
                    "CONTROL.STALE_TARGET",
                    "Firefox new tab changed before navigation",
                ));
            }
            let realm = self.realm(client, &frame, request)?;
            let observation = uuid::Uuid::new_v4().to_string();
            let mut blank = request.value(
                client,
                &realm,
                &format!(
                    "globalThis.__aadControlV1.observe({}, 1)",
                    json!(observation)
                ),
            )?;
            let body = blank["nodes"]
                .as_array()
                .and_then(|nodes| nodes.iter().find(|node| node["tag"] == "body"))
                .ok_or_else(unsupported)?;
            let blank_args = json!({"document_token":blank["document_token"],"observation":observation,"node_id":body["node_id"],"action":"navigate"});
            blank["metadata"] = json!({"context":id,"realm":realm,"parent":null,"main_frame":true});
            self.input_check(client, &blank, &blank_args, request)?;
            self.navigate(client, id, &url, request)
        })();
        // Never close a tab whose document may have changed while handling an
        // error. Expose the created identity so the caller can observe/close it.
        result.map_err(|e| e.with_detail("created_page_id", json!(id)))
    }
}

impl Backend for FirefoxBackend {
    fn close(&self) {
        if let Ok(mut state) = self.client.lock() {
            state.take();
        }
    }
    fn name(&self) -> &str {
        PROVIDER
    }
    fn capabilities(&self) -> Vec<String> {
        ACTIONS.iter().map(|s| (*s).into()).collect()
    }
    fn reads(&self) -> Vec<String> {
        vec!["capture".into()]
    }
    fn lease_keys(&self, _snapshot: &Value) -> Result<Vec<String>> {
        // Conservative until process/start identity evidence is implemented.
        // Every endpoint attachment uses the same key, independent of session ID.
        Ok(vec![
            format!("browser-bidi-{}", self.port),
            "desktop-input".into(),
        ])
    }
    fn resources(&self, timeout: Duration) -> Result<Vec<BackendResource>> {
        let request = Request {
            deadline: Instant::now() + timeout,
            action: None,
        };
        let mut state = self.lock(&request)?;
        let client = state.as_mut().unwrap();
        let tree = frames(client, &request)?;
        Ok(tree.values().map(|frame|{
            let selector=json!({"context":frame.id});
            let access=self.authorized(&tree,&frame.id);
            let capabilities=if access.is_ok()&&frame.parent.is_none(){self.capabilities()}else{Vec::new()};
            BackendResource{resource:Resource{resource_id:resource_id(PROVIDER,&selector),provider:PROVIDER.into(),kind:"browser".into(),name:frame.url.clone(),capabilities,
                metadata:json!({"context":frame.id,"parent":frame.parent,"main_frame":frame.parent.is_none(),"url":frame.url,"access_error":access.err().map(|e|json!({"code":e.code,"message":e.message}))})},selector}
        }).collect())
    }
    fn observe(&self, selector: &Value, max_nodes: usize, timeout: Duration) -> Result<Value> {
        self.observe_inner(selector, None, None, max_nodes, timeout)
    }
    fn observe_scope(
        &self,
        selector: &Value,
        scope: &str,
        max_nodes: usize,
        timeout: Duration,
    ) -> Result<Value> {
        self.observe_inner(selector, Some(scope), None, max_nodes, timeout)
    }
    fn observe_target_scope(
        &self,
        snapshot: &Value,
        node: &Value,
        max_nodes: usize,
        timeout: Duration,
    ) -> Result<Value> {
        self.observe_inner(
            &json!({"context":snapshot["metadata"]["context"]}),
            None,
            Some((snapshot, node)),
            max_nodes,
            timeout,
        )
    }
    fn find(
        &self,
        snapshot: &Value,
        locator: &Locator,
        expect: Expect,
        _timeout: Duration,
    ) -> Result<Option<Value>> {
        crate::browser::browser_find(snapshot, locator, expect).map(Some)
    }
    fn capture(&self, snapshot: &Value, node: &Value, timeout: Duration) -> Result<Value> {
        if snapshot["metadata"]["main_frame"] != true {
            return Err(unsupported());
        }
        let request = Request {
            deadline: Instant::now() + timeout,
            action: None,
        };
        let mut state = self.lock(&request)?;
        let client = state.as_mut().unwrap();
        let (id, realm) = self.fresh(client, snapshot, &request)?;
        // A screenshot may contain descendant documents. Their grants must
        // remain valid even though first-version clipping excludes their pixels.
        let tree = frames(client, &request)?;
        for frame in tree.values() {
            let mut at = frame;
            for _ in 0..MAX_FRAMES {
                if at.id == id {
                    self.authorized(&tree, &frame.id)?;
                    break;
                }
                let Some(parent) = at.parent.as_ref().and_then(|p| tree.get(p)) else {
                    break;
                };
                at = parent;
            }
        }
        let args = json!({"document_token":snapshot["document_token"],"observation":snapshot["snapshot_id"],"node_id":node["node_id"]});
        request.value(
            client,
            &realm,
            &format!("globalThis.__aadBidiV1.capture({args})"),
        )?;
        let reference = request.script(
            client,
            json!({"realm":realm}),
            &format!("globalThis.__aadControlV1.scope_element({args})"),
        )?;
        let shared = reference["result"]["sharedId"]
            .as_str()
            .ok_or_else(unsupported)?;
        self.authorized(&frames(client, &request)?, &id)?;
        let shot=request.command(client,"browsingContext.captureScreenshot",json!({"context":id,"clip":{"type":"element","element":{"sharedId":shared}},"format":{"type":"image/png"}}))?;
        self.fresh(client, snapshot, &request)?;
        request.value(
            client,
            &realm,
            &format!("globalThis.__aadBidiV1.capture({args})"),
        )?;
        use base64::Engine;
        let data = shot["data"].as_str().ok_or_else(unsupported)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| unsupported())?;
        let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .map_err(|_| unsupported())?;
        Ok(json!({"format":"png","data":data,"width":image.width(),"height":image.height()}))
    }
    fn act(
        &self,
        snapshot: &Value,
        node: &Value,
        action: &Action,
        context: &Context,
    ) -> Result<Value> {
        validate_action(action)?;
        if let Action::Navigate { url } | Action::OpenTab { url } = action {
            self.access.navigation_url(url)?;
        }
        let request = Request {
            deadline: Instant::now() + context.remaining()?,
            action: Some(context),
        };
        let mut state = self.lock(&request)?;
        let client = state.as_mut().unwrap();
        let action_name = if matches!(action, Action::Invoke {}) {
            "click"
        } else {
            action.name()
        };
        let mut args = json!({"document_token":snapshot["document_token"],"observation":snapshot["snapshot_id"],"node_id":node["node_id"],"action":action_name});
        let (id, realm) = self.input_check(client, snapshot, &args, &request)?;
        match action {
            Action::Back {} | Action::Forward {} => self.history(client, snapshot, &args, &request),
            Action::Navigate { url } => self.navigate(client, &id, url, &request),
            Action::OpenTab { url } => self.open_tab(client, snapshot, &args, url, &request),
            Action::Reload {} => {
                request.commit()?;
                request.command(
                    client,
                    "browsingContext.reload",
                    json!({"context":id,"wait":"interactive"}),
                )?;
                let current = self.authorized(&frames(client, &request)?, &id)?;
                Ok(
                    json!({"submitted":true,"native_operation":"WebDriver BiDi browsingContext.reload","page_id":id,"url":current.url}),
                )
            }
            Action::ActivatePage {} | Action::ClosePage {} => {
                let method = if matches!(action, Action::ActivatePage {}) {
                    "browsingContext.activate"
                } else {
                    "browsingContext.close"
                };
                request.commit()?;
                // Do not set promptUnload=false to bypass a page's beforeunload
                // protection; unsupported prompts remain explicit failures.
                let params = if matches!(action, Action::ClosePage {}) {
                    json!({"context":id,"promptUnload":true})
                } else {
                    json!({"context":id})
                };
                request.command(client, method, params)?;
                Ok(
                    json!({"submitted":true,"native_operation":format!("WebDriver BiDi {method}"),"page_id":id}),
                )
            }
            Action::Focus {}
            | Action::Fill { .. }
            | Action::Invoke {}
            | Action::Select { .. }
            | Action::Check { .. } => {
                match action {
                    Action::Fill { value } => args["value"] = json!(value),
                    Action::Select { option } => args["option"] = json!(option),
                    Action::Check { checked } => args["checked"] = json!(checked),
                    _ => {}
                }
                request.commit()?;
                request.value(
                    client,
                    &realm,
                    &format!("globalThis.__aadControlV1.act({args})"),
                )
            }
            Action::Click { fraction, .. }
            | Action::Hover { fraction, .. }
            | Action::Scroll { fraction, .. } => {
                args["fraction"] = json!(fraction);
                request.commit()?;
                let point = request.value(
                    client,
                    &realm,
                    &format!("globalThis.__aadBidiV1.point({args})"),
                )?;
                let x = point["x"].as_f64().ok_or_else(unsupported)?.round() as i64;
                let y = point["y"].as_f64().ok_or_else(unsupported)?.round() as i64;
                let result = (|| {
                    let count = match action {
                        Action::Click { count, .. } => count.unwrap_or(1),
                        _ => 1,
                    };
                    for _ in 0..count {
                        self.input_check(client, snapshot, &args, &request)?;
                        args["noscroll"] = json!(true);
                        let current = request.value(
                            client,
                            &realm,
                            &format!("globalThis.__aadBidiV1.point({args})"),
                        )?;
                        if (current["x"].as_f64().unwrap_or(f64::NAN) - x as f64).abs() > 1.0
                            || (current["y"].as_f64().unwrap_or(f64::NAN) - y as f64).abs() > 1.0
                        {
                            return Err(error(
                                "CONTROL.STALE_TARGET",
                                "Firefox pointer anchor moved",
                            ));
                        }
                        request.commit()?;
                        if let Action::Scroll {
                            direction, notches, ..
                        } = action
                        {
                            let amount = i64::from(notches.unwrap_or(1)) * 80;
                            let (dx, dy) = match direction {
                                Direction::Up => (0, -amount),
                                Direction::Down => (0, amount),
                                Direction::Left => (-amount, 0),
                                Direction::Right => (amount, 0),
                            };
                            request.command(client,"input.performActions",json!({"context":id,"actions":[{"type":"wheel","id":"aad-wheel","actions":[{"type":"scroll","x":x,"y":y,"deltaX":dx,"deltaY":dy,"duration":0,"origin":"viewport"}]}]}))?;
                        } else {
                            pointer(
                                client,
                                &request,
                                &id,
                                json!({"type":"pointerMove","x":x,"y":y,"origin":"viewport","duration":0}),
                            )?;
                            if let Action::Click {
                                button, hold_ms, ..
                            } = action
                            {
                                self.input_check(client, snapshot, &args, &request)?;
                                let current = request.value(
                                    client,
                                    &realm,
                                    &format!("globalThis.__aadBidiV1.point({args})"),
                                )?;
                                if current["x"] != x || current["y"] != y {
                                    return Err(error(
                                        "CONTROL.STALE_TARGET",
                                        "Firefox pointer anchor moved after hover",
                                    ));
                                }
                                request.commit()?;
                                let button = match button.unwrap_or(Button::Left) {
                                    Button::Left => 0,
                                    Button::Middle => 1,
                                    Button::Right => 2,
                                };
                                request.hold_budget(*hold_ms)?;
                                pointer(
                                    client,
                                    &request,
                                    &id,
                                    json!({"type":"pointerDown","button":button}),
                                )?;
                                self.hold(
                                    client,
                                    snapshot,
                                    &args,
                                    Some((x, y)),
                                    &request,
                                    *hold_ms,
                                )?;
                                self.input_check(client, snapshot, &args, &request)?;
                                request.commit()?;
                                pointer(
                                    client,
                                    &request,
                                    &id,
                                    json!({"type":"pointerUp","button":button}),
                                )?;
                            }
                        }
                    }
                    Ok(
                        json!({"submitted":true,"trusted_event":true,"native_operation":"WebDriver BiDi input.performActions"}),
                    )
                })();
                let cleared = Self::release(client, &id);
                match result {
                    Err(e) => Err(e.with_detail("residual_state_cleared", json!(cleared))),
                    Ok(v) if cleared => Ok(v),
                    Ok(_) => Err(error(
                        "CONTROL.INPUT_RELEASE_FAILED",
                        "Firefox input release was not confirmed",
                    )),
                }
            }
            Action::Key { .. } | Action::TypeText { .. } => {
                let hold_ms = match action {
                    Action::Key { hold_ms, .. } => *hold_ms,
                    _ => None,
                };
                let chords = match action {
                    Action::Key { keys, .. } => vec![key_values(keys)?],
                    Action::TypeText { text, .. } => {
                        text.chars().map(|c| vec![c.to_string()]).collect()
                    }
                    _ => unreachable!(),
                };
                request.commit()?;
                request.value(
                    client,
                    &realm,
                    &format!("globalThis.__aadControlV1.act({args})"),
                )?;
                let result = (|| {
                    for chord in chords {
                        request.hold_budget(hold_ms)?;
                        self.input_check(client, snapshot, &args, &request)?;
                        if request.value(
                            client,
                            &realm,
                            &format!("globalThis.__aadControlV1.verify({args})"),
                        )?["ok"]
                            != true
                        {
                            return Err(error(
                                "CONTROL.FOCUS_FAILED",
                                "Firefox focus left the observed element",
                            ));
                        }
                        for key in &chord {
                            self.input_check(client, snapshot, &args, &request)?;
                            if request.value(
                                client,
                                &realm,
                                &format!("globalThis.__aadControlV1.verify({args})"),
                            )?["ok"]
                                != true
                            {
                                return Err(error(
                                    "CONTROL.FOCUS_FAILED",
                                    "Firefox focus left the observed element",
                                ));
                            }
                            request.hold_budget(hold_ms)?;
                            request.commit()?;
                            keyboard(client, &request, &id, "keyDown", key)?;
                        }
                        self.hold(client, snapshot, &args, None, &request, hold_ms)?;
                        for key in chord.iter().rev() {
                            self.input_check(client, snapshot, &args, &request)?;
                            request.commit()?;
                            keyboard(client, &request, &id, "keyUp", key)?;
                        }
                    }
                    Ok(
                        json!({"submitted":true,"trusted_event":true,"native_operation":"WebDriver BiDi input.performActions"}),
                    )
                })();
                let cleared = Self::release(client, &id);
                match result {
                    Err(e) => Err(e.with_detail("residual_state_cleared", json!(cleared))),
                    Ok(v) if cleared => Ok(v),
                    Ok(_) => Err(error(
                        "CONTROL.INPUT_RELEASE_FAILED",
                        "Firefox key release was not confirmed",
                    )),
                }
            }
            _ => Err(unsupported()),
        }
    }
}

fn pointer(client: &mut Client, request: &Request<'_>, context: &str, action: Value) -> Result<()> {
    request.command(client,"input.performActions",json!({"context":context,"actions":[{"type":"pointer","id":"aad-pointer","parameters":{"pointerType":"mouse"},"actions":[action]}]})).map(|_|())
}
fn keyboard(
    client: &mut Client,
    request: &Request<'_>,
    context: &str,
    kind: &str,
    key: &str,
) -> Result<()> {
    request.command(client,"input.performActions",json!({"context":context,"actions":[{"type":"key","id":"aad-key","actions":[{"type":kind,"value":key}]}]})).map(|_|())
}
fn key_values(keys: &[String]) -> Result<Vec<String>> {
    // Until headed Firefox accelerator behavior is verified, only one plain
    // editing key or the conventional Select All chord is admitted.
    if keys.len() == 2 && keys[0].eq_ignore_ascii_case("ctrl") && keys[1].eq_ignore_ascii_case("a")
    {
        return Ok(vec!["\u{e009}".into(), "a".into()]);
    }
    if keys.len() != 1 {
        return Err(unsupported());
    }
    let key = &keys[0];
    let value = match key.to_ascii_lowercase().as_str() {
        "backspace" => "\u{e003}",
        "tab" => "\u{e004}",
        "enter" | "return" => "\u{e007}",
        "escape" | "esc" => "\u{e00c}",
        "home" => "\u{e011}",
        "end" => "\u{e010}",
        "left" | "arrowleft" => "\u{e012}",
        "up" | "arrowup" => "\u{e013}",
        "right" | "arrowright" => "\u{e014}",
        "down" | "arrowdown" => "\u{e015}",
        "delete" | "del" => "\u{e017}",
        "space" => " ",
        _ if key.chars().count() == 1
            && !key
                .chars()
                .any(|c| c.is_control() || ('\u{e000}'..='\u{f8ff}').contains(&c)) =>
        {
            key
        }
        _ => return Err(unsupported()),
    };
    Ok(vec![value.into()])
}
fn validate_action(action: &Action) -> Result<()> {
    match action {
        Action::Focus {}
        | Action::Fill { .. }
        | Action::Invoke {}
        | Action::Select { .. }
        | Action::Check { .. }
        | Action::Scroll { .. }
        | Action::Navigate { .. }
        | Action::Back {}
        | Action::Forward {}
        | Action::Reload {}
        | Action::OpenTab { .. }
        | Action::ActivatePage {}
        | Action::ClosePage {} => Ok(()),
        Action::Click {
            modifiers,
            hold_ms,
            count,
            ..
        } if modifiers.is_empty()
            && hold_ms.is_none_or(|ms| (1..=5000).contains(&ms))
            && count.unwrap_or(1) == 1 =>
        {
            Ok(())
        }
        Action::Hover { ms, .. } if ms.is_none() => Ok(()),
        Action::Key { keys, hold_ms } if hold_ms.is_none_or(|ms| (1..=5000).contains(&ms)) => {
            key_values(keys).map(|_| ())
        }
        Action::TypeText { text, mode }
            if *mode == Some(TypeMode::Keys)
                && text
                    .chars()
                    .all(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c)) =>
        {
            Ok(())
        }
        _ => Err(unsupported()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_parameters_are_rejected_before_dispatch() {
        let double: Action = serde_json::from_value(json!({"type":"click","count":2})).unwrap();
        assert!(validate_action(&double).is_err());
        assert!(validate_action(&Action::Key {
            keys: vec!["ctrl".into(), "s".into()],
            hold_ms: None
        })
        .is_err());
        assert!(validate_action(&Action::Key {
            keys: vec!["a".into()],
            hold_ms: Some(5001)
        })
        .is_err());
        assert!(validate_action(&Action::TypeText {
            text: "safe".into(),
            mode: Some(TypeMode::Unicode)
        })
        .is_err());
        assert!(validate_action(&Action::Hover {
            ms: Some(20),
            fraction: None
        })
        .is_err());
        assert!(validate_action(&Action::TypeText {
            text: "\u{e009}".into(),
            mode: Some(TypeMode::Keys)
        })
        .is_err());
        assert!(validate_action(&Action::Key {
            keys: vec!["ctrl".into(), "a".into()],
            hold_ms: None
        })
        .is_ok());
    }
    #[test]
    fn blank_child_cannot_escape_an_ungranted_parent_document() {
        let backend = FirefoxBackend::new(9223, BrowserAccess::default());
        let tree = BTreeMap::from([
            (
                "main".into(),
                Frame {
                    id: "main".into(),
                    parent: None,
                    url: "file:///ungranted/fixture.html".into(),
                },
            ),
            (
                "child".into(),
                Frame {
                    id: "child".into(),
                    parent: Some("main".into()),
                    url: "about:srcdoc".into(),
                },
            ),
        ]);
        assert_eq!(
            backend.authorized(&tree, "child").err().unwrap().code,
            "POLICY.DENIED"
        );
        let mut allowed = tree;
        allowed.get_mut("main").unwrap().url = "https://example.test/".into();
        assert!(backend.authorized(&allowed, "child").is_ok());
        allowed.remove("main");
        assert_eq!(
            backend.authorized(&allowed, "child").err().unwrap().code,
            "CONTROL.STALE_TARGET"
        );
    }
}
