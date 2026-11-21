//! Opt-in, single-writer reconciliation of interrupted provider calls.
mod store;

use crate::engine::{digest_json, run_inner, validate_schema};
use crate::{plan_digest, RunOptions, RunResult, RUNTIME_VERSION};
use aad_core::model::{CompiledStep, StepType};
use aad_core::{AutomationError, WorkflowDescriptor};
use aad_plugin::manifest::ActionContract;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use store::Store;
use uuid::Uuid;

type Result<T> = std::result::Result<T, AutomationError>;

fn invalid(message: impl std::fmt::Display) -> AutomationError {
    AutomationError::new("RECOVERY.REFUSED", message.to_string()).with_effect("unknown")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    path: PathBuf,
    run_id: String,
    workflow: String,
    runtime: String,
    plan: String,
    inputs: String,
    business_key: Option<(String, String)>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    ticket: String,
    step: String,
    uses: String,
    fingerprint: String,
    key: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Started(Header),
    Dispatch(Intent),
    Completed {
        ticket: String,
        output: Value,
        receipt: Option<String>,
    },
    Retry {
        ticket: String,
        replacement: String,
        receipt: String,
    },
    Finished,
}

#[derive(Default)]
struct History {
    header: Option<Header>,
    calls: Vec<(Intent, Option<Value>)>,
    keys: BTreeSet<(String, String)>,
    reconciliations: Vec<Value>,
    finished: bool,
}

impl History {
    fn pending(&self) -> Option<&Intent> {
        self.calls
            .last()
            .filter(|(_, output)| output.is_none())
            .map(|(intent, _)| intent)
    }

    fn apply(&mut self, event: Event) -> Result<()> {
        if self.finished {
            return Err(invalid("events after terminal recovery record"));
        }
        match event {
            Event::Started(header) if self.header.is_none() => self.header = Some(header),
            _ if self.header.is_none() => return Err(invalid("missing recovery header")),
            Event::Dispatch(intent) if self.pending().is_none() => {
                if let Some(key) = &intent.key {
                    if !self.keys.insert((intent.step.clone(), key.clone())) {
                        return Err(invalid("repeated business key for this step"));
                    }
                }
                self.calls.push((intent, None));
            }
            Event::Completed {
                ticket,
                output,
                receipt,
            } if self.pending().is_some_and(|intent| intent.ticket == ticket) => {
                if let Some(reference) = receipt {
                    let intent = self.pending().unwrap();
                    self.reconciliations.push(json!({"step":intent.step,"key":intent.key,"ticket":ticket,"decision":"applied","receipt":reference}));
                }
                self.calls.last_mut().unwrap().1 = Some(output);
            }
            Event::Retry {
                ticket,
                replacement,
                receipt,
            } if self.pending().is_some_and(|intent| intent.ticket == ticket) => {
                let intent = self.pending().unwrap();
                self.reconciliations.push(json!({"step":intent.step,"key":intent.key,"ticket":ticket,"decision":"not_applied","receipt":receipt}));
                self.calls.last_mut().unwrap().0.ticket = replacement;
            }
            Event::Finished if self.pending().is_none() => self.finished = true,
            _ => return Err(invalid("inconsistent recovery journal transition")),
        }
        Ok(())
    }
}

fn load(store: &mut Store, path: &Path) -> Result<History> {
    let mut history = History::default();
    store.read(|event| history.apply(serde_json::from_value(event).map_err(invalid)?))?;
    if history.header.as_ref().unwrap().path != path.canonicalize().map_err(invalid)? {
        return Err(invalid(
            "use the original recovery journal at its original path",
        ));
    }
    Ok(history)
}

pub fn review(path: &Path) -> Result<Value> {
    let history = load(&mut Store::open(path, false, true)?, path)?;
    let header = history.header.as_ref().unwrap();
    Ok(
        json!({"kind":"RecoveryReview","run_id":header.run_id,"workflow":header.workflow,"plan_digest":header.plan,
        "status":if history.finished {"succeeded"} else if history.pending().is_some() {"needs_receipt"} else {"between_actions"},
        "completed_actions":history.calls.iter().filter(|(_,output)|output.is_some()).count(),"reconciliations":history.reconciliations,
        "pending":history.pending().map(|intent|json!({"step":intent.step,"uses":intent.uses,"ticket":intent.ticket,"key":intent.key}))}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    step: String,
    receipt: String,
    decision: String,
    run_id: Option<String>,
    ticket: Option<String>,
    key: Option<String>,
    #[serde(default, deserialize_with = "present")]
    output: Option<Value>,
}

fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn evidence(history: &History, raw: Option<Value>) -> Result<Option<Receipt>> {
    let Some(intent) = history.pending() else {
        return if raw.is_none() {
            Ok(None)
        } else {
            Err(invalid("no pending action to reconcile"))
        };
    };
    let raw = raw.ok_or_else(|| {
        invalid("pending action: review the journal and provide an independent receipt")
    })?;
    let receipt = if raw.is_array() {
        let rows: Vec<Receipt> = serde_json::from_value(raw).map_err(invalid)?;
        let mut matches = rows
            .into_iter()
            .filter(|row| row.step == intent.step && row.key.is_some() && row.key == intent.key);
        let row = matches.next().ok_or_else(|| {
            invalid("no matching business receipt; absence is not proof of non-submission")
        })?;
        if matches.next().is_some()
            || row.decision != "applied"
            || row.run_id.is_some()
            || row.ticket.is_some()
        {
            return Err(invalid(
                "business lookup requires one unambiguous applied receipt",
            ));
        }
        row
    } else {
        let row: Receipt = serde_json::from_value(raw).map_err(invalid)?;
        if row.step != intent.step
            || row.run_id.as_ref() != history.header.as_ref().map(|h| &h.run_id)
            || row.ticket.as_ref() != Some(&intent.ticket)
            || row.key.is_some()
        {
            return Err(invalid(
                "receipt is not for the pending run, step and dispatch ticket",
            ));
        }
        row
    };
    if receipt.receipt.trim().is_empty()
        || receipt.receipt.len() > 512
        || !matches!(
            (receipt.decision.as_str(), receipt.output.is_some()),
            ("applied", true) | ("not_applied", false)
        )
    {
        return Err(invalid("supply a receipt identifier and an applied output, or an explicit not_applied decision"));
    }
    Ok(Some(receipt))
}

pub(crate) struct Ledger {
    store: Store,
    history: History,
    cursor: usize,
    receipt: Option<Receipt>,
}

impl Ledger {
    fn append(&mut self, event: Event) -> Result<()> {
        let raw = serde_json::to_value(&event).map_err(invalid)?;
        self.history.apply(event)?;
        self.store.append(raw)
    }

    pub(crate) fn dispatch(
        &mut self,
        step: &CompiledStep,
        scope: &Map<String, Value>,
        args: &Value,
        contract: &ActionContract,
        invoke: impl FnOnce() -> Result<Value>,
    ) -> Result<Value> {
        let uses = step.get_str("uses").unwrap_or_default();
        let header = self.history.header.as_ref().unwrap();
        let key = match &header.business_key {
            Some((id, pointer)) if id == &step.id => {
                let bindings = Value::Object(scope.clone());
                let value = bindings.pointer(pointer).and_then(Value::as_str).filter(|key| !key.trim().is_empty() && key.len() <= 512)
                    .ok_or_else(||invalid("business-key pointer must resolve to a nonempty string in the step scope"))?;
                Some(value.to_string())
            }
            _ => None,
        };
        let fingerprint = digest_json(&json!([step.path, uses, contract.raw, args, key]));
        let mut receipt_id = None;
        let mut supplied = None;
        let ticket = if let Some((intent, prior)) = self.history.calls.get(self.cursor) {
            if intent.fingerprint != fingerprint
                || intent.step != step.id
                || intent.uses != uses
                || intent.key != key
            {
                return Err(invalid(
                    "action sequence, contract or resolved arguments diverged",
                ));
            }
            if let Some(output) = prior {
                self.cursor += 1;
                return Ok(output.clone());
            }
            let ticket = intent.ticket.clone();
            let receipt = self
                .receipt
                .take()
                .ok_or_else(|| invalid("unconfirmed dispatch must not be repeated"))?;
            if let Some(output) = receipt.output {
                supplied = Some(output);
                receipt_id = Some(receipt.receipt);
                ticket
            } else {
                let replacement = Uuid::new_v4().to_string();
                self.append(Event::Retry {
                    ticket,
                    replacement: replacement.clone(),
                    receipt: receipt.receipt,
                })?;
                replacement
            }
        } else {
            let ticket = Uuid::new_v4().to_string();
            self.append(Event::Dispatch(Intent {
                ticket: ticket.clone(),
                step: step.id.clone(),
                uses: uses.into(),
                fingerprint,
                key,
            }))?;
            ticket
        };
        let output = match supplied {
            Some(value) => value,
            None => invoke()?,
        };
        validate_schema(
            &output,
            contract.output_schema.as_ref(),
            "RECOVERY.INVALID_OUTPUT",
            uses,
            true,
        )?;
        self.append(Event::Completed {
            ticket,
            output: output.clone(),
            receipt: receipt_id,
        })?;
        self.cursor += 1;
        Ok(output)
    }
}

/// Run with a fresh journal or resume its original plan under a single exclusive writer.
pub fn execute(
    descriptor: &WorkflowDescriptor,
    mut options: RunOptions,
    path: &Path,
    create: bool,
    business_key: Option<(&str, &str)>,
    receipt: Option<Value>,
) -> Result<RunResult> {
    if descriptor.on_error.is_some()
        || !descriptor.finally_steps.is_empty()
        || descriptor.defaults.contains_key("retry")
        || descriptor
            .inputs
            .values()
            .chain(descriptor.outputs.values())
            .chain(descriptor.variables.values())
            .any(|value| value.sensitive)
    {
        return Err(invalid(
            "recovery refuses sensitive values, retries, handlers and cleanup",
        ));
    }
    for step in descriptor.all_steps() {
        if step.step_type == StepType::Script
            || step.on_error.is_some()
            || !step.finally_steps.is_empty()
            || ["retry", "postcondition"]
                .iter()
                .any(|key| step.get(key).is_some())
        {
            return Err(invalid(
                "recovery refuses scripts, retries, handlers, cleanup and action postconditions",
            ));
        }
        if let Some(uses) = step.get_str("uses") {
            let (_, contract) = options
                .providers
                .resolve(uses)
                .ok_or_else(|| invalid(format!("provider not available for {uses}")))?;
            let sensitive = |value: &Value| {
                ["input", "output", "error"]
                    .iter()
                    .any(|name| value.get(name).is_some_and(|field| field != "public"))
            };
            if contract.has_artifacts()
                || contract.sensitivity.as_ref().is_some_and(sensitive)
                || step.get("sensitivity").is_some_and(sensitive)
            {
                return Err(invalid(
                    "artifact and sensitive actions cannot be journalled",
                ));
            }
        }
    }
    if let Some((id, pointer)) = business_key {
        if !create
            || !pointer.starts_with('/')
            || !descriptor
                .all_steps()
                .iter()
                .any(|step| step.id == id && step.step_type == StepType::Action)
        {
            return Err(invalid(
                "business key must name an original action step and a JSON pointer",
            ));
        }
    }
    if create && receipt.is_some() {
        return Err(invalid("a new run takes no receipt"));
    }
    let mut store = Store::open(path, create, false)?;
    let inputs = digest_json(&Value::Object(options.inputs.clone()));
    let plan = plan_digest(descriptor);
    let mut history = if create {
        History::default()
    } else {
        load(&mut store, path)?
    };
    if create {
        let event = Event::Started(Header {
            run_id: Uuid::new_v4().to_string(),
            path: path.canonicalize().map_err(invalid)?,
            workflow: descriptor.name.clone(),
            runtime: RUNTIME_VERSION.into(),
            plan: plan.clone(),
            inputs: inputs.clone(),
            business_key: business_key.map(|(id, key)| (id.into(), key.into())),
        });
        store.append(serde_json::to_value(&event).map_err(invalid)?)?;
        history.apply(event)?;
    }
    let header = history.header.as_ref().unwrap();
    if history.finished
        || header.plan != plan
        || header.inputs != inputs
        || header.runtime != RUNTIME_VERSION
        || header.workflow != descriptor.name
    {
        return Err(invalid(
            "run is terminal or workflow, supplied inputs or runtime differ",
        ));
    }
    options.run_id = Some(header.run_id.clone());
    let receipt = evidence(&history, receipt)?;
    let mut ledger = Ledger {
        store,
        history,
        receipt,
        cursor: 0,
    };
    let result = run_inner(descriptor, options, Some(&mut ledger));
    if result.status.is_success() {
        if ledger.cursor != ledger.history.calls.len() || ledger.receipt.is_some() {
            return Err(invalid("run did not consume its original dispatch history"));
        }
        ledger.append(Event::Finished)?;
    }
    Ok(result)
}
