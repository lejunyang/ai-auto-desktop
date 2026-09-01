//! Native UI Automation: application discovery, UI description and control.
//!
//! The crate is split so that only [`windows`] touches the operating system:
//!
//! - [`model`] — the platform-independent UI vocabulary (windows, nodes,
//!   locators, snapshot handles);
//! - [`backend`] — the trait an operating system must implement, plus the
//!   snapshot store that makes handles safe to hold;
//! - [`driver`] — action dispatch, staleness enforcement and the capability
//!   manifest, all of which are platform independent and tested everywhere.

pub mod backend;
pub mod driver;
pub mod model;

#[cfg(windows)]
pub mod windows;

pub use backend::{Backend, CaptureLimits, CapturedTree, DriverError, SnapshotStore};
pub use driver::{action_ids, is_node_action, is_write_action, UiaDriver, PROVIDER_NAME};
pub use model::{Bounds, Locator, Node, Snapshot, States, Target, WindowInfo};

use std::sync::Arc;

/// Build a driver on the current platform's native backend.
///
/// Returns a clear, actionable error on platforms with no backend rather than
/// silently offering a driver that cannot do anything.
pub fn native_driver() -> Result<UiaDriver, DriverError> {
    #[cfg(windows)]
    {
        let backend = windows::WindowsUiaBackend::new()?;
        Ok(UiaDriver::new(Arc::new(backend)))
    }
    #[cfg(not(windows))]
    {
        Err(DriverError::unavailable(
            "UI automation currently requires Windows; \
             no backend is available on this platform",
        ))
    }
}

/// Whether this build has a native backend for the current platform.
pub fn is_supported() -> bool {
    cfg!(windows)
}
