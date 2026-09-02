//! The workflow execution engine.
//!
//! The engine walks a compiled descriptor, resolves templates against a
//! well-defined scope, and executes each step through a [`Provider`].  Its
//! defining properties:
//!
//! * **Budgets are enforced, not advisory.**  A run stops at its wall-clock
//!   and step-count limits regardless of what the workflow asks for.
//! * **Effects are tracked.**  When an action's outcome is unprovable the run
//!   ends as `unknown_effect` rather than pretending it failed cleanly.
//! * **Cleanup always runs.**  `finally` blocks execute on every exit path,
//!   and a failure inside cleanup is suppressed onto the original error.

use crate::journal::{now_rfc3339, EventSink, Journal, RunResult, RunStatus};
use crate::provider::ProviderRegistry;
use crate::template;
use aad_core::model::{CompiledStep, ErrorHandler, HandlerMode, StepType};
use aad_core::{parse_duration, AutomationError, WorkflowDescriptor};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const RUNTIME_VERSION: &str = "0.1.0";

/// How execution should proceed after a step.
enum Flow {
    /// Continue with the next step.
    Next,
    /// A `return` step ended the workflow early.
    Return(Value),
}

/// Options for one run.
pub struct RunOptions {
    pub inputs: Map<String, Value>,
    pub providers: ProviderRegistry,
    pub sink: Option<Arc<dyn EventSink>>,
    pub run_id: Option<String>,
    /// Set from another thread to request cooperative cancellation.
    pub cancel: Arc<AtomicBool>,
    /// Overrides the descriptor's own wall-clock budget when smaller.
    pub max_duration: Option<Duration>,
    /// Where a script step's `entrypoint` is resolved from, normally the
    /// directory holding the descriptor.
    pub base_directory: std::path::PathBuf,
    /// Whether `script` steps may execute at all.
    ///
    /// Off by default. A `script` step runs arbitrary code from the descriptor
    /// with this process's privileges, so executing one has to be a decision the
    /// caller made deliberately rather than a side effect of the descriptor
    /// asking for it. A caller that never opts in cannot be talked into running
    /// code by the contents of a file it was handed.
    pub allow_scripts: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            inputs: Map::new(),
            providers: ProviderRegistry::new(),
            sink: None,
            run_id: None,
            cancel: Arc::new(AtomicBool::new(false)),
            max_duration: None,
            base_directory: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            allow_scripts: false,
        }
    }
}

impl RunOptions {
    pub fn with_providers(mut self, providers: ProviderRegistry) -> Self {
        self.providers = providers;
        self
    }

    pub fn with_inputs(mut self, inputs: Map<String, Value>) -> Self {
        self.inputs = inputs;
        self
    }

    pub fn with_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Resolve script entrypoints relative to this directory.
    pub fn with_base_directory(mut self, directory: std::path::PathBuf) -> Self {
        self.base_directory = directory;
        self
    }

    /// Permit `script` steps to execute.
    ///
    /// Only a caller that trusts the descriptor's code should call this.
    pub fn with_scripts_allowed(mut self, allowed: bool) -> Self {
        self.allow_scripts = allowed;
        self
    }
}

/// The mutable state of one run.
struct Run<'a> {
    descriptor: &'a WorkflowDescriptor,
    providers: ProviderRegistry,
    journal: Journal,
    inputs: Map<String, Value>,
    variables: Map<String, Value>,
    /// Per-step results, exposed to expressions as `steps.<id>.output`.
    steps: Map<String, Value>,
    /// Loop bindings, innermost last; consulted before the outer scope.
    loop_bindings: Vec<(String, Value)>,
    /// The error currently bound by an `on_error` handler.
    error_bindings: Vec<(String, Value)>,
    deadline: Instant,
    executed_steps: u64,
    max_executed_steps: u64,
    cancel: Arc<AtomicBool>,
    /// Where a script step's `entrypoint` is resolved from.
    base_directory: std::path::PathBuf,
    /// Whether `script` steps may execute; see [`RunOptions::allow_scripts`].
    allow_scripts: bool,
    /// Set once an action's outcome could not be proven.
    unknown_effect: bool,
}

impl<'a> Run<'a> {
    /// The variable scope visible to an expression at this point.
    fn scope(&self) -> Map<String, Value> {
        let mut scope = Map::new();
        scope.insert("inputs".into(), Value::Object(self.inputs.clone()));
        scope.insert("vars".into(), Value::Object(self.variables.clone()));
        scope.insert("steps".into(), Value::Object(self.steps.clone()));
        scope.insert(
            "runtime".into(),
            json!({"version": RUNTIME_VERSION, "run_id": self.journal.run_id()}),
        );
        // Inner bindings shadow outer ones.
        for (name, value) in &self.loop_bindings {
            scope.insert(name.clone(), value.clone());
        }
        for (name, value) in &self.error_bindings {
            scope.insert(name.clone(), value.clone());
        }
        scope
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Stop if the run was cancelled or has exhausted a budget.
    fn check_budget(&self) -> Result<(), AutomationError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(AutomationError::new(
                "WORKFLOW.CANCELLED",
                "the run was cancelled",
            )
            .with_category("workflow")
            .with_effect("unknown"));
        }
        if self.remaining().is_zero() {
            return Err(AutomationError::new(
                "WORKFLOW.TIMEOUT",
                "the run exceeded its maximum duration",
            )
            .with_category("workflow")
            .with_effect("unknown"));
        }
        if self.executed_steps >= self.max_executed_steps {
            return Err(AutomationError::new(
                "WORKFLOW.STEP_BUDGET_EXCEEDED",
                format!(
                    "the run exceeded its budget of {} executed steps",
                    self.max_executed_steps
                ),
            )
            .with_category("workflow")
            .with_effect("not_applied"));
        }
        Ok(())
    }

    /// Execute a list of sibling steps in dependency order.
    fn run_steps(&mut self, steps: &[CompiledStep]) -> Result<Flow, AutomationError> {
        for step in order_steps(steps) {
            match self.run_step(step)? {
                Flow::Next => continue,
                flow @ Flow::Return(_) => return Ok(flow),
            }
        }
        Ok(Flow::Next)
    }

    /// Execute one step, including its retry, handler and cleanup behaviour.
    fn run_step(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        self.check_budget()?;

        // A false `if` skips the step without consuming budget.
        if let Some(condition) = step.get("if") {
            if !template::condition(condition, &self.scope())? {
                self.journal
                    .emit("step.skipped", json!({"id": step.id, "path": step.path}));
                return Ok(Flow::Next);
            }
        }

        self.executed_steps += 1;
        self.journal.emit(
            "step.started",
            json!({"id": step.id, "type": step.step_type.as_str(), "path": step.path}),
        );

        let outcome = self.attempt_with_retry(step);

        // `finally` runs on every exit path, and its own failure is recorded
        // as suppressed rather than replacing the original error.
        let outcome = match self.run_cleanup(step, outcome) {
            Ok(flow) => Ok(flow),
            Err(error) => Err(error),
        };

        match outcome {
            Ok(flow) => {
                self.journal
                    .emit("step.finished", json!({"id": step.id, "status": "succeeded"}));
                Ok(flow)
            }
            Err(error) => {
                let located = error.at_step(
                    &step.id,
                    Some(&step.path),
                    None,
                    Some(&self.descriptor.name),
                );
                // An unprovable outcome must survive to the run's status.
                if located.effect == "unknown" {
                    self.unknown_effect = true;
                }
                match self.apply_handler(step, located) {
                    Ok(flow) => Ok(flow),
                    Err(error) => {
                        self.journal.emit(
                            "step.finished",
                            json!({
                                "id": step.id,
                                "status": "failed",
                                "error": error.to_json(),
                            }),
                        );
                        Err(error)
                    }
                }
            }
        }
    }

    fn run_cleanup(
        &mut self,
        step: &CompiledStep,
        outcome: Result<Flow, AutomationError>,
    ) -> Result<Flow, AutomationError> {
        if step.finally_steps.is_empty() {
            return outcome;
        }
        self.journal
            .emit("cleanup.started", json!({"id": step.id}));
        let cleanup = self.run_steps(&step.finally_steps);
        self.journal.emit(
            "cleanup.finished",
            json!({"id": step.id, "ok": cleanup.is_ok()}),
        );

        match (outcome, cleanup) {
            (Ok(flow), Ok(_)) => Ok(flow),
            // Cleanup failing on a successful path is itself the failure.
            (Ok(_), Err(error)) => Err(error),
            // Cleanup failing on an error path must not hide the real cause.
            (Err(mut original), Err(cleanup_error)) => {
                original.add_suppressed(cleanup_error);
                Err(original)
            }
            (Err(original), Ok(_)) => Err(original),
        }
    }

    /// Run a step, retrying while its policy and the remaining budget allow.
    fn attempt_with_retry(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        let policy = retry_policy(step, self.descriptor);
        let mut attempt = 1u32;

        loop {
            let result = self.dispatch(step, attempt);
            let Err(error) = result else {
                return result;
            };

            let retryable = policy.as_ref().is_some_and(|policy| {
                attempt < policy.max_attempts && policy.matches(&error) && error.retryable
            });
            if !retryable {
                return Err(error);
            }
            // A non-idempotent action whose outcome is unknown must never be
            // retried automatically: it may already have taken effect.
            if error.effect == "unknown" && !is_safely_repeatable(step) {
                return Err(error.with_detail(
                    "retry_suppressed",
                    Value::String("effect is unknown and the action is not idempotent".into()),
                ));
            }

            let delay = policy
                .as_ref()
                .map(|policy| policy.delay(attempt))
                .unwrap_or_default();
            let delay = delay.min(self.remaining());
            self.journal.emit(
                "step.retrying",
                json!({
                    "id": step.id,
                    "attempt": attempt,
                    "delay_seconds": delay.as_secs_f64(),
                    "error": error.to_json(),
                }),
            );
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            self.check_budget()?;
            attempt += 1;
        }
    }

    /// Execute the body of a step according to its type.
    fn dispatch(&mut self, step: &CompiledStep, attempt: u32) -> Result<Flow, AutomationError> {
        match step.step_type {
            StepType::Action => self.run_action(step, attempt),
            StepType::Set => self.run_set(step),
            StepType::If => self.run_if(step),
            StepType::Switch => self.run_switch(step),
            StepType::Foreach => self.run_foreach(step),
            StepType::While => self.run_while(step),
            StepType::Block => self.run_steps(&step.steps),
            StepType::Return => {
                let value = match step.get("value") {
                    Some(value) => template::resolve(value, &self.scope())?,
                    None => Value::Null,
                };
                Ok(Flow::Return(value))
            }
            StepType::Fail => Err(self.build_failure(step)?),
            StepType::Script => self.run_script(step),
        }
    }

    /// Run a `script` step in the platform sandbox.
    fn run_script(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        // Refused before the step is even parsed, so nothing in the descriptor
        // influences a run that never agreed to execute code.
        if !self.allow_scripts {
            return Err(AutomationError::new(
                "SCRIPT.SANDBOX_DENIED",
                "script steps are disabled for this run",
            )
            .with_category("script")
            .with_effect("not_applied")
            .with_detail(
                "remedy",
                Value::String(
                    "Scripts execute arbitrary code. Re-run with scripts enabled \
only if you trust this descriptor."
                        .into(),
                ),
            ));
        }
        let script = crate::script::ScriptStep::from_params(&step.params)?;

        // Script inputs are templated like any other step's arguments. The
        // descriptor spells this field `inputs`.
        let inputs = match step.get("inputs") {
            Some(value) => template::resolve(value, &self.scope())?,
            None => Value::Object(Map::new()),
        };

        let timeout = match step.get_str("timeout") {
            // `parse_duration` yields seconds; anything unparseable would
            // already have been rejected by the compiler.
            Some(text) => {
                let seconds = aad_core::parse_duration(text).ok_or_else(|| {
                    AutomationError::new("SCRIPT.INVALID", format!("invalid timeout {text:?}"))
                        .with_category("script")
                })?;
                Duration::from_secs_f64(seconds)
            }
            // Never outlive the run's own budget.
            None => self.remaining(),
        };

        let value =
            crate::script::execute(&script, &self.base_directory, &inputs, Some(timeout))?;
        // A script's result is recorded like an action's, so later steps can
        // read it as `steps.<id>.output`.
        self.steps.insert(
            step.id.clone(),
            json!({"output": value, "status": "succeeded"}),
        );
        Ok(Flow::Next)
    }

    fn run_action(&mut self, step: &CompiledStep, attempt: u32) -> Result<Flow, AutomationError> {
        let uses = step.get_str("uses").unwrap_or_default().to_string();
        let scope = self.scope();
        let args = match step.get("with") {
            Some(value) => template::resolve(value, &scope)?,
            None => Value::Object(Map::new()),
        };

        // A precondition is checked before the action is dispatched, so a
        // failure here provably has not applied anything.
        if let Some(Value::Object(precondition)) = step.get("precondition") {
            if let Some(condition) = precondition.get("condition") {
                if !template::condition(condition, &scope)? {
                    let message = precondition
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("precondition was not satisfied");
                    return Err(AutomationError::new("ACTION.PRECONDITION_FAILED", message)
                        .with_category("action")
                        .with_effect("not_applied"));
                }
            }
        }

        let Some((provider, contract)) = self.providers.resolve(&uses) else {
            return Err(AutomationError::new(
                "ACTION.UNKNOWN",
                format!("no provider offers action {uses:?}"),
            )
            .with_category("action")
            .with_effect("not_applied")
            .with_detail("uses", Value::String(uses.clone()))
            .with_detail(
                "available",
                json!(self.providers.names()),
            ));
        };
        let declared_effect = contract.effect_class.clone();

        let timeout = step
            .get_str("attempt_timeout")
            .or_else(|| step.get_str("timeout"))
            .and_then(parse_duration)
            .map(Duration::from_secs_f64)
            .unwrap_or_else(|| self.remaining())
            .min(self.remaining());

        self.journal.emit(
            "action.started",
            json!({"id": step.id, "uses": uses, "attempt": attempt}),
        );
        let started = Instant::now();
        let result = provider.invoke(&uses, args, Some(timeout));
        let elapsed = started.elapsed().as_secs_f64();

        match result {
            Ok(output) => {
                self.journal.emit(
                    "action.finished",
                    json!({"id": step.id, "uses": uses, "duration_seconds": elapsed}),
                );
                self.record_output(step, output.clone());

                // A postcondition observes the world after the fact; failing it
                // means the action may well have applied.
                if let Some(Value::Object(postcondition)) = step.get("postcondition") {
                    self.check_postcondition(postcondition)?;
                }
                Ok(Flow::Next)
            }
            Err(mut error) => {
                // A read-only action cannot have changed anything, even when
                // the transport outcome was ambiguous.
                if declared_effect.as_deref() == Some("read_only") && error.effect == "unknown" {
                    error.effect = "not_applied".to_string();
                }
                self.journal.emit(
                    "action.failed",
                    json!({
                        "id": step.id,
                        "uses": uses,
                        "duration_seconds": elapsed,
                        "error": error.to_json(),
                    }),
                );
                Err(error)
            }
        }
    }

    /// Check an action's postcondition, re-observing the world until it holds.
    ///
    /// Without this, "the action was dispatched" and "the action worked" are the
    /// same answer. A click that landed on nothing, a value that was rejected by
    /// the form, a dialog that never appeared: all report success. That matters
    /// most for the MCP caller, which has no eyes on the screen and takes
    /// `succeeded` at face value.
    ///
    /// Two decisions worth stating:
    ///
    /// * `observe` is dispatched on every attempt, and its result is bound to
    ///   `observation` for the condition to read. Re-reading the *stored* output
    ///   of the action would only ever confirm what was already recorded, which
    ///   is precisely the thing under suspicion.
    /// * without a `timeout` the condition is evaluated exactly once. Desktop
    ///   UI is asynchronous, so polling is what makes an assertion usable at
    ///   all -- but silently waiting when no wait was asked for would turn a
    ///   fast failure into a slow one.
    fn check_postcondition(
        &mut self,
        postcondition: &Map<String, Value>,
    ) -> Result<(), AutomationError> {
        let Some(condition) = postcondition.get("condition").cloned() else {
            return Ok(());
        };
        let message = postcondition
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("postcondition was not satisfied")
            .to_string();
        let observe = postcondition.get("observe").cloned();

        let timeout = postcondition
            .get("timeout")
            .and_then(Value::as_str)
            .and_then(parse_duration)
            .map(Duration::from_secs_f64);
        // Matches the Python runtime's default rather than inventing one, so a
        // descriptor that omits it behaves the same on both.
        let interval = postcondition
            .get("poll_interval")
            .and_then(Value::as_str)
            .and_then(parse_duration)
            .map(Duration::from_secs_f64)
            .unwrap_or_else(|| Duration::from_millis(100));

        // Never poll past the run's own budget.
        let deadline = timeout.map(|window| {
            let capped = window.min(self.remaining());
            Instant::now() + capped
        });

        let mut last_observation = Value::Null;
        loop {
            let scope = match &observe {
                Some(observe) => {
                    last_observation = self.observe_for_postcondition(observe)?;
                    let mut scope = self.scope();
                    scope.insert("observation".into(), last_observation.clone());
                    scope
                }
                None => self.scope(),
            };

            if template::condition(&condition, &scope)? {
                return Ok(());
            }

            let expired = deadline.is_none_or(|deadline| Instant::now() >= deadline);
            if expired {
                let mut error = AutomationError::new("ACTION.POSTCONDITION_FAILED", &message)
                    .with_category("action")
                    // The action itself succeeded; only the expected outcome is
                    // missing. Whether the desktop changed is genuinely unknown,
                    // and claiming otherwise in either direction would be a guess.
                    .with_effect("unknown");
                if observe.is_some() {
                    error = error.with_detail("last_observation", last_observation);
                }
                return Err(error);
            }

            // Cancellation and budget exhaustion must interrupt a wait, not be
            // discovered after it.
            self.check_budget()?;
            let remaining = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(interval);
            std::thread::sleep(interval.min(remaining));
        }
    }

    /// Dispatch a postcondition's observation action.
    ///
    /// Restricted to read-only actions: an assertion that changes the thing it
    /// is checking cannot establish anything, and a write hidden in a
    /// postcondition would also bypass the risk and confirmation checks that
    /// apply to a real action step.
    fn observe_for_postcondition(&mut self, observe: &Value) -> Result<Value, AutomationError> {
        let uses = observe
            .get("uses")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AutomationError::new(
                    "ACTION.POSTCONDITION_INVALID",
                    "postcondition.observe requires a uses",
                )
                .with_category("action")
                .with_effect("not_applied")
            })?
            .to_string();

        let scope = self.scope();
        let args = match observe.get("with") {
            Some(value) => template::resolve(value, &scope)?,
            None => Value::Object(Map::new()),
        };

        let Some((provider, contract)) = self.providers.resolve(&uses) else {
            return Err(AutomationError::new(
                "ACTION.UNKNOWN",
                format!("no provider offers action {uses:?}"),
            )
            .with_category("action")
            .with_effect("not_applied")
            .with_detail("uses", Value::String(uses.clone()))
            .with_detail("available", json!(self.providers.names())));
        };

        if contract.effect_class.as_deref() != Some("read_only") {
            return Err(AutomationError::new(
                "POLICY.DENIED",
                "postcondition observation must be read-only",
            )
            .with_category("policy")
            .with_effect("not_applied")
            .with_detail("uses", Value::String(uses.clone()))
            .with_detail(
                "effect",
                contract
                    .effect_class
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            ));
        }

        let timeout = self.remaining();
        provider.invoke(&uses, args, Some(timeout)).map_err(|error| {
            // An observation that failed leaves the caller no worse off: it read
            // nothing and changed nothing.
            error.with_effect("not_applied")
        })
    }

    fn record_output(&mut self, step: &CompiledStep, output: Value) {
        self.steps
            .insert(step.id.clone(), json!({"output": output}));
    }

    fn run_set(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        let Some(Value::Object(assign)) = step.get("assign") else {
            return Ok(Flow::Next);
        };
        let scope = self.scope();
        // Evaluate every value against the pre-assignment scope so the order
        // of keys within one `set` cannot change the outcome.
        let mut resolved = Vec::new();
        for (target, value) in assign {
            let name = target.strip_prefix("vars.").unwrap_or(target).to_string();
            resolved.push((name, template::resolve(value, &scope)?));
        }
        for (name, value) in resolved {
            self.variables.insert(name, value);
        }
        Ok(Flow::Next)
    }

    fn run_if(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        let taken = match step.get("condition") {
            Some(condition) => template::condition(condition, &self.scope())?,
            None => false,
        };
        if taken {
            self.run_steps(&step.then_steps)
        } else {
            self.run_steps(&step.else_steps)
        }
    }

    fn run_switch(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        for case in &step.cases {
            let matched = match &case.when {
                Some(when) => template::condition(when, &self.scope())?,
                None => false,
            };
            if matched {
                return self.run_steps(&case.steps);
            }
        }
        self.run_steps(&step.default_steps)
    }

    fn run_foreach(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        let items = match step.get("items") {
            Some(value) => template::resolve(value, &self.scope())?,
            None => Value::Array(Vec::new()),
        };
        let Value::Array(items) = items else {
            return Err(AutomationError::new(
                "LOOP.ITEMS_NOT_ITERABLE",
                "foreach items must evaluate to an array",
            )
            .with_category("loop")
            .with_effect("not_applied"));
        };

        let binding = step.get_str("as").unwrap_or("item").to_string();
        let index_binding = step.get_str("index_as").map(str::to_string);
        // The compiler guarantees max_items exists; it bounds an otherwise
        // unbounded loop over caller-supplied data.
        let limit = step.get("max_items").and_then(Value::as_u64).unwrap_or(0);
        if items.len() as u64 > limit {
            return Err(AutomationError::new(
                "LOOP.MAX_ITEMS_EXCEEDED",
                format!("foreach received {} items but max_items is {limit}", items.len()),
            )
            .with_category("loop")
            .with_effect("not_applied"));
        }

        for (index, item) in items.iter().enumerate() {
            self.check_budget()?;
            self.loop_bindings.push((binding.clone(), item.clone()));
            if let Some(name) = &index_binding {
                self.loop_bindings.push((name.clone(), json!(index)));
            }
            let outcome = self.run_steps(&step.steps);
            self.loop_bindings.pop();
            if index_binding.is_some() {
                self.loop_bindings.pop();
            }
            match outcome? {
                Flow::Next => continue,
                flow @ Flow::Return(_) => return Ok(flow),
            }
        }
        Ok(Flow::Next)
    }

    fn run_while(&mut self, step: &CompiledStep) -> Result<Flow, AutomationError> {
        let limit = step
            .get("max_iterations")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut iteration = 0u64;

        loop {
            self.check_budget()?;
            let proceed = match step.get("condition") {
                Some(condition) => template::condition(condition, &self.scope())?,
                None => false,
            };
            if !proceed {
                return Ok(Flow::Next);
            }
            if iteration >= limit {
                // Stopping loudly is safer than silently truncating: the
                // workflow's own exit condition never became true.
                return Err(AutomationError::new(
                    "LOOP.MAX_ITERATIONS_EXCEEDED",
                    format!("while loop exceeded {limit} iterations"),
                )
                .with_category("loop")
                .with_effect("unknown"));
            }
            iteration += 1;
            match self.run_steps(&step.steps)? {
                Flow::Next => continue,
                flow @ Flow::Return(_) => return Ok(flow),
            }
        }
    }

    fn build_failure(&self, step: &CompiledStep) -> Result<AutomationError, AutomationError> {
        let Some(Value::Object(spec)) = step.get("error") else {
            return Ok(AutomationError::new("WORKFLOW.FAILED", "the workflow failed"));
        };
        let scope = self.scope();
        let code = spec
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("WORKFLOW.FAILED")
            .to_string();
        let message = match spec.get("message") {
            Some(value) => template::resolve(value, &scope)?
                .as_str()
                .unwrap_or("the workflow failed")
                .to_string(),
            None => "the workflow failed".to_string(),
        };

        let mut error = AutomationError::new(code, message)
            .with_retryable(spec.get("retryable") == Some(&Value::Bool(true)))
            .with_effect(
                spec.get("effect")
                    .and_then(Value::as_str)
                    .unwrap_or("not_applied"),
            );
        if let Some(category) = spec.get("category").and_then(Value::as_str) {
            error = error.with_category(category);
        }
        if let Some(details) = spec.get("details") {
            if let Value::Object(map) = template::resolve(details, &scope)? {
                error = error.with_details(map);
            }
        }
        Ok(error)
    }

    /// Give a matching `on_error` handler the chance to absorb the failure.
    fn apply_handler(
        &mut self,
        step: &CompiledStep,
        error: AutomationError,
    ) -> Result<Flow, AutomationError> {
        let Some(handler) = &step.on_error else {
            return Err(error);
        };
        if !handler.matches(&error.code, &error.category, &error.effect) {
            return Err(error);
        }
        self.run_handler(handler.clone(), error, &step.id)
    }

    fn run_handler(
        &mut self,
        handler: ErrorHandler,
        error: AutomationError,
        step_id: &str,
    ) -> Result<Flow, AutomationError> {
        self.journal.emit(
            "handler.started",
            json!({"id": step_id, "mode": handler.mode.as_str(), "error": error.to_json()}),
        );
        self.error_bindings
            .push((handler.as_name.clone(), error.to_json()));

        let outcome = (|| -> Result<Flow, AutomationError> {
            if let Flow::Return(value) = self.run_steps(&handler.steps)? {
                return Ok(Flow::Return(value));
            }
            match handler.mode {
                HandlerMode::Rethrow => Err(error.clone()),
                HandlerMode::Continue => {
                    if let Some(output) = &handler.output {
                        let resolved = template::resolve(output, &self.scope())?;
                        self.steps
                            .insert(step_id.to_string(), json!({"output": resolved}));
                    }
                    Ok(Flow::Next)
                }
                HandlerMode::Return => {
                    let value = match &handler.output {
                        Some(output) => template::resolve(output, &self.scope())?,
                        None => Value::Null,
                    };
                    Ok(Flow::Return(value))
                }
            }
        })();

        self.error_bindings.pop();
        self.journal.emit(
            "handler.finished",
            json!({"id": step_id, "absorbed": outcome.is_ok()}),
        );
        outcome
    }

    fn workflow_outputs(&self) -> Result<Map<String, Value>, AutomationError> {
        let scope = self.scope();
        let mut outputs = Map::new();
        for (name, definition) in &self.descriptor.outputs {
            if let Some(value) = &definition.value {
                outputs.insert(name.clone(), template::resolve(value, &scope)?);
            }
        }
        Ok(outputs)
    }
}

/// A step's effective retry policy.
struct RetryPolicy {
    max_attempts: u32,
    initial_delay: Duration,
    max_delay: Option<Duration>,
    multiplier: f64,
    exponential: bool,
    codes: Vec<String>,
    categories: Vec<String>,
}

impl RetryPolicy {
    fn matches(&self, error: &AutomationError) -> bool {
        let code_ok = self.codes.is_empty()
            || self.codes.iter().any(|pattern| match pattern.as_str() {
                "*" => true,
                pattern => match pattern.strip_suffix('*') {
                    Some(prefix) => error.code.starts_with(prefix),
                    None => pattern == error.code,
                },
            });
        let category_ok = self.categories.is_empty()
            || self.categories.iter().any(|value| *value == error.category);
        code_ok && category_ok
    }

    fn delay(&self, attempt: u32) -> Duration {
        let mut delay = if self.exponential {
            self.initial_delay
                .mul_f64(self.multiplier.powi(attempt as i32 - 1))
        } else {
            self.initial_delay
        };
        if let Some(maximum) = self.max_delay {
            delay = delay.min(maximum);
        }
        delay
    }
}

fn retry_policy(step: &CompiledStep, descriptor: &WorkflowDescriptor) -> Option<RetryPolicy> {
    let raw = step
        .get("retry")
        .or_else(|| descriptor.defaults.get("retry"))?;
    let Value::Object(policy) = raw else {
        return None;
    };
    let backoff = policy.get("backoff").and_then(Value::as_object);
    let on = policy.get("on").and_then(Value::as_object);
    let strings = |key: &str| -> Vec<String> {
        on.and_then(|value| value.get(key))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    Some(RetryPolicy {
        max_attempts: policy.get("max_attempts").and_then(Value::as_u64).unwrap_or(1) as u32,
        initial_delay: backoff
            .and_then(|value| value.get("initial_delay"))
            .and_then(Value::as_str)
            .and_then(parse_duration)
            .map(Duration::from_secs_f64)
            .unwrap_or_default(),
        max_delay: backoff
            .and_then(|value| value.get("max_delay"))
            .and_then(Value::as_str)
            .and_then(parse_duration)
            .map(Duration::from_secs_f64),
        multiplier: backoff
            .and_then(|value| value.get("multiplier"))
            .and_then(Value::as_f64)
            .unwrap_or(2.0),
        exponential: backoff
            .and_then(|value| value.get("strategy"))
            .and_then(Value::as_str)
            == Some("exponential"),
        codes: strings("codes"),
        categories: strings("categories"),
    })
}

/// Whether repeating this step cannot cause additional side effects.
fn is_safely_repeatable(step: &CompiledStep) -> bool {
    if step.step_type != StepType::Action {
        return true;
    }
    matches!(
        step.get("effect")
            .and_then(Value::as_object)
            .and_then(|effect| effect.get("class"))
            .and_then(Value::as_str),
        Some("read_only" | "idempotent")
    )
}

/// Order sibling steps so every dependency precedes its dependents.
///
/// The compiler has already proven the graph is acyclic and that each
/// dependency exists in this scope, so a stable topological sort is enough;
/// declaration order breaks ties to keep runs reproducible.
fn order_steps(steps: &[CompiledStep]) -> Vec<&CompiledStep> {
    let mut remaining: Vec<&CompiledStep> = steps.iter().collect();
    let mut ordered: Vec<&CompiledStep> = Vec::with_capacity(steps.len());
    let mut done: std::collections::HashSet<&str> = std::collections::HashSet::new();

    while !remaining.is_empty() {
        let ready = remaining.iter().position(|step| {
            step.depends_on
                .iter()
                .all(|dependency| done.contains(dependency.as_str()))
        });
        match ready {
            Some(index) => {
                let step = remaining.remove(index);
                done.insert(step.id.as_str());
                ordered.push(step);
            }
            // Defensive: a cycle should be impossible after compilation.
            None => {
                ordered.extend(remaining.drain(..));
            }
        }
    }
    ordered
}

/// A stable digest of the compiled plan, used to detect descriptor drift.
pub fn plan_digest(descriptor: &WorkflowDescriptor) -> String {
    let canonical = canonical_json(&descriptor.raw);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Serialize with object keys sorted, so equal plans hash equally.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|key| format!("{}:{}", Value::String((*key).clone()), canonical_json(&map[*key])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// Prepare inputs by applying declared defaults and rejecting missing ones.
fn prepare_inputs(
    descriptor: &WorkflowDescriptor,
    supplied: &Map<String, Value>,
) -> Result<Map<String, Value>, AutomationError> {
    let mut inputs = Map::new();
    let mut missing = Vec::new();

    for (name, definition) in &descriptor.inputs {
        if let Some(value) = supplied.get(name) {
            inputs.insert(name.clone(), value.clone());
        } else if let Some(default) = &definition.default {
            inputs.insert(name.clone(), default.clone());
        } else if definition.required {
            missing.push(name.clone());
        }
    }
    if !missing.is_empty() {
        return Err(AutomationError::new(
            "INPUT.MISSING",
            format!("required inputs are missing: {}", missing.join(", ")),
        )
        .with_category("input")
        .with_phase("prepare")
        .with_effect("not_applied")
        .with_detail("missing", json!(missing)));
    }

    // An undeclared input is a typo far more often than an intention.
    let undeclared: Vec<String> = supplied
        .keys()
        .filter(|name| !descriptor.inputs.contains_key(*name))
        .cloned()
        .collect();
    if !undeclared.is_empty() {
        return Err(AutomationError::new(
            "INPUT.UNDECLARED",
            format!("inputs are not declared: {}", undeclared.join(", ")),
        )
        .with_category("input")
        .with_phase("prepare")
        .with_effect("not_applied")
        .with_detail("undeclared", json!(undeclared)));
    }
    Ok(inputs)
}

fn prepare_variables(descriptor: &WorkflowDescriptor) -> Map<String, Value> {
    descriptor
        .variables
        .iter()
        .map(|(name, definition)| {
            (
                name.clone(),
                definition.initial.clone().unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// A run's resumable state, everything needed to continue after a restart.
///
/// Deliberately plain data: it has to survive being written to a journal and
/// read back by a different process, so it holds no handles, no `Instant` and
/// nothing tied to this process's lifetime.
#[derive(Clone, Debug, Default)]
pub struct SegmentState {
    pub variables: Map<String, Value>,
    /// Step outputs visible to expressions as `steps.<id>.output`.
    pub steps: Map<String, Value>,
    /// The index of the next top-level step to execute.
    pub next_index: usize,
    pub executed_steps: u64,
    /// Sticky: once an outcome could not be proven it must reach the status.
    pub unknown_effect: bool,
    /// Set when a `return` step ended the workflow early.
    pub returned: Option<Value>,
}

/// How far a segment got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    /// A top-level step ran; more may remain.
    Advanced,
    /// A `return` ended the workflow body early.
    Returned,
    /// Every top-level step has been executed.
    Exhausted,
}

/// A run driven one top-level step at a time.
///
/// This exists so a durable executor can persist a checkpoint between steps and
/// resume in a **different process**. It shares the same [`Run`] internals as
/// [`run`], so a segmented run cannot drift from an ordinary one in how it
/// resolves scopes, enforces budgets or handles errors.
pub struct Segmented<'a> {
    state: Run<'a>,
    next_index: usize,
    returned: Option<Value>,
    started: Instant,
    started_at: String,
    digest: String,
}

impl<'a> Segmented<'a> {
    /// Begin a run, stopping before the first top-level step.
    ///
    /// `deadline_epoch` is wall-clock rather than an `Instant` because the
    /// budget has to outlive the process: a run paused for an hour has spent an
    /// hour of it, and resuming must not silently hand it a fresh allowance.
    pub fn begin(
        descriptor: &'a WorkflowDescriptor,
        options: &RunOptions,
        deadline_epoch: f64,
    ) -> Result<Self, AutomationError> {
        let run_id = options
            .run_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let inputs = prepare_inputs(descriptor, &options.inputs)?;
        Ok(Self {
            next_index: 0,
            returned: None,
            started: Instant::now(),
            started_at: now_rfc3339(),
            digest: plan_digest(descriptor),
            state: Run {
                descriptor,
                providers: options.providers.clone(),
                journal: Journal::new(run_id, options.sink.clone()),
                inputs,
                variables: prepare_variables(descriptor),
                steps: Map::new(),
                loop_bindings: Vec::new(),
                error_bindings: Vec::new(),
                deadline: deadline_instant(deadline_epoch),
                executed_steps: 0,
                max_executed_steps: descriptor.budgets.max_executed_steps,
                cancel: options.cancel.clone(),
                base_directory: options.base_directory.clone(),
                allow_scripts: options.allow_scripts,
                unknown_effect: false,
            },
        })
    }

    /// Restore a run from a checkpoint written by an earlier process.
    pub fn restore(
        descriptor: &'a WorkflowDescriptor,
        options: &RunOptions,
        deadline_epoch: f64,
        state: SegmentState,
    ) -> Result<Self, AutomationError> {
        let mut resumed = Self::begin(descriptor, options, deadline_epoch)?;
        resumed.state.variables = state.variables;
        resumed.state.steps = state.steps;
        resumed.state.executed_steps = state.executed_steps;
        // Sticky across restarts: an unprovable effect from before the crash
        // must still decide the final status.
        resumed.state.unknown_effect = state.unknown_effect;
        resumed.next_index = state.next_index;
        resumed.returned = state.returned;
        Ok(resumed)
    }

    /// The state to persist before executing the next segment.
    pub fn snapshot(&self) -> SegmentState {
        SegmentState {
            variables: self.state.variables.clone(),
            steps: self.state.steps.clone(),
            next_index: self.next_index,
            executed_steps: self.state.executed_steps,
            unknown_effect: self.state.unknown_effect,
            returned: self.returned.clone(),
        }
    }

    pub fn run_id(&self) -> &str {
        self.state.journal.run_id()
    }

    pub fn plan_digest(&self) -> &str {
        &self.digest
    }

    /// The id of the next top-level step, or `None` when the body is done.
    pub fn next_step_id(&self) -> Option<&str> {
        if self.returned.is_some() {
            return None;
        }
        self.ordered()
            .get(self.next_index)
            .map(|step| step.id.as_str())
    }

    /// Top-level steps in the order they will execute.
    ///
    /// Resolved the same way as an ordinary run so an index recorded in a
    /// checkpoint always names the same step.
    fn ordered(&self) -> Vec<&'a CompiledStep> {
        order_steps(&self.state.descriptor.steps)
    }

    /// Execute exactly one top-level step.
    pub fn run_segment(&mut self) -> Result<Segment, AutomationError> {
        if self.returned.is_some() {
            return Ok(Segment::Returned);
        }
        let ordered = self.ordered();
        let Some(step) = ordered.get(self.next_index) else {
            return Ok(Segment::Exhausted);
        };
        // Advance first: a step that panics or whose process dies mid-flight
        // must not be silently retried as though it had never started. The
        // durable layer decides what to do with an interrupted segment.
        self.next_index += 1;
        match self.state.run_step(step)? {
            Flow::Next => Ok(Segment::Advanced),
            Flow::Return(value) => {
                self.returned = Some(value);
                Ok(Segment::Returned)
            }
        }
    }

    /// Whether an unprovable effect has been seen.
    pub fn unknown_effect(&self) -> bool {
        self.state.unknown_effect
    }

    pub fn emit(&self, event_type: &str, payload: Value) {
        self.state.journal.emit(event_type, payload);
    }

    /// Run the workflow handler and `finally` blocks, then build the result.
    ///
    /// `body` is the outcome of the segments so far. Cleanup runs on every path,
    /// including cancellation, exactly as in an ordinary run.
    ///
    /// Takes `&mut self` rather than consuming the run so a caller holding it
    /// behind a mutable borrow can finalise in place; calling it twice would run
    /// cleanup twice, so callers must not.
    pub fn finish(&mut self, body: Result<(), AutomationError>) -> RunResult {
        let mut outcome = body;

        if let (Err(error), Some(handler)) = (&outcome, &self.state.descriptor.on_error) {
            if handler.matches(&error.code, &error.category, &error.effect) {
                let error = error.clone();
                outcome = self
                    .state
                    .run_handler(handler.clone(), error, "$workflow")
                    .map(|_| ());
            }
        }

        if !self.state.descriptor.finally_steps.is_empty() {
            self.state
                .journal
                .emit("cleanup.started", json!({"id": "$workflow"}));
            let finally_steps = &self.state.descriptor.finally_steps;
            let cleanup = self.state.run_steps(finally_steps);
            self.state.journal.emit(
                "cleanup.finished",
                json!({"id": "$workflow", "ok": cleanup.is_ok()}),
            );
            outcome = match (outcome, cleanup) {
                (Ok(()), Ok(_)) => Ok(()),
                (Ok(()), Err(error)) => Err(error),
                (Err(mut original), Err(cleanup_error)) => {
                    original.add_suppressed(cleanup_error);
                    Err(original)
                }
                (Err(original), Ok(_)) => Err(original),
            };
        }

        let outputs = match &outcome {
            Ok(()) => self.state.workflow_outputs().unwrap_or_default(),
            Err(_) => Map::new(),
        };
        let (status, error) = match outcome {
            Ok(()) => (RunStatus::Succeeded, None),
            Err(error) => {
                let status = match error.code.as_str() {
                    "WORKFLOW.TIMEOUT" => RunStatus::TimedOut,
                    "WORKFLOW.CANCELLED" => RunStatus::Cancelled,
                    _ if error.effect == "unknown" || self.state.unknown_effect => {
                        RunStatus::UnknownEffect
                    }
                    _ => RunStatus::Failed,
                };
                (status, Some(error))
            }
        };

        self.state.providers.close_all();
        RunResult {
            run_id: self.state.journal.run_id().to_string(),
            workflow: self.state.descriptor.name.clone(),
            plan_digest: self.digest.clone(),
            status,
            outputs,
            error,
            executed_steps: self.state.executed_steps,
            duration_seconds: self.started.elapsed().as_secs_f64(),
            started_at: self.started_at.clone(),
            finished_at: now_rfc3339(),
            events: self.state.journal.events(),
        }
    }
}

/// Convert an absolute wall-clock deadline into this process's monotonic one.
///
/// The stored deadline is wall-clock so it survives a restart; the engine checks
/// budgets against a monotonic `Instant` so a clock adjustment mid-run cannot
/// extend or curtail it. This converts once, at the boundary.
fn deadline_instant(deadline_epoch: f64) -> Instant {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0);
    let remaining = deadline_epoch - now;
    if remaining <= 0.0 {
        // Already spent. Not an error here: the caller reports the timeout with
        // the context only it has.
        Instant::now()
    } else {
        Instant::now() + Duration::from_secs_f64(remaining)
    }
}

/// Execute a compiled workflow to completion.
pub fn run(descriptor: &WorkflowDescriptor, options: RunOptions) -> RunResult {
    let run_id = options
        .run_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let started_at = now_rfc3339();
    let started = Instant::now();
    let digest = plan_digest(descriptor);

    let budget = Duration::from_secs_f64(descriptor.budgets.max_duration);
    let budget = options.max_duration.map_or(budget, |limit| limit.min(budget));

    let journal = Journal::new(run_id.clone(), options.sink.clone());
    journal.emit(
        "run.started",
        json!({
            "workflow": descriptor.name,
            "plan_digest": digest,
            "runtime_version": RUNTIME_VERSION,
            "budgets": {
                "max_duration_seconds": budget.as_secs_f64(),
                "max_executed_steps": descriptor.budgets.max_executed_steps,
            },
        }),
    );

    let inputs = match prepare_inputs(descriptor, &options.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            journal.emit("run.failed", json!({"error": error.to_json()}));
            return RunResult {
                run_id,
                workflow: descriptor.name.clone(),
                plan_digest: digest,
                status: RunStatus::Failed,
                outputs: Map::new(),
                error: Some(error),
                executed_steps: 0,
                duration_seconds: started.elapsed().as_secs_f64(),
                started_at,
                finished_at: now_rfc3339(),
                events: journal.events(),
            };
        }
    };

    let mut state = Run {
        descriptor,
        providers: options.providers.clone(),
        journal,
        inputs,
        variables: prepare_variables(descriptor),
        steps: Map::new(),
        loop_bindings: Vec::new(),
        error_bindings: Vec::new(),
        deadline: started + budget,
        executed_steps: 0,
        max_executed_steps: descriptor.budgets.max_executed_steps,
        cancel: options.cancel.clone(),
        base_directory: options.base_directory.clone(),
        allow_scripts: options.allow_scripts,
        unknown_effect: false,
    };

    let mut outcome = state.run_steps(&descriptor.steps).map(|_| ());

    // A workflow-level handler gets the same chance as a step-level one.
    if let (Err(error), Some(handler)) = (&outcome, &descriptor.on_error) {
        if handler.matches(&error.code, &error.category, &error.effect) {
            let error = error.clone();
            outcome = state
                .run_handler(handler.clone(), error, "$workflow")
                .map(|_| ());
        }
    }

    // Workflow `finally` runs on every path, including cancellation.
    if !descriptor.finally_steps.is_empty() {
        state.journal.emit("cleanup.started", json!({"id": "$workflow"}));
        let cleanup = state.run_steps(&descriptor.finally_steps);
        state.journal.emit(
            "cleanup.finished",
            json!({"id": "$workflow", "ok": cleanup.is_ok()}),
        );
        outcome = match (outcome, cleanup) {
            (Ok(()), Ok(_)) => Ok(()),
            (Ok(()), Err(error)) => Err(error),
            (Err(mut original), Err(cleanup_error)) => {
                original.add_suppressed(cleanup_error);
                Err(original)
            }
            (Err(original), Ok(_)) => Err(original),
        };
    }

    let outputs = match &outcome {
        Ok(()) => state.workflow_outputs().unwrap_or_default(),
        Err(_) => Map::new(),
    };

    let (status, error) = match outcome {
        Ok(()) => (RunStatus::Succeeded, None),
        Err(error) => {
            let status = match error.code.as_str() {
                "WORKFLOW.TIMEOUT" => RunStatus::TimedOut,
                "WORKFLOW.CANCELLED" => RunStatus::Cancelled,
                // An unprovable effect is never reported as a clean failure.
                _ if error.effect == "unknown" || state.unknown_effect => {
                    RunStatus::UnknownEffect
                }
                _ => RunStatus::Failed,
            };
            (status, Some(error))
        }
    };

    state.journal.emit(
        "run.finished",
        json!({
            "status": status.as_str(),
            "executed_steps": state.executed_steps,
            "duration_seconds": started.elapsed().as_secs_f64(),
            "error": error.as_ref().map(AutomationError::to_json).unwrap_or(Value::Null),
        }),
    );
    options.providers.close_all();

    RunResult {
        run_id,
        workflow: descriptor.name.clone(),
        plan_digest: digest,
        status,
        outputs,
        error,
        executed_steps: state.executed_steps,
        duration_seconds: started.elapsed().as_secs_f64(),
        started_at,
        finished_at: now_rfc3339(),
        events: state.journal.events(),
    }
}
