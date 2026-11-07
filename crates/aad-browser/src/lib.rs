//! Local browser protocol clients and owned-browser lifecycle.
//!
//! - [`transport`]: protocol-neutral loopback WebSocket framing and JSON
//!   request/reply multiplexing, shared by every protocol below;
//! - [`cdp`]: Chrome DevTools Protocol client for Chromium pages;
//! - [`bidi`]: persistent Firefox WebDriver BiDi session;
//! - [`launch`]: owned browser processes, profiles and profile leases;
//! - [`recording`]: passive page-activity recording.
//!
//! Each protocol keeps its own sessions, methods, capabilities and error
//! interpretation; nothing here falls back from one protocol to another.

pub mod bidi;
pub mod cdp;
mod connection;
pub mod launch;
pub mod recording;
mod tcp;
mod ws;

/// Protocol-neutral local WebSocket framing and JSON request/reply transport.
/// This does not construct or send protocol commands. Each protocol keeps its
/// own sessions, methods, capabilities and error interpretation.
pub mod transport {
    pub use crate::connection::{Connection, EventBatch};
    pub use crate::ws::{CdpError as TransportError, WebSocket};
}
