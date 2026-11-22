//! Attribute native Save dialogs to the CDP browser process and window.
use crate::cdp::Client;
use aad_platform::picker::Kind;
use serde_json::{json, Value};
use std::time::Instant;

#[cfg(target_os = "linux")]
pub use aad_linux::browser_picker::FilePicker;
#[cfg(target_os = "macos")]
pub use aad_macos::browser_picker::FilePicker;
#[cfg(windows)]
pub use aad_windows::browser_picker::FilePicker;

type Result<T> = crate::recording::Result<T>;

pub fn window(browser: &mut Client, page: &str) -> Result<(i64, [i32; 4])> {
    let info = browser.command("Browser.getWindowForTarget", json!({"targetId":page}))?;
    if info["bounds"]["windowState"] == "minimized" {
        return Err("browser window is minimized".into());
    }
    let bound = |k: &str| {
        info["bounds"][k]
            .as_i64()
            .and_then(|x| i32::try_from(x).ok())
            .ok_or("missing browser window bounds")
    };
    Ok((
        info["windowId"].as_i64().ok_or("missing window id")?,
        [
            bound("left")?,
            bound("top")?,
            bound("width")?,
            bound("height")?,
        ],
    ))
}

pub fn pid(browser: &mut Client) -> Result<u32> {
    let info = browser.command("SystemInfo.getProcessInfo", json!({}))?;
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
        .ok_or("missing browser process")?;
    if processes.next().is_some() {
        return Err("ambiguous browser process".into());
    }
    Ok(pid)
}

pub fn verify_owner(
    browser: &mut Client,
    page: &str,
    expected: (i64, [i32; 4]),
    process_only: bool,
) -> Result<()> {
    if window(browser, page)? != expected {
        return Err("the observed browser window changed".into());
    }
    let targets = browser.command("Target.getTargets", json!({}))?;
    let mut selected = false;
    for t in targets["targetInfos"]
        .as_array()
        .ok_or("missing browser targets")?
        .iter()
        .filter(|t| t["type"] == "page")
    {
        let other = t["targetId"].as_str().ok_or("missing peer target id")?;
        if other == page {
            selected = true;
        } else if process_only || window(browser, other)?.0 == expected.0 {
            return Err("another tab could own this dialog".into());
        }
    }
    if !selected {
        return Err("the observed page closed".into());
    }
    Ok(())
}

pub fn download_dialog(
    pid: u32,
    bounds: [i32; 4],
    suggestion: &str,
    deadline: Instant,
) -> Result<FilePicker> {
    if suggestion.is_empty()
        || suggestion.contains(['/', '\\'])
        || suggestion.chars().any(char::is_control)
    {
        return Err("missing or invalid browser download suggestion".into());
    }
    let dialog = FilePicker::observe(pid, None, bounds, Kind::Save, deadline)?;
    dialog.verify_suggestion(suggestion, deadline)?;
    Ok(dialog)
}

/// A page-scoped protocol subscription preserves the browser's download policy.
/// Keep the GUID and browser-side suggestion in memory only.
pub struct Download {
    pub guid: String,
    pub suggestion: String,
}
impl Download {
    pub fn from_begin(params: &Value, frame: &str, href: &str) -> Option<Self> {
        if frame.is_empty()
            || href.is_empty()
            || params["frameId"] != frame
            || params["url"] != href
        {
            return None;
        }
        Some(Self {
            guid: params["guid"]
                .as_str()
                .filter(|s| !s.is_empty())?
                .to_owned(),
            suggestion: params["suggestedFilename"]
                .as_str()
                .filter(|s| !s.is_empty())?
                .to_owned(),
        })
    }
}
