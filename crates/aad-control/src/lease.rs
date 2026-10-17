//! Cross-process input ownership for cooperating control servers.
//! These files contain no credentials and are never removed while locking:
//! unlinking an active lock would let another process lock a different inode.
use crate::{error, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub struct InputLease {
    _file: File,
}
impl InputLease {
    pub fn acquire(key: &str) -> Result<Self> {
        Self::in_directory(&directory(), key)
    }
    fn in_directory(directory: &Path, key: &str) -> Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
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
        let path = directory.join(format!("{:x}.lock", Sha256::digest(key.as_bytes())));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(error(
                "CONTROL.LEASE_UNAVAILABLE",
                "lease file must not be a symlink",
            ));
        }
        let file = options
            .open(path)
            .map_err(|e| error("CONTROL.LEASE_UNAVAILABLE", e.to_string()))?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_| {
            error(
                "CONTROL.RESOURCE_BUSY",
                "another control session owns this input resource; close that session first",
            )
        })?;
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
    fn an_input_lease_is_exclusive_until_its_last_guard_is_dropped() {
        let root = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
        let lease = std::sync::Arc::new(InputLease::in_directory(&root, "desktop").unwrap());
        let active = lease.clone();
        assert!(InputLease::in_directory(&root, "desktop").is_err());
        drop(lease);
        assert!(InputLease::in_directory(&root, "desktop").is_err());
        drop(active);
        assert!(InputLease::in_directory(&root, "desktop").is_ok());
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
}
