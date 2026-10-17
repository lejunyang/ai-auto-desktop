//! One session contract for native desktop providers and browser targets.
//! No network listener: remote sessions inherit an authenticated SSH stdio pipe.

pub mod model;
pub mod native;
pub use model::*;

use aad_core::AutomationError;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
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
    fn name(&self) -> &str;
    fn capabilities(&self) -> Vec<String>;
    fn resources(&self, timeout: Duration) -> Result<Vec<BackendResource>>;
    fn observe(&self, selector: &Value, max_nodes: usize, timeout: Duration) -> Result<Value>;
    fn act(
        &self,
        snapshot: &Value,
        node: &Value,
        action: &Action,
        timeout: Duration,
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
    created: Instant,
}

#[derive(Default)]
struct State {
    resources: BTreeMap<String, BackendResource>,
    observations: BTreeMap<String, Observation>,
    requests: BTreeSet<String>,
}

pub struct Context {
    pub cancelled: AtomicBool,
    dispatched: AtomicBool,
    deadline: Instant,
    mutating: bool,
}

impl Context {
    fn remaining(&self) -> Result<Duration> {
        let code = if self.cancelled.load(Ordering::SeqCst) {
            Some("CONTROL.CANCELLED")
        } else if Instant::now() >= self.deadline {
            Some("CONTROL.TIMEOUT")
        } else {
            None
        };
        if let Some(code) = code {
            let effect = if self.mutating && self.dispatched.load(Ordering::SeqCst) {
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
    gate: Mutex<()>,
    active: Mutex<BTreeMap<String, Arc<Context>>>,
    closed: AtomicBool,
    created: Instant,
    lifetime: Duration,
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
            gate: Mutex::new(()),
            active: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
            created: Instant::now(),
            lifetime: Duration::from_secs(3600),
        }
    }

    pub fn native(policy: Policy) -> Self {
        let (backends, failures) = native::backends();
        let mut session = Self::new(backends, policy);
        session.failures = failures;
        session
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn cancel(&self, id: &str) -> bool {
        if let Some(context) = self.active.lock().unwrap().get(id) {
            context.cancelled.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for context in self.active.lock().unwrap().values() {
            context.cancelled.store(true, Ordering::SeqCst);
        }
        // Do not wait on the backend gate: a close must cancel an in-flight request.
        self.state.lock().unwrap().observations.clear();
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
            return Err(error(
                "CONTROL.SESSION_CLOSED",
                "session is closed or expired",
            ));
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.requests.len() >= 10_000 {
                return Err(error(
                    "CONTROL.SESSION_LIMIT",
                    "open a new session after 10000 requests",
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
                return Ok(json!({"cancellation_requested": self.cancel(request_id)}))
            }
            Operation::Close {} => {
                self.close();
                return Ok(json!({"closed": true}));
            }
            _ => {}
        }
        let context = Arc::new(Context {
            cancelled: AtomicBool::new(false),
            dispatched: AtomicBool::new(false),
            deadline: Instant::now() + Duration::from_millis(request.timeout_ms),
            mutating: request.mutating(),
        });
        self.active
            .lock()
            .unwrap()
            .insert(request.request_id.clone(), context.clone());
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
                "allow_input": self.policy.allow_input, "providers": self.backends.values().filter(|b| self.allowed_provider(b.name()))
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
            } => {
                if *max_nodes == 0 || *max_nodes > MAX_NODES {
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
                        created: Instant::now(),
                    },
                );
                Ok(
                    json!({"observation_id": observation_id, "resource_id": resource_id, "nodes": nodes, "truncated": raw["truncated"] == true, "metadata": raw.get("metadata").cloned().unwrap_or(Value::Null)}),
                )
            }
            Operation::Find {
                observation_id,
                locator,
            } => {
                if locator.name.is_none() && locator.role.is_none() && locator.node_id.is_none() {
                    return Err(error(
                        "CONTROL.INVALID_REQUEST",
                        "find requires at least one exact condition",
                    ));
                }
                let observation = self.observation(observation_id)?;
                if observation.raw["truncated"] == true {
                    return Err(error(
                        "CONTROL.OBSERVATION_TRUNCATED",
                        "truncated observation cannot prove uniqueness",
                    ));
                }
                let nodes = observation.raw["nodes"].as_array().unwrap();
                let matching: Vec<_> = nodes
                    .iter()
                    .filter(|n| {
                        locator.name.as_ref().is_none_or(|v| n["name"] == *v)
                            && locator.role.as_ref().is_none_or(|v| n["role"] == *v)
                            && locator.node_id.as_ref().is_none_or(|v| n["node_id"] == *v)
                    })
                    .collect();
                if matching.len() != 1 {
                    return Err(error(
                        if matching.is_empty() {
                            "CONTROL.NOT_FOUND"
                        } else {
                            "CONTROL.AMBIGUOUS"
                        },
                        "find must resolve exactly one observed node",
                    ));
                }
                let node = matching[0];
                Ok(
                    json!({"node": node, "target": Target { session_id: self.id.clone(), resource_id: observation.resource_id, observation_id: observation_id.clone(), node_id: node["node_id"].as_str().unwrap().into() }}),
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
                let node = observation.raw["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["node_id"] == target.node_id)
                    .ok_or_else(|| error("CONTROL.STALE_TARGET", "node was not observed"))?;
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
                let timeout = context.remaining()?;
                context.dispatched.store(true, Ordering::SeqCst);
                let result = backend.act(&observation.raw, node, action, timeout);
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
            .filter(|o| o.created.elapsed() < Duration::from_secs(300))
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
        Action::TypeText { text } => Some(text),
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
    Ok(())
}
