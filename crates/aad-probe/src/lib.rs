//! Conservative, read-only probes for desktop automation prerequisites.
//!
//! The probe reports what is visible to the current process.  It never asks
//! for a permission, opens a portal session, injects input, captures the
//! screen, or traverses an accessibility tree.  An `available` result
//! therefore does **not** mean that desktop automation has succeeded — it
//! means nothing observable is currently standing in its way.
//!
//! This distinction matters: an agent that treats a probe as proof will
//! confidently attempt work that then fails. The `notice` field in every
//! report states the limitation explicitly.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
#[cfg(windows)]
use std::path::Path;
#[cfg(any(windows, target_os = "linux"))]
use std::path::PathBuf;

pub use aad_platform::probe::{Check, State};

pub const PROBE_API_VERSION: &str = "ai-auto-desktop.dev/probe/v1alpha1";
pub const PROBE_KIND: &str = "CapabilityProbe";

/// The versioned, JSON-facing result of a probe run.
#[derive(Clone, Debug)]
pub struct Report {
    pub platform: Value,
    pub session: Value,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn to_json(&self) -> Value {
        let mut counts: BTreeMap<&str, usize> =
            State::ALL.iter().map(|state| (state.as_str(), 0)).collect();
        for check in &self.checks {
            *counts.entry(check.state.as_str()).or_insert(0) += 1;
        }

        let mut checks = Map::new();
        for check in &self.checks {
            checks.insert(check.name.clone(), check.to_json());
        }

        json!({
            "api_version": PROBE_API_VERSION,
            "kind": PROBE_KIND,
            "status": "completed",
            "platform": self.platform,
            "session": self.session,
            "checks": Value::Object(checks),
            "summary": counts,
            "notice": "Read-only prerequisite observations only; this report does not \
        prove that UI discovery, input, capture, or automation succeeds.",
        })
    }

    /// The worst state observed, which is what a caller should gate on.
    pub fn worst(&self) -> State {
        // Unknown is deliberately ranked below unavailable: a definite "no" is
        // more actionable than "we could not tell".
        self.checks
            .iter()
            .map(|check| check.state)
            .max_by_key(|state| match state {
                State::Available => 0,
                State::Degraded => 1,
                State::Unknown => 2,
                State::Unavailable => 3,
            })
            .unwrap_or(State::Unknown)
    }

    pub fn get(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.name == name)
    }
}

fn canonical_platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

fn platform_info() -> Value {
    json!({
        "name": canonical_platform(),
        "system": std::env::consts::OS,
        "machine": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "runtime": env!("CARGO_PKG_VERSION"),
    })
}

/// Classify the session from environment signals alone.
fn session_info() -> Value {
    let variable = |name: &str| std::env::var(name).unwrap_or_default();
    let display = !variable("DISPLAY").is_empty();
    let wayland = !variable("WAYLAND_DISPLAY").is_empty();
    let ssh = !variable("SSH_CONNECTION").is_empty() || !variable("SSH_TTY").is_empty();
    let xdg_type = variable("XDG_SESSION_TYPE").to_lowercase();
    let windows_session = variable("SESSIONNAME").to_lowercase();

    let mut kind = "unknown";
    let mut interactive: Option<bool> = None;

    match canonical_platform() {
        "linux" => {
            if wayland || xdg_type == "wayland" {
                kind = "wayland";
                interactive = Some(true);
            } else if display || xdg_type == "x11" {
                kind = "x11";
                interactive = Some(true);
            } else if ssh {
                kind = "ssh";
                interactive = Some(display || wayland);
            } else if xdg_type == "tty" || xdg_type == "console" {
                kind = "tty";
                interactive = Some(true);
            }
            if ssh {
                kind = match kind {
                    "x11" => "ssh_x11",
                    "wayland" => "ssh_wayland",
                    _ => "ssh",
                };
                // Environment variables alone do not prove a forwarded display
                // is connected or controllable.
                interactive = None;
            }
        }
        "windows" => {
            if windows_session.starts_with("rdp") {
                kind = "remote_desktop";
                interactive = Some(true);
            } else if windows_session == "console" {
                kind = "console";
                interactive = Some(true);
            } else if windows_session == "services" {
                kind = "service";
                interactive = Some(false);
            }
        }
        "macos" if ssh => {
            kind = "ssh";
            interactive = Some(false);
        }
        _ => {}
    }

    json!({
        "kind": kind,
        "interactive": interactive,
        "signals": {
            "display_advertised": display,
            "wayland_display_advertised": wayland,
            "xdg_session_type_advertised": !xdg_type.is_empty(),
            "ssh_advertised": ssh,
            "windows_session_advertised": !windows_session.is_empty(),
        },
    })
}

/// Observe platform prerequisites without requesting or exercising them.
pub fn probe() -> Report {
    Report {
        platform: platform_info(),
        session: session_info(),
        checks: platform_checks(),
    }
}

#[cfg(windows)]
fn platform_checks() -> Vec<Check> {
    let mut checks = aad_windows::probe::run();
    checks.push(script_sandbox_check());
    checks
}

#[cfg(target_os = "linux")]
fn platform_checks() -> Vec<Check> {
    let mut checks = aad_linux::probe::run();
    checks.push(script_sandbox_check());
    checks
}

#[cfg(target_os = "macos")]
fn platform_checks() -> Vec<Check> {
    let mut checks = aad_macos::probe::run();
    checks.push(script_sandbox_check());
    checks
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_checks() -> Vec<Check> {
    vec![Check::new(
        "platform.supported",
        State::Unavailable,
        "This operating system has no platform-specific probe.",
        json!({"platform": canonical_platform()}),
    )]
}

fn script_sandbox_check() -> Check {
    #[cfg(target_os = "linux")]
    {
        let bubblewrap = aad_linux::probe::fixed_command("bwrap").is_some();
        let prlimit = aad_linux::probe::fixed_command("prlimit").is_some();
        let interpreter = PathBuf::from("/usr/bin/python3").is_file();
        let available = bubblewrap && prlimit && interpreter;
        Check::new(
            "script.sandbox",
            if available {
                State::Available
            } else {
                State::Unavailable
            },
            if available {
                "The Linux script sandbox prerequisites are installed; no script was executed."
            } else {
                "bubblewrap, prlimit, or the fixed Python interpreter is missing; script steps fail closed."
            },
            json!({"mechanism": "bubblewrap", "bubblewrap_found": bubblewrap, "prlimit_found": prlimit, "interpreter_resolved": interpreter, "enforced": ["memory_limit", "cpu_time_limit", "process_tree_reclamation", "empty_environment", "isolated_working_directory", "isolated_interpreter", "network_namespace", "pid_namespace", "filesystem_namespace"], "not_enforced": []}),
        )
    }
    #[cfg(windows)]
    {
        let interpreter = windows_interpreter_exists();
        let job = aad_plugin::windows_job::Job::create();
        let job_limits = job
            .as_ref()
            .is_some_and(|job| job.set_limits(536_870_912, 31, 8));
        let usable = interpreter && job_limits;
        Check::new(
            "script.sandbox",
            if usable {
                State::Degraded
            } else {
                State::Unavailable
            },
            if usable {
                "Job Object limits are available, but network and filesystem access are not isolated."
            } else {
                "The fixed interpreter or Job Object resource limits are unavailable."
            },
            json!({"mechanism": "windows_job_object", "interpreter_resolved": interpreter, "job_object_limits_available": job_limits, "enforced": ["memory_limit", "cpu_time_limit", "process_count_limit", "process_tree_reclamation", "empty_environment", "isolated_working_directory", "isolated_interpreter"], "not_enforced": ["network", "filesystem"]}),
        )
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    Check::new(
        "script.sandbox",
        State::Unavailable,
        "No supported script sandbox is available on this platform.",
        json!({"mechanism": null, "enforced": [], "not_enforced": []}),
    )
}

#[cfg(windows)]
fn windows_interpreter_exists() -> bool {
    let mut candidates = Vec::new();
    if let Some(configured) = std::env::var_os("AAD_SCRIPT_PYTHON") {
        let configured = PathBuf::from(configured);
        if configured.is_absolute()
            && !matches!(configured.file_name().and_then(|name| name.to_str()), Some(name) if name.eq_ignore_ascii_case("py.exe") || name.eq_ignore_ascii_case("pyw.exe"))
        {
            candidates.push(configured);
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let root = PathBuf::from(local).join("Programs").join("Python");
        if let Ok(entries) = std::fs::read_dir(root) {
            candidates.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path().join("python.exe")),
            );
        }
    }
    candidates.extend(
        [
            "C:\\Python313",
            "C:\\Python312",
            "C:\\Python311",
            "C:\\Python310",
        ]
        .into_iter()
        .map(|root| Path::new(root).join("python.exe")),
    );
    candidates.into_iter().any(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_declares_its_api_version_and_kind() {
        let document = probe().to_json();

        assert_eq!(document["api_version"], PROBE_API_VERSION);
        assert_eq!(document["kind"], PROBE_KIND);
        assert_eq!(document["status"], "completed");
    }

    #[test]
    fn a_report_always_carries_the_limitation_notice() {
        let document = probe().to_json();
        let notice = document["notice"].as_str().expect("a notice is present");

        // Callers must never read `available` as proof that automation works.
        assert!(notice.contains("does not"), "{notice}");
        assert!(notice.to_lowercase().contains("read-only"), "{notice}");
    }

    #[test]
    fn the_summary_counts_every_state_and_totals_the_checks() {
        let report = probe();
        let document = report.to_json();
        let summary = document["summary"].as_object().expect("a summary");

        for state in State::ALL {
            assert!(
                summary.contains_key(state.as_str()),
                "missing count for {}",
                state.as_str()
            );
        }
        let total: u64 = summary.values().filter_map(Value::as_u64).sum();
        assert_eq!(total as usize, report.checks.len());
    }

    #[test]
    fn every_check_reports_a_valid_state_and_a_summary() {
        let report = probe();
        assert!(!report.checks.is_empty(), "a probe must observe something");

        let valid: Vec<&str> = State::ALL.iter().map(|state| state.as_str()).collect();
        for check in &report.checks {
            assert!(valid.contains(&check.state.as_str()), "{}", check.name);
            assert!(!check.summary.is_empty(), "{} has no summary", check.name);
            assert!(!check.name.is_empty());
        }
    }

    #[test]
    fn check_names_are_unique_so_the_json_map_loses_nothing() {
        let report = probe();
        let mut names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();

        assert_eq!(names.len(), count, "duplicate check names collide in JSON");
        assert_eq!(report.to_json()["checks"].as_object().unwrap().len(), count);
    }

    #[test]
    fn the_platform_block_identifies_this_machine() {
        let document = probe().to_json();
        let platform = &document["platform"];

        assert!(!platform["name"].as_str().unwrap().is_empty());
        assert_eq!(platform["system"], std::env::consts::OS);
        assert_eq!(platform["machine"], std::env::consts::ARCH);
    }

    #[test]
    fn the_session_block_reports_a_kind_and_its_signals() {
        let document = probe().to_json();
        let session = &document["session"];

        assert!(session["kind"].as_str().is_some());
        assert!(session["signals"].is_object());
    }

    #[test]
    fn the_worst_state_is_the_one_a_caller_should_gate_on() {
        let report = Report {
            platform: json!({}),
            session: json!({}),
            checks: vec![
                Check::new("a", State::Available, "fine", json!({})),
                Check::new("b", State::Degraded, "partial", json!({})),
                Check::new("c", State::Unavailable, "absent", json!({})),
            ],
        };
        assert_eq!(report.worst(), State::Unavailable);

        let softer = Report {
            platform: json!({}),
            session: json!({}),
            checks: vec![
                Check::new("a", State::Available, "fine", json!({})),
                Check::new("b", State::Degraded, "partial", json!({})),
            ],
        };
        assert_eq!(softer.worst(), State::Degraded);
    }

    #[test]
    fn an_unknown_result_outranks_a_merely_degraded_one() {
        // "We could not tell" must not be presented as better than a caveat.
        let report = Report {
            platform: json!({}),
            session: json!({}),
            checks: vec![
                Check::new("a", State::Degraded, "partial", json!({})),
                Check::new("b", State::Unknown, "undetermined", json!({})),
            ],
        };
        assert_eq!(report.worst(), State::Unknown);
    }

    #[test]
    fn the_report_is_serialisable_and_stable_across_runs() {
        let first = probe().to_json();
        let second = probe().to_json();

        // The shape must not vary between runs, or consumers cannot rely on it.
        assert_eq!(
            first["checks"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            second["checks"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>()
        );
        assert!(serde_json::to_string(&first).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_the_checks_the_driver_depends_on() {
        let report = probe();

        for name in ["windows.display", "windows.session", "windows.uia"] {
            assert!(report.get(name).is_some(), "missing check {name}");
        }
        // The driver cannot work at all without UI Automation, so on a real
        // desktop this must be observable.
        let uia = report.get("windows.uia").unwrap();
        assert_eq!(
            uia.state,
            State::Available,
            "UI Automation should be available on a Windows desktop: {}",
            uia.summary
        );
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn an_unsupported_platform_says_so_plainly() {
        let report = probe();
        let check = report.get("platform.supported").expect("a platform check");

        assert_eq!(check.state, State::Unavailable);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reports_each_distinct_desktop_boundary() {
        let report = probe();
        for name in [
            "linux.at_spi",
            "linux.x11",
            "linux.wayland",
            "linux.remote_desktop_portal",
            "linux.libei",
            "linux.uinput",
            "script.sandbox",
        ] {
            assert!(report.get(name).is_some(), "missing check {name}");
        }
    }
}
