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
    Describe(DescribeArgs),
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
    /// Serve the Model Context Protocol over stdio, for AI clients.
    Mcp,
    /// Print the tools exposed over MCP.
    Tools,
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
        Command::Describe(args) => with_driver(|driver| {
            driver
                .call("describe", &describe_arguments(args))
                .map_err(|error| driver_failure(&error))
        }),
        Command::Snapshot(args) => with_driver(|driver| {
            driver
                .call("snapshot", &describe_arguments(args))
                .map_err(|error| driver_failure(&error))
        }),
        Command::Find(args) => {
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
            if args.contains {
                locator.insert("match".into(), json!("contains"));
            }
            if locator.is_empty() {
                return (
                    failure(
                        "CLI.INVALID_ARGUMENTS",
                        "give at least one of --role, --name or --automation-id",
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
                        &json!({"window_id": window_id, "locator": Value::Object(locator)}),
                    )
                    .map_err(|error| driver_failure(&error))
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
        .with_inputs(inputs);

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
            "tools",
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
            role: None,
            name: None,
            automation_id: None,
            contains: false,
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
        }));
        let _ = std::fs::remove_file(&path);

        // A shell script must be able to detect the failure.
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(payload["status"], "failed");
        assert_eq!(payload["error"]["code"], "DEMO.STOP");
    }
}
