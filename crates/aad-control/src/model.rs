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
    /// The visible text of the page or of one region, with tables as rows.
    /// Read-only; bounded by max_characters.
    Text,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    #[serde(default)]
    pub mode: ViewMode,
    /// Outline or text: a region named by an overview.
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
    /// Window actions, offered on an observed top-level window's own node.
    Minimize {},
    Maximize {},
    /// Back to the normal size from minimized or maximized.
    Restore {},
    /// Ask the window to close, as its close button would; it may prompt.
    CloseWindow {},
    /// Bring the window to the foreground, restoring it if minimized.
    Activate {},
    /// Paste text through the clipboard into a focusable element, then put
    /// the clipboard back as it was.
    Paste {
        text: String,
    },
    /// Open an http(s) address in a new tab of the observed page's browser.
    OpenTab {
        url: String,
    },
    /// Bring the observed page to the front.
    ActivatePage {},
    /// Close the observed page.
    ClosePage {},
    /// Accept the JavaScript dialog an observation shows; a prompt takes `text`.
    Accept {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Dismiss (cancel) the JavaScript dialog an observation shows.
    Dismiss {},
    /// Put these local files into a file input. Absolute paths under a
    /// directory the server operator allowed; nothing else is readable.
    Upload {
        files: Vec<String>,
    },
    /// Emulate a device class and/or colour scheme on the observed page, for
    /// as long as this session's connection to it lasts.
    Emulate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preset: Option<DevicePreset>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color_scheme: Option<ColorScheme>,
    },
    /// Clear the observed page's site data: cookies and storage of its origin.
    ClearSiteData {},
}

/// A device class to emulate: viewport, scale and touch come from the preset.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DevicePreset {
    Desktop,
    Tablet,
    Mobile,
    /// Back to the browser's own window.
    Reset,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColorScheme {
    Light,
    Dark,
    NoPreference,
}

pub const MAX_UPLOAD_FILES: usize = 32;

pub const ACTION_NAMES: [&str; 29] = [
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
    "minimize",
    "maximize",
    "restore",
    "close_window",
    "activate",
    "paste",
    "open_tab",
    "activate_page",
    "close_page",
    "accept",
    "dismiss",
    "upload",
    "emulate",
    "clear_site_data",
];

/// Actions on the page as a whole, offered on its document node.
pub const PAGE_ACTIONS: [&str; 9] = [
    "navigate",
    "back",
    "forward",
    "reload",
    "open_tab",
    "activate_page",
    "close_page",
    "emulate",
    "clear_site_data",
];

/// Page actions that address the browser's tabs rather than one document.
pub const TAB_ACTIONS: [&str; 3] = ["open_tab", "activate_page", "close_page"];

/// Actions on a desktop window as a whole, offered on its own node.
pub const WINDOW_ACTIONS: [&str; 5] = [
    "minimize",
    "maximize",
    "restore",
    "close_window",
    "activate",
];

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
            Self::Minimize {} => "minimize",
            Self::Maximize {} => "maximize",
            Self::Restore {} => "restore",
            Self::CloseWindow {} => "close_window",
            Self::Activate {} => "activate",
            Self::Paste { .. } => "paste",
            Self::OpenTab { .. } => "open_tab",
            Self::ActivatePage {} => "activate_page",
            Self::ClosePage {} => "close_page",
            Self::Accept { .. } => "accept",
            Self::Dismiss {} => "dismiss",
            Self::Upload { .. } => "upload",
            Self::Emulate { .. } => "emulate",
            Self::ClearSiteData {} => "clear_site_data",
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
    /// The text one observed element shows, read by OCR from its pixels.
    /// Read-only; recognised lines are not targets.
    ReadText {
        target: Target,
        /// A BCP-47 tag naming the OCR language; the user's languages otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Start a program the server operator allowlisted, then wait (bounded)
    /// for its windows and answer them as targets. Never through a shell.
    Launch {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        /// How long to wait for a window, at most the request's timeout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_ms: Option<u64>,
    },
    /// Press a key combination wherever keyboard focus is -- Win+D, Alt+Tab --
    /// anchored on no observed element. The one un-anchored input, refused
    /// unless the operator granted it separately; its effect is never verified.
    GlobalKey {
        keys: Vec<String>,
    },
    /// The clipboard's text, bounded. Read-only, but refused unless the server
    /// operator granted it separately: the clipboard often holds secrets.
    ClipboardRead {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_characters: Option<usize>,
    },
    /// Observe a resource repeatedly until a locator's element appears,
    /// disappears or reaches the given states, within the request deadline.
    Wait {
        resource_id: String,
        locator: Box<Locator>,
        condition: WaitCondition,
        /// For `state`: every key must equal the found node's `states` value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        states: Option<Value>,
        #[serde(default = "default_nodes")]
        max_nodes: usize,
        /// Pause between observations; defaults to 250 ms.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u64>,
    },
    Cancel {
        request_id: String,
    },
    Close {},
}

/// What `wait` waits for. An ambiguous match is never "found".
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitCondition {
    /// Exactly one node matches.
    Appears,
    /// No node matches.
    Disappears,
    /// Exactly one node matches and its states equal the given ones.
    State,
}

pub const DEFAULT_WAIT_INTERVAL_MS: u64 = 250;
pub const MIN_WAIT_INTERVAL_MS: u64 = 50;
pub const MAX_WAIT_INTERVAL_MS: u64 = 5_000;

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
        matches!(
            self.operation,
            Operation::Act { .. } | Operation::Launch { .. } | Operation::GlobalKey { .. }
        )
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
