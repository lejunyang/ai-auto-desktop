//! One session contract for native desktop providers and browser targets.
//! No network listener: remote sessions inherit an authenticated SSH stdio pipe.

pub mod browser;
pub mod lease;
pub mod model;
pub mod native;
pub mod transport;
pub use model::*;

use aad_core::AutomationError;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, AutomationError>;

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
    fn resources(&self, timeout: Duration) -> Result<Vec<BackendResource>>;
    fn observe(&self, selector: &Value, max_nodes: usize, timeout: Duration) -> Result<Value>;
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
    /// Empty means all installed providers in this OS account, never arbitrary plugins.
    pub providers: BTreeSet<String>,
    /// Optional server-owned allowlist; a client cannot extend it.
    pub resources: Option<BTreeSet<String>>,
}

#[derive(Clone)]
struct Observation {
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
    // 0=pending, 1=may have committed, 2=cancelled before, 3=cancelled after.
    phase: AtomicU8,
    deadline: Instant,
    mutating: bool,
}

impl Context {
    pub fn new(timeout: Duration, mutating: bool) -> Self {
        Self {
            phase: AtomicU8::new(0),
            deadline: Instant::now() + timeout,
            mutating,
        }
    }
    pub fn cancel(&self) {
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
        let phase = self.phase.load(Ordering::SeqCst);
        let code = if matches!(phase, 2 | 3) {
            Some("CONTROL.CANCELLED")
        } else if Instant::now() >= self.deadline {
            Some("CONTROL.TIMEOUT")
        } else {
            None
        };
        if let Some(code) = code {
            let effect = if self.mutating && matches!(phase, 1 | 3) {
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
    active: Mutex<BTreeMap<String, Arc<Context>>>,
    closed: AtomicBool,
    created: Instant,
    lifetime: Duration,
    observation_ttl: Duration,
    max_requests: usize,
    leases: Mutex<BTreeMap<String, Arc<lease::InputLease>>>,
    /// The native providers behind this session, for saved workflows.
    providers: Vec<Arc<dyn aad_runtime::Provider>>,
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
            active: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
            created: Instant::now(),
            lifetime: Duration::from_secs(3600),
            observation_ttl: Duration::from_secs(300),
            max_requests: 10_000,
            leases: Mutex::new(BTreeMap::new()),
            providers: Vec::new(),
        }
    }

    pub fn native(policy: Policy) -> Self {
        Self::native_with_browser(policy, None)
    }

    pub fn native_with_browser(policy: Policy, browser_port: Option<u16>) -> Self {
        let (natives, failures) = native::backends();
        let providers = natives.iter().map(|b| b.provider()).collect();
        let backends = natives.into_iter().map(|b| b as Arc<dyn Backend>).collect();
        let mut session = Self::new(backends, policy).with_providers(providers);
        if let Some(port) = browser_port {
            session.backends.insert(
                browser::PROVIDER.into(),
                Arc::new(browser::BrowserBackend::new(port)),
            );
        }
        session.failures = failures;
        session
    }

    /// Native providers that saved workflows may drive. They must be the
    /// instances behind this session's backends, so a workflow shares their
    /// snapshot stores rather than opening a second driver.
    pub fn with_providers(mut self, providers: Vec<Arc<dyn aad_runtime::Provider>>) -> Self {
        self.providers = providers;
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
        self.closed.store(true, Ordering::SeqCst);
        for context in self.active.lock().unwrap().values() {
            context.cancel();
        }
        // Do not wait on the backend gate: a close must cancel an in-flight request.
        self.state.lock().unwrap().observations.clear();
        self.leases.lock().unwrap().clear();
        for backend in self.backends.values() {
            backend.close();
        }
    }

    pub fn input_lease(&self, key: &str) -> Result<Arc<lease::InputLease>> {
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
        if let Some(lease) = leases.get(key) {
            return Ok(lease.clone());
        }
        let lease = Arc::new(lease::InputLease::acquire(key)?);
        leases.insert(key.into(), lease.clone());
        Ok(lease)
    }

    pub fn execute(&self, request: &Request) -> Response {
        let result = self.run(request);
        let (data, failure, effect) = match result {
            Ok(value) => (
                Some(value),
                None,
                if request.mutating() {
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
            Operation::Capabilities {} => Ok(
                json!({"session_id": self.id, "protocol_version": PROTOCOL_VERSION,
                "expires_in_ms": self.lifetime.saturating_sub(self.created.elapsed()).as_millis(),
                "allow_input": self.policy.allow_input,"observation_retention":"until_next_observation_or_input_attempt","observation_ttl_ms":self.observation_ttl.as_millis(),"max_requests":self.max_requests,"input_lease":"exclusive_cooperating_session","input_lease_scope":"os_user","providers": self.backends.values().filter(|b| self.allowed_provider(b.name()))
                    .map(|b| json!({"provider": b.name(), "actions": b.capabilities()})).collect::<Vec<_>>(),
                "unavailable": self.failures}),
            ),
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
                self.state
                    .lock()
                    .unwrap()
                    .observations
                    .retain(|_, old| old.provider != item.resource.provider);
                let raw = backend.observe(&item.selector, *max_nodes, context.remaining()?)?;
                context.remaining()?;
                let observation_id = uuid::Uuid::new_v4().to_string();
                let mut nodes = raw["nodes"].as_array().cloned().ok_or_else(|| {
                    error("CONTROL.BAD_PROVIDER_OUTPUT", "snapshot omitted nodes")
                })?;
                if nodes.len() > *max_nodes {
                    return Err(error(
                        "CONTROL.BAD_PROVIDER_OUTPUT",
                        "snapshot exceeded node budget",
                    ));
                }
                for node in &mut nodes {
                    node["capabilities"] = json!(backend.node_capabilities(node));
                    let node_id = node["node_id"]
                        .as_str()
                        .ok_or_else(|| {
                            error("CONTROL.BAD_PROVIDER_OUTPUT", "node omitted identity")
                        })?
                        .to_string();
                    node["target"] = serde_json::to_value(Target {
                        session_id: self.id.clone(),
                        resource_id: resource_id.clone(),
                        observation_id: observation_id.clone(),
                        node_id,
                    })
                    .unwrap();
                }
                let mut state = self.state.lock().unwrap();
                // Native providers have different snapshot retention rules. A new
                // observation invalidates this provider's previous references.
                state
                    .observations
                    .retain(|_, old| old.provider != item.resource.provider);
                state.observations.insert(
                    observation_id.clone(),
                    Observation {
                        provider: item.resource.provider,
                        resource_id: resource_id.clone(),
                        raw: raw.clone(),
                        nodes: nodes.clone(),
                        created: Instant::now(),
                    },
                );
                drop(state);
                let mut answer = json!({"observation_id": observation_id, "resource_id": resource_id, "node_count": nodes.len(), "truncated": raw["truncated"] == true, "metadata": raw.get("metadata").cloned().unwrap_or(Value::Null)});
                match view.mode {
                    ViewMode::Nodes => answer["nodes"] = json!(nodes),
                    ViewMode::Outline | ViewMode::Overview => {
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
                if observation.raw["truncated"] == true {
                    return Err(error(
                        "CONTROL.OBSERVATION_TRUNCATED",
                        "truncated observation cannot prove uniqueness",
                    ));
                }
                let backend = &self.backends[&observation.provider];
                let resolved =
                    match backend.find(&observation.raw, locator, *expect, context.remaining()?)? {
                        Some(resolved) => resolved,
                        None => portable_find(&observation.nodes, locator, *expect)?,
                    };
                context.remaining()?;
                if resolved["found"] != true {
                    return Ok(
                        json!({"found": false, "match_count": 0, "observation_id": observation_id}),
                    );
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
                    json!({"found": true, "match_count": resolved["match_count"].as_u64().unwrap_or(1), "node": node, "target": node["target"]}),
                )
            }
            Operation::Act { target, action } => {
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
                let observed = |id: &str| {
                    observation.raw["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|n| n["node_id"] == id)
                        .ok_or_else(|| error("CONTROL.STALE_TARGET", "node was not observed"))
                };
                let node = observed(&target.node_id)?;
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
                    if to.node_id == target.node_id {
                        return Err(error(
                            "CONTROL.INVALID_REQUEST",
                            "drag destination must be a different node",
                        ));
                    }
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
                let _lease = backend
                    .lease_key(&observation.raw)
                    .map(|key| self.input_lease(&key))
                    .transpose()?;
                context.remaining()?;
                let result = backend.act(&observation.raw, node, action, context);
                self.state
                    .lock()
                    .unwrap()
                    .observations
                    .retain(|_, old| old.provider != observation.provider);
                let value = result?;
                context.remaining()?;
                Ok(
                    json!({"submitted": true, "provider": observation.provider, "operation": action.name(), "backend_result": value, "requires_fresh_observation": true}),
                )
            }
            Operation::Cancel { .. } | Operation::Close {} => unreachable!(),
        }
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

fn validate_action(action: &Action) -> Result<()> {
    let text = match action {
        Action::Fill { value } => Some(value),
        Action::TypeText { text, .. } => Some(text),
        _ => None,
    };
    if text.is_some_and(|s| s.len() > 4096 || s.contains('\0')) {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "input text must be bounded and contain no NUL",
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

fn validate_view(view: &View) -> Result<()> {
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
    if view.mode != ViewMode::Outline
        && (view.region.is_some() || view.limit.is_some() || view.max_characters.is_some())
    {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "region, limit and max_characters shape an outline view only",
        ));
    }
    Ok(())
}

/// Elements a caller can act on or read, in capture order, within both the
/// element and character ceilings. Providers with richer structure override it.
fn portable_view(nodes: &[Value], view: &View) -> Result<Value> {
    if view.mode == ViewMode::Overview || view.region.is_some() {
        return Err(error(
            "CONTROL.VIEW_UNSUPPORTED",
            "this provider has no region overview; observe with an outline view",
        ));
    }
    let limit = view.limit.unwrap_or(DEFAULT_OUTLINE_LIMIT);
    let budget = view.max_characters.unwrap_or(DEFAULT_OUTLINE_CHARACTERS);
    let eligible: Vec<&Value> = nodes
        .iter()
        .filter(|n| {
            n["capabilities"].as_array().is_some_and(|c| !c.is_empty())
                || n["name"].as_str().is_some_and(|s| !s.is_empty())
        })
        .filter(|n| n["states"]["offscreen"] != true)
        .collect();
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
    Ok(json!({
        "nodes": shown,
        "shown": shown.len(),
        "matched": eligible.len(),
        "outline_truncated": shown.len() < eligible.len(),
        "stopped_by": stopped_by,
    }))
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
