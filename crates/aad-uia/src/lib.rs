//! Native UI Automation: application discovery, UI description and control.
//!
//! The crate is split so that only [`windows`] touches the operating system:
//!
//! - [`model`] — the platform-independent UI vocabulary (windows, nodes,
//!   locators, snapshot handles);
//! - [`backend`] — the trait an operating system must implement, plus the
//!   snapshot store that makes handles safe to hold;
//! - [`driver`] — action dispatch, staleness enforcement and the capability
//!   manifest, all of which are platform independent and tested everywhere;
//! - [`capture`] — turning observed interactions into recordable events, also
//!   platform independent.
//! - [`observe`] — long-running, system-wide ambient observation (foreground
//!   switches and incremental change triggers), platform independent apart
//!   from the native subscription in [`windows`].

pub mod backend;
pub mod capture;
pub mod driver;
pub mod model;
pub mod observe;
pub mod observe_session;
pub mod selector;

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub mod observer_win;

pub use backend::{Backend, CaptureLimits, CapturedTree, DriverError, SnapshotStore};
pub use capture::{coalesce, recordable, CapturedEvent, EventBuffer, EventKind};
pub use driver::{action_ids, is_node_action, is_write_action, UiaDriver, PROVIDER_NAME};
pub use model::{Bounds, Locator, Node, Snapshot, States, Target, WindowInfo};
pub use observe::{
    diff_trees, digest, triggers as fold_triggers, ChangeDigest, ChangeSet, ContentItem,
    DigestOptions, EventElement, EventWindow, ObservationBuffer, ObservationKind, ObservedEvent,
    TextChange, Trigger,
};

#[cfg(windows)]
use std::sync::Arc;

/// Build a driver on the current platform's native backend.
///
/// Returns a clear, actionable error on platforms with no backend rather than
/// silently offering a driver that cannot do anything.
pub fn native_driver() -> Result<UiaDriver, DriverError> {
    native_driver_with_store(SnapshotStore::default())
}

/// Build a native driver with an explicit handle scope. Long-lived protocol
/// sessions use an in-memory store; standalone CLI calls keep the disk cache.
pub fn native_driver_with_store(snapshots: SnapshotStore) -> Result<UiaDriver, DriverError> {
    #[cfg(windows)]
    {
        let backend = windows::WindowsUiaBackend::new()?;
        Ok(UiaDriver::with_store(Arc::new(backend), snapshots))
    }
    #[cfg(not(windows))]
    {
        let _ = snapshots;
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
