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

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandOutcome {
    Ok,
    Nonzero,
    Timeout,
    Error,
    NotRun,
}

#[cfg(target_os = "linux")]
impl CommandOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Nonzero => "nonzero",
            Self::Timeout => "timeout",
            Self::Error => "error",
            Self::NotRun => "not_run",
        }
    }
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
    let mut checks = windows_checks::run();
    checks.push(script_sandbox_check());
    checks
}

#[cfg(target_os = "linux")]
fn platform_checks() -> Vec<Check> {
    let mut checks = linux_checks::run();
    checks.push(script_sandbox_check());
    checks
}

#[cfg(target_os = "macos")]
fn platform_checks() -> Vec<Check> {
    let mut checks = macos_checks::run();
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

#[cfg(target_os = "linux")]
fn fixed_command(name: &str) -> Option<PathBuf> {
    let roots = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    roots
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn script_sandbox_check() -> Check {
    #[cfg(target_os = "linux")]
    {
        let bubblewrap = fixed_command("bwrap").is_some();
        let prlimit = fixed_command("prlimit").is_some();
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

#[cfg(target_os = "linux")]
mod linux_checks {
    use super::{fixed_command, Check, CommandOutcome, State};
    use serde_json::json;
    use std::io::Read;
    use std::os::unix::fs::FileTypeExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    pub fn run() -> Vec<Check> {
        vec![atspi(), x11(), wayland(), portal(), libei(), uinput()]
    }

    fn sanitized(command: &Path, args: &[&str], inherited: &[&str]) -> CommandOutcome {
        let mut command = Command::new(command);
        command
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for name in inherited {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return CommandOutcome::Error,
        };
        let stdout = child.stdout.take().expect("probe stdout is piped");
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            let _ = stdout.take(64 * 1024 + 1).read_to_end(&mut output);
            output
        });
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = reader.join().unwrap_or_default();
                    return if status.success() && !output.is_empty() && output.len() <= 64 * 1024 {
                        CommandOutcome::Ok
                    } else {
                        CommandOutcome::Nonzero
                    };
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return CommandOutcome::Timeout;
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = reader.join();
                    return CommandOutcome::Error;
                }
            }
        }
    }

    fn atspi() -> Check {
        let address = std::env::var_os("AT_SPI_BUS_ADDRESS").is_some();
        let session = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
        let gdbus = fixed_command("gdbus");
        let outcome = if let (Some(command), true) = (gdbus.as_deref(), session) {
            sanitized(
                command,
                &[
                    "call",
                    "--session",
                    "--dest",
                    "org.a11y.Bus",
                    "--object-path",
                    "/org/a11y/bus",
                    "--method",
                    "org.a11y.Bus.GetAddress",
                ],
                &["DBUS_SESSION_BUS_ADDRESS"],
            )
        } else {
            CommandOutcome::NotRun
        };
        let (state, summary) = match outcome {
            CommandOutcome::Ok => (
                State::Available,
                "The session AT-SPI bus returned an address.",
            ),
            CommandOutcome::Timeout | CommandOutcome::Error if session => (
                State::Unknown,
                "The AT-SPI bus query could not be completed.",
            ),
            _ if address => (
                State::Degraded,
                "An AT-SPI address is advertised but could not be queried.",
            ),
            _ => (State::Unavailable, "No usable AT-SPI bus is advertised."),
        };
        Check::new(
            "linux.at_spi",
            state,
            summary,
            json!({"address_advertised": address, "session_bus_advertised": session, "gdbus_found": gdbus.is_some(), "query": outcome.as_str()}),
        )
    }

    fn x11() -> Check {
        let display = std::env::var_os("DISPLAY").is_some();
        let xprop = fixed_command("xprop");
        let outcome = if let (Some(command), true) = (xprop.as_deref(), display) {
            sanitized(
                command,
                &["-root", "_NET_SUPPORTING_WM_CHECK"],
                &["DISPLAY", "XAUTHORITY"],
            )
        } else {
            CommandOutcome::NotRun
        };
        let (state, summary) = match outcome {
            CommandOutcome::Ok => (
                State::Available,
                "The advertised X11 display answered a bounded metadata query.",
            ),
            _ if !display => (
                State::Unavailable,
                "No X11 display is advertised to this process.",
            ),
            CommandOutcome::Timeout | CommandOutcome::Error => {
                (State::Unknown, "The X11 query could not be completed.")
            }
            CommandOutcome::NotRun => (
                State::Degraded,
                "An X11 display is advertised, but no trusted query tool was found.",
            ),
            CommandOutcome::Nonzero => (
                State::Degraded,
                "The advertised X11 display did not answer the query.",
            ),
        };
        Check::new(
            "linux.x11",
            state,
            summary,
            json!({"display_advertised": display, "xprop_found": xprop.is_some(), "query": outcome.as_str()}),
        )
    }

    fn wayland() -> Check {
        let display = std::env::var_os("WAYLAND_DISPLAY");
        let runtime = std::env::var_os("XDG_RUNTIME_DIR");
        let endpoint = display.as_ref().map(Path::new).map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                runtime
                    .as_ref()
                    .map(Path::new)
                    .unwrap_or(Path::new(""))
                    .join(path)
            }
        });
        let socket = endpoint
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.file_type().is_socket());
        Check::new(
            "linux.wayland",
            if socket {
                State::Available
            } else if display.is_some() {
                State::Degraded
            } else {
                State::Unavailable
            },
            if socket {
                "The advertised Wayland endpoint is a socket."
            } else {
                "No usable Wayland endpoint was confirmed."
            },
            json!({"display_advertised": display.is_some(), "runtime_dir_advertised": runtime.is_some(), "socket_confirmed": socket}),
        )
    }

    fn portal() -> Check {
        let session = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
        let gdbus = fixed_command("gdbus");
        let outcome = if let (Some(command), true) = (gdbus.as_deref(), session) {
            sanitized(
                command,
                &[
                    "call",
                    "--session",
                    "--dest",
                    "org.freedesktop.portal.Desktop",
                    "--object-path",
                    "/org/freedesktop/portal/desktop",
                    "--method",
                    "org.freedesktop.DBus.Properties.Get",
                    "org.freedesktop.portal.RemoteDesktop",
                    "version",
                ],
                &["DBUS_SESSION_BUS_ADDRESS"],
            )
        } else {
            CommandOutcome::NotRun
        };
        let (state, summary) = match outcome {
            CommandOutcome::Ok => (
                State::Available,
                "The RemoteDesktop portal interface is exposed; authorization was not requested.",
            ),
            _ if !session => (
                State::Unavailable,
                "No D-Bus session address is available for the portal.",
            ),
            _ => (
                State::Unknown,
                "The RemoteDesktop portal interface could not be verified.",
            ),
        };
        Check::new(
            "linux.remote_desktop_portal",
            state,
            summary,
            json!({"gdbus_found": gdbus.is_some(), "session_bus_advertised": session, "query": outcome.as_str(), "permission_requested": false, "session_created": false}),
        )
    }

    fn libei() -> Check {
        let library = fixed_library("ei");
        let oeffis = fixed_library("oeffis");
        let tools =
            fixed_command("ei-debug-events").is_some() || fixed_command("ei-demo").is_some();
        Check::new(
            "linux.libei",
            if library {
                State::Available
            } else if tools || oeffis {
                State::Degraded
            } else {
                State::Unavailable
            },
            if library {
                "The libei client library is discoverable; no compositor connection was attempted."
            } else if tools || oeffis {
                "libei tooling is present; no compositor connection was attempted."
            } else {
                "No known libei diagnostic command was found."
            },
            json!({"libei_found": library, "liboeffis_found": oeffis, "tooling_found": tools, "connection_attempted": false}),
        )
    }

    fn uinput() -> Check {
        let path = ["/dev/uinput", "/dev/input/uinput"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.exists());
        let character = path
            .and_then(|path| std::fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.file_type().is_char_device());
        let writable = path.is_some_and(|path| {
            std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                .ok()
                .is_some_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0)
        });
        Check::new(
            "linux.uinput",
            if character && writable {
                State::Available
            } else if path.is_some() {
                State::Degraded
            } else {
                State::Unavailable
            },
            if character && writable {
                "A writable uinput character device is present; it was not opened."
            } else {
                "No writable uinput device is exposed to this process."
            },
            json!({"device_present": path.is_some(), "character_device": character, "writable_mode": writable, "libevdev_found": fixed_library("evdev"), "ydotool_found": fixed_command("ydotool").is_some(), "evemu_device_found": fixed_command("evemu-device").is_some(), "device_opened": false}),
        )
    }

    fn fixed_library(name: &str) -> bool {
        let prefix = format!("lib{name}.so");
        ["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
            .into_iter()
            .map(Path::new)
            .filter_map(|root| std::fs::read_dir(root).ok())
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| {
                let path = entry.path();
                let direct = entry.file_name().to_string_lossy().starts_with(&prefix);
                direct
                    || (path.is_dir()
                        && std::fs::read_dir(path).ok().is_some_and(|children| {
                            children.filter_map(Result::ok).any(|child| {
                                child.file_name().to_string_lossy().starts_with(&prefix)
                            })
                        }))
            })
    }
}

#[cfg(target_os = "macos")]
mod macos_checks {
    use super::{Check, State};
    use serde_json::json;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }

    pub fn run() -> Vec<Check> {
        let accessibility = unsafe { AXIsProcessTrusted() };
        let capture = unsafe { CGPreflightScreenCaptureAccess() };
        vec![
            Check::new(
                "macos.accessibility",
                if accessibility {
                    State::Available
                } else {
                    State::Unavailable
                },
                if accessibility {
                    "Accessibility trust is granted to this process identity."
                } else {
                    "Accessibility trust is not granted to this process identity."
                },
                json!({"preflight_completed": true, "authorized": accessibility, "prompt_requested": false}),
            ),
            Check::new(
                "macos.screen_capture",
                if capture {
                    State::Available
                } else {
                    State::Unavailable
                },
                if capture {
                    "Screen Capture permission is granted to this process identity."
                } else {
                    "Screen Capture permission is not granted to this process identity."
                },
                json!({"preflight_completed": true, "authorized": capture, "prompt_requested": false, "capture_attempted": false}),
            ),
        ]
    }
}

#[cfg(windows)]
mod windows_checks {
    use super::{Check, State};
    use serde_json::json;
    use windows_sys::Win32::Graphics::Gdi::{
        GetDC, GetDeviceCaps, ReleaseDC, DESKTOPHORZRES, HORZRES,
    };
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenElevation,
        TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::SystemServices::{
        SECURITY_MANDATORY_HIGH_RID, SECURITY_MANDATORY_LOW_RID, SECURITY_MANDATORY_MEDIUM_RID,
        SECURITY_MANDATORY_SYSTEM_RID,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::HiDpi::{
        GetProcessDpiAwareness, PROCESS_DPI_AWARENESS, PROCESS_DPI_UNAWARE,
        PROCESS_PER_MONITOR_DPI_AWARE, PROCESS_SYSTEM_DPI_AWARE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CMONITORS, SM_CXSCREEN, SM_CYSCREEN, SM_REMOTESESSION,
    };

    pub fn run() -> Vec<Check> {
        vec![
            display(),
            session(),
            input_desktop(),
            integrity(),
            scaling(),
            uia(),
        ]
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
    fn session() -> Check {
        use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;

        let remote = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
        let mut session_id = 0u32;
        let session_known =
            unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } != 0;
        let session_zero = session_known.then_some(session_id == 0);
        let evidence = json!({
            "session_id_available": session_known,
            "session_zero": session_zero,
            "interactive_session": session_known.then_some(session_id != 0),
            "remote_session": remote,
        });

        if session_zero == Some(true) {
            Check::new(
                "windows.session",
                State::Unavailable,
                "This process runs in Session 0, which has no interactive desktop.",
                evidence,
            )
        } else if !session_known {
            Check::new(
                "windows.session",
                State::Unknown,
                "The process session could not be determined.",
                evidence,
            )
        } else if remote {
            Check::new(
                "windows.session",
                State::Degraded,
                "This is a remote desktop session; input and capture can differ after disconnect.",
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

    fn input_desktop() -> Check {
        use windows_sys::Win32::Foundation::FALSE;
        use windows_sys::Win32::System::StationsAndDesktops::{
            CloseDesktop, GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW,
            OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
        };
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;

        unsafe fn object_name(handle: *mut std::ffi::c_void) -> Option<String> {
            if handle.is_null() {
                return None;
            }
            let mut needed = 0u32;
            GetUserObjectInformationW(handle, UOI_NAME, std::ptr::null_mut(), 0, &mut needed);
            if !(2..=4096).contains(&needed) {
                return None;
            }
            let mut buffer = vec![0u16; needed.div_ceil(2) as usize];
            if GetUserObjectInformationW(
                handle,
                UOI_NAME,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                return None;
            }
            let end = buffer
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(buffer.len());
            Some(String::from_utf16_lossy(&buffer[..end]))
        }

        let station = unsafe { object_name(GetProcessWindowStation().cast()) };
        let thread_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
        let thread_name = unsafe { object_name(thread_desktop) };
        let input = unsafe { OpenInputDesktop(0, FALSE, DESKTOP_READOBJECTS) };
        if input.is_null() {
            return Check::new("windows.input_desktop", State::Degraded, "The current input desktop cannot be opened; a secure desktop or another session may own input.", json!({"interactive_window_station": station.as_deref().is_some_and(|name| name.eq_ignore_ascii_case("WinSta0")), "input_desktop_readable": false, "thread_desktop_is_input_desktop": null}));
        }
        let input_name = unsafe { object_name(input) };
        unsafe { CloseDesktop(input) };
        let interactive = station
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("WinSta0"));
        let same = thread_name
            .as_ref()
            .zip(input_name.as_ref())
            .map(|(left, right)| left == right);
        let (state, summary) = if !interactive {
            (
                State::Unavailable,
                "This process is not on the interactive window station.",
            )
        } else if same == Some(true) {
            (
                State::Available,
                "This thread desktop is the current input desktop; no input was injected.",
            )
        } else {
            (
                State::Degraded,
                "This thread desktop could not be confirmed as the current input desktop.",
            )
        };
        Check::new(
            "windows.input_desktop",
            state,
            summary,
            json!({"interactive_window_station": interactive, "input_desktop_readable": true, "thread_desktop_is_input_desktop": same}),
        )
    }

    /// DPI virtualisation makes reported coordinates disagree with real pixels.
    fn scaling() -> Check {
        let mut awareness: PROCESS_DPI_AWARENESS = PROCESS_DPI_UNAWARE;
        let awareness_readable =
            unsafe { GetProcessDpiAwareness(std::ptr::null_mut(), &mut awareness) >= 0 };
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
        let quantisation = (logical > 0 && physical > 0)
            .then(|| (physical as f64 / logical as f64 * 10_000.0).round() / 10_000.0);
        let awareness_name = awareness_readable.then_some(match awareness {
            PROCESS_DPI_UNAWARE => "unaware",
            PROCESS_SYSTEM_DPI_AWARE => "system",
            PROCESS_PER_MONITOR_DPI_AWARE => "per_monitor",
            _ => "unknown",
        });
        let evidence = json!({
            "awareness_readable": awareness_readable,
            "awareness": awareness_name,
            "logical_width": logical,
            "physical_width": physical,
            "scaled_display": logical > 0 && physical > 0 && logical != physical,
            "pointer_quantisation": quantisation,
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
                "The process is not fully DPI aware on a scaled display, so pointer coordinates quantise physical pixels.",
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

        let (elevated, level) = unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                (None, None)
            } else {
                let mut elevation: u32 = 0;
                let mut returned: u32 = 0;
                let elevation_ok = GetTokenInformation(
                    token,
                    TokenElevation,
                    &mut elevation as *mut u32 as *mut _,
                    std::mem::size_of::<u32>() as u32,
                    &mut returned,
                );
                let mut required = 0u32;
                GetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    std::ptr::null_mut(),
                    0,
                    &mut required,
                );
                let level = if required == 0 || required > 4096 {
                    None
                } else {
                    let mut buffer = vec![0u8; required as usize];
                    if GetTokenInformation(
                        token,
                        TokenIntegrityLevel,
                        buffer.as_mut_ptr().cast(),
                        required,
                        &mut returned,
                    ) == 0
                    {
                        None
                    } else {
                        let label = &*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
                        let count = *GetSidSubAuthorityCount(label.Label.Sid) as u32;
                        if count == 0 {
                            None
                        } else {
                            let rid = *GetSidSubAuthority(label.Label.Sid, count - 1) as i32;
                            Some(if rid >= SECURITY_MANDATORY_SYSTEM_RID {
                                "system"
                            } else if rid >= SECURITY_MANDATORY_HIGH_RID {
                                "high"
                            } else if rid >= SECURITY_MANDATORY_MEDIUM_RID {
                                "medium"
                            } else if rid >= SECURITY_MANDATORY_LOW_RID {
                                "low"
                            } else {
                                "untrusted"
                            })
                        }
                    }
                };
                CloseHandle(token);
                ((elevation_ok != 0).then_some(elevation != 0), level)
            }
        };

        let evidence = json!({"token_readable": level.is_some(), "elevated": elevated, "integrity_level": level});
        match level {
            Some("untrusted" | "low") => Check::new("windows.integrity", State::Unavailable, "This process has low integrity, so UIPI blocks ordinary applications.", evidence),
            Some(level) => Check::new("windows.integrity", State::Available, &format!("This process runs at {level} integrity; UIPI still blocks higher-integrity applications."), evidence),
            None => Check::new("windows.integrity", State::Unknown, "The process integrity level could not be determined.", evidence),
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
