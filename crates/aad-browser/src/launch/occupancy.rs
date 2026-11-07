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

/// One pass over the process table: the browsers proven to own `path`, or the
/// first browser-family process whose profile/owner could not be read.
enum Scan {
    Owners(Vec<Pid>),
    Unverified(Pid),
}

/// Browser processes that own `path`. A process caught while starting or
/// exiting briefly exposes no command line, executable or owner (on Windows
/// the system Edge and WebView2 hosts spawn and reap children constantly), so
/// rescan for a short while before treating it as an unverifiable owner.
fn roots(system: &mut System, path: &Path, browser: &str) -> Result<Vec<Pid>> {
    settle(|| scan(system, path, browser), std::thread::sleep)
}

const SETTLE_STEP: Duration = Duration::from_millis(100);
const SETTLE_STEPS: u32 = 20;

/// Repeat `scan` while some browser process is unverifiable, up to
/// `SETTLE_STEPS` waits. Only a process that stays unreadable that long is
/// reported; any other scan error is returned at once.
fn settle(
    mut scan: impl FnMut() -> Result<Scan>,
    mut wait: impl FnMut(Duration),
) -> Result<Vec<Pid>> {
    let mut waits = 0;
    loop {
        match scan()? {
            Scan::Owners(owners) => return Ok(owners),
            Scan::Unverified(pid) if waits >= SETTLE_STEPS => {
                return Err(format!("cannot verify browser PID {pid}'s profile/owner; close it manually (even --kill-browser cannot select it safely)").into());
            }
            Scan::Unverified(_) => {
                wait(SETTLE_STEP);
                waits += 1;
            }
        }
    }
}

fn scan(system: &mut System, path: &Path, browser: &str) -> Result<Scan> {
    refresh(system);
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
            return Ok(Scan::Unverified(*pid));
        }
        let directory = profile_argument(process, &args)?.or_else(|| default.clone());
        if directory.as_deref() == Some(path) {
            found.push(*pid);
        }
    }
    Ok(Scan::Owners(found))
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
    ensure_available_guarded(path, browser, kill, &super::Unrestricted)
}
pub(super) fn ensure_available_guarded(
    path: &Path,
    browser: &str,
    kill: bool,
    authority: &dyn super::Authority,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let path = path.canonicalize()?;
    let mut system = System::new();
    let owners = roots(&mut system, &path, browser)?;
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
        // Rescan (which refreshes) before sending signals to guard against
        // exited/reused PIDs.
        let confirmed = roots(&mut system, &path, browser)?;
        for pid in &owners {
            if let Some(process) = system
                .process(*pid)
                .filter(|p| tree.get(pid).is_some_and(|id| matches(p, id)))
            {
                if !confirmed.contains(pid) {
                    return Err("browser ownership changed before termination; retry".into());
                }
                authority.effect(&mut || {
                    let _ = process.kill_with(Signal::Term);
                    Ok(())
                })?;
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
                    authority.effect(&mut || {
                        let _ = process.kill();
                        Ok(())
                    })?;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !roots(&mut system, &path, browser)?.is_empty() {
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
        // Give the owning recorder time to finish. The OS releases its guard
        // even after a crash; file existence alone does not establish ownership.
        for _ in 0..50 {
            if super::lease::check(&path).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    super::lease::check(&path)?;
    // Windows counterpart of SingletonLock: a running Chromium holds
    // `lockfile` open sharing only reads, so a write open fails with a sharing
    // violation. A stale file left by a crash opens; nothing is written.
    #[cfg(windows)]
    if let Err(error) = std::fs::OpenOptions::new()
        .write(true)
        .open(path.join("lockfile"))
    {
        const ERROR_SHARING_VIOLATION: i32 = 32;
        if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
            return Err("profile has a browser lock but its owner cannot be verified; close it manually. --kill-browser never kills an unverified process".into());
        }
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
            .is_some_and(crate::cdp::Client::available)
        {
            return Err("profile has an active CDP endpoint with no verified owner; attach with --page or close it manually".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_process_caught_starting_or_exiting_is_rescanned_not_reported() {
        let scans = Cell::new(0);
        let waits = Cell::new(0);
        let owners = settle(
            || {
                scans.set(scans.get() + 1);
                Ok(if scans.get() < 3 {
                    Scan::Unverified(Pid::from_u32(7))
                } else {
                    Scan::Owners(vec![Pid::from_u32(42)])
                })
            },
            |_| waits.set(waits.get() + 1),
        )
        .unwrap();
        assert_eq!(owners, vec![Pid::from_u32(42)]);
        assert_eq!((scans.get(), waits.get()), (3, 2));
    }

    #[test]
    fn a_persistently_unreadable_process_still_blocks_the_profile() {
        let waits = Cell::new(0);
        let error = settle(
            || Ok(Scan::Unverified(Pid::from_u32(7))),
            |step| {
                assert_eq!(step, SETTLE_STEP);
                waits.set(waits.get() + 1);
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot verify browser PID 7"), "{error}");
        assert_eq!(waits.get(), SETTLE_STEPS);
    }

    /// Held exactly as Chromium holds it: write access, read sharing only.
    #[cfg(windows)]
    #[test]
    fn a_held_windows_lockfile_blocks_the_profile_until_released() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let path = std::env::temp_dir().join(format!("aad-lockfile-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(FILE_SHARE_READ)
            .open(path.join("lockfile"))
            .unwrap();
        let error = ensure_available(&path, "edge", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("browser lock"), "{error}");
        drop(held);
        // A stale lockfile, as a crashed browser leaves it, does not block.
        ensure_available(&path, "edge", false).unwrap();
        assert!(
            path.join("lockfile").exists(),
            "the check must not remove it"
        );
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn other_scan_errors_are_not_retried() {
        let waits = Cell::new(0);
        let error = settle(
            || Err("cannot verify current process owner".into()),
            |_| waits.set(waits.get() + 1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("current process owner"));
        assert_eq!(waits.get(), 0);
    }
}
