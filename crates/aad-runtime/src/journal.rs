//! The run journal: an append-only, continuously sequenced event stream.
//!
//! Every observable thing the engine does becomes an event.  The sequence
//! numbers start at 1 and never skip, so a consumer can detect a truncated
//! journal, and the shape matches the `RunEvent` schema used by the previous
//! implementation so existing tooling keeps working.

use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const API_VERSION: &str = "ai-auto-desktop.dev/v1alpha1";
pub const EVENT_KIND: &str = "RunEvent";
pub const RUN_KIND: &str = "Run";

/// One immutable journal event.
#[derive(Clone, Debug)]
pub struct RunEvent {
    pub run_id: String,
    pub seq: u64,
    pub event_type: String,
    pub payload: Value,
    pub created_at: String,
}

impl RunEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "apiVersion": API_VERSION,
            "kind": EVENT_KIND,
            "runId": self.run_id,
            "seq": self.seq,
            "type": self.event_type,
            "payload": self.payload,
            "createdAt": self.created_at,
        })
    }
}

/// Anything that consumes journal events as they are produced.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &RunEvent);
}

/// A sink that discards everything.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: &RunEvent) {}
}

/// A sink that writes one JSON object per line to a writer.
pub struct NdjsonSink<W: std::io::Write + Send> {
    writer: Mutex<W>,
}

impl<W: std::io::Write + Send> NdjsonSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }
}

impl<W: std::io::Write + Send> EventSink for NdjsonSink<W> {
    fn emit(&self, event: &RunEvent) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writeln!(writer, "{}", event.to_json());
            let _ = writer.flush();
        }
    }
}

/// Collects the journal in memory and forwards it to an optional sink.
pub struct Journal {
    run_id: String,
    events: Mutex<Vec<RunEvent>>,
    sink: Option<Arc<dyn EventSink>>,
}

impl Journal {
    pub fn new(run_id: impl Into<String>, sink: Option<Arc<dyn EventSink>>) -> Self {
        Self {
            run_id: run_id.into(),
            events: Mutex::new(Vec::new()),
            sink,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Append an event, assigning the next sequence number.
    pub fn emit(&self, event_type: &str, payload: Value) {
        let Ok(mut events) = self.events.lock() else {
            return;
        };
        let event = RunEvent {
            run_id: self.run_id.clone(),
            seq: events.len() as u64 + 1,
            event_type: event_type.to_string(),
            payload,
            created_at: now_rfc3339(),
        };
        if let Some(sink) = &self.sink {
            sink.emit(&event);
        }
        events.push(event);
    }

    pub fn events(&self) -> Vec<RunEvent> {
        self.events.lock().map(|events| events.clone()).unwrap_or_default()
    }

    pub fn to_json(&self) -> Vec<Value> {
        self.events().iter().map(RunEvent::to_json).collect()
    }
}

/// The terminal state of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    /// The run stopped without being able to prove whether an action applied.
    UnknownEffect,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::UnknownEffect => "unknown_effect",
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// Everything a finished run produced.
#[derive(Clone, Debug)]
pub struct RunResult {
    pub run_id: String,
    pub workflow: String,
    pub plan_digest: String,
    pub status: RunStatus,
    pub outputs: Map<String, Value>,
    pub error: Option<aad_core::AutomationError>,
    pub executed_steps: u64,
    pub duration_seconds: f64,
    pub started_at: String,
    pub finished_at: String,
    pub events: Vec<RunEvent>,
}

impl RunResult {
    /// The `Run` document, matching the runtime schema.
    pub fn to_json(&self) -> Value {
        json!({
            "apiVersion": API_VERSION,
            "kind": RUN_KIND,
            "runId": self.run_id,
            "workflow": {"name": self.workflow, "planDigest": self.plan_digest},
            "status": self.status.as_str(),
            "desiredState": "run",
            "inputs": {},
            "output": Value::Object(self.outputs.clone()),
            "error": self
                .error
                .as_ref()
                .map(aad_core::AutomationError::to_json)
                .unwrap_or(Value::Null),
            "createdAt": self.started_at,
            "updatedAt": self.finished_at,
            "finishedAt": self.finished_at,
        })
    }

    /// A compact summary for CLI and MCP consumers.
    pub fn summary(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "workflow": self.workflow,
            "plan_digest": self.plan_digest,
            "status": self.status.as_str(),
            "outputs": Value::Object(self.outputs.clone()),
            "error": self
                .error
                .as_ref()
                .map(aad_core::AutomationError::to_json)
                .unwrap_or(Value::Null),
            "executed_steps": self.executed_steps,
            "duration_seconds": self.duration_seconds,
        })
    }
}

/// An RFC 3339 timestamp in UTC, computed without a date-time dependency.
pub fn now_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_seconds = now.as_secs();
    let millis = now.subsec_millis();

    let days = (total_seconds / 86_400) as i64;
    let seconds_of_day = total_seconds % 86_400;
    let (hour, minute, second) = (
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_start_at_one_and_never_skip() {
        let journal = Journal::new("run-1", None);
        journal.emit("run.started", json!({}));
        journal.emit("step.started", json!({"id": "a"}));
        journal.emit("run.finished", json!({}));

        let sequences: Vec<u64> = journal.events().iter().map(|event| event.seq).collect();
        assert_eq!(sequences, vec![1, 2, 3]);
    }

    #[test]
    fn an_event_matches_the_runevent_schema_shape() {
        let journal = Journal::new("run-7", None);
        journal.emit("step.finished", json!({"id": "a"}));

        let event = journal.to_json().remove(0);
        assert_eq!(event["apiVersion"], API_VERSION);
        assert_eq!(event["kind"], "RunEvent");
        assert_eq!(event["runId"], "run-7");
        assert_eq!(event["seq"], 1);
        assert_eq!(event["type"], "step.finished");
        assert_eq!(event["payload"], json!({"id": "a"}));
        assert!(event["createdAt"].as_str().is_some());
    }

    #[test]
    fn events_reach_an_attached_sink_immediately() {
        struct Counting(Mutex<Vec<String>>);
        impl EventSink for Counting {
            fn emit(&self, event: &RunEvent) {
                self.0.lock().unwrap().push(event.event_type.clone());
            }
        }

        let sink = Arc::new(Counting(Mutex::new(Vec::new())));
        let journal = Journal::new("run-2", Some(sink.clone()));
        journal.emit("a.b", json!({}));
        journal.emit("c.d", json!({}));

        assert_eq!(*sink.0.lock().unwrap(), vec!["a.b".to_string(), "c.d".to_string()]);
    }

    #[test]
    fn the_ndjson_sink_writes_one_object_per_line() {
        #[derive(Clone)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let shared = Arc::new(Mutex::new(Vec::new()));
        let journal = Journal::new("run-3", Some(Arc::new(NdjsonSink::new(Buffer(shared.clone())))));
        journal.emit("one", json!({}));
        journal.emit("two", json!({}));

        let text = String::from_utf8(shared.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_str::<Value>(line).expect("each line is valid JSON");
        }
    }

    #[test]
    fn timestamps_are_rfc3339_in_utc() {
        let stamp = now_rfc3339();
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.len(), 24, "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
    }

    #[test]
    fn the_civil_calendar_conversion_is_correct_at_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 2024 is a leap year, so day 60 of that year is 29 February.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
