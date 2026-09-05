//! `aad` — the command line interface.
//!
//! Every command prints JSON to stdout and diagnostics to stderr, so the same
//! binary serves a person at a terminal and a program parsing its output. The
//! exit code carries the outcome: `0` success, `1` a failed run or workflow
//! error, `2` a usage or input error.

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const EXIT_OK: u8 = 0;
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;

#[derive(Parser)]
#[command(
    name = "aad",
    version,
    about = "Desktop automation: discover applications, describe their UI, and act on them.",
    long_about = "Desktop automation for people and agents.\n\n\
Discovery and inspection are read-only. Actions require a target obtained from \
`find` or `describe`, so nothing is ever clicked at a guessed position.\n\n\
All output is JSON on stdout; diagnostics go to stderr."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Print compact JSON instead of indented JSON.
    #[arg(long, global = true)]
    compact: bool,
}

#[derive(Subcommand)]
enum Command {
    /// List running applications and their windows.
    Apps(AppsArgs),
    /// Describe a window's interactive elements.
    Describe(OutlineArgs),
    /// Map a window's regions and their sizes, without listing their contents.
    Overview(DescribeArgs),
    /// Capture a window's full element tree.
    Snapshot(DescribeArgs),
    /// Find the single element matching a locator.
    Find(FindArgs),
    /// Act on an element that was found or described.
    #[command(subcommand)]
    Do(DoCommand),
    /// Check whether this machine can support desktop automation.
    Probe,
    /// Validate a workflow descriptor without running it.
    Validate(FileArgs),
    /// Run a workflow descriptor.
    Run(RunArgs),
    /// Record what someone does in a window, as replayable steps.
    Record(RecordArgs),
    /// Serve the Model Context Protocol over stdio, for AI clients.
    Mcp,
    /// Print the tools exposed over MCP.
    Tools,
    /// Start a durable run that survives this process exiting.
    Start(StartArgs),
    /// Continue a durable run that was paused or whose runner died.
    Resume(ResumeArgs),
    /// Report a durable run's status.
    Status(RunRefArgs),
    /// Ask a durable run to pause at its next safe point.
    Pause(RunRefArgs),
    /// Ask a durable run to stop for good.
    Cancel(RunRefArgs),
    /// List durable runs, newest first.
    List(ListArgs),
    /// Print a durable run's event history.
    Events(EventsArgs),
}

/// Where the durable run store lives.
///
/// Deliberately not called `--journal`: `run --journal` writes an NDJSON event
/// log and truncates the file it is given, so reusing that name would invite
/// someone to destroy a run store with a typo.
#[derive(Args)]
struct StoreArgs {
    /// The SQLite run store. Created if it does not exist.
    #[arg(long, value_name = "PATH")]
    store: PathBuf,
}

#[derive(Args)]
struct StartArgs {
    /// A workflow descriptor in YAML or JSON.
    file: PathBuf,
    #[command(flatten)]
    store: StoreArgs,
    /// Workflow inputs as a JSON object.
    #[arg(long, value_name = "JSON")]
    inputs: Option<String>,
    /// Use this run id instead of generating one.
    #[arg(long, value_name = "ID")]
    run_id: Option<String>,
    /// Name this process in the run's ownership record.
    #[arg(long, value_name = "ID")]
    owner_id: Option<String>,
    /// Seconds to hold the ownership lease between renewals.
    #[arg(long, value_name = "SECONDS")]
    lease_ttl: Option<f64>,
}

#[derive(Args)]
struct ResumeArgs {
    /// The run to continue.
    run_id: String,
    /// The same workflow descriptor the run was started from.
    file: PathBuf,
    #[command(flatten)]
    store: StoreArgs,
    /// Name this process in the run's ownership record.
    #[arg(long, value_name = "ID")]
    owner_id: Option<String>,
    /// Seconds to hold the ownership lease between renewals.
    #[arg(long, value_name = "SECONDS")]
    lease_ttl: Option<f64>,
}

#[derive(Args)]
struct RunRefArgs {
    /// The run to act on.
    run_id: String,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Args)]
struct ListArgs {
    #[command(flatten)]
    store: StoreArgs,
    /// Only list runs with this status.
    #[arg(long)]
    status: Option<String>,
    /// Maximum runs to report.
    #[arg(long, default_value_t = 100)]
    limit: i64,
    /// Skip this many runs before reporting.
    #[arg(long, default_value_t = 0)]
    offset: i64,
}

#[derive(Args)]
struct EventsArgs {
    /// The run whose history to print.
    run_id: String,
    #[command(flatten)]
    store: StoreArgs,
    /// Only report events after this sequence number.
    #[arg(long, value_name = "SEQ", default_value_t = 0)]
    after_seq: i64,
    /// Maximum events to report.
    #[arg(long, default_value_t = 1000)]
    limit: i64,
}

#[derive(Args)]
struct AppsArgs {
    /// Only list windows whose title contains this text.
    #[arg(long)]
    title: Option<String>,
    /// Only list windows belonging to this process name.
    #[arg(long)]
    process: Option<String>,
}

#[derive(Args)]
struct DescribeArgs {
    /// The window to inspect, from `aad apps`.
    window_id: String,
    /// Maximum elements to report.
    #[arg(long, default_value_t = 80)]
    limit: u32,
    /// Maximum tree depth to walk.
    #[arg(long)]
    max_depth: Option<u32>,
    /// Maximum elements to capture.
    #[arg(long)]
    max_nodes: Option<u32>,
}

#[derive(Args)]
struct OutlineArgs {
    #[command(flatten)]
    window: DescribeArgs,
    /// Only list one region, named as `aad overview` reports it.
    ///
    /// A separate struct from `snapshot`'s: a region means nothing when asking
    /// for the whole tree, and a flag that is accepted and then ignored is worse
    /// than one that does not exist.
    #[arg(long)]
    region: Option<String>,
    /// Stop after roughly this many characters (default 20000, 0 for no limit).
    ///
    /// `--limit` counts elements, which does not bound the answer's size:
    /// elements run from 169 to 4890 characters, and 500 of them reached 188147
    /// on one window here.
    #[arg(long)]
    max_characters: Option<u64>,
    /// Write the full listing to this file instead of standard output.
    ///
    /// No character limit applies: a file has no reason to be trimmed, and
    /// trimming it would defeat the point of asking for one.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args, Default)]
struct FindArgs {
    /// The window to search, from `aad apps`.
    window_id: String,
    /// Match the element's control type, for example Button.
    #[arg(long)]
    role: Option<String>,
    /// Match the element's name.
    #[arg(long)]
    name: Option<String>,
    /// Match the element's automation id.
    #[arg(long)]
    automation_id: Option<String>,
    /// Match names and ids by substring instead of exactly.
    #[arg(long)]
    contains: bool,
    /// Match only password fields, or only non-password ones.

    ///

    /// Password fields often carry no stable name, so this is frequently the

    /// only way to address one.

    #[arg(long)]

    protected: Option<bool>,

    /// Which one to take when several match: a number from 1, `first` or `last`.
    ///
    /// Rows, repeated toolbar buttons and unlabelled fields often share every
    /// attribute, so counting is the only way to tell them apart. Counted down
    /// the screen and then across, which is the order they are read in.
    ///
    /// Counting spans the whole window, including its frame -- so on a plain
    /// window the first button is usually Minimise, and in a browser the count
    /// runs through the toolbar before reaching the page (measured: 20 buttons
    /// reported for a page containing 3). Pair it with `--near` to count inside
    /// a region instead: the first button below "Username" is the page's, the
    /// third button in the window is not.
    #[arg(long)]
    nth: Option<String>,

    /// Find it beside another element, named here.
    ///
    /// For a control whose own label is useless or absent: the box next to
    /// "Username" is describable even when the box itself has no name.
    #[arg(long, value_name = "NAME")]
    near: Option<String>,

    /// Which way to look from `--near`: left, right, above, below.
    #[arg(long, requires = "near")]
    direction: Option<String>,

    /// Ignore anything further than this many pixels from `--near`.
    #[arg(long, requires = "near")]
    within: Option<i32>,

    /// Search only inside the container with this name.
    ///
    /// For elements that share every attribute and differ only in which panel
    /// holds them. Measured on this machine: of the interactive elements no
    /// attribute combination could identify, naming the container brought
    /// same-role siblings from a median of 66 down to 3 -- and it makes `--nth`
    /// count inside that container rather than across the window.
    ///
    /// A container matching nothing, or several things, finds nothing: quietly
    /// widening the search back to the whole window would act on some unrelated
    /// element.
    ///
    /// Deliberately not `--within`, which is already a distance in pixels from
    /// `--near`. The driver tells the two apart by nesting; a flag has no
    /// nesting to rely on.
    #[arg(long = "in", value_name = "NAME")]
    inside: Option<String>,

    /// Narrow `--in` to a container of this control type.
    #[arg(long = "in-role", requires = "inside", value_name = "ROLE")]
    inside_role: Option<String>,

    /// The whole locator as JSON, for what the flags above cannot express.
    ///
    /// Anchors can nest -- "the button beside the field beside Username" --
    /// and that shape does not fit into flags. Given this, the other match
    /// flags are ignored.
    #[arg(long, value_name = "JSON", conflicts_with_all = ["role", "name", "automation_id", "nth", "near", "inside"])]
    locator: Option<String>,

    /// Report absence instead of failing: for asking whether something is there.
    ///
    /// "Has the dialog closed?" cannot be asked otherwise -- the search itself
    /// fails, so there is no answer to test. Prints `found: false` and exits 0.
    #[arg(long)]
    optional: bool,

    /// Take the first match instead of refusing an ambiguous one.
    ///
    /// The refusal is the default because acting on "whichever came first" is
    /// how automation clicks the wrong button. Use this while working out what
    /// a locator selects: the competing elements are listed alongside, which is
    /// what shows how to narrow it.
    #[arg(long, conflicts_with = "optional")]
    any: bool,
}

#[derive(Args)]
struct RecordArgs {
    /// The window to watch, from `aad apps`.
    window_id: String,
    /// How long to record for, in seconds.
    ///
    /// Recording runs for a fixed time because a command-line session has no
    /// way to be told "stop now": the process has to stay alive to hold the
    /// subscription, so it waits, then reports.
    #[arg(long, default_value_t = 15)]
    seconds: u64,
    /// Print compact JSON instead of indented JSON.
    #[arg(long)]
    compact: bool,
}

#[derive(Subcommand)]
enum DoCommand {
    /// Give an element keyboard focus.
    Focus(TargetArgs),
    /// Activate an element's default action.
    Invoke(TargetArgs),
    /// Click the centre of an element.
    Click(TargetArgs),
    /// Replace an element's value.
    SetValue(ValueArgs),
    /// Type text into an element.
    TypeText(TextArgs),
}

#[derive(Args)]
struct TargetArgs {
    /// The element to act on, as printed by `aad find` (`snapshot:revision:node`).
    #[arg(long, value_name = "REF")]
    target: String,
}

#[derive(Args)]
struct ValueArgs {
    /// The element to act on, as printed by `aad find`.
    #[arg(long, value_name = "REF")]
    target: String,
    /// The value to set.
    #[arg(long)]
    value: String,
}

#[derive(Args)]
struct TextArgs {
    /// The element to act on, as printed by `aad find`.
    #[arg(long, value_name = "REF")]
    target: String,
    /// The text to type.
    #[arg(long)]
    text: String,
}

#[derive(Args)]
struct FileArgs {
    /// A workflow descriptor in YAML or JSON.
    file: PathBuf,
}

#[derive(Args)]
struct RunArgs {
    /// A workflow descriptor in YAML or JSON.
    file: PathBuf,
    /// Workflow inputs as a JSON object.
    #[arg(long, value_name = "JSON")]
    inputs: Option<String>,
    /// Write the run journal here as NDJSON.
    #[arg(long, value_name = "PATH")]
    journal: Option<PathBuf>,
    /// Report what would run without executing anything.
    #[arg(long)]
    dry_run: bool,
    /// Allow `script` steps to execute arbitrary code.
    ///
    /// Off by default: a descriptor should not be able to run code on this
    /// machine just because someone was asked to run the file.
    #[arg(long)]
    allow_scripts: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // The MCP server owns stdout for the whole session: it is a protocol
    // stream, so printing anything else onto it would desynchronise the client.
    if matches!(cli.command, Command::Mcp) {
        return match aad_mcp::serve_stdio() {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(error) => {
                eprintln!("aad mcp: {error}");
                ExitCode::from(EXIT_FAILED)
            }
        };
    }

    let (payload, code) = dispatch(&cli.command);
    emit(&payload, cli.compact);
    ExitCode::from(code)
}

fn emit(payload: &Value, compact: bool) {
    let text = if compact {
        payload.to_string()
    } else {
        serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
    };
    println!("{text}");
    let _ = std::io::stdout().flush();
}

/// How strict a `find` should be, from the flags the caller gave.
///
/// `None` means strict: a missing element is an error and an ambiguous one is
/// refused. That is the default because a caller is usually acquiring something
/// to act on, and "whichever came first" is how automation clicks the wrong
/// button.
fn expectation_for(args: &FindArgs) -> Option<&'static str> {
    if args.optional {
        // Asking whether something is there. Absence is the answer, not a
        // failure -- without this, "has the dialog closed?" cannot be asked at
        // all, because the search fails before there is anything to test.
        Some("optional")
    } else if args.any {
        // Asking for one of several. Note this does not also relax absence:
        // with nothing matching there is no "one of them" to return, and
        // handing back an empty target would fail later as a puzzling missing
        // element.
        Some("any")
    } else {
        None
    }
}

fn dispatch(command: &Command) -> (Value, u8) {
    match command {
        Command::Probe => {
            let report = aad_probe::probe();
            let code = match report.worst() {
                aad_probe::State::Unavailable => EXIT_FAILED,
                _ => EXIT_OK,
            };
            (report.to_json(), code)
        }
        Command::Tools => (aad_mcp::tools::list_payload(), EXIT_OK),
        // Handled in `main`, which hands stdout to the protocol stream.
        Command::Mcp => (json!({"status": "closed"}), EXIT_OK),
        Command::Validate(args) => validate(&args.file),
        Command::Run(args) => run_workflow(args),
        Command::Start(args) => start_run(args),
        Command::Resume(args) => resume_run(args),
        Command::Status(args) => with_store(&args.store, |store| {
            store
                .get_run(&args.run_id)
                .map(|run| run.to_json())
                .map_err(journal_failure)
        }),
        Command::Pause(args) => control(&args.store, &args.run_id, Intent::Pause),
        Command::Cancel(args) => control(&args.store, &args.run_id, Intent::Cancel),
        Command::List(args) => list_runs(args),
        Command::Events(args) => list_events(args),
        Command::Apps(args) => with_driver(|driver| {
            let mut result = driver
                .call("list_windows", &json!({}))
                .map_err(|error| driver_failure(&error))?;

            // Filtering here keeps the common "find the app I mean" case to a
            // single command rather than a pipeline.
            if args.title.is_some() || args.process.is_some() {
                let windows: Vec<Value> = result["windows"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|window| {
                        let matches = |field: &str, wanted: &Option<String>| match wanted {
                            None => true,
                            Some(wanted) => window[field]
                                .as_str()
                                .is_some_and(|actual| {
                                    actual.to_lowercase().contains(&wanted.to_lowercase())
                                }),
                        };
                        matches("title", &args.title) && matches("process_name", &args.process)
                    })
                    .collect();
                result = json!({"count": windows.len(), "windows": windows});
            }
            Ok(result)
        }),
        Command::Describe(args) => {
            let destination = args.out.clone();
            let outcome = with_driver(|driver| {
                let mut arguments = describe_arguments(&args.window);
                if let Some(region) = args.region.as_deref() {
                    arguments["region"] = json!(region);
                }
                // A file takes everything. Asked for both, the file wins, and the
                // answer says so rather than leaving the caller to believe the
                // ceiling applied.
                let ceiling = if destination.is_some() {
                    Some(0)
                } else {
                    args.max_characters
                };
                if let Some(value) = ceiling {
                    arguments["max_characters"] = json!(value);
                }
                driver
                    .call("describe", &arguments)
                    .map_err(|error| driver_failure(&error))
            });
            match (&outcome, destination) {
                // Only write when the driver actually answered: writing a failure
                // payload to the file would look like a listing on next read.
                ((answer, EXIT_OK), Some(path)) => {
                    write_listing(answer, &path, args.max_characters)
                }
                _ => outcome,
            }
        }
        Command::Overview(args) => with_driver(|driver| {
            driver
                .call("overview", &describe_arguments(args))
                .map_err(|error| driver_failure(&error))
        }),
        Command::Snapshot(args) => with_driver(|driver| {
            driver
                .call("snapshot", &describe_arguments(args))
                .map_err(|error| driver_failure(&error))
        }),
        Command::Find(args) => {
            // Worked out once for both routes below. Duplicating it is how
            // `--optional` ends up silently not applying to `--locator`.
            let expectation = expectation_for(args);

            // A whole locator as JSON wins outright: it is the escape hatch for
            // shapes the flags cannot express, so mixing the two would only
            // raise the question of which half applied.
            if let Some(raw) = &args.locator {
                let parsed: Value = match serde_json::from_str(raw) {
                    Ok(value) => value,
                    Err(error) => {
                        return (
                            failure(
                                "CLI.INVALID_ARGUMENTS",
                                &format!("--locator is not valid JSON: {error}"),
                                None,
                            ),
                            EXIT_USAGE,
                        )
                    }
                };
                let window_id = args.window_id.clone();
                return with_driver(move |driver| {
                    driver
                        .call(
                            "find",
                            &json!({
                                "window_id": window_id,
                                "locator": parsed,
                                "expect": expectation,
                            }),
                        )
                        .map_err(|error| driver_failure(&error))
                });
            }

            let mut locator = serde_json::Map::new();
            if let Some(role) = &args.role {
                locator.insert("role".into(), json!(role));
            }
            if let Some(name) = &args.name {
                locator.insert("name".into(), json!(name));
            }
            if let Some(id) = &args.automation_id {
                locator.insert("automation_id".into(), json!(id));
            }
            if let Some(protected) = args.protected {

                locator.insert(

                    "states".into(),

                    json!({"protected": protected}),

                );

            }

            if args.contains {
                locator.insert("match".into(), json!("contains"));
            }
            if let Some(nth) = &args.nth {
                // A number if it looks like one, otherwise the word, so both
                // `--nth 2` and `--nth last` reach the driver in the shape it
                // expects. Validation belongs to the locator parser, which
                // reports the position rules in one place.
                locator.insert(
                    "nth".into(),
                    match nth.parse::<u64>() {
                        Ok(index) => json!(index),
                        Err(_) => json!(nth),
                    },
                );
            }
            if let Some(near) = &args.near {
                let mut relation = serde_json::Map::new();
                relation.insert("anchor".into(), json!({"name": near}));
                if let Some(direction) = &args.direction {
                    relation.insert("direction".into(), json!(direction));
                }
                if let Some(within) = args.within {
                    relation.insert("within".into(), json!(within));
                }
                locator.insert("near".into(), Value::Object(relation));
            }
            if let Some(inside) = &args.inside {
                let mut container = serde_json::Map::new();
                container.insert("name".into(), json!(inside));
                if let Some(role) = &args.inside_role {
                    container.insert("role".into(), json!(role));
                }
                locator.insert("within".into(), Value::Object(container));
            }
            if locator.is_empty() {
                return (
                    failure(
                        "CLI.INVALID_ARGUMENTS",
                        "give at least one of --role, --name, --automation-id, --protected, --nth, --near, --in or --locator",
                        None,
                    ),
                    EXIT_USAGE,
                );
            }
            let window_id = args.window_id.clone();
            with_driver(move |driver| {
                driver
                    .call(
                        "find",
                        &json!({
                            "window_id": window_id,
                            "locator": Value::Object(locator),
                            "expect": expectation,
                        }),
                    )
                    .map_err(|error| driver_failure(&error))
            })
        }
        Command::Record(args) => {
            let window_id = args.window_id.clone();
            let seconds = args.seconds.clamp(1, 600);
            with_driver(move |driver| {
                let started = driver
                    .call("watch", &json!({"window_id": window_id}))
                    .map_err(|error| driver_failure(&error))?;
                let capture_id = started["capture_id"]
                    .as_str()
                    .expect("watch returns a capture id")
                    .to_string();

                // Poll rather than sleeping the whole time: the buffer is
                // bounded, so a long recording of a busy window would otherwise
                // overflow and lose the earliest steps.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(seconds);
                let mut steps: Vec<Value> = Vec::new();
                let mut dropped = 0u64;
                let mut sources = started["sources"].clone();
                while std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let batch = driver
                        .call("collect", &json!({"capture_id": capture_id}))
                        .map_err(|error| driver_failure(&error))?;
                    if let Some(found) = batch["steps"].as_array() {
                        steps.extend(found.iter().cloned());
                    }
                    dropped += batch["dropped"].as_u64().unwrap_or(0);
                }

                // Release even if the last collect failed: leaving hooks
                // installed would outlive the command that asked for them.
                let release = driver.call("release", &json!({"capture_id": capture_id}));
                if sources.is_null() {
                    sources = json!([]);
                }
                release.map_err(|error| driver_failure(&error))?;

                let replayable = steps
                    .iter()
                    .filter(|step| step["replayable"] == json!(true))
                    .count();
                Ok(json!({
                    "window_id": started["window_id"],
                    "sources": sources,
                    "seconds": seconds,
                    "steps": steps,
                    "count": steps.len(),
                    // Split out because the difference is what a caller acts on:
                    // steps that cannot replay need a person to look at them.
                    "replayable": replayable,
                    "dropped": dropped,
                }))
            })
        }
        Command::Do(action) => {
            let (name, raw_target, extra) = match action {
                DoCommand::Focus(args) => ("focus", &args.target, json!({})),
                DoCommand::Invoke(args) => ("invoke", &args.target, json!({})),
                DoCommand::Click(args) => ("pointer_click", &args.target, json!({})),
                DoCommand::SetValue(args) => {
                    ("set_value", &args.target, json!({"value": args.value}))
                }
                DoCommand::TypeText(args) => {
                    ("type_text", &args.target, json!({"text": args.text}))
                }
            };

            // Accept the compact reference and raw JSON alike: the reference is
            // what survives shell quoting, but JSON is what a script may hold.
            let target: Value = match serde_json::from_str::<Value>(raw_target) {
                Ok(value) if value.is_object() => value,
                _ => match aad_uia::Target::parse_ref(raw_target) {
                    Ok(target) => target.to_json(),
                    Err(error) => {
                        return (
                            failure(
                                "CLI.INVALID_ARGUMENTS",
                                &format!("--target is not usable: {error}"),
                                Some(json!({
                                    "hint": "Pass the `ref` printed by `aad find`, \
for example abc123:1:e9."
                                })),
                            ),
                            EXIT_USAGE,
                        );
                    }
                },
            };

            let mut arguments = extra.as_object().cloned().unwrap_or_default();
            arguments.insert("target".into(), target);
            with_driver(move |driver| {
                driver
                    .call(name, &Value::Object(arguments))
                    .map_err(|error| driver_failure(&error))
            })
        }
    }
}

fn describe_arguments(args: &DescribeArgs) -> Value {
    let mut arguments = serde_json::Map::new();
    arguments.insert("window_id".into(), json!(args.window_id));
    arguments.insert("limit".into(), json!(args.limit));
    if let Some(depth) = args.max_depth {
        arguments.insert("max_depth".into(), json!(depth));
    }
    if let Some(nodes) = args.max_nodes {
        arguments.insert("max_nodes".into(), json!(nodes));
    }
    Value::Object(arguments)
}

/// Run a closure against the native driver, reporting a missing driver clearly.
fn with_driver<F>(action: F) -> (Value, u8)
where
    F: FnOnce(&aad_uia::UiaDriver) -> Result<Value, Value>,
{
    let driver = match aad_uia::native_driver() {
        Ok(driver) => driver,
        Err(error) => {
            return (
                failure(
                    &error.code,
                    &error.message,
                    Some(json!({"hint": "Run `aad probe` to see what is missing."})),
                ),
                EXIT_FAILED,
            )
        }
    };
    match action(&driver) {
        Ok(result) => (result, EXIT_OK),
        Err(payload) => (payload, EXIT_FAILED),
    }
}

/// Write a listing to a file, answering with where it went rather than with the
/// listing itself.
///
/// The answer stays small on purpose: a caller that asked for a file wants the
/// file, and echoing tens of thousands of characters back to the terminal as well
/// would undo the reason for asking.
fn write_listing(
    answer: &Value,
    path: &std::path::Path,
    requested_ceiling: Option<u64>,
) -> (Value, u8) {
    let rendered = match serde_json::to_string_pretty(answer) {
        Ok(text) => text,
        Err(error) => {
            return (
                failure(
                    "CLI.SERIALISE_FAILED",
                    &format!("could not render the listing: {error}"),
                    None,
                ),
                EXIT_FAILED,
            )
        }
    };
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return (
                failure(
                    "CLI.WRITE_FAILED",
                    &format!("could not create {}: {error}", parent.display()),
                    None,
                ),
                EXIT_FAILED,
            );
        }
    }
    if let Err(error) = std::fs::write(path, &rendered) {
        return (
            failure(
                "CLI.WRITE_FAILED",
                &format!("could not write {}: {error}", path.display()),
                None,
            ),
            EXIT_FAILED,
        );
    }

    let mut summary = json!({
        "written_to": path.display().to_string(),
        "characters": rendered.chars().count(),
        "shown": answer["shown"].clone(),
        "matched": answer["matched"].clone(),
        "truncated": answer["truncated"].clone(),
    });
    if let Some(region) = answer.get("region") {
        summary["region"] = region.clone();
    }
    if requested_ceiling.is_some() {
        summary["note"] =
            json!("--max-characters was ignored: a file is written in full");
    }
    (summary, EXIT_OK)
}

fn driver_failure(error: &aad_uia::DriverError) -> Value {
    let mut payload = json!({
        "status": "error",
        "error": {
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable,
            "effect": error.effect,
        }
    });
    if !error.details.is_empty() {
        payload["error"]["details"] = Value::Object(error.details.clone());
    }
    payload
}

fn failure(code: &str, message: &str, extra: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(extra) = extra {
        if let Some(fields) = extra.as_object() {
            for (key, value) in fields {
                error[key] = value.clone();
            }
        }
    }
    json!({"status": "error", "error": error})
}

/// Read a descriptor from YAML or JSON.
fn read_descriptor(path: &Path) -> Result<Value, Value> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        failure(
            "CLI.FILE_UNREADABLE",
            &format!("{}: {error}", path.display()),
            None,
        )
    })?;

    // Windows editors routinely save a UTF-8 byte order mark, which no JSON or
    // YAML parser accepts. Refusing such a file would be a baffling failure.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    // YAML is a superset of JSON, so one parser handles both; but JSON gets
    // first refusal so that duplicate-key rejection still applies.
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }
    serde_yaml::from_str::<Value>(text).map_err(|error| {
        failure(
            "CLI.FILE_INVALID",
            &format!("{} is not valid JSON or YAML: {error}", path.display()),
            None,
        )
    })
}

fn validate(path: &Path) -> (Value, u8) {
    let document = match read_descriptor(path) {
        Ok(document) => document,
        Err(payload) => return (payload, EXIT_USAGE),
    };

    match aad_core::compile_descriptor(document, None) {
        Ok(workflow) => (
            json!({
                "status": "valid",
                "workflow": workflow.name,
                "steps": workflow.steps.len(),
                "plan_digest": aad_runtime::plan_digest(&workflow),
            }),
            EXIT_OK,
        ),
        Err(error) => {
            // Report every problem at once: fixing them one run at a time is
            // needlessly slow.
            let issues: Vec<Value> = error
                .issues
                .iter()
                .map(|issue| json!({"path": issue.path, "message": issue.message}))
                .collect();
            (
                json!({
                    "status": "invalid",
                    "error": {
                        "code": error.code,
                        "message": error.message,
                        "issue_count": issues.len(),
                        "issues": issues,
                    }
                }),
                EXIT_FAILED,
            )
        }
    }
}

fn run_workflow(args: &RunArgs) -> (Value, u8) {
    let document = match read_descriptor(&args.file) {
        Ok(document) => document,
        Err(payload) => return (payload, EXIT_USAGE),
    };

    let workflow = match aad_core::compile_descriptor(document, None) {
        Ok(workflow) => workflow,
        Err(error) => {
            return (
                json!({
                    "status": "invalid",
                    "error": {
                        "code": error.code,
                        "message": error.message,
                        "issues": error.issues.iter()
                            .map(|issue| json!({"path": issue.path, "message": issue.message}))
                            .collect::<Vec<_>>(),
                    }
                }),
                EXIT_FAILED,
            )
        }
    };

    let inputs = match &args.inputs {
        None => serde_json::Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => {
                return (
                    failure("CLI.INVALID_ARGUMENTS", "--inputs must be a JSON object", None),
                    EXIT_USAGE,
                )
            }
            Err(error) => {
                return (
                    failure(
                        "CLI.INVALID_ARGUMENTS",
                        &format!("--inputs is not valid JSON: {error}"),
                        None,
                    ),
                    EXIT_USAGE,
                )
            }
        },
    };

    if args.dry_run {
        return (
            json!({
                "status": "planned",
                "workflow": workflow.name,
                "steps": workflow.steps.iter().map(|step| json!({
                    "id": step.id,
                    "type": step.step_type.as_str(),
                    "uses": step.params.get("uses"),
                    "depends_on": step.depends_on,
                })).collect::<Vec<_>>(),
                "plan_digest": aad_runtime::plan_digest(&workflow),
            }),
            EXIT_OK,
        );
    }

    let mut providers = aad_runtime::ProviderRegistry::new();
    // The desktop driver is always offered; a workflow that never uses it pays
    // nothing, and one that does should not need extra configuration.
    if let Ok(driver) = aad_uia::native_driver() {
        providers.insert(std::sync::Arc::new(driver));
    }

    let mut options = aad_runtime::RunOptions::default()
        .with_providers(providers)
        .with_inputs(inputs)
        .with_scripts_allowed(args.allow_scripts);

    if let Some(path) = &args.journal {
        match std::fs::File::create(path) {
            Ok(file) => {
                options.sink = Some(std::sync::Arc::new(aad_runtime::NdjsonSink::new(
                    std::io::BufWriter::new(file),
                )))
            }
            Err(error) => {
                return (
                    failure(
                        "CLI.JOURNAL_UNWRITABLE",
                        &format!("{}: {error}", path.display()),
                        None,
                    ),
                    EXIT_USAGE,
                )
            }
        }
    }

    let result = aad_runtime::run(&workflow, options);
    let code = match result.status {
        aad_runtime::RunStatus::Succeeded => EXIT_OK,
        _ => EXIT_FAILED,
    };
    // `summary` is the shape meant for a caller reading the result, rather
    // than the full Run document, which nests outputs under `output`.
    (result.summary(), code)
}

// ------------------------------------------------------------ durable runs

/// Open the run store, reporting an unusable path clearly.
fn with_store<F>(args: &StoreArgs, action: F) -> (Value, u8)
where
    F: FnOnce(&aad_runtime::durable::JournalStore) -> Result<Value, Value>,
{
    let store = match aad_runtime::durable::JournalStore::open(&args.store) {
        Ok(store) => store,
        Err(error) => {
            return (
                failure(
                    error.code(),
                    error.message(),
                    Some(json!({"store": args.store.display().to_string()})),
                ),
                EXIT_USAGE,
            )
        }
    };
    match action(&store) {
        Ok(payload) => (payload, EXIT_OK),
        Err(payload) => (payload, EXIT_FAILED),
    }
}

fn journal_failure(error: aad_runtime::durable::JournalError) -> Value {
    failure(error.code(), error.message(), None)
}

/// Render an `AutomationError` the way the rest of the CLI renders failures.
fn automation_failure(error: &aad_core::AutomationError) -> Value {
    let mut payload = json!({
        "status": "error",
        "error": {
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable,
            "effect": error.effect,
        }
    });
    if !error.details.is_empty() {
        payload["error"]["details"] = Value::Object(error.details.clone());
    }
    payload
}

/// Compile the descriptor a durable command was given.
fn durable_descriptor(path: &Path) -> Result<aad_core::WorkflowDescriptor, (Value, u8)> {
    let document = read_descriptor(path).map_err(|payload| (payload, EXIT_USAGE))?;
    aad_core::compile_descriptor(document, None).map_err(|error| {
        (
            json!({
                "status": "invalid",
                "error": {
                    "code": error.code,
                    "message": error.message,
                    "issues": error.issues.iter()
                        .map(|issue| json!({"path": issue.path, "message": issue.message}))
                        .collect::<Vec<_>>(),
                }
            }),
            EXIT_FAILED,
        )
    })
}

fn durable_options(
    inputs: Option<&String>,
    owner_id: Option<&String>,
    lease_ttl: Option<f64>,
) -> Result<aad_runtime::durable_exec::DurableOptions, (Value, u8)> {
    let mut options = aad_runtime::durable_exec::DurableOptions::default();
    if let Some(text) = inputs {
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => options = options.with_inputs(map),
            Ok(_) => {
                return Err((
                    failure("CLI.INVALID_ARGUMENTS", "--inputs must be a JSON object", None),
                    EXIT_USAGE,
                ))
            }
            Err(error) => {
                return Err((
                    failure(
                        "CLI.INVALID_ARGUMENTS",
                        &format!("--inputs is not valid JSON: {error}"),
                        None,
                    ),
                    EXIT_USAGE,
                ))
            }
        }
    }
    if let Some(owner) = owner_id {
        options = options.with_owner_id(owner.clone());
    }
    if let Some(ttl) = lease_ttl {
        // Rejected here rather than deeper down, so the message names the flag
        // the user actually typed.
        if !(ttl.is_finite() && ttl > 0.0) {
            return Err((
                failure(
                    "CLI.INVALID_ARGUMENTS",
                    "--lease-ttl must be a positive number of seconds",
                    None,
                ),
                EXIT_USAGE,
            ));
        }
        options = options.with_lease_ttl_seconds(ttl);
    }
    Ok(options)
}

/// The exit code for a durable attempt.
///
/// `paused` is a success: the run stopped because it was asked to, and the
/// caller's request was carried out. Everything else non-terminal-successful is
/// a failure a script must be able to detect.
fn durable_code(outcome: &aad_runtime::durable_exec::DurableOutcome) -> u8 {
    use aad_runtime::durable::RunStatus;
    match outcome.run.status {
        RunStatus::Succeeded | RunStatus::Paused => EXIT_OK,
        _ => EXIT_FAILED,
    }
}

fn start_run(args: &StartArgs) -> (Value, u8) {
    let descriptor = match durable_descriptor(&args.file) {
        Ok(descriptor) => descriptor,
        Err(result) => return result,
    };
    let options = match durable_options(
        args.inputs.as_ref(),
        args.owner_id.as_ref(),
        args.lease_ttl,
    ) {
        Ok(options) => options,
        Err(result) => return result,
    };

    // Opened separately from `with_store` because the executor takes ownership
    // of the connection for the whole run.
    let store = match aad_runtime::durable::JournalStore::open(&args.store.store) {
        Ok(store) => store,
        Err(error) => {
            return (
                failure(
                    error.code(),
                    error.message(),
                    Some(json!({"store": args.store.store.display().to_string()})),
                ),
                EXIT_USAGE,
            )
        }
    };
    let executor = aad_runtime::durable_exec::DurableExecutor::new(store);
    match executor.start(&descriptor, args.run_id.as_deref(), options) {
        Ok(outcome) => (outcome.to_json(), durable_code(&outcome)),
        Err(error) => (automation_failure(&error), EXIT_FAILED),
    }
}

fn resume_run(args: &ResumeArgs) -> (Value, u8) {
    let descriptor = match durable_descriptor(&args.file) {
        Ok(descriptor) => descriptor,
        Err(result) => return result,
    };
    // No `--inputs` here by design: the run's inputs were persisted when it was
    // created, and letting them be replaced on resume would mean the second half
    // of a run executed against different values than the first.
    let options = match durable_options(None, args.owner_id.as_ref(), args.lease_ttl) {
        Ok(options) => options,
        Err(result) => return result,
    };

    let store = match aad_runtime::durable::JournalStore::open(&args.store.store) {
        Ok(store) => store,
        Err(error) => {
            return (
                failure(
                    error.code(),
                    error.message(),
                    Some(json!({"store": args.store.store.display().to_string()})),
                ),
                EXIT_USAGE,
            )
        }
    };
    let executor = aad_runtime::durable_exec::DurableExecutor::new(store);
    match executor.resume(&descriptor, &args.run_id, options) {
        Ok(outcome) => (outcome.to_json(), durable_code(&outcome)),
        Err(error) => (automation_failure(&error), EXIT_FAILED),
    }
}

#[derive(Clone, Copy)]
enum Intent {
    Pause,
    Cancel,
}

impl Intent {
    fn desired(self) -> aad_runtime::durable::DesiredState {
        match self {
            Intent::Pause => aad_runtime::durable::DesiredState::Pause,
            Intent::Cancel => aad_runtime::durable::DesiredState::Cancel,
        }
    }

    fn event(self) -> &'static str {
        match self {
            Intent::Pause => "run.pause_requested",
            Intent::Cancel => "run.cancel_requested",
        }
    }
}

/// Record operator intent against a run.
///
/// This only asks. The runner applies the request at its next safe point, so
/// the reply reports `desiredState` rather than claiming the run has already
/// stopped — saying "paused" while a step is still in flight would be a lie the
/// operator might act on.
fn control(store: &StoreArgs, run_id: &str, intent: Intent) -> (Value, u8) {
    use aad_runtime::durable::DesiredState;

    with_store(store, |journal| {
        let run = journal.get_run(run_id).map_err(journal_failure)?;

        // A terminal run is immutable; pretending to accept the request would
        // leave the operator waiting for something that will never happen.
        if run.is_terminal() {
            return Err(failure(
                "RUN.TERMINAL",
                &format!(
                    "terminal run {run_id} is immutable ({})",
                    run.status.as_str()
                ),
                Some(json!({"status": run.status.as_str()})),
            ));
        }
        // Asking twice is a success, not a conflict: the operator's wish is
        // already recorded, and an error here would suggest it was not.
        if run.desired_state == intent.desired() {
            return Ok(run.to_json());
        }
        // Cancel is sticky. Downgrading it to a pause would silently revive a
        // run somebody has already decided to stop.
        if run.desired_state == DesiredState::Cancel {
            return Err(failure(
                "RUN.CANCEL_PENDING",
                &format!("run {run_id} already has a sticky cancel request"),
                Some(json!({"requestedDesiredState": intent.desired().as_str()})),
            ));
        }

        let payload = json!({
            "fromDesiredState": run.desired_state.as_str(),
            "toDesiredState": intent.desired().as_str(),
        });
        journal
            .compare_and_set_desired_state(
                run_id,
                run.desired_state,
                intent.desired(),
                Some((intent.event(), &payload)),
            )
            .map(|updated| updated.to_json())
            .map_err(journal_failure)
    })
}

fn list_runs(args: &ListArgs) -> (Value, u8) {
    let status = match &args.status {
        None => None,
        Some(text) => match aad_runtime::durable::RunStatus::parse(text) {
            Ok(status) => Some(status),
            Err(error) => {
                return (
                    failure("CLI.INVALID_ARGUMENTS", error.message(), None),
                    EXIT_USAGE,
                )
            }
        },
    };
    with_store(&args.store, |journal| {
        let runs = journal
            .list_runs(status, args.limit, args.offset)
            .map_err(journal_failure)?;
        Ok(json!({
            "count": runs.len(),
            "runs": runs.iter().map(|run| run.to_json()).collect::<Vec<_>>(),
        }))
    })
}

fn list_events(args: &EventsArgs) -> (Value, u8) {
    with_store(&args.store, |journal| {
        let events = journal
            .list_events(&args.run_id, args.after_seq, args.limit)
            .map_err(journal_failure)?;
        // `nextAfterSeq` lets a caller tail the history without re-reading it,
        // and holds its position when the page came back empty.
        let next = events.last().map(|event| event.seq).unwrap_or(args.after_seq);
        Ok(json!({
            "count": events.len(),
            "events": events.iter().map(|event| event.to_json()).collect::<Vec<_>>(),
            "nextAfterSeq": next,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_internally_consistent() {
        // Catches conflicting flags, duplicate names and bad defaults.
        Cli::command().debug_assert();
    }

    #[test]
    fn every_command_is_reachable_and_documented() {
        let command = Cli::command();
        let names: Vec<&str> = command
            .get_subcommands()
            .map(|sub| sub.get_name())
            .collect();

        for expected in [
            "apps", "describe", "snapshot", "find", "do", "probe", "validate", "run", "mcp",
            "tools", "start", "resume", "status", "pause", "cancel", "list", "events",
        ] {
            assert!(names.contains(&expected), "missing command {expected}");
        }
        for sub in command.get_subcommands() {
            assert!(
                sub.get_about().is_some(),
                "{} needs a description",
                sub.get_name()
            );
        }
    }

    #[test]
    fn the_action_subcommands_all_require_a_target() {
        let command = Cli::command();
        let action = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "do")
            .expect("the do command");

        for sub in action.get_subcommands() {
            let has_target = sub.get_arguments().any(|arg| arg.get_id() == "target");
            assert!(has_target, "{} must take a target", sub.get_name());
        }
    }

    #[test]
    fn a_find_with_no_criteria_is_a_usage_error() {
        let (payload, code) = dispatch(&Command::Find(FindArgs {
            window_id: "hwnd:1".into(),
            ..Default::default()
        }));

        assert_eq!(code, EXIT_USAGE);
        assert_eq!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
    }

    #[test]
    fn matching_a_password_field_alone_is_enough_to_search() {
        // A password box often exposes no name and no automation id, so
        // `--protected` has to count as a criterion in its own right. Reaching
        // the driver is proof enough here: in a test environment the driver is
        // what fails, not argument validation.
        let (payload, code) = dispatch(&Command::Find(FindArgs {
            window_id: "hwnd:1".into(),
            protected: Some(true),
            ..Default::default()
        }));

        assert_ne!(code, EXIT_USAGE, "protected alone must be a valid criterion");
        assert_ne!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
    }

    #[test]
    fn a_position_alone_is_enough_to_search() {
        // "the third button" and "the box next to Username" are whole
        // descriptions on their own. Requiring an attribute alongside them
        // would rule out the elements that need them most -- the ones with no
        // usable attribute at all.
        for args in [
            FindArgs {
                window_id: "hwnd:1".into(),
                nth: Some("2".into()),
                ..Default::default()
            },
            FindArgs {
                window_id: "hwnd:1".into(),
                near: Some("Username".into()),
                ..Default::default()
            },
        ] {
            let (payload, code) = dispatch(&Command::Find(args));
            assert_ne!(code, EXIT_USAGE, "a position alone must be a valid criterion");
            assert_ne!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
        }
    }

    #[test]
    fn a_container_scopes_the_search_and_the_counting() {
        // Verified against a real window: `--nth 1` alone selects Minimise,
        // because the count spans the frame. Scoped to a panel it selects that
        // panel's first button, and the same number in a different panel selects
        // a different element -- which is what makes the position usable.
        let args = Cli::parse_from([
            "aad",
            "find",
            "hwnd:1",
            "--role",
            "button",
            "--in",
            "Terminal actions",
            "--nth",
            "1",
        ]);
        let Command::Find(find) = args.command else {
            panic!("expected find");
        };

        assert_eq!(find.inside.as_deref(), Some("Terminal actions"));
        assert_eq!(find.nth.as_deref(), Some("1"));
    }

    #[test]
    fn a_container_role_needs_the_container_it_narrows() {
        // `--in-role tool_bar` on its own would read as "in some toolbar", which
        // matches several and therefore nothing -- a locator that looks specific
        // and finds none.
        let refused = Cli::try_parse_from([
            "aad", "find", "hwnd:1", "--role", "button", "--in-role", "tool_bar",
        ]);

        assert!(refused.is_err(), "a container role without a container must be refused");
    }

    #[test]
    fn a_distance_and_a_container_are_separate_flags() {
        // Both are `within` in the driver's JSON -- a number under `near`, an
        // object at the top level. One flag for both would send `40` and
        // `tool_bar` to the same place.
        let args = Cli::parse_from([
            "aad",
            "find",
            "hwnd:1",
            "--role",
            "edit",
            "--near",
            "Name:",
            "--within",
            "40",
            "--in",
            "Details",
        ]);
        let Command::Find(find) = args.command else {
            panic!("expected find");
        };

        assert_eq!(find.within, Some(40), "the pixel distance");
        assert_eq!(find.inside.as_deref(), Some("Details"), "the container");
    }

    #[test]
    fn optional_and_any_relax_different_things() {
        // Two different questions, and swapping them is how automation goes
        // wrong quietly. `--optional` asks whether something is there, so it
        // must not also start picking one of several. `--any` asks for one of
        // several, so it must not also start reporting absence as success --
        // the caller wants an element to act on and would get nothing.
        let strict = expectation_for(&FindArgs::default());
        assert_eq!(strict, None, "the default must refuse both");

        let optional = expectation_for(&FindArgs {
            optional: true,
            ..Default::default()
        });
        assert_eq!(optional, Some("optional"));

        let any = expectation_for(&FindArgs {
            any: true,
            ..Default::default()
        });
        assert_eq!(any, Some("any"));
    }

    #[test]
    fn the_expectation_reaches_the_driver_through_the_json_route_too() {
        // `--locator` is a separate code path from the match flags. A caller
        // who writes a descriptive locator as JSON and adds `--optional` would
        // otherwise get the strict behaviour, and the flag would look broken
        // for no visible reason.
        let (payload, code) = dispatch(&Command::Find(FindArgs {
            window_id: "hwnd:1".into(),
            locator: Some(r#"{"role":"button"}"#.into()),
            optional: true,
            ..Default::default()
        }));

        // No desktop here, so the call cannot succeed -- but it must fail for
        // the reason that proves the argument was accepted and forwarded,
        // rather than being rejected as a usage error.
        assert_ne!(code, EXIT_USAGE, "the combination must be valid");
        assert_ne!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
    }

    #[test]
    fn a_malformed_locator_json_is_a_usage_error() {
        // The escape hatch is hand-written or agent-written JSON, so a typo in
        // it has to say so rather than reaching the driver as an empty locator.
        let (payload, code) = dispatch(&Command::Find(FindArgs {
            window_id: "hwnd:1".into(),
            locator: Some("{not json".into()),
            ..Default::default()
        }));

        assert_eq!(code, EXIT_USAGE);
        assert_eq!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
    }

    #[test]
    fn a_malformed_target_is_a_usage_error_with_a_hint() {
        let (payload, code) = dispatch(&Command::Do(DoCommand::Invoke(TargetArgs {
            target: "not a target".into(),
        })));

        assert_eq!(code, EXIT_USAGE);
        assert_eq!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
        assert!(payload["error"]["hint"].as_str().unwrap().contains("aad find"));
    }

    #[test]
    fn a_compact_reference_is_accepted_where_a_target_is_required() {
        // A reference for a snapshot that does not exist must fail on the
        // lookup, not on parsing: that proves the reference itself was read.
        let (payload, code) = dispatch(&Command::Do(DoCommand::Invoke(TargetArgs {
            target: "deadbeef:1:e9".into(),
        })));

        assert_ne!(
            payload["error"]["code"], "CLI.INVALID_ARGUMENTS",
            "a well-formed reference must not be a usage error: {payload}"
        );
        assert_eq!(code, EXIT_FAILED);
    }

    #[test]
    fn probe_reports_the_environment_and_succeeds_when_usable() {
        let (payload, code) = dispatch(&Command::Probe);

        assert_eq!(payload["kind"], "CapabilityProbe");
        if cfg!(windows) {
            assert_eq!(code, EXIT_OK, "a Windows desktop should be usable");
        }
    }

    #[test]
    fn tools_lists_the_mcp_surface() {
        let (payload, code) = dispatch(&Command::Tools);

        assert_eq!(code, EXIT_OK);
        assert!(!payload["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_missing_file_is_reported_as_a_usage_error() {
        let (payload, code) = dispatch(&Command::Validate(FileArgs {
            file: PathBuf::from("no/such/workflow.yaml"),
        }));

        assert_eq!(code, EXIT_USAGE);
        assert_eq!(payload["error"]["code"], "CLI.FILE_UNREADABLE");
    }

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("aad-cli-test-{name}"));
        std::fs::write(&path, contents).expect("the fixture can be written");
        path
    }

    #[test]
    fn a_valid_descriptor_validates_and_reports_a_digest() {
        let path = write_temp(
            "valid.yaml",
            "apiVersion: ai-auto-desktop.dev/v1alpha1\n\
             kind: Workflow\n\
             metadata:\n  name: demo\n\
             budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n\
             steps:\n  - id: done\n    type: return\n    value: 1\n",
        );

        let (payload, code) = dispatch(&Command::Validate(FileArgs { file: path.clone() }));
        let _ = std::fs::remove_file(&path);

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["status"], "valid");
        assert_eq!(payload["workflow"], "demo");
        assert!(payload["plan_digest"].as_str().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn an_invalid_descriptor_reports_every_problem_at_once() {
        let path = write_temp(
            "invalid.yaml",
            "apiVersion: wrong/version\n\
             kind: NotAWorkflow\n\
             metadata:\n  name: demo\n\
             steps: []\n",
        );

        let (payload, code) = dispatch(&Command::Validate(FileArgs { file: path.clone() }));
        let _ = std::fs::remove_file(&path);

        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["status"], "invalid");
        // Reporting one problem per run would make fixing a file needlessly slow.
        assert!(
            payload["error"]["issue_count"].as_u64().unwrap() >= 2,
            "{payload}"
        );
    }

    #[test]
    fn json_descriptors_are_accepted_as_well_as_yaml() {
        let path = write_temp(
            "valid.json",
            r#"{"apiVersion":"ai-auto-desktop.dev/v1alpha1","kind":"Workflow",
                "metadata":{"name":"json-demo"},
                "budgets":{"max_duration":"30s","max_executed_steps":10},
                "steps":[{"id":"done","type":"return","value":1}]}"#,
        );

        let (payload, code) = dispatch(&Command::Validate(FileArgs { file: path.clone() }));
        let _ = std::fs::remove_file(&path);

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["workflow"], "json-demo");
    }

    #[test]
    fn a_file_saved_with_a_byte_order_mark_still_parses() {
        // PowerShell's `Set-Content -Encoding utf8` writes a BOM by default,
        // so refusing one would break the most obvious way to create a file.
        let path = write_temp(
            "bom.yaml",
            "\u{feff}apiVersion: ai-auto-desktop.dev/v1alpha1\n\
             kind: Workflow\n\
             metadata:\n  name: bom-demo\n\
             budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n\
             steps:\n  - id: done\n    type: return\n    value: 1\n",
        );

        let (payload, code) = dispatch(&Command::Validate(FileArgs { file: path.clone() }));
        let _ = std::fs::remove_file(&path);

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["workflow"], "bom-demo");
    }

    #[test]
    fn a_dry_run_reports_the_plan_without_executing_it() {
        let path = write_temp(
            "dryrun.yaml",
            "apiVersion: ai-auto-desktop.dev/v1alpha1\n\
             kind: Workflow\n\
             metadata:\n  name: demo\n\
             budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n\
             steps:\n  - id: done\n    type: return\n    value: 1\n",
        );

        let (payload, code) = dispatch(&Command::Run(RunArgs {
            file: path.clone(),
            inputs: None,
            journal: None,
            dry_run: true,
            allow_scripts: false,
        }));
        let _ = std::fs::remove_file(&path);

        assert_eq!(code, EXIT_OK);
        assert_eq!(payload["status"], "planned");
        assert_eq!(payload["steps"][0]["id"], "done");
    }

    #[test]
    fn a_workflow_runs_and_reports_its_result() {
        let path = write_temp(
            "run.yaml",
            "apiVersion: ai-auto-desktop.dev/v1alpha1\n\
             kind: Workflow\n\
             metadata:\n  name: demo\n\
             budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n\
             inputs:\n  who:\n    schema: {type: string}\n    default: world\n\
             steps:\n  - id: done\n    type: return\n    value: 1\n\
             outputs:\n  greeting:\n    value: \"${{ inputs.who }}\"\n",
        );

        let (payload, code) = dispatch(&Command::Run(RunArgs {
            file: path.clone(),
            inputs: Some(r#"{"who":"agent"}"#.to_string()),
            journal: None,
            dry_run: false,
            allow_scripts: false,
        }));
        let _ = std::fs::remove_file(&path);

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["status"], "succeeded");
        assert_eq!(payload["outputs"]["greeting"], "agent");
    }

    #[test]
    fn malformed_inputs_are_a_usage_error() {
        let path = write_temp(
            "inputs.yaml",
            "apiVersion: ai-auto-desktop.dev/v1alpha1\n\
             kind: Workflow\n\
             metadata:\n  name: demo\n\
             budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n\
             steps:\n  - id: done\n    type: return\n    value: 1\n",
        );

        for bad in ["not json", "[1,2,3]"] {
            let (payload, code) = dispatch(&Command::Run(RunArgs {
                file: path.clone(),
                inputs: Some(bad.to_string()),
                journal: None,
                dry_run: false,
                allow_scripts: false,
            }));
            assert_eq!(code, EXIT_USAGE, "{bad} should be rejected");
            assert_eq!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_run_writes_a_journal_when_asked() {
        let path = write_temp(
            "journalled.yaml",
            "apiVersion: ai-auto-desktop.dev/v1alpha1\n\
             kind: Workflow\n\
             metadata:\n  name: demo\n\
             budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n\
             steps:\n  - id: done\n    type: return\n    value: 1\n",
        );
        let journal = std::env::temp_dir().join("aad-cli-test-journal.ndjson");
        let _ = std::fs::remove_file(&journal);

        let (_, code) = dispatch(&Command::Run(RunArgs {
            file: path.clone(),
            inputs: None,
            journal: Some(journal.clone()),
            dry_run: false,
            allow_scripts: false,
        }));

        assert_eq!(code, EXIT_OK);
        let written = std::fs::read_to_string(&journal).expect("the journal exists");
        let lines: Vec<&str> = written.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(!lines.is_empty(), "the journal must record the run");
        for line in &lines {
            serde_json::from_str::<Value>(line).expect("each line is JSON");
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
    }

    #[test]
    fn a_failing_workflow_exits_non_zero() {
        let path = write_temp(
            "failing.yaml",
            "apiVersion: ai-auto-desktop.dev/v1alpha1\n\
             kind: Workflow\n\
             metadata:\n  name: demo\n\
             budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n\
             steps:\n  - id: stop\n    type: fail\n    error:\n      code: DEMO.STOP\n      message: halted\n",
        );

        let (payload, code) = dispatch(&Command::Run(RunArgs {
            file: path.clone(),
            inputs: None,
            journal: None,
            dry_run: false,
            allow_scripts: false,
        }));
        let _ = std::fs::remove_file(&path);

        // A shell script must be able to detect the failure.
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["status"], "failed");
        assert_eq!(payload["error"]["code"], "DEMO.STOP");
    }

    // ------------------------------------------------------- durable commands

    /// A durable-eligible workflow: no action or script steps.
    const DURABLE_WORKFLOW: &str = concat!(
        "apiVersion: ai-auto-desktop.dev/v1alpha1\n",
        "kind: Workflow\n",
        "metadata:\n  name: durable.demo\n",
        "budgets:\n  max_duration: 30s\n  max_executed_steps: 50\n",
        "inputs:\n  label:\n    schema: {type: string}\n    default: hi\n",
        "variables:\n  count:\n",
        "    schema: {type: integer}\n    mutable: true\n    initial: 0\n",
        "steps:\n",
        "  - id: first\n    type: set\n    assign: {vars.count: '${{ vars.count + 1 }}'}\n",
        "  - id: second\n    type: set\n    assign: {vars.count: '${{ vars.count + 1 }}'}\n",
        "outputs:\n",
        "  total:\n    value: \"${{ vars.count }}\"\n",
        "  echo:\n    value: \"${{ inputs.label }}\"\n",
    );

    /// A directory that cleans itself up, so a failed test cannot poison a later
    /// one through a leftover store.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "aad-cli-durable-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the temp directory can be created");
            Self { path }
        }

        fn store(&self) -> StoreArgs {
            StoreArgs {
                store: self.path.join("runs.sqlite3"),
            }
        }

        fn workflow(&self) -> PathBuf {
            let path = self.path.join("workflow.yaml");
            std::fs::write(&path, DURABLE_WORKFLOW).expect("the fixture can be written");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn start_args(temp: &TempDir, run_id: &str, inputs: Option<&str>) -> StartArgs {
        StartArgs {
            file: temp.workflow(),
            store: temp.store(),
            inputs: inputs.map(str::to_string),
            run_id: Some(run_id.to_string()),
            owner_id: Some("test-runner".into()),
            lease_ttl: None,
        }
    }

    #[test]
    fn a_durable_run_persists_to_disk_and_reports_its_outcome() {
        let temp = TempDir::new("start");

        let (payload, code) = dispatch(&Command::Start(start_args(
            &temp,
            "run-1",
            Some(r#"{"label":"agent"}"#),
        )));

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["kind"], "Run");
        assert_eq!(payload["status"], "succeeded");
        assert_eq!(payload["output"]["total"], 2);
        assert_eq!(payload["output"]["echo"], "agent");
        // The store must be a real file, otherwise nothing was durable.
        assert!(temp.store().store.exists(), "the run store must exist on disk");
    }

    #[test]
    fn a_status_query_reads_the_run_back_from_a_separate_open() {
        let temp = TempDir::new("status");
        let (_, code) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));
        assert_eq!(code, EXIT_OK);

        // A fresh open of the store: this is the "come back tomorrow" case.
        let (payload, code) = dispatch(&Command::Status(RunRefArgs {
            run_id: "run-1".into(),
            store: temp.store(),
        }));

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["runId"], "run-1");
        assert_eq!(payload["status"], "succeeded");
        // The lease token must never reach a caller.
        assert!(
            !payload.to_string().contains("token"),
            "the bearer token must not be exposed: {payload}"
        );
    }

    #[test]
    fn an_unknown_run_is_reported_as_not_found() {
        let temp = TempDir::new("missing");
        // Create the store so the failure is about the run, not the file.
        let (_, _) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));

        let (payload, code) = dispatch(&Command::Status(RunRefArgs {
            run_id: "nope".into(),
            store: temp.store(),
        }));

        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["error"]["code"], "JOURNAL.RUN_NOT_FOUND");
    }

    #[test]
    fn events_are_listed_with_a_cursor_for_tailing() {
        let temp = TempDir::new("events");
        let (_, code) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));
        assert_eq!(code, EXIT_OK);

        let (payload, code) = dispatch(&Command::Events(EventsArgs {
            run_id: "run-1".into(),
            store: temp.store(),
            after_seq: 0,
            limit: 1000,
        }));

        assert_eq!(code, EXIT_OK, "{payload}");
        let events = payload["events"].as_array().expect("events");
        assert!(!events.is_empty(), "a completed run has a history");
        assert_eq!(events[0]["kind"], "RunEvent");

        // The cursor must let a caller resume without re-reading.
        let last = payload["nextAfterSeq"].as_i64().expect("cursor");
        let (tail, _) = dispatch(&Command::Events(EventsArgs {
            run_id: "run-1".into(),
            store: temp.store(),
            after_seq: last,
            limit: 1000,
        }));
        assert_eq!(tail["count"], 0, "nothing new after the last event");
        // An empty page must hold its position rather than rewind to zero.
        assert_eq!(tail["nextAfterSeq"], last);
    }

    #[test]
    fn runs_can_be_listed_and_filtered_by_status() {
        let temp = TempDir::new("list");
        for id in ["run-1", "run-2"] {
            let (_, code) = dispatch(&Command::Start(start_args(&temp, id, None)));
            assert_eq!(code, EXIT_OK);
        }

        let (payload, code) = dispatch(&Command::List(ListArgs {
            store: temp.store(),
            status: None,
            limit: 100,
            offset: 0,
        }));
        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["count"], 2);

        let (filtered, _) = dispatch(&Command::List(ListArgs {
            store: temp.store(),
            status: Some("succeeded".into()),
            limit: 100,
            offset: 0,
        }));
        assert_eq!(filtered["count"], 2);

        let (none, _) = dispatch(&Command::List(ListArgs {
            store: temp.store(),
            status: Some("running".into()),
            limit: 100,
            offset: 0,
        }));
        assert_eq!(none["count"], 0, "no run is still running");
    }

    #[test]
    fn an_unknown_status_filter_is_a_usage_error() {
        let temp = TempDir::new("badstatus");
        let (_, _) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));

        let (payload, code) = dispatch(&Command::List(ListArgs {
            store: temp.store(),
            status: Some("wibble".into()),
            limit: 100,
            offset: 0,
        }));

        assert_eq!(code, EXIT_USAGE);
        assert_eq!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
    }

    #[test]
    fn a_pause_request_records_intent_without_claiming_it_took_effect() {
        let temp = TempDir::new("pause");
        // A pending run: created but not executed, so intent is observable.
        let store = aad_runtime::durable::JournalStore::open(&temp.store().store)
            .expect("the store opens");
        let descriptor = durable_descriptor(&temp.workflow()).expect("compiles");
        store
            .create_run(
                "run-1",
                &descriptor.name,
                &json!({}),
                &descriptor.raw,
                None,
                Some(&aad_runtime::plan_digest(&descriptor)),
                None,
            )
            .expect("create");
        drop(store);

        let (payload, code) = dispatch(&Command::Pause(RunRefArgs {
            run_id: "run-1".into(),
            store: temp.store(),
        }));

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["desiredState"], "pause");
        // Crucially, the run is NOT claimed to be paused: it has not stopped yet.
        assert_eq!(
            payload["status"], "pending",
            "asking for a pause must not fabricate a paused status"
        );

        // Asking twice is a success, not a conflict.
        let (again, code) = dispatch(&Command::Pause(RunRefArgs {
            run_id: "run-1".into(),
            store: temp.store(),
        }));
        assert_eq!(code, EXIT_OK, "repeating a request must be idempotent");
        assert_eq!(again["desiredState"], "pause");
    }

    #[test]
    fn a_cancel_request_is_sticky_and_cannot_be_downgraded_to_a_pause() {
        let temp = TempDir::new("sticky");
        let store = aad_runtime::durable::JournalStore::open(&temp.store().store)
            .expect("the store opens");
        let descriptor = durable_descriptor(&temp.workflow()).expect("compiles");
        store
            .create_run(
                "run-1",
                &descriptor.name,
                &json!({}),
                &descriptor.raw,
                None,
                Some(&aad_runtime::plan_digest(&descriptor)),
                None,
            )
            .expect("create");
        drop(store);

        let (payload, code) = dispatch(&Command::Cancel(RunRefArgs {
            run_id: "run-1".into(),
            store: temp.store(),
        }));
        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["desiredState"], "cancel");

        // Downgrading would silently revive a run someone decided to stop.
        let (refused, code) = dispatch(&Command::Pause(RunRefArgs {
            run_id: "run-1".into(),
            store: temp.store(),
        }));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(refused["error"]["code"], "RUN.CANCEL_PENDING");
    }

    #[test]
    fn controlling_a_finished_run_is_refused_rather_than_silently_accepted() {
        let temp = TempDir::new("terminal");
        let (_, code) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));
        assert_eq!(code, EXIT_OK);

        for command in [
            Command::Pause(RunRefArgs {
                run_id: "run-1".into(),
                store: temp.store(),
            }),
            Command::Cancel(RunRefArgs {
                run_id: "run-1".into(),
                store: temp.store(),
            }),
        ] {
            let (payload, code) = dispatch(&command);
            assert_eq!(code, EXIT_FAILED);
            assert_eq!(
                payload["error"]["code"], "RUN.TERMINAL",
                "a finished run cannot accept control requests: {payload}"
            );
        }
    }

    #[test]
    fn a_cancel_requested_before_execution_stops_the_run_without_running_it() {
        let temp = TempDir::new("cancelfirst");
        let store = aad_runtime::durable::JournalStore::open(&temp.store().store)
            .expect("the store opens");
        let descriptor = durable_descriptor(&temp.workflow()).expect("compiles");
        store
            .create_run(
                "run-1",
                &descriptor.name,
                &json!({}),
                &descriptor.raw,
                None,
                Some(&aad_runtime::plan_digest(&descriptor)),
                None,
            )
            .expect("create");
        drop(store);

        let (_, code) = dispatch(&Command::Cancel(RunRefArgs {
            run_id: "run-1".into(),
            store: temp.store(),
        }));
        assert_eq!(code, EXIT_OK);

        // Resuming a run that was cancelled while down must honour the cancel.
        let (payload, code) = dispatch(&Command::Resume(ResumeArgs {
            run_id: "run-1".into(),
            file: temp.workflow(),
            store: temp.store(),
            owner_id: Some("test-runner".into()),
            lease_ttl: None,
        }));

        assert_eq!(code, EXIT_FAILED, "a cancelled run is not a success");
        assert_eq!(payload["status"], "cancelled");
        // Nothing ran: the outputs were never produced.
        assert!(
            payload["output"].is_null(),
            "a cancelled run must not report outputs: {payload}"
        );
    }

    #[test]
    fn a_paused_run_resumes_and_finishes_without_clearing_intent_by_hand() {
        // The whole point of the durable commands: `aad pause` then `aad resume`
        // must actually finish the run. An operator has no way to clear the
        // recorded pause themselves, so resume has to do it.
        let temp = TempDir::new("pauseresume");
        let store = aad_runtime::durable::JournalStore::open(&temp.store().store)
            .expect("the store opens");
        let descriptor = durable_descriptor(&temp.workflow()).expect("compiles");
        store
            .create_run(
                "run-1",
                &descriptor.name,
                &json!({}),
                &descriptor.raw,
                None,
                Some(&aad_runtime::plan_digest(&descriptor)),
                None,
            )
            .expect("create");
        drop(store);

        let (_, code) = dispatch(&Command::Pause(RunRefArgs {
            run_id: "run-1".into(),
            store: temp.store(),
        }));
        assert_eq!(code, EXIT_OK);

        // The runner honours the pause before dispatching anything, which is
        // what leaves a resumable checkpoint behind.
        let store = aad_runtime::durable::JournalStore::open(&temp.store().store)
            .expect("the store opens");
        let paused = aad_runtime::durable_exec::DurableExecutor::new(store)
            .execute(
                &descriptor,
                "run-1",
                aad_runtime::durable_exec::DurableOptions::default().with_owner_id("first"),
            )
            .expect("honour the pause");
        assert_eq!(paused.run.status, aad_runtime::durable::RunStatus::Paused);

        // Now `aad resume`, with the pause request still standing. Nothing else
        // can clear it, so resume must -- otherwise the run is stuck forever.
        let (finished, code) = dispatch(&Command::Resume(ResumeArgs {
            run_id: "run-1".into(),
            file: temp.workflow(),
            store: temp.store(),
            owner_id: Some("second".into()),
            lease_ttl: None,
        }));
        assert_eq!(code, EXIT_OK, "{finished}");
        assert_eq!(
            finished["status"], "succeeded",
            "resume must make progress, not stop again on the standing request: {finished}"
        );
        assert_eq!(finished["output"]["total"], 2);
        assert_eq!(finished["desiredState"], "run");
    }

    #[test]
    fn resuming_a_finished_run_is_refused_rather_than_run_twice() {
        let temp = TempDir::new("already");
        let (_, code) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));
        assert_eq!(code, EXIT_OK);

        let (payload, code) = dispatch(&Command::Resume(ResumeArgs {
            run_id: "run-1".into(),
            file: temp.workflow(),
            store: temp.store(),
            owner_id: None,
            lease_ttl: None,
        }));

        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["error"]["code"], "DURABLE.ALREADY_TERMINAL");
    }

    #[test]
    fn a_workflow_with_actions_is_refused_by_name_before_anything_runs() {
        let temp = TempDir::new("unsupported");
        let path = temp.path.join("actions.yaml");
        std::fs::write(
            &path,
            concat!(
                "apiVersion: ai-auto-desktop.dev/v1alpha1\n",
                "kind: Workflow\n",
                "metadata:\n  name: durable.actions\n",
                "budgets:\n  max_duration: 30s\n  max_executed_steps: 10\n",
                "steps:\n",
                "  - id: press\n    type: action\n",
                "    uses: desktop.focus@1\n    with: {target: x}\n",
            ),
        )
        .expect("write");

        let (payload, code) = dispatch(&Command::Start(StartArgs {
            file: path,
            store: temp.store(),
            inputs: None,
            run_id: Some("run-1".into()),
            owner_id: None,
            lease_ttl: None,
        }));

        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["error"]["code"], "DURABLE.UNSUPPORTED_PLAN");
        // Naming the step is what makes the refusal actionable.
        assert_eq!(payload["error"]["details"]["unsupportedSteps"], json!(["press"]));
    }

    #[test]
    fn a_resumed_run_reuses_the_persisted_inputs() {
        // `resume` deliberately takes no --inputs: the second half of a run must
        // not execute against different values than the first.
        let command = Cli::command();
        let resume = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "resume")
            .expect("the resume command");
        assert!(
            !resume.get_arguments().any(|arg| arg.get_id() == "inputs"),
            "resume must not accept --inputs"
        );
    }

    #[test]
    fn the_durable_store_flag_is_not_called_journal() {
        // `run --journal` truncates the file it is given. If the durable store
        // used the same flag name, one wrong command would destroy a run store.
        let command = Cli::command();
        for name in ["start", "resume", "status", "pause", "cancel", "list", "events"] {
            let sub = command
                .get_subcommands()
                .find(|sub| sub.get_name() == name)
                .expect("the command exists");
            let ids: Vec<String> = sub
                .get_arguments()
                .map(|arg| arg.get_id().to_string())
                .collect();
            assert!(
                ids.contains(&"store".to_string()),
                "{name} must take --store"
            );
            assert!(
                !ids.contains(&"journal".to_string()),
                "{name} must not reuse the truncating --journal flag"
            );
        }
    }

    #[test]
    fn an_invalid_lease_ttl_is_a_usage_error() {
        let temp = TempDir::new("ttl");
        for bad in [0.0, -5.0, f64::NAN] {
            let (payload, code) = dispatch(&Command::Start(StartArgs {
                file: temp.workflow(),
                store: temp.store(),
                inputs: None,
                run_id: Some("run-1".into()),
                owner_id: None,
                lease_ttl: Some(bad),
            }));
            assert_eq!(code, EXIT_USAGE, "{bad} should be rejected");
            assert_eq!(payload["error"]["code"], "CLI.INVALID_ARGUMENTS");
        }
    }

    #[test]
    fn a_run_id_cannot_be_started_twice() {
        let temp = TempDir::new("twice");
        let (_, code) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));
        assert_eq!(code, EXIT_OK);

        let (payload, code) = dispatch(&Command::Start(start_args(&temp, "run-1", None)));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["error"]["code"], "JOURNAL.CONFLICT", "{payload}");
    }
}
