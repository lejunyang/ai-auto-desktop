//! Immutable compiled descriptor model.
//!
//! The compiler produces these types once and the runtime only reads them, so a
//! plan cannot change while it executes.

use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::path::PathBuf;

/// A `${{ ... }}` template that occupies a whole string, or a literal value.
pub type Json = Value;

/// How an error handler ends: rethrow the original, continue with a
/// replacement output, or return from the workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerMode {
    Rethrow,
    Continue,
    Return,
}

impl HandlerMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rethrow" => Some(Self::Rethrow),
            "continue" => Some(Self::Continue),
            "return" => Some(Self::Return),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rethrow => "rethrow",
            Self::Continue => "continue",
            Self::Return => "return",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ErrorHandler {
    pub steps: Vec<CompiledStep>,
    /// Codes to match; `*` and a trailing `PREFIX.*` are both supported.
    pub match_codes: Vec<String>,
    pub match_categories: Vec<String>,
    pub match_effects: Vec<String>,
    /// Name the error is bound to inside handler steps.
    pub as_name: String,
    pub mode: HandlerMode,
    /// `None` means the handler declared no output at all.
    pub output: Option<Json>,
}

impl Default for ErrorHandler {
    fn default() -> Self {
        Self {
            steps: Vec::new(),
            match_codes: vec!["*".to_string()],
            match_categories: Vec::new(),
            match_effects: Vec::new(),
            as_name: "error".to_string(),
            mode: HandlerMode::Rethrow,
            output: None,
        }
    }
}

impl ErrorHandler {
    /// Whether this handler claims an error with the given code/category/effect.
    pub fn matches(&self, code: &str, category: &str, effect: &str) -> bool {
        let code_matches = self.match_codes.iter().any(|pattern| match pattern.as_str() {
            "*" => true,
            pattern => match pattern.strip_suffix('*') {
                Some(prefix) => code.starts_with(prefix),
                None => pattern == code,
            },
        });
        let category_matches =
            self.match_categories.is_empty() || self.match_categories.iter().any(|c| c == category);
        let effect_matches =
            self.match_effects.is_empty() || self.match_effects.iter().any(|e| e == effect);
        code_matches && category_matches && effect_matches
    }
}

#[derive(Clone, Debug)]
pub struct SwitchCase {
    pub steps: Vec<CompiledStep>,
    /// A complete boolean expression template; never an implicit equality.
    pub when: Option<Json>,
}

/// The set of step types the v0 runtime understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepType {
    Action,
    Set,
    If,
    Switch,
    Foreach,
    While,
    Block,
    Script,
    Fail,
    Return,
}

impl StepType {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "action" => Self::Action,
            "set" => Self::Set,
            "if" => Self::If,
            "switch" => Self::Switch,
            "foreach" => Self::Foreach,
            "while" => Self::While,
            "block" => Self::Block,
            "script" => Self::Script,
            "fail" => Self::Fail,
            "return" => Self::Return,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::Set => "set",
            Self::If => "if",
            Self::Switch => "switch",
            Self::Foreach => "foreach",
            Self::While => "while",
            Self::Block => "block",
            Self::Script => "script",
            Self::Fail => "fail",
            Self::Return => "return",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompiledStep {
    pub id: String,
    pub step_type: StepType,
    /// The descriptor location, e.g. `$.steps[2].then[0]`, used in diagnostics.
    pub path: String,
    /// Normalized sibling dependencies; an omitted `depends_on` becomes the
    /// previous sibling so legacy serial descriptors keep their ordering.
    pub depends_on: Vec<String>,
    /// Every non-structural field of the step, kept as raw JSON.
    pub params: Map<String, Value>,
    pub steps: Vec<CompiledStep>,
    pub then_steps: Vec<CompiledStep>,
    pub else_steps: Vec<CompiledStep>,
    pub cases: Vec<SwitchCase>,
    pub default_steps: Vec<CompiledStep>,
    pub on_error: Option<ErrorHandler>,
    pub finally_steps: Vec<CompiledStep>,
}

impl CompiledStep {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.params.get(key)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(Value::as_str)
    }

    /// This step and every step nested inside it, in declaration order.
    pub fn walk(&self) -> Vec<&CompiledStep> {
        let mut found = Vec::new();
        self.collect(&mut found);
        found
    }

    fn collect<'a>(&'a self, found: &mut Vec<&'a CompiledStep>) {
        found.push(self);
        for group in [
            &self.steps,
            &self.then_steps,
            &self.else_steps,
            &self.default_steps,
            &self.finally_steps,
        ] {
            group.iter().for_each(|step| step.collect(found));
        }
        for case in &self.cases {
            case.steps.iter().for_each(|step| step.collect(found));
        }
        if let Some(handler) = &self.on_error {
            handler.steps.iter().for_each(|step| step.collect(found));
        }
    }
}

/// A declared input, variable or output.
#[derive(Clone, Debug, Default)]
pub struct NamedValue {
    pub schema: Option<Value>,
    pub required: bool,
    pub default: Option<Value>,
    pub sensitive: bool,
    pub mutable: bool,
    pub initial: Option<Value>,
    pub value: Option<Value>,
}

/// Execution budgets; `max_duration` and `max_executed_steps` are mandatory.
#[derive(Clone, Debug)]
pub struct Budgets {
    pub max_duration: f64,
    pub max_executed_steps: u64,
    pub cleanup_timeout: Option<f64>,
    pub max_concurrency: u32,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            max_duration: 300.0,
            max_executed_steps: 1_000,
            cleanup_timeout: None,
            max_concurrency: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowDescriptor {
    pub api_version: String,
    pub name: String,
    pub description: Option<String>,
    pub source: Option<PathBuf>,
    pub steps: Vec<CompiledStep>,
    pub metadata: Map<String, Value>,
    pub inputs: IndexMap<String, NamedValue>,
    pub variables: IndexMap<String, NamedValue>,
    pub outputs: IndexMap<String, NamedValue>,
    pub requires: Map<String, Value>,
    pub defaults: Map<String, Value>,
    pub budgets: Budgets,
    pub policy: Map<String, Value>,
    pub extensions: Map<String, Value>,
    pub on_error: Option<ErrorHandler>,
    pub finally_steps: Vec<CompiledStep>,
    /// The canonical JSON the descriptor was compiled from.
    pub raw: Value,
}

impl WorkflowDescriptor {
    /// Every step in the workflow, including handlers and finally blocks.
    pub fn all_steps(&self) -> Vec<&CompiledStep> {
        let mut found = Vec::new();
        self.steps.iter().for_each(|step| step.collect(&mut found));
        if let Some(handler) = &self.on_error {
            handler
                .steps
                .iter()
                .for_each(|step| step.collect(&mut found));
        }
        self.finally_steps
            .iter()
            .for_each(|step| step.collect(&mut found));
        found
    }
}

/// Parse a duration such as `250ms`, `2s`, `5m` or `1h` into seconds.
///
/// Only the four documented suffixes are accepted, and the value must be a
/// positive integer: a bare number or a float would be ambiguous about units.
pub fn parse_duration(value: &str) -> Option<f64> {
    let (digits, multiplier) = if let Some(head) = value.strip_suffix("ms") {
        (head, 0.001)
    } else if let Some(head) = value.strip_suffix('s') {
        (head, 1.0)
    } else if let Some(head) = value.strip_suffix('m') {
        (head, 60.0)
    } else if let Some(head) = value.strip_suffix('h') {
        (head, 3600.0)
    } else {
        return None;
    };
    if digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
        || digits == "0"
    {
        return None;
    }
    digits.parse::<f64>().ok().map(|number| number * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_the_four_documented_suffixes() {
        assert_eq!(parse_duration("250ms"), Some(0.25));
        assert_eq!(parse_duration("2s"), Some(2.0));
        assert_eq!(parse_duration("5m"), Some(300.0));
        assert_eq!(parse_duration("1h"), Some(3600.0));
    }

    #[test]
    fn durations_reject_ambiguous_or_non_positive_values() {
        for value in ["", "2", "2 s", "-1s", "0s", "01s", "2.5s", "2sec", "s"] {
            assert_eq!(parse_duration(value), None, "{value} must be rejected");
        }
    }

    #[test]
    fn handler_matching_supports_wildcards_and_prefixes() {
        let handler = ErrorHandler {
            match_codes: vec!["OCR.*".to_string()],
            ..Default::default()
        };
        assert!(handler.matches("OCR.NO_TEXT", "ocr", "not_applied"));
        assert!(!handler.matches("ACTION.TIMEOUT", "action", "unknown"));

        let any = ErrorHandler::default();
        assert!(any.matches("ANYTHING.AT_ALL", "anything", "none"));
    }

    #[test]
    fn handler_matching_requires_every_declared_dimension() {
        let handler = ErrorHandler {
            match_codes: vec!["*".to_string()],
            match_categories: vec!["action".to_string()],
            match_effects: vec!["not_applied".to_string()],
            ..Default::default()
        };
        assert!(handler.matches("ACTION.TIMEOUT", "action", "not_applied"));
        assert!(!handler.matches("ACTION.TIMEOUT", "action", "unknown"));
        assert!(!handler.matches("SCRIPT.EXIT_NONZERO", "script", "not_applied"));
    }
}
