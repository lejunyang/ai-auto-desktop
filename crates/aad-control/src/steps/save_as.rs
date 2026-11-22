//! Bind the native Save dialog to the same browser, page, and document as the
//! trusted input dispatched by the step. Native adapters observe and recheck
//! the dialog controls and the destination before accepting it.
use super::{error, Result, Session};
use aad_browser::{
    cdp::Client,
    picker::{self, Download},
};
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
    download: bool,
    // A concurrent session close can clear its lease map while the native
    // adapter is still confirming the dialog. Keep our own ownership then.
    _leases: Vec<Arc<crate::lease::InputLease>>,
}

impl Picker {
    pub(super) fn watch(
        session: &Session,
        meta: &Value,
        deadline: Instant,
        download: bool,
    ) -> Result<Self> {
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
        let pid = picker::pid(&mut browser).map_err(refused)?;
        let (window, bounds) = picker::window(
            &mut browser,
            meta["page_id"]
                .as_str()
                .ok_or_else(|| refused("missing page id"))?,
        )
        .map_err(refused)?;
        let target = Client::list_targets_until(port, deadline)
            .map_err(refused)?
            .into_iter()
            .find(|p| meta["page_id"] == p.id)
            .ok_or_else(|| refused("page closed"))?;
        let mut page = Client::attach_bare_until(&target, deadline).map_err(refused)?;
        page.set_deadline(deadline);
        let mut events = vec![
            "Page.fileChooserOpened",
            "Page.frameNavigated",
            "Page.navigatedWithinDocument",
            "Page.frameDetached",
        ];
        if download {
            events.extend(["Page.downloadWillBegin", "Page.downloadProgress"]);
        }
        page.filter_events(&events, &[]);
        page.command("Page.enable", json!({"enableFileChooserOpenedEvent":true}))
            .map_err(refused)?;
        let mut world_params = json!({"frameId":meta["frame_id"]});
        if download {
            world_params["worldName"] = json!(format!("aad-download-{}", uuid::Uuid::new_v4()));
        }
        let world = page
            .command("Page.createIsolatedWorld", world_params)
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
            download,
            _leases: leases,
        };
        if download {
            watch.evaluate(&aad_browser::cdp::observer_script(""))?;
        }
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
        picker::verify_owner(
            &mut self.browser,
            self.meta["page_id"].as_str().unwrap_or_default(),
            (self.window, self.bounds),
            process_only,
        )
        .map_err(refused)
    }

    fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let r = self
            .page
            .command(
                "Runtime.evaluate",
                json!({"contextId":self.world,"expression":expression,"returnByValue":true}),
            )
            .map_err(refused)?;
        if r.get("exceptionDetails").is_some() {
            return Err(refused("download trigger document changed"));
        }
        Ok(r["result"]["value"].clone())
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
        if self.download {
            return self.select_download(token, destination, deadline);
        }
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
        self.document()?;
        self.owner(picker.process_only())?;
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

#[derive(Default)]
struct Transfer {
    download: Option<Download>,
    bytes: Option<u64>,
}

impl Picker {
    fn download_events(&self, transfer: &mut Transfer, href: &str) -> Result<()> {
        let batch = self.page.drain_events();
        if batch.dropped != 0 || batch.failure.is_some() {
            return Err(refused("download events were lost"));
        }
        for event in batch.events {
            let p = &event["params"];
            match event["method"].as_str() {
                Some("Page.downloadWillBegin") if transfer.download.is_none() => {
                    transfer.download = Some(
                        Download::from_begin(p, self.meta["frame_id"].as_str().unwrap_or(""), href)
                            .ok_or_else(|| {
                                refused("download frame or URL differs from the trusted link")
                            })?,
                    );
                }
                Some("Page.downloadProgress")
                    if transfer
                        .download
                        .as_ref()
                        .is_some_and(|d| p["guid"] == d.guid) =>
                {
                    match p["state"].as_str() {
                        Some("inProgress") => (),
                        Some("completed") => {
                            transfer.bytes = Some(
                                p["receivedBytes"]
                                    .as_u64()
                                    .ok_or_else(|| refused("missing download byte count"))?,
                            );
                        }
                        _ => return Err(refused("download canceled or interrupted")),
                    }
                }
                _ => {
                    return Err(refused(
                        "ambiguous download, chooser or document transition",
                    ))
                }
            }
        }
        Ok(())
    }

    fn select_download(
        &mut self,
        token: &CancellationToken,
        destination: &NewFile,
        deadline: Instant,
    ) -> Result<()> {
        let report = self.evaluate("globalThis.__aadCdp.takeReport()")?;
        let events = report["events"]
            .as_array()
            .ok_or_else(|| refused("missing trusted download trigger"))?;
        let click = match events.as_slice() {
            [click] => click,
            [enter, click]
                if enter["kind"] == "enter"
                    && enter["trusted"] == true
                    && enter["node_id"] == click["node_id"]
                    && enter["sequence"] == click["enter_sequence"] =>
            {
                click
            }
            _ => return Err(refused("ambiguous download activation")),
        };
        if report["dropped"] != 0
            || click["trusted"] != true
            || click["kind"] != "click"
            || click["download_link"] != true
        {
            return Err(refused("expected a single trusted download link"));
        }
        let href = click["href"]
            .as_str()
            .ok_or_else(|| refused("missing download link URL"))?;
        let check = || {
            token.check()?;
            if Instant::now() >= deadline {
                return Err(refused("timed out waiting for download"));
            }
            Ok(())
        };
        let mut transfer = Transfer::default();
        let dialog = loop {
            check()?;
            self.download_events(&mut transfer, href)?;
            if transfer.bytes.is_some() {
                return Err(refused(
                    "download completed without an observed Save dialog",
                ));
            }
            if let Some(download) = &transfer.download {
                if let Ok(dialog) =
                    picker::download_dialog(self.pid, self.bounds, &download.suggestion, deadline)
                {
                    break dialog;
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        };
        self.document()?;
        self.owner(dialog.process_only())?;
        let latest = self.evaluate("globalThis.__aadCdp.takeReport()")?;
        if latest["dropped"] != 0
            || latest["events"]
                .as_array()
                .is_none_or(|events| !events.is_empty())
        {
            return Err(refused("another action followed the trusted download link"));
        }
        self.download_events(&mut transfer, href)?;
        if transfer.bytes.is_some() {
            return Err(refused("download ended before native selection"));
        }
        dialog
            .verify_suggestion(&transfer.download.as_ref().unwrap().suggestion, deadline)
            .map_err(refused)?;
        destination.revalidate().map_err(refused)?;
        check()?;
        dialog
            .save_new(destination.path(), deadline)
            .map_err(refused)?;
        loop {
            check()?;
            self.download_events(&mut transfer, href)?;
            if let Some(size) = transfer.bytes {
                let stat = destination.path().symlink_metadata().map_err(refused)?;
                if !stat.is_file()
                    || stat.len() != size
                    || destination.path().canonicalize().map_err(refused)? != destination.path()
                {
                    return Err(refused(
                        "the selected destination does not match the completed download",
                    ));
                }
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        if self.download {
            self.page
                .set_deadline(Instant::now() + Duration::from_millis(300));
            let _ = self.evaluate("globalThis.__aadCdp?.stop()");
        }
    }
}
