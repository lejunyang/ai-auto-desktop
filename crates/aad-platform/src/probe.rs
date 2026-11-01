//! What a probe of the system's prerequisites reports, check by check. The
//! platform crates make the checks of their own system; `aad-probe` puts
//! them in a report.
use serde_json::{json, Map, Value};

/// The four states a prerequisite observation can report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Nothing observable prevents the capability from working.
    Available,
    /// Partially present, or present but with a caveat.
    Degraded,
    /// Observably absent.
    Unavailable,
    /// Could not be determined; absence of evidence, not evidence of absence.
    Unknown,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Available => "available",
            State::Degraded => "degraded",
            State::Unavailable => "unavailable",
            State::Unknown => "unknown",
        }
    }

    pub const ALL: [State; 4] = [
        State::Available,
        State::Degraded,
        State::Unavailable,
        State::Unknown,
    ];
}

/// One narrowly scoped prerequisite observation.
#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub state: State,
    pub summary: String,
    pub evidence: Map<String, Value>,
}

impl Check {
    pub fn new(name: &str, state: State, summary: &str, evidence: Value) -> Self {
        Self {
            name: name.to_string(),
            state,
            summary: summary.to_string(),
            evidence: evidence.as_object().cloned().unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "summary": self.summary,
            "evidence": Value::Object(self.evidence.clone()),
        })
    }
}
