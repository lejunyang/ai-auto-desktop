//! `aad` — the command line interface.
//!
//! Every command prints JSON to stdout and diagnostics to stderr, so the same
//! binary serves a person at a terminal and a program parsing its output. The
//! exit code carries the outcome: `0` success, `1` a failed run or workflow
//! error, `2` a usage or input error.

use clap::{Args, Parser, Subcommand, ValueEnum};

mod assist;
mod browser_launch;
mod browser_record;
mod cdp_watch;
mod control;
mod desk;

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
    /// List the windows and applications that can be read and driven.
    Apps(desk::AppsArgs),
    /// Describe a window's interactive elements.
    Describe(desk::OutlineArgs),
    /// Map a window's regions and their sizes, without listing their contents (Windows).
    Overview(desk::ResourceArgs),
    /// Capture a window's full element tree.
    Snapshot(desk::ResourceArgs),
    /// Find the single element matching a locator.
    Find(desk::FindArgs),
    /// Write a PNG screenshot of one element (browser pages).
    ///
    /// Compare `digest` between captures to tell whether what it shows changed.
    Capture(desk::CaptureArgs),
    /// Find one element and act on it, in one step.
    #[command(subcommand)]
    Do(desk::DoCommand),
    /// Check whether this machine can support desktop automation.
    Probe,
    /// List saved workflows, or show what one of them does.
    ///
    /// `aad record --save NAME` and `save_workflow` over MCP both store here, so
    /// this is how to see what has accumulated without knowing the paths.
    Workflows(WorkflowsArgs),
    /// Validate a workflow descriptor without running it.
    Validate(FileArgs),
    /// Run a workflow descriptor.
    Run(RunArgs),
    /// Record what someone does in a window, as steps that replay on any platform.
    Record(desk::RecordArgs),
    /// Continuously observe foreground app switches and in-app changes.
    ///
    /// Streams one JSON object per line (JSONL). Unlike `record`, it is not
    /// anchored to one window: it follows the foreground application and
    /// reports incremental snapshot diffs rather than replayable steps.
    Watch(WatchArgs),
    /// Record browser task evidence: actions, network, semantic changes and screenshots.
    #[command(subcommand)]
    BrowserRecord(browser_record::Command),
    /// Share a browser page or the desktop with a person, who can use it remotely.
    ///
    /// Prints a loopback URL with a token, then streams the screen over a
    /// WebSocket and passes the viewer's mouse and keyboard on until Ctrl+C.
    Assist(assist::AssistArgs),
    /// Serve the Model Context Protocol over stdio, for AI clients.
    Mcp(control::McpArgs),
    /// Persistent unified desktop/browser control, locally or over SSH stdio.
    #[command(subcommand)]
    Control(control::ControlCommand),
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
    #[command(flatten)]
    execution: DurableExecutionArgs,
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
    #[command(flatten)]
    execution: DurableExecutionArgs,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum DurableActions {
    #[default]
    Deny,
    ReadOnly,
}

#[derive(Args, Default)]
struct DurableExecutionArgs {
    /// Register a process plugin as NAME=COMMAND. Repeat for multiple providers.
    #[arg(long = "plugin", value_name = "NAME=COMMAND")]
    plugins: Vec<String>,
    /// Grant a permission declared by the workflow/provider.
    #[arg(long = "permission")]
    permissions: Vec<String>,
    /// Permit strictly validated top-level read-only actions in durable runs.
    #[arg(long, value_enum, default_value_t)]
    durable_actions: DurableActions,
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
struct WatchArgs {
    /// Stop after this many seconds; 0 runs until interrupted (Ctrl+C).
    #[arg(long, default_value_t = 0)]
    seconds: u64,
    /// Only observe this window, from `aad apps` (default: follow foreground).
    #[arg(long)]
    window: Option<String>,
    /// Also emit the full first snapshot of each newly focused window.
    ///
    /// Without it the first sighting of a window establishes a silent
    /// baseline and only later changes are emitted, which is the point of an
    /// incremental log.
    #[arg(long)]
    full: bool,
    /// Poll the foreground window for changes at this rate even when no
    /// events arrived, covering applications whose accessibility events are
    /// missing or partial (WinForms, some Electron, remote desktops). 0
    /// disables the fallback and trusts events alone.
    #[arg(long, default_value_t = 1.0)]
    poll_fallback_hz: f64,
    /// Maximum nodes captured per re-snapshot.
    #[arg(long, default_value_t = 1000)]
    max_nodes: u32,
    /// Write the JSONL stream to this file as UTF-8 instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Fold a burst of the same trigger within this many milliseconds into
    /// one re-snapshot.
    #[arg(long, default_value_t = 300)]
    debounce_ms: u64,
    /// Print the full structural ChangeSet instead of the filtered digest.
    ///
    /// Default is the content-first digest: anonymous structural noise is
    /// counted, only readable text and named controls are listed, and
    /// empty-value repaint churn is dropped.
    #[arg(long)]
    raw: bool,
    /// Maximum items kept per digest category.
    #[arg(long, default_value_t = 40)]
    max_items: usize,
    /// Use the Chrome DevTools Protocol for a debug-enabled Chromium browser
    /// on this port (started with --remote-debugging-port). When the
    /// foreground is a browser, rendered text and click/input are read from
    /// the page instead of the UIA tree. 0 disables CDP and uses UIA only.
    #[arg(long, default_value_t = 0)]
    cdp_port: u16,
    /// Auto-launch a debug-enabled browser if no CDP endpoint is listening,
    /// rather than silently falling back to UIA.
    ///
    /// Select profile separately using --existing-profile or --profile-dir.
    #[arg(long, value_name = "BROWSER", value_parser = ["edge", "chrome", "brave"])]
    browser: Option<String>,
    /// Copy an existing profile; omit ID only when exactly one exists.
    #[arg(long, requires = "browser", num_args = 0..=1, value_name = "ID")]
    existing_profile: Option<Option<String>>,
    #[arg(long, requires = "existing_profile")]
    profile_source: Option<PathBuf>,
    /// Working profile directory; required for retaining a detached login copy.
    #[arg(long, requires = "browser")]
    profile_dir: Option<PathBuf>,
    /// Explicitly close the browser occupying the selected user-data directory.
    #[arg(long, requires = "browser")]
    kill_browser: bool,
}

#[derive(Args)]
struct FileArgs {
    /// A workflow descriptor in YAML or JSON.
    file: PathBuf,
}

#[derive(Args)]
struct WorkflowsArgs {
    /// A workflow to describe. Without one, every saved workflow is listed.
    name: Option<String>,
}

#[derive(Args)]
struct RunArgs {
    /// A descriptor file in YAML or JSON, or the name of a saved workflow.
    ///
    /// A path is tried first, so a local file is never passed over in favour of a
    /// stored workflow of the same name.
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
    /// Register a process plugin as NAME=COMMAND. Repeat for multiple providers.
    #[arg(long = "plugin", value_name = "NAME=COMMAND")]
    plugins: Vec<String>,
    /// Grant a permission declared by the workflow/provider.
    #[arg(long = "permission")]
    permissions: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // The MCP server owns stdout for the whole session: it is a protocol
    // stream, so printing anything else onto it would desynchronise the client.
    if let Command::Mcp(args) = &cli.command {
        return match control::mcp(args) {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(error) => {
                eprintln!("aad mcp: {error}");
                ExitCode::from(EXIT_FAILED)
            }
        };
    }
    if let Command::Control(command) = &cli.command {
        return match control::run(command) {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(error) => {
                eprintln!("aad control: {error}");
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

fn dispatch(command: &Command) -> (Value, u8) {
    match command {
        Command::Probe => {
            let report = aad_probe::probe();
            // Missing desktop capabilities are the result of a successful
            // read-only probe, not a failure to run the probe itself. Callers
            // gate on `checks.*.state`; a non-zero exit is reserved for usage
            // or internal probe failures. This also makes headless CI useful.
            (report.to_json(), EXIT_OK)
        }
        Command::Tools => (aad_mcp::tools::list_payload(), EXIT_OK),
        // Handled in `main`, which hands stdout to the protocol stream.
        Command::Mcp(_) | Command::Control(_) => (json!({"status": "closed"}), EXIT_OK),
        Command::Workflows(args) => match &args.name {
            Some(name) => explain_workflow(name),
            None => list_saved_workflows(),
        },
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
        Command::Apps(args) => desk::apps(args),
        Command::Describe(args) => desk::describe(args),
        Command::Overview(args) => desk::overview(args),
        Command::Snapshot(args) => desk::snapshot(args),
        Command::Find(args) => desk::find(args),
        Command::Do(command) => desk::act(command),
        Command::Capture(args) => desk::capture(args),
        Command::Assist(args) => match assist::run(args) {
            Ok(value) => (value, EXIT_OK),
            Err(message) => (
                json!({"status": "error", "error": {"code": "ASSIST.FAILED", "message": message}}),
                EXIT_FAILED,
            ),
        },
        Command::Record(args) => desk::record(args),
        Command::Watch(args) => run_watch(args),
        Command::BrowserRecord(command) => match browser_record::run(command) {
            Ok(value) => (value, EXIT_OK),
            Err(error) => (
                failure("BROWSER.RECORDING_FAILED", &error.to_string(), None),
                EXIT_FAILED,
            ),
        },
    }
}

/// Stream ambient observations as JSONL: one JSON object per line.
///
/// The loop is "events wake, snapshots tell the truth": native events are
/// folded into per-window triggers, and each trigger causes one fresh
/// foreground snapshot which is diffed against that window's previous
/// snapshot. Only the incremental ChangeSet is emitted. A low-frequency poll
/// covers applications whose events do not arrive.
fn run_watch(args: &WatchArgs) -> (Value, u8) {
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

    // JSONL goes to a file or stdout. Every line is a complete, self-contained
    // JSON object; diagnostics still go to stderr so the stream stays parseable.
    let mut sink: Box<dyn std::io::Write> = match &args.out {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => Box::new(std::io::BufWriter::new(file)),
            Err(error) => {
                return (
                    failure("AAD.CANNOT_OPEN_OUTPUT", &error.to_string(), None),
                    EXIT_FAILED,
                )
            }
        },
        None => Box::new(std::io::stdout().lock()),
    };
    let emit = |sink: &mut Box<dyn std::io::Write>, mut object: Value| {
        object["emitted_at"] = json!(aad_runtime::journal::now_rfc3339());
        // Compact on purpose: one object per line is the JSONL contract.
        let line = serde_json::to_string(&object).unwrap_or_else(|_| "{}".to_string());
        let _ = writeln!(sink, "{line}");
        let _ = sink.flush();
    };

    let started = match driver.call("observe_start", &json!({})) {
        Ok(value) => value,
        Err(error) => return (driver_failure(&error), EXIT_FAILED),
    };
    let observe_id = started["observe_id"]
        .as_str()
        .expect("observe_start returns an observe id")
        .to_string();
    emit(
        &mut sink,
        json!({"kind": "session_started", "sources": started["sources"], "observe_id": observe_id}),
    );

    // Per-window baseline snapshots, keyed by the backend's window id. A
    // window revisited after being closed is a new baseline either way, which
    // re-keying by the current window id naturally expresses.
    let mut baselines: std::collections::HashMap<String, aad_uia::Snapshot> =
        std::collections::HashMap::new();

    let started_at = std::time::Instant::now();
    let deadline = if args.seconds == 0 {
        None
    } else {
        Some(started_at + std::time::Duration::from_secs(args.seconds))
    };
    let quiet = std::time::Duration::from_millis(args.debounce_ms.max(1));
    let fallback_interval = if args.poll_fallback_hz > 0.0 {
        Some(std::time::Duration::from_secs_f64(
            1.0 / args.poll_fallback_hz,
        ))
    } else {
        None
    };
    let tick = std::time::Duration::from_millis(100);
    let mut last_fallback = std::time::Instant::now();
    let mut total_dropped: u64 = 0;

    // Resolve the effective CDP port and an optional auto-launched browser.
    let cdp_launch = match args.browser.as_deref().map(browser_launch::parse) {
        Some(Some(mut launch)) => {
            launch.existing_profile = args.existing_profile.clone();
            launch.profile_source = args.profile_source.clone();
            launch.profile_dir = args.profile_dir.clone();
            launch.kill_browser = args.kill_browser;
            Some(launch)
        }
        Some(None) => {
            return (
                failure(
                    "CLI.INVALID_ARGUMENTS",
                    "--browser expects edge, chrome or brave; use --existing-profile to choose a profile",
                    None,
                ),
                EXIT_USAGE,
            );
        }
        None => None,
    };
    let cdp_port = match (args.cdp_port, &cdp_launch) {
        (0, Some(_)) => 9222,
        (port, _) => port,
    };
    if let Some(launch) = &cdp_launch {
        if !browser_launch::ensure_browser(cdp_port, launch) {
            emit(
                &mut sink,
                json!({"kind": "cdp_unavailable", "browser": launch.browser, "port": cdp_port}),
            );
        } else {
            emit(
                &mut sink,
                json!({"kind": "cdp_ready", "browser": launch.browser, "port": cdp_port,
                       "login_copy": launch.existing_profile.is_some()}),
            );
        }
    }

    // Optional CDP source for Chromium browsers. Constructed once; each poll
    // decides whether the foreground is actually a browser on the port.
    let mut cdp = if cdp_port != 0 {
        Some(cdp_watch::CdpWatcher::new())
    } else {
        None
    };

    let finish = |sink: &mut Box<dyn std::io::Write>,
                  driver: &aad_uia::UiaDriver,
                  observe_id: &str,
                  total_dropped: u64,
                  stopped_early: bool| {
        let _ = driver.call("observe_stop", &json!({"observe_id": observe_id}));
        let mut summary = json!({
            "kind": "session_ended",
            "dropped_events": total_dropped,
        });
        summary["stopped_early"] = json!(stopped_early);
        emit(sink, summary);
    };

    loop {
        if let Some(deadline) = deadline {
            if std::time::Instant::now() >= deadline {
                finish(&mut sink, &driver, &observe_id, total_dropped, false);
                return (
                    json!({"watched": true, "dropped_events": total_dropped}),
                    EXIT_OK,
                );
            }
        }
        std::thread::sleep(tick);

        let polled = match driver.call("observe_poll", &json!({"observe_id": observe_id})) {
            Ok(value) => value,
            // The session vanished (e.g. driver error): report and stop rather
            // than spinning on a dead subscription.
            Err(error) => {
                finish(&mut sink, &driver, &observe_id, total_dropped, true);
                return (driver_failure(&error), EXIT_FAILED);
            }
        };
        total_dropped += polled["dropped_events"].as_u64().unwrap_or(0);
        let events_json = polled["events"].as_array().cloned().unwrap_or_default();

        // CDP takes the foreground when enabled and the current window is a
        // Chromium browser on the debug port. It reads rendered text and
        // in-page activity directly; UIA still supplies app switches.
        if let Some(watcher) = cdp.as_mut() {
            if args.window.is_none() {
                if let Some(id) = current_foreground_id(&driver) {
                    let Some(window) = window_listing(&driver, &id) else {
                        std::thread::sleep(tick);
                        continue;
                    };
                    let process = window
                        .process_name
                        .clone()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let is_chromium = matches!(
                        process.as_str(),
                        "msedge.exe" | "chrome.exe" | "brave.exe" | "vivaldi.exe"
                    );
                    // Restrict to the launched browser when one was requested,
                    // so another Chromium app does not get CDP-read by mistake.
                    let browser_matches = cdp_launch
                        .as_ref()
                        .map(|launch| browser_launch::matches_process(launch, &process))
                        .unwrap_or(true);
                    if is_chromium && browser_matches && cdp_watch::CdpWatcher::available(cdp_port)
                    {
                        if let Some(observation) = watcher.observe(cdp_port, &window.title) {
                            let any = !observation.activity.is_empty()
                                || !observation.added_lines.is_empty()
                                || !observation.removed_lines.is_empty()
                                || observation.page_changed
                                || observation.dropped_events > 0;
                            if any {
                                emit(
                                    &mut sink,
                                    json!({
                                        "kind": "page_activity",
                                        "source": "cdp",
                                        "title": observation.title,
                                        "url": observation.url,
                                        "activity": serde_json::to_value(&observation.activity).unwrap_or(Value::Null),
                                        "added_text": observation.added_lines,
                                        "removed_text": observation.removed_lines,
                                        "dropped_events": observation.dropped_events,
                                        "truncated": observation.truncated,
                                    }),
                                );
                            }
                            // CDP already described this window's content; skip
                            // the UIA snapshot for it this tick.
                            continue;
                        }
                    }
                }
            }
        }
        let mut events: Vec<aad_uia::ObservedEvent> = events_json
            .iter()
            .filter_map(|value| serde_json::from_value(value.clone()).ok())
            .collect();

        // Fallback: even with no events, periodically re-look at the foreground
        // window so applications with a missing event surface cannot hide a
        // change forever.
        let fallback_due =
            fallback_interval.is_some_and(|interval| last_fallback.elapsed() >= interval);
        if fallback_due {
            last_fallback = std::time::Instant::now();
            events.push(aad_uia::ObservedEvent {
                sequence: u64::MAX,
                kind: aad_uia::ObservationKind::Other,
                source: "poll_fallback".to_string(),
                window: aad_uia::EventWindow::unknown(),
                element: None,
                observed_at: std::time::Instant::now(),
            });
        }

        if events.is_empty() {
            continue;
        }

        // Fold the raw burst into a few (window, kind) re-snapshot triggers.
        let triggers = aad_uia::fold_triggers(&events, quiet);

        // Resolve the current foreground window once for this batch. UIA
        // events only carry a pid, and a foreground switch that is still
        // settling can outrun a title read: attributing the whole batch to one
        // window snapshot is both correct and cheap.
        let foreground_id = if args.window.is_some() {
            args.window.clone()
        } else {
            current_foreground_id(&driver)
        };
        let foreground_listing = foreground_id
            .as_ref()
            .and_then(|id| window_listing(&driver, id));

        let mut targets: Vec<Option<String>> = Vec::new();
        let mut switches: Vec<&aad_uia::ObservedEvent> = Vec::new();
        for trigger in &triggers {
            if trigger.kind == aad_uia::ObservationKind::ForegroundChanged {
                if let Some(event) = events
                    .iter()
                    .find(|event| event.kind == aad_uia::ObservationKind::ForegroundChanged)
                {
                    switches.push(event);
                }
                targets.push(trigger.window.window_id.clone());
            } else {
                // pid-only UIA events belong to the foreground window when the
                // process matches; attributed WinEvent events name their own
                // window. Internal popups/renderers are excluded below.
                let candidate = match &trigger.window.window_id {
                    Some(id) => Some(id.clone()),
                    None => foreground_listing
                        .as_ref()
                        .map(|info| info.window_id.clone()),
                };
                if let Some(id) = candidate {
                    let placeable = match window_listing(&driver, &id) {
                        // Snapshot real, titled, visible top-level windows only.
                        // WinEvent/UIA attribute an internal renderer or popup
                        // ("Chrome Legacy Window", a 1-node hidden helper);
                        // walking it produces noise and, worse, evicts the real
                        // window's baseline.
                        Some(info) => {
                            info.process_id == trigger.window.process_id
                                && !info.title.is_empty()
                                && !info.is_minimized
                        }
                        None => false,
                    };
                    if placeable {
                        targets.push(Some(id));
                    }
                }
            }
        }
        // A pure fallback tick (folded away as nothing new) still re-looks at
        // the foreground once.
        if triggers.is_empty() && fallback_due {
            targets.push(foreground_id.clone());
        }

        for switch in switches {
            emit(
                &mut sink,
                json!({
                    "kind": "foreground_changed",
                    "source": switch.source,
                    "window": serde_json::to_value(&switch.window).unwrap_or(Value::Null),
                }),
            );
        }

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for target in targets {
            let Some(window_id) = target else { continue };
            if !seen.insert(window_id.clone()) {
                continue;
            }
            if let Some(only) = &args.window {
                if only != &window_id {
                    continue;
                }
            }
            if let Err(error) =
                resnapshot_and_diff(&driver, &mut sink, &mut baselines, &window_id, args, &emit)
            {
                // A window closing mid-switch is normal: skip it this round,
                // keep watching. Persistent errors still surface on stderr.
                eprintln!("watch: snapshot of {window_id} skipped: {}", error.message);
            }
        }
    }
}

/// Resolve one window id to its current listing, or None when it is gone.
fn window_listing(driver: &aad_uia::UiaDriver, window_id: &str) -> Option<aad_uia::WindowInfo> {
    let listing = driver.call("list_windows", &json!({})).ok()?;
    listing["windows"]
        .as_array()?
        .iter()
        .filter_map(|value| serde_json::from_value::<aad_uia::WindowInfo>(value.clone()).ok())
        .find(|window| window.window_id == window_id)
}

/// The foreground window's id, or None when it cannot be determined.
fn current_foreground_id(driver: &aad_uia::UiaDriver) -> Option<String> {
    let listing = driver.call("list_windows", &json!({})).ok()?;
    listing["windows"]
        .as_array()?
        .iter()
        .find(|window| window["is_foreground"] == json!(true))
        .and_then(|window| window["window_id"].as_str())
        .map(str::to_string)
}

/// Capture one window, diff against its stored baseline, emit the ChangeSet,
/// and roll the baseline forward. The first capture of a window becomes a
/// baseline (and, with --full, an emitted snapshot).
fn resnapshot_and_diff(
    driver: &aad_uia::UiaDriver,
    sink: &mut Box<dyn std::io::Write>,
    baselines: &mut std::collections::HashMap<String, aad_uia::Snapshot>,
    window_id: &str,
    args: &WatchArgs,
    emit: &impl Fn(&mut Box<dyn std::io::Write>, Value),
) -> Result<(), aad_uia::DriverError> {
    let captured = driver.call(
        "snapshot",
        &json!({"window_id": window_id, "max_nodes": args.max_nodes}),
    )?;
    let snapshot: aad_uia::Snapshot = serde_json::from_value(captured.clone())
        .map_err(|error| aad_uia::DriverError::invalid(format!("bad snapshot json: {error}")))?;

    match baselines.get(window_id) {
        None => {
            if args.full {
                emit(
                    sink,
                    json!({
                        "kind": "window_baseline",
                        "window": captured["window"].clone(),
                        "revision": snapshot.revision,
                        "node_count": snapshot.nodes.len(),
                        "snapshot": captured,
                    }),
                );
            }
            baselines.insert(window_id.to_string(), snapshot);
        }
        Some(previous) => {
            let changes = aad_uia::diff_trees(previous, &snapshot);
            if changes.is_empty() {
                baselines.insert(window_id.to_string(), snapshot);
                return Ok(());
            }
            if args.raw {
                let (added, removed, changed) = changes.counts();
                emit(
                    sink,
                    json!({
                        "kind": "window_changes",
                        "window": captured["window"].clone(),
                        "baseline_revision": previous.revision,
                        "revision": snapshot.revision,
                        "added": serde_json::to_value(&changes.added).unwrap_or(Value::Null),
                        "removed": serde_json::to_value(&changes.removed).unwrap_or(Value::Null),
                        "changed": serde_json::to_value(&changes.changed).unwrap_or(Value::Null),
                        "added_count": added,
                        "removed_count": removed,
                        "changed_count": changed,
                        "truncated": snapshot.truncated,
                    }),
                );
            } else {
                let digest = aad_uia::digest(
                    &changes,
                    &aad_uia::DigestOptions {
                        max_items: args.max_items,
                        include_states: true,
                    },
                );
                // A digest with no content is pure structural churn: skip it so
                // an idle Electron workbench stays silent.
                if !digest.is_empty() {
                    emit(
                        sink,
                        json!({
                            "kind": "window_activity",
                            "window": captured["window"].clone(),
                            "baseline_revision": previous.revision,
                            "revision": snapshot.revision,
                            "appeared": serde_json::to_value(&digest.appeared).unwrap_or(Value::Null),
                            "disappeared": serde_json::to_value(&digest.disappeared).unwrap_or(Value::Null),
                            "text_changes": serde_json::to_value(&digest.text_changes).unwrap_or(Value::Null),
                            "state_changes": serde_json::to_value(&digest.state_changes).unwrap_or(Value::Null),
                            "structural_noise": digest.structural_noise,
                            "truncated": digest.truncated || snapshot.truncated,
                        }),
                    );
                }
            }
            baselines.insert(window_id.to_string(), snapshot);
        }
    }
    Ok(())
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
    requested_ceiling: Option<usize>,
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
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
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
        "stopped_by": answer["stopped_by"].clone(),
        "outline_truncated": answer["outline_truncated"].clone(),
    });
    if let Some(region) = answer.get("region") {
        summary["region"] = region.clone();
    }
    if requested_ceiling.is_some() {
        summary["note"] = json!(
            "--max-characters was ignored: a file gets the largest limits the session allows"
        );
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

/// Where a descriptor named on the command line actually lives.
///
/// A path is tried first and a saved name second, never the other way round: a
/// local file whose name happens to match a saved workflow must not be quietly
/// passed over in favour of the stored one. A path that does not exist fails
/// loudly; a path silently overruled by a name would not.
///
/// This exists because `aad record --save NAME` stores under a name while `aad
/// run` took only a path, so the two halves of the same session referred to a
/// workflow differently.
fn locate_descriptor(given: &Path) -> PathBuf {
    if given.exists() {
        return given.to_path_buf();
    }
    // Only a bare name can be one: anything with a separator or an extension was
    // meant as a path, and treating it as a name would answer a mistyped path
    // with a confusing "no such workflow".
    let bare = given
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
        && given.extension().is_none();
    if !bare {
        return given.to_path_buf();
    }
    let Some(name) = given.to_str() else {
        return given.to_path_buf();
    };
    match aad_runtime::recordings::workflow_path(name) {
        Ok(stored) if stored.exists() => stored,
        // Fall back to the original so the error names what was asked for rather
        // than a store path the caller never mentioned.
        _ => given.to_path_buf(),
    }
}

/// Read a descriptor from YAML or JSON.
fn read_descriptor(path: &Path) -> Result<Value, Value> {
    let path = &locate_descriptor(path);
    let text = std::fs::read_to_string(path).map_err(|error| {
        failure(
            "CLI.FILE_UNREADABLE",
            &format!("{}: {error}", path.display()),
            // Say where saved workflows live, since a bare name that did not
            // resolve is most likely a workflow the caller thought was saved.
            Some(json!({
                "hint": "If you meant a saved workflow, `aad workflows` lists the \
            ones that exist."
            })),
        )
    })?;

    // Windows editors routinely save a UTF-8 byte order mark, which no JSON or
    // YAML parser accepts. Refusing such a file would be a baffling failure.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    // YAML is a superset of JSON, so one parser handles both; but JSON gets
    // first refusal so that duplicate-key rejection still applies.
    let document = if let Ok(value) = serde_json::from_str::<Value>(text) {
        value
    } else {
        serde_yaml::from_str::<Value>(text).map_err(|error| {
            failure(
                "CLI.FILE_INVALID",
                &format!("{} is not valid JSON or YAML: {error}", path.display()),
                None,
            )
        })?
    };

    // Checked here rather than at each of the three places that compile a
    // descriptor: missing one of them silently restores the old behaviour, which
    // is exactly what happened when this lived in `validate` alone.
    if let Some(problem) = mistaken_for_a_workflow(&document) {
        return Err(problem);
    }
    Ok(document)
}

/// Every saved workflow, newest first.
fn list_saved_workflows() -> (Value, u8) {
    let directory = aad_runtime::recordings::recordings_dir();
    match aad_runtime::recordings::list_workflows() {
        Ok(saved) => {
            // The store already reports the path and the modification time, so
            // the listing does not go back to the filesystem for either.
            // Whether each one would run is worth the 2ms it costs to compile:
            // a listing is what someone reads before picking one, and a workflow
            // that cannot run looks exactly like one that can until it is tried.
            // Only the verdict and a count go here -- `aad validate` and
            // `describe_workflow` report the issues themselves, and ten broken
            // files would otherwise bury the listing.
            let workflows: Vec<Value> = saved
                .into_iter()
                .map(|entry| {
                    let mut row = json!({
                        "name": entry.name,
                        "modified": entry.modified,
                        "path": entry.path,
                    });
                    let path = Path::new(&entry.path);
                    match read_descriptor(path) {
                        Ok(document) => {
                            let directory = path.parent().map(Path::to_path_buf);
                            match aad_core::compiler::compile_descriptor(document, directory) {
                                Ok(_) => {
                                    row["runnable"] = json!(true);
                                }
                                Err(error) => {
                                    row["runnable"] = json!(false);
                                    row["issue_count"] = json!(error.issues.len());
                                }
                            }
                        }
                        // A file that cannot be read is a different problem from
                        // one whose contents are wrong, and saying which saves
                        // looking in the wrong place. `read_descriptor` already
                        // returns the user-facing failure payload, so it is
                        // carried through rather than rebuilt.
                        Err(failure) => {
                            row["runnable"] = json!(false);
                            row["unreadable"] = failure;
                        }
                    }
                    row
                })
                .collect();
            (
                json!({
                    "kind": "WorkflowList",
                    "count": workflows.len(),
                    "workflows": workflows,
                    "directory": directory.to_string_lossy(),
                }),
                EXIT_OK,
            )
        }
        Err(error) => (failure(error.code, &error.message, None), EXIT_FAILED),
    }
}

/// Fold generated snapshot/find helpers back into one entry per recorded action.
fn logical_actions(steps: &[Value]) -> Vec<Value> {
    let mut actions = Vec::new();
    for step in steps {
        let id = step.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.ends_with("_window") || id.ends_with("_element") || id.ends_with("_destination") {
            continue;
        }
        let with = step.get("with").cloned().unwrap_or(Value::Null);
        let uses = step.get("uses").and_then(Value::as_str).unwrap_or_default();
        let action = uses
            .rsplit_once('.')
            .map(|(_, tail)| tail.split('@').next().unwrap_or(tail))
            .unwrap_or(uses);
        let borrowed = |suffix: &str, key: &str| -> Option<Value> {
            let wanted = format!("{id}{suffix}");
            steps
                .iter()
                .find(|other| other.get("id").and_then(Value::as_str) == Some(wanted.as_str()))
                .and_then(|other| other.get("with"))
                .and_then(|with| with.get(key))
                .cloned()
        };

        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), json!(id));
        entry.insert("action".into(), json!(action));
        if let Some(window) = borrowed("_window", "window") {
            entry.insert("window".into(), window);
        }
        if let Some(locator) = borrowed("_element", "locator") {
            entry.insert("finds".into(), locator);
        }
        if let Some(locator) = borrowed("_destination", "locator") {
            entry.insert("destination".into(), locator);
        }
        if let Some(with) = with.as_object() {
            let parameters: serde_json::Map<String, Value> = with
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "target" | "to" | "value" | "text"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if !parameters.is_empty() {
                entry.insert("parameters".into(), Value::Object(parameters));
            }
        }
        for key in ["value", "text"] {
            if let Some(argument) = with.get(key).and_then(Value::as_str) {
                if argument.contains("inputs.") {
                    entry.insert("needs_input".into(), json!(argument));
                } else {
                    entry.insert("writes".into(), json!(argument));
                }
            }
        }
        actions.push(Value::Object(entry));
    }
    actions
}
/// What a saved workflow would do, before running it.
///
/// Most saved actions expand to snapshot/find/act; drag expands to
/// snapshot/source-find/destination-find/act. Those helper steps are folded back
/// together here, and the window, locators, arguments, and durable parameters are
/// named. `describe_workflow` over MCP follows the same logical-action view.
fn explain_workflow(name: &str) -> (Value, u8) {
    let path = match aad_runtime::recordings::workflow_path(name) {
        Ok(path) => path,
        Err(error) => return (failure(error.code, &error.message, None), EXIT_USAGE),
    };
    let document = match read_descriptor(&path) {
        Ok(document) => document,
        Err(payload) => {
            // Say which workflows exist: a mistyped name is the likeliest reason
            // to be here, and the list is one command away.
            let mut payload = payload;
            payload["error"]["hint"] = json!("`aad workflows` lists the ones that are saved.");
            return (payload, EXIT_USAGE);
        }
    };

    // Compiled as well as read, so a workflow that would not run says so here
    // rather than at the moment someone relies on it.
    let compiled =
        aad_core::compiler::compile_descriptor(document.clone(), path.parent().map(Into::into));
    let problem = compiled.as_ref().err().map(|error| {
        // I dropped these when this was written, which left the report saying only
        // "invalid" -- an error nobody can act on. `validate` had them all along.
        let issues: Vec<Value> = error
            .issues
            .iter()
            .map(|issue| json!({"path": issue.path, "message": issue.message}))
            .collect();
        json!({
            "code": error.code,
            "message": error.message,
            "issue_count": issues.len(),
            "issues": issues,
        })
    });

    let steps = document
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let actions = logical_actions(&steps);
    let inputs = document
        .get("inputs")
        .and_then(Value::as_object)
        .map(|inputs| {
            inputs
                .iter()
                .map(|(key, spec)| {
                    json!({
                        "name": key,
                        "required": spec.get("required").cloned().unwrap_or(json!(false)),
                        "sensitive": spec.get("sensitive").cloned().unwrap_or(json!(false)),
                    })
                })
                .collect::<Vec<Value>>()
        })
        .unwrap_or_default();

    let mut answer = json!({
        "kind": "WorkflowExplanation",
        "name": name,
        "path": path.to_string_lossy(),
        "actions": actions,
        "executed_steps": steps.len(),
        "inputs": inputs,
    });
    if let Some(problem) = problem {
        // Reported alongside rather than instead of: seeing what a broken
        // workflow meant to do is how it gets repaired.
        answer["will_not_run"] = problem;
        return (answer, EXIT_FAILED);
    }
    (answer, EXIT_OK)
}

/// Say so when the document is a recording rather than a workflow.
///
/// Measured: compiling a recording yields thirteen issues -- `budgets` missing,
/// `kind` wrong, every step's `type` absent -- not one of which says what is
/// actually wrong. Being told to add budgets to a file that was never meant to
/// have them sends the reader off repairing the wrong thing.
///
/// The two live in the same folder and differ by one field, so mixing them up is
/// easy; the compiler's answer to one is a list of everything the other kind has.
fn mistaken_for_a_workflow(document: &Value) -> Option<Value> {
    // Only the other kind this project actually writes. A `kind` of anything else
    // is a mistake in the file rather than the wrong file, and answering that with
    // "you picked the wrong document" would hide the several real problems the
    // compiler is about to name.
    let kind = document.get("kind").and_then(Value::as_str)?;
    if kind != aad_runtime::recordings::RECORDING_KIND {
        return None;
    }
    Some(failure(
        "CLI.NOT_A_WORKFLOW",
        &format!("this is a {kind}, not a workflow"),
        Some(json!({
            "kind": kind,
            "hint": "A recording is the editable form. Open it in the desktop app, \
        or run the workflow saved beside it -- `aad workflows` lists those.",
        })),
    ))
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
                    failure(
                        "CLI.INVALID_ARGUMENTS",
                        "--inputs must be a JSON object",
                        None,
                    ),
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

    let artifacts = std::sync::Arc::new(aad_runtime::ArtifactStore::default());
    let mut providers = match provider_registry(&args.plugins) {
        Ok(providers) => providers,
        Err(error) => return (automation_failure(&error), EXIT_USAGE),
    };
    if !providers.names().contains(&aad_ocr::PROVIDER_NAME) {
        if let Err(error) = aad_ocr::register(&mut providers) {
            return (automation_failure(&error), EXIT_USAGE);
        }
    }
    // The native providers come from one control session, which also runs
    // saved control steps, so both kinds of workflow drive the same driver.
    // Input is still gated by the workflow's declared permissions.
    let session = std::sync::Arc::new(aad_control::Session::native(aad_control::Policy {
        allow_input: true,
        ..Default::default()
    }));
    for provider in session.providers() {
        if !providers
            .names()
            .contains(&provider.manifest().name.as_str())
        {
            providers.insert(provider.clone());
        }
    }
    if !providers.names().contains(&aad_control::steps::PROVIDER) {
        providers.insert(std::sync::Arc::new(aad_control::steps::StepProvider::new(
            session,
        )));
    }

    let mut options = aad_runtime::RunOptions::default()
        .with_providers(providers)
        .with_inputs(inputs)
        .with_artifacts(artifacts)
        .with_scripts_allowed(args.allow_scripts)
        .with_granted_permissions(args.permissions.clone());

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
    execution: &DurableExecutionArgs,
) -> Result<aad_runtime::durable_exec::DurableOptions, (Value, u8)> {
    let mut options = aad_runtime::durable_exec::DurableOptions::default();
    if let Some(text) = inputs {
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => options = options.with_inputs(map),
            Ok(_) => {
                return Err((
                    failure(
                        "CLI.INVALID_ARGUMENTS",
                        "--inputs must be a JSON object",
                        None,
                    ),
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
    let mut providers = provider_registry(&execution.plugins)
        .map_err(|error| (automation_failure(&error), EXIT_USAGE))?;
    if !providers.names().contains(&aad_ocr::PROVIDER_NAME) {
        aad_ocr::register(&mut providers)
            .map_err(|error| (automation_failure(&error), EXIT_USAGE))?;
    }
    #[cfg(target_os = "linux")]
    if !providers.names().contains(&aad_atspi::PROVIDER_NAME) {
        if let Ok(provider) = aad_atspi::native_provider() {
            providers.insert(std::sync::Arc::new(provider));
        }
    }
    #[cfg(target_os = "macos")]
    if !providers.names().contains(&aad_macos_ax::PROVIDER_NAME) {
        if let Ok(provider) = aad_macos_ax::native_provider() {
            providers.insert(std::sync::Arc::new(provider));
        }
    }
    if let Ok(driver) = aad_uia::native_driver() {
        providers.insert(std::sync::Arc::new(driver));
    }
    let mode = match execution.durable_actions {
        DurableActions::Deny => aad_runtime::durable_exec::DurableActionMode::Deny,
        DurableActions::ReadOnly => aad_runtime::durable_exec::DurableActionMode::ReadOnly,
    };
    options = options
        .with_providers(providers)
        .with_granted_permissions(execution.permissions.clone())
        .with_action_mode(mode);
    Ok(options)
}

fn provider_registry(
    values: &[String],
) -> Result<aad_runtime::ProviderRegistry, aad_core::AutomationError> {
    let mut providers = aad_runtime::ProviderRegistry::new();
    let mut names = std::collections::BTreeSet::new();
    for raw in values {
        let Some((name, command)) = raw.split_once('=') else {
            return Err(aad_core::AutomationError::new(
                "CLI.INVALID_ARGUMENT",
                "--plugin must use NAME=COMMAND",
            )
            .with_category("cli")
            .with_effect("not_applied"));
        };
        if name.is_empty() {
            return Err(aad_core::AutomationError::new(
                "CLI.INVALID_ARGUMENT",
                "--plugin name is empty",
            )
            .with_category("cli")
            .with_effect("not_applied"));
        }
        if !names.insert(name.to_string()) {
            return Err(aad_core::AutomationError::new(
                "CLI.INVALID_ARGUMENT",
                format!("plugin {name:?} is repeated"),
            )
            .with_category("cli")
            .with_effect("not_applied"));
        }
        let argv = split_plugin_command(command).ok_or_else(|| {
            aad_core::AutomationError::new(
                "CLI.INVALID_ARGUMENT",
                format!("plugin {name:?} command has invalid quoting"),
            )
            .with_category("cli")
            .with_effect("not_applied")
        })?;
        if argv.is_empty() {
            return Err(aad_core::AutomationError::new(
                "CLI.INVALID_ARGUMENT",
                format!("plugin {name:?} command is empty"),
            )
            .with_category("cli")
            .with_effect("not_applied"));
        }
        aad_runtime::register_process_plugin(&mut providers, name, argv)?;
    }
    Ok(providers)
}

/// Split a plugin command without treating Windows path separators as escapes.
///
/// The CLI accepts one command string as its stable plugin interface. On
/// Windows, doubling backslashes before POSIX-style tokenisation preserves both
/// ordinary drive paths and UNC paths while retaining useful quote validation.
fn split_plugin_command(command: &str) -> Option<Vec<String>> {
    #[cfg(windows)]
    let command = command.replace('\\', "\\\\");
    #[cfg(windows)]
    let command = command.as_str();
    shlex::split(command)
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
        &args.execution,
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
    let options = match durable_options(
        None,
        args.owner_id.as_ref(),
        args.lease_ttl,
        &args.execution,
    ) {
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
        let next = events
            .last()
            .map(|event| event.seq)
            .unwrap_or(args.after_seq);
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
    fn a_workflow_that_will_not_run_says_where_the_problem_is() {
        // I wrote this report without the issues, which left it saying only
        // "invalid" -- an error nobody can act on, while `validate` had the paths
        // all along. Measured: a bad name and a bad step give three located
        // issues, all of which were being dropped.
        let folder = std::env::temp_dir().join("aad_issue_report");
        let _ = std::fs::create_dir_all(&folder);
        let path = folder.join("broken.json");
        std::fs::write(
            &path,
            r#"{"apiVersion":"ai-auto-desktop.dev/v1alpha1","kind":"Workflow",
               "metadata":{"name":"BadName"},
               "budgets":{"max_duration":"1m","max_executed_steps":1},
               "steps":[{"id":"done","type":"return","value":1}]}"#,
        )
        .expect("write");

        let (payload, code) = validate(&path);
        assert_eq!(code, EXIT_FAILED);
        let issues = payload["error"]["issues"]
            .as_array()
            .expect("issues are reported");
        assert!(
            !issues.is_empty(),
            "an error without a location cannot be fixed"
        );
        assert!(
            issues.iter().any(|issue| issue["path"]
                .as_str()
                .is_some_and(|path| path.contains("name"))),
            "the offending path is named: {issues:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_recording_is_not_reported_as_a_broken_workflow() {
        // The two kinds live in the same folder and differ by one field. Compiling
        // a recording lists everything a workflow has that it does not -- thirteen
        // issues, measured, telling the reader to add budgets to a file that was
        // never meant to have them.
        let folder = std::env::temp_dir().join("aad_kind_check");
        let _ = std::fs::create_dir_all(&folder);
        let path = folder.join("some.recording.json");
        std::fs::write(
            &path,
            r#"{"apiVersion":"ai-auto-desktop.dev/v1alpha1","kind":"Recording",
               "metadata":{"name":"SubmitButton"},"steps":[]}"#,
        )
        .expect("write");

        let problem = read_descriptor(&path).expect_err("a recording cannot be run");
        assert_eq!(problem["error"]["code"], "CLI.NOT_A_WORKFLOW");
        assert_eq!(problem["error"]["kind"], "Recording");
        let whole = problem.to_string();
        assert!(
            !whole.contains("budgets"),
            "the report should not send the reader after fields this kind never has: {whole}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_way_in_agrees_a_recording_is_not_runnable() {
        // Checked at the shared entry point rather than at each of the three
        // places that compile a descriptor: putting it in `validate` alone left
        // `run` reporting the old thirteen issues, which is how this was found.
        let folder = std::env::temp_dir().join("aad_kind_check_all");
        let _ = std::fs::create_dir_all(&folder);
        let path = folder.join("shared.recording.json");
        std::fs::write(
            &path,
            r#"{"apiVersion":"ai-auto-desktop.dev/v1alpha1","kind":"Recording",
               "metadata":{"name":"x"},"steps":[]}"#,
        )
        .expect("write");

        let (from_validate, validate_code) = validate(&path);
        assert_eq!(validate_code, EXIT_USAGE);
        assert_eq!(from_validate["error"]["code"], "CLI.NOT_A_WORKFLOW");

        let (from_explain, _) = super::run_workflow(&RunArgs {
            file: path.clone(),
            inputs: None,
            journal: None,
            dry_run: true,
            allow_scripts: false,
            plugins: Vec::new(),
            permissions: Vec::new(),
        });
        assert_eq!(
            from_explain["error"]["code"], "CLI.NOT_A_WORKFLOW",
            "running one has to say the same thing validating one does"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_path_is_never_overruled_by_a_saved_name() {
        // `--save NAME` stores under a name, so `run NAME` has to work or the two
        // halves of one session name a workflow differently. But a local file must
        // win: a path that does not exist fails loudly, while a path silently
        // replaced by a stored workflow does not, and the caller would act on a
        // different workflow than the one they pointed at.
        let folder = std::env::temp_dir().join("aad_locate_test");
        let _ = std::fs::create_dir_all(&folder);
        let local = folder.join("some_name");
        std::fs::write(&local, "{}").expect("write");

        assert_eq!(
            locate_descriptor(&local),
            local,
            "an existing file is used as given"
        );
        let _ = std::fs::remove_file(&local);
    }

    #[test]
    fn something_that_reads_as_a_path_stays_a_path() {
        // Answering a mistyped path with "no such workflow" would send the reader
        // looking in the wrong place.
        let with_extension = Path::new("missing.json");
        assert_eq!(locate_descriptor(with_extension), with_extension);

        let with_directory = Path::new("some/where/missing");
        assert_eq!(locate_descriptor(with_directory), with_directory);
    }

    #[test]
    fn an_unresolved_bare_name_is_reported_as_asked_for() {
        // Falling back to the store path would name a location the caller never
        // mentioned; falling back to what they typed keeps the error legible.
        let name = Path::new("definitely_not_saved_anywhere_12345");
        let located = locate_descriptor(name);
        assert!(
            located == name || located.exists(),
            "either what was asked for, or something that is really there: {located:?}"
        );
    }

    #[test]
    fn workflow_explanation_folds_drag_helpers_and_keeps_parameters() {
        let steps = vec![
            aad_runtime::assemble::PerformedStep {
                action: "key_chord".into(),
                locator: json!({"automation_id": "1001"}),
                window: json!({"process_name": "fixture.exe"}),
                argument: None,
                parameters: serde_json::from_value(json!({"keys": ["ctrl", "a"]})).unwrap(),
                destination: None,
                protected: false,
            },
            aad_runtime::assemble::PerformedStep {
                action: "drag".into(),
                locator: json!({"automation_id": "1002"}),
                window: json!({"process_name": "fixture.exe"}),
                argument: None,
                parameters: serde_json::from_value(json!({
                    "button": "left",
                    "fraction": {"fx": 0.4, "fy": 0.5},
                    "to_fraction": {"fx": 0.6, "fy": 0.5}
                }))
                .unwrap(),
                destination: Some(json!({"automation_id": "1001"})),
                protected: false,
            },
        ];
        let workflow = aad_runtime::assemble::assemble("explain_drag", &steps).unwrap();
        let expanded = workflow["steps"].as_array().unwrap();

        let actions = logical_actions(expanded);

        assert_eq!(expanded.len(), 7);
        assert_eq!(actions.len(), 2, "destination find is not a logical action");
        assert_eq!(actions[0]["parameters"]["keys"], json!(["ctrl", "a"]));
        assert_eq!(actions[1]["destination"], json!({"automation_id": "1001"}));
        assert_eq!(
            actions[1]["parameters"]["fraction"],
            json!({"fx": 0.4, "fy": 0.5})
        );
        assert_eq!(
            actions[1]["parameters"]["to_fraction"],
            json!({"fx": 0.6, "fy": 0.5})
        );
        assert!(actions
            .iter()
            .all(|action| action["parameters"].get("target").is_none()));
        assert!(actions
            .iter()
            .all(|action| action["parameters"].get("to").is_none()));
    }
    #[test]
    fn saved_workflows_can_be_listed_and_explained() {
        // Storing under a name was only half of it: without these, someone who
        // recorded a workflow cannot see what accumulated, and `aad list` is about
        // durable runs rather than workflows.
        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|command| command.get_name().to_string())
            .collect();
        assert!(
            names.contains(&"workflows".to_string()),
            "no way to see saved workflows: {names:?}"
        );
    }

    #[test]
    fn recording_saves_steps_that_replay_anywhere() {
        let help = Cli::command()
            .get_subcommands()
            .find(|command| command.get_name() == "record")
            .expect("record exists")
            .clone()
            .render_long_help()
            .to_string();
        assert!(
            help.contains("--save"),
            "no way to keep a recording: {help}"
        );
        assert!(
            help.contains("--overwrite"),
            "replacing has to be asked for: {help}"
        );
        assert!(
            help.contains("only exists in this session"),
            "say why an id will not do: {help}"
        );
    }

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
    fn probe_reports_the_environment_and_succeeds_when_usable() {
        let (payload, code) = dispatch(&Command::Probe);

        assert_eq!(payload["kind"], "CapabilityProbe");
        assert_eq!(code, EXIT_OK, "the probe completed and reported its state");
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
        assert!(payload["plan_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
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
            plugins: Vec::new(),
            permissions: Vec::new(),
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
            plugins: Vec::new(),
            permissions: Vec::new(),
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
                plugins: Vec::new(),
                permissions: Vec::new(),
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
            plugins: Vec::new(),
            permissions: Vec::new(),
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
            plugins: Vec::new(),
            permissions: Vec::new(),
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
            let path =
                std::env::temp_dir().join(format!("aad-cli-durable-{name}-{}", std::process::id()));
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
            execution: DurableExecutionArgs::default(),
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
        assert!(
            temp.store().store.exists(),
            "the run store must exist on disk"
        );
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
        let store =
            aad_runtime::durable::JournalStore::open(&temp.store().store).expect("the store opens");
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
        let store =
            aad_runtime::durable::JournalStore::open(&temp.store().store).expect("the store opens");
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
        let store =
            aad_runtime::durable::JournalStore::open(&temp.store().store).expect("the store opens");
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
            execution: DurableExecutionArgs::default(),
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
        let store =
            aad_runtime::durable::JournalStore::open(&temp.store().store).expect("the store opens");
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
        let store =
            aad_runtime::durable::JournalStore::open(&temp.store().store).expect("the store opens");
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
            execution: DurableExecutionArgs::default(),
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
            execution: DurableExecutionArgs::default(),
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
            execution: DurableExecutionArgs::default(),
        }));

        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["error"]["code"], "DURABLE.UNSUPPORTED_PLAN");
        // Naming the step is what makes the refusal actionable.
        assert_eq!(
            payload["error"]["details"]["unsupportedSteps"],
            json!(["press"])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_cli_runs_the_native_linux_atspi_provider_durably() {
        let temp = TempDir::new("readonly-plugin");
        let path = temp.path.join("readonly.yaml");
        std::fs::write(
            &path,
            concat!(
                "apiVersion: ai-auto-desktop.dev/v1alpha1\n",
                "kind: Workflow\n",
                "metadata:\n  name: durable.readonly\n",
                "requires:\n  platforms: [linux]\n  permissions: [desktop.observe]\n",
                "budgets:\n  max_duration: 10s\n  max_executed_steps: 2\n",
                "outputs:\n  backend:\n    value: '${{ steps.inspect.output.backend }}'\n",
                "steps:\n",
                "  - id: inspect\n    type: action\n",
                "    uses: desktop.linux_atspi.inspect_session@1\n",
                "    with: {}\n",
                "    effect: {class: read_only}\n",
                "    risk: {category: observe, level: low}\n",
                "    sensitivity: {input: public, output: public, error: public}\n",
                "    checkpoint:\n      output:\n        mode: project\n",
                "        fields: [backend]\n",
            ),
        )
        .unwrap();
        let (payload, code) = dispatch(&Command::Start(StartArgs {
            file: path,
            store: temp.store(),
            inputs: None,
            run_id: Some("run-1".into()),
            owner_id: Some("test-runner".into()),
            lease_ttl: None,
            execution: DurableExecutionArgs {
                plugins: Vec::new(),
                permissions: vec!["desktop.observe".into()],
                durable_actions: DurableActions::ReadOnly,
            },
        }));

        assert_eq!(code, EXIT_OK, "{payload}");
        assert_eq!(payload["status"], "succeeded");
        assert!(payload["output"]["backend"].is_string());
    }

    #[test]
    fn plugin_commands_preserve_arguments_and_reject_unclosed_quotes() {
        assert_eq!(
            split_plugin_command(r#"worker "driver with spaces" --mode inspect"#),
            Some(vec![
                "worker".to_string(),
                "driver with spaces".to_string(),
                "--mode".to_string(),
                "inspect".to_string(),
            ])
        );
        assert_eq!(split_plugin_command("worker 'unterminated"), None);
    }

    #[cfg(windows)]
    #[test]
    fn plugin_commands_preserve_windows_path_separators() {
        assert_eq!(
            split_plugin_command(r#""C:\Program Files\AAD\worker.exe" fixture"#),
            Some(vec![
                r"C:\Program Files\AAD\worker.exe".to_string(),
                "fixture".to_string(),
            ])
        );
        assert_eq!(
            split_plugin_command(r"C:\tools\aad-worker.exe"),
            Some(vec![r"C:\tools\aad-worker.exe".to_string()])
        );
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
        for name in [
            "start", "resume", "status", "pause", "cancel", "list", "events",
        ] {
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
                execution: DurableExecutionArgs::default(),
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
