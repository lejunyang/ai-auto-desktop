//! Bind the native Save dialog to the same browser, page, and document as the
//! trusted input dispatched by the step. Native adapters observe and recheck
//! the dialog controls and the destination before accepting it.
use super::{error, Result, Session};
use aad_browser::cdp::Client;
use aad_core::AutomationError;
#[cfg(target_os = "linux")]
use aad_linux::browser_picker::FilePicker;
#[cfg(target_os = "macos")]
use aad_macos::browser_picker::FilePicker;
use aad_platform::picker::{Kind, NewFile};
use aad_runtime::CancellationToken;
#[cfg(windows)]
use aad_windows::browser_picker::FilePicker;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn refused(message: impl std::fmt::Display) -> AutomationError {
    error("STEP.SAVE_PICKER", message.to_string())
}

pub(super) fn destination(session: &Session, path: &str) -> Result<NewFile> {
    if session.policy.allow_input {
        for root in &session.policy.save_roots {
            if let Ok(file) = NewFile::under(root, Path::new(path)) {
                return Ok(file);
            }
        }
    }
    Err(error(
        "POLICY.DENIED",
        "Save As requires input permission and a new absolute filename below --allow-save-dir",
    ))
}

pub(super) struct Picker {
    browser: Client,
    page: Client,
    meta: Value,
    pid: u32,
    window: i64,
    bounds: [i32; 4],
    world: i64,
    // A concurrent session close can clear its lease map while the native
    // adapter is still confirming the dialog. Keep our own ownership then.
    _leases: Vec<Arc<crate::lease::InputLease>>,
}

fn window(browser: &mut Client, page: &Value) -> Result<(i64, [i32; 4])> {
    let info = browser
        .command("Browser.getWindowForTarget", json!({"targetId":page}))
        .map_err(refused)?;
    if info["bounds"]["windowState"] == "minimized" {
        return Err(refused("the observed browser window is minimized"));
    }
    let bound = |key: &str| {
        info["bounds"][key]
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| refused("missing browser window bounds"))
    };
    Ok((
        info["windowId"]
            .as_i64()
            .ok_or_else(|| refused("missing window id"))?,
        [
            bound("left")?,
            bound("top")?,
            bound("width")?,
            bound("height")?,
        ],
    ))
}

impl Picker {
    pub(super) fn watch(session: &Session, meta: &Value, deadline: Instant) -> Result<Self> {
        if meta["main_frame"] != true || meta["loader_id"].as_str().is_none_or(str::is_empty) {
            return Err(refused("Save As requires an observed main document"));
        }
        let port = session
            .browser_port()
            .ok_or_else(|| refused("missing Chromium connection"))?;
        let (mut browser, instance) =
            Client::browser_with_identity_until(port, deadline).map_err(refused)?;
        if meta["browser_instance_id"] != instance {
            return Err(refused("browser instance changed"));
        }
        let leases =
            session.input_leases(&[format!("browser-input:{instance}"), "desktop-input".into()])?;
        browser.set_deadline(deadline);
        let info = browser
            .command("SystemInfo.getProcessInfo", json!({}))
            .map_err(refused)?;
        let mut processes = info["processInfo"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["type"] == "browser");
        let pid = processes
            .next()
            .and_then(|p| p["id"].as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| refused("missing browser process"))?;
        if processes.next().is_some() {
            return Err(refused("ambiguous browser process"));
        }
        let (window, bounds) = window(&mut browser, &meta["page_id"])?;
        let target = Client::list_targets_until(port, deadline)
            .map_err(refused)?
            .into_iter()
            .find(|p| meta["page_id"] == p.id)
            .ok_or_else(|| refused("page closed"))?;
        let mut page = Client::attach_bare_until(&target, deadline).map_err(refused)?;
        page.set_deadline(deadline);
        page.filter_events(
            &[
                "Page.fileChooserOpened",
                "Page.frameNavigated",
                "Page.navigatedWithinDocument",
                "Page.frameDetached",
            ],
            &[],
        );
        page.command("Page.enable", json!({"enableFileChooserOpenedEvent":true}))
            .map_err(refused)?;
        let world = page
            .command(
                "Page.createIsolatedWorld",
                json!({"frameId":meta["frame_id"]}),
            )
            .map_err(refused)?["executionContextId"]
            .as_i64()
            .ok_or_else(|| refused("missing page context"))?;
        let mut watch = Self {
            browser,
            page,
            meta: meta.clone(),
            pid,
            window,
            bounds,
            world,
            _leases: leases,
        };
        watch.document()?;
        watch.owner(false)?;
        watch.events(false)?;
        Ok(watch)
    }

    fn document(&mut self) -> Result<()> {
        let tree = self
            .page
            .command("Page.getFrameTree", json!({}))
            .map_err(refused)?;
        let frame = &tree["frameTree"]["frame"];
        // The isolated world's child count includes out-of-process frames.
        let result = self.page.command("Runtime.evaluate", json!({"contextId":self.world,"expression":"window === top && length === 0","returnByValue":true})).map_err(refused)?;
        if frame["id"] != self.meta["frame_id"]
            || frame["loaderId"] != self.meta["loader_id"]
            || tree["frameTree"]["childFrames"]
                .as_array()
                .is_some_and(|frames| !frames.is_empty())
            || result["result"]["value"] != true
            || !result["exceptionDetails"].is_null()
        {
            return Err(refused("the observed single-frame document changed"));
        }
        Ok(())
    }

    fn owner(&mut self, process_only: bool) -> Result<()> {
        if window(&mut self.browser, &self.meta["page_id"])? != (self.window, self.bounds) {
            return Err(refused("the observed window changed"));
        }
        let targets = self
            .browser
            .command("Target.getTargets", json!({}))
            .map_err(refused)?;
        let mut selected = false;
        for page in targets["targetInfos"]
            .as_array()
            .ok_or_else(|| refused("missing browser targets"))?
            .iter()
            .filter(|p| p["type"] == "page")
        {
            if page["targetId"] == self.meta["page_id"] {
                selected = true;
            } else if process_only
                || self
                    .browser
                    .command(
                        "Browser.getWindowForTarget",
                        json!({"targetId":page["targetId"]}),
                    )
                    .map_err(refused)?["windowId"]
                    .as_i64()
                    .ok_or_else(|| refused("missing peer window id"))?
                    == self.window
            {
                return Err(refused("another tab could own this dialog"));
            }
        }
        if !selected {
            return Err(refused("the observed page closed"));
        }
        Ok(())
    }

    fn events(&self, allow_chooser: bool) -> Result<bool> {
        let batch = self.page.drain_events();
        if batch.failure.is_none() && batch.dropped == 0 {
            match batch.events.as_slice() {
                [] => return Ok(false),
                [event]
                    if allow_chooser
                        && event["method"] == "Page.fileChooserOpened"
                        && event["params"]["frameId"] == self.meta["frame_id"]
                        && event["params"]["mode"] == "selectSingle"
                        && event["params"]["backendNodeId"].is_null() =>
                {
                    return Ok(true)
                }
                _ => (),
            }
        }
        Err(refused(
            "could not attribute one native file chooser to the recorded trigger",
        ))
    }

    pub(super) fn select(
        &mut self,
        token: &CancellationToken,
        destination: &NewFile,
        deadline: Instant,
    ) -> Result<()> {
        let check = || {
            token.check()?;
            if Instant::now() >= deadline {
                return Err(refused("timed out waiting for the observed Save dialog"));
            }
            Ok(())
        };
        while !self.events(true)? {
            check()?;
            std::thread::sleep(Duration::from_millis(25));
        }
        let picker = loop {
            check()?;
            self.events(false)?;
            match FilePicker::observe(self.pid, None, self.bounds, Kind::Save, deadline) {
                Ok(dialog) => break dialog,
                Err(err) if Instant::now() + Duration::from_millis(75) >= deadline => {
                    return Err(refused(err))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(75)),
            }
        };
        self.owner(picker.process_only())?;
        self.document()?;
        if window(&mut self.browser, &self.meta["page_id"])? != (self.window, self.bounds) {
            return Err(refused(
                "the page moved or changed bounds during picker validation",
            ));
        }
        self.events(false)?;
        picker.revalidate(deadline).map_err(refused)?;
        destination.revalidate().map_err(refused)?;
        check()?;
        picker
            .save_new(destination.path(), deadline)
            .map_err(refused)?;
        Ok(())
    }
}
