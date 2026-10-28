//! The commands a person runs by hand: list, read, find and act.
//!
//! They go through the same control session as `aad control serve` and the MCP
//! tools, so every platform the session supports works here too. A process
//! lives for one command, so `do` observes, finds and acts within that one
//! process rather than taking a reference from an earlier `find`: a reference
//! into a session that has exited would point at nothing.
use aad_control::{Action, Expect, Locator, Operation, Policy, Request, Session, View, ViewMode};
use clap::{Args, Subcommand};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

use crate::{failure, EXIT_FAILED, EXIT_OK, EXIT_USAGE};

/// Observation can be slow on a large window; the protocol allows up to 30 s.
const TIMEOUT_MS: u64 = 30_000;

#[derive(Args)]
pub struct AppsArgs {
    /// Only list resources whose name contains this text.
    #[arg(long)]
    title: Option<String>,
    /// Only list resources belonging to a process whose name contains this text.
    #[arg(long)]
    process: Option<String>,
}

#[derive(Args)]
pub struct ResourceArgs {
    /// The window or application, from `aad apps`: its resource id, a unique
    /// prefix of the id's hash, or the window id printed beside it.
    resource: String,
    /// Maximum elements to capture (1..2000).
    #[arg(long, default_value_t = 1000)]
    max_nodes: usize,
}

#[derive(Args)]
pub struct OutlineArgs {
    #[command(flatten)]
    at: ResourceArgs,
    /// Maximum elements to report (1..500).
    #[arg(long, default_value_t = 80)]
    limit: usize,
    /// Only list one region, named as `aad overview` reports it (Windows).
    #[arg(long)]
    region: Option<String>,
    /// Stop after roughly this many characters (500..200000, default 20000).
    ///
    /// `--limit` counts elements, which does not bound the answer's size:
    /// elements run from 169 to 4890 characters, and 500 of them reached 188147
    /// on one window here.
    #[arg(long)]
    max_characters: Option<usize>,
    /// Write the listing to this file instead of standard output, with the
    /// largest limits the session allows.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// How to pick one element. `--role`, `--name` and `--locator` with those two
/// fields work everywhere; the rest are evaluated by the Windows driver and
/// refused elsewhere rather than ignored.
#[derive(Args, Default)]
pub struct LocatorArgs {
    /// Match the element's role, for example Button.
    #[arg(long)]
    role: Option<String>,
    /// Match the element's name.
    #[arg(long)]
    name: Option<String>,
    /// Match the element's automation id (Windows).
    #[arg(long)]
    automation_id: Option<String>,
    /// Match names and ids by substring instead of exactly (Windows).
    #[arg(long)]
    contains: bool,
    /// Match only password fields, or only non-password ones (Windows).
    ///
    /// Password fields often carry no stable name, so this is frequently the
    /// only way to address one.
    #[arg(long)]
    protected: Option<bool>,
    /// Which one to take when several match: a number from 1, `first` or `last` (Windows).
    ///
    /// Counted down the screen and then across, over the whole window
    /// including its frame -- pair it with `--in` or `--near` to count inside a
    /// region instead.
    #[arg(long)]
    nth: Option<String>,
    /// Find it beside another element, named here (Windows).
    #[arg(long, value_name = "NAME")]
    near: Option<String>,
    /// Which side of `--near` to look on: left, right, above, below.
    #[arg(long, requires = "near")]
    side: Option<String>,
    /// Ignore anything further than this many pixels from `--near`.
    #[arg(long, requires = "near")]
    within: Option<i32>,
    /// Search only inside the container with this name (Windows).
    ///
    /// Deliberately not `--within`, which is already a distance from `--near`.
    #[arg(long = "in", value_name = "NAME")]
    inside: Option<String>,
    /// Narrow `--in` to a container of this role.
    #[arg(long = "in-role", requires = "inside", value_name = "ROLE")]
    inside_role: Option<String>,
    /// The whole locator as JSON, for shapes the flags cannot express.
    #[arg(long, value_name = "JSON", conflicts_with_all = ["role", "name", "automation_id", "nth", "near", "inside", "protected", "contains"])]
    locator: Option<String>,
}

#[derive(Args)]
pub struct FindArgs {
    #[command(flatten)]
    at: ResourceArgs,
    #[command(flatten)]
    locator: LocatorArgs,
    /// Report absence as `found: false` instead of failing, to ask whether
    /// something is gone.
    #[arg(long)]
    optional: bool,
}

#[derive(Args)]
pub struct Pick {
    #[command(flatten)]
    at: ResourceArgs,
    #[command(flatten)]
    locator: LocatorArgs,
}

#[derive(Args, Default)]
pub struct Position {
    /// Horizontal position inside the element, 0.0..1.0 (with --fy).
    #[arg(long, requires = "fy")]
    fx: Option<f64>,
    /// Vertical position inside the element, 0.0..1.0 (with --fx).
    #[arg(long, requires = "fx")]
    fy: Option<f64>,
}

#[derive(Args)]
pub struct ClickArgs {
    #[command(flatten)]
    pick: Pick,
    /// left (default), right or middle.
    #[arg(long, value_parser = ["left", "right", "middle"])]
    button: Option<String>,
    /// 1 (default) or 2 for a double click.
    #[arg(long)]
    count: Option<u8>,
    #[command(flatten)]
    at: Position,
}

#[derive(Args)]
pub struct FillArgs {
    #[command(flatten)]
    pick: Pick,
    /// The value to set.
    #[arg(long)]
    value: String,
}

#[derive(Args)]
pub struct TextArgs {
    #[command(flatten)]
    pick: Pick,
    /// The text to type.
    #[arg(long)]
    text: String,
    /// Input encoding: unicode (default) or layout-aware keys.
    #[arg(long, value_parser = ["unicode", "keys"], default_value = "unicode")]
    mode: String,
}

#[derive(Args)]
pub struct KeyArgs {
    #[command(flatten)]
    pick: Pick,
    /// The combination, comma-separated: modifiers then one key, e.g.
    /// `ctrl,c`, `f5`, `shift,end`.
    #[arg(long, value_name = "KEYS")]
    keys: String,
}

#[derive(Args)]
pub struct ScrollArgs {
    #[command(flatten)]
    pick: Pick,
    /// up, down, left or right.
    #[arg(long, value_parser = ["up", "down", "left", "right"])]
    direction: String,
    /// Wheel notches, 1..30 (default 3).
    #[arg(long)]
    notches: Option<u8>,
    #[command(flatten)]
    at: Position,
}

#[derive(Args)]
pub struct HoverArgs {
    #[command(flatten)]
    pick: Pick,
    /// How long to rest, 1..5000 ms (default 500).
    #[arg(long)]
    ms: Option<u32>,
    #[command(flatten)]
    at: Position,
}

#[derive(Args)]
pub struct DragArgs {
    #[command(flatten)]
    pick: Pick,
    /// The destination's name.
    #[arg(long, value_name = "NAME")]
    to_name: Option<String>,
    /// The destination's role.
    #[arg(long, value_name = "ROLE")]
    to_role: Option<String>,
    /// The destination's whole locator as JSON.
    #[arg(long, value_name = "JSON", conflicts_with_all = ["to_name", "to_role"])]
    to_locator: Option<String>,
    /// Which button to hold. Defaults to left.
    #[arg(long, value_parser = ["left", "right", "middle"])]
    button: Option<String>,
    #[command(flatten)]
    at: Position,
    /// Horizontal release position inside the destination (with --to-fy).
    #[arg(long, requires = "to_fy")]
    to_fx: Option<f64>,
    /// Vertical release position inside the destination (with --to-fx).
    #[arg(long, requires = "to_fx")]
    to_fy: Option<f64>,
}

#[derive(Subcommand)]
pub enum DoCommand {
    /// Give an element keyboard focus.
    Focus(Pick),
    /// Activate an element's default action.
    Invoke(Pick),
    /// Click an element; right, middle, double or off-centre with the options.
    Click(ClickArgs),
    /// Replace an element's value.
    Fill(FillArgs),
    /// Type text into an element.
    TypeText(TextArgs),
    /// Press a key combination on an element, e.g. ctrl,c or f5.
    Key(KeyArgs),
    /// Scroll the wheel over an element.
    Scroll(ScrollArgs),
    /// Rest the pointer over an element.
    Hover(HoverArgs),
    /// Drag one element onto another.
    Drag(DragArgs),
}

type Outcome = Result<Value, (Value, u8)>;

fn usage(message: impl AsRef<str>) -> (Value, u8) {
    (
        failure("CLI.INVALID_ARGUMENTS", message.as_ref(), None),
        EXIT_USAGE,
    )
}

/// One session for one command.
struct Desk {
    session: Session,
}

impl Desk {
    fn open(allow_input: bool) -> Self {
        Self {
            session: Session::native(Policy {
                allow_input,
                ..Default::default()
            }),
        }
    }

    fn call(&self, operation: Operation) -> Outcome {
        let mut request = Request::new(Some(self.session.id().to_string()), operation);
        request.timeout_ms = TIMEOUT_MS;
        let response = self.session.execute(&request);
        match (response.data, response.error) {
            (Some(data), _) => Ok(data),
            (None, error) => {
                let mut error = error.unwrap_or_else(|| json!({"code": "CONTROL.UNKNOWN"}));
                if let Some(hint) = error["code"].as_str().and_then(hint) {
                    error["hint"] = json!(hint);
                }
                Err((json!({"status": "error", "error": error}), EXIT_FAILED))
            }
        }
    }

    fn targets(&self) -> Outcome {
        self.call(Operation::Targets {})
    }

    /// The resource a person named: its id, a unique prefix of its hash, or
    /// the platform's own window id.
    fn resolve(&self, given: &str) -> Result<String, (Value, u8)> {
        let listed = self.targets()?;
        let resources = listed["resources"].as_array().cloned().unwrap_or_default();
        let id = |r: &Value| r["resource_id"].as_str().unwrap_or_default().to_string();
        if let Some(exact) = resources
            .iter()
            .find(|r| id(r) == given || r["metadata"]["window_id"].as_str() == Some(given))
        {
            return Ok(id(exact));
        }
        let matches: Vec<_> = if given.len() >= 4 {
            resources
                .iter()
                .filter(|r| {
                    let id = id(r);
                    let hash = id.rsplit(':').next().unwrap_or_default();
                    hash.starts_with(given) || id.starts_with(given)
                })
                .collect()
        } else {
            Vec::new()
        };
        match matches.as_slice() {
            [one] => Ok(id(one)),
            [] => Err((
                failure(
                    "CONTROL.TARGET_UNKNOWN",
                    &format!("no window or application matches {given:?}"),
                    Some(
                        json!({"hint": hint("CONTROL.TARGET_UNKNOWN"), "unavailable": listed["unavailable"]}),
                    ),
                ),
                EXIT_FAILED,
            )),
            several => Err((
                failure(
                    "CLI.AMBIGUOUS_RESOURCE",
                    &format!(
                        "{} resources start with {given:?}; give more of the id",
                        several.len()
                    ),
                    Some(
                        json!({"candidates": several.iter().map(|r| json!({"resource_id": r["resource_id"], "name": r["name"]})).collect::<Vec<_>>()}),
                    ),
                ),
                EXIT_USAGE,
            )),
        }
    }

    fn observe(&self, at: &ResourceArgs, view: View) -> Outcome {
        let resource_id = self.resolve(&at.resource)?;
        self.call(Operation::Observe {
            resource_id,
            max_nodes: at.max_nodes,
            view,
        })
    }

    fn find(&self, observation: &Value, locator: Locator, expect: Expect) -> Outcome {
        self.call(Operation::Find {
            observation_id: observation["observation_id"]
                .as_str()
                .unwrap_or_default()
                .into(),
            locator: Box::new(locator),
            expect,
        })
    }
}

/// What the CLI user can do about a failure, in this command line's terms.
fn hint(code: &str) -> Option<&'static str> {
    Some(match code {
        "CONTROL.STALE_TARGET" | "DRIVER.STALE_HANDLE" => {
            "The interface changed while the command ran. Run it again."
        }
        "CONTROL.AMBIGUOUS" | "DRIVER.AMBIGUOUS_MATCH" => {
            "Several elements matched; details.candidates lists them. Narrow it with --in \
(the container) or --near (a neighbouring label), and --nth only when position is all \
that tells them apart."
        }
        "CONTROL.NOT_FOUND" | "DRIVER.NOT_FOUND" => {
            "Nothing matched. `aad describe` shows what is there; `aad find --optional` asks \
whether something is gone."
        }
        "CONTROL.ACTION_UNSUPPORTED" | "DRIVER.ACTION_UNSUPPORTED" => {
            "The element does not offer that action; `aad find` lists its capabilities."
        }
        "CONTROL.OBSERVATION_TRUNCATED" => {
            "The window has more elements than were captured. Raise --max-nodes (up to 2000)."
        }
        "CONTROL.LOCATOR_UNSUPPORTED" => {
            "This platform matches --role and --name exactly; drop the other flags."
        }
        "CONTROL.VIEW_UNSUPPORTED" => {
            "Regions are computed for Windows windows only; use `aad describe` without --region."
        }
        "CONTROL.TARGET_UNKNOWN" => "Run `aad apps` and pass one of the resource ids it lists.",
        _ => return None,
    })
}

impl LocatorArgs {
    fn build(&self) -> Result<Locator, (Value, u8)> {
        let value = if let Some(raw) = &self.locator {
            serde_json::from_str::<Value>(raw)
                .map_err(|error| usage(format!("--locator is not valid JSON: {error}")))?
        } else {
            let mut locator = Map::new();
            let mut put = |key: &str, value: Value| {
                locator.insert(key.into(), value);
            };
            if let Some(role) = &self.role {
                put("role", json!(role));
            }
            if let Some(name) = &self.name {
                put("name", json!(name));
            }
            if let Some(id) = &self.automation_id {
                put("automation_id", json!(id));
            }
            if let Some(protected) = self.protected {
                put("states", json!({"protected": protected}));
            }
            if self.contains {
                put("match", json!("contains"));
            }
            if let Some(nth) = &self.nth {
                // A number if it looks like one, so `--nth 2` and `--nth last`
                // both reach the driver in the shape it expects.
                put(
                    "nth",
                    nth.parse::<u64>().map_or_else(|_| json!(nth), |n| json!(n)),
                );
            }
            if let Some(near) = &self.near {
                let mut relation = json!({"anchor": {"name": near}});
                if let Some(side) = &self.side {
                    relation["direction"] = json!(side);
                }
                if let Some(within) = self.within {
                    relation["within"] = json!(within);
                }
                put("near", relation);
            }
            if let Some(inside) = &self.inside {
                let mut container = json!({"name": inside});
                if let Some(role) = &self.inside_role {
                    container["role"] = json!(role);
                }
                put("within", container);
            }
            Value::Object(locator)
        };
        let locator: Locator = serde_json::from_value(value)
            .map_err(|error| usage(format!("not a usable locator: {error}")))?;
        if locator.is_empty() {
            return Err(usage(
                "give at least one of --role, --name, --automation-id, --protected, --nth, --near, --in or --locator",
            ));
        }
        Ok(locator)
    }
}

impl Position {
    fn fraction(&self) -> Value {
        match (self.fx, self.fy) {
            (Some(fx), Some(fy)) => json!({"fx": fx, "fy": fy}),
            _ => Value::Null,
        }
    }
}

/// Drop what only means something inside the session that just ended.
fn public(node: &Value) -> Value {
    let mut node = node.clone();
    if let Some(object) = node.as_object_mut() {
        object.remove("target");
    }
    node
}

fn finish(outcome: Outcome) -> (Value, u8) {
    match outcome {
        Ok(value) => (value, EXIT_OK),
        Err(failed) => failed,
    }
}

pub fn apps(args: &AppsArgs) -> (Value, u8) {
    let desk = Desk::open(false);
    finish(desk.targets().map(|listed| {
        let contains = |value: &Value, wanted: &Option<String>| match wanted {
            None => true,
            Some(wanted) => value
                .as_str()
                .is_some_and(|actual| actual.to_lowercase().contains(&wanted.to_lowercase())),
        };
        let apps: Vec<Value> = listed["resources"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                let process = r["metadata"]
                    .get("process_name")
                    .or_else(|| r["metadata"].get("name"))
                    .cloned()
                    .unwrap_or(Value::Null);
                contains(&r["name"], &args.title) && contains(&process, &args.process)
            })
            .collect();
        json!({"count": apps.len(), "apps": apps, "unavailable": listed["unavailable"]})
    }))
}

pub fn describe(args: &OutlineArgs) -> (Value, u8) {
    let desk = Desk::open(false);
    let to_file = args.out.is_some();
    let view = View {
        mode: ViewMode::Outline,
        region: args.region.clone(),
        limit: Some(if to_file {
            aad_control::MAX_OUTLINE_LIMIT
        } else {
            args.limit
        }),
        max_characters: if to_file {
            Some(aad_control::MAX_OUTLINE_CHARACTERS)
        } else {
            args.max_characters
        },
    };
    match (desk.observe(&args.at, view), &args.out) {
        (Ok(answer), Some(path)) => crate::write_listing(&answer, path, args.max_characters),
        (outcome, _) => finish(outcome),
    }
}

pub fn overview(args: &ResourceArgs) -> (Value, u8) {
    let view = View {
        mode: ViewMode::Overview,
        ..Default::default()
    };
    finish(Desk::open(false).observe(args, view))
}

pub fn snapshot(args: &ResourceArgs) -> (Value, u8) {
    finish(Desk::open(false).observe(args, View::default()))
}

pub fn find(args: &FindArgs) -> (Value, u8) {
    let locator = match args.locator.build() {
        Ok(locator) => locator,
        Err(failed) => return failed,
    };
    let desk = Desk::open(false);
    let expect = if args.optional {
        Expect::Optional
    } else {
        Expect::One
    };
    finish(
        desk.observe(&args.at, View::default())
            .and_then(|observed| desk.find(&observed, locator, expect))
            .map(|mut found| {
                if let Some(object) = found.as_object_mut() {
                    object.remove("target");
                    object.remove("observation_id");
                }
                if let Some(node) = found.get("node").map(public) {
                    found["node"] = node;
                }
                found
            }),
    )
}

/// The action and the element it applies to, still to be resolved.
fn plan(command: &DoCommand) -> Result<(&Pick, Value, Option<Locator>), (Value, u8)> {
    let (pick, mut action, to) = match command {
        DoCommand::Focus(pick) => (pick, json!({"type": "focus"}), None),
        DoCommand::Invoke(pick) => (pick, json!({"type": "invoke"}), None),
        DoCommand::Click(args) => (
            &args.pick,
            json!({"type": "click", "button": args.button, "count": args.count, "fraction": args.at.fraction()}),
            None,
        ),
        DoCommand::Fill(args) => (
            &args.pick,
            json!({"type": "fill", "value": args.value}),
            None,
        ),
        DoCommand::TypeText(args) => (
            &args.pick,
            json!({"type": "type_text", "text": args.text, "mode": args.mode}),
            None,
        ),
        DoCommand::Key(args) => {
            // Empty segments are dropped so a stray comma is not a blank key.
            let keys: Vec<&str> = args
                .keys
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect();
            (&args.pick, json!({"type": "key", "keys": keys}), None)
        }
        DoCommand::Scroll(args) => (
            &args.pick,
            json!({"type": "scroll", "direction": args.direction, "notches": args.notches, "fraction": args.at.fraction()}),
            None,
        ),
        DoCommand::Hover(args) => (
            &args.pick,
            json!({"type": "hover", "ms": args.ms, "fraction": args.at.fraction()}),
            None,
        ),
        DoCommand::Drag(args) => {
            let to = if let Some(raw) = &args.to_locator {
                serde_json::from_str::<Value>(raw)
                    .map_err(|error| usage(format!("--to-locator is not valid JSON: {error}")))?
            } else {
                let mut to = Map::new();
                if let Some(name) = &args.to_name {
                    to.insert("name".into(), json!(name));
                }
                if let Some(role) = &args.to_role {
                    to.insert("role".into(), json!(role));
                }
                Value::Object(to)
            };
            let to: Locator = serde_json::from_value(to)
                .map_err(|error| usage(format!("not a usable destination: {error}")))?;
            if to.is_empty() {
                return Err(usage(
                    "drag needs a destination: --to-name, --to-role or --to-locator",
                ));
            }
            let to_fraction = match (args.to_fx, args.to_fy) {
                (Some(fx), Some(fy)) => json!({"fx": fx, "fy": fy}),
                _ => Value::Null,
            };
            (
                &args.pick,
                json!({"type": "drag", "button": args.button, "fraction": args.at.fraction(), "to_fraction": to_fraction}),
                Some(to),
            )
        }
    };
    if let Some(object) = action.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
    Ok((pick, action, to))
}

pub fn act(command: &DoCommand) -> (Value, u8) {
    let (pick, mut action, to) = match plan(command) {
        Ok(planned) => planned,
        Err(failed) => return failed,
    };
    let locator = match pick.locator.build() {
        Ok(locator) => locator,
        Err(failed) => return failed,
    };
    // Typed now with a placeholder destination, so a malformed action is a
    // usage error before anything on screen is touched.
    let mut check = action.clone();
    if to.is_some() {
        check["to"] =
            json!({"session_id": "", "resource_id": "", "observation_id": "", "node_id": ""});
    }
    if let Err(error) = serde_json::from_value::<Action>(check) {
        return usage(format!("not a usable action: {error}"));
    }

    // Running `aad do` is the person's own grant of input.
    let desk = Desk::open(true);
    let outcome = (|| {
        let observed = desk.observe(&pick.at, View::default())?;
        let found = desk.find(&observed, locator, Expect::One)?;
        if let Some(to) = to {
            let destination = desk.find(&observed, to, Expect::One)?;
            action["to"] = destination["target"].clone();
        }
        let action: Action =
            serde_json::from_value(action).map_err(|error| usage(error.to_string()))?;
        let result = desk.call(Operation::Act {
            target: serde_json::from_value(found["target"].clone())
                .map_err(|error| usage(error.to_string()))?,
            action,
        })?;
        Ok(json!({"acted_on": public(&found["node"]), "result": result}))
    })();
    finish(outcome)
}

#[derive(Args)]
pub struct RecordArgs {
    /// The window or application to watch, as for `aad describe`.
    resource: String,
    /// How long to record for, in seconds (1..600).
    ///
    /// Recording runs for a fixed time because a command-line session has no
    /// way to be told "stop now": the process stays alive to hold the watch,
    /// then reports.
    #[arg(long, default_value_t = 15)]
    seconds: u64,
    /// Save the steps as a named recording and workflow, ready for `aad run`.
    ///
    /// The steps name the application by process or title and each element by
    /// locator, never by an id that only exists in this session, so the saved
    /// workflow replays later, on this or another machine of the same kind.
    #[arg(long, value_name = "NAME")]
    save: Option<String>,
    /// Replace an existing recording of that name.
    #[arg(long)]
    overwrite: bool,
}

fn automation(error: aad_core::AutomationError) -> (Value, u8) {
    let mut error = error.to_json();
    if let Some(hint) = error["code"].as_str().and_then(hint) {
        error["hint"] = json!(hint);
    }
    (json!({"status": "error", "error": error}), EXIT_FAILED)
}

/// Watch a window for a while and report, or save, what was done in it.
pub fn record(args: &RecordArgs) -> (Value, u8) {
    use aad_control::{recording::Recording, steps};
    use aad_runtime::recordings;
    let desk = Desk::open(false);
    let outcome = (|| {
        if let Some(name) = &args.save {
            let existing = recordings::workflow_path(name).map_err(|e| usage(e.message))?;
            if existing.exists() && !args.overwrite {
                return Err(usage(format!(
                    "a recording called {name:?} exists; pass --overwrite to replace it"
                )));
            }
        }
        let resource_id = desk.resolve(&args.resource)?;
        let listed = desk.targets()?;
        let all = listed["resources"].as_array().cloned().unwrap_or_default();
        let resource = all
            .iter()
            .find(|r| r["resource_id"] == resource_id.as_str())
            .cloned()
            .ok_or_else(|| usage("the window closed"))?;
        let app = steps::describe(&resource, &all);
        let seconds = args.seconds.clamp(1, 600);
        let mut recording =
            Recording::start(&desk.session, &resource, app.clone(), 1).map_err(automation)?;
        eprintln!(
            "recording {} for {seconds}s…",
            resource["name"].as_str().unwrap_or("the window")
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let (mut recorded, mut skipped, mut dropped) = (Vec::new(), Vec::new(), 0);
        // Collected as it goes: the providers buffer a bounded number of events.
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let batch = recording.collect().map_err(automation)?;
            recorded.extend(batch.steps);
            skipped.extend(batch.skipped);
            dropped += batch.dropped;
        }
        let batch = recording.stop().map_err(automation)?;
        recorded.extend(batch.steps);
        skipped.extend(batch.skipped);
        dropped += batch.dropped;
        let mut answer = json!({
            "app": app,
            "seconds": seconds,
            "count": recorded.len(),
            "steps": recorded,
            "skipped": skipped,
            "dropped": dropped,
        });
        if let Some(name) = &args.save {
            answer["saved"] = if recorded.is_empty() {
                json!({"failed": "nothing replayable was recorded"})
            } else {
                match steps::workflow(name, &recorded) {
                    Err(error) => json!({"failed": error.message}),
                    Ok(workflow) => {
                        let written =
                            recordings::save_recording(name, &steps::recording(name, &recorded))
                                .and_then(|recording| {
                                    recordings::save_workflow(name, &workflow)
                                        .map(|w| (recording, w))
                                });
                        match written {
                            Ok((recording, workflow)) => json!({
                                "recording_path": recording.display().to_string(),
                                "workflow_path": workflow.display().to_string(),
                                "run": format!(
                                    "aad run {} --permission desktop.input --permission desktop.observe",
                                    workflow.display()
                                ),
                            }),
                            Err(error) => json!({"failed": error.message}),
                        }
                    }
                }
            };
        }
        Ok(answer)
    })();
    finish(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(subcommand)]
        command: DoCommand,
    }

    fn parse(args: &[&str]) -> Result<DoCommand, clap::Error> {
        let mut argv = vec!["do"];
        argv.extend_from_slice(args);
        Probe::try_parse_from(argv).map(|p| p.command)
    }

    #[test]
    fn locator_flags_build_the_session_locator() {
        let args = LocatorArgs {
            role: Some("Button".into()),
            nth: Some("2".into()),
            near: Some("Username".into()),
            within: Some(40),
            inside: Some("Details".into()),
            protected: Some(true),
            ..Default::default()
        };
        let locator = serde_json::to_value(args.build().unwrap()).unwrap();
        assert_eq!(locator["role"], "Button");
        assert_eq!(locator["nth"], 2);
        assert_eq!(locator["near"]["anchor"]["name"], "Username");
        assert_eq!(locator["near"]["within"], 40, "the pixel distance");
        assert_eq!(locator["within"]["name"], "Details", "the container");
        assert_eq!(locator["states"]["protected"], true);
    }

    #[test]
    fn an_empty_or_malformed_locator_is_a_usage_error() {
        for args in [
            LocatorArgs::default(),
            LocatorArgs {
                locator: Some("{not json".into()),
                ..Default::default()
            },
            LocatorArgs {
                locator: Some(r#"{"colour":"red"}"#.into()),
                ..Default::default()
            },
        ] {
            let (payload, code) = args.build().unwrap_err();
            assert_eq!(code, EXIT_USAGE);
            assert_eq!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
        }
    }

    #[test]
    fn every_action_needs_a_resource_and_parses_into_a_typed_action() {
        assert!(parse(&["invoke", "--name", "OK"]).is_err(), "no resource");
        for args in [
            vec!["focus", "w", "--name", "OK"],
            vec!["invoke", "w", "--name", "OK"],
            vec![
                "click", "w", "--name", "OK", "--button", "right", "--count", "2",
            ],
            vec!["fill", "w", "--name", "Search", "--value", "x"],
            vec!["type-text", "w", "--name", "Search", "--text", "x"],
            vec!["key", "w", "--name", "Search", "--keys", "ctrl, a,"],
            vec![
                "scroll",
                "w",
                "--name",
                "List",
                "--direction",
                "down",
                "--fx",
                "0.5",
                "--fy",
                "0.5",
            ],
            vec!["hover", "w", "--name", "Help", "--ms", "300"],
            vec![
                "drag",
                "w",
                "--name",
                "File",
                "--to-name",
                "Trash",
                "--to-fx",
                "0.2",
                "--to-fy",
                "0.8",
            ],
        ] {
            let command = parse(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let (_, mut action, to) = plan(&command).unwrap();
            if to.is_some() {
                action["to"] = json!({"session_id": "s", "resource_id": "r", "observation_id": "o", "node_id": "n"});
            }
            let typed: Action = serde_json::from_value(action.clone())
                .unwrap_or_else(|e| panic!("{args:?} -> {action}: {e}"));
            assert_eq!(typed.name().replace('_', "-"), args[0]);
        }
        let (_, key, _) =
            plan(&parse(&["key", "w", "--name", "x", "--keys", "ctrl, a,"]).unwrap()).unwrap();
        assert_eq!(key["keys"], json!(["ctrl", "a"]));
    }

    #[test]
    fn incomplete_or_conflicting_pointer_requests_fail_before_the_desktop() {
        for args in [
            vec!["click", "w", "--name", "x", "--fx", "0.5"],
            vec!["scroll", "w", "--name", "x"],
            vec![
                "drag",
                "w",
                "--name",
                "x",
                "--to-name",
                "y",
                "--to-fx",
                "0.8",
            ],
            vec![
                "drag",
                "w",
                "--name",
                "x",
                "--to-name",
                "y",
                "--to-locator",
                "{}",
            ],
            vec![
                "type-text",
                "w",
                "--name",
                "x",
                "--text",
                "x",
                "--mode",
                "guess",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?} must be rejected");
        }
        let drag = parse(&["drag", "w", "--name", "x"]).unwrap();
        let Err((_, code)) = plan(&drag) else {
            panic!("a drag needs a destination");
        };
        assert_eq!(code, EXIT_USAGE);
    }

    #[test]
    fn hints_speak_in_command_line_terms() {
        assert!(hint("CONTROL.AMBIGUOUS").unwrap().contains("--in"));
        assert!(hint("CONTROL.TARGET_UNKNOWN").unwrap().contains("aad apps"));
        assert!(hint("CONTROL.SOMETHING_ELSE").is_none());
    }
}
