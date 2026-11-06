//! Profile ownership uses an OS lock, released on process exit including crashes.
//! Keep the guard inode: unlinking it would let concurrent holders lock different files.
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{
    mpsc::{channel, RecvTimeoutError, Sender},
    Arc, Mutex,
};
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

const MARKER: &str = "aad-profile-lease-v1\n";
const SENTINEL: &str = ".aad-recording.lock";

pub(super) struct Lease {
    path: PathBuf,
    _guard: Guard,
}

struct Guard(File);

impl Drop for Guard {
    fn drop(&mut self) {
        // Release explicitly even if another thread is currently spawning a
        // child that briefly inherited this descriptor before exec.
        let _ = FileExt::unlock(&self.0);
    }
}

fn guard(path: &Path) -> super::Result<Guard> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.join(".aad-recording.guard"))?;
    file.try_lock_exclusive().map_err(|e| {
        if e.kind() == ErrorKind::WouldBlock {
            "profile is owned by an active AAD browser session; close that session first"
                .to_string()
        } else {
            format!("cannot check AAD profile ownership: {e}")
        }
    })?;
    Ok(Guard(file))
}

// Older versions used an empty sentinel and did not hold an OS lock. Reclaim
// those only when no other possible legacy owner remains; never guess a PID.
fn legacy_owner_running() -> bool {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    let own = Pid::from_u32(std::process::id());
    let exe = std::env::current_exe().ok();
    system.processes().iter().any(|(pid, p)| {
        if *pid == own || p.status() == ProcessStatus::Zombie || p.thread_kind().is_some() {
            return false;
        }
        let name = p
            .exe()
            .and_then(Path::file_name)
            .unwrap_or(p.name())
            .to_string_lossy()
            .to_ascii_lowercase();
        matches!(
            name.as_str(),
            "aad" | "aad.exe" | "aad-browser" | "aad-browser.exe"
        ) || exe.as_deref().is_some_and(|exe| p.exe() == Some(exe))
    })
}

/// Called only while holding the guard. New sentinels without that guard are
/// abandoned. Unknown legacy sentinels are preserved while an old owner may live.
fn reclaim(path: &Path) -> super::Result<()> {
    let sentinel = path.join(SENTINEL);
    let bytes = match fs::read(&sentinel) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if bytes != MARKER.as_bytes() && legacy_owner_running() {
        return Err("profile has a legacy AAD lease with a possible live owner; close other AAD browser sessions and retry so it can be recovered automatically".into());
    }
    fs::remove_file(sentinel)?;
    Ok(())
}

pub(super) fn check(path: &Path) -> super::Result<()> {
    let _guard = guard(path)?;
    reclaim(path)
}

/// A manually closed or explicitly terminated browser no longer owns its profile,
/// even if its MCP host remains connected. Weak handles do not keep either alive.
pub(super) fn watch_child(
    child: &Arc<Mutex<Child>>,
    lease: &Arc<Mutex<Option<Lease>>>,
) -> Sender<()> {
    let child = Arc::downgrade(child);
    let lease = Arc::downgrade(lease);
    let (stop, stopped) = channel();
    std::thread::spawn(move || {
        while matches!(
            stopped.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ) {
            let Some(child) = child.upgrade() else {
                break;
            };
            if child.lock().unwrap().try_wait().ok().flatten().is_some() {
                if let Some(lease) = lease.upgrade() {
                    lease.lock().unwrap().take();
                }
                break;
            }
        }
    });
    stop
}

impl Lease {
    pub(super) fn acquire(path: &Path) -> super::Result<Self> {
        let guard = guard(path)?;
        reclaim(path)?;
        let mut sentinel = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join(SENTINEL))?;
        let lease = Self {
            path: path.to_path_buf(),
            _guard: guard,
        };
        let result = sentinel.write_all(MARKER.as_bytes());
        drop(sentinel);
        result?;
        Ok(lease)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // The guard stays held until after removal, including on write failure.
        let _ = fs::remove_file(self.path.join(SENTINEL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn profile() -> PathBuf {
        let path = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn live_owner_is_preserved_and_normal_close_releases_the_profile() {
        let path = profile();
        let lease = Lease::acquire(&path).unwrap();
        assert!(check(&path).is_err());
        assert!(Lease::acquire(&path).is_err());
        drop(lease);
        assert!(!path.join(SENTINEL).exists());
        let next = Lease::acquire(&path).unwrap();
        drop(next);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn lease_subprocess() {
        let Some(path) = std::env::var_os("AAD_LEASE_CHILD_DIR") else {
            return;
        };
        let path = PathBuf::from(path);
        let _lease = if std::env::var_os("AAD_PROCESS_ONLY").is_none() {
            Some(Lease::acquire(&path).unwrap())
        } else {
            None
        };
        fs::write(path.join("ready"), b"ready").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn a_killed_owner_does_not_leave_a_blocking_lease() {
        let path = profile();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launch::lease::tests::lease_subprocess",
                "--nocapture",
            ])
            .env("AAD_LEASE_CHILD_DIR", &path)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.join("ready").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let ready = path.join("ready").exists();
        let blocked = ready && check(&path).is_err();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready && blocked);
        assert!(path.join(SENTINEL).exists());
        let recovered = Lease::acquire(&path).unwrap();
        drop(recovered);
        assert!(!path.join(SENTINEL).exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn browser_exit_releases_the_lease_while_its_host_stays_alive() {
        let path = profile();
        let lease = Arc::new(Mutex::new(Some(Lease::acquire(&path).unwrap())));
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launch::lease::tests::lease_subprocess",
                "--nocapture",
            ])
            .env("AAD_LEASE_CHILD_DIR", &path)
            .env("AAD_PROCESS_ONLY", "1")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let child = Arc::new(Mutex::new(child));
        let stop = watch_child(&child, &lease);
        assert!(Lease::acquire(&path).is_err());
        child.lock().unwrap().kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while lease.lock().unwrap().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(lease.lock().unwrap().is_none());
        let next = Lease::acquire(&path).unwrap();
        drop(next);
        drop(stop);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn an_unowned_legacy_sentinel_is_recovered() {
        let path = profile();
        fs::write(path.join(SENTINEL), b"").unwrap();
        // Other AAD processes on a developer's machine are real possible owners.
        if legacy_owner_running() {
            assert!(Lease::acquire(&path).is_err());
            assert!(path.join(SENTINEL).exists());
        } else {
            let lease = Lease::acquire(&path).unwrap();
            drop(lease);
            assert!(!path.join(SENTINEL).exists());
        }
        fs::remove_dir_all(path).unwrap();
    }
}
