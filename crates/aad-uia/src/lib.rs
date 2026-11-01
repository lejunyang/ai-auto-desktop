//! Windows UI Automation: the [`aad_desktop::Backend`] for Windows, the
//! interaction recorder and the desktop-wide observer. Built on Windows only;
//! elsewhere this crate is empty.
//!
//! Everything above the backend -- the UI model, action dispatch, staleness
//! checks -- is `aad-desktop`, re-exported here.
#![cfg(windows)]

pub use aad_desktop::*;

pub mod observer_win;
pub mod windows;

use std::sync::Arc;

/// Build a driver on Windows UI Automation.
pub fn native_driver() -> Result<UiaDriver, DriverError> {
    native_driver_with_store(SnapshotStore::default())
}

/// Build a native driver with an explicit handle scope. Long-lived protocol
/// sessions use an in-memory store; standalone CLI calls keep the disk cache.
pub fn native_driver_with_store(snapshots: SnapshotStore) -> Result<UiaDriver, DriverError> {
    let backend = windows::WindowsUiaBackend::new()?;
    Ok(UiaDriver::with_store(Arc::new(backend), snapshots))
}
