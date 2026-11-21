//! One browser-level Fetch owner. Only this thread writes to its CDP socket.
//! Policy I/O happens on separate threads, including while a tab is idle.
use crate::{
    cdp::Client,
    recording::network::redact_url,
    request_gate::{payload, Decision, Gate, Scope},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

type Key = (String, String);
const TICK: Duration = Duration::from_millis(4);
const REFRESH: Duration = Duration::from_secs(10);
const ATTACH: &str = "Target.setAutoAttach";

#[derive(Default)]
struct History {
    next: u64,
    pending: HashMap<Key, (u64, String, Value)>,
    blocked: VecDeque<(u64, String, Value)>,
}

#[path = "gate_peer.rs"]
mod peer;
pub use peer::{enable_shared_owner, serve_owner};

pub struct GateSession(Owner);
enum Owner {
    Local(Local),
    Peer(peer::Peer),
}

/// Membership lasts until the caller detaches, even if policy arrives later.
#[derive(Default)]
pub struct GateParticipant(Participation);

#[derive(Default)]
enum Participation {
    #[default]
    Detached,
    Local(Arc<GateSession>),
    Shared {
        port: u16,
        _membership: std::fs::File,
        active: Mutex<Option<Arc<GateSession>>>,
    },
}

impl GateParticipant {
    pub fn join(port: u16) -> Result<Self, &'static str> {
        if port == 0 {
            return Ok(Self::default());
        }
        let gate = Gate::from_environment()?;
        let participant = if peer::enabled() {
            let membership = peer::participate(port)?;
            let active = match gate {
                Some(gate) => Some(Arc::new(GateSession(Owner::Peer(peer::Peer::join(
                    port, &gate,
                )?)))),
                None => None,
            };
            Participation::Shared {
                port,
                _membership: membership,
                active: Mutex::new(active),
            }
        } else if let Some(gate) = gate {
            Participation::Local(Arc::new(GateSession(Owner::Local(Local::start(
                port, gate,
            )?))))
        } else {
            Participation::Detached
        };
        Ok(Self(participant))
    }

    pub fn current(&self) -> Result<Option<Arc<GateSession>>, &'static str> {
        match &self.0 {
            Participation::Detached => Ok(None),
            Participation::Local(gate) => Ok(Some(gate.clone())),
            Participation::Shared { port, active, .. } => {
                let mut active = active.lock().unwrap();
                if active.is_none() {
                    *active =
                        peer::Peer::existing(*port)?.map(|p| Arc::new(GateSession(Owner::Peer(p))));
                }
                Ok(active.clone())
            }
        }
    }
}

impl GateSession {
    pub fn healthy(&self) -> bool {
        match &self.0 {
            Owner::Local(l) => l.healthy(),
            Owner::Peer(p) => p.call(&json!(["healthy"])).as_bool() == Some(true),
        }
    }
    fn cursor(&self) -> u64 {
        match &self.0 {
            Owner::Local(l) => l.history.lock().unwrap().next,
            Owner::Peer(p) => p.call(&json!(["cursor"])).as_u64().unwrap_or(0),
        }
    }
    pub fn records(&self, page: &str, from: u64) -> Vec<Value> {
        self.window(page, from, u64::MAX).1
    }
    fn window(&self, page: &str, from: u64, end: u64) -> (bool, Vec<Value>) {
        match &self.0 {
            Owner::Local(l) => l.window(page, from, end),
            Owner::Peer(p) => serde_json::from_value(p.call(&json!(["window", page, from, end])))
                .unwrap_or_default(),
        }
    }
    pub fn tracker(self: &Arc<Self>) -> ActionTracker {
        ActionTracker {
            gate: Arc::downgrade(self),
            from: self.cursor(),
        }
    }
}
struct Local {
    history: Arc<Mutex<History>>,
    stop: mpsc::Sender<()>,
    replies: mpsc::Sender<Reply>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Local {
    fn update(&self, gate: Gate) -> Result<(), &'static str> {
        let scope = gate.scope();
        let (tx, rx) = mpsc::channel();
        self.replies
            .send(Reply::Configure(gate, scope, tx))
            .map_err(|_| "request gate disconnected")?;
        rx.recv().map_err(|_| "request gate disconnected")?
    }
    pub fn start(port: u16, gate: Gate) -> Result<Self, &'static str> {
        let scope = gate.scope();
        let (client, identity) =
            Client::browser_with_identity_until(port, Instant::now() + Duration::from_secs(5))
                .map_err(|_| "request gate could not attach")?;
        let events = client.connection.subscribe(&[
            "Target.attachedToTarget",
            "Target.detachedFromTarget",
            "Target.targetInfoChanged",
            "Fetch.requestPaused",
        ]);
        let (tx, rx) = mpsc::channel();
        let history = Arc::new(Mutex::new(History::default()));
        let mut driver = Driver {
            client,
            gate,
            scope,
            events,
            pages: HashMap::new(),
            history: history.clone(),
            identity,
        };
        driver
            .client
            .command("Target.setDiscoverTargets", json!({"discover":true}))
            .map_err(|_| "request gate discovery failed")?;
        driver
            .client
            .command(ATTACH, attach())
            .map_err(|_| "request gate auto-attach failed")?;
        let (done, answers) = mpsc::channel();
        while driver.events(&done)? {}
        let replies = done.clone();
        let worker = thread::spawn(move || driver.run(rx, done, answers));
        Ok(Self {
            replies,
            stop: tx,
            worker: Some(worker),
            history,
        })
    }
    pub fn healthy(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| !w.is_finished())
    }
    fn window(&self, page: &str, from: u64, end: u64) -> (bool, Vec<Value>) {
        let h = self.history.lock().unwrap();
        let selected = |n: u64, p: &str| n >= from && n < end && p == page;
        let pending = h.pending.values().any(|(n, p, _)| selected(*n, p));
        let records = h
            .blocked
            .iter()
            .filter(|(n, p, _)| selected(*n, p))
            .map(|(_, _, r)| r.clone())
            .collect();
        (pending, records)
    }
}
pub struct ActionTracker {
    gate: std::sync::Weak<GateSession>,
    from: u64,
}
impl ActionTracker {
    pub fn end(&self) -> u64 {
        thread::sleep(TICK * 3);
        self.gate.upgrade().map_or(self.from, |g| g.cursor())
    }
    pub fn finish(self, page: &str, end: u64, cancelled: impl Fn() -> bool) -> Vec<Value> {
        let until = Instant::now() + Duration::from_secs(126);
        loop {
            let Some(gate) = self.gate.upgrade() else {
                return Vec::new();
            };
            let (pending, records) = gate.window(page, self.from, end);
            drop(gate);
            if !pending || cancelled() || Instant::now() >= until {
                return records;
            }
            thread::sleep(TICK * 3);
        }
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

struct Target {
    info: Value,
    page: String,
    fetch: bool,
}
struct Driver {
    events: mpsc::Receiver<Value>,
    client: Client,
    gate: Gate,
    scope: Result<Scope, &'static str>,
    pages: HashMap<String, Target>,
    history: Arc<Mutex<History>>,
    identity: String,
}
enum Reply {
    Configure(
        Gate,
        Result<Scope, &'static str>,
        mpsc::Sender<Result<(), &'static str>>,
    ),
    Scope(u64, Result<Scope, &'static str>),
    Verdict(Key, Option<Value>),
}

fn attach() -> Value {
    json!({"autoAttach":true,"flatten":true,"waitForDebuggerOnStart":true,"filter":[{"type":"browser","exclude":true},{"type":"tab","exclude":true},{}]})
}
fn patterns(scope: Option<&Scope>) -> Value {
    let mut patterns = std::collections::BTreeSet::new();
    for host in scope
        .into_iter()
        .flat_map(|s| &s.watch)
        .map(|w| w.host.as_deref())
        .chain(scope.is_none().then_some(None))
    {
        for scheme in ["http", "https"] {
            if let Some(host) = host {
                let host = host.to_ascii_lowercase();
                for name in [
                    host.trim_end_matches('.').to_owned(),
                    format!("{}.", host.trim_end_matches('.')),
                ] {
                    patterns.insert(format!("{scheme}://{name}/*"));
                    patterns.insert(format!("{scheme}://{name}:*/*"));
                }
            } else {
                patterns.insert(format!("{scheme}://*"));
            }
        }
    }
    json!(patterns
        .into_iter()
        .map(|p| json!({"urlPattern":p,"requestStage":"Request"}))
        .collect::<Vec<_>>())
}
impl Driver {
    fn configure(&mut self, sid: &str) -> Result<(), &'static str> {
        let patterns = patterns(self.scope.as_ref().ok());
        let target = self.pages.get_mut(sid).unwrap();
        if patterns.as_array().unwrap().is_empty() {
            if target.fetch {
                self.client
                    .session_command(Some(sid), "Fetch.disable", json!({}))
                    .map_err(|_| "request gate disable failed")?;
                target.fetch = false;
            }
        } else {
            if let Err(e) =
                self.client
                    .session_command(Some(sid), "Fetch.enable", json!({"patterns":patterns}))
            {
                if target.info["type"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("worker"))
                    && matches!(e,crate::ws::CdpError::Protocol(ref message) if serde_json::from_str::<Value>(message).is_ok_and(|err|err["code"]== -32601))
                {
                    return Ok(());
                }
                return Err("request gate interception failed");
            }
            target.fetch = true;
        }
        Ok(())
    }
    fn events(&mut self, tx: &mpsc::Sender<Reply>) -> Result<bool, &'static str> {
        let batch = self.client.drain_events();
        if batch.dropped != 0 || batch.failure.is_some() {
            return Err("request gate connection interrupted");
        }
        let events = self.events.try_iter().collect::<Vec<_>>();
        let observed = !events.is_empty();
        for e in events {
            let p = &e["params"];
            let sid = p["sessionId"].as_str().unwrap_or_default();
            match e["method"].as_str() {
                Some("Target.attachedToTarget") => {
                    let info = &p["targetInfo"];
                    if matches!(
                        info["type"].as_str(),
                        Some(
                            "page"
                                | "iframe"
                                | "worker"
                                | "service_worker"
                                | "shared_worker"
                                | "webview"
                        )
                    ) {
                        let id = info["targetId"].as_str().unwrap_or_default();
                        // A shared target may be reported by multiple related parents.
                        if self.pages.values().any(|t| t.info["targetId"] == id) {
                            let _ = self.client.session_command(
                                e["sessionId"].as_str(),
                                "Target.detachFromTarget",
                                json!({"sessionId":sid}),
                            );
                            continue;
                        }
                        let page = self
                            .pages
                            .get(e["sessionId"].as_str().unwrap_or_default())
                            .map_or(id, |t| &t.page)
                            .to_owned();
                        self.pages.insert(
                            sid.into(),
                            Target {
                                info: info.clone(),
                                page,
                                fetch: false,
                            },
                        );
                        if let Err(failure) = self.configure(sid) {
                            // A target may disappear between its attach event and our command.
                            if self
                                .client
                                .command("Target.getTargetInfo", json!({"targetId":id}))
                                .is_err()
                            {
                                self.pages.remove(sid);
                                continue;
                            }
                            return Err(failure);
                        }
                        self.client
                            .session_command(Some(sid), ATTACH, attach())
                            .ok();
                    }
                    if p["waitingForDebugger"] == true {
                        self.client
                            .session_command(
                                Some(sid),
                                "Runtime.runIfWaitingForDebugger",
                                json!({}),
                            )
                            .ok();
                    }
                }
                Some("Target.targetInfoChanged") => {
                    let info = &p["targetInfo"];
                    for t in self
                        .pages
                        .values_mut()
                        .filter(|t| t.info["targetId"] == info["targetId"])
                    {
                        t.info = info.clone();
                    }
                }
                Some("Target.detachedFromTarget") => {
                    self.pages.remove(sid);
                    self.history
                        .lock()
                        .unwrap()
                        .pending
                        .retain(|(s, _), _| s != sid);
                }
                Some("Fetch.requestPaused") => {
                    self.paused(e["sessionId"].as_str().unwrap_or_default(), p, tx)?
                }
                _ => {}
            }
        }
        Ok(observed)
    }
    fn resolve(&mut self, key: &Key, allow: bool) -> Result<(), &'static str> {
        let (method, params) = if allow {
            ("Fetch.continueRequest", json!({"requestId":key.1}))
        } else {
            (
                "Fetch.failRequest",
                json!({"requestId":key.1,"errorReason":"BlockedByClient"}),
            )
        };
        self.client
            .session_command(Some(&key.0), method, params)
            .map(|_| ())
            .map_err(|_| "request gate lost paused request")
    }
    fn paused(
        &mut self,
        sid: &str,
        p: &Value,
        tx: &mpsc::Sender<Reply>,
    ) -> Result<(), &'static str> {
        let req = &p["request"];
        let raw = req["url"].as_str().unwrap_or_default();
        let method = req["method"].as_str().unwrap_or_default();
        let key = (
            sid.to_string(),
            p["requestId"].as_str().unwrap_or_default().to_string(),
        );
        if self
            .scope
            .as_ref()
            .is_ok_and(|s| url::Url::parse(raw).is_ok_and(|u| !s.matches(&u, method)))
        {
            // Cancellation or tab closure can invalidate this interception id.
            // Other targets continue on their independent sessions.
            let _ = self.resolve(&key, true);
            return Ok(());
        }
        let target = self.pages.get(sid).ok_or("unknown request gate target")?;
        let page = self
            .pages
            .values()
            .find(|t| t.info["targetId"] == target.page)
            .unwrap_or(target);
        let body = payload(
            req,
            json!({"title":page.info["title"],"url":page.info["url"]}),
            &self.identity,
        );
        let mut record = json!({"method":method,"url":crate::recording::network::truncate(&self.gate.redact(&redact_url(raw)),2048).0,"request_id":p["networkId"]});
        let mut h = self.history.lock().unwrap();
        if h.pending.contains_key(&key) {
            return Ok(());
        }
        let seq = h.next;
        h.next += 1;
        let busy = h.pending.len() >= 128;
        h.pending
            .insert(key.clone(), (seq, target.page.clone(), record.clone()));
        drop(h);

        let gate = self.gate.clone();
        let tx = tx.clone();
        let failure = if busy {
            Some("request gate at capacity")
        } else {
            self.scope.as_ref().err().copied()
        };
        let decide = move || {
            let decision = match failure {
                Some(e) => Err(e),
                None => gate.decide(&body),
            };
            let (kind, message, hint, ask) = match decision {
                Ok(Decision::Allow) => {
                    let _ = tx.send(Reply::Verdict(key, None));
                    return;
                }
                Ok(Decision::Pending { message, ask_id }) => (
                    "pending",
                    message,
                    "Wait for owner approval, then repeat this step.",
                    Some(ask_id),
                ),
                Ok(Decision::Deny { message }) => (
                    "deny",
                    message,
                    "The rule forbids this request. Do not try another route.",
                    None,
                ),
                Err(e) => (
                    "unavailable",
                    e.into(),
                    "Restore the request gate before retrying.",
                    None,
                ),
            };
            record["decision"] = json!(kind);
            record["message"] =
                json!(crate::recording::network::truncate(&gate.redact(&message), 2048).0);
            record["hint"] = json!(hint);
            if let Some(ask) = ask {
                record["askId"] =
                    json!(crate::recording::network::truncate(&gate.redact(&ask), 512).0);
            }
            let _ = tx.send(Reply::Verdict(key, Some(record)));
        };
        if failure.is_some() {
            decide()
        } else {
            thread::spawn(decide);
        }
        Ok(())
    }
    fn reconfigure(&mut self) -> Result<(), &'static str> {
        for sid in self.pages.keys().cloned().collect::<Vec<_>>() {
            if self.scope.as_ref().is_ok_and(|s| s.watch.is_empty())
                && self
                    .history
                    .lock()
                    .unwrap()
                    .pending
                    .keys()
                    .any(|(s, _)| s == &sid)
            {
                continue;
            }
            self.configure(&sid)?;
        }
        Ok(())
    }
    fn run(
        &mut self,
        stop: mpsc::Receiver<()>,
        tx: mpsc::Sender<Reply>,
        answers: mpsc::Receiver<Reply>,
    ) {
        let mut generation = 0;
        let mut refresh = Some(Instant::now() + REFRESH);
        'drive: while stop.recv_timeout(TICK) == Err(mpsc::RecvTimeoutError::Timeout) {
            if self.events(&tx).is_err() {
                break;
            }
            for reply in answers.try_iter() {
                match reply {
                    Reply::Configure(gate, scope, ack) => {
                        generation += 1;
                        self.abort_pending("request gate credentials changed");
                        self.gate = gate;
                        self.scope = scope;
                        let result = self.reconfigure();
                        let failed = result.is_err();
                        let _ = ack.send(result);
                        if failed {
                            break 'drive;
                        }
                        refresh = Some(Instant::now() + REFRESH);
                    }
                    Reply::Scope(current, scope) => {
                        if current != generation {
                            continue;
                        }
                        refresh = Some(Instant::now() + REFRESH);
                        if let Ok(scope) = scope {
                            if self
                                .scope
                                .as_ref()
                                .is_ok_and(|s| s.revision == scope.revision)
                            {
                                continue;
                            }
                            self.scope = Ok(scope);
                            if self.reconfigure().is_err() {
                                break 'drive;
                            }
                        }
                    }
                    Reply::Verdict(key, record) => {
                        if !self.history.lock().unwrap().pending.contains_key(&key) {
                            continue;
                        }
                        let allow = record.is_none();

                        let mut h = self.history.lock().unwrap();
                        let (n, page, _) = h.pending.remove(&key).unwrap();
                        if let Some(record) = record {
                            if h.blocked.len() == 512 {
                                h.blocked.pop_front();
                            }
                            h.blocked.push_back((n, page, record));
                        }
                        let disable = self.scope.as_ref().is_ok_and(|s| s.watch.is_empty())
                            && !h.pending.keys().any(|(s, _)| s == &key.0);
                        drop(h);
                        let _ = self.resolve(&key, allow);
                        if disable && self.configure(&key.0).is_err() {
                            break 'drive;
                        }
                    }
                }
            }
            if refresh.is_some_and(|due| Instant::now() >= due) {
                refresh = None;
                let gate = self.gate.clone();
                let tx = tx.clone();
                thread::spawn(move || {
                    let _ = tx.send(Reply::Scope(generation, gate.scope()));
                });
            }
        }
        self.abort_pending("request gate session ended");
    }
    fn abort_pending(&mut self, message: &str) {
        let mut h = self.history.lock().unwrap();
        let pending = std::mem::take(&mut h.pending);
        let keys = pending
            .into_iter()
            .map(|(key, (n, page, mut record))| {
                record["decision"] = json!("unavailable");
                record["message"] = json!(message);
                if h.blocked.len() == 512 {
                    h.blocked.pop_front();
                }
                h.blocked.push_back((n, page, record));
                key
            })
            .collect::<Vec<_>>();
        drop(h);
        for key in keys {
            let _ = self.resolve(&key, false);
        }
    }
}
