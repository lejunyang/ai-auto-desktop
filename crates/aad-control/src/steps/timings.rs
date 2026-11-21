use crate::Operation;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default, Serialize)]
struct Phase {
    calls: u64,
    failed_calls: u64,
    elapsed_us: u64,
}

#[derive(Default, Serialize)]
struct Totals {
    targets: Phase,
    observe: Phase,
    find: Phase,
    act: Phase,
    read_fields: Phase,
    collect_table: Phase,
}

/// Fixed-size counters for a single run of control steps. Request contents are never stored.
#[derive(Default)]
pub struct StepTimings(Mutex<Totals>);

impl StepTimings {
    pub fn snapshot(&self) -> Value {
        json!(*self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(super) fn record(&self, op: &Operation, elapsed: Duration, failed: bool) {
        let mut totals = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let phase = match op {
            Operation::Targets { .. } => &mut totals.targets,
            Operation::Observe { .. } => &mut totals.observe,
            Operation::Find { .. } => &mut totals.find,
            Operation::Act { .. } => &mut totals.act,
            Operation::ReadFields { .. } => &mut totals.read_fields,
            Operation::CollectTable { .. } => &mut totals.collect_table,
            _ => return,
        };
        phase.calls = phase.calls.saturating_add(1);
        phase.failed_calls = phase.failed_calls.saturating_add(u64::from(failed));
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        phase.elapsed_us = phase.elapsed_us.saturating_add(micros);
    }
}
