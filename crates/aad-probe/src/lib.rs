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

pub const PROBE_API_VERSION: &str = "ai-auto-desktop.dev/probe/v1alpha1";
pub const PROBE_KIND: &str = "CapabilityProbe";

/// The four states a prerequisite observation can report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Nothing observable prevents the capability from working.
    Available,
    /// Partially present, or present but with a caveat.
    Degraded,
    /// Observably absent.
    Unavailable,
    /// Could not be determined; absence of evidence, not evidence of absence.
    Unknown,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Available => "available",
            State::Degraded => "degraded",
            State::Unavailable => "unavailable",
            State::Unknown => "unknown",
        }
    }

    pub const ALL: [State; 4] = [
        State::Available,
        State::Degraded,
        State::Unavailable,
        State::Unknown,
    ];
}

/// One narrowly scoped prerequisite observation.
#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub state: State,
    pub summary: String,
    pub evidence: Map<String, Value>,
}

impl Check {
    pub fn new(name: &str, state: State, summary: &str, evidence: Value) -> Self {
        Self {
            name: name.to_string(),
            state,
            summary: summary.to_string(),
            evidence: evidence.as_object().cloned().unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "summary": self.summary,
            "evidence": Value::Object(self.evidence.clone()),
        })
    }
}

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
    windows_checks::run()
}

#[cfg(not(windows))]
fn platform_checks() -> Vec<Check> {
    vec![Check::new(
        "platform.supported",
        State::Unavailable,
        "Desktop automation currently requires Windows; this platform has no driver.",
        json!({"platform": canonical_platform()}),
    )]
}

#[cfg(windows)]
mod windows_checks {
    use super::{Check, State};
    use serde_json::json;
    use windows_sys::Win32::Graphics::Gdi::{
        GetDC, GetDeviceCaps, ReleaseDC, DESKTOPHORZRES, HORZRES,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CMONITORS, SM_CXSCREEN, SM_CYSCREEN, SM_REMOTESESSION,
    };

    pub fn run() -> Vec<Check> {
        vec![display(), remote_session(), scaling(), uia(), integrity()]
    }

    /// Whether an addressable desktop surface exists at all.
    fn display() -> Check {
        let (width, height, monitors) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
                GetSystemMetrics(SM_CMONITORS),
            )
        };
        let evidence = json!({
            "screen_width": width,
            "screen_height": height,
            "monitor_count": monitors,
        });

        if width > 0 && height > 0 {
            Check::new(
                "windows.display",
                State::Available,
                "A desktop surface with a non-zero extent is advertised.",
                evidence,
            )
        } else {
            Check::new(
                "windows.display",
                State::Unavailable,
                "No desktop surface with a usable extent is advertised to this process.",
                evidence,
            )
        }
    }

    /// A remote session can work, but input and capture behave differently.
    fn remote_session() -> Check {
        let remote = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
        let evidence = json!({"remote_session": remote});

        if remote {
            Check::new(
                "windows.session",
                State::Degraded,
                "This is a remote desktop session; input injection and capture may \
behave differently, and a disconnected session has no visible desktop.",
                evidence,
            )
        } else {
            Check::new(
                "windows.session",
                State::Available,
                "This is a local console session.",
                evidence,
            )
        }
    }

    /// DPI virtualisation makes reported coordinates disagree with real pixels.
    fn scaling() -> Check {
        let (logical, physical) = unsafe {
            let dc = GetDC(std::ptr::null_mut());
            if dc.is_null() {
                (0, 0)
            } else {
                let logical = GetDeviceCaps(dc, HORZRES as i32);
                let physical = GetDeviceCaps(dc, DESKTOPHORZRES as i32);
                ReleaseDC(std::ptr::null_mut(), dc);
                (logical, physical)
            }
        };
        let evidence = json!({
            "logical_width": logical,
            "physical_width": physical,
            "virtualized": logical > 0 && physical > 0 && logical != physical,
        });

        if logical <= 0 || physical <= 0 {
            return Check::new(
                "windows.dpi",
                State::Unknown,
                "Display scaling could not be determined.",
                evidence,
            );
        }
        if logical != physical {
            return Check::new(
                "windows.dpi",
                State::Degraded,
                "The process sees virtualized coordinates; screen positions will not \
match physical pixels unless the process is DPI aware.",
                evidence,
            );
        }
        Check::new(
            "windows.dpi",
            State::Available,
            "Reported coordinates match physical pixels.",
            evidence,
        )
    }

    /// Whether the UI Automation client stack can be instantiated.
    fn uia() -> Check {
        // This is the one prerequisite worth confirming directly, because the
        // whole driver depends on it and creating a client object is a purely
        // read-only operation that changes nothing on the desktop.
        if check_uia_available() {
            Check::new(
                "windows.uia",
                State::Available,
                "The UI Automation client stack is available to this process.",
                json!({"client_created": true}),
            )
        } else {
            Check::new(
                "windows.uia",
                State::Unavailable,
                "The UI Automation client stack could not be created.",
                json!({"client_created": false}),
            )
        }
    }

    fn check_uia_available() -> bool {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
        };
        use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};

        unsafe {
            // COM apartments are per-thread; initialise the one we are on.
            // "Already initialised" is a success for a read-only probe.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            // The reference is dropped at the end of this scope, so the probe
            // owns nothing once it has answered.
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER).is_ok()
        }
    }

    /// Integrity level governs which windows this process may automate.
    fn integrity() -> Check {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_QUERY};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let elevated = unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                None
            } else {
                let mut elevation: u32 = 0;
                let mut returned: u32 = 0;
                let ok = GetTokenInformation(
                    token,
                    TokenElevation,
                    &mut elevation as *mut u32 as *mut _,
                    std::mem::size_of::<u32>() as u32,
                    &mut returned,
                );
                CloseHandle(token);
                (ok != 0).then_some(elevation != 0)
            }
        };

        let evidence = json!({"elevated": elevated});
        match elevated {
            // A non-elevated process cannot automate an elevated window, which
            // is a real and frequently surprising limit.
            Some(false) => Check::new(
                "windows.integrity",
                State::Degraded,
                "This process is not elevated; windows owned by elevated processes \
cannot be inspected or controlled.",
                evidence,
            ),
            Some(true) => Check::new(
                "windows.integrity",
                State::Available,
                "This process is elevated and can reach windows at its own level or below.",
                evidence,
            ),
            None => Check::new(
                "windows.integrity",
                State::Unknown,
                "The process elevation state could not be determined.",
                evidence,
            ),
        }
    }
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

    #[cfg(not(windows))]
    #[test]
    fn an_unsupported_platform_says_so_plainly() {
        let report = probe();
        let check = report.get("platform.supported").expect("a platform check");

        assert_eq!(check.state, State::Unavailable);
    }
}
