//! Driver-side lifecycle for ambient observation sessions.
//!
//! Platform independent apart from opening the native session, which the
//! driver does through [`ObserveSource`]. This is to observation what the
//! `watch`/`collect`/`release` actions are to recording, but kept separate:
//! their buffers, lifetime and event vocabulary do not overlap.

use crate::backend::{DriverError, Result};
use crate::observe::ObservedEvent;
use aad_runtime::journal::now_rfc3339;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// How many observed events one poll takes at a time.
pub const MAX_POLLED_EVENTS: usize = 1_024;

/// A running native observation subscription.
pub trait ObserveSource: Send {
    /// Take the events observed so far and how many were dropped.
    fn drain(&self, max: usize) -> (Vec<ObservedEvent>, u64);
    /// Which native mechanisms are installed (`uia`, `winevent`).
    fn sources(&self) -> Vec<String>;
}

/// Holds the observation sessions for one driver instance.
pub(crate) struct ObserveManager {
    sessions: Mutex<HashMap<String, Box<dyn ObserveSource>>>,
    /// Recorded at start so a poll can attribute events to a session even
    /// before the first foreground switch.
    metadata: Mutex<HashMap<String, SessionMetadata>>,
}

#[derive(Clone)]
struct SessionMetadata {
    sources: Vec<String>,
}

impl ObserveManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            metadata: Mutex::new(HashMap::new()),
        }
    }

    /// Open a session backed by a native subscription.
    pub fn start(&self, source: Box<dyn ObserveSource>) -> Value {
        let sources = source.sources();
        let observe_id = uuid::Uuid::new_v4().simple().to_string();
        self.sessions
            .lock()
            .unwrap()
            .insert(observe_id.clone(), source);
        self.metadata.lock().unwrap().insert(
            observe_id.clone(),
            SessionMetadata {
                sources: sources.clone(),
            },
        );
        json!({
            "observe_id": observe_id,
            "sources": sources,
            "started_at": now_rfc3339(),
        })
    }

    /// Take the events observed so far, leaving the session running.
    pub fn poll(&self, args: &Value) -> Result<Value> {
        let observe_id = required(args, "observe_id")?;
        let max = args
            .get("max")
            .and_then(Value::as_u64)
            .unwrap_or(MAX_POLLED_EVENTS as u64)
            .clamp(1, MAX_POLLED_EVENTS as u64) as usize;

        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(&observe_id).ok_or_else(|| not_found(&observe_id))?;
        let (events, dropped) = session.drain(max);
        let sources = self
            .metadata
            .lock()
            .unwrap()
            .get(&observe_id)
            .map(|metadata| metadata.sources.clone())
            .unwrap_or_default();

        let events_json: Vec<Value> = events
            .into_iter()
            .map(|event| {
                let mut value = event.to_json();
                if let Some(object) = value.as_object_mut() {
                    object.insert("at".into(), json!(now_rfc3339()));
                }
                value
            })
            .collect();

        Ok(json!({
            "observe_id": observe_id,
            "sources": sources,
            "events": events_json,
            "count": events_json.len(),
            // A silently dropped event is indistinguishable from the user
            // doing nothing, so overflow is always surfaced.
            "dropped_events": dropped,
        }))
    }

    /// End a session; dropping the source removes the hooks and handlers.
    pub fn stop(&self, args: &Value) -> Result<Value> {
        let observe_id = required(args, "observe_id")?;
        let removed = self.sessions.lock().unwrap().remove(&observe_id);
        self.metadata.lock().unwrap().remove(&observe_id);
        match removed {
            Some(_) => Ok(json!({"stopped": true, "observe_id": observe_id})),
            None => Err(not_found(&observe_id)),
        }
    }
}

impl Default for ObserveManager {
    fn default() -> Self {
        Self::new()
    }
}

fn required<'a>(args: &'a Value, name: &str) -> Result<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| DriverError::invalid(format!("{name} is required")))
}

fn not_found(observe_id: &str) -> DriverError {
    DriverError::new(
        "DRIVER.OBSERVE_NOT_FOUND",
        format!("no observation session {observe_id:?}"),
    )
    .with_detail("observe_id", json!(observe_id))
}

/// The observe actions' capability contracts. Observing installs hooks but
/// changes nothing, so they sit in the same risk class as the recorder's
/// watch/release: `idempotent` for start/stop, `read_only` for poll.
pub fn contracts(actions: &mut Map<String, Value>) {
    actions.insert(
        "observe_start".into(),
        json!({
            "contract_major": 1,
            "summary": "Start a desktop-wide observation session reporting foreground switches and in-app changes.",
            "effect": {"default_class": "idempotent"},
            "risk": {"category": "observe", "level": "low"},
        }),
    );
    actions.insert(
        "observe_poll".into(),
        json!({
            "contract_major": 1,
            "summary": "Take the events observed so far by an observation session.",
            "effect": {"default_class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
        }),
    );
    actions.insert(
        "observe_stop".into(),
        json!({
            "contract_major": 1,
            "summary": "Stop an observation session and remove its hooks.",
            "effect": {"default_class": "idempotent"},
            "risk": {"category": "observe", "level": "low"},
        }),
    );
}

#[cfg(windows)]
impl ObserveSource for crate::observer_win::ObserverSession {
    fn drain(&self, max: usize) -> (Vec<ObservedEvent>, u64) {
        self.drain(max)
    }

    fn sources(&self) -> Vec<String> {
        self.sources.iter().map(|name| name.to_string()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::{EventWindow, ObservationKind};
    use std::time::Instant;

    struct FakeSource {
        events: Mutex<Vec<ObservedEvent>>,
        dropped: u64,
    }

    impl FakeSource {
        fn with(events: Vec<ObservedEvent>, dropped: u64) -> Self {
            Self {
                events: Mutex::new(events),
                dropped,
            }
        }
    }

    impl ObserveSource for FakeSource {
        fn drain(&self, _max: usize) -> (Vec<ObservedEvent>, u64) {
            let taken = std::mem::take(&mut *self.events.lock().unwrap());
            (taken, self.dropped)
        }

        fn sources(&self) -> Vec<String> {
            vec!["fake".into()]
        }
    }

    fn sample(kind: ObservationKind) -> ObservedEvent {
        ObservedEvent {
            sequence: 0,
            kind,
            source: "fake",
            window: EventWindow::unknown(),
            element: None,
            observed_at: Instant::now(),
        }
    }

    #[test]
    fn start_poll_stop_round_trip() {
        let manager = ObserveManager::new();
        let started = manager.start(Box::new(FakeSource::with(
            vec![sample(ObservationKind::ForegroundChanged)],
            2,
        )));
        let observe_id = started["observe_id"].as_str().unwrap().to_string();
        assert_eq!(started["sources"], json!(["fake"]));

        let polled = manager
            .poll(&json!({"observe_id": observe_id}))
            .expect("poll finds the session");
        assert_eq!(polled["count"], 1);
        assert_eq!(polled["dropped_events"], 2);
        assert_eq!(polled["events"][0]["kind"], "foreground_changed");
        assert!(polled["events"][0]["at"].is_string());

        let stopped = manager
            .stop(&json!({"observe_id": observe_id}))
            .expect("stop ends the session");
        assert_eq!(stopped["stopped"], true);
        assert!(manager.poll(&json!({"observe_id": observe_id})).is_err());
    }

    #[test]
    fn unknown_session_is_a_clear_error() {
        let manager = ObserveManager::new();
        let error = manager
            .poll(&json!({"observe_id": "nope"}))
            .expect_err("no such session");
        assert_eq!(error.code, "DRIVER.OBSERVE_NOT_FOUND");
    }
}
