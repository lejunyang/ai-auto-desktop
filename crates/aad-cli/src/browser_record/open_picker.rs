//! Complete a file selection that already has a visible native window.
use super::{usage, Desk, Outcome};
use aad_browser::cdp::Client;
use aad_control::browser_access::BrowserAccess;
use aad_control::{Action, Expect, Locator, Operation, Policy, Session, View};
use aad_linux::browser_picker::OpenPicker;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(clap::Args)]
pub struct Args {
    /// Loopback Chromium debugging port.
    #[arg(long, default_value_t = 9222, value_parser = clap::value_parser!(u16).range(1..))]
    cdp_port: u16,
    /// Exact page ID printed by browser-record targets.
    #[arg(long)]
    page: String,
    /// Require this dialog XID (decimal or 0x hex). Defaults to the active chooser.
    #[arg(long, value_parser = xid)]
    window: Option<u32>,
    /// Authorize file selection and dismissal of the observed native dialog.
    #[arg(long, required = true)]
    allow_input: bool,
    /// Permit files under this existing directory. Repeatable.
    #[arg(long = "allow-upload-dir", required = true, value_name = "DIR")]
    upload_dirs: Vec<PathBuf>,
    /// Permit a file:// page under this directory. Independent of upload grants.
    #[arg(long = "allow-browser-file-dir", value_name = "DIR")]
    page_dirs: Vec<PathBuf>,
    /// Absolute path of an existing file to select. Repeat for a multiple input.
    #[arg(long = "file", required = true, value_name = "PATH")]
    files: Vec<String>,
}

fn xid(text: &str) -> Result<u32, String> {
    let id = match text.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.parse(),
    };
    id.ok()
        .filter(|&id| id != 0)
        .ok_or_else(|| "expected a nonzero X11 window ID".into())
}

fn refused(reason: impl std::fmt::Display) -> (Value, u8) {
    (
        crate::failure("CLI.OPEN_PICKER", &reason.to_string(), None),
        crate::EXIT_FAILED,
    )
}

fn page_window(browser: &mut Client, page: &str) -> Result<(i64, [i32; 4]), (Value, u8)> {
    let info = browser
        .command("Browser.getWindowForTarget", json!({"targetId":page}))
        .map_err(refused)?;
    let bounds = &info["bounds"];
    if bounds["windowState"] == "minimized" {
        return Err(refused("the selected browser window is minimized"));
    }
    let component = |name: &str| {
        bounds[name]
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| refused("missing browser window bounds"))
    };
    Ok((
        info["windowId"]
            .as_i64()
            .ok_or_else(|| refused("missing browser window ID"))?,
        [
            component("left")?,
            component("top")?,
            component("width")?,
            component("height")?,
        ],
    ))
}

pub(super) fn run(args: &Args) -> Outcome {
    let roots = args
        .upload_dirs
        .iter()
        .map(|dir| {
            dir.canonicalize()
                .ok()
                .filter(|dir| dir.is_dir())
                .ok_or_else(|| usage("--allow-upload-dir requires an existing directory"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let desk = Desk {
        session: Session::native_with_browser(
            Policy {
                allow_input: args.allow_input,
                upload_roots: roots,
                browser_access: BrowserAccess::new(&args.page_dirs, &[]).map_err(refused)?,
                ..Policy::default()
            },
            Some(args.cdp_port),
        ),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut browser, instance) =
        Client::browser_with_identity_until(args.cdp_port, deadline).map_err(refused)?;
    browser.set_deadline(deadline);
    // Use the same session and lock keys as the normal Upload action. Keep both
    // leases until the native modal has actually unmapped.
    let _leases = desk
        .session
        .input_leases(&[format!("browser-input:{instance}"), "desktop-input".into()])
        .map_err(refused)?;
    let info = browser
        .command("SystemInfo.getProcessInfo", json!({}))
        .map_err(refused)?;
    let mut browsers = info["processInfo"]
        .as_array()
        .ok_or_else(|| refused("missing CDP process list"))?
        .iter()
        .filter(|p| p["type"] == "browser");
    let pid = browsers
        .next()
        .and_then(|p| p["id"].as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&pid| pid != 0)
        .ok_or_else(|| refused("missing browser process identity"))?;
    if browsers.next().is_some() {
        return Err(refused("ambiguous browser process identity"));
    }
    let window = page_window(&mut browser, &args.page)?;
    let picker = OpenPicker::observe(pid, args.window, window.1, deadline).map_err(refused)?;
    let dialog = picker.xid();
    let resources = desk.call(Operation::Targets {
        provider: Some(aad_control::browser::PROVIDER.into()),
    })?;
    let mut pages = resources["resources"]
        .as_array()
        .ok_or_else(|| refused("missing browser resources"))?
        .iter()
        .filter(|r| r["metadata"]["page_id"] == args.page && r["metadata"]["main_frame"] == true);
    let id = pages
        .next()
        .and_then(|r| r["resource_id"].as_str())
        .ok_or_else(|| refused("unknown main page"))?;
    if pages.next().is_some() {
        return Err(refused("ambiguous main page"));
    }
    let observed = desk.call(Operation::Observe {
        resource_id: id.into(),
        max_nodes: aad_control::MAX_NODES,
        view: View::default(),
    })?;
    if observed["truncated"] != false || observed["metadata"]["browser_instance_id"] != instance {
        return Err(refused(
            "incomplete observation or changed browser instance",
        ));
    }
    let found = desk.find(
        &observed,
        Locator {
            states: Some(json!({"picker_open":true})),
            ..Locator::default()
        },
        Expect::One,
    )?;
    let target = serde_json::from_value(found["target"].clone()).map_err(refused)?;
    if page_window(&mut browser, &args.page)? != window {
        return Err(refused("page window changed"));
    }
    picker.revalidate(deadline).map_err(refused)?;
    let result = desk.call(Operation::Act {
        target,
        action: Action::Upload {
            files: args.files.clone(),
        },
        verify: None,
    })?;
    // The page already received the files. A cleanup failure must never suggest
    // repeating the upload. The native operation addresses only the pinned XID.
    picker
        .dismiss(Instant::now() + Duration::from_secs(4))
        .map_err(|e| {
            (
                json!({"status":"error","error":{"code":"CLI.PICKER_DISMISS_FAILED",
            "message":e.to_string(),"effect":"applied","retryable":false},
            "files_submitted":true,"picker_closed":false,"window":dialog,"result":result}),
                crate::EXIT_FAILED,
            )
        })?;
    Ok(
        json!({"files_submitted":true,"file_count":args.files.len(),"picker_closed":true,
        "window":dialog,"browser_pid":pid,"page":args.page}),
    )
}
