//! One session contract for native desktop providers and browser targets.
//! No network listener: remote sessions inherit an authenticated SSH stdio pipe.

pub mod browser;
pub mod launch;
pub mod lease;
pub mod model;
pub mod native;
pub mod qr;
pub mod recording;
pub mod secrets;
pub mod steps;
pub mod transport;
pub use model::*;
pub use secrets::{valid_secret_ref, SecretBindings};

use aad_core::AutomationError;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, AutomationError>;

/// Ambiguous matches across browser and native desktop providers.
pub fn is_ambiguous(code: &str) -> bool {
    matches!(
        code,
        "CONTROL.AMBIGUOUS" | "DRIVER.AMBIGUOUS_MATCH" | "DRIVER.AMBIGUOUS"
    )
}

pub fn error(code: &str, message: impl Into<String>) -> AutomationError {
    AutomationError::new(code, message).with_effect("not_applied")
}

#[derive(Clone)]
pub struct BackendResource {
    pub resource: Resource,
    pub selector: Value,
}

/// Implementations retain their native stale-target checks. The facade never
/// turns an unsupported native operation into a coordinate or script fallback.
pub trait Backend: Send + Sync {
    fn close(&self) {}
    fn name(&self) -> &str;
    /// Replace an observed protected field from a server-owned binding. A
    /// backend must revalidate protection, editability and target identity.
    /// This is separate from ordinary text input and never uses the clipboard.
    fn secret_input(
        &self,
        _snapshot: &Value,
        _node: &Value,
        _text: &str,
        _context: &Context,
    ) -> Result<Value> {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "secret input is unavailable",
        ))
    }
    fn capabilities(&self) -> Vec<String>;
    fn node_capabilities(&self, node: &Value) -> Vec<String> {
        let supported = self.capabilities();
        match node["actions"].as_array() {
            Some(actions) => supported
                .into_iter()
                .filter(|action| actions.iter().any(|v| v == action))
                .collect(),
            None => supported,
        }
    }
    fn lease_key(&self, _snapshot: &Value) -> Option<String> {
        None
    }
    /// All cooperating input resources needed by this operation. A browser
    /// always owns its instance, and may also need the physical desktop.
    fn lease_keys(&self, snapshot: &Value) -> Result<Vec<String>> {
        Ok(self.lease_key(snapshot).into_iter().collect())
    }
    /// Read-only operations beyond observe and find this provider offers,
    /// such as `capture`; listed by `capabilities` so none is guessed at.
    fn reads(&self) -> Vec<String> {
        Vec::new()
    }
    /// A PNG of an observed node: `{"format": "png", "data": base64, "width", "height"}`.
    fn capture(&self, _snapshot: &Value, _node: &Value, _timeout: Duration) -> Result<Value> {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "this provider cannot take screenshots",
        ))
    }
    /// Cookies and storage keys of a resource, read-only; browsers only.
    fn storage(&self, _selector: &Value, _timeout: Duration, _values: bool) -> Result<Value> {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "this provider has no site storage",
        ))
    }
    /// Recent network requests of a resource, read-only; browsers only.
    fn network(
        &self,
        _selector: &Value,
        _limit: usize,
        _bodies: bool,
        _timeout: Duration,
    ) -> Result<Value> {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "this provider records no network requests",
        ))
    }
    /// OCR over the node's pixels: the PNG read (`format`, `data`, `width`,
    /// `height`), the `engine` and `language`, and `lines` of `{text, bounds,
    /// words}` with bounds in that image's pixels.
    fn read_text(
        &self,
        _snapshot: &Value,
        _node: &Value,
        _language: Option<&str>,
        _timeout: Duration,
    ) -> Result<Value> {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "this provider cannot read text from pixels",
        ))
    }
    /// Whether this provider can press a key chord anchored on nothing.
    fn offers_global_keys(&self) -> bool {
        false
    }
    /// Press a key chord wherever keyboard focus is, anchored on nothing.
    fn global_key(&self, _keys: &[String], _context: &Context) -> Result<Value> {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "this provider cannot press global keys",
        ))
    }
    /// The desktop clipboard's text: `{"has_text", "text", "characters",
    /// "truncated"}`. Not anchored; the session checks its own grant first.
    fn clipboard_read(&self, _max_characters: Option<usize>, _timeout: Duration) -> Result<Value> {
        Err(error(
            "CONTROL.ACTION_UNSUPPORTED",
            "this provider cannot read the clipboard",
        ))
    }
    fn resources(&self, timeout: Duration) -> Result<Vec<BackendResource>>;
    fn observe(&self, selector: &Value, max_nodes: usize, timeout: Duration) -> Result<Value>;
    fn observe_scope(
        &self,
        _selector: &Value,
        _scope: &str,
        _max_nodes: usize,
        _timeout: Duration,
    ) -> Result<Value> {
        Err(error(
            "CONTROL.SCOPE_UNSUPPORTED",
            "provider does not support complete scoped observation",
        ))
    }
    /// Recapture the subtree of an observed node. Implementations must verify
    /// the root's native identity and retain that boundary for later input
    /// revalidation; filtering an incomplete capture is not sufficient.
    fn observe_target_scope(
        &self,
        _snapshot: &Value,
        _node: &Value,
        _max_nodes: usize,
        _timeout: Duration,
    ) -> Result<Value> {
        Err(error(
            "CONTROL.SCOPE_UNSUPPORTED",
            "provider does not support observation rooted at an observed node",
        ))
    }
    /// Shape a retained snapshot for the caller. `None` uses the portable
    /// outline; a provider answers with `nodes` keyed by `node_id`, or `regions`.
    fn view(&self, _snapshot: &Value, _view: &View, _timeout: Duration) -> Result<Option<Value>> {
        Ok(None)
    }
    /// Resolve a locator inside a retained snapshot, answering
    /// `{"found", "node_id", "match_count"}`. `None` uses the portable exact matcher.
    fn find(
        &self,
        _snapshot: &Value,
        _locator: &Locator,
        _expect: Expect,
        _timeout: Duration,
    ) -> Result<Option<Value>> {
        Ok(None)
    }
    fn act(
        &self,
        snapshot: &Value,
        node: &Value,
        action: &Action,
        context: &Context,
    ) -> Result<Value>;
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// Input is denied unless the trusted server operator explicitly grants it.
    pub allow_input: bool,
    /// A separate server grant for writing protected fields using bindings.
    pub allow_secret_input: bool,
    /// Values stay server-side; protocol clients can only name a binding.
    pub secret_bindings: SecretBindings,
    /// Empty means all installed providers in this OS account, never arbitrary plugins.
    pub providers: BTreeSet<String>,
    /// Optional server-owned allowlist; a client cannot extend it.
    pub resources: Option<BTreeSet<String>>,
    /// Programs `launch` may start, as the operator named them (an absolute
    /// path or a bare name on PATH). Empty: launching is refused. Needs
    /// `allow_input` as well.
    pub launch: BTreeSet<String>,
    /// Whether `clipboard_read` may return the clipboard's text. Separate from
    /// input and off by default: the clipboard often holds secrets.
    pub allow_clipboard_read: bool,
    /// Whether global_key may press keys anchored on no element. Needs
    /// llow_input as well; off by default.
    pub allow_global_keys: bool,
    /// Directories whose files an `upload` may hand to a page. Empty refuses
    /// every upload; a client can neither extend nor see past them.
    pub upload_roots: Vec<std::path::PathBuf>,
    /// Whether browser_storage may show cookie values. Off: names and flags only.
    pub allow_cookie_values: bool,
}

#[derive(Clone)]
struct Observation {
    scope: Option<String>,
    max_nodes: usize,
    provider: String,
    resource_id: String,
    raw: Value,
    /// The captured nodes with session targets and negotiated capabilities.
    nodes: Vec<Value>,
    created: Instant,
}

#[derive(Default)]
struct State {
    resources: BTreeMap<String, BackendResource>,
    observations: BTreeMap<String, Observation>,
    requests: BTreeSet<String>,
    cancelled_requests: BTreeSet<String>,
}

pub struct Context {
    effect_gate: Mutex<()>,
    // 0=pending, 1=may have committed, 2=cancelled before, 3=cancelled after.
    phase: AtomicU8,
    browser_publication: AtomicU8,
    browser_cancelled: Arc<AtomicBool>,
    deadline: Instant,
    mutating: bool,
}

// Backend screenshot APIs may only accept a duration. Bound their worker
// count while allowing the caller to stop waiting immediately on cancellation.
// The backend retains its original finite deadline and its output is discarded
// if the caller has already terminated.
fn capture_cancellable(
    context: &Context,
    capture: impl FnOnce() -> Result<Value> + Send + 'static,
) -> Result<Value> {
    static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if ACTIVE
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < 8).then_some(n + 1)
        })
        .is_err()
    {
        return Err(error("CONTROL.BUSY", "screenshot workers are occupied"));
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        struct Permit;
        impl Drop for Permit {
            fn drop(&mut self) {
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _permit = Permit;
        let _ = tx.send(capture());
    });
    loop {
        let remaining = context.remaining()?;
        match rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
            Ok(result) => {
                context.remaining()?;
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => {
                context.remaining()?;
                return Err(error("CONTROL.CAPTURE_FAILED", "screenshot worker stopped"));
            }
        }
    }
}

impl Context {
    pub fn new(timeout: Duration, mutating: bool) -> Self {
        Self {
            effect_gate: Mutex::new(()),
            browser_publication: AtomicU8::new(0),
            browser_cancelled: Arc::new(AtomicBool::new(false)),
            phase: AtomicU8::new(0),
            deadline: Instant::now() + timeout,
            mutating,
        }
    }
    /// Nonblocking browser-open cancellation. Publication and cancellation
    /// compete for this single terminal transition, independently of slow effects.
    pub fn browser_cancellation(&self) -> Arc<AtomicBool> {
        self.browser_cancelled.clone()
    }
    pub fn cancel_browser(&self) {
        if self
            .browser_publication
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| match state {
                0 => Some(1),
                3 => Some(4),
                _ => None,
            })
            .is_ok()
        {
            self.browser_cancelled.store(true, Ordering::SeqCst);
        }
    }
    pub fn begin_browser_navigation(&self) -> Result<()> {
        self.remaining()?;
        self.browser_publication
            .compare_exchange(0, 3, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| error("CONTROL.CANCELLED", "browser navigation cancelled"))?;
        Ok(())
    }

    pub fn cancel(&self) {
        let _effect = self.effect_gate.lock().unwrap();
        let mut phase = self.phase.load(Ordering::SeqCst);
        loop {
            let next = match phase {
                0 => 2,
                1 => 3,
                _ => return,
            };
            match self
                .phase
                .compare_exchange_weak(phase, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return,
                Err(actual) => phase = actual,
            }
        }
    }
    /// Atomically cross the may-commit boundary immediately before input. A
    /// cancellation that wins first prevents dispatch; later interruption is unknown.
    pub fn commit(&self) -> Result<Duration> {
        self.remaining()?;
        let _ = self
            .phase
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        self.remaining()
    }
    pub fn remaining(&self) -> Result<Duration> {
        let browser_phase = self.browser_publication.load(Ordering::SeqCst);
        if matches!(browser_phase, 1 | 4) {
            return Err(
                error("CONTROL.CANCELLED", "browser request cancelled").with_effect(
                    if browser_phase == 4 {
                        "unknown"
                    } else {
                        "not_applied"
                    },
                ),
            );
        }
        let phase = self.phase.load(Ordering::SeqCst);
        let code = if matches!(phase, 2 | 3) {
            Some("CONTROL.CANCELLED")
        } else if Instant::now() >= self.deadline {
            Some("CONTROL.TIMEOUT")
        } else {
            None
        };
        if let Some(code) = code {
            let effect =
                if matches!(browser_phase, 3 | 4) || self.mutating && matches!(phase, 1 | 3) {
                    "unknown"
                } else {
                    "not_applied"
                };
            return Err(error(code, "request cancelled or deadline elapsed").with_effect(effect));
        }
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }
}

pub struct Session {
    id: String,
    policy: Policy,
    backends: BTreeMap<String, Arc<dyn Backend>>,
    failures: BTreeMap<String, Value>,
    state: Mutex<State>,
    /// Native focus/input is serialized, even when cancellation is concurrent.
    gate: Arc<Mutex<()>>,
    browser_contexts: Mutex<Vec<std::sync::Weak<Context>>>,
    active: Mutex<BTreeMap<String, Arc<Context>>>,
    closed: AtomicBool,
    browser_publication: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    created: Instant,
    lifetime: Duration,
    observation_ttl: Duration,
    max_requests: usize,
    leases: Mutex<BTreeMap<String, Arc<lease::InputLease>>>,
    /// The native providers behind this session, for saved workflows.
    providers: Vec<Arc<dyn aad_runtime::Provider>>,
    /// The browser provider, which can be pointed at a browser later.
    browser: Option<Arc<browser::BrowserBackend>>,
}

impl Session {
    pub fn new(backends: Vec<Arc<dyn Backend>>, policy: Policy) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            policy,
            backends: backends
                .into_iter()
                .map(|b| (b.name().to_string(), b))
                .collect(),
            failures: BTreeMap::new(),
            state: Mutex::new(State::default()),
            gate: Arc::new(Mutex::new(())),
            browser_contexts: Mutex::new(Vec::new()),
            active: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
            browser_publication: Mutex::new(Vec::new()),
            created: Instant::now(),
            lifetime: Duration::from_secs(3600),
            observation_ttl: Duration::from_secs(300),
            max_requests: 10_000,
            leases: Mutex::new(BTreeMap::new()),
            providers: Vec::new(),
            browser: None,
        }
    }

    pub fn register_browser_context(&self, context: &Arc<Context>) {
        let mut contexts = self.browser_contexts.lock().unwrap();
        contexts.retain(|c| c.strong_count() > 0);
        if self.closed.load(Ordering::SeqCst) {
            context.cancel_browser();
        }
        contexts.push(Arc::downgrade(context));
    }
    pub fn browser_navigation(&self, context: &Context) -> Result<()> {
        let _contexts = self.browser_contexts.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            context.cancel_browser();
        }
        context.begin_browser_navigation()
    }

    pub fn browser_request<T>(&self, id: &str, run: impl FnOnce(&Context) -> T) -> Result<T> {
        let context = Arc::new(Context::new(Duration::from_secs(60), false));
        self.register_browser_context(&context);
        {
            let _publication = self.browser_publication.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) {
                return Err(error("CONTROL.SESSION_CLOSED", "session closed"));
            }
            let mut active = self.active.lock().unwrap();
            if self.state.lock().unwrap().cancelled_requests.remove(id) {
                context.cancel();
            }
            active.insert(id.into(), context.clone());
        }
        let result = run(&context);
        self.active.lock().unwrap().remove(id);
        Ok(result)
    }
    pub fn browser_effect<T>(&self, context: &Context, effect: impl FnOnce() -> T) -> Result<T> {
        let _publication = self.browser_publication.lock().unwrap();
        let _effect = context.effect_gate.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(error("CONTROL.SESSION_CLOSED", "session closed"));
        }
        context.remaining()?;
        Ok(effect())
    }

    pub fn register_browser_resource<T>(
        &self,
        context: &Context,
        create: impl FnOnce() -> Result<(T, Box<dyn FnOnce() + Send>)>,
    ) -> Result<T> {
        let mut resources = self.browser_publication.lock().unwrap();
        let _effect = context.effect_gate.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(error("CONTROL.SESSION_CLOSED", "session closed"));
        }
        context.remaining()?;
        let (value, cleanup) = create()?;
        resources.push(cleanup);
        Ok(value)
    }

    pub fn native(policy: Policy) -> Self {
        Self::native_with_browser(policy, None)
    }

    pub fn native_with_browser(policy: Policy, browser_port: Option<u16>) -> Self {
        let (natives, failures) = native::backends();
        let providers = natives.iter().map(|b| b.provider()).collect();
        let backends = natives.into_iter().map(|b| b as Arc<dyn Backend>).collect();
        let mut session = Self::new(backends, policy).with_providers(providers);
        // Always present, so a browser started later can be attached.
        let browser = Arc::new(browser::BrowserBackend::new(browser_port.unwrap_or(0)));
        session
            .backends
            .insert(browser::PROVIDER.into(), browser.clone());
        session.browser = Some(browser);
        session.failures = failures;
        session
    }

    /// Native providers that saved workflows may drive. They must be the
    /// instances behind this session's backends, so a workflow shares their
    /// snapshot stores rather than opening a second driver.
    /// Point the browser provider at the CDP endpoint on this loopback port.
    /// Earlier browser observations stop resolving.
    pub fn attach_browser(&self, port: u16) -> Result<()> {
        let _gate = self.gate.lock().unwrap();
        let _publication = self.browser_publication.lock().unwrap();
        self.attach_browser_locked(port, false)
    }

    fn attach_browser_locked(&self, port: u16, launched: bool) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(error(
                "CONTROL.SESSION_CLOSED",
                "cannot attach a browser to a closed session",
            ));
        }
        let browser = self.browser.as_ref().ok_or_else(|| {
            error(
                "BROWSER.NOT_CONNECTED",
                "this session has no browser provider",
            )
        })?;
        if launched {
            browser.attach_launched(port);
        } else {
            browser.attach(port);
        }
        if self.closed.load(Ordering::SeqCst) {
            browser.attach(0);
            return Err(error(
                "CONTROL.SESSION_CLOSED",
                "session closed during attachment",
            ));
        }
        let mut state = self.state.lock().unwrap();
        state
            .observations
            .retain(|_, old| old.provider != browser::PROVIDER);
        state
            .resources
            .retain(|_, old| old.resource.provider != browser::PROVIDER);
        Ok(())
    }

    /// Forget the browser connection and every resource and observation it owned.
    /// This does not close an externally owned browser process.
    pub fn detach_browser(&self) {
        let _gate = self.gate.lock().unwrap();
        if let Some(browser) = &self.browser {
            browser.attach(0);
        }
        let mut state = self.state.lock().unwrap();
        state
            .observations
            .retain(|_, old| old.provider != browser::PROVIDER);
        state
            .resources
            .retain(|_, old| old.resource.provider != browser::PROVIDER);
    }

    /// Point the browser provider at a browser this server launched on `port`:
    /// unlike [`Session::attach_browser`], its downloads are redirected to the
    /// session's directory and tracked.
    pub fn attach_launched_browser(&self, port: u16) -> Result<()> {
        let _gate = self.gate.lock().unwrap();
        let _publication = self.browser_publication.lock().unwrap();
        self.attach_browser_locked(port, true)
    }

    /// Publish endpoint and ownership together with their close cleanup.
    /// Preparation must finish before this short critical section.
    pub fn publish_launched_browser(
        &self,
        port: u16,
        publish: impl FnOnce() -> Box<dyn FnOnce() + Send>,
    ) -> Result<()> {
        self.publish_browser_with_context(port, None, publish)
    }

    pub fn publish_browser_with_context(
        &self,
        port: u16,
        context: Option<&Context>,
        publish: impl FnOnce() -> Box<dyn FnOnce() + Send>,
    ) -> Result<()> {
        let _gate = self.gate.lock().unwrap();
        let mut publication = self.browser_publication.lock().unwrap();
        let _effect = context.map(|c| c.effect_gate.lock().unwrap());
        if let Some(context) = context {
            context.remaining()?;
            context
                .browser_publication
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                    matches!(state, 0 | 3).then_some(2)
                })
                .map_err(|_| error("CONTROL.CANCELLED", "browser publication cancelled"))?;
        }
        self.attach_browser_locked(port, true)?;
        publication.push(publish());
        Ok(())
    }

    /// Give a launched browser its own download setting back, e.g. before closing it.
    pub fn release_browser_downloads(&self) {
        let _gate = self.gate.lock().unwrap();
        if let Some(browser) = &self.browser {
            browser.release_downloads();
        }
    }

    /// What the attached browser downloaded into this session's directory. Read-only.
    pub fn browser_downloads(&self, timeout: Duration) -> Result<Value> {
        let _gate = self.gate.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(error("CONTROL.SESSION_CLOSED", "session is closed"));
        }
        let browser = self.browser.as_ref().ok_or_else(|| {
            error(
                "BROWSER.NOT_CONNECTED",
                "this session has no browser provider",
            )
        })?;
        browser.downloads(timeout)
    }

    /// A listed resource's provider and selector, under this session's grants.
    fn readable(&self, resource_id: &str) -> Result<(Arc<dyn Backend>, Value)> {
        if self.closed.load(Ordering::SeqCst) || self.created.elapsed() >= self.lifetime {
            return Err(error(
                "CONTROL.SESSION_CLOSED",
                "session is closed or expired",
            ));
        }
        let item = self
            .state
            .lock()
            .unwrap()
            .resources
            .get(resource_id)
            .cloned()
            .ok_or_else(|| {
                error(
                    "CONTROL.TARGET_UNKNOWN",
                    "list targets before reading a resource",
                )
            })?;
        if !self.allowed_provider(&item.resource.provider) || !self.allowed_resource(resource_id) {
            return Err(error(
                "POLICY.DENIED",
                "the resource is outside this session's grant",
            ));
        }
        Ok((
            self.backends[&item.resource.provider].clone(),
            item.selector,
        ))
    }

    /// Cookies (values only under the operator's grant) and localStorage keys
    /// of a listed browser page or frame. Read-only.
    pub fn browser_storage(&self, resource_id: &str, timeout: Duration) -> Result<Value> {
        let (backend, selector) = self.readable(resource_id)?;
        let _gate = self.gate.lock().unwrap();
        backend.storage(&selector, timeout, self.policy.allow_cookie_values)
    }

    /// The recent requests of a listed browser page. Read-only.
    pub fn browser_network(
        &self,
        resource_id: &str,
        limit: usize,
        bodies: bool,
        timeout: Duration,
    ) -> Result<Value> {
        if limit == 0 || limit > 500 {
            return Err(error("CONTROL.INVALID_REQUEST", "limit must be in 1..500"));
        }
        let (backend, selector) = self.readable(resource_id)?;
        let _gate = self.gate.lock().unwrap();
        backend.network(&selector, limit, bodies, timeout)
    }
    /// The CDP port the browser provider works with, if one is attached.
    pub fn browser_port(&self) -> Option<u16> {
        self.browser.as_ref().and_then(|b| b.attached_port())
    }

    pub fn with_providers(mut self, providers: Vec<Arc<dyn aad_runtime::Provider>>) -> Self {
        self.providers = providers
            .into_iter()
            .map(native::workflow_provider)
            .collect();
        self
    }
    pub fn providers(&self) -> &[Arc<dyn aad_runtime::Provider>] {
        &self.providers
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    /// Providers that could not start, keyed by kind, with their structured errors.
    pub fn unavailable(&self) -> &BTreeMap<String, Value> {
        &self.failures
    }
    /// Report providers that a host tried and failed to start.
    pub fn with_unavailable(mut self, failures: BTreeMap<String, Value>) -> Self {
        self.failures = failures;
        self
    }
    /// The serialisation gate shared by everything that drives this desktop.
    pub fn gate(&self) -> Arc<Mutex<()>> {
        self.gate.clone()
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Share serialization with a compatibility facade over the same desktop.
    pub fn with_gate(mut self, gate: Arc<Mutex<()>>) -> Self {
        self.gate = gate;
        self
    }
    /// Trusted hosts may shorten, but never silently extend, the authority and
    /// replay-guard budgets. Clients cannot modify these limits over the wire.
    pub fn with_limits(
        mut self,
        lifetime: Duration,
        observation_ttl: Duration,
        max_requests: usize,
    ) -> Result<Self> {
        if lifetime.is_zero()
            || lifetime > Duration::from_secs(3600)
            || observation_ttl.is_zero()
            || observation_ttl > Duration::from_secs(300)
            || max_requests == 0
            || max_requests > 10_000
        {
            return Err(error(
                "CONTROL.INVALID_LIMITS",
                "session limits must be positive and no greater than the built-in ceilings",
            ));
        }
        self.lifetime = lifetime;
        self.observation_ttl = observation_ttl;
        self.max_requests = max_requests;
        Ok(self)
    }

    pub fn cancel(&self, id: &str) -> bool {
        if id.is_empty() || id.len() > 128 {
            return false;
        }
        let active = self.active.lock().unwrap();
        if let Some(context) = active.get(id) {
            context.cancel();
            true
        } else {
            let mut state = self.state.lock().unwrap();
            if state.requests.contains(id) || state.cancelled_requests.len() >= self.max_requests {
                false
            } else {
                state.cancelled_requests.insert(id.into());
                true
            }
        }
    }

    pub fn close(&self) {
        {
            let contexts = self.browser_contexts.lock().unwrap();
            self.closed.store(true, Ordering::SeqCst);
            for context in contexts.iter().filter_map(std::sync::Weak::upgrade) {
                context.cancel_browser();
            }
        }
        for context in self.active.lock().unwrap().values() {
            context.cancel();
        }
        let mut publication = self.browser_publication.lock().unwrap();
        for cleanup in publication.drain(..) {
            cleanup();
        }
        if let Some(browser) = &self.browser {
            browser.close();
        }
        // Do not wait on the backend gate: a close must cancel an in-flight request.
        self.state.lock().unwrap().observations.clear();
        self.leases.lock().unwrap().clear();
        for backend in self.backends.values() {
            backend.close();
        }
    }

    pub fn input_lease(&self, key: &str) -> Result<Arc<lease::InputLease>> {
        Ok(self.input_leases(&[key.to_string()])?.pop().unwrap())
    }

    /// Acquire a sorted set atomically with respect to this session. Failed
    /// acquisition drops only this call's new leases; existing ones survive.
    pub fn input_leases(&self, keys: &[String]) -> Result<Vec<Arc<lease::InputLease>>> {
        if self.closed.load(Ordering::SeqCst) || self.created.elapsed() >= self.lifetime {
            return Err(error(
                "CONTROL.SESSION_CLOSED",
                "session is closed or expired",
            ));
        }
        let mut leases = self.leases.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(error("CONTROL.SESSION_CLOSED", "session is closed"));
        }
        let ordered: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
        let mut acquired = Vec::new();
        let mut result = Vec::new();
        for key in ordered {
            if let Some(lease) = leases.get(key) {
                result.push(lease.clone());
            } else {
                let lease = Arc::new(lease::InputLease::acquire_for(key, &self.id)?);
                acquired.push((key.to_string(), lease.clone()));
                result.push(lease);
            }
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(error("CONTROL.SESSION_CLOSED", "session is closed"));
        }
        leases.extend(acquired);
        Ok(result)
    }

    pub fn execute(&self, request: &Request) -> Response {
        let result = self.run(request);
        let (data, failure, effect) = match result {
            Ok(value) => (
                Some(value),
                None,
                if matches!(request.operation, Operation::GlobalKey { .. }) {
                    // Delivered to whatever held focus: nothing anchors or
                    // verifies it, so the commit state stays unknown.
                    "dispatched".to_string()
                } else if request.mutating() {
                    "applied".to_string()
                } else {
                    "not_applied".to_string()
                },
            ),
            Err(e) => (None, Some(e.to_json()), e.effect),
        };
        Response {
            protocol_version: PROTOCOL_VERSION.into(),
            request_id: request.request_id.clone(),
            session_id: self.id.clone(),
            commit_state: CommitState::from_effect(&effect),
            effect,
            data,
            error: failure,
        }
    }

    fn run(&self, request: &Request) -> Result<Value> {
        if request.request_id.is_empty()
            || request.request_id.len() > 128
            || request.timeout_ms == 0
            || request.timeout_ms > MAX_TIMEOUT_MS
        {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "bounded request id and timeout_ms (1..30000) required",
            ));
        }
        if !matches!(request.operation, Operation::Capabilities {})
            && request.session_id.as_deref() != Some(self.id())
        {
            return Err(error(
                "CONTROL.SESSION_MISMATCH",
                "request belongs to another session",
            ));
        }
        if self.closed.load(Ordering::SeqCst) || self.created.elapsed() >= self.lifetime {
            self.close();
            return Err(error(
                "CONTROL.SESSION_CLOSED",
                "session is closed or expired",
            ));
        }
        // Register the replay guard and cancellation context under the same
        // admission lock. Otherwise cancel could see a seen ID with no active
        // context and incorrectly treat an admitted request as already finished.
        let mut active = self.active.lock().unwrap();
        {
            let mut state = self.state.lock().unwrap();
            if state.requests.len() >= self.max_requests {
                drop(state);
                drop(active);
                self.close();
                return Err(error(
                    "CONTROL.SESSION_LIMIT",
                    "session request budget exhausted; open a fresh session",
                ));
            }
            if !state.requests.insert(request.request_id.clone()) {
                return Err(error(
                    "CONTROL.DUPLICATE_REQUEST",
                    "request IDs cannot be replayed",
                ));
            }
        }
        match &request.operation {
            Operation::Cancel { request_id } => {
                drop(active);
                return Ok(json!({"cancellation_requested": self.cancel(request_id)}));
            }
            Operation::Close {} => {
                if self
                    .state
                    .lock()
                    .unwrap()
                    .cancelled_requests
                    .remove(&request.request_id)
                {
                    return Err(error(
                        "CONTROL.CANCELLED",
                        "close cancelled before admission",
                    ));
                }
                drop(active);
                self.close();
                return Ok(json!({"closed": true}));
            }
            _ => {}
        }
        let cancelled = self
            .state
            .lock()
            .unwrap()
            .cancelled_requests
            .remove(&request.request_id);
        let context = Arc::new(Context {
            effect_gate: Mutex::new(()),
            browser_publication: AtomicU8::new(0),
            browser_cancelled: Arc::new(AtomicBool::new(false)),
            phase: AtomicU8::new(if cancelled { 2 } else { 0 }),
            deadline: Instant::now() + Duration::from_millis(request.timeout_ms),
            mutating: request.mutating(),
        });
        active.insert(request.request_id.clone(), context.clone());
        drop(active);
        let result = {
            let _gate = self.gate.lock().unwrap();
            context.remaining().and_then(|_| {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(error("CONTROL.SESSION_CLOSED", "session is closed"));
                }
                self.dispatch(&request.operation, &context)
            })
        };
        self.active.lock().unwrap().remove(&request.request_id);
        result
    }

    fn allowed_provider(&self, name: &str) -> bool {
        self.policy.providers.is_empty() || self.policy.providers.contains(name)
    }
    fn allowed_resource(&self, id: &str) -> bool {
        self.policy
            .resources
            .as_ref()
            .is_none_or(|ids| ids.contains(id))
    }

    fn dispatch(&self, operation: &Operation, context: &Context) -> Result<Value> {
        match operation {
            Operation::BrowserNetwork { resource_id, limit } => {
                if *limit == 0 || *limit > 500 {
                    return Err(error("CONTROL.INVALID_REQUEST", "limit must be in 1..500"));
                }
                let (backend, selector) = self.readable(resource_id)?;
                backend.network(&selector, *limit, false, context.remaining()?)
            }
            Operation::WatchQr { resource_id, locator, expired, confirmed, previous } =>
                self.watch_qr(resource_id, locator, expired.as_deref(), confirmed.as_deref(), previous.as_deref(), context),
            Operation::Capabilities {} => Ok(self.with_grants(
                json!({"session_id": self.id, "protocol_version": PROTOCOL_VERSION,
                "expires_in_ms": self.lifetime.saturating_sub(self.created.elapsed()).as_millis(),
                "session_usable": !self.is_closed() && self.created.elapsed() < self.lifetime
                    && self.state.lock().unwrap().requests.len() < self.max_requests,
                "allow_input": self.policy.allow_input,"allow_secret_input":self.policy.allow_secret_input,"secret_bindings":if self.policy.allow_secret_input { self.policy.secret_bindings.names() } else { Vec::new() },"upload_dirs": self.policy.upload_roots,"cookie_values": self.policy.allow_cookie_values,"observation_retention":"until_next_observation_or_input_attempt","observation_ttl_ms":self.observation_ttl.as_millis(),"max_requests":self.max_requests,"input_lease":"exclusive_cooperating_session","input_lease_scope":"os_user","providers": self.backends.values().filter(|b| self.allowed_provider(b.name()))
                    .map(|b| json!({"provider": b.name(), "actions": b.capabilities(), "reads": b.reads()})).collect::<Vec<_>>(),
                "unavailable": self.failures}),
            )),
            Operation::Targets {} => {
                let mut resources = BTreeMap::new();
                let mut unavailable = self.failures.clone();
                for backend in self
                    .backends
                    .values()
                    .filter(|b| self.allowed_provider(b.name()))
                {
                    match backend.resources(context.remaining()?) {
                        Ok(items) => {
                            for item in items {
                                if self.allowed_resource(&item.resource.resource_id) {
                                    resources.insert(item.resource.resource_id.clone(), item);
                                }
                            }
                        }
                        Err(e) => {
                            unavailable.insert(backend.name().into(), e.to_json());
                        }
                    }
                }
                context.remaining()?;
                let public = resources
                    .values()
                    .map(|item| item.resource.clone())
                    .collect::<Vec<_>>();
                self.state.lock().unwrap().resources = resources;
                Ok(json!({"resources": public, "unavailable": unavailable}))
            }
            Operation::Observe {
                resource_id,
                max_nodes,
                view,
            } => {
                if *max_nodes == 0 || *max_nodes > MAX_NODES {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "max_nodes must be in 1..2000",
                    ));
                }
                validate_view(view)?;
                let (observation_id, observation) =
                    self.observe_resource_scoped(resource_id, *max_nodes, view.scope_id.as_deref(), view.scope_target.as_ref(), context)?;
                let backend = &self.backends[&observation.provider];
                let (raw, nodes) = (observation.raw, observation.nodes);
                let mut answer = json!({"observation_id": observation_id, "resource_id": resource_id, "node_count": nodes.len(), "truncated": raw["truncated"] == true, "metadata": raw.get("metadata").cloned().unwrap_or(Value::Null)});
                if let Some(reasons) = raw.get("truncation_reasons") {
                    answer["truncation_reasons"] = reasons.clone();
                }
                if raw["scope_id"].is_string() {
                    answer["scope"] = json!({"id":raw["scope_id"], "revision":raw["scope_revision"], "complete":raw["truncated"] != true});
                }
                match view.mode {
                    ViewMode::Nodes => answer["nodes"] = json!(nodes),
                    ViewMode::Outline | ViewMode::Overview | ViewMode::Text => {
                        let shaped = match backend.view(&raw, view, context.remaining()?)? {
                            Some(shaped) => shaped,
                            None => portable_view(&nodes, view)?,
                        };
                        context.remaining()?;
                        let Value::Object(shaped) = shaped else {
                            return Err(error(
                                "CONTROL.BAD_PROVIDER_OUTPUT",
                                "view was not an object",
                            ));
                        };
                        for (key, value) in shaped {
                            answer[key] = value;
                        }
                        // Provider views name nodes; targets and negotiated
                        // capabilities always come from the retained capture.
                        if let Some(shown) = answer["nodes"].as_array_mut() {
                            for item in shown {
                                let full = item["node_id"]
                                    .as_str()
                                    .and_then(|id| nodes.iter().find(|n| n["node_id"] == id));
                                let Some(full) = full else {
                                    return Err(error(
                                        "CONTROL.BAD_PROVIDER_OUTPUT",
                                        "view named a node outside the observation",
                                    ));
                                };
                                item["capabilities"] = full["capabilities"].clone();
                                item["target"] = full["target"].clone();
                            }
                        }
                    }
                }
                Ok(answer)
            }
            Operation::Find {
                observation_id,
                locator,
                expect,
            } => {
                if locator.is_empty() {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "find requires at least one condition",
                    ));
                }
                let observation = self.observation(observation_id)?;
                self.find_in(observation_id, &observation, locator, *expect, context)
            }
            Operation::Wait {
                resource_id,
                locator,
                condition,
                states,
                max_nodes,
                interval_ms,
            } => self.wait(
                resource_id,
                locator,
                *condition,
                states.as_ref(),
                *max_nodes,
                *interval_ms,
                None,
                context,
            ),
            Operation::Act { target, action, verify } => {
                if verify.as_ref().is_some_and(|locator| locator.is_empty()) {
                    return Err(error("CONTROL.INVALID_REQUEST", "verification requires a nonempty locator"));
                }
                if !self.policy.allow_input {
                    return Err(error(
                        "POLICY.DENIED",
                        "input permission was not granted by the server operator",
                    ));
                }
                if target.session_id != self.id {
                    return Err(error(
                        "CONTROL.SESSION_MISMATCH",
                        "target belongs to another session",
                    ));
                }
                let observation = self.observation(&target.observation_id)?;
                if target.resource_id != observation.resource_id
                    || !self.allowed_resource(&target.resource_id)
                {
                    return Err(error(
                        "POLICY.DENIED",
                        "target is outside the resource grant",
                    ));
                }
                if observation.raw["truncated"] == true {
                    return Err(error(
                        "CONTROL.OBSERVATION_TRUNCATED",
                        "truncated observation cannot authorize input",
                    ));
                }
                validate_action(action)?;
                // Upload paths are resolved here, under the server's grant, so
                // no provider ever sees a path outside it.
                let resolved;
                let action = match action {
                    Action::Upload { files } => {
                        resolved = Action::Upload {
                            files: self.upload_files(files)?,
                        };
                        &resolved
                    }
                    other => other,
                };
                let observed = |id: &str| {
                    observation.raw["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|n| n["node_id"] == id)
                        .ok_or_else(|| error("CONTROL.STALE_TARGET", "node was not observed"))
                };
                let node = observed(&target.node_id)?;
                let secret = if let Action::SecretInput { secret_ref } = action {
                    if !self.policy.allow_secret_input {
                        return Err(error("POLICY.DENIED", "secret input permission was not granted by the server operator"));
                    }
                    if node["states"]["protected"] != true {
                        return Err(error("CONTROL.ACTION_UNSUPPORTED", "secret input requires an observed protected field"));
                    }
                    Some(self.policy.secret_bindings.resolve(secret_ref).ok_or_else(|| {
                        error("CONTROL.SECRET_BINDING_MISSING", "the server has no such secret binding")
                    })?)
                } else {
                    if node["states"]["protected"] == true
                        && matches!(action, Action::Fill { .. } | Action::TypeText { .. } | Action::Paste { .. } | Action::Key { .. })
                    {
                        return Err(error("POLICY.DENIED", "protected fields require the dedicated secret input action"));
                    }
                    None
                };
                if let Action::Drag { to, .. } = action {
                    // Both ends must come from the same view of the same resource,
                    // so one live revalidation covers the whole gesture.
                    if to.session_id != target.session_id
                        || to.resource_id != target.resource_id
                        || to.observation_id != target.observation_id
                    {
                        return Err(error(
                            "CONTROL.INVALID_REQUEST",
                            "drag destination must belong to the same observation",
                        ));
                    }
                    // A surface can contain both ends (for example a canvas
                    // selection). Its fractions still undergo validation and
                    // the provider revalidates the observed native element.
                    observed(&to.node_id)?;
                }
                let backend = &self.backends[&observation.provider];
                if !backend
                    .capabilities()
                    .iter()
                    .any(|name| name == action.name())
                {
                    return Err(error(
                        "CONTROL.ACTION_UNSUPPORTED",
                        "provider does not implement this action",
                    ));
                }
                if !backend
                    .node_capabilities(node)
                    .iter()
                    .any(|name| name == action.name())
                {
                    return Err(error(
                        "CONTROL.ACTION_UNSUPPORTED",
                        "observed node does not expose this typed action",
                    ));
                }
                context.remaining()?;
                let _lease = self.input_leases(&backend.lease_keys(&observation.raw)?)?;
                context.remaining()?;
                let result = match secret {
                    Some(text) => backend
                        .secret_input(&observation.raw, node, text, context)
                        .map(|_| json!({"submitted": true, "redacted": true}))
                        .map_err(|failure| {
                            // Platform APIs may echo arguments even in errors.
                            // No free-form backend value leaves this boundary.
                            error("CONTROL.SECRET_INPUT_FAILED", "secret input failed; no secret values are included in this response")
                                .with_effect(if failure.effect == "not_applied" { "not_applied" } else { "unknown" })
                                .with_retryable(failure.retryable)
                        }),
                    None => backend.act(&observation.raw, node, action, context),
                };
                self.state
                    .lock()
                    .unwrap()
                    .observations
                    .retain(|_, old| old.provider != observation.provider);
                let value = result?;
                drop(_lease);
                if let Some(locator) = verify {
                    let verification = self.wait(&target.resource_id, locator, WaitCondition::Appears, None, observation.max_nodes, None, observation.scope.as_deref(), context);
                    return Ok(match verification {
                        Ok(evidence) => json!({"submitted": true, "verified": true, "verification": "verified", "evidence": evidence, "backend_result": value}),
                        Err(failure) => json!({"submitted": true, "verified": false, "verification": if matches!(failure.code.as_str(), "CONTROL.WAIT_TIMEOUT" | "CONTROL.TIMEOUT") { "timeout_unconfirmed" } else { "unconfirmed" }, "diagnostics": self.verification_diagnostics(&target.resource_id), "verification_error": {"code": failure.code, "message": failure.message, "details": failure.details}, "backend_result": value}),
                    });
                }
                context.remaining()?;
                Ok(
                    json!({"submitted": true, "provider": observation.provider, "operation": action.name(), "backend_result": value, "requires_fresh_observation": true}),
                )
            }
            Operation::Capture { target } => {
                let (observation, node) = self.observed_node(target)?;
                let backend = self.backends[&observation.provider].clone();
                let timeout = context.remaining()?;
                let raw = observation.raw.clone();
                let mut shot = capture_cancellable(context, move || backend.capture(&raw, &node, timeout))?;
                shot["digest"] = json!(digest_of(&shot));
                shot["provider"] = json!(observation.provider);
                Ok(shot)
            }
            Operation::ReadText { target, language } => {
                if language.as_ref().is_some_and(|tag| {
                    tag.is_empty()
                        || tag.len() > 35
                        || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                }) {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "language must be a BCP-47 tag such as en-US",
                    ));
                }
                let (observation, node) = self.observed_node(target)?;
                let backend = &self.backends[&observation.provider];
                let read = backend.read_text(
                    &observation.raw,
                    &node,
                    language.as_deref(),
                    context.remaining()?,
                )?;
                context.remaining()?;
                Ok(text_answer(&read, &observation.provider))
            }
            Operation::Launch {
                program,
                args,
                wait_ms,
            } => self.launch(program, args, *wait_ms, context),
            Operation::GlobalKey { keys } => {
                if !self.policy.allow_input || !self.policy.allow_global_keys {
                    return Err(error(
                        "POLICY.DENIED",
                        "global keys were not granted by the server operator",
                    ));
                }
                validate_action(&Action::Key { keys: keys.clone() })?;
                let backend = self
                    .backends
                    .values()
                    .find(|b| {
                        b.name() != browser::PROVIDER
                            && self.allowed_provider(b.name())
                            && b.offers_global_keys()
                    })
                    .cloned()
                    .ok_or_else(|| {
                        error(
                            "CONTROL.ACTION_UNSUPPORTED",
                            "no provider in this session can press global keys",
                        )
                    })?;
                let _lease = self.input_lease("desktop-input")?;
                context.remaining()?;
                let sent = backend.global_key(keys, context)?;
                // Whatever was observed may have changed under the keys.
                self.state.lock().unwrap().observations.clear();
                Ok(json!({"submitted": true, "provider": backend.name(), "operation": "global_key", "backend_result": sent, "verified": false, "requires_fresh_observation": true}))
            }
            Operation::ClipboardRead { max_characters } => {
                if !self.policy.allow_clipboard_read {
                    return Err(error(
                        "POLICY.DENIED",
                        "reading the clipboard was not granted by the server operator",
                    ));
                }
                if max_characters.is_some_and(|max| max == 0 || max > 65_536) {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "max_characters must be in 1..65536",
                    ));
                }
                let mut last = error(
                    "CONTROL.ACTION_UNSUPPORTED",
                    "no provider in this session can read the clipboard",
                );
                for backend in self
                    .backends
                    .values()
                    .filter(|b| b.name() != browser::PROVIDER && self.allowed_provider(b.name()))
                {
                    match backend.clipboard_read(*max_characters, context.remaining()?) {
                        Ok(mut read) => {
                            read["provider"] = json!(backend.name());
                            return Ok(read);
                        }
                        Err(e) if e.code == "CONTROL.ACTION_UNSUPPORTED" => last = e,
                        Err(e) => return Err(e),
                    }
                }
                Err(last)
            }
            Operation::Cancel { .. } | Operation::Close {} => unreachable!(),
        }
    }

    /// Add the operator's extra grants to a capabilities answer. Each appears
    /// only when granted, so a client never learns of a refused ability by
    /// seeing it listed.
    fn with_grants(&self, mut answer: Value) -> Value {
        if self.policy.allow_input && !self.policy.launch.is_empty() {
            answer["launch"] = json!(self.policy.launch);
        }
        if self.policy.allow_clipboard_read {
            answer["clipboard_read"] = json!(true);
        }
        if self.policy.allow_input && self.policy.allow_global_keys {
            answer["global_keys"] = json!(true);
        }
        answer
    }

    /// Start an allowlisted program and answer the windows it opens.
    fn launch(
        &self,
        program: &str,
        args: &[String],
        wait_ms: Option<u64>,
        context: &Context,
    ) -> Result<Value> {
        if !self.policy.allow_input || self.policy.launch.is_empty() {
            return Err(error(
                "POLICY.DENIED",
                "launching programs was not granted by the server operator",
            ));
        }
        launch::validate_args(program, args)?;
        if !launch::allowed(&self.policy.launch, program) {
            return Err(error(
                "POLICY.DENIED",
                "that program is not on the server's launch allowlist",
            )
            .with_detail("allowlist", json!(self.policy.launch)));
        }
        let path = launch::resolve(program)?;
        let image = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let desktops: Vec<Arc<dyn Backend>> = self
            .backends
            .values()
            .filter(|b| b.name() != browser::PROVIDER && self.allowed_provider(b.name()))
            .cloned()
            .collect();
        let listed = |timeout: Duration| -> Vec<BackendResource> {
            desktops
                .iter()
                .filter_map(|backend| backend.resources(timeout).ok())
                .flatten()
                .collect()
        };
        let before: BTreeSet<String> = listed(context.remaining()?)
            .into_iter()
            .map(|item| item.resource.resource_id)
            .collect();
        let _lease = self.input_lease("desktop-input")?;
        let remaining = context.commit()?;
        let process_id = launch::spawn(&path, args)?;
        // Stop waiting early enough to answer inside the request's deadline:
        // the process has started, so running out of time would turn a
        // committed launch into an unknown one.
        let wait = Duration::from_millis(wait_ms.unwrap_or(10_000).min(MAX_TIMEOUT_MS))
            .min(remaining.saturating_sub(Duration::from_millis(500)));
        let until = Instant::now() + wait;
        let mut found: Vec<BackendResource>;
        loop {
            let now = listed(
                until
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(200)),
            );
            let by_process: Vec<BackendResource> = now
                .iter()
                .filter(|item| item.resource.metadata["process_id"] == json!(process_id))
                .cloned()
                .collect();
            // A launcher stub may hand over to another process of the same
            // program (and exit): new windows of that image count too.
            found = if by_process.is_empty() {
                now.into_iter()
                    .filter(|item| {
                        !before.contains(&item.resource.resource_id)
                            && item.resource.metadata["process_name"]
                                .as_str()
                                .is_some_and(|name| name.eq_ignore_ascii_case(&image))
                    })
                    .collect()
            } else {
                by_process
            };
            if !found.is_empty() || Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut state = self.state.lock().unwrap();
        let resources: Vec<Resource> = found
            .into_iter()
            .filter(|item| self.allowed_resource(&item.resource.resource_id))
            .map(|item| {
                let public = item.resource.clone();
                state.resources.insert(public.resource_id.clone(), item);
                public
            })
            .collect();
        drop(state);
        Ok(json!({
            "process_id": process_id,
            "program": path.display().to_string(),
            "window_appeared": !resources.is_empty(),
            "resources": resources,
        }))
    }

    /// The retained observation and raw node a read-only target names.
    fn observed_node(&self, target: &Target) -> Result<(Observation, Value)> {
        if target.session_id != self.id {
            return Err(error(
                "CONTROL.SESSION_MISMATCH",
                "target belongs to another session",
            ));
        }
        let observation = self.observation(&target.observation_id)?;
        if target.resource_id != observation.resource_id
            || !self.allowed_resource(&target.resource_id)
        {
            return Err(error(
                "POLICY.DENIED",
                "target is outside the resource grant",
            ));
        }
        let node = observation.raw["nodes"]
            .as_array()
            .and_then(|nodes| {
                nodes
                    .iter()
                    .find(|n| n["node_id"] == target.node_id.as_str())
            })
            .cloned()
            .ok_or_else(|| error("CONTROL.STALE_TARGET", "node was not observed"))?;
        Ok((observation, node))
    }

    fn verification_diagnostics(&self, resource: &str) -> Value {
        let page = {
            let state = self.state.lock().unwrap();
            state.observations.values().find(|o| o.resource_id == resource).map(|o| {
                let nodes: Vec<_> = o.nodes.iter().take(20).map(|n| json!({
                    "role": n["role"].as_str().unwrap_or_default().chars().take(64).collect::<String>(),
                    "name": n["name"].as_str().unwrap_or_default().chars().take(128).collect::<String>()
                })).collect();
                json!({"scope_id":o.scope,"nodes":nodes,"omitted":o.nodes.len().saturating_sub(20),"truncated":o.raw["truncated"]})
            })
        };
        let network = self.readable(resource).and_then(|(backend, selector)| {
            backend.network(&selector, 20, false, Duration::from_millis(200))
        });
        let network = match network {
            Ok(log) => {
                json!({"summary":log["summary"],"since":log["since"],"dropped":log["dropped"],"omitted_by_limit":log["omitted_by_limit"]})
            }
            Err(e) => json!({"unavailable":e.code}),
        };
        json!({"page":page,"network":network})
    }

    /// Capture a listed resource and retain it as this provider's only
    /// current observation, with session targets on every node.
    fn observe_resource(
        &self,
        resource_id: &str,
        max_nodes: usize,
        context: &Context,
    ) -> Result<(String, Observation)> {
        self.observe_resource_scoped(resource_id, max_nodes, None, None, context)
    }

    fn observe_resource_scoped(
        &self,
        resource_id: &str,
        max_nodes: usize,
        scope: Option<&str>,
        scope_target: Option<&Target>,
        context: &Context,
    ) -> Result<(String, Observation)> {
        if scope.is_some_and(|id| id.is_empty() || id.len() > 256) {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "scope_id must contain 1..256 bytes",
            ));
        }
        if max_nodes == 0 || max_nodes > MAX_NODES {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "max_nodes must be in 1..2000",
            ));
        }
        let item = self
            .state
            .lock()
            .unwrap()
            .resources
            .get(resource_id)
            .cloned()
            .ok_or_else(|| {
                error(
                    "CONTROL.TARGET_UNKNOWN",
                    "list targets before observing a resource",
                )
            })?;
        let backend = &self.backends[&item.resource.provider];
        // Read the anchor before expiring this provider's old observations.
        // A truncated capture cannot authorize input, but an actually observed
        // node can anchor a fresh, complete subtree capture.
        let anchored = scope_target
            .map(|target| {
                if target.session_id != self.id {
                    return Err(error(
                        "CONTROL.SESSION_MISMATCH",
                        "scope target belongs to another session",
                    ));
                }
                let observed = self.observation(&target.observation_id)?;
                if target.resource_id != resource_id
                    || observed.resource_id != resource_id
                    || observed.provider != item.resource.provider
                    || !self.allowed_resource(resource_id)
                {
                    return Err(error(
                        "POLICY.DENIED",
                        "scope target is outside the resource grant",
                    ));
                }
                let node = observed.raw["nodes"]
                    .as_array()
                    .and_then(|nodes| nodes.iter().find(|node| node["node_id"] == target.node_id))
                    .cloned()
                    .ok_or_else(|| error("CONTROL.STALE_TARGET", "scope node was not observed"))?;
                if node["provenance"]["scope_context"] == true {
                    return Err(error(
                        "CONTROL.STALE_TARGET",
                        "ancestor context is outside the observed scope",
                    ));
                }
                Ok((observed.raw, node))
            })
            .transpose()?;
        self.state
            .lock()
            .unwrap()
            .observations
            .retain(|_, old| old.provider != item.resource.provider);
        let raw = match (scope, anchored) {
            (_, Some((snapshot, node))) => {
                backend.observe_target_scope(&snapshot, &node, max_nodes, context.remaining()?)?
            }
            (Some(scope), None) => {
                backend.observe_scope(&item.selector, scope, max_nodes, context.remaining()?)?
            }
            (None, None) => backend.observe(&item.selector, max_nodes, context.remaining()?)?,
        };
        context.remaining()?;
        let observation_id = uuid::Uuid::new_v4().to_string();
        let mut nodes = raw["nodes"]
            .as_array()
            .cloned()
            .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "snapshot omitted nodes"))?;
        if nodes.len() > max_nodes {
            return Err(error(
                "CONTROL.BAD_PROVIDER_OUTPUT",
                "snapshot exceeded node budget",
            ));
        }
        for node in &mut nodes {
            let mut capabilities = backend.node_capabilities(node);
            if !self.policy.allow_secret_input {
                capabilities.retain(|name| name != "secret_input");
            }
            if node["states"]["protected"] == true {
                node["value"] = Value::Null;
                capabilities.retain(|name| {
                    !matches!(name.as_str(), "fill" | "type_text" | "paste" | "key")
                });
            }
            node["capabilities"] = json!(capabilities);
            let node_id = node["node_id"]
                .as_str()
                .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "node omitted identity"))?
                .to_string();
            node["target"] = serde_json::to_value(Target {
                session_id: self.id.clone(),
                resource_id: resource_id.to_string(),
                observation_id: observation_id.clone(),
                node_id,
            })
            .unwrap();
        }
        let observation = Observation {
            scope: raw["scope_id"].as_str().or(scope).map(str::to_owned),
            max_nodes,
            provider: item.resource.provider.clone(),
            resource_id: resource_id.to_string(),
            raw,
            nodes,
            created: Instant::now(),
        };
        let mut state = self.state.lock().unwrap();
        // Native providers have different snapshot retention rules. A new
        // observation invalidates this provider's previous references.
        state
            .observations
            .retain(|_, old| old.provider != item.resource.provider);
        state
            .observations
            .insert(observation_id.clone(), observation.clone());
        Ok((observation_id, observation))
    }

    /// Resolve a locator inside one retained observation.
    fn find_in(
        &self,
        observation_id: &str,
        observation: &Observation,
        locator: &Locator,
        expect: Expect,
        context: &Context,
    ) -> Result<Value> {
        if observation.raw["truncated"] == true {
            return Err(error(
                "CONTROL.OBSERVATION_TRUNCATED",
                "truncated observation cannot prove uniqueness",
            ));
        }
        let backend = &self.backends[&observation.provider];
        let resolved =
            match backend.find(&observation.raw, locator, expect, context.remaining()?)? {
                Some(resolved) => resolved,
                None => portable_find(&observation.nodes, locator, expect)?,
            };
        context.remaining()?;
        if resolved["found"] != true {
            return Ok(json!({"found": false, "match_count": 0, "observation_id": observation_id}));
        }
        let node = resolved["node_id"]
            .as_str()
            .and_then(|id| observation.nodes.iter().find(|n| n["node_id"] == id))
            .ok_or_else(|| {
                error(
                    "CONTROL.BAD_PROVIDER_OUTPUT",
                    "find named a node outside the observation",
                )
            })?;
        Ok(
            json!({"found": true, "match_count": resolved["match_count"].as_u64().unwrap_or(1), "node": node, "target": node["target"], "observation_id": observation_id}),
        )
    }

    /// Observe and find again and again until the condition holds. Built only
    /// from observe and find, so every provider gets it; an ambiguous match
    /// never counts as found, and the last observation stays current so a
    /// found node's target can be acted on directly.
    #[allow(clippy::too_many_arguments)]
    fn wait(
        &self,
        resource_id: &str,
        locator: &Locator,
        condition: WaitCondition,
        states: Option<&Value>,
        max_nodes: usize,
        interval_ms: Option<u64>,
        scope: Option<&str>,
        context: &Context,
    ) -> Result<Value> {
        let invalid = |message: &str| Err(error("CONTROL.INVALID_REQUEST", message.to_string()));
        if locator.is_empty() {
            return invalid("wait requires a locator with at least one condition");
        }
        let interval = interval_ms.unwrap_or(DEFAULT_WAIT_INTERVAL_MS);
        if !(MIN_WAIT_INTERVAL_MS..=MAX_WAIT_INTERVAL_MS).contains(&interval) {
            return invalid("interval_ms must be in 50..5000");
        }
        let wanted = match (condition, states) {
            (WaitCondition::State, Some(Value::Object(map)))
                if !map.is_empty()
                    && map.len() <= 16
                    && map
                        .values()
                        .all(|v| v.is_boolean() || v.is_string() || v.is_number()) =>
            {
                map.clone()
            }
            (WaitCondition::State, _) => {
                return invalid(
                    "a state wait needs `states`: 1..16 state names with boolean, string or number values",
                )
            }
            (_, Some(_)) => return invalid("`states` applies to the state condition only"),
            (_, None) => serde_json::Map::new(),
        };
        let interval = Duration::from_millis(interval);
        let started = Instant::now();
        let mut polls = 0u64;
        loop {
            polls += 1;
            let (observation_id, observation) =
                match self.observe_resource_scoped(resource_id, max_nodes, scope, None, context) {
                    Ok(observed) => observed,
                    // The page was between documents: keep polling.
                    Err(e) if e.code == "BROWSER.CONTEXT_LOST" => {
                        let remaining = context.remaining()?;
                        if remaining < interval + Duration::from_millis(200) {
                            return Err(e.with_detail("polls", json!(polls)));
                        }
                        std::thread::sleep(interval.min(remaining));
                        continue;
                    }
                    Err(e) => return Err(e),
                };
            let (outcome, ambiguous) = match self.find_in(
                &observation_id,
                &observation,
                locator,
                Expect::Optional,
                context,
            ) {
                Ok(found) => (Some(found), None),
                Err(e) if is_ambiguous(&e.code) => (None, Some(e)),
                Err(e) => return Err(e),
            };
            let present = outcome.as_ref().is_some_and(|f| f["found"] == true);
            let satisfied = match condition {
                WaitCondition::Appears => present,
                // Several matches are still something present.
                WaitCondition::Disappears => outcome.is_some() && !present,
                WaitCondition::State => {
                    present
                        && outcome.as_ref().is_some_and(|f| {
                            wanted
                                .iter()
                                .all(|(name, value)| f["node"]["states"][name] == *value)
                        })
                }
            };
            if satisfied {
                let mut answer = json!({
                    "satisfied": true,
                    "condition": condition,
                    "resource_id": resource_id,
                    "observation_id": observation_id,
                    "polls": polls,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                });
                if let Some(found) = outcome.filter(|_| present) {
                    answer["node"] = found["node"].clone();
                    answer["target"] = found["target"].clone();
                }
                return Ok(answer);
            }
            // Stop while there is still time to say why, rather than letting
            // the next capture run into the request deadline.
            let remaining = context.remaining()?;
            if remaining < interval + Duration::from_millis(200) {
                if let Some(ambiguous) = ambiguous {
                    // The candidates are node ids of this observation: name
                    // it, so they can be narrowed with find there.
                    return Err(ambiguous
                        .with_detail("polls", json!(polls))
                        .with_detail("observation_id", json!(observation_id)));
                }
                let mut failure = error(
                    "CONTROL.WAIT_TIMEOUT",
                    match condition {
                        WaitCondition::Appears => "the element did not appear in time",
                        WaitCondition::Disappears => "the element was still there",
                        WaitCondition::State => "the element did not reach the states in time",
                    },
                )
                .with_retryable(true);
                failure.details.insert("polls".into(), json!(polls));
                failure
                    .details
                    .insert("observation_id".into(), json!(observation_id));
                failure.details.insert("present".into(), json!(present));
                if let Some(found) = outcome.filter(|_| present) {
                    failure
                        .details
                        .insert("states".into(), found["node"]["states"].clone());
                }
                return Err(failure);
            }
            let wake = Instant::now() + interval;
            while Instant::now() < wake {
                context.remaining()?;
                std::thread::sleep(
                    wake.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(25)),
                );
            }
        }
    }

    /// Each file as an existing regular file under an allowed directory, by
    /// its resolved path, so a link cannot lead outside the grant.
    fn upload_files(&self, files: &[String]) -> Result<Vec<String>> {
        if self.policy.upload_roots.is_empty() {
            return Err(error(
                "POLICY.DENIED",
                "file uploads were not allowed by whoever started this server",
            ));
        }
        let roots: Vec<std::path::PathBuf> = self
            .policy
            .upload_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect();
        files
            .iter()
            .map(|file| {
                let path = std::path::Path::new(file);
                if !path.is_absolute() {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "upload takes absolute file paths",
                    ));
                }
                let real = path
                    .canonicalize()
                    .ok()
                    .filter(|real| real.is_file())
                    .ok_or_else(|| {
                        error(
                            "CONTROL.FILE_NOT_FOUND",
                            format!("no regular file at {file}"),
                        )
                    })?;
                if !roots.iter().any(|root| real.starts_with(root)) {
                    return Err(error(
                        "POLICY.DENIED",
                        format!("{file} is outside the directories allowed for uploads"),
                    ));
                }
                Ok(plain_path(&real))
            })
            .collect()
    }

    fn observation(&self, id: &str) -> Result<Observation> {
        self.state
            .lock()
            .unwrap()
            .observations
            .get(id)
            .filter(|o| o.created.elapsed() < self.observation_ttl)
            .cloned()
            .ok_or_else(|| {
                error(
                    "CONTROL.STALE_TARGET",
                    "observation expired or is no longer current",
                )
            })
    }
}

/// The sha256 of a screenshot's base64 PNG. Compared across captures to tell
/// whether what is shown changed, e.g. a login code that refreshes, without
/// reading the image; a read_text of the same pixels carries the same digest.
fn digest_of(shot: &Value) -> String {
    use sha2::Digest;
    shot["data"]
        .as_str()
        .map(|data| {
            sha2::Sha256::digest(data.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        })
        .unwrap_or_default()
}

/// A provider's OCR answer as callers see it: positions as fractions of the
/// element's box (never screen or image coordinates), the image's digest
/// instead of the image.
fn text_answer(read: &Value, provider: &str) -> Value {
    let width = read["width"].as_f64().filter(|w| *w > 0.0).unwrap_or(1.0);
    let height = read["height"].as_f64().filter(|h| *h > 0.0).unwrap_or(1.0);
    let round = |value: f64| (value.clamp(0.0, 1.0) * 10_000.0).round() / 10_000.0;
    let shape = |span: &Value| {
        let number = |key: &str| span["bounds"][key].as_f64().unwrap_or_default();
        json!({
            "text": span["text"],
            "box": {
                "fx": round(number("x") / width),
                "fy": round(number("y") / height),
                "fw": round(number("width") / width),
                "fh": round(number("height") / height),
            },
        })
    };
    let lines: Vec<Value> = read["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .map(|line| {
                    let mut shaped = shape(line);
                    shaped["words"] = json!(line["words"]
                        .as_array()
                        .map(|words| words.iter().map(shape).collect::<Vec<_>>())
                        .unwrap_or_default());
                    shaped
                })
                .collect()
        })
        .unwrap_or_default();
    let text = lines
        .iter()
        .filter_map(|line| line["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "provider": provider,
        "engine": read["engine"],
        "language": read["language"],
        "text": text,
        "lines": lines,
        "digest": digest_of(read),
        "clipped": read["clipped"] == true,
        "read_only": true,
    })
}

fn validate_action(action: &Action) -> Result<()> {
    if let Action::SecretInput { secret_ref } = action {
        if !valid_secret_ref(secret_ref) {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "invalid secret binding reference",
            ));
        }
    }
    let text = match action {
        Action::Fill { value } => Some(value),
        Action::TypeText { text, .. } => Some(text),
        Action::Paste { text } => Some(text),
        Action::Select { option } => Some(option),
        Action::Accept { text } => text.as_ref(),
        _ => None,
    };
    if let Action::Navigate { url } | Action::OpenTab { url } = action {
        let scheme = url
            .split_once("://")
            .map(|(scheme, _)| scheme.to_ascii_lowercase());
        if url.len() > 2048
            || url.chars().any(char::is_control)
            || !matches!(scheme.as_deref(), Some("http" | "https"))
        {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "navigate and open_tab take an http or https address of at most 2048 characters",
            ));
        }
    }
    if text.is_some_and(|s| s.len() > 4096 || s.contains('\0')) {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "input text must be bounded and contain no NUL",
        ));
    }
    if matches!(action, Action::Paste { text } if text.is_empty()) {
        return Err(error("CONTROL.INVALID_REQUEST", "paste needs some text"));
    }
    if let Action::Upload { files } = action {
        if files.is_empty()
            || files.len() > MAX_UPLOAD_FILES
            || files
                .iter()
                .any(|f| f.is_empty() || f.len() > 4096 || f.chars().any(char::is_control))
        {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "upload takes 1..32 file paths of at most 4096 characters",
            ));
        }
    }
    if let Action::Emulate {
        preset: None,
        color_scheme: None,
    } = action
    {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "emulate needs a preset, a color_scheme or both",
        ));
    }
    if let Action::Key { keys } = action {
        if keys.is_empty() || keys.len() > 6 || keys.iter().any(|s| s.is_empty() || s.len() > 32) {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "key chord is outside the allowed limits",
            ));
        }
    }
    let invalid = |message: &str| Err(error("CONTROL.INVALID_REQUEST", message.to_string()));
    if let Action::Click { modifiers, .. } | Action::Drag { modifiers, .. } = action {
        if modifiers.len() > 4
            || modifiers
                .iter()
                .enumerate()
                .any(|(index, modifier)| modifiers[..index].contains(modifier))
        {
            return invalid("pointer modifiers must be unique: ctrl, shift, alt or meta");
        }
    }
    let fractions: Vec<&Fraction> = match action {
        Action::Click { fraction, .. }
        | Action::Scroll { fraction, .. }
        | Action::Hover { fraction, .. } => fraction.iter().collect(),
        Action::Drag {
            fraction,
            to_fraction,
            ..
        } => fraction.iter().chain(to_fraction.iter()).collect(),
        _ => Vec::new(),
    };
    if fractions
        .iter()
        .any(|f| !(0.0..=1.0).contains(&f.fx) || !(0.0..=1.0).contains(&f.fy))
    {
        return invalid("fraction must lie within the element: fx and fy in 0..1");
    }
    match action {
        Action::Click { count: Some(c), .. } if !matches!(c, 1 | 2) => {
            invalid("click count must be 1 or 2")
        }
        Action::Scroll {
            notches: Some(n), ..
        } if !(1..=30).contains(n) => invalid("scroll notches must be in 1..30"),
        Action::Hover { ms: Some(ms), .. } if !(1..=5000).contains(ms) => {
            invalid("hover ms must be in 1..5000")
        }
        _ => Ok(()),
    }
}

/// A resolved path as browsers accept it: without the Windows `\\?\` prefix.
fn plain_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        Some(rest) => match rest.strip_prefix("UNC\\") {
            Some(share) => format!(r"\\{share}"),
            None => text,
        },
        None => text,
    }
}

fn validate_view(view: &View) -> Result<()> {
    if view.scope_id.is_some() && view.scope_target.is_some() {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "scope_id and scope_target are mutually exclusive",
        ));
    }
    if view
        .limit
        .is_some_and(|limit| limit == 0 || limit > MAX_OUTLINE_LIMIT)
        || view.max_characters.is_some_and(|chars| {
            !(MIN_OUTLINE_CHARACTERS..=MAX_OUTLINE_CHARACTERS).contains(&chars)
        })
    {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "view limit must be in 1..500 and max_characters in 500..200000",
        ));
    }
    let shaped = match view.mode {
        ViewMode::Outline => true,
        ViewMode::Text => view.limit.is_none(),
        _ => false,
    };
    if !shaped && (view.region.is_some() || view.limit.is_some() || view.max_characters.is_some()) {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "region and max_characters shape an outline or text view; limit an outline only",
        ));
    }
    Ok(())
}

/// Elements a caller can act on or read, in capture order, within both the
/// element and character ceilings. Providers with richer structure override it.
///
/// A provider that reports a `region` on its nodes (the name of the landmark
/// a node sits in, null outside any) also gets the overview and the outline
/// of one region.
fn portable_view(nodes: &[Value], view: &View) -> Result<Value> {
    if view.mode == ViewMode::Text {
        return Err(error(
            "CONTROL.VIEW_UNSUPPORTED",
            "this provider has no text view; observe with an outline view",
        ));
    }
    let regional = nodes.iter().any(|n| n.get("region").is_some());
    if !regional && (view.mode == ViewMode::Overview || view.region.is_some()) {
        return Err(error(
            "CONTROL.VIEW_UNSUPPORTED",
            "this provider has no region overview; observe with an outline view",
        ));
    }
    let region_of = |n: &Value| n["region"].as_str().unwrap_or(LOOSE_REGION).to_string();
    if view.mode == ViewMode::Overview {
        return Ok(portable_overview(nodes, region_of));
    }
    let limit = view.limit.unwrap_or(DEFAULT_OUTLINE_LIMIT);
    let budget = view.max_characters.unwrap_or(DEFAULT_OUTLINE_CHARACTERS);
    let mut eligible: Vec<&Value> = nodes
        .iter()
        .filter(|n| {
            n["capabilities"].as_array().is_some_and(|c| !c.is_empty())
                || n["name"].as_str().is_some_and(|s| !s.is_empty())
        })
        .filter(|n| n["states"]["offscreen"] != true)
        .collect();
    if let Some(region) = &view.region {
        eligible.retain(|n| region_of(n) == *region);
        if eligible.is_empty() {
            // Not an empty region: a misremembered name is a different fact.
            return Err(error(
                "CONTROL.NOT_FOUND",
                format!("no region named {region:?} here"),
            )
            .with_retryable(true)
            .with_detail(
                "regions",
                portable_overview(nodes, region_of)["regions"].clone(),
            ));
        }
    }
    let mut shown = Vec::new();
    let mut characters = 0;
    let mut stopped_by = Value::Null;
    for node in &eligible {
        if shown.len() >= limit {
            stopped_by = json!("limit");
            break;
        }
        let size = serde_json::to_string(node).map(|s| s.len()).unwrap_or(0);
        if characters + size > budget {
            stopped_by = json!("characters");
            break;
        }
        characters += size;
        shown.push((*node).clone());
    }
    let mut answer = json!({
        "nodes": shown,
        "shown": shown.len(),
        "matched": eligible.len(),
        "outline_truncated": shown.len() < eligible.len(),
        "stopped_by": stopped_by,
    });
    if let Some(region) = &view.region {
        answer["region"] = json!(region);
    }
    Ok(answer)
}

/// The region of a node outside every landmark.
pub const LOOSE_REGION: &str = "(loose in the page)";
/// Regions an overview names; the rest are counted as folded.
const OVERVIEW_REGIONS: usize = 12;

/// Regions by how many actionable elements each holds, largest first, with
/// what kind of thing is in them.
fn portable_overview(nodes: &[Value], region_of: impl Fn(&Value) -> String) -> Value {
    let mut regions: Vec<(String, Vec<&Value>)> = Vec::new();
    for node in nodes.iter().filter(|n| {
        n["capabilities"].as_array().is_some_and(|c| !c.is_empty())
            && n["states"]["offscreen"] != true
    }) {
        let label = region_of(node);
        match regions.iter_mut().find(|(known, _)| *known == label) {
            Some((_, members)) => members.push(node),
            None => regions.push((label, vec![node])),
        }
    }
    regions.sort_by_key(|(_, members)| std::cmp::Reverse(members.len()));
    let interactive: usize = regions.iter().map(|(_, m)| m.len()).sum();
    let (named, folded) = regions.split_at(OVERVIEW_REGIONS.min(regions.len()));
    let described: Vec<Value> = named
        .iter()
        .map(|(label, members)| {
            let mut roles: BTreeMap<&str, usize> = BTreeMap::new();
            for node in members {
                *roles.entry(node["role"].as_str().unwrap_or("")).or_default() += 1;
            }
            json!({
                "region": label,
                "elements": members.len(),
                "holds": roles.iter().map(|(role, count)| json!({"role": role, "count": count})).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "regions": described,
        "interactive": interactive,
        "folded_regions": folded.len(),
        "folded_elements": folded.iter().map(|(_, m)| m.len()).sum::<usize>(),
    })
}

/// Exact `role` / `name` / `node_id` matching, the subset every provider shares.
fn portable_find(nodes: &[Value], locator: &Locator, expect: Expect) -> Result<Value> {
    if locator.extended() {
        return Err(error(
            "CONTROL.LOCATOR_UNSUPPORTED",
            "this provider matches only role, name and node_id exactly",
        ));
    }
    let matching: Vec<_> = nodes
        .iter()
        .filter(|node| node["provenance"]["scope_context"] != true)
        .filter(|n| {
            locator.name.as_ref().is_none_or(|v| n["name"] == *v)
                && locator.role.as_ref().is_none_or(|v| n["role"] == *v)
                && locator.node_id.as_ref().is_none_or(|v| n["node_id"] == *v)
        })
        .collect();
    match matching.as_slice() {
        [] if expect == Expect::Optional => Ok(json!({"found": false})),
        [] => Err(error(
            "CONTROL.NOT_FOUND",
            "find must resolve exactly one observed node",
        )
        .with_retryable(true)),
        [only] => Ok(json!({"found": true, "node_id": only["node_id"], "match_count": 1})),
        many => {
            let mut failure = error(
                "CONTROL.AMBIGUOUS",
                format!("the locator matched {} observed nodes", many.len()),
            );
            failure
                .details
                .insert("match_count".into(), json!(many.len()));
            failure.details.insert(
                "candidates".into(),
                json!(many
                    .iter()
                    .take(10)
                    .map(|n| json!({"node_id": n["node_id"], "role": n["role"], "name": n["name"]}))
                    .collect::<Vec<_>>()),
            );
            Err(failure)
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn close_waits_for_attachment_final_state_update() {
        let mut session = Session::new(Vec::new(), Policy::default());
        session.browser = Some(Arc::new(browser::BrowserBackend::new(0)));
        let session = Arc::new(session);
        let state = session.state.lock().unwrap();
        let attaching = session.clone();
        let worker = std::thread::spawn(move || attaching.attach_launched_browser(32123));
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.browser_port() != Some(32123) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let closing = session.clone();
        let closer = std::thread::spawn(move || closing.close());
        while !session.closed.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(state);
        let _ = worker.join().unwrap();
        closer.join().unwrap();
        assert_eq!(session.browser_port(), None);
        assert!(session.attach_launched_browser(32124).is_err());
    }

    #[test]
    fn close_serializes_with_endpoint_and_owner_publication() {
        let mut session = Session::new(Vec::new(), Policy::default());
        session.browser = Some(Arc::new(browser::BrowserBackend::new(0)));
        let session = Arc::new(session);
        let owned = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_session = session.clone();
        let worker_owned = owned.clone();
        let worker = std::thread::spawn(move || {
            worker_session.publish_launched_browser(32123, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                worker_owned.store(true, Ordering::SeqCst);
                Box::new(move || {
                    worker_owned.store(false, Ordering::SeqCst);
                })
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let closing = session.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let closer = std::thread::spawn(move || {
            closing.close();
            done_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !session.closed.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(
            done_rx.try_recv().is_err(),
            "close returned before candidate publication cleanup"
        );
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        closer.join().unwrap();
        assert!(!owned.load(Ordering::SeqCst));
        assert_eq!(session.browser_port(), None);
        assert!(session
            .publish_launched_browser(32124, || panic!("published after close"))
            .is_err());
    }

    #[test]
    fn browser_lifecycle_and_downloads_wait_for_inflight_dispatch() {
        for operation in ["attach", "launch", "detach", "downloads"] {
            let mut session = Session::new(Vec::new(), Policy::default());
            session.browser = Some(Arc::new(browser::BrowserBackend::new(0)));
            let session = Arc::new(session);
            // Model an admitted enumeration paused before writing its results.
            let gate = session.gate.lock().unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let running = session.clone();
            let worker = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                match operation {
                    "attach" => running.attach_browser(0).unwrap(),
                    "launch" => running.attach_launched_browser(0).unwrap(),
                    "detach" => running.detach_browser(),
                    _ => {
                        assert!(running
                            .browser_downloads(Duration::from_millis(50))
                            .is_err());
                    }
                }
                done_tx.send(()).unwrap();
            });
            started_rx.recv().unwrap();
            assert!(
                done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
                "{operation} bypassed the in-flight query gate"
            );
            drop(gate);
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            worker.join().unwrap();
            assert_eq!(session.browser_port(), None);
            assert!(session.state.lock().unwrap().resources.is_empty());
        }
    }

    #[test]
    fn new_tabs_open_only_web_addresses() {
        for url in ["http://127.0.0.1/", "https://example.com/a?b"] {
            assert!(validate_action(&Action::OpenTab { url: url.into() }).is_ok());
        }
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "chrome://settings",
            "https://example.com/\n",
        ] {
            assert!(validate_action(&Action::OpenTab { url: url.into() }).is_err());
        }
        assert!(validate_action(&Action::ClosePage {}).is_ok());
    }

    #[test]
    fn cancellation_cannot_fall_between_replay_guard_and_context_registration() {
        let session = Arc::new(Session::new(Vec::new(), Policy::default()));
        // Keep execution queued, then pause admission at its state update.
        let gate = session.gate.lock().unwrap();
        let state = session.state.lock().unwrap();
        let request = Request::new(None, Operation::Capabilities {});
        let id = request.request_id.clone();
        let running = session.clone();
        let worker = std::thread::spawn(move || running.execute(&request));
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.active.try_lock().is_ok() {
            assert!(Instant::now() < deadline, "admission did not begin");
            std::thread::yield_now();
        }
        let cancelling = session.clone();
        let cancel = std::thread::spawn(move || cancelling.cancel(&id));
        drop(state);
        assert!(cancel.join().unwrap(), "queued request must be cancellable");
        drop(gate);
        let response = worker.join().unwrap();
        assert_eq!(response.commit_state, CommitState::NotCommitted);
        assert_eq!(response.error.unwrap()["code"], "CONTROL.CANCELLED");
    }
}

#[cfg(test)]
mod capture_cancellation_tests {
    use super::*;
    #[test]
    fn capture_barrier_cancellation_wins_before_backend_error() {
        let context = Arc::new(Context::new(Duration::from_secs(2), false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let running = context.clone();
        let worker = std::thread::spawn(move || {
            capture_cancellable(&running, move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Err(error("BROWSER.CDP_ERROR", "late capture error"))
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let start = Instant::now();
        context.cancel();
        let result = worker.join().unwrap().unwrap_err();
        assert_eq!(result.code, "CONTROL.CANCELLED");
        assert!(start.elapsed() < Duration::from_millis(250));
        release_tx.send(()).unwrap();
    }
    #[test]
    fn capture_barrier_timeout_discards_late_image() {
        let context = Context::new(Duration::from_millis(50), false);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let start = Instant::now();
        let result = capture_cancellable(&context, move || {
            release_rx.recv().unwrap();
            Ok(json!({"data":"late-image"}))
        })
        .unwrap_err();
        assert_eq!(result.code, "CONTROL.TIMEOUT");
        assert!(start.elapsed() < Duration::from_millis(250));
        release_tx.send(()).unwrap();
    }
}

#[cfg(test)]
mod browser_request_tests {
    use super::*;
    #[test]
    fn queued_browser_request_cannot_commit_after_cancel_or_close() {
        for close in [false, true] {
            let session = Arc::new(Session::new(Vec::new(), Policy::default()));
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let running = session.clone();
            let worker = std::thread::spawn(move || {
                running.browser_request("queued", |context| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    running.browser_effect(context, || panic!("side effect after revocation"))
                })
            });
            entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            if close {
                session.close();
            } else {
                assert!(session.cancel("queued"));
            }
            release_tx.send(()).unwrap();
            let failure = worker.join().unwrap().unwrap().unwrap_err();
            assert_eq!(
                failure.code,
                if close {
                    "CONTROL.SESSION_CLOSED"
                } else {
                    "CONTROL.CANCELLED"
                }
            );
        }
    }
}

#[cfg(test)]
mod input_lease_set_tests {
    use super::*;

    #[test]
    fn a_failed_lease_set_releases_only_new_locks() {
        let id = uuid::Uuid::new_v4();
        let fresh = format!("lease-test-{id}-a");
        let retained = format!("lease-test-{id}-m");
        let blocked = format!("lease-test-{id}-z");
        let owner = Session::new(Vec::new(), Policy::default());
        let contender = Session::new(Vec::new(), Policy::default());
        let verifier = Session::new(Vec::new(), Policy::default());
        owner.input_lease(&blocked).unwrap();
        contender.input_lease(&retained).unwrap();
        // Input order is intentionally reversed: sorted acquisition takes the
        // new lock first, then the pre-existing one, then encounters conflict.
        let failed = contender.input_leases(&[blocked.clone(), retained.clone(), fresh.clone()]);
        assert_eq!(failed.err().unwrap().code, "CONTROL.RESOURCE_BUSY");
        verifier
            .input_lease(&fresh)
            .expect("new lock was rolled back");
        assert_eq!(
            verifier.input_lease(&retained).err().unwrap().code,
            "CONTROL.RESOURCE_BUSY"
        );
        owner.close();
        contender
            .input_leases(&[blocked.clone(), blocked, retained])
            .unwrap();
        assert_eq!(
            contender.leases.lock().unwrap().len(),
            2,
            "duplicates are acquired once"
        );
    }
}

#[cfg(test)]
mod browser_cancel_boundary_tests {
    use super::*;
    #[test]
    fn cancellation_does_not_wait_for_effect_and_prevents_publication() {
        let context = Arc::new(Context::new(Duration::from_secs(10), false));
        let effect = context.effect_gate.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancelling = context.clone();
        let worker = std::thread::spawn(move || {
            cancelling.cancel_browser();
            tx.send(()).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        drop(effect);
        worker.join().unwrap();
        result.expect("cancel must not wait for lifecycle effects");
        assert!(context.remaining().is_err());
        assert!(context
            .browser_publication
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| matches!(
                state,
                0 | 3
            )
            .then_some(2))
            .is_err());
    }
    #[test]
    fn cancellation_after_publication_does_not_claim_to_undo_it() {
        let context = Context::new(Duration::from_secs(10), false);
        context.browser_publication.store(2, Ordering::SeqCst);
        context.cancel_browser();
        assert!(context.remaining().is_ok());
    }
}
