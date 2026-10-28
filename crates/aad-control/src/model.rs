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

/// Every given field must match. `role`, `name` and `node_id` work on every
/// provider; the rest are provider extensions and are refused, not ignored,
/// where the provider cannot evaluate them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<Value>,
    /// `exact` (default) or `contains`.
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_mode: Option<String>,
    /// Another locator naming the container to search inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nth: Option<Value>,
}

impl Locator {
    pub fn is_empty(&self) -> bool {
        self.role.is_none() && self.name.is_none() && self.node_id.is_none() && !self.extended()
    }
    /// Whether any field beyond the portable exact trio is present.
    pub fn extended(&self) -> bool {
        self.value.is_some()
            || self.automation_id.is_some()
            || self.class_name.is_some()
            || self.framework_id.is_some()
            || self.states.is_some()
            || self.match_mode.is_some()
            || self.within.is_some()
            || self.near.is_some()
            || self.nth.is_some()
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Expect {
    /// A miss or several matches is an error.
    #[default]
    One,
    /// A miss is an ordinary `found: false` answer; several matches still fail.
    Optional,
}

/// What an observation returns to the caller. The full capture is retained
/// either way: views only shape the answer, never what may be targeted.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// Every captured node.
    #[default]
    Nodes,
    /// Actionable or named elements only, bounded by `limit` and `max_characters`.
    Outline,
    /// Regions and their sizes without their contents.
    Overview,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    #[serde(default)]
    pub mode: ViewMode,
    /// Outline only: a region named by an overview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_characters: Option<usize>,
}

pub const MAX_OUTLINE_LIMIT: usize = 500;
pub const DEFAULT_OUTLINE_LIMIT: usize = 80;
pub const DEFAULT_OUTLINE_CHARACTERS: usize = 20_000;
pub const MIN_OUTLINE_CHARACTERS: usize = 500;
pub const MAX_OUTLINE_CHARACTERS: usize = 200_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypeMode {
    Unicode,
    Keys,
}

/// A position inside the observed element's box, never a screen coordinate.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Fraction {
    pub fx: f64,
    pub fy: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Focus {},
    Invoke {},
    /// A plain left click unless a button, a double click or a position is given.
    Click {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<Button>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<Fraction>,
    },
    Fill {
        value: String,
    },
    TypeText {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<TypeMode>,
    },
    Key {
        keys: Vec<String>,
    },
    Scroll {
        direction: Direction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notches: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<Fraction>,
    },
    Hover {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<Fraction>,
    },
    /// Both ends are nodes of the same observation.
    Drag {
        to: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<Button>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<Fraction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_fraction: Option<Fraction>,
    },
    /// Choose one option of a list box by its label or value.
    Select {
        option: String,
    },
    /// Set a checkbox, switch or radio button to this state.
    Check {
        checked: bool,
    },
    /// Load an http(s) address in the observed page.
    Navigate {
        url: String,
    },
    Back {},
    Forward {},
    Reload {},
}

pub const ACTION_NAMES: [&str; 15] = [
    "focus",
    "invoke",
    "click",
    "fill",
    "type_text",
    "key",
    "scroll",
    "hover",
    "drag",
    "select",
    "check",
    "navigate",
    "back",
    "forward",
    "reload",
];

/// Actions on the page as a whole, offered on its document node.
pub const PAGE_ACTIONS: [&str; 4] = ["navigate", "back", "forward", "reload"];

impl Action {
    /// A plain left click.
    pub fn click() -> Self {
        Self::Click {
            button: None,
            count: None,
            fraction: None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Focus {} => "focus",
            Self::Invoke {} => "invoke",
            Self::Click { .. } => "click",
            Self::Fill { .. } => "fill",
            Self::TypeText { .. } => "type_text",
            Self::Key { .. } => "key",
            Self::Scroll { .. } => "scroll",
            Self::Hover { .. } => "hover",
            Self::Drag { .. } => "drag",
            Self::Select { .. } => "select",
            Self::Check { .. } => "check",
            Self::Navigate { .. } => "navigate",
            Self::Back {} => "back",
            Self::Forward {} => "forward",
            Self::Reload {} => "reload",
        }
    }
    /// A left single click at the centre needs nothing beyond a plain click.
    pub fn plain_click(&self) -> bool {
        matches!(
            self,
            Self::Click {
                button: None | Some(Button::Left),
                count: None | Some(1),
                fraction: None,
            }
        )
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
        #[serde(default)]
        view: View,
    },
    Find {
        observation_id: String,
        locator: Box<Locator>,
        #[serde(default)]
        expect: Expect,
    },
    Act {
        target: Target,
        action: Action,
    },
    /// A PNG of one observed element, read-only.
    Capture {
        target: Target,
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
    pub commit_state: CommitState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// Shared with mobile control. Native effect details remain in `effect` and
/// the structured error; this coarse state must never enable unknown retries.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitState {
    NotCommitted,
    Committed,
    Unknown,
}
impl CommitState {
    pub fn from_effect(effect: &str) -> Self {
        match effect {
            "not_applied" => Self::NotCommitted,
            "applied" | "contextual" => Self::Committed,
            _ => Self::Unknown,
        }
    }
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
