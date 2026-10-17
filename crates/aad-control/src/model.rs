//! Transport-independent control contract. Provider-native snapshots stay opaque.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "aad-control/1";
pub const MAX_TIMEOUT_MS: u64 = 30_000;
pub const MAX_NODES: usize = 2_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub session_id: String,
    pub resource_id: String,
    pub observation_id: String,
    pub node_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Focus {},
    Invoke {},
    Click {},
    Fill { value: String },
    TypeText { text: String },
    Key { keys: Vec<String> },
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Focus {} => "focus",
            Self::Invoke {} => "invoke",
            Self::Click {} => "click",
            Self::Fill { .. } => "fill",
            Self::TypeText { .. } => "type_text",
            Self::Key { .. } => "key",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Capabilities {},
    Targets {},
    Observe {
        resource_id: String,
        #[serde(default = "default_nodes")]
        max_nodes: usize,
    },
    Find {
        observation_id: String,
        locator: Locator,
    },
    Act {
        target: Target,
        action: Action,
    },
    Cancel {
        request_id: String,
    },
    Close {},
}

fn default_nodes() -> usize {
    1_000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    pub operation: Operation,
}

fn default_timeout() -> u64 {
    10_000
}

impl Request {
    pub fn new(session_id: Option<String>, operation: Operation) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id,
            timeout_ms: default_timeout(),
            operation,
        }
    }
    pub fn mutating(&self) -> bool {
        matches!(self.operation, Operation::Act { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub protocol_version: String,
    pub request_id: String,
    pub session_id: String,
    pub effect: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resource {
    pub resource_id: String,
    pub provider: String,
    pub kind: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub metadata: Value,
}
