//! Sandboxed execution of `script` steps.
//!
//! A script step runs untrusted Python. The runtime never evaluates it in
//! process: it spawns a real interpreter under whatever isolation the platform
//! can actually enforce, and **fails closed** where it cannot.
//!
//! # What each platform enforces
//!
//! | Platform | Isolation |
//! |---|---|
//! | Windows | Job Object: memory, CPU time and process-count ceilings, guaranteed reclamation of the whole tree, empty environment, isolated working directory, `-I` interpreter mode. **No network or filesystem isolation** — Windows has no per-process network or mount namespace. |
//! | Linux | bubblewrap: all of the above plus private network and PID namespaces, no host `/etc` or home, tmpfs working directory, read-only bind of the single script file. |
//! | Anything else | Refused with `SCRIPT.SANDBOX_UNAVAILABLE`. |
//!
//! The Windows gap is reported by [`availability`] rather than papered over.
//! A caller that needs true containment must check it, not assume it.

use aad_core::AutomationError;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The default cap on a script's stdout.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Matches the Linux `--as` limit, so both platforms cap memory alike.
#[cfg(windows)]
const MEMORY_LIMIT_BYTES: u64 = 536_870_912;
/// The interpreter plus a small margin.
#[cfg(windows)]
const MAX_ACTIVE_PROCESSES: u32 = 8;

fn error(code: &str, message: impl Into<String>) -> AutomationError {
    AutomationError::new(code, message)
        .with_category("script")
        // A script that never started cannot have changed anything.
        .with_effect("not_applied")
}

/// What isolation this platform can actually provide.
pub fn availability() -> Value {
    #[cfg(windows)]
    {
        let interpreter = find_interpreter();
        let job_object = aad_plugin::windows_job::Job::create();
        let job_limits = job_object.as_ref().is_some_and(|job| {
            job.set_limits(
                MEMORY_LIMIT_BYTES,
                DEFAULT_TIMEOUT.as_secs() + 1,
                MAX_ACTIVE_PROCESSES,
            )
        });
        let state = if interpreter.is_some() && job_limits {
            "degraded"
        } else {
            "unavailable"
        };
        json!({
            "state": state,
            "mechanism": "windows_job_object",
            "interpreter": interpreter.map(|path| path.display().to_string()),
            "job_object_limits_available": job_limits,
            "enforced": [
                "memory_limit", "cpu_time_limit", "process_count_limit",
                "process_tree_reclamation", "empty_environment",
                "isolated_working_directory", "isolated_interpreter",
            ],
            // Stated plainly: a caller must not mistake this for containment.
            "gaps": [
                "no network isolation: the script can reach the network",
                "no filesystem isolation: the script can read what the user can read",
            ],
            "summary": "Resource and environment isolation are enforced by a Job \
        Object, but network and filesystem access are not restricted.",
        })
    }
    #[cfg(target_os = "linux")]
    {
        let bubblewrap = which("bwrap");
        let prlimit = which("prlimit");
        let python = PathBuf::from("/usr/bin/python3");
        let usable = bubblewrap.is_some() && prlimit.is_some() && python.is_file();
        let missing = [
            bubblewrap.is_none().then_some("bwrap"),
            prlimit.is_none().then_some("prlimit"),
            (!python.is_file()).then_some("/usr/bin/python3"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        json!({
            "state": if usable { "available" } else { "unavailable" },
            "mechanism": "bubblewrap",
            "interpreter": python.is_file().then(|| python.display().to_string()),
            "enforced": [
                "memory_limit", "cpu_time_limit", "process_count_limit",
                "process_tree_reclamation", "empty_environment",
                "isolated_working_directory", "isolated_interpreter",
                "network_namespace", "pid_namespace", "filesystem_namespace",
            ],
            "gaps": [],
            "missing": missing,
            "summary": if usable {
                "Full namespace isolation is enforced by bubblewrap."
            } else {
                "bubblewrap, prlimit, or /usr/bin/python3 is missing."
            },
        })
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        json!({
            "state": "unavailable",
            "mechanism": null,
            "enforced": [],
            "gaps": ["no sandbox implementation for this platform"],
            "summary": "Script execution fails closed on this platform.",
        })
    }
}

#[cfg(unix)]
fn which(command: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .into_iter()
        .map(|directory| Path::new(directory).join(command))
        .find(|candidate| candidate.is_file())
}

/// Locate an interpreter without trusting the caller's `PATH`.
///
/// A script interpreter is executable code, so resolving it from `PATH` would
/// let an attacker-controlled entry decide what runs.
#[cfg(windows)]
fn find_interpreter() -> Option<PathBuf> {
    // The operator may pin an exact executable. Never resolve a bare name from
    // PATH: a script interpreter is executable code.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(configured) = std::env::var_os("AAD_SCRIPT_PYTHON") {
        let configured = PathBuf::from(configured);
        if configured.is_absolute()
            && !matches!(
                configured.file_name().and_then(|name| name.to_str()),
                Some(name) if name.eq_ignore_ascii_case("py.exe") || name.eq_ignore_ascii_case("pyw.exe")
            )
        {
            candidates.push(configured);
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let root = PathBuf::from(local).join("Programs").join("Python");
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.filter_map(|entry| entry.ok()) {
                candidates.push(entry.path().join("python.exe"));
            }
        }
    }
    for root in [
        "C:\\Python313",
        "C:\\Python312",
        "C:\\Python311",
        "C:\\Python310",
    ] {
        candidates.push(PathBuf::from(root).join("python.exe"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let root = PathBuf::from(program_files);
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.filter_map(|entry| entry.ok()) {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("Python") {
                    candidates.push(entry.path().join("python.exe"));
                }
            }
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// A script step's resolved parameters.
pub struct ScriptStep<'a> {
    /// Inline source, or `None` when an entrypoint file is used.
    pub source: Option<&'a str>,
    /// A path relative to the descriptor's directory.
    pub entrypoint: Option<&'a str>,
    pub max_output_bytes: usize,
}

impl<'a> ScriptStep<'a> {
    /// Read a script step from its compiled parameters.
    pub fn from_params(
        params: &'a serde_json::Map<String, Value>,
    ) -> Result<Self, AutomationError> {
        let source = params.get("source").and_then(Value::as_str);
        let entrypoint = params.get("entrypoint").and_then(Value::as_str);
        if source.is_none() && entrypoint.is_none() {
            return Err(error(
                "SCRIPT.INVALID",
                "a script step needs either source or entrypoint",
            ));
        }
        if source.is_some() && entrypoint.is_some() {
            return Err(error(
                "SCRIPT.INVALID",
                "a script step cannot have both source and entrypoint",
            ));
        }
        let max_output_bytes = params
            .get("sandbox")
            .and_then(|sandbox| sandbox.get("max_output_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES as u64) as usize;

        Ok(Self {
            source,
            entrypoint,
            max_output_bytes,
        })
    }
}

/// Resolve an entrypoint against the descriptor's directory.
///
/// The result must stay inside that directory: an entrypoint is data from a
/// descriptor, so `../../etc/passwd` must not be reachable.
pub fn resolve_entrypoint(base: &Path, entrypoint: &str) -> Result<PathBuf, AutomationError> {
    let candidate = Path::new(entrypoint);
    if candidate.is_absolute() {
        return Err(error(
            "SCRIPT.ENTRYPOINT_INVALID",
            "a script entrypoint must be a relative path",
        ));
    }
    if candidate
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(error(
            "SCRIPT.ENTRYPOINT_INVALID",
            "a script entrypoint must not traverse outside the workflow directory",
        ));
    }
    let resolved = base.join(candidate);
    // Re-check after canonicalisation, so a symlink cannot escape either.
    if let (Ok(real), Ok(root)) = (resolved.canonicalize(), base.canonicalize()) {
        if !real.starts_with(&root) {
            return Err(error(
                "SCRIPT.ENTRYPOINT_INVALID",
                "a script entrypoint must resolve inside the workflow directory",
            ));
        }
        return Ok(real);
    }
    Err(error(
        "SCRIPT.ENTRYPOINT_INVALID",
        format!("the script entrypoint {entrypoint:?} was not found"),
    ))
}

/// Execute a script step, returning the JSON value it printed.
pub fn execute(
    step: &ScriptStep<'_>,
    base: &Path,
    inputs: &Value,
    timeout: Option<Duration>,
) -> Result<Value, AutomationError> {
    if availability()["state"] == "unavailable" {
        return Err(error(
            "SCRIPT.SANDBOX_UNAVAILABLE",
            "no script sandbox is available on this platform",
        ));
    }

    let workspace = TempDirectory::new()?;
    let source_path = match (step.source, step.entrypoint) {
        (Some(source), _) => {
            let path = workspace.path().join("script.py");
            std::fs::write(&path, source).map_err(|error_value| {
                error("SCRIPT.SANDBOX_UNAVAILABLE", format!("{error_value}"))
            })?;
            path
        }
        (None, Some(entrypoint)) => resolve_entrypoint(base, entrypoint)?,
        (None, None) => unreachable!("validated in ScriptStep::from_params"),
    };

    // The script runs from its own empty directory, so a bare relative path
    // inside it cannot reach workflow files.
    let working = workspace.path().join("cwd");
    std::fs::create_dir_all(&working)
        .map_err(|value| error("SCRIPT.SANDBOX_UNAVAILABLE", format!("{value}")))?;

    let budget = timeout.unwrap_or(DEFAULT_TIMEOUT);
    run_sandboxed(
        &source_path,
        &working,
        inputs,
        budget,
        step.max_output_bytes,
    )
}

#[cfg(windows)]
fn run_sandboxed(
    source: &Path,
    working: &Path,
    inputs: &Value,
    budget: Duration,
    max_output_bytes: usize,
) -> Result<Value, AutomationError> {
    let interpreter = find_interpreter().ok_or_else(|| {
        error(
            "SCRIPT.SANDBOX_UNAVAILABLE",
            "no usable Python interpreter was found for the Windows sandbox",
        )
    })?;

    let mut command = Command::new(&interpreter);
    command
        // -I: isolated mode. No PYTHONPATH, no user site-packages, no
        // environment influence over the interpreter.
        .arg("-I")
        .arg("-B")
        .arg(source)
        .current_dir(working)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // SystemRoot is required for the interpreter to initialise at all.
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }

    let mut child = command
        .spawn()
        .map_err(|value| error("SCRIPT.SANDBOX_UNAVAILABLE", format!("{value}")))?;

    // Bound the tree before it can spawn anything: a Job Object guarantees
    // reclamation even if the script forks or the host dies.
    let job = aad_plugin::windows_job::Job::create();
    let bounded = match job.as_ref() {
        Some(job) => {
            let limited = job.set_limits(
                MEMORY_LIMIT_BYTES,
                budget.as_secs().max(1) + 1,
                MAX_ACTIVE_PROCESSES,
            );
            job.assign(&child) && limited
        }
        None => false,
    };

    let outcome = finish(&mut child, inputs, budget, max_output_bytes, bounded);
    // Dropping the job closes the last handle, killing any survivor.
    drop(job);
    outcome
}

#[cfg(not(windows))]
fn run_sandboxed(
    source: &Path,
    working: &Path,
    inputs: &Value,
    budget: Duration,
    max_output_bytes: usize,
) -> Result<Value, AutomationError> {
    let bubblewrap = which("bwrap")
        .ok_or_else(|| error("SCRIPT.SANDBOX_UNAVAILABLE", "bubblewrap is not available"))?;
    let prlimit = which("prlimit")
        .ok_or_else(|| error("SCRIPT.SANDBOX_UNAVAILABLE", "prlimit is not available"))?;
    // `/usr/bin/python3` is a symlink on Debian and Ubuntu (including GitHub's
    // runners). Binding the symlink alone into a fresh mount namespace leaves
    // its target absent, so bwrap starts successfully and then reports the
    // misleading `execvp ... No such file or directory`. Resolve the trusted,
    // fixed path on the host and bind the actual executable at the canonical
    // in-sandbox name.
    let interpreter = Path::new("/usr/bin/python3")
        .canonicalize()
        .map_err(|value| error("SCRIPT.SANDBOX_UNAVAILABLE", format!("{value}")))?;

    let seconds = budget.as_secs().max(1) + 1;
    let mut command = Command::new(bubblewrap);
    command
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
        ])
        .arg("--ro-bind")
        .arg(interpreter)
        .arg("/usr/bin/python3")
        .arg("--ro-bind")
        .arg(&prlimit)
        .arg("/usr/bin/prlimit")
        .args(["--ro-bind", "/usr/lib", "/usr/lib"]);
    // Dynamic loaders live under /lib or /lib64 depending on the distribution.
    // They are outside /usr/lib on GitHub's Ubuntu runners, so the interpreter
    // exists in the sandbox but cannot be executed unless these roots follow it.
    for library_root in ["/lib", "/lib64"] {
        if Path::new(library_root).exists() {
            command.args(["--ro-bind", library_root, library_root]);
        }
    }
    let mut child = command
        .args(["--dir", "/workflow"])
        .args([
            "--ro-bind",
            &source.display().to_string(),
            "/workflow/script.py",
        ])
        .args(["--tmpfs", "/tmp", "--chdir", "/tmp"])
        .args(["--proc", "/proc", "--dev", "/dev"])
        .arg("--clearenv")
        .args(["--setenv", "PATH", "/usr/bin:/bin"])
        .args(["--setenv", "PYTHONIOENCODING", "utf-8"])
        .arg("/usr/bin/prlimit")
        .arg(format!("--fsize={max_output_bytes}"))
        .arg("--as=536870912")
        .arg(format!("--cpu={seconds}"))
        .arg("--nofile=64")
        .arg("--core=0")
        .arg("--")
        .args(["/usr/bin/python3", "-I", "-B", "/workflow/script.py"])
        .current_dir(working)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|value| error("SCRIPT.SANDBOX_UNAVAILABLE", format!("{value}")))?;

    finish(&mut child, inputs, budget, max_output_bytes, true)
}

/// Feed inputs, enforce the deadline, and decode the result.
fn finish(
    child: &mut std::process::Child,
    inputs: &Value,
    budget: Duration,
    max_output_bytes: usize,
    bounded: bool,
) -> Result<Value, AutomationError> {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(inputs.to_string().as_bytes());
        // Closing stdin lets a script that reads to EOF terminate.
        drop(stdin);
    }

    // Readers must run on their own threads, and the deadline must be watched
    // independently of them: reading to EOF blocks until the process exits, so
    // waiting for the readers first would let a hung script ignore its budget
    // entirely. Killing the child closes the pipes, which unblocks the readers.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    std::thread::scope(|scope| {
        let out = scope.spawn(move || read_capped(&mut stdout, max_output_bytes));
        let err = scope.spawn(move || read_capped(&mut stderr, MAX_STDERR_BYTES));

        let deadline = std::time::Instant::now() + budget;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(value) => {
                    let _ = child.kill();
                    return Err(error("SCRIPT.FAILED", format!("{value}")));
                }
            }
        };

        let Some(status) = status else {
            // Killing it both stops the work and releases the reader threads.
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let _ = err.join();
            return Err(AutomationError::new(
                "SCRIPT.TIMEOUT",
                "the script exceeded its time budget",
            )
            .with_category("script")
            // The script ran, so it may already have had an effect.
            .with_effect("unknown")
            .with_details(
                json!({"bounded_process_tree": bounded})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ));
        };

        let out_bytes = out.join().unwrap_or_default();
        let err_bytes = err.join().unwrap_or_default();
        decode(&out_bytes, &err_bytes, status.code(), max_output_bytes)
    })
}

fn read_capped(stream: &mut Option<impl std::io::Read>, limit: usize) -> Vec<u8> {
    use std::io::Read;
    let Some(stream) = stream.as_mut() else {
        return Vec::new();
    };
    let mut buffer = Vec::new();
    // Read one byte past the limit so an over-limit stream is detectable.
    let _ = stream.take(limit as u64 + 1).read_to_end(&mut buffer);
    buffer
}

/// The shared output contract: size limit, exit status, then one JSON value.
pub fn decode(
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i32>,
    max_output_bytes: usize,
) -> Result<Value, AutomationError> {
    if stdout.len() > max_output_bytes || stderr.len() > MAX_STDERR_BYTES.min(max_output_bytes) {
        return Err(error(
            "SCRIPT.OUTPUT_INVALID",
            "script output exceeded its configured limit",
        ));
    }
    let stderr_text = String::from_utf8_lossy(stderr);

    if exit_code != Some(0) {
        let tail: String = stderr_text
            .chars()
            .rev()
            .take(4096)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(AutomationError::new(
            "SCRIPT.EXIT_NONZERO",
            format!("the script exited with status {exit_code:?}"),
        )
        .with_category("script")
        // A script that ran and failed may still have acted first.
        .with_effect("unknown")
        .with_details(
            json!({"returncode": exit_code, "stderr": tail})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        ));
    }

    let text = std::str::from_utf8(stdout).map_err(|value| {
        error(
            "SCRIPT.OUTPUT_INVALID",
            format!("script stdout must be UTF-8: {value}"),
        )
    })?;
    serde_json::from_str(text).map_err(|value| {
        error(
            "SCRIPT.OUTPUT_INVALID",
            format!("script stdout must be one JSON value: {value}"),
        )
    })
}

/// A temporary directory removed when dropped.
struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Result<Self, AutomationError> {
        let path =
            std::env::temp_dir().join(format!("aad-script-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path)
            .map_err(|value| error("SCRIPT.SANDBOX_UNAVAILABLE", format!("{value}")))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(body: Value) -> serde_json::Map<String, Value> {
        body.as_object().cloned().unwrap()
    }

    #[test]
    fn availability_states_what_it_does_and_does_not_enforce() {
        let report = availability();

        assert!(report["state"].as_str().is_some());
        assert!(report["enforced"].is_array());
        assert!(report["gaps"].is_array());

        if cfg!(windows) {
            // The Windows gap must never be hidden from a caller.
            let gaps: Vec<String> = report["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|gap| gap.as_str().unwrap_or_default().to_string())
                .collect();
            assert!(
                gaps.iter().any(|gap| gap.contains("network")),
                "the missing network isolation must be declared: {gaps:?}"
            );
            assert_ne!(
                report["state"], "available",
                "partial isolation must not be reported as full"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_availability_checks_every_required_executable() {
        let report = availability();
        assert_eq!(
            report["state"] == "available",
            which("bwrap").is_some()
                && which("prlimit").is_some()
                && Path::new("/usr/bin/python3").is_file()
        );
        assert!(report["missing"].is_array());
    }

    #[test]
    fn a_script_step_needs_exactly_one_of_source_or_entrypoint() {
        assert!(ScriptStep::from_params(&params(json!({}))).is_err());
        assert!(ScriptStep::from_params(&params(
            json!({"source": "print(1)", "entrypoint": "a.py"})
        ))
        .is_err());
        assert!(ScriptStep::from_params(&params(json!({"source": "print(1)"}))).is_ok());
        assert!(ScriptStep::from_params(&params(json!({"entrypoint": "a.py"}))).is_ok());
    }

    #[test]
    fn the_output_limit_defaults_and_can_be_overridden() {
        let plain = params(json!({"source": "x"}));
        let default = ScriptStep::from_params(&plain).unwrap();
        assert_eq!(default.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);

        let bounded = params(json!({"source": "x", "sandbox": {"max_output_bytes": 2048}}));
        let custom = ScriptStep::from_params(&bounded).unwrap();
        assert_eq!(custom.max_output_bytes, 2048);
    }

    #[test]
    fn an_entrypoint_cannot_escape_the_workflow_directory() {
        let base = std::env::temp_dir();
        for hostile in ["../secrets.py", "..\\secrets.py", "a/../../b.py"] {
            let outcome = resolve_entrypoint(&base, hostile);
            assert!(outcome.is_err(), "{hostile:?} must be refused");
            assert_eq!(outcome.unwrap_err().code, "SCRIPT.ENTRYPOINT_INVALID");
        }
    }

    #[test]
    fn an_absolute_entrypoint_is_refused() {
        let base = std::env::temp_dir();
        let absolute = if cfg!(windows) {
            "C:\\evil.py"
        } else {
            "/evil.py"
        };

        assert!(resolve_entrypoint(&base, absolute).is_err());
    }

    #[test]
    fn a_missing_entrypoint_is_reported_clearly() {
        let error = resolve_entrypoint(&std::env::temp_dir(), "definitely-absent.py").unwrap_err();
        assert_eq!(error.code, "SCRIPT.ENTRYPOINT_INVALID");
    }

    #[test]
    fn an_entrypoint_inside_the_directory_resolves() {
        let base = std::env::temp_dir().join("aad-script-entry-test");
        std::fs::create_dir_all(&base).unwrap();
        let script = base.join("task.py");
        std::fs::write(&script, "print(1)").unwrap();

        let resolved = resolve_entrypoint(&base, "task.py").expect("it resolves");
        assert!(resolved.ends_with("task.py"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_successful_script_returns_its_json_value() {
        let value = decode(b"{\"ok\": true, \"n\": 3}", b"", Some(0), 1024).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["n"], 3);
    }

    #[test]
    fn a_non_zero_exit_reports_the_code_and_stderr() {
        let error = decode(b"", b"Traceback: boom", Some(2), 1024).unwrap_err();

        assert_eq!(error.code, "SCRIPT.EXIT_NONZERO");
        assert_eq!(error.details["returncode"], json!(2));
        assert!(error.details["stderr"].as_str().unwrap().contains("boom"));
        // It ran, so its effect cannot be proven absent.
        assert_eq!(error.effect, "unknown");
    }

    #[test]
    fn output_that_is_not_json_is_rejected() {
        let error = decode(b"just some text", b"", Some(0), 1024).unwrap_err();
        assert_eq!(error.code, "SCRIPT.OUTPUT_INVALID");
    }

    #[test]
    fn output_beyond_the_limit_is_rejected() {
        let big = vec![b'x'; 2048];
        let error = decode(&big, b"", Some(0), 1024).unwrap_err();
        assert_eq!(error.code, "SCRIPT.OUTPUT_INVALID");
    }

    #[test]
    fn non_utf8_output_is_rejected() {
        let error = decode(&[0xff, 0xfe, 0xfd], b"", Some(0), 1024).unwrap_err();
        assert_eq!(error.code, "SCRIPT.OUTPUT_INVALID");
    }

    #[test]
    fn a_killed_script_reports_a_non_zero_exit() {
        // A signalled process has no exit code; it must not look like success.
        let error = decode(b"{}", b"", None, 1024).unwrap_err();
        assert_eq!(error.code, "SCRIPT.EXIT_NONZERO");
    }

    #[test]
    fn a_real_script_runs_and_returns_its_output() {
        if availability()["state"] == "unavailable" {
            return;
        }
        let step = ScriptStep {
            source: Some("import json; print(json.dumps({'answer': 6 * 7}))"),
            entrypoint: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };

        let value =
            execute(&step, &std::env::temp_dir(), &json!({}), None).expect("the script runs");

        assert_eq!(value["answer"], 42);
    }

    #[test]
    fn a_real_script_receives_its_inputs_on_stdin() {
        if availability()["state"] == "unavailable" {
            return;
        }
        let step = ScriptStep {
            source: Some(
                "import json,sys; data=json.load(sys.stdin); \
print(json.dumps({'doubled': data['n'] * 2}))",
            ),
            entrypoint: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };

        let value = execute(&step, &std::env::temp_dir(), &json!({"n": 21}), None)
            .expect("the script runs");

        assert_eq!(value["doubled"], 42);
    }

    #[test]
    fn a_failing_script_surfaces_its_traceback() {
        if availability()["state"] == "unavailable" {
            return;
        }
        let step = ScriptStep {
            source: Some("raise SystemExit(3)"),
            entrypoint: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };

        let error =
            execute(&step, &std::env::temp_dir(), &json!({}), None).expect_err("the script fails");

        assert_eq!(error.code, "SCRIPT.EXIT_NONZERO");
        assert_eq!(error.details["returncode"], json!(3));
    }

    #[test]
    fn a_script_that_overruns_its_budget_is_stopped() {
        if availability()["state"] == "unavailable" {
            return;
        }
        let step = ScriptStep {
            source: Some("import time; time.sleep(30)"),
            entrypoint: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };

        let started = std::time::Instant::now();
        let error = execute(
            &step,
            &std::env::temp_dir(),
            &json!({}),
            Some(Duration::from_millis(500)),
        )
        .expect_err("the script is stopped");

        assert_eq!(error.code, "SCRIPT.TIMEOUT");
        assert_eq!(error.effect, "unknown");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the budget must actually be enforced"
        );
    }

    #[test]
    fn a_script_runs_with_an_empty_environment() {
        if availability()["state"] == "unavailable" {
            return;
        }
        // A secret in the host environment must not leak into the sandbox.
        std::env::set_var("AAD_SANDBOX_LEAK_CHECK", "must-not-be-visible");
        let step = ScriptStep {
            source: Some(
                "import json,os; print(json.dumps({'leaked': \
os.environ.get('AAD_SANDBOX_LEAK_CHECK')}))",
            ),
            entrypoint: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };

        let value =
            execute(&step, &std::env::temp_dir(), &json!({}), None).expect("the script runs");
        std::env::remove_var("AAD_SANDBOX_LEAK_CHECK");

        assert_eq!(
            value["leaked"],
            Value::Null,
            "the host environment leaked in"
        );
    }

    #[test]
    fn a_script_runs_in_its_own_empty_directory() {
        if availability()["state"] == "unavailable" {
            return;
        }
        let step = ScriptStep {
            source: Some("import json,os; print(json.dumps({'entries': os.listdir('.')}))"),
            entrypoint: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };

        let value =
            execute(&step, &std::env::temp_dir(), &json!({}), None).expect("the script runs");

        assert_eq!(
            value["entries"].as_array().map(Vec::len),
            Some(0),
            "the working directory must be empty: {value}"
        );
    }
}
