//! Explicit RemoteDesktop authorization transport, not a Wayland input driver.
//! Portal device permission does not prove a target application's identity or
//! keyboard focus. No input events are exposed by this module.
mod mapping;
mod transport;

pub use mapping::{EiRegion, FrameGeometry, StreamMapping};
use std::collections::HashMap;
pub use transport::{AuthorizedSession, PortalClient};
use zbus::zvariant::{OwnedValue, Value};

type Dict = HashMap<String, OwnedValue>;
pub type Result<T> = std::result::Result<T, PortalError>;

/// Errors contain no portal text, restore token, request path or session path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalError {
    Cancelled,
    Deadline,
    Transport,
    Unsupported,
    InvalidResponse,
    Denied,
    Revoked,
    InvalidState,
    MappingUnproven,
}
impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "portal {:?}", self)
    }
}
impl std::error::Error for PortalError {}

/// Intentionally has no Default: a caller must explicitly request the dialog.
pub enum AuthorizationIntent {
    UserRequested,
}

/// The first implementation asks for a monitor stream, keyboard and pointer.
/// Persistence is disabled; restore tokens are never accepted, stored or logged.
pub struct AuthorizationOptions {
    pub parent_window: String,
}
impl AuthorizationOptions {
    pub fn unparented() -> Self {
        Self {
            parent_window: String::new(),
        }
    }
}

/// Opaque identity and revision of one authorization. Obtain it from the live
/// session; matching stream metadata cannot transfer a mapping to another grant.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionGeneration {
    session_id: uuid::Uuid,
    revision: u64,
}
impl SessionGeneration {
    fn new() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4(),
            revision: 1,
        }
    }
    fn invalidate(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedStream {
    pub node_id: u32,
    pub pipewire_serial: Option<u64>,
    pub mapping_id: Option<String>,
    pub source_type: u32,
    pub logical_origin: Option<(i32, i32)>,
    pub logical_size: Option<(i32, i32)>,
}

fn owned(value: Value<'_>) -> Result<OwnedValue> {
    value
        .try_to_owned()
        .map_err(|_| PortalError::InvalidResponse)
}

fn streams(results: &mut Dict) -> Result<Vec<AuthorizedStream>> {
    let raw = results
        .remove("streams")
        .ok_or(PortalError::InvalidResponse)?;
    let entries: Vec<(u32, Dict)> = raw.try_into().map_err(|_| PortalError::InvalidResponse)?;
    if entries.is_empty() || entries.len() > 16 {
        return Err(PortalError::InvalidResponse);
    }
    let mut nodes = std::collections::BTreeSet::new();
    let mut output = Vec::new();
    for (node_id, mut properties) in entries {
        if node_id == 0 || !nodes.insert(node_id) {
            return Err(PortalError::InvalidResponse);
        }
        let mut optional = |name: &str| properties.remove(name);
        let pipewire_serial = optional("pipewire-serial")
            .map(u64::try_from)
            .transpose()
            .map_err(|_| PortalError::InvalidResponse)?;
        let mapping_id = optional("mapping_id")
            .map(String::try_from)
            .transpose()
            .map_err(|_| PortalError::InvalidResponse)?;
        if pipewire_serial == Some(0)
            || mapping_id
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 256)
        {
            return Err(PortalError::InvalidResponse);
        }
        let source_type = optional("source_type")
            .map(u32::try_from)
            .transpose()
            .map_err(|_| PortalError::InvalidResponse)?
            .unwrap_or(0);
        let logical_origin = optional("position")
            .map(<(i32, i32)>::try_from)
            .transpose()
            .map_err(|_| PortalError::InvalidResponse)?;
        let logical_size = optional("size")
            .map(<(i32, i32)>::try_from)
            .transpose()
            .map_err(|_| PortalError::InvalidResponse)?;
        if logical_size.is_some_and(|(w, h)| w <= 0 || h <= 0) {
            return Err(PortalError::InvalidResponse);
        }
        output.push(AuthorizedStream {
            node_id,
            pipewire_serial,
            mapping_id,
            source_type,
            logical_origin,
            logical_size,
        });
    }
    Ok(output)
}
