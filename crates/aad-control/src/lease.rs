//! Cross-process input ownership for cooperating control servers.
//! These files contain no credentials and are never removed while locking:
//! unlinking an active lock would let another process lock a different inode.
//! Owner metadata lives in a separate `.owner` file: Windows byte-range locks
//! are mandatory, so a contender could never read it from the locked file.
use crate::{error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    process_id: u32,
    session_id: Option<String>,
    acquired_at_ms: u64,
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

// Owner metadata is advisory: an older server may not write it, and the holder
// can change between a failed lock and this read. Never use it to break a lock.
fn owner_hint(path: &Path) -> Option<serde_json::Value> {
    if std::fs::symlink_metadata(path)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).ok()?;
    let mut data = Vec::new();
    file.take(1025).read_to_end(&mut data).ok()?;
    if data.len() > 1024 {
        return None;
    }
    let owner: Owner = serde_json::from_slice(&data).ok()?;
    if owner.process_id == 0
        || owner
            .session_id
            .as_deref()
            .is_some_and(|id| !valid_session_id(id))
    {
        return None;
    }
    serde_json::to_value(owner).ok()
}

fn lock_paths(directory: &Path, key: &str) -> (PathBuf, PathBuf) {
    let stem = format!("{:x}", Sha256::digest(key.as_bytes()));
    (
        directory.join(format!("{stem}.lock")),
        directory.join(format!("{stem}.owner")),
    )
}

fn open_private(path: &Path, truncate: bool) -> Result<File> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(error(
            "CONTROL.LEASE_UNAVAILABLE",
            "lease file must not be a symlink",
        ));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
        .open(path)
        .map_err(|e| error("CONTROL.LEASE_UNAVAILABLE", e.to_string()))
}

pub struct InputLease {
    _file: File,
}
impl InputLease {
    pub fn acquire(key: &str) -> Result<Self> {
        Self::in_directory(&directory(), key)
    }
    pub fn acquire_for(key: &str, session_id: &str) -> Result<Self> {
        Self::with_owner(&directory(), key, Some(session_id))
    }
    fn in_directory(directory: &Path, key: &str) -> Result<Self> {
        Self::with_owner(directory, key, None)
    }
    fn with_owner(directory: &Path, key: &str, session_id: Option<&str>) -> Result<Self> {
        if session_id.is_some_and(|id| !valid_session_id(id)) {
            return Err(error(
                "CONTROL.LEASE_UNAVAILABLE",
                "invalid lease session id",
            ));
        }
        #[cfg(unix)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(directory) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(error("CONTROL.LEASE_UNAVAILABLE", e.to_string())),
        }
        let metadata = std::fs::symlink_metadata(directory)
            .map_err(|e| error("CONTROL.LEASE_UNAVAILABLE", e.to_string()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(error(
                "CONTROL.LEASE_UNAVAILABLE",
                "lease directory must be a real private directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(error(
                    "CONTROL.LEASE_UNAVAILABLE",
                    "lease directory must be owned by this user with mode 0700",
                ));
            }
        }
        let (path, owner_path) = lock_paths(directory, key);
        let file = open_private(&path, false)?;
        if let Err(cause) = fs2::FileExt::try_lock_exclusive(&file) {
            if cause.raw_os_error() != fs2::lock_contended_error().raw_os_error() {
                return Err(error("CONTROL.LEASE_UNAVAILABLE", cause.to_string()));
            }
            let mut busy = error(
                "CONTROL.RESOURCE_BUSY",
                "another control session owns this input resource; close that session first",
            )
            .with_detail("owner_metadata_is_advisory", serde_json::json!(true));
            if let Some(owner) = owner_hint(&owner_path) {
                busy = busy.with_detail("owner", owner);
            }
            return Err(busy);
        }
        let owner = Owner {
            process_id: std::process::id(),
            session_id: session_id.map(str::to_owned),
            acquired_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        };
        let data = serde_json::to_vec(&owner)
            .map_err(|e| error("CONTROL.LEASE_UNAVAILABLE", e.to_string()))?;
        open_private(&owner_path, true)?
            .write_all(&data)
            .map_err(|e| error("CONTROL.LEASE_UNAVAILABLE", e.to_string()))?;
        Ok(Self { _file: file })
    }
}
fn directory() -> PathBuf {
    if let Some(path) = std::env::var_os("AAD_CONTROL_LEASE_DIR") {
        return PathBuf::from(path);
    }
    #[cfg(unix)]
    let suffix = unsafe { libc::geteuid() }.to_string();
    #[cfg(not(unix))]
    let suffix = "user".to_string();
    std::env::temp_dir().join(format!("aad-control-leases-{suffix}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contention_names_the_holder_and_reacquisition_replaces_old_metadata() {
        let root = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
        let first = InputLease::with_owner(&root, "desktop", Some("session-first")).unwrap();
        let busy = InputLease::with_owner(&root, "desktop", Some("session-waiting"))
            .err()
            .unwrap();
        assert_eq!(busy.code, "CONTROL.RESOURCE_BUSY");
        assert_eq!(busy.details["owner"]["session_id"], "session-first");
        assert_eq!(busy.details["owner"]["process_id"], std::process::id());
        assert!(busy.details["owner"]["acquired_at_ms"].as_u64().unwrap() > 0);
        assert_eq!(busy.details["owner_metadata_is_advisory"], true);
        drop(first);
        // A concurrently forked child can briefly retain the previous lock.
        drop(once_released(&root, "desktop").unwrap());
        let second = InputLease::with_owner(&root, "desktop", Some("session-second")).unwrap();
        let busy = InputLease::in_directory(&root, "desktop").err().unwrap();
        assert_eq!(busy.details["owner"]["session_id"], "session-second");
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unknown_or_corrupt_metadata_never_becomes_error_details() {
        let root = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
        let holder = InputLease::in_directory(&root, "desktop").unwrap();
        let (_, owner_path) = lock_paths(&root, "desktop");
        for bytes in [
            Vec::new(),
            vec![b'x'; 2000],
            br#"{"process_id":1,"session_id":"private text","acquired_at_ms":1}"#.to_vec(),
            br#"{"process_id":1,"session_id":"valid","acquired_at_ms":1,"secret":"hidden"}"#
                .to_vec(),
        ] {
            std::fs::write(&owner_path, &bytes).unwrap();
            let busy = InputLease::in_directory(&root, "desktop").err().unwrap();
            assert_eq!(busy.code, "CONTROL.RESOURCE_BUSY");
            assert!(!busy.details.contains_key("owner"));
        }
        drop(holder);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn an_input_lease_is_exclusive_until_its_last_guard_is_dropped() {
        let root = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
        let lease = std::sync::Arc::new(InputLease::in_directory(&root, "desktop").unwrap());
        let active = lease.clone();
        assert!(InputLease::in_directory(&root, "desktop").is_err());
        drop(lease);
        assert!(InputLease::in_directory(&root, "desktop").is_err());
        drop(active);
        assert!(once_released(&root, "desktop").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn distinct_explicit_scopes_do_not_share_a_lock() {
        let root = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
        let a = InputLease::in_directory(&root, "page-a").unwrap();
        let b = InputLease::in_directory(&root, "page-b").unwrap();
        drop((a, b));
        std::fs::remove_dir_all(root).unwrap();
    }
    /// A child forked while the lease is held (another test's launch::spawn,
    /// whose pre_exec makes std fork) shares the locked file description
    /// until it execs and O_CLOEXEC closes it.
    #[cfg(unix)]
    #[test]
    fn a_dropped_lease_is_free_once_a_child_forked_meanwhile_has_execed() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let root = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
        let lease = InputLease::in_directory(&root, "desktop").unwrap();
        let (mut forked, signal) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = signal.as_raw_fd();
        let mut child = std::process::Command::new("true");
        unsafe {
            child.pre_exec(move || {
                libc::write(fd, b"f".as_ptr().cast(), 1);
                libc::sleep(1);
                Ok(())
            });
        }
        let forker = std::thread::spawn(move || {
            let status = child.status().unwrap();
            drop(signal);
            status
        });
        forked.read_exact(&mut [0]).unwrap();
        drop(lease);
        assert!(InputLease::in_directory(&root, "desktop").is_err());
        assert!(once_released(&root, "desktop").is_ok());
        forker.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Tests run in parallel and some spawn processes, so a forked child may
    /// briefly share a dropped lease's lock; it is released when it execs.
    fn once_released(root: &Path, key: &str) -> Result<InputLease> {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match InputLease::in_directory(root, key) {
                Err(_) if std::time::Instant::now() < until => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                done => return done,
            }
        }
    }
}
