//! A lease on one native file panel attached to the selected browser window.

use crate::ax::helper::SwiftHelperBackend;
use aad_platform::picker::{Kind, NewFile};
use serde_json::{json, Value};
use std::{error::Error, path::Path, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

pub struct FilePicker {
    helper: SwiftHelperBackend,
    token: String,
    kind: Kind,
}

impl FilePicker {
    pub fn observe(
        pid: u32,
        expected: Option<u64>,
        bounds: [i32; 4],
        kind: Kind,
        deadline: Instant,
    ) -> Result<Self> {
        if expected.unwrap_or(0) != 0 {
            return Err("macOS AX sheets have no numeric dialog ID".into());
        }
        if pid == 0 || pid > i32::MAX as u32 || bounds[2] <= 0 || bounds[3] <= 0 {
            return Err("missing browser PID or window bounds".into());
        }
        let helper = SwiftHelperBackend::new_until(deadline)?;
        let operation = match kind {
            Kind::Open => "open",
            Kind::Save => "save",
        };
        let observed = helper.rpc(
            "picker_observe",
            json!({"process_id": pid, "bounds": bounds, "kind": operation}),
            deadline,
            false,
        )?;
        let token = observed["token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("missing native file panel lease")?
            .to_owned();
        Ok(Self {
            helper,
            token,
            kind,
        })
    }

    pub fn id(&self) -> u64 {
        0
    }

    pub fn process_only(&self) -> bool {
        false
    }

    pub fn revalidate(&self, deadline: Instant) -> Result<()> {
        self.check("picker_revalidate", json!({"token": self.token}), deadline)
    }

    pub fn dismiss(&self, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Open {
            return Err("only an observed Open panel can be dismissed after upload".into());
        }
        self.check("picker_dismiss", json!({"token": self.token}), deadline)
    }

    /// Select one file in an AppKit list view. AX must expose the visible
    /// row's absolute local URL or filename, and report the sole selected row.
    /// The caller supplies the resolved regular file within its upload grant.
    pub fn open_file(&self, path: &Path, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Open {
            return Err("expected an observed native Open panel".into());
        }
        if !path.is_absolute()
            || path.canonicalize()?.as_os_str() != path.as_os_str()
            || !path.symlink_metadata()?.is_file()
        {
            return Err("expected a resolved absolute path to a regular file".into());
        }
        self.check(
            "picker_open_file",
            json!({"token": self.token, "path": path.to_str().ok_or("open path is not UTF-8")?}),
            deadline,
        )
    }

    /// Save under the visible suggested extension in a directory whose full
    /// location the panel exposes. To navigate, the exact destination folder
    /// must be a visible row offering AXOpen. Otherwise select the directory
    /// in the panel before calling this method.
    pub fn save_new(&self, path: &Path, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Save {
            return Err("the observed panel is not a Save panel".into());
        }
        let parent = path.parent().ok_or("missing save directory")?;
        let destination = NewFile::under(parent, path)?;
        if destination.path() != path {
            return Err("expected the resolved absolute save path".into());
        }
        self.check(
            "picker_save_new",
            json!({"token": self.token, "path": path.to_str().ok_or("save path is not UTF-8")?}),
            deadline,
        )
    }

    fn check(&self, op: &str, args: Value, deadline: Instant) -> Result<()> {
        let result = self
            .helper
            .rpc(op, args, deadline, op != "picker_revalidate")?;
        if result["verified"] != true {
            return Err("could not verify the native file panel result".into());
        }
        Ok(())
    }
}
