//! The desktop driver, apart from any operating system.
//!
//! An operating system's accessibility backend (Windows UI Automation in
//! `aad-uia`) implements [`Backend`]; everything above it lives here and is
//! tested everywhere:
//!
//! - [`model`] — the platform-independent UI vocabulary (windows, nodes,
//!   locators, snapshot handles);
//! - [`backend`] — the trait an operating system must implement, plus the
//!   snapshot store that makes handles safe to hold;
//! - [`driver`] — action dispatch, staleness enforcement and the capability
//!   manifest;
//! - [`capture`] — turning observed interactions into recordable events;
//! - [`observe`] — long-running, system-wide ambient observation (foreground
//!   switches and incremental change triggers), fed by the backend's native
//!   subscription.

pub mod backend;
pub mod capture;
pub mod driver;
pub mod model;
pub mod observe;
pub mod observe_session;
pub mod selector;

pub use backend::{Backend, CaptureLimits, CapturedTree, DriverError, Pixels, SnapshotStore};
pub use capture::{coalesce, recordable, CapturedEvent, EventBuffer, EventKind};
pub use driver::{action_ids, is_node_action, is_write_action, UiaDriver, PROVIDER_NAME};
pub use model::{Bounds, Locator, Node, Snapshot, States, Target, WindowInfo};
pub use observe::{
    diff_trees, digest, triggers as fold_triggers, ChangeDigest, ChangeSet, ContentItem,
    DigestOptions, EventElement, EventWindow, ObservationBuffer, ObservationKind, ObservedEvent,
    TextChange, Trigger,
};

/// The error for a system without a backend of this driver (all but
/// Windows: Linux and macOS have their own providers).
pub fn unavailable() -> DriverError {
    DriverError::unavailable(
        "UI automation currently requires Windows; \
         no backend is available on this platform",
    )
}
