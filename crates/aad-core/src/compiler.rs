//! Strict compiler for canonical v1alpha1 workflow descriptors.
//!
//! Validation is fail-closed: unknown core fields are rejected, every
//! expression is parsed at compile time, and each step list is checked as an
//! independent DAG scope.  All problems are collected so one run reports every
//! issue instead of only the first.

use crate::errors::{AutomationError, DescriptorIssue, Result};
use crate::expression::compile_expression;
use crate::model::*;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const API_VERSION: &str = "ai-auto-desktop.dev/v1alpha1";
pub const KIND: &str = "Workflow";
pub const MAX_DESCRIPTOR_BYTES: usize = 2 * 1024 * 1024;

const TOP_FIELDS: &[&str] = &[
    "apiVersion", "kind", "metadata", "requires", "inputs", "variables", "outputs", "defaults",
    "budgets", "policy", "steps", "on_error", "finally", "extensions",
];
const COMMON_FIELDS: &[&str] = &[
    "id", "type", "depends_on", "description", "if", "timeout", "attempt_timeout", "retry",
    "on_error", "finally", "extensions",
];
const EFFECT_CLASSES: &[&str] = &["read_only", "idempotent", "non_idempotent", "contextual"];
const RISK_CATEGORIES: &[&str] = &[
    "observe", "navigate", "input", "modify", "send", "delete", "purchase", "authorize", "install",
    "execute_script", "capture_screen", "custom",
];
const RISK_LEVELS: &[&str] = &["low", "medium", "high", "critical", "contextual"];

fn type_fields(step_type: &str) -> Option<&'static [&'static str]> {
    Some(match step_type {
        "action" => &[
            "uses", "with", "effect", "risk", "precondition", "postcondition", "sensitivity",
            "checkpoint",
        ],
        "set" => &["assign"],
        "if" => &["condition", "then", "else"],
        "switch" => &["cases", "default"],
        "foreach" => &["items", "as", "index_as", "max_items", "concurrency", "steps"],
        "while" => &["condition", "max_iterations", "steps"],
        "block" => &["steps"],
        "script" => &[
            "runtime", "source", "entrypoint", "inputs", "output_schema", "sandbox",
        ],
        "fail" => &["error"],
        "return" => &["value"],
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Identifier and template patterns, hand-rolled to avoid a regex dependency
// ---------------------------------------------------------------------------

/// `^[a-z][a-z0-9_]{0,63}$`
fn is_step_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// `^[A-Za-z_][A-Za-z0-9_]{0,127}$`
fn is_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// `^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$`
fn is_dotted_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut previous_separator = false;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => previous_separator = false,
            b'.' | b'_' | b'-' => {
                if index == 0 || index + 1 == bytes.len() || previous_separator {
                    return false;
                }
                previous_separator = true;
            }
            _ => return false,
        }
    }
    !previous_separator
}

/// `capability.action@major`, e.g. `desktop.windows_uia.snapshot@1`.
fn is_uses(value: &str) -> bool {
    let Some((path, major)) = value.rsplit_once('@') else {
        return false;
    };
    if major.is_empty()
        || major.starts_with('0')
        || !major.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let Some((provider, action)) = path.rsplit_once('.') else {
        return false;
    };
    if !is_dotted_name(provider) {
        return false;
    }
    let bytes = action.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// An uppercase dotted error code such as `OCR.LOW_CONFIDENCE`.
fn is_error_code(value: &str) -> bool {
    let segments: Vec<&str> = value.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            let bytes = segment.as_bytes();
            !bytes.is_empty()
                && bytes[0].is_ascii_uppercase()
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        })
}

/// Locate every `${{ ... }}` template inside a string.
fn template_spans(text: &str) -> Vec<(usize, usize, &str)> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index + 3 < bytes.len() {
        if bytes[index] == b'$' && bytes[index + 1] == b'{' && bytes[index + 2] == b'{' {
            if let Some(offset) = text[index + 3..].find("}}") {
                let start = index + 3;
                let end = start + offset;
                spans.push((index, end + 2, &text[start..end]));
                index = end + 2;
                continue;
            }
        }
        index += 1;
    }
    spans
}

/// Whether the whole string is exactly one template.
fn whole_template(text: &str) -> Option<&str> {
    let trimmed = text.strip_prefix("${{")?.strip_suffix("}}")?;
    // A string like "${{a}} ${{b}}" must not be treated as a single template.
    if trimmed.contains("}}") {
        return None;
    }
    Some(trimmed.trim())
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

struct Compiler {
    issues: Vec<DescriptorIssue>,
    /// Step id to the path where it was first declared, for duplicate reports.
    ids: HashMap<String, String>,
    /// Each independent DAG scope, recorded for post-pass validation.
    scopes: Vec<(String, Vec<ScopeEntry>)>,
    source: Option<PathBuf>,
}

/// The scope-level view of a step needed to validate dependencies.
#[derive(Clone, Debug)]
struct ScopeEntry {
    id: String,
    path: String,
    depends_on: Vec<String>,
    references: BTreeSet<String>,
}

impl Compiler {
    fn new(source: Option<PathBuf>) -> Self {
        Self {
            issues: Vec::new(),
            ids: HashMap::new(),
            scopes: Vec::new(),
            source,
        }
    }

    fn issue(&mut self, path: impl Into<String>, message: impl Into<String>, code: &str) {
        self.issues.push(DescriptorIssue::new(path, message, code));
    }

    fn object<'a>(&mut self, value: Option<&'a Value>, path: &str) -> Option<&'a Map<String, Value>> {
        match value {
            Some(Value::Object(map)) => Some(map),
            _ => {
                self.issue(path, "must be an object", "type");
                None
            }
        }
    }

    fn array<'a>(&mut self, value: Option<&'a Value>, path: &str) -> Option<&'a Vec<Value>> {
        match value {
            Some(Value::Array(items)) => Some(items),
            _ => {
                self.issue(path, "must be an array", "type");
                None
            }
        }
    }

    fn unknown(&mut self, object: &Map<String, Value>, allowed: &[&str], path: &str) {
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                self.issue(format!("{path}.{key}"), "unknown field", "unknown_field");
            }
        }
    }

    fn required(&mut self, object: &Map<String, Value>, fields: &[&str], path: &str) {
        for field in fields {
            if !object.contains_key(*field) {
                self.issue(
                    format!("{path}.{field}"),
                    "required field is missing",
                    "required",
                );
            }
        }
    }

    fn duration(&mut self, value: Option<&Value>, path: &str) {
        match value.and_then(Value::as_str).and_then(parse_duration) {
            Some(_) => {}
            None => self.issue(
                path,
                "must be a duration such as 250ms or 2s",
                "format",
            ),
        }
    }

    fn strings(&mut self, value: Option<&Value>, path: &str) {
        let Some(items) = self.array(value, path) else {
            return;
        };
        let invalid: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| !matches!(item, Value::String(text) if !text.is_empty()))
            .map(|(index, _)| index)
            .collect();
        for index in invalid {
            self.issue(
                format!("{path}[{index}]"),
                "must be a non-empty string",
                "type",
            );
        }
    }

    /// A field that must be exactly one complete `${{ ... }}` expression.
    fn expression(&mut self, value: Option<&Value>, path: &str) {
        let Some(Value::String(text)) = value else {
            self.issue(path, "must be an expression string", "type");
            return;
        };
        let Some(inner) = whole_template(text) else {
            self.issue(path, "must be one complete expression template", "expression");
            return;
        };
        if let Err(error) = compile_expression(inner) {
            self.issue(path, error.to_string(), "expression");
        }
    }

    /// Any value that may contain interpolated templates at any depth.
    fn values(&mut self, value: &Value, path: &str) {
        match value {
            Value::String(text) => {
                for (_, _, inner) in template_spans(text) {
                    if let Err(error) = compile_expression(inner.trim()) {
                        self.issue(path, error.to_string(), "expression");
                    }
                }
                if text.contains("${{") && template_spans(text).is_empty() {
                    self.issue(path, "unterminated expression", "expression");
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    self.values(item, &format!("{path}.{key}"));
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    self.values(item, &format!("{path}[{index}]"));
                }
            }
            _ => {}
        }
    }

    /// Statically resolvable `steps.<id>` references inside any value.
    fn step_references(&self, value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::String(text) => {
                for (_, _, inner) in template_spans(text) {
                    if let Ok(expression) = compile_expression(inner.trim()) {
                        found.extend(expression.step_references());
                    }
                }
            }
            Value::Object(map) => map
                .values()
                .for_each(|item| self.step_references(item, found)),
            Value::Array(items) => items
                .iter()
                .for_each(|item| self.step_references(item, found)),
            _ => {}
        }
    }

    /// Statically resolvable `inputs.<name>` references inside any value.
    fn input_references(&self, value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::String(text) => {
                for (_, _, inner) in template_spans(text) {
                    if let Ok(expression) = compile_expression(inner.trim()) {
                        found.extend(expression.input_references());
                    }
                }
            }
            Value::Object(map) => map
                .values()
                .for_each(|item| self.input_references(item, found)),
            Value::Array(items) => items
                .iter()
                .for_each(|item| self.input_references(item, found)),
            _ => {}
        }
    }

    // -- scope validation ---------------------------------------------------

    /// Validate every DAG scope: unknown/self/cross-scope dependencies, cycles,
    /// and expression references that were never declared as dependencies.
    fn validate_scopes(&mut self) {
        let scopes = std::mem::take(&mut self.scopes);
        for (scope_path, entries) in &scopes {
            let by_id: HashMap<&str, &ScopeEntry> = entries
                .iter()
                .map(|entry| (entry.id.as_str(), entry))
                .collect();

            for entry in entries {
                for (index, dependency) in entry.depends_on.iter().enumerate() {
                    let path = format!("{}.depends_on[{index}]", entry.path);
                    if dependency == &entry.id {
                        self.issue(path, "step cannot depend on itself", "self_dependency");
                    } else if !by_id.contains_key(dependency.as_str()) {
                        if self.ids.contains_key(dependency) {
                            self.issue(
                                path,
                                format!(
                                    "dependency '{dependency}' is outside sibling scope {scope_path}"
                                ),
                                "cross_scope_dependency",
                            );
                        } else {
                            self.issue(
                                path,
                                format!("unknown sibling dependency '{dependency}'"),
                                "unknown_dependency",
                            );
                        }
                    }
                }
            }

            let cyclic = find_cycles(entries, &by_id);
            for entry in entries {
                if cyclic.contains(&entry.id) {
                    self.issue(
                        format!("{}.depends_on", entry.path),
                        "dependency cycle detected in sibling scope",
                        "dependency_cycle",
                    );
                }
            }

            // A step reading steps.<sibling> must have that sibling in its
            // transitive dependency set, otherwise the value it reads depends
            // on scheduling order rather than on a declared edge.
            let mut resolved: HashMap<String, HashSet<String>> = HashMap::new();
            for entry in entries {
                let covered =
                    transitive_dependencies(&entry.id, &by_id, &mut resolved, &mut HashSet::new());
                for reference in &entry.references {
                    if reference == &entry.id {
                        continue;
                    }
                    // A sibling that is present is checked for a declared edge.
                    // One that is absent is a name that resolves to nothing --
                    // previously it was filtered out here and only surfaced at
                    // run time as EXPRESSION.EVALUATION_FAILED, by which point
                    // the earlier steps have already touched the interface. A
                    // step id known in another scope is not a typo, so it is
                    // reported the way a cross-scope dependency is.
                    if by_id.contains_key(reference.as_str()) {
                        if !covered.contains(reference) {
                            self.issue(
                                entry.path.clone(),
                                format!(
                                    "steps reference '{reference}' is not covered by depends_on"
                                ),
                                "uncovered_step_reference",
                            );
                        }
                    } else if self.ids.contains_key(reference) {
                        self.issue(
                            entry.path.clone(),
                            format!(
                                "steps reference '{reference}' is outside sibling scope                                  {scope_path}"
                            ),
                            "cross_scope_step_reference",
                        );
                    } else {
                        self.issue(
                            entry.path.clone(),
                            format!("steps reference '{reference}' is not a step in this workflow"),
                            "unknown_step_reference",
                        );
                    }
                }
            }
        }
    }

    // -- fragment validators ------------------------------------------------

    fn retry(&mut self, value: Option<&Value>, path: &str) {
        let Some(object) = self.object(value, path).cloned() else {
            return;
        };
        self.unknown(&object, &["max_attempts", "backoff", "on"], path);
        self.required(&object, &["max_attempts"], path);
        if !matches!(object.get("max_attempts"), Some(Value::Number(number)) if number.as_u64().is_some_and(|value| value >= 1))
        {
            self.issue(
                format!("{path}.max_attempts"),
                "must be a positive integer",
                "range",
            );
        }
        if let Some(backoff_value) = object.get("backoff") {
            let backoff_path = format!("{path}.backoff");
            if let Some(backoff) = self.object(Some(backoff_value), &backoff_path).cloned() {
                self.unknown(
                    &backoff,
                    &["strategy", "initial_delay", "max_delay", "multiplier", "jitter"],
                    &backoff_path,
                );
                self.required(&backoff, &["strategy", "initial_delay"], &backoff_path);
                if !matches!(backoff.get("strategy").and_then(Value::as_str), Some("fixed" | "exponential"))
                {
                    self.issue(
                        format!("{backoff_path}.strategy"),
                        "must be fixed or exponential",
                        "enum",
                    );
                }
                for key in ["initial_delay", "max_delay"] {
                    if backoff.contains_key(key) {
                        self.duration(backoff.get(key), &format!("{backoff_path}.{key}"));
                    }
                }
                if let Some(multiplier) = backoff.get("multiplier") {
                    if !matches!(multiplier.as_f64(), Some(value) if value >= 1.0) {
                        self.issue(
                            format!("{backoff_path}.multiplier"),
                            "must be at least 1",
                            "range",
                        );
                    }
                }
                if let Some(jitter) = backoff.get("jitter") {
                    if !matches!(jitter.as_f64(), Some(value) if (0.0..=1.0).contains(&value)) {
                        self.issue(
                            format!("{backoff_path}.jitter"),
                            "must be between 0 and 1",
                            "range",
                        );
                    }
                }
            }
        }
        if let Some(on_value) = object.get("on") {
            let on_path = format!("{path}.on");
            if let Some(on) = self.object(Some(on_value), &on_path).cloned() {
                self.unknown(&on, &["codes", "categories"], &on_path);
                for key in ["codes", "categories"] {
                    if on.contains_key(key) {
                        self.strings(on.get(key), &format!("{on_path}.{key}"));
                    }
                }
            }
        }
    }

    fn effect(&mut self, value: Option<&Value>, path: &str) {
        let Some(object) = self.object(value, path).cloned() else {
            return;
        };
        self.unknown(&object, &["class"], path);
        self.required(&object, &["class"], path);
        if let Some(class) = object.get("class") {
            if !class.as_str().is_some_and(|c| EFFECT_CLASSES.contains(&c)) {
                self.issue(format!("{path}.class"), "invalid effect class", "enum");
            }
        }
    }

    fn risk(&mut self, value: Option<&Value>, path: &str) {
        let Some(object) = self.object(value, path).cloned() else {
            return;
        };
        self.unknown(&object, &["category", "level", "custom_name"], path);
        self.required(&object, &["category", "level"], path);
        let category = object.get("category").and_then(Value::as_str);
        if !category.is_some_and(|value| RISK_CATEGORIES.contains(&value)) {
            self.issue(format!("{path}.category"), "invalid risk category", "enum");
        }
        if !object
            .get("level")
            .and_then(Value::as_str)
            .is_some_and(|value| RISK_LEVELS.contains(&value))
        {
            self.issue(format!("{path}.level"), "invalid risk level", "enum");
        }
        if category == Some("custom") && !matches!(object.get("custom_name"), Some(Value::String(_)))
        {
            self.issue(
                format!("{path}.custom_name"),
                "required for custom risk",
                "required",
            );
        }
        if category != Some("custom") && object.contains_key("custom_name") {
            self.issue(
                format!("{path}.custom_name"),
                "only valid for custom risk",
                "policy",
            );
        }
    }

    fn observation(&mut self, value: Option<&Value>, path: &str) {
        let Some(object) = self.object(value, path).cloned() else {
            return;
        };
        self.unknown(&object, &["uses", "with"], path);
        self.required(&object, &["uses", "with"], path);
        if !object.get("uses").and_then(Value::as_str).is_some_and(is_uses) {
            self.issue(
                format!("{path}.uses"),
                "must match capability.action@major",
                "format",
            );
        }
        if let Some(with) = object.get("with") {
            if !with.is_object() {
                self.issue(format!("{path}.with"), "must be an object", "type");
            }
            self.values(with, &format!("{path}.with"));
        }
    }

    fn assertion(&mut self, value: Option<&Value>, path: &str, post: bool) {
        let Some(object) = self.object(value, path).cloned() else {
            return;
        };
        let allowed: &[&str] = if post {
            &["condition", "message", "timeout", "poll_interval", "observe"]
        } else {
            &["condition", "message", "timeout", "poll_interval"]
        };
        self.unknown(&object, allowed, path);
        self.required(&object, &["condition"], path);
        if object.contains_key("condition") {
            self.expression(object.get("condition"), &format!("{path}.condition"));
        }
        if object.get("message").is_some_and(|value| !value.is_string()) {
            self.issue(format!("{path}.message"), "must be a string", "type");
        }
        if post && object.contains_key("observe") {
            self.observation(object.get("observe"), &format!("{path}.observe"));
        }
        for key in ["timeout", "poll_interval"] {
            if object.contains_key(key) {
                if !post {
                    self.issue(
                        format!("{path}.{key}"),
                        "preconditions cannot poll",
                        "unsupported",
                    );
                }
                self.duration(object.get(key), &format!("{path}.{key}"));
            }
        }
    }

    fn action_sensitivity(&mut self, value: Option<&Value>, path: &str) {
        let Some(object) = self.object(value, path).cloned() else {
            return;
        };
        self.unknown(&object, &["input", "output", "error"], path);
        for key in ["input", "output", "error"] {
            if let Some(found) = object.get(key) {
                if !matches!(found.as_str(), Some("public" | "sensitive")) {
                    self.issue(
                        format!("{path}.{key}"),
                        "must be public or sensitive",
                        "enum",
                    );
                }
            }
        }
    }

    fn action_checkpoint(&mut self, value: Option<&Value>, path: &str) {
        let Some(checkpoint) = self.object(value, path).cloned() else {
            return;
        };
        self.unknown(&checkpoint, &["output"], path);
        self.required(&checkpoint, &["output"], path);
        let output_path = format!("{path}.output");
        let Some(output) = self.object(checkpoint.get("output"), &output_path).cloned() else {
            return;
        };
        self.unknown(&output, &["mode", "fields"], &output_path);
        self.required(&output, &["mode"], &output_path);
        let mode = output.get("mode").and_then(Value::as_str);
        if !matches!(mode, Some("omit" | "project")) {
            self.issue(
                format!("{output_path}.mode"),
                "invalid checkpoint mode",
                "enum",
            );
        }
        if mode == Some("project") {
            self.required(&output, &["fields"], &output_path);
            if output.contains_key("fields") {
                self.strings(output.get("fields"), &format!("{output_path}.fields"));
            }
        } else if output.contains_key("fields") {
            self.issue(
                format!("{output_path}.fields"),
                "fields require project mode",
                "policy",
            );
        }
    }

    fn handler(&mut self, value: Option<&Value>, path: &str) -> Option<ErrorHandler> {
        let object = self.object(value, path).cloned()?;
        self.unknown(&object, &["match", "as", "steps", "outcome"], path);
        self.required(&object, &["steps", "outcome"], path);

        let mut handler = ErrorHandler::default();
        if let Some(match_value) = object.get("match") {
            let match_path = format!("{path}.match");
            if let Some(matcher) = self.object(Some(match_value), &match_path).cloned() {
                self.unknown(&matcher, &["codes", "categories", "effects"], &match_path);
                for key in ["codes", "categories", "effects"] {
                    if matcher.contains_key(key) {
                        self.strings(matcher.get(key), &format!("{match_path}.{key}"));
                    }
                }
                handler.match_codes = string_list(matcher.get("codes")).unwrap_or_default();
                handler.match_categories = string_list(matcher.get("categories")).unwrap_or_default();
                handler.match_effects = string_list(matcher.get("effects")).unwrap_or_default();
                if handler.match_codes.is_empty() && matcher.get("codes").is_none() {
                    handler.match_codes = vec!["*".to_string()];
                }
            }
        }
        match object.get("as") {
            None => {}
            Some(Value::String(name)) if is_identifier(name) => {
                handler.as_name = name.clone();
            }
            Some(_) => self.issue(format!("{path}.as"), "must be an identifier", "format"),
        }
        if let Some(outcome_value) = object.get("outcome") {
            let outcome_path = format!("{path}.outcome");
            if let Some(outcome) = self.object(Some(outcome_value), &outcome_path).cloned() {
                self.unknown(&outcome, &["mode", "output"], &outcome_path);
                self.required(&outcome, &["mode"], &outcome_path);
                match outcome.get("mode").and_then(Value::as_str).map(HandlerMode::parse) {
                    Some(Some(mode)) => handler.mode = mode,
                    None => {}
                    Some(None) => self.issue(
                        format!("{outcome_path}.mode"),
                        "invalid outcome mode",
                        "enum",
                    ),
                }
                if let Some(output) = outcome.get("output") {
                    self.values(output, &format!("{outcome_path}.output"));
                    handler.output = Some(output.clone());
                }
            }
        }
        handler.steps = self.steps(object.get("steps"), &format!("{path}.steps"));
        Some(handler)
    }

    // -- steps --------------------------------------------------------------

    fn steps(&mut self, value: Option<&Value>, path: &str) -> Vec<CompiledStep> {
        let Some(items) = self.array(value, path).cloned() else {
            return Vec::new();
        };
        let mut compiled: Vec<CompiledStep> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            // An omitted depends_on normalizes to the previous sibling, which
            // is what keeps legacy serial descriptors serial.
            let implicit: Vec<String> = compiled
                .last()
                .map(|previous| vec![previous.id.clone()])
                .unwrap_or_default();
            if let Some(step) = self.step(item, &format!("{path}[{index}]"), implicit) {
                compiled.push(step);
            }
        }

        let entries: Vec<ScopeEntry> = compiled
            .iter()
            .map(|step| {
                let mut references = BTreeSet::new();
                self.step_references(&Value::Object(step.params.clone()), &mut references);
                for case in &step.cases {
                    if let Some(when) = &case.when {
                        self.step_references(when, &mut references);
                    }
                }
                if let Some(handler) = &step.on_error {
                    if let Some(output) = &handler.output {
                        self.step_references(output, &mut references);
                    }
                }
                ScopeEntry {
                    id: step.id.clone(),
                    path: step.path.clone(),
                    depends_on: step.depends_on.clone(),
                    references,
                }
            })
            .collect();
        self.scopes.push((path.to_string(), entries));
        compiled
    }

    fn step(
        &mut self,
        value: &Value,
        path: &str,
        implicit_dependencies: Vec<String>,
    ) -> Option<CompiledStep> {
        let object = self.object(Some(value), path).cloned()?;
        self.required(&object, &["id", "type"], path);

        let raw_type = object.get("type").and_then(Value::as_str).unwrap_or_default();
        let mut allowed: Vec<&str> = COMMON_FIELDS.to_vec();
        allowed.extend(type_fields(raw_type).unwrap_or(&[]));
        self.unknown(&object, &allowed, path);

        let step_type = match StepType::parse(raw_type) {
            Some(found) => found,
            None => {
                self.issue(
                    format!("{path}.type"),
                    format!("unsupported step type '{raw_type}'"),
                    "enum",
                );
                return None;
            }
        };

        let step_id = match object.get("id").and_then(Value::as_str) {
            Some(found) if is_step_id(found) => {
                if let Some(first) = self.ids.get(found) {
                    let first = first.clone();
                    self.issue(
                        format!("{path}.id"),
                        format!("duplicate step id; first at {first}"),
                        "duplicate",
                    );
                } else {
                    self.ids.insert(found.to_string(), format!("{path}.id"));
                }
                found.to_string()
            }
            _ => {
                self.issue(format!("{path}.id"), "invalid step id", "format");
                format!("invalid_{}", self.ids.len())
            }
        };

        let depends_on = match object.get("depends_on") {
            None => implicit_dependencies,
            Some(raw) => {
                let mut built = Vec::new();
                let mut seen = HashSet::new();
                if let Some(items) = self.array(Some(raw), &format!("{path}.depends_on")).cloned() {
                    for (index, dependency) in items.iter().enumerate() {
                        let dependency_path = format!("{path}.depends_on[{index}]");
                        match dependency.as_str() {
                            Some(name) if is_step_id(name) => {
                                if !seen.insert(name.to_string()) {
                                    self.issue(
                                        dependency_path,
                                        format!("duplicate dependency '{name}'"),
                                        "duplicate",
                                    );
                                } else {
                                    built.push(name.to_string());
                                }
                            }
                            _ => self.issue(
                                dependency_path,
                                "must be a valid step id",
                                "format",
                            ),
                        }
                    }
                }
                built
            }
        };

        if object.contains_key("if") {
            self.expression(object.get("if"), &format!("{path}.if"));
        }
        for key in ["timeout", "attempt_timeout"] {
            if object.contains_key(key) {
                self.duration(object.get(key), &format!("{path}.{key}"));
            }
        }
        if object.contains_key("retry") {
            self.retry(object.get("retry"), &format!("{path}.retry"));
        }
        let on_error = object
            .get("on_error")
            .and_then(|value| self.handler(Some(value), &format!("{path}.on_error")));
        let finally_steps = match object.get("finally") {
            Some(value) => self.steps(Some(value), &format!("{path}.finally")),
            None => Vec::new(),
        };

        const STRUCTURAL: &[&str] = &[
            "id", "type", "depends_on", "on_error", "finally", "steps", "then", "else", "cases",
            "default",
        ];
        let params: Map<String, Value> = object
            .iter()
            .filter(|(key, _)| !STRUCTURAL.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        let mut nested = Vec::new();
        let mut then_steps = Vec::new();
        let mut else_steps = Vec::new();
        let mut default_steps = Vec::new();
        let mut cases = Vec::new();

        match step_type {
            StepType::Action => self.action_step(&object, path),
            StepType::Set => self.set_step(&object, path),
            StepType::If => {
                self.required(&object, &["condition", "then"], path);
                if object.contains_key("condition") {
                    self.expression(object.get("condition"), &format!("{path}.condition"));
                }
                then_steps = self.steps(object.get("then"), &format!("{path}.then"));
                if object.contains_key("else") {
                    else_steps = self.steps(object.get("else"), &format!("{path}.else"));
                }
            }
            StepType::Switch => {
                self.required(&object, &["cases"], path);
                if let Some(raw_cases) = self.array(object.get("cases"), &format!("{path}.cases")).cloned()
                {
                    for (index, raw_case) in raw_cases.iter().enumerate() {
                        let case_path = format!("{path}.cases[{index}]");
                        let Some(case) = self.object(Some(raw_case), &case_path).cloned() else {
                            continue;
                        };
                        self.unknown(&case, &["when", "steps"], &case_path);
                        self.required(&case, &["when", "steps"], &case_path);
                        if case.contains_key("when") {
                            self.expression(case.get("when"), &format!("{case_path}.when"));
                        }
                        cases.push(SwitchCase {
                            steps: self.steps(case.get("steps"), &format!("{case_path}.steps")),
                            when: case.get("when").cloned(),
                        });
                    }
                }
                if object.contains_key("default") {
                    default_steps = self.steps(object.get("default"), &format!("{path}.default"));
                }
            }
            StepType::Foreach | StepType::While => {
                nested = self.loop_step(&object, path, step_type);
            }
            StepType::Block => {
                self.required(&object, &["steps"], path);
                nested = self.steps(object.get("steps"), &format!("{path}.steps"));
            }
            StepType::Script => self.script_step(&object, path),
            StepType::Fail => self.fail_step(&object, path),
            StepType::Return => {
                if let Some(value) = object.get("value") {
                    self.values(value, &format!("{path}.value"));
                }
            }
        }

        Some(CompiledStep {
            id: step_id,
            step_type,
            path: path.to_string(),
            depends_on,
            params,
            steps: nested,
            then_steps,
            else_steps,
            cases,
            default_steps,
            on_error,
            finally_steps,
        })
    }

    fn action_step(&mut self, object: &Map<String, Value>, path: &str) {
        self.required(object, &["uses", "with"], path);
        if !object.get("uses").and_then(Value::as_str).is_some_and(is_uses) {
            self.issue(
                format!("{path}.uses"),
                "must match capability.action@major",
                "format",
            );
        }
        if let Some(with) = object.get("with") {
            if !with.is_object() {
                self.issue(format!("{path}.with"), "must be an object", "type");
            }
            self.values(with, &format!("{path}.with"));
        }
        if object.contains_key("effect") {
            self.effect(object.get("effect"), &format!("{path}.effect"));
        }
        if object.contains_key("risk") {
            self.risk(object.get("risk"), &format!("{path}.risk"));
        }
        if object.contains_key("precondition") {
            self.assertion(object.get("precondition"), &format!("{path}.precondition"), false);
        }
        if object.contains_key("postcondition") {
            self.assertion(object.get("postcondition"), &format!("{path}.postcondition"), true);
        }
        if object.contains_key("sensitivity") {
            self.action_sensitivity(object.get("sensitivity"), &format!("{path}.sensitivity"));
        }
        if object.contains_key("checkpoint") {
            self.action_checkpoint(object.get("checkpoint"), &format!("{path}.checkpoint"));
        }
    }

    fn set_step(&mut self, object: &Map<String, Value>, path: &str) {
        self.required(object, &["assign"], path);
        let Some(assign) = self.object(object.get("assign"), &format!("{path}.assign")).cloned()
        else {
            return;
        };
        if assign.is_empty() {
            self.issue(format!("{path}.assign"), "must not be empty", "range");
        }
        for (target, assigned) in &assign {
            // Nested writes such as vars.a.b are deliberately unsupported in v0.
            let valid = target
                .strip_prefix("vars.")
                .is_some_and(is_identifier);
            if !valid {
                self.issue(
                    format!("{path}.assign.{target}"),
                    "target must be vars.name",
                    "format",
                );
            }
            self.values(assigned, &format!("{path}.assign.{target}"));
        }
    }

    fn loop_step(
        &mut self,
        object: &Map<String, Value>,
        path: &str,
        step_type: StepType,
    ) -> Vec<CompiledStep> {
        let is_foreach = step_type == StepType::Foreach;
        let required: &[&str] = if is_foreach {
            &["items", "as", "max_items", "steps"]
        } else {
            &["condition", "max_iterations", "timeout", "steps"]
        };
        self.required(object, required, path);

        let expression_key = if is_foreach { "items" } else { "condition" };
        if object.contains_key(expression_key) {
            self.expression(
                object.get(expression_key),
                &format!("{path}.{expression_key}"),
            );
        }
        if is_foreach {
            for key in ["as", "index_as"] {
                if let Some(name) = object.get(key) {
                    if !name.as_str().is_some_and(is_identifier) {
                        self.issue(format!("{path}.{key}"), "must be an identifier", "format");
                    }
                }
            }
        }
        let limit_key = if is_foreach { "max_items" } else { "max_iterations" };
        if !object
            .get(limit_key)
            .and_then(Value::as_u64)
            .is_some_and(|value| value >= 1)
        {
            self.issue(
                format!("{path}.{limit_key}"),
                "must be a positive integer",
                "range",
            );
        }
        if is_foreach {
            if let Some(concurrency) = object.get("concurrency") {
                if !concurrency.as_u64().is_some_and(|value| value >= 1) {
                    self.issue(
                        format!("{path}.concurrency"),
                        "must be a positive integer",
                        "range",
                    );
                }
            }
        }
        self.steps(object.get("steps"), &format!("{path}.steps"))
    }

    fn script_step(&mut self, object: &Map<String, Value>, path: &str) {
        self.required(object, &["runtime", "output_schema"], path);
        if object.get("runtime").and_then(Value::as_str) != Some("python") {
            self.issue(
                format!("{path}.runtime"),
                "v0 only supports python script runtime",
                "unsupported",
            );
        }
        if object.contains_key("source") == object.contains_key("entrypoint") {
            self.issue(
                path,
                "exactly one of source and entrypoint is required",
                "one_of",
            );
        }
        if let Some(inputs) = object.get("inputs") {
            if !inputs.is_object() {
                self.issue(format!("{path}.inputs"), "must be an object", "type");
            }
            self.values(inputs, &format!("{path}.inputs"));
        }
        if let Some(schema) = object.get("output_schema") {
            if !schema.is_object() && !schema.is_boolean() {
                self.issue(
                    format!("{path}.output_schema"),
                    "must be an object or boolean JSON Schema",
                    "type",
                );
            }
        }
        // Interpolating into source would let data become code.
        if object
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(|source| source.contains("${{"))
        {
            self.issue(
                format!("{path}.source"),
                "expressions are forbidden in source",
                "policy",
            );
        }
        if let Some(sandbox_value) = object.get("sandbox") {
            let sandbox_path = format!("{path}.sandbox");
            let Some(sandbox) = self.object(Some(sandbox_value), &sandbox_path).cloned() else {
                return;
            };
            self.unknown(
                &sandbox,
                &["network", "filesystem", "environment", "max_output_bytes"],
                &sandbox_path,
            );
            if let Some(limit) = sandbox.get("max_output_bytes") {
                if !limit.as_u64().is_some_and(|value| value >= 1) {
                    self.issue(
                        format!("{sandbox_path}.max_output_bytes"),
                        "must be a positive integer",
                        "range",
                    );
                }
            }
            for boundary in ["network", "filesystem", "environment"] {
                let Some(raw) = sandbox.get(boundary) else {
                    continue;
                };
                let boundary_path = format!("{sandbox_path}.{boundary}");
                let Some(config) = self.object(Some(raw), &boundary_path).cloned() else {
                    continue;
                };
                self.unknown(&config, &["mode"], &boundary_path);
                self.required(&config, &["mode"], &boundary_path);
                if config.get("mode").and_then(Value::as_str) != Some("deny") {
                    self.issue(
                        format!("{boundary_path}.mode"),
                        format!("v0 only supports deny-only script {boundary} sandbox"),
                        "unsupported",
                    );
                }
            }
        }
    }

    fn fail_step(&mut self, object: &Map<String, Value>, path: &str) {
        self.required(object, &["error"], path);
        let error_path = format!("{path}.error");
        let Some(error) = self.object(object.get("error"), &error_path).cloned() else {
            return;
        };
        self.unknown(
            &error,
            &["code", "message", "category", "retryable", "effect", "details"],
            &error_path,
        );
        self.required(&error, &["code", "message"], &error_path);
        if !error
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(is_error_code)
        {
            self.issue(
                format!("{error_path}.code"),
                "must be an uppercase dotted code",
                "format",
            );
        }
        if error.get("message").is_some_and(|value| !value.is_string()) {
            self.issue(format!("{error_path}.message"), "must be a string", "type");
        }
        self.values(&Value::Object(error), &error_path);
    }

    fn named(
        &mut self,
        value: Option<&Value>,
        path: &str,
        kind: &str,
    ) -> IndexMap<String, NamedValue> {
        let mut result = IndexMap::new();
        // These sections are optional; only a present-but-wrong value is an error.
        if value.is_none() {
            return result;
        }
        let Some(object) = self.object(value, path).cloned() else {
            return result;
        };
        let (fields, required): (&[&str], &[&str]) = match kind {
            "inputs" => (&["schema", "required", "default", "sensitive"], &["schema"]),
            "variables" => (&["schema", "mutable", "initial"], &["schema"]),
            _ => (&["value", "schema", "sensitive"], &["value"]),
        };

        for (name, raw) in &object {
            let item_path = format!("{path}.{name}");
            if !is_identifier(name) {
                self.issue(&item_path, "name must be an identifier", "format");
            }
            let Some(item) = self.object(Some(raw), &item_path).cloned() else {
                continue;
            };
            self.unknown(&item, fields, &item_path);
            self.required(&item, required, &item_path);
            if let Some(schema) = item.get("schema") {
                if !schema.is_object() && !schema.is_boolean() {
                    self.issue(
                        format!("{item_path}.schema"),
                        "must be an object or boolean JSON Schema",
                        "type",
                    );
                }
            }
            for flag in ["required", "sensitive", "mutable"] {
                if item.get(flag).is_some_and(|value| !value.is_boolean()) {
                    self.issue(format!("{item_path}.{flag}"), "must be a boolean", "type");
                }
            }
            if kind == "inputs"
                && item.get("required") == Some(&Value::Bool(true))
                && item.contains_key("default")
            {
                self.issue(
                    &item_path,
                    "required input cannot also define a default",
                    "policy",
                );
            }
            for key in ["default", "initial", "value"] {
                if let Some(found) = item.get(key) {
                    self.values(found, &format!("{item_path}.{key}"));
                }
            }
            result.insert(
                name.clone(),
                NamedValue {
                    schema: item.get("schema").cloned(),
                    required: item.get("required") == Some(&Value::Bool(true)),
                    default: item.get("default").cloned(),
                    sensitive: item.get("sensitive") == Some(&Value::Bool(true)),
                    mutable: item.get("mutable") == Some(&Value::Bool(true)),
                    initial: item.get("initial").cloned(),
                    value: item.get("value").cloned(),
                },
            );
        }
        result
    }

    fn compile(mut self, raw: Value) -> Result<WorkflowDescriptor> {
        let Some(root) = self.object(Some(&raw), "$").cloned() else {
            return Err(AutomationError::descriptor(self.issues));
        };
        self.unknown(&root, TOP_FIELDS, "$");
        self.required(
            &root,
            &["apiVersion", "kind", "metadata", "budgets", "steps"],
            "$",
        );
        if root.get("apiVersion").and_then(Value::as_str) != Some(API_VERSION) {
            self.issue(
                "$.apiVersion",
                format!("only {API_VERSION} is supported"),
                "unsupported_version",
            );
        }
        if root.get("kind").and_then(Value::as_str) != Some(KIND) {
            self.issue("$.kind", "must be Workflow", "enum");
        }

        let metadata = match root.get("metadata") {
            Some(value) => self
                .object(Some(value), "$.metadata")
                .cloned()
                .unwrap_or_default(),
            None => Map::new(),
        };
        self.unknown(
            &metadata,
            &["name", "version", "description", "labels", "annotations"],
            "$.metadata",
        );
        self.required(&metadata, &["name"], "$.metadata");
        let name = metadata.get("name").and_then(Value::as_str).unwrap_or("");
        if !is_dotted_name(name) {
            self.issue("$.metadata.name", "invalid workflow name", "format");
        }

        let inputs = self.named(root.get("inputs"), "$.inputs", "inputs");
        let variables = self.named(root.get("variables"), "$.variables", "variables");
        let outputs = self.named(root.get("outputs"), "$.outputs", "outputs");

        let defaults = match root.get("defaults") {
            Some(value) => self
                .object(Some(value), "$.defaults")
                .cloned()
                .unwrap_or_default(),
            None => Map::new(),
        };
        self.unknown(&defaults, &["timeout", "retry"], "$.defaults");
        if defaults.contains_key("timeout") {
            self.duration(defaults.get("timeout"), "$.defaults.timeout");
        }
        if defaults.contains_key("retry") {
            self.retry(defaults.get("retry"), "$.defaults.retry");
        }

        let budgets_map = match root.get("budgets") {
            Some(value) => self
                .object(Some(value), "$.budgets")
                .cloned()
                .unwrap_or_default(),
            None => Map::new(),
        };
        self.unknown(
            &budgets_map,
            &["max_duration", "max_executed_steps", "cleanup_timeout", "max_concurrency"],
            "$.budgets",
        );
        if root.contains_key("budgets") {
            self.required(
                &budgets_map,
                &["max_duration", "max_executed_steps"],
                "$.budgets",
            );
        }
        for key in ["max_duration", "cleanup_timeout"] {
            if budgets_map.contains_key(key) {
                self.duration(budgets_map.get(key), &format!("$.budgets.{key}"));
            }
        }
        if budgets_map.contains_key("max_executed_steps")
            && !budgets_map
                .get("max_executed_steps")
                .and_then(Value::as_u64)
                .is_some_and(|value| value >= 1)
        {
            self.issue(
                "$.budgets.max_executed_steps",
                "must be a positive integer",
                "range",
            );
        }
        if budgets_map.contains_key("max_concurrency")
            && !budgets_map
                .get("max_concurrency")
                .and_then(Value::as_u64)
                .is_some_and(|value| (1..=64).contains(&value))
        {
            self.issue(
                "$.budgets.max_concurrency",
                "must be an integer between 1 and 64",
                "range",
            );
        }
        let budgets = Budgets {
            max_duration: budgets_map
                .get("max_duration")
                .and_then(Value::as_str)
                .and_then(parse_duration)
                .unwrap_or(300.0),
            max_executed_steps: budgets_map
                .get("max_executed_steps")
                .and_then(Value::as_u64)
                .unwrap_or(1_000),
            cleanup_timeout: budgets_map
                .get("cleanup_timeout")
                .and_then(Value::as_str)
                .and_then(parse_duration),
            max_concurrency: budgets_map
                .get("max_concurrency")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32,
        };

        let requires = match root.get("requires") {
            Some(value) => self
                .object(Some(value), "$.requires")
                .cloned()
                .unwrap_or_default(),
            None => Map::new(),
        };
        let policy = match root.get("policy") {
            Some(value) => self
                .object(Some(value), "$.policy")
                .cloned()
                .unwrap_or_default(),
            None => Map::new(),
        };
        let extensions = match root.get("extensions") {
            Some(value) => self
                .object(Some(value), "$.extensions")
                .cloned()
                .unwrap_or_default(),
            None => Map::new(),
        };
        self.unknown(
            &requires,
            &["runtime", "platforms", "capabilities", "permissions"],
            "$.requires",
        );
        self.unknown(
            &policy,
            &["allowed_risk", "confirmation", "untrusted_inputs", "screenshots", "desktop"],
            "$.policy",
        );
        for (key, value) in &policy {
            if !value.is_object() {
                self.issue(format!("$.policy.{key}"), "must be an object", "type");
            }
        }
        for key in extensions.keys() {
            if !key.contains('/') {
                self.issue(
                    format!("$.extensions.{key}"),
                    "extension name must use domain/name",
                    "format",
                );
            }
        }

        let steps = self.steps(root.get("steps"), "$.steps");
        if steps.is_empty() {
            self.issue("$.steps", "must contain at least one step", "range");
        }
        let on_error = root
            .get("on_error")
            .and_then(|value| self.handler(Some(value), "$.on_error"));
        let finally_steps = match root.get("finally") {
            Some(value) => self.steps(Some(value), "$.finally"),
            None => Vec::new(),
        };

        self.validate_scopes();

        // `set` may only write declared, mutable variables.
        let mutable: HashSet<&String> = variables
            .iter()
            .filter(|(_, definition)| definition.mutable)
            .map(|(name, _)| name)
            .collect();
        let mut assignment_issues = Vec::new();
        for step in walk_all(&steps, on_error.as_ref(), &finally_steps) {
            if step.step_type != StepType::Set {
                continue;
            }
            let Some(Value::Object(assign)) = step.params.get("assign") else {
                continue;
            };
            for target in assign.keys() {
                let name = target.split_once('.').map(|(_, tail)| tail).unwrap_or("");
                if !variables.contains_key(name) {
                    assignment_issues.push(DescriptorIssue::new(
                        format!("{}.assign.{target}", step.path),
                        "variable is not declared",
                        "reference",
                    ));
                } else if !mutable.contains(&name.to_string()) {
                    assignment_issues.push(DescriptorIssue::new(
                        format!("{}.assign.{target}", step.path),
                        "variable is immutable",
                        "policy",
                    ));
                }
            }
        }
        self.issues.extend(assignment_issues);

        // Every `${{ inputs.<name> }}` must name a declared input. Like an
        // unknown step reference this used to surface only at run time, as
        // EXPRESSION.EVALUATION_FAILED raised by whichever step read it -- and
        // measured on a real recording that step was the fifth of six, so the
        // interface had already been written to before the run stopped. The
        // information needed to catch it is entirely in the document.
        //
        // Only `inputs` is checked. The engine also puts `vars`, `runtime`,
        // `observation`, `failure` and a `foreach` binding in scope, and none of
        // those can be resolved from the document alone: `vars` is spelled
        // differently from its `$.variables` declaration, two of them exist only
        // inside a particular step, and a loop binding is named by that step's
        // own `as`. Reporting them would refuse workflows that run.
        let mut input_issues = Vec::new();
        for step in walk_all(&steps, on_error.as_ref(), &finally_steps) {
            let mut wanted = BTreeSet::new();
            self.input_references(&Value::Object(step.params.clone()), &mut wanted);
            for case in &step.cases {
                if let Some(when) = &case.when {
                    self.input_references(when, &mut wanted);
                }
            }
            if let Some(handler) = &step.on_error {
                if let Some(output) = &handler.output {
                    self.input_references(output, &mut wanted);
                }
            }
            for name in wanted {
                if !inputs.contains_key(&name) {
                    input_issues.push(DescriptorIssue::new(
                        step.path.clone(),
                        format!("inputs reference '{name}' is not a declared input"),
                        "unknown_input_reference",
                    ));
                }
            }
        }
        self.issues.extend(input_issues);

        if !self.issues.is_empty() {
            return Err(AutomationError::descriptor(self.issues));
        }

        Ok(WorkflowDescriptor {
            api_version: API_VERSION.to_string(),
            name: name.to_string(),
            description: metadata
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            source: self.source,
            steps,
            metadata,
            inputs,
            variables,
            outputs,
            requires,
            defaults,
            budgets,
            policy,
            extensions,
            on_error,
            finally_steps,
            raw,
        })
    }
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

fn walk_all<'a>(
    steps: &'a [CompiledStep],
    handler: Option<&'a ErrorHandler>,
    finally_steps: &'a [CompiledStep],
) -> Vec<&'a CompiledStep> {
    let mut found: Vec<&CompiledStep> = steps.iter().flat_map(CompiledStep::walk).collect();
    if let Some(handler) = handler {
        found.extend(handler.steps.iter().flat_map(CompiledStep::walk));
    }
    found.extend(finally_steps.iter().flat_map(CompiledStep::walk));
    found
}

/// Iterative DFS returning every step that participates in a cycle.
fn find_cycles(entries: &[ScopeEntry], by_id: &HashMap<&str, &ScopeEntry>) -> HashSet<String> {
    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut cyclic = HashSet::new();
    let mut stack: Vec<&str> = Vec::new();

    fn visit<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a ScopeEntry>,
        state: &mut HashMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
        cyclic: &mut HashSet<String>,
    ) {
        state.insert(id, 1);
        stack.push(id);
        if let Some(entry) = by_id.get(id) {
            for dependency in &entry.depends_on {
                let Some((key, _)) = by_id.get_key_value(dependency.as_str()) else {
                    continue;
                };
                if *key == id {
                    continue;
                }
                match state.get(key).copied().unwrap_or(0) {
                    0 => visit(key, by_id, state, stack, cyclic),
                    1 => {
                        if let Some(start) = stack.iter().position(|item| item == key) {
                            cyclic.extend(stack[start..].iter().map(|item| item.to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }
        stack.pop();
        state.insert(id, 2);
    }

    for entry in entries {
        if state.get(entry.id.as_str()).copied().unwrap_or(0) == 0 {
            visit(&entry.id, by_id, &mut state, &mut stack, &mut cyclic);
        }
    }
    cyclic
}

fn transitive_dependencies(
    id: &str,
    by_id: &HashMap<&str, &ScopeEntry>,
    cache: &mut HashMap<String, HashSet<String>>,
    visiting: &mut HashSet<String>,
) -> HashSet<String> {
    if let Some(found) = cache.get(id) {
        return found.clone();
    }
    if !visiting.insert(id.to_string()) {
        return HashSet::new();
    }
    let mut covered = HashSet::new();
    if let Some(entry) = by_id.get(id) {
        for dependency in &entry.depends_on {
            if by_id.contains_key(dependency.as_str()) {
                covered.insert(dependency.clone());
                covered.extend(transitive_dependencies(dependency, by_id, cache, visiting));
            }
        }
    }
    visiting.remove(id);
    cache.insert(id.to_string(), covered.clone());
    covered
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Compile an already-parsed descriptor value.
pub fn compile_descriptor(descriptor: Value, source: Option<PathBuf>) -> Result<WorkflowDescriptor> {
    Compiler::new(source).compile(descriptor)
}

/// Parse descriptor text as JSON or YAML, choosing by extension then content.
pub fn parse_descriptor_text(text: &str, source: &str) -> Result<Value> {
    if text.len() > MAX_DESCRIPTOR_BYTES {
        return Err(AutomationError::descriptor(vec![DescriptorIssue::new(
            "$",
            "descriptor exceeds 2 MiB",
            "limit",
        )]));
    }
    let extension = Path::new(source)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    let is_json = extension.as_deref() == Some("json")
        || (!matches!(extension.as_deref(), Some("yaml" | "yml"))
            && text.trim_start().starts_with(['{', '[']));

    let parsed = if is_json {
        serde_json::from_str::<Value>(text).map_err(|error| {
            AutomationError::descriptor_with(
                format!("Cannot parse descriptor: {error}"),
                "DESCRIPTOR.INVALID",
                vec![DescriptorIssue::new("$", error.to_string(), "parse")],
            )
        })?
    } else {
        // serde_yaml rejects aliases by default via its recursion limits and
        // does not implement merge keys, matching the strict loader rules.
        serde_yaml::from_str::<Value>(text).map_err(|error| {
            AutomationError::descriptor_with(
                format!("Cannot parse descriptor: {error}"),
                "DESCRIPTOR.INVALID",
                vec![DescriptorIssue::new("$", error.to_string(), "parse")],
            )
        })?
    };
    reject_duplicate_keys(text, is_json)?;
    Ok(parsed)
}

/// serde silently keeps the last duplicate key; a descriptor must not.
fn reject_duplicate_keys(text: &str, is_json: bool) -> Result<()> {
    if !is_json {
        return Ok(());
    }
    let mut deserializer = serde_json::Deserializer::from_str(text);
    match serde_path_to_error_check(&mut deserializer) {
        Ok(()) => Ok(()),
        Err(message) => Err(AutomationError::descriptor(vec![DescriptorIssue::new(
            "$", message, "parse",
        )])),
    }
}

/// Walk the JSON document rejecting any object with a repeated key.
fn serde_path_to_error_check(
    deserializer: &mut serde_json::Deserializer<serde_json::de::StrRead<'_>>,
) -> std::result::Result<(), String> {
    use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
    use std::fmt;

    struct UniqueKeys;

    impl<'de> Visitor<'de> for UniqueKeys {
        type Value = ();

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a JSON document with unique object keys")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
            let mut seen = HashSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !seen.insert(key.clone()) {
                    return Err(serde::de::Error::custom(format!("duplicate key '{key}'")));
                }
                map.next_value_seed(UniqueKeysSeed)?;
            }
            Ok(())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
            while seq.next_element_seed(UniqueKeysSeed)?.is_some() {}
            Ok(())
        }

        fn visit_unit<E>(self) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_none<E>(self) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
            Ok(())
        }
    }

    struct UniqueKeysSeed;

    impl<'de> serde::de::DeserializeSeed<'de> for UniqueKeysSeed {
        type Value = ();

        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<(), D::Error> {
            deserializer.deserialize_any(UniqueKeys)
        }
    }

    impl<'de> Deserialize<'de> for UniqueKeysSeed {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
            deserializer.deserialize_any(UniqueKeys)?;
            Ok(UniqueKeysSeed)
        }
    }

    UniqueKeysSeed::deserialize(deserializer)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Read and compile a descriptor from disk.
pub fn load_descriptor(path: impl AsRef<Path>) -> Result<WorkflowDescriptor> {
    let path = path.as_ref();
    let resolved = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf());
    let text = std::fs::read_to_string(&resolved).map_err(|error| {
        AutomationError::descriptor_with(
            format!("Cannot read descriptor: {error}"),
            "DESCRIPTOR.INVALID",
            vec![DescriptorIssue::new("$", error.to_string(), "read")],
        )
    })?;
    let value = parse_descriptor_text(&text, &resolved.to_string_lossy())?;
    compile_descriptor(value, Some(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal(steps: Value) -> Value {
        json!({
            "apiVersion": API_VERSION,
            "kind": "Workflow",
            "metadata": {"name": "example"},
            "budgets": {"max_duration": "30s", "max_executed_steps": 10},
            "steps": steps
        })
    }

    fn compile(value: Value) -> Result<WorkflowDescriptor> {
        compile_descriptor(value, None)
    }

    fn issue_codes(error: &AutomationError) -> Vec<String> {
        error.issues.iter().map(|issue| issue.code.clone()).collect()
    }

    #[test]
    fn a_minimal_descriptor_compiles() {
        let descriptor = compile(minimal(json!([
            {"id": "done", "type": "return", "value": 1}
        ])))
        .expect("descriptor should compile");

        assert_eq!(descriptor.name, "example");
        assert_eq!(descriptor.api_version, API_VERSION);
        assert_eq!(descriptor.steps.len(), 1);
        assert_eq!(descriptor.budgets.max_duration, 30.0);
        assert_eq!(descriptor.budgets.max_concurrency, 1);
    }

    #[test]
    fn unknown_top_level_fields_are_rejected() {
        let mut value = minimal(json!([{"id": "done", "type": "return"}]));
        value["surprise"] = json!(true);

        let error = compile(value).unwrap_err();
        assert!(issue_codes(&error).contains(&"unknown_field".to_string()));
    }

    #[test]
    fn every_issue_is_reported_not_only_the_first() {
        let error = compile(json!({
            "apiVersion": "wrong",
            "kind": "NotAWorkflow",
            "metadata": {},
            "budgets": {},
            "steps": []
        }))
        .unwrap_err();

        let codes = issue_codes(&error);
        assert!(codes.contains(&"unsupported_version".to_string()));
        assert!(codes.contains(&"enum".to_string()));
        assert!(codes.contains(&"required".to_string()));
        assert!(codes.len() >= 4, "expected several issues, got {codes:?}");
    }

    #[test]
    fn omitted_depends_on_normalizes_to_the_previous_sibling() {
        let descriptor = compile(minimal(json!([
            {"id": "first", "type": "return"},
            {"id": "second", "type": "return"}
        ])))
        .expect("descriptor should compile");

        assert_eq!(descriptor.steps[0].depends_on, Vec::<String>::new());
        assert_eq!(descriptor.steps[1].depends_on, vec!["first".to_string()]);
    }

    #[test]
    fn an_explicit_empty_depends_on_breaks_the_serial_chain() {
        let descriptor = compile(minimal(json!([
            {"id": "first", "type": "return"},
            {"id": "second", "type": "return", "depends_on": []}
        ])))
        .expect("descriptor should compile");

        assert!(descriptor.steps[1].depends_on.is_empty());
    }

    #[test]
    fn dependency_cycles_are_rejected() {
        let error = compile(minimal(json!([
            {"id": "a", "type": "return", "depends_on": ["b"]},
            {"id": "b", "type": "return", "depends_on": ["a"]}
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"dependency_cycle".to_string()));
    }

    #[test]
    fn self_and_unknown_dependencies_are_rejected() {
        let error = compile(minimal(json!([
            {"id": "a", "type": "return", "depends_on": ["a"]},
            {"id": "b", "type": "return", "depends_on": ["ghost"]}
        ])))
        .unwrap_err();

        let codes = issue_codes(&error);
        assert!(codes.contains(&"self_dependency".to_string()));
        assert!(codes.contains(&"unknown_dependency".to_string()));
    }

    #[test]
    fn cross_scope_dependencies_are_rejected() {
        let error = compile(minimal(json!([
            {"id": "outer", "type": "return"},
            {
                "id": "wrapper", "type": "block",
                "steps": [{"id": "inner", "type": "return", "depends_on": ["outer"]}]
            }
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"cross_scope_dependency".to_string()));
    }

    #[test]
    fn a_step_reference_must_be_covered_by_depends_on() {
        let error = compile(minimal(json!([
            {"id": "producer", "type": "return", "value": 1},
            {
                "id": "consumer", "type": "return", "depends_on": [],
                "value": "${{ steps.producer.output }}"
            }
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"uncovered_step_reference".to_string()));
    }

    #[test]
    fn a_transitively_covered_step_reference_is_accepted() {
        compile(minimal(json!([
            {"id": "producer", "type": "return", "value": 1},
            {"id": "middle", "type": "return", "depends_on": ["producer"]},
            {
                "id": "consumer", "type": "return", "depends_on": ["middle"],
                "value": "${{ steps.producer.output }}"
            }
        ])))
        .expect("a transitive dependency covers the reference");
    }

    #[test]
    fn duplicate_step_ids_are_rejected() {
        let error = compile(minimal(json!([
            {"id": "same", "type": "return"},
            {"id": "same", "type": "return"}
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"duplicate".to_string()));
    }

    #[test]
    fn set_must_target_a_declared_mutable_variable() {
        let mut value = minimal(json!([
            {"id": "assign", "type": "set", "assign": {"vars.counter": 1}}
        ]));
        let error = compile(value.clone()).unwrap_err();
        assert!(issue_codes(&error).contains(&"reference".to_string()));

        value["variables"] = json!({"counter": {"schema": {"type": "integer"}}});
        let error = compile(value.clone()).unwrap_err();
        assert!(issue_codes(&error).contains(&"policy".to_string()));

        value["variables"] =
            json!({"counter": {"schema": {"type": "integer"}, "mutable": true, "initial": 0}});
        compile(value).expect("a mutable declared variable is assignable");
    }

    #[test]
    fn set_rejects_nested_targets() {
        let mut value = minimal(json!([
            {"id": "assign", "type": "set", "assign": {"vars.a.b": 1}}
        ]));
        value["variables"] = json!({"a": {"schema": true, "mutable": true, "initial": {}}});

        let error = compile(value).unwrap_err();
        assert!(issue_codes(&error).contains(&"format".to_string()));
    }

    #[test]
    fn invalid_expressions_fail_at_compile_time() {
        let error = compile(minimal(json!([
            {"id": "branch", "type": "if", "condition": "${{ open('x') }}",
             "then": [{"id": "inner", "type": "return"}]}
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"expression".to_string()));
    }

    #[test]
    fn a_condition_must_be_one_complete_template() {
        let error = compile(minimal(json!([
            {"id": "branch", "type": "if", "condition": "value is ${{ x }}",
             "then": [{"id": "inner", "type": "return"}]}
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"expression".to_string()));
    }

    #[test]
    fn action_uses_must_be_a_canonical_action_id() {
        for uses in ["fixture.ocr", "fixture.ocr@0", "Fixture.OCR@1", "ocr@1"] {
            let error = compile(minimal(json!([
                {"id": "act", "type": "action", "uses": uses, "with": {}}
            ])))
            .unwrap_err();
            assert!(
                issue_codes(&error).contains(&"format".to_string()),
                "{uses} must be rejected"
            );
        }

        compile(minimal(json!([
            {"id": "act", "type": "action", "uses": "desktop.windows_uia.snapshot@1", "with": {}}
        ])))
        .expect("a canonical action id is accepted");
    }

    #[test]
    fn script_steps_only_allow_a_deny_only_sandbox() {
        let error = compile(minimal(json!([
            {
                "id": "run", "type": "script", "runtime": "python",
                "source": "print(1)", "output_schema": true,
                "sandbox": {"network": {"mode": "allow"}}
            }
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"unsupported".to_string()));
    }

    #[test]
    fn script_source_must_not_contain_expressions() {
        let error = compile(minimal(json!([
            {
                "id": "run", "type": "script", "runtime": "python",
                "source": "print('${{ inputs.secret }}')", "output_schema": true
            }
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"policy".to_string()));
    }

    #[test]
    fn script_requires_exactly_one_of_source_and_entrypoint() {
        for script in [
            json!({"id": "run", "type": "script", "runtime": "python", "output_schema": true}),
            json!({
                "id": "run", "type": "script", "runtime": "python", "output_schema": true,
                "source": "print(1)", "entrypoint": "main.py"
            }),
        ] {
            let error = compile(minimal(json!([script]))).unwrap_err();
            assert!(issue_codes(&error).contains(&"one_of".to_string()));
        }
    }

    #[test]
    fn budgets_are_mandatory_and_bounded() {
        let mut value = minimal(json!([{"id": "done", "type": "return"}]));
        value["budgets"] = json!({"max_duration": "30s", "max_executed_steps": 10, "max_concurrency": 65});

        let error = compile(value).unwrap_err();
        assert!(issue_codes(&error).contains(&"range".to_string()));
    }

    #[test]
    fn a_step_reference_that_names_nothing_is_refused() {
        // Previously this filtered out: only references to steps that exist were
        // checked for a declared edge, so a typo passed validation and failed at
        // run time as EXPRESSION.EVALUATION_FAILED. Measured on a real recording
        // the failing step was the fifth of six, so the field had already been
        // written before the run stopped.
        let error = compile(minimal(json!([
            {"id": "first", "type": "action", "uses": "desktop.windows_uia.snapshot@1",
             "with": {}},
            {"id": "second", "type": "action", "uses": "desktop.windows_uia.find@1",
             "depends_on": ["first"],
             "with": {"snapshot_id": "${{ steps.typo_first.output.snapshot_id }}"}}
        ])))
        .unwrap_err();

        assert!(
            issue_codes(&error).contains(&"unknown_step_reference".to_string()),
            "a reference to a step that does not exist must be refused: {error}"
        );
    }

    #[test]
    fn an_input_reference_that_names_nothing_is_refused() {
        let error = compile(minimal(json!([
            {"id": "act", "type": "action", "uses": "desktop.windows_uia.set_value@1",
             "with": {"target": "x", "value": "${{ inputs.never_declared }}"}}
        ])))
        .unwrap_err();

        assert!(
            issue_codes(&error).contains(&"unknown_input_reference".to_string()),
            "a reference to an input that was never declared must be refused: {error}"
        );
    }

    #[test]
    fn the_other_scope_roots_are_left_alone() {
        // `vars`, `observation`, `failure` and a loop binding cannot be resolved
        // from the document: `vars` is spelled differently from its
        // `$.variables` declaration, two exist only inside one step, and a
        // `foreach` binding is named by that step's own `as`. Checking them would
        // refuse workflows that run correctly, so they are deliberately not
        // checked -- and this test is what says so.
        let mut value = minimal(json!([
            {"id": "loop", "type": "foreach", "items": "${{ vars.rows }}", "as": "row",
             "max_items": 10,
             "steps": [
                 {"id": "act", "type": "action", "uses": "desktop.windows_uia.set_value@1",
                  "with": {"target": "${{ row.ref }}", "value": "${{ vars.text }}"}}
             ]}
        ]));
        value["variables"] = json!({
            "rows": {"schema": {"type": "array"}, "initial": []},
            "text": {"schema": {"type": "string"}, "initial": "x"}
        });

        compile(value).expect("references to other scope roots must be left alone");
    }

    #[test]
    fn a_required_input_cannot_also_have_a_default() {
        let mut value = minimal(json!([{"id": "done", "type": "return"}]));
        value["inputs"] = json!({"name": {"schema": {"type": "string"}, "required": true, "default": "x"}});

        let error = compile(value).unwrap_err();
        assert!(issue_codes(&error).contains(&"policy".to_string()));
    }

    #[test]
    fn preconditions_cannot_poll() {
        let error = compile(minimal(json!([
            {
                "id": "act", "type": "action", "uses": "fixture.ocr@1", "with": {},
                "precondition": {"condition": "${{ True }}", "timeout": "1s"}
            }
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"unsupported".to_string()));
    }

    #[test]
    fn foreach_requires_a_bound() {
        let error = compile(minimal(json!([
            {
                "id": "loop", "type": "foreach", "items": "${{ [1, 2] }}", "as": "item",
                "steps": [{"id": "inner", "type": "return"}]
            }
        ])))
        .unwrap_err();

        assert!(issue_codes(&error).contains(&"required".to_string()));
    }

    #[test]
    fn all_steps_includes_nested_handler_and_finally_steps() {
        let descriptor = compile(minimal(json!([
            {
                "id": "outer", "type": "block",
                "steps": [{"id": "inner", "type": "return"}],
                "finally": [{"id": "cleanup", "type": "return"}]
            }
        ])))
        .expect("descriptor should compile");

        let ids: Vec<&str> = descriptor
            .all_steps()
            .iter()
            .map(|step| step.id.as_str())
            .collect();
        assert_eq!(ids, vec!["outer", "inner", "cleanup"]);
    }

    #[test]
    fn yaml_and_json_produce_the_same_plan() {
        let yaml = r#"
apiVersion: ai-auto-desktop.dev/v1alpha1
kind: Workflow
metadata:
  name: example
budgets:
  max_duration: 30s
  max_executed_steps: 10
steps:
  - id: done
    type: return
    value: 1
"#;
        let from_yaml = compile(parse_descriptor_text(yaml, "workflow.yaml").unwrap()).unwrap();
        let from_json = compile(minimal(json!([{"id": "done", "type": "return", "value": 1}]))).unwrap();

        assert_eq!(from_yaml.name, from_json.name);
        assert_eq!(from_yaml.steps.len(), from_json.steps.len());
        assert_eq!(from_yaml.steps[0].id, from_json.steps[0].id);
    }

    #[test]
    fn duplicate_json_keys_are_rejected() {
        let text = r#"{"apiVersion": "a", "apiVersion": "b"}"#;
        let error = parse_descriptor_text(text, "workflow.json").unwrap_err();
        assert_eq!(error.code, "DESCRIPTOR.INVALID");
    }

    #[test]
    fn descriptors_over_two_mebibytes_are_rejected() {
        let text = "x".repeat(MAX_DESCRIPTOR_BYTES + 1);
        let error = parse_descriptor_text(&text, "workflow.json").unwrap_err();
        assert_eq!(error.issues[0].code, "limit");
    }
}
