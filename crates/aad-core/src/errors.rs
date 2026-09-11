//! Structured errors shared by the compiler, runtime, plugin host and CLI.
//!
//! The JSON shape produced by [`AutomationError::to_json`] is a stable wire
//! contract: workflows match on `code`, `category` and `effect`, never on
//! `message`.  It is a direct port of the Python `errors.py` contract so that
//! journals and transcripts written by the previous implementation stay
//! readable.

use serde_json::{json, Map, Value};

/// Derive the coarse category from a dotted error code.
///
/// `ACTION.TIMEOUT` becomes `action`; an empty code falls back to `runtime`,
/// matching the Python implementation.
fn category_of(code: &str) -> String {
    match code.split_once('.') {
        Some((head, _)) if !head.is_empty() => head.to_ascii_lowercase(),
        _ if code.is_empty() => "runtime".to_string(),
        _ => code.to_ascii_lowercase(),
    }
}

/// Where an error happened inside a workflow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorLocation {
    pub workflow: Option<String>,
    pub step_path: Option<String>,
    pub step_id: Option<String>,
    pub attempt: Option<u32>,
}

impl ErrorLocation {
    fn is_empty(&self) -> bool {
        self.workflow.is_none()
            && self.step_path.is_none()
            && self.step_id.is_none()
            && self.attempt.is_none()
    }

    fn to_json(&self) -> Value {
        let mut map = Map::new();
        if let Some(value) = &self.workflow {
            map.insert("workflow".into(), Value::String(value.clone()));
        }
        if let Some(value) = &self.step_path {
            map.insert("step_path".into(), Value::String(value.clone()));
        }
        if let Some(value) = &self.step_id {
            map.insert("step_id".into(), Value::String(value.clone()));
        }
        if let Some(value) = self.attempt {
            map.insert("attempt".into(), Value::from(value));
        }
        Value::Object(map)
    }
}

/// One descriptor validation problem, addressed by a JSON-ish path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorIssue {
    pub path: String,
    pub message: String,
    pub code: String,
}

impl DescriptorIssue {
    pub fn new(
        path: impl Into<String>,
        message: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            code: code.into(),
        }
    }

    /// An issue with the default `invalid` code.
    pub fn invalid(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(path, message, "invalid")
    }

    pub fn to_json(&self) -> Value {
        json!({ "path": self.path, "message": self.message, "code": self.code })
    }
}

/// A stable, machine-readable workflow error.
#[derive(Clone, Debug)]
pub struct AutomationError {
    pub code: String,
    pub message: String,
    pub category: String,
    pub phase: Option<String>,
    pub retryable: bool,
    /// One of `none`, `not_applied`, `applied` or `unknown`.
    pub effect: String,
    pub location: ErrorLocation,
    pub details: Map<String, Value>,
    pub cause: Option<Box<AutomationError>>,
    /// Raw cause for non-`AutomationError` sources, kept verbatim.
    pub raw_cause: Option<Value>,
    pub suppressed: Vec<AutomationError>,
    /// Populated only for descriptor errors; also mirrored into `details`.
    pub issues: Vec<DescriptorIssue>,
}

impl AutomationError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let category = category_of(&code);
        Self {
            code,
            message: message.into(),
            category,
            phase: None,
            retryable: false,
            effect: "none".to_string(),
            location: ErrorLocation::default(),
            details: Map::new(),
            cause: None,
            raw_cause: None,
            suppressed: Vec::new(),
            issues: Vec::new(),
        }
    }

    /// Build the `DESCRIPTOR.INVALID` error carrying compile-time issues.
    pub fn descriptor(issues: Vec<DescriptorIssue>) -> Self {
        Self::descriptor_with(
            "Workflow descriptor is invalid",
            "DESCRIPTOR.INVALID",
            issues,
        )
    }

    pub fn descriptor_with(
        message: impl Into<String>,
        code: impl Into<String>,
        issues: Vec<DescriptorIssue>,
    ) -> Self {
        let mut error = Self::new(code, message);
        error.category = "descriptor".to_string();
        error.phase = Some("compile".to_string());
        error.details.insert(
            "issues".into(),
            Value::Array(issues.iter().map(DescriptorIssue::to_json).collect()),
        );
        error.issues = issues;
        error
    }

    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = category.into();
        self
    }

    pub fn with_phase(mut self, phase: impl Into<String>) -> Self {
        self.phase = Some(phase.into());
        self
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_effect(mut self, effect: impl Into<String>) -> Self {
        self.effect = effect.into();
        self
    }

    pub fn with_detail(mut self, key: impl Into<String>, value: Value) -> Self {
        self.details.insert(key.into(), value);
        self
    }

    pub fn with_details(mut self, details: Map<String, Value>) -> Self {
        self.details = details;
        self
    }

    pub fn with_cause(mut self, cause: AutomationError) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    pub fn add_suppressed(&mut self, error: AutomationError) {
        self.suppressed.push(error);
    }

    /// Attach location information, without overwriting values already set.
    ///
    /// The innermost scope wins, exactly as in the Python runtime: an error
    /// bubbling outwards keeps the step that actually produced it.
    pub fn at_step(
        mut self,
        step_id: &str,
        step_path: Option<&str>,
        attempt: Option<u32>,
        workflow: Option<&str>,
    ) -> Self {
        if self.location.step_id.is_none() {
            self.location.step_id = Some(step_id.to_string());
        }
        if self.location.step_path.is_none() {
            self.location.step_path = Some(step_path.unwrap_or(step_id).to_string());
        }
        if self.location.attempt.is_none() {
            self.location.attempt = attempt;
        }
        if self.location.workflow.is_none() {
            self.location.workflow = workflow.map(str::to_string);
        }
        self
    }

    pub fn to_json(&self) -> Value {
        let cause = match (&self.cause, &self.raw_cause) {
            (Some(inner), _) => inner.to_json(),
            (None, Some(raw)) => raw.clone(),
            (None, None) => Value::Null,
        };
        let mut map = Map::new();
        map.insert("schema_version".into(), Value::String("1".into()));
        map.insert("code".into(), Value::String(self.code.clone()));
        map.insert("category".into(), Value::String(self.category.clone()));
        map.insert("message".into(), Value::String(self.message.clone()));
        map.insert("retryable".into(), Value::Bool(self.retryable));
        map.insert("effect".into(), Value::String(self.effect.clone()));
        map.insert("details".into(), Value::Object(self.details.clone()));
        map.insert("cause".into(), cause);
        map.insert(
            "suppressed".into(),
            Value::Array(self.suppressed.iter().map(Self::to_json).collect()),
        );
        if let Some(phase) = &self.phase {
            map.insert("phase".into(), Value::String(phase.clone()));
        }
        if !self.location.is_empty() {
            map.insert("location".into(), self.location.to_json());
        }
        Value::Object(map)
    }

    /// Rebuild a structured error from its stable v1 JSON representation.
    pub fn from_json(value: &Value) -> Option<Self> {
        let map = value.as_object()?;
        if map.get("schema_version").and_then(Value::as_str) != Some("1") {
            return None;
        }
        let code = map.get("code")?.as_str()?;
        let category = map.get("category")?.as_str()?;
        let message = map.get("message")?.as_str()?;
        let retryable = map.get("retryable")?.as_bool()?;
        let effect = map.get("effect")?.as_str()?;
        let details = map.get("details")?.as_object()?.clone();
        let suppressed = map
            .get("suppressed")?
            .as_array()?
            .iter()
            .map(Self::from_json)
            .collect::<Option<Vec<_>>>()?;
        let (cause, raw_cause) = match map.get("cause") {
            None | Some(Value::Null) => (None, None),
            Some(value) => match Self::from_json(value) {
                Some(error) => (Some(Box::new(error)), None),
                None => (None, Some(value.clone())),
            },
        };
        let location = match map.get("location") {
            None => ErrorLocation::default(),
            Some(Value::Object(location)) => ErrorLocation {
                workflow: optional_string(location, "workflow")?,
                step_path: optional_string(location, "step_path")?,
                step_id: optional_string(location, "step_id")?,
                attempt: match location.get("attempt") {
                    None => None,
                    Some(value) => Some(u32::try_from(value.as_u64()?).ok()?),
                },
            },
            Some(_) => return None,
        };
        let allowed: std::collections::BTreeSet<&str> = [
            "schema_version",
            "code",
            "category",
            "message",
            "retryable",
            "effect",
            "details",
            "cause",
            "suppressed",
            "phase",
            "location",
        ]
        .into_iter()
        .collect();
        if map.keys().any(|key| !allowed.contains(key.as_str())) {
            return None;
        }
        Some(Self {
            code: code.to_string(),
            message: message.to_string(),
            category: category.to_string(),
            phase: optional_string(map, "phase")?,
            retryable,
            effect: effect.to_string(),
            location,
            details,
            cause,
            raw_cause,
            suppressed,
            issues: Vec::new(),
        })
    }
}

fn optional_string(map: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    match map.get(key) {
        None => Some(None),
        Some(Value::String(value)) => Some(Some(value.clone())),
        Some(_) => None,
    }
}

impl std::fmt::Display for AutomationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AutomationError {}

pub type Result<T> = std::result::Result<T, AutomationError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_is_derived_from_the_code_prefix() {
        assert_eq!(
            AutomationError::new("ACTION.TIMEOUT", "x").category,
            "action"
        );
        assert_eq!(AutomationError::new("", "x").category, "runtime");
    }

    #[test]
    fn serialized_error_carries_the_stable_v1_fields() {
        let error = AutomationError::new("ACTION.UNKNOWN_EFFECT", "unclear")
            .with_effect("unknown")
            .with_phase("verify")
            .at_step("submit", None, Some(1), Some("example"));
        let value = error.to_json();

        assert_eq!(value["schema_version"], "1");
        assert_eq!(value["code"], "ACTION.UNKNOWN_EFFECT");
        assert_eq!(value["category"], "action");
        assert_eq!(value["effect"], "unknown");
        assert_eq!(value["phase"], "verify");
        assert_eq!(value["retryable"], false);
        assert_eq!(value["location"]["step_path"], "submit");
        assert_eq!(value["location"]["attempt"], 1);
        assert_eq!(value["cause"], Value::Null);
        assert_eq!(value["suppressed"], json!([]));
    }

    #[test]
    fn a_serialized_error_round_trips_for_durable_recovery() {
        let mut error = AutomationError::new("TEST.FAIL", "root")
            .with_category("test")
            .with_phase("execute")
            .with_retryable(true)
            .with_effect("not_applied")
            .with_detail("field", json!("value"))
            .with_cause(AutomationError::new("TEST.CAUSE", "cause"))
            .at_step("step", Some("$.steps[0]"), Some(2), Some("workflow"));
        error.add_suppressed(AutomationError::new("TEST.CLEANUP", "cleanup"));
        let value = error.to_json();

        let restored = AutomationError::from_json(&value).expect("valid v1 error");

        assert_eq!(restored.to_json(), value);
    }

    #[test]
    fn malformed_serialized_errors_are_rejected() {
        let mut value = AutomationError::new("TEST.FAIL", "root").to_json();
        value["retryable"] = json!("yes");
        assert!(AutomationError::from_json(&value).is_none());
    }

    #[test]
    fn at_step_keeps_the_innermost_location() {
        let error = AutomationError::new("ACTION.EXECUTION_FAILED", "x")
            .at_step("inner", Some("outer.inner"), Some(2), Some("wf"))
            .at_step("outer", Some("outer"), Some(1), Some("wf"));

        assert_eq!(error.location.step_id.as_deref(), Some("inner"));
        assert_eq!(error.location.step_path.as_deref(), Some("outer.inner"));
        assert_eq!(error.location.attempt, Some(2));
    }

    #[test]
    fn descriptor_errors_expose_issues_in_details() {
        let error = AutomationError::descriptor(vec![DescriptorIssue::invalid("$.steps", "empty")]);
        let value = error.to_json();

        assert_eq!(value["code"], "DESCRIPTOR.INVALID");
        assert_eq!(value["category"], "descriptor");
        assert_eq!(value["phase"], "compile");
        assert_eq!(value["details"]["issues"][0]["path"], "$.steps");
        assert_eq!(value["details"]["issues"][0]["code"], "invalid");
    }

    #[test]
    fn suppressed_errors_are_nested_not_flattened() {
        let mut error = AutomationError::new("WORKFLOW.FINALLY_FAILED", "cleanup failed");
        error.add_suppressed(AutomationError::new("ACTION.TIMEOUT", "slow"));
        let value = error.to_json();

        assert_eq!(value["suppressed"].as_array().unwrap().len(), 1);
        assert_eq!(value["suppressed"][0]["code"], "ACTION.TIMEOUT");
    }
}
