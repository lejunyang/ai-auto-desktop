//! Provider-independent tools. Schemas and dispatch both reject extra fields.
//!
//! These are the only way an agent looks at or acts on the desktop: list
//! targets, observe one, find an element, act on the session-bound target the
//! observation returned. Every position is a fraction of an observed element's
//! box, never a screen coordinate.
use crate::tools::Tool;
use aad_control::{transport::Endpoint, Operation, Request, Response};
use serde_json::{json, Map, Value};

fn target_schema() -> Value {
    let mut properties = Map::new();
    for field in ["session_id", "resource_id", "observation_id", "node_id"] {
        properties.insert(
            field.into(),
            json!({"type":"string","minLength":1,"maxLength":256}),
        );
    }
    json!({
        "type": "object",
        "description": "A target exactly as control_observe or control_find returned it. \
    Quoting it proves the element was observed in this session.",
        "properties": properties,
        "required": ["session_id", "resource_id", "observation_id", "node_id"],
        "additionalProperties": false
    })
}

fn fraction_schema(what: &str) -> Value {
    json!({
        "type": "object",
        "description": format!("{what}, as a fraction of the element's box: fx and fy each \
    from 0.0 (left/top) to 1.0 (right/bottom). Omit for the centre. A position within the \
    observed element, not a screen location."),
        "properties": {
            "fx": {"type": "number", "minimum": 0.0, "maximum": 1.0},
            "fy": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["fx", "fy"],
        "additionalProperties": false
    })
}

fn button_schema() -> Value {
    json!({"enum": ["left", "right", "middle"], "description": "Which button. Defaults to left."})
}

fn action_schema() -> Value {
    let variant = |name: &str, description: &str, properties: Value, required: &[&str]| {
        let mut all = Map::from_iter([("type".to_string(), json!({"const": name}))]);
        all.extend(properties.as_object().unwrap().clone());
        let mut required_all = vec!["type"];
        required_all.extend_from_slice(required);
        json!({
            "type": "object",
            "description": description,
            "properties": all,
            "required": required_all,
            "additionalProperties": false
        })
    };
    json!({"oneOf": [
        variant("focus", "Give keyboard focus to the element.", json!({}), &[]),
        variant("invoke", "Activate the element's default action, such as pressing a \
    button. Prefer this over click where offered: it does not depend on window position or \
    z-order.", json!({}), &[]),
        variant("click", "Click the element. A plain left click unless a button, a double \
    click or a position is given; those need a provider that offers gestures. In a browser a \
    plain click is DOM activation (isTrusted false, works in frames); giving a position, a \
    button or a count sends trusted pointer input instead, main frame only, and is refused if \
    something else covers the element.", json!({
            "button": button_schema(),
            "count": {"type": "integer", "enum": [1, 2], "description": "1 for a single click, 2 for a double click."},
            "fraction": fraction_schema("Where inside the element to click")
        }), &[]),
        variant("fill", "Replace the element's value directly. Prefer this over \
    type_text for text fields: it is atomic and does not depend on focus.", json!({
            "value": {"type": "string", "maxLength": 4096}
        }), &["value"]),
        variant("type_text", "Type text as keystrokes after focusing the element. Use \
    only when fill is unsupported, since keystrokes go wherever focus lands.", json!({
            "text": {"type": "string", "maxLength": 1024},
            "mode": {
                "enum": ["unicode", "keys"],
                "default": "unicode",
                "description": "unicode types arbitrary text. keys maps characters through \
    the target window's keyboard layout into virtual-key presses, which Raw Input consumers \
    can see; it rejects characters the layout cannot express."
            }
        }), &["text"]),
        variant("key", "Press a key combination on the element after focusing it: \
    shortcuts and keys no text can express, such as Ctrl+Z, F5, Tab or Escape. Modifiers \
    (ctrl, shift, alt, win) followed by exactly one main key. Browsers accept only \
    navigation and editing keys.", json!({
            "keys": {"type": "array", "items": {"type": "string", "maxLength": 32}, "minItems": 1, "maxItems": 6}
        }), &["keys"]),
        variant("scroll", "Scroll the wheel over the element. In a browser, offered on the \
    page body and on scrollable containers of the main frame.", json!({
            "direction": {"enum": ["up", "down", "left", "right"]},
            "notches": {"type": "integer", "minimum": 1, "maximum": 30, "description": "Wheel detents. Defaults to 3."},
            "fraction": fraction_schema("Where inside the element to scroll")
        }), &["direction"]),
        variant("hover", "Move onto the element and dwell, e.g. to raise a tooltip or a \
    menu.", json!({
            "ms": {"type": "integer", "minimum": 1, "maximum": 5000, "description": "How long to dwell. Defaults to 500."},
            "fraction": fraction_schema("Where inside the element to hover")
        }), &[]),
        variant("drag", "Press on the element, glide, release on another element of the \
    same observation. Use for reordering a list or moving an item between panes.", json!({
            "to": target_schema(),
            "button": button_schema(),
            "fraction": fraction_schema("Where inside the source element to grab"),
            "to_fraction": fraction_schema("Where inside the destination element to release")
        }), &["to"]),
        variant("select", "Choose one option of a list box (a browser <select>), by its \
    label as the observation lists it in `options`, or by its value.", json!({
            "option": {"type": "string", "minLength": 1, "maxLength": 1024}
        }), &["option"]),
        variant("check", "Set a checkbox, switch or radio button to checked or unchecked. \
    Nothing is clicked when it is already in that state; a radio button is cleared by checking \
    another one.", json!({
            "checked": {"type": "boolean"}
        }), &["checked"]),
        variant("navigate", "Load an http or https address in the observed browser page. \
    Offered on the page's document node of the main frame.", json!({
            "url": {"type": "string", "minLength": 8, "maxLength": 2048}
        }), &["url"]),
        variant("back", "Go back one page in the observed browser page's history.", json!({}), &[]),
        variant("forward", "Go forward one page in the observed browser page's history.", json!({}), &[]),
        variant("reload", "Reload the observed browser page.", json!({}), &[]),
        variant("open_tab", "Open an http or https address in a new tab of the observed \
    page's browser. Offered on the page's document node. The answer names the new page_id; \
    call control_targets to observe it.", json!({
            "url": {"type": "string", "minLength": 8, "maxLength": 2048}
        }), &["url"]),
        variant("activate_page", "Bring the observed browser page (tab) to the front. \
    Offered on the page's document node.", json!({}), &[]),
        variant("close_page", "Close the observed browser page (tab or popup). Offered on \
    the page's document node; its targets stop resolving.", json!({}), &[]),
        variant("accept", "Accept (OK) the JavaScript dialog -- alert, confirm, prompt or \
    leave-page -- that blocks a browser page. While one is open, observing the page shows \
    only that dialog, as a node with role alertdialog. A prompt takes the answer as `text`; \
    without it the prompt's default is used.", json!({
            "text": {"type": "string", "maxLength": 4096}
        }), &[]),
        variant("dismiss", "Dismiss (Cancel) the JavaScript dialog that blocks a browser \
    page, observed as an alertdialog node.", json!({}), &[]),
    ]})
}

/// A control action as a saved step stores it: a drag names its destination
/// in the step's `to` locator instead of a session-bound target.
pub(crate) fn saved_action_schema() -> Value {
    let mut schema = action_schema();
    for variant in schema["oneOf"].as_array_mut().into_iter().flatten() {
        // The text may come from the step's `input` instead.
        if matches!(
            variant["properties"]["type"]["const"].as_str(),
            Some("fill" | "type_text")
        ) {
            variant["required"] = json!(["type"]);
        }
        if variant["properties"]["type"]["const"] == "drag" {
            if let Some(properties) = variant["properties"].as_object_mut() {
                properties.remove("to");
            }
            variant["required"] = json!(["type"]);
            variant["description"] = json!(
                "Press on the element, glide, and release on the element the step's \
    `to` locator finds in the same application."
            );
        }
    }
    schema["description"] = json!(
        "The control_act action you performed, without its target. For fill and \
    type_text with an `input`, leave the text out."
    );
    schema
}

fn view_schema() -> Value {
    json!({
        "type": "object",
        "description": "How much of the observation to return. The whole capture is kept \
    either way, so any node of it can be found or acted on.",
        "properties": {
            "mode": {
                "enum": ["outline", "overview", "nodes"],
                "default": "outline",
                "description": "outline (default): elements you can act on or read, \
    bounded by limit and max_characters. overview: the window's regions and their sizes \
    without their contents -- start here on any window of substance. Measured across one \
    desktop, nine windows out of twenty held more interactive elements than an outline \
    returns, the largest 367; an overview cost about 2000 characters where listing 500 \
    elements cost 127000. Regions are computed for Windows windows; elsewhere use outline. \
    nodes: every captured node, for callers that need the raw tree."
            },
            "region": {
                "type": "string",
                "description": "Outline only: list just this region, named as the overview \
    reports it. It is the same name a locator carries in `within`, so a region drilled into \
    and a locator written against it agree."
            },
            "limit": {
                "type": "integer", "minimum": 1, "maximum": 500,
                "description": "Outline only: maximum elements (default 80). `matched` in \
    the answer says how many were eligible, so it tells you whether raising this would show more."
            },
            "max_characters": {
                "type": "integer", "minimum": 500, "maximum": 200000,
                "description": "Outline only: stop after roughly this many characters \
    (default 20000). An element count does not bound the size -- elements ran from 169 to \
    4890 characters, and 500 of them reached 188147 on one window. When the outline is cut, \
    `stopped_by` says which ceiling bit: raising `limit` helps when it says `limit` and does \
    nothing when it says `characters`, where narrowing to a region is the way forward."
            }
        },
        "additionalProperties": false
    })
}

fn locator_schema() -> Value {
    let mut locator = crate::tools::locator_schema();
    locator["properties"]["node_id"] = json!({
        "type": "string",
        "description": "A node_id from this observation. Works on every provider."
    });
    locator["description"] = json!(format!(
        "{} role, name and node_id work on every provider; the other fields are \
evaluated by the Windows provider and refused elsewhere.",
        locator["description"].as_str().unwrap_or_default()
    ));
    locator
}

pub fn catalogue() -> Vec<Tool> {
    let definitions = [
        (
            "control_capabilities",
            "Start here: this session's providers, the actions each offers and whether the \
server operator granted input. Unsupported capabilities are never synthesized.",
            json!({}),
            vec![],
            false,
        ),
        (
            "control_targets",
            "List the desktop windows or applications and the browser pages or frames this \
session may observe. Resource IDs are chosen by the server, not given as OS handles or URLs. \
Browser pages carry metadata.page_id; a tab or popup the page opened (window.open, \
target=_blank, a sign-in popup) carries metadata.opener_page_id naming its opener. Call \
this again after an action may have opened or closed a page.",
            json!({}),
            vec![],
            false,
        ),
        (
            "control_observe",
            "Observe one resource from control_targets. Answers with session-bound targets \
for the nodes it shows, plus their capabilities. On a crowded window ask for an overview \
first, then an outline of one region. A new observation of the same provider, or any \
input attempt, invalidates earlier targets.",
            json!({
                "resource_id": {"type": "string"},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Capture budget (default 1000). A capture that hits it is truncated and cannot be used to find or act."},
                "view": view_schema()
            }),
            vec!["resource_id"],
            false,
        ),
        (
            "control_find",
            "Find exactly one node in an existing observation and return its target. \
Several matches fail with the candidates listed so the locator can be narrowed; never \
the first one. To ask whether something has gone away, pass expect: optional and read \
`found`.",
            json!({
                "observation_id": {"type": "string"},
                "locator": locator_schema(),
                "expect": {
                    "enum": ["one", "optional"],
                    "default": "one",
                    "description": "one: a miss is an error, which is what you want when \
            acquiring something to act on. optional: a miss is an ordinary answer with found=false, \
            for asking whether a dialog has closed or a spinner has gone. Neither lets an ambiguous \
            match through."
                }
            }),
            vec!["observation_id", "locator"],
            false,
        ),
        (
            "control_wait",
            "Wait for something to happen on one resource from control_targets: observe it \
again and again (every interval_ms) until the locator's element appears, disappears, or \
reaches the given states, or timeout_ms runs out (CONTROL.WAIT_TIMEOUT). Use it after an \
action that loads, opens or closes something, instead of observing in a loop yourself. \
When the element is found the answer carries its target, from an observation that stays \
current, so control_act can use it directly. Several matches are never treated as found; \
narrow the locator. control_cancel stops a wait.",
            json!({
                "resource_id": {"type": "string"},
                "locator": locator_schema(),
                "condition": {
                    "enum": ["appears", "disappears", "state"],
                    "description": "appears: exactly one element matches. disappears: none \
            matches. state: exactly one matches and its `states` equal those given in `states`."
                },
                "states": {
                    "type": "object",
                    "description": "For condition state: state names and values the element \
            must show, as observations report them in `states`, e.g. {\"enabled\": true} or \
            {\"checked\": true}.",
                    "minProperties": 1,
                    "maxProperties": 16,
                    "additionalProperties": {"type": ["boolean", "string", "number"]}
                },
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Capture budget of each observation (default 1000)."},
                "interval_ms": {"type": "integer", "minimum": 50, "maximum": 5000, "description": "Pause between observations (default 250)."}
            }),
            vec!["resource_id", "locator", "condition"],
            false,
        ),
        (
            "control_act",
            "Perform a typed action on an observed, freshly revalidated target. Every \
attempt consumes that provider's observation, so observe again before the next one. \
Submission is not proof the task succeeded: observe again and check. Never retry an \
unknown-effect write automatically.",
            json!({"target": target_schema(), "action": action_schema()}),
            vec!["target", "action"],
            true,
        ),
        (
            "control_capture",
            "Take a PNG screenshot of one observed element, e.g. a login QR code to show a person, or the page body for the whole page. Read-only. The answer carries the image and a `digest` of it: capture again later and compare digests to tell whether what is shown has changed (a QR code that refreshed) without looking at the image. Browser pages only for now; password fields are refused.",
            json!({"target": target_schema()}),
            vec!["target"],
            false,
        ),
        (
            "control_cancel",
            "Request cancellation of an in-flight control request. If input was already \
dispatched, its effect is unknown; cancellation never means rollback.",
            json!({"target_request_id": {"type": "string"}}),
            vec!["target_request_id"],
            false,
        ),
        (
            "control_close",
            "Close the control session, invalidate its observations and request \
cancellation of all in-flight work.",
            json!({}),
            vec![],
            false,
        ),
    ];
    definitions
        .into_iter()
        .map(|(name, description, properties, required, mutating)| {
            let mut properties = properties.as_object().unwrap().clone();
            properties.insert(
                "request_id".into(),
                json!({"type":"string","minLength":1,"maxLength":128}),
            );
            properties.insert(
                "timeout_ms".into(),
                json!({"type":"integer","minimum":1,"maximum":30000,"default":10000}),
            );
            Tool {
                name,
                description,
                schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
                mutating,
            }
        })
        .collect()
}

pub fn prepare(name: &str, arguments: &Value, endpoint: &dyn Endpoint) -> Result<Request, String> {
    let mut args = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| "arguments must be an object".to_string())?;
    let mut request = Request::new(
        Some(endpoint.session_id().into()),
        Operation::Capabilities {},
    );
    if let Some(id) = args.remove("request_id") {
        request.request_id = id
            .as_str()
            .ok_or("request_id must be a string")?
            .to_string();
    }
    if let Some(timeout) = args.remove("timeout_ms") {
        request.timeout_ms = timeout.as_u64().ok_or("timeout_ms must be an integer")?;
    }
    let method = name
        .strip_prefix("control_")
        .ok_or("unknown control tool")?;
    if method == "cancel" {
        let id = args
            .remove("target_request_id")
            .ok_or("target_request_id is required")?;
        args.insert("request_id".into(), id);
    }
    // An agent pays for every node it is shown, so the default here is the
    // bounded outline rather than the protocol's raw node list.
    if method == "observe" {
        let view = args.entry("view").or_insert_with(|| json!({}));
        if let Some(view) = view.as_object_mut() {
            view.entry("mode").or_insert_with(|| json!("outline"));
        }
    }
    args.insert("method".into(), json!(method));
    request.operation = serde_json::from_value(Value::Object(args)).map_err(|e| e.to_string())?;
    Ok(request)
}

/// The recovery that actually works for a failure, by error code.
fn hint(code: &str) -> Option<&'static str> {
    Some(match code {
        "CONTROL.STALE_TARGET" | "DRIVER.STALE_HANDLE" => {
            "The interface changed since it was observed, or a newer observation or an \
input attempt replaced this one. Call control_observe again and use a target from that answer."
        }
        // Recommending automation_id and role would mislead: colliding elements
        // usually already share both -- five Edit buttons in a table have the
        // same role and no automation_id. Ordered by how well each survives the
        // interface changing.
        "CONTROL.AMBIGUOUS" | "DRIVER.AMBIGUOUS_MATCH" => {
            "The locator matched several elements, listed in details.candidates. Narrow \
it with `within` to name the container the wanted one sits in -- a row labelled with its \
own text stays correct after the list reorders -- or with `near` to place it against a \
neighbouring label. Use `nth` only when nothing distinguishes them but position, or pick \
the node_id of the right candidate."
        }
        "CONTROL.NOT_FOUND" | "DRIVER.NOT_FOUND" => {
            "No element matched. Read an outline with control_observe to see what is \
present, or observe again if it may still be appearing. To ask whether something is \
gone, pass expect: optional."
        }
        "CONTROL.ACTION_UNSUPPORTED" | "DRIVER.ACTION_UNSUPPORTED" => {
            "This node does not offer that action; its `capabilities` in the observation \
list what it does."
        }
        "CONTROL.OBSERVATION_TRUNCATED" => {
            "The capture hit its node budget, so uniqueness cannot be proven. Raise \
max_nodes (up to 2000) or observe a smaller window."
        }
        "CONTROL.LOCATOR_UNSUPPORTED" => {
            "This provider matches role, name and node_id exactly. Drop the other fields, \
or take the node_id from an outline."
        }
        "CONTROL.VIEW_UNSUPPORTED" => {
            "Regions are computed for Windows windows only. Observe with view mode outline."
        }
        "CONTROL.OCCLUDED" => {
            "Something is laid over the element, such as a dialog, a banner or a menu, so pointer input would land on that instead. Observe again and deal with what covers it, or use a plain click (DOM activation) if that is enough."
        }
        "CONTROL.OPTION_NOT_FOUND" => {
            "No option has that label or value. The observation lists the labels in the node's `options`."
        }
        "CONTROL.DIALOG_OPEN" => {
            "A JavaScript dialog (alert, confirm, prompt or leave-page) blocks this browser \
page, so nothing else on it can be done. Call control_observe on the page: it shows the \
dialog as an alertdialog node with its message; control_act accept (with `text` for a \
prompt) or dismiss on it, then observe again."
        }
        "BROWSER.PAGE_UNRESPONSIVE" => {
            "The browser page does not answer, usually because a JavaScript dialog opened \
before this session first looked at it; such a dialog cannot be answered from here. Ask the \
person to close it in the browser, then call control_targets again."
        }
        "CONTROL.WAIT_TIMEOUT" => {
            "The condition did not hold before timeout_ms ran out; details.present says \
whether the element was there at the last look. Observe an outline to see what the page or \
window shows now, or wait again with a longer timeout_ms (up to 30000)."
        }
        "CONTROL.TARGET_UNKNOWN" => {
            "Call control_targets first, then observe a resource_id it returned."
        }
        "POLICY.DENIED" => {
            "Whoever started this server did not grant input (aad mcp --allow-input). Do \
not look for another way to act; ask the person you are helping."
        }
        _ => return None,
    })
}

pub fn payload(response: Response) -> Value {
    let mut value = serde_json::to_value(response).unwrap();
    let code = value["error"]["code"].as_str().map(str::to_string);
    if let Some(hint) = code.as_deref().and_then(hint) {
        value["error"]["hint"] = json!(hint);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Tool {
        catalogue().into_iter().find(|t| t.name == name).unwrap()
    }
    fn variant(name: &str) -> Value {
        tool("control_act").schema["properties"]["action"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["properties"]["type"]["const"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} action"))
    }

    #[test]
    fn schemas_require_session_observation_and_resource_bound_targets() {
        let act = tool("control_act");
        assert!(act.mutating);
        assert!(act.schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("target")));
        for target in [
            &act.schema["properties"]["target"],
            &variant("drag")["properties"]["to"],
        ] {
            for field in ["session_id", "resource_id", "observation_id", "node_id"] {
                assert!(target["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(field)));
            }
        }
    }

    #[test]
    fn every_protocol_action_is_published_and_parses() {
        for name in aad_control::ACTION_NAMES {
            let schema = variant(name);
            let mut sample = json!({"type": name});
            for field in schema["required"].as_array().unwrap() {
                let field = field.as_str().unwrap();
                sample[field] = match field {
                    "type" => json!(name),
                    "value" | "text" => json!("x"),
                    "keys" => json!(["enter"]),
                    "direction" => json!("down"),
                    "option" => json!("x"),
                    "checked" => json!(true),
                    "url" => json!("https://example.com/"),
                    "to" => {
                        json!({"session_id":"s","resource_id":"r","observation_id":"o","node_id":"n"})
                    }
                    other => panic!("no sample for {other}"),
                };
            }
            serde_json::from_value::<aad_control::Action>(sample)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn an_agent_can_ration_the_answer_by_size_not_only_by_count() {
        let view = &tool("control_observe").schema["properties"]["view"]["properties"];
        let text = view["max_characters"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("stopped_by"), "{text:?}");
        assert!(text.contains("region"), "{text:?}");
        assert!(view["limit"].is_object());
    }

    #[test]
    fn a_crowded_window_can_be_read_in_two_steps() {
        let observe = tool("control_observe");
        assert!(!observe.mutating);
        assert!(observe.description.contains("overview"));
        let view = &observe.schema["properties"]["view"]["properties"];
        assert_eq!(
            view["mode"]["enum"],
            json!(["outline", "overview", "nodes"])
        );
        assert!(view["mode"]["description"]
            .as_str()
            .unwrap()
            .contains("367"));
        // The region the overview names is the ancestor name `within` matches.
        assert!(view["region"]["description"]
            .as_str()
            .unwrap()
            .contains("within"));
    }

    #[test]
    fn observing_over_mcp_defaults_to_the_bounded_outline() {
        let session = aad_control::Session::new(Vec::new(), Default::default());
        let request = prepare("control_observe", &json!({"resource_id": "r"}), &session).unwrap();
        match request.operation {
            Operation::Observe { view, .. } => {
                assert_eq!(view.mode, aad_control::ViewMode::Outline)
            }
            other => panic!("{other:?}"),
        }
        let raw = prepare(
            "control_observe",
            &json!({"resource_id": "r", "view": {"mode": "nodes"}}),
            &session,
        )
        .unwrap();
        assert!(matches!(
            raw.operation,
            Operation::Observe { view, .. } if view.mode == aad_control::ViewMode::Nodes
        ));
    }

    #[test]
    fn waiting_is_a_read_only_tool_that_parses_into_the_protocol() {
        let wait = tool("control_wait");
        assert!(!wait.mutating);
        assert_eq!(
            wait.schema["properties"]["condition"]["enum"],
            json!(["appears", "disappears", "state"])
        );
        let session = aad_control::Session::new(Vec::new(), Default::default());
        let request = prepare(
            "control_wait",
            &json!({"resource_id": "r", "locator": {"name": "Done"}, "condition": "state",
                    "states": {"enabled": true}, "interval_ms": 100, "timeout_ms": 5000}),
            &session,
        )
        .unwrap();
        assert!(!request.mutating());
        assert_eq!(request.timeout_ms, 5000);
        match request.operation {
            Operation::Wait {
                condition, states, ..
            } => {
                assert_eq!(condition, aad_control::WaitCondition::State);
                assert_eq!(states, Some(json!({"enabled": true})));
            }
            other => panic!("{other:?}"),
        }
        assert!(prepare(
            "control_wait",
            &json!({"resource_id": "r", "locator": {"name": "Done"}, "condition": "loaded"}),
            &session,
        )
        .is_err());
    }

    #[test]
    fn asking_whether_something_is_gone_is_expressible() {
        let expect = &tool("control_find").schema["properties"]["expect"];
        assert_eq!(expect["enum"], json!(["one", "optional"]));
    }

    #[test]
    fn an_agent_can_express_which_of_several_namesakes_it_means() {
        let locator = &tool("control_find").schema["properties"]["locator"]["properties"];
        for field in ["within", "near", "nth"] {
            let described = locator[field]["description"]
                .as_str()
                .unwrap_or_default()
                .len();
            assert!(described > 40, "{field} needs to say when it applies");
        }
        assert!(locator["node_id"].is_object());
        // Password fields often have no stable name; `protected` is how to find one.
        assert_eq!(
            locator["states"]["properties"]["protected"]["type"],
            "boolean"
        );
        // Every published field is one the protocol accepts.
        let mut sample = Map::new();
        for field in locator.as_object().unwrap().keys() {
            sample.insert(
                field.clone(),
                match field.as_str() {
                    "states" => json!({"protected": true}),
                    "within" => json!({"name": "row"}),
                    "near" => json!({"anchor": {"name": "City"}}),
                    "nth" => json!(1),
                    "match" => json!("contains"),
                    _ => json!("x"),
                },
            );
        }
        serde_json::from_value::<aad_control::Locator>(Value::Object(sample)).unwrap();
    }

    #[test]
    fn text_input_is_bounded_in_the_schema() {
        assert_eq!(
            variant("type_text")["properties"]["text"]["maxLength"],
            1024
        );
        assert_eq!(variant("fill")["properties"]["value"]["maxLength"], 4096);
        assert_eq!(
            variant("type_text")["properties"]["mode"]["enum"],
            json!(["unicode", "keys"])
        );
    }

    #[test]
    fn failures_carry_the_recovery_that_works() {
        let response = |code: &str| Response {
            protocol_version: "aad-control/1".into(),
            request_id: "r".into(),
            session_id: "s".into(),
            effect: "not_applied".into(),
            commit_state: aad_control::CommitState::NotCommitted,
            data: None,
            error: Some(json!({"code": code})),
        };
        let stale = payload(response("CONTROL.STALE_TARGET"));
        assert!(stale["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("control_observe"));
        let ambiguous = payload(response("DRIVER.AMBIGUOUS_MATCH"));
        let hint = ambiguous["error"]["hint"].as_str().unwrap();
        for way in ["within", "near", "nth", "candidates"] {
            assert!(hint.contains(way), "{way}");
        }
        assert!(payload(response("CONTROL.SOMETHING_ELSE"))["error"]
            .get("hint")
            .is_none());
    }
}
