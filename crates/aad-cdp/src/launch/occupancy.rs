//! Profile-scoped occupancy checks. Never terminate browsers by image name.
use super::{real_user_data, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use sysinfo::{
    Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System, UpdateKind,
};

fn refresh(system: &mut System) {
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
}

fn browser_name(name: &str, browser: &str) -> bool {
    let name = name.to_ascii_lowercase();
    match browser {
        "edge" => matches!(
            name.as_str(),
            "msedge" | "msedge.exe" | "microsoft edge" | "microsoft-edge"
        ),
        "chrome" => matches!(
            name.as_str(),
            "chrome" | "chrome.exe" | "google chrome" | "chromium" | "chromium-browser"
        ),
        "brave" => matches!(
            name.as_str(),
            "brave" | "brave.exe" | "brave browser" | "brave-browser"
        ),
        _ => false,
    }
}

fn switches(process: &Process) -> Vec<String> {
    // Chromium on Linux may rewrite argv into a single process title. Split
    // switch boundaries, not whitespace, so profile paths can contain spaces.
    if process.cmd().len() == 1 {
        return process.cmd()[0]
            .to_string_lossy()
            .split(" --")
            .skip(1)
            .map(|s| format!("--{s}"))
            .collect();
    }
    process
        .cmd()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn profile_argument(process: &Process, args: &[String]) -> Result<Option<PathBuf>> {
    for (index, arg) in args.iter().enumerate() {
        let value = if let Some(value) = arg.strip_prefix("--user-data-dir=") {
            Some(value.to_string())
        } else if arg == "--user-data-dir" {
            Some(
                args.get(index + 1)
                    .ok_or("browser user-data-dir argument is missing its value")?
                    .to_string(),
            )
        } else {
            arg.strip_prefix("--user-data-dir ").map(str::to_owned)
        };
        if let Some(value) = value {
            let mut path = PathBuf::from(value.trim_matches('"'));
            if path.is_relative() {
                path = process
                    .cwd()
                    .ok_or("cannot resolve browser's relative profile directory")?
                    .join(path);
            }
            return Ok(Some(path.canonicalize()?));
        }
    }
    Ok(None)
}

fn roots(system: &System, path: &Path, browser: &str) -> Result<Vec<Pid>> {
    let uid = system
        .process(Pid::from_u32(std::process::id()))
        .and_then(Process::user_id)
        .ok_or("cannot verify current process owner for profile occupancy check")?;
    let default = real_user_data(browser).and_then(|p| p.canonicalize().ok());
    let mut found = Vec::new();
    for (pid, process) in system.processes() {
        if process.status() == ProcessStatus::Zombie || process.thread_kind().is_some() {
            continue;
        }
        let name = process
            .exe()
            .and_then(Path::file_name)
            .unwrap_or(process.name())
            .to_string_lossy();
        if !browser_name(&name, browser) {
            continue;
        }
        if process.user_id().is_some_and(|owner| owner != uid) {
            continue;
        }
        let args = switches(process);
        if args
            .iter()
            .any(|s| s.starts_with("--type=") || s == "--type")
        {
            continue;
        }
        if process.cmd().is_empty() || process.exe().is_none() || process.user_id().is_none() {
            return Err(format!("cannot verify browser PID {pid}'s profile/owner; close it manually (even --kill-browser cannot select it safely)").into());
        }
        let directory = profile_argument(process, &args)?.or_else(|| default.clone());
        if directory.as_deref() == Some(path) {
            found.push(*pid);
        }
    }
    Ok(found)
}

#[derive(Clone)]
struct Identity {
    start: u64,
    exe: PathBuf,
}
fn identity(process: &Process) -> Option<Identity> {
    Some(Identity {
        start: process.start_time(),
        exe: process.exe()?.to_path_buf(),
    })
}
fn matches(process: &Process, id: &Identity) -> bool {
    process.status() != ProcessStatus::Zombie
        && process.start_time() == id.start
        && process.exe() == Some(id.exe.as_path())
}

/// Detect and optionally terminate only processes proven to own this data root.
/// Chromium's process lock spans all named profiles in the root, not just one.
pub fn ensure_available(path: &Path, browser: &str, kill: bool) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let path = path.canonicalize()?;
    let mut system = System::new();
    refresh(&mut system);
    let owners = roots(&system, &path, browser)?;
    if !owners.is_empty() && !kill {
        return Err(format!("profile source/working directory {} is occupied by browser PID(s) {}; close it or add --kill-browser to close all browser windows sharing this user-data directory and start again", path.display(), owners.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")).into());
    }
    if !owners.is_empty() {
        eprintln!("--kill-browser: closing browser PID(s) {} using {}; all profiles sharing this directory may close.", owners.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "), path.display());
        let mut tree: HashMap<Pid, Identity> = owners
            .iter()
            .filter_map(|pid| Some((*pid, identity(system.process(*pid)?)?)))
            .collect();
        loop {
            let before = tree.len();
            for (pid, process) in system.processes() {
                if process.thread_kind().is_some() {
                    continue;
                }
                if process
                    .parent()
                    .is_some_and(|parent| tree.contains_key(&parent))
                {
                    if let Some(id) = identity(process) {
                        tree.insert(*pid, id);
                    }
                }
            }
            if before == tree.len() {
                break;
            }
        }
        // Refresh before sending signals to guard against exited/reused PIDs.
        refresh(&mut system);
        let confirmed = roots(&system, &path, browser)?;
        for pid in &owners {
            if let Some(process) = system
                .process(*pid)
                .filter(|p| tree.get(pid).is_some_and(|id| matches(p, id)))
            {
                if !confirmed.contains(pid) {
                    return Err("browser ownership changed before termination; retry".into());
                }
                let _ = process.kill_with(Signal::Term);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            refresh(&mut system);
            let live = tree
                .iter()
                .filter_map(|(pid, id)| system.process(*pid).filter(|p| matches(p, id)))
                .collect::<Vec<_>>();
            if live.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(
                    "browser did not exit after --kill-browser; no new browser was launched".into(),
                );
            }
            if deadline.saturating_duration_since(Instant::now()) < Duration::from_secs(2) {
                for process in live {
                    let _ = process.kill();
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !roots(&system, &path, browser)?.is_empty() {
            return Err("another browser acquired the profile during shutdown; retry".into());
        }
        // A forced exit can leave Chromium's PID symlink behind. Remove only
        // the lock of an owner that this invocation verified and terminated.
        #[cfg(unix)]
        if let Ok(target) = std::fs::read_link(path.join("SingletonLock")) {
            let pid = target
                .to_string_lossy()
                .rsplit('-')
                .next()
                .and_then(|s| s.parse::<u32>().ok());
            if pid.is_some_and(|pid| owners.contains(&Pid::from_u32(pid))) {
                std::fs::remove_file(path.join("SingletonLock"))?;
            }
        }
        // An existing AAD recorder will observe the browser exit and release
        // its lease. Never delete someone else's live recorder lock ourselves.
        for _ in 0..50 {
            if !path.join(".aad-recording.lock").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if path.join(".aad-recording.lock").exists() {
        return Err("profile has an AAD recording lease; stop its recorder first, or use --kill-browser while its browser is running. A stale lease requires manual cleanup after checking the process".into());
    }
    #[cfg(unix)]
    if std::fs::symlink_metadata(path.join("SingletonLock")).is_ok() {
        return Err("profile has a browser lock but its owner cannot be verified; close it manually. --kill-browser never kills an unverified process".into());
    }
    if let Ok(contents) = std::fs::read_to_string(path.join("DevToolsActivePort")) {
        if contents
            .lines()
            .next()
            .and_then(|s| s.parse().ok())
            .is_some_and(crate::Client::available)
        {
            return Err("profile has an active CDP endpoint with no verified owner; attach with --page or close it manually".into());
        }
    }
    Ok(())
}
