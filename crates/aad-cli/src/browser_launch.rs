//! Auto-launch of a debug-enabled Chromium browser for `aad watch --browser`.
//!
//! Resolves the `aad-browser` helper relative to the running `aad` executable
//! (they ship in the same directory), so it works both out of
//! `target/debug` and after install without relying on PATH.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A parsed `--browser` choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserLaunch {
    /// Browser id passed to aad-browser: edge / chrome / brave.
    pub browser: String,
    pub existing_profile: Option<Option<String>>,
    pub profile_source: Option<PathBuf>,
    pub profile_dir: Option<PathBuf>,
    pub kill_browser: bool,
}

impl BrowserLaunch {
    /// Browser family only; profile selection is independent.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        let browser = value;
        matches!(browser.as_str(), "edge" | "chrome" | "brave").then_some(Self {
            browser,
            existing_profile: None,
            profile_source: None,
            profile_dir: None,
            kill_browser: false,
        })
    }
}

/// Locate the `aad-browser` executable next to the current `aad` binary.
fn helper_exe() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop(); // drop aad.exe
    let candidate = path.join(if cfg!(windows) {
        "aad-browser.exe"
    } else {
        "aad-browser"
    });
    candidate.exists().then_some(candidate)
}

/// Launch the debug browser if it is not already reachable on `port`.
///
/// Returns true when a CDP endpoint should be available (either it already was
/// listening, or a browser was started and confirmed ready).
pub fn ensure_browser(port: u16, launch: &BrowserLaunch) -> bool {
    if aad_cdp::Client::available(port) {
        if launch.existing_profile.is_some() || launch.profile_dir.is_some() || launch.kill_browser
        {
            eprintln!("warning: CDP port already in use; cannot apply profile selection to it. Use another --cdp-port or browser-record start");
            return false;
        }
        return true;
    }
    let Some(helper) = helper_exe() else {
        eprintln!(
            "warning: --browser given but aad-browser helper not found next to aad; using UIA"
        );
        return false;
    };

    let mut command = Command::new(helper);
    command.arg("--port").arg(port.to_string());
    command.arg("--browser").arg(&launch.browser);
    command.arg("--detached");
    if let Some(id) = &launch.existing_profile {
        command.arg("--existing-profile");
        if let Some(id) = id {
            command.arg(id);
        }
    }
    if let Some(source) = &launch.profile_source {
        command.arg("--profile-source").arg(source);
    }
    if let Some(work) = &launch.profile_dir {
        command.arg("--profile-dir").arg(work);
    }
    if launch.kill_browser {
        command.arg("--kill-browser");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const FLAGS: u32 = 0x0000_0200 | 0x0000_0008;
        command.creation_flags(FLAGS);
    }

    match command.status() {
        Ok(status) if status.success() => aad_cdp::Client::available(port),
        Ok(status) => {
            eprintln!("warning: aad-browser exited with {status}; using UIA");
            false
        }
        Err(error) => {
            eprintln!("warning: could not launch aad-browser: {error}; using UIA");
            false
        }
    }
}

/// Best-effort: does the given path/file name look like this browser.
pub fn matches_process(launch: &BrowserLaunch, process_name: &str) -> bool {
    let process_name = process_name.to_ascii_lowercase();
    let needle = match launch.browser.as_str() {
        "edge" => "msedge.exe",
        "chrome" => "chrome.exe",
        "brave" => "brave.exe",
        _ => return false,
    };
    process_name == needle
}

/// Exposed for tests / callers that only need to resolve a launch spec.
pub fn parse(value: &str) -> Option<BrowserLaunch> {
    BrowserLaunch::parse(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_selection_does_not_imply_profile_selection() {
        assert_eq!(parse("edge").unwrap().browser, "edge");
        assert!(parse("edge").unwrap().existing_profile.is_none());
        assert_eq!(parse("Chrome").unwrap().browser, "chrome");
        assert_eq!(parse("Chrome-Login"), None);
        assert_eq!(parse("firefox"), None);
        assert!(matches_process(&parse("brave").unwrap(), "brave.exe"));
        assert!(!matches_process(&parse("edge").unwrap(), "chrome.exe"));
    }
}
