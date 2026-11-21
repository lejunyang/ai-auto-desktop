//! Finish the file dialog belonging to an observed Chromium page.
use super::{finish, usage, Desk, Outcome};
use aad_browser::cdp::Client;
use aad_control::browser_access::BrowserAccess;
use aad_control::{Action, Expect, Locator, Operation, Policy, Session, View};
#[cfg(target_os = "linux")]
use aad_linux::browser_picker::FilePicker;
#[cfg(target_os = "macos")]
use aad_macos::browser_picker::FilePicker;
use aad_platform::picker::{Kind, NewFile};
#[cfg(windows)]
use aad_windows::browser_picker::FilePicker;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(clap::Args)]
struct PageArgs {
    /// Loopback Chromium debugging port.
    #[arg(long, default_value_t = 9222, value_parser = clap::value_parser!(u16).range(1..))]
    cdp_port: u16,
    /// Exact page ID printed by browser-record targets.
    #[arg(long)]
    page: String,
    /// Require a specific native dialog ID (decimal or hex) on backends that expose one.
    #[arg(long, value_parser = xid)]
    window: Option<u64>,
    /// Authorize actions on the observed file dialog.
    #[arg(long, required = true)]
    allow_input: bool,
    /// Permit a file:// page under this directory. Independent of file grants.
    #[arg(long = "allow-browser-file-dir", value_name = "DIR")]
    page_dirs: Vec<PathBuf>,
}

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    page: PageArgs,
    /// Permit files under this existing directory. Repeatable.
    #[arg(long = "allow-upload-dir", required = true, value_name = "DIR")]
    upload_dirs: Vec<PathBuf>,
    /// Absolute path of an existing file to select. Repeat for a multiple input.
    #[arg(long = "file", required = true, value_name = "PATH")]
    files: Vec<String>,
    /// Select one file in a native showOpenFilePicker dialog on a single-frame page.
    #[arg(long)]
    native_file: bool,
}

#[derive(clap::Args)]
pub struct SaveArgs {
    #[command(flatten)]
    page: PageArgs,
    /// Allow a new file under this existing directory.
    #[arg(long = "allow-save-dir", value_name = "DIR")]
    root: PathBuf,
    /// Absolute destination path. The file must not already exist.
    #[arg(long = "file", value_name = "NEW_PATH")]
    file: PathBuf,
}

fn xid(text: &str) -> Result<u64, String> {
    let id = match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    id.ok()
        .filter(|&id| id != 0)
        .ok_or_else(|| "expected a nonzero native window ID".into())
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

pub fn choose(args: &Args) -> (Value, u8) {
    if args.native_file && args.files.len() != 1 {
        return usage("--native-file takes exactly one --file");
    }
    finish(run(&args.page, Selection::Open(args)))
}

pub fn save(args: &SaveArgs) -> (Value, u8) {
    let destination = match NewFile::under(&args.root, &args.file) {
        Ok(path) => path,
        Err(err) => return usage(err.to_string()),
    };
    finish(
        run(&args.page, Selection::Save(&destination)).map_err(|(mut report, status)| {
            if report["error"]["code"] == "CLI.OPEN_PICKER" {
                report["error"]["code"] = json!("CLI.SAVE_PICKER");
            }
            (report, status)
        }),
    )
}

enum Selection<'a> {
    Open(&'a Args),
    Save(&'a NewFile),
}

fn run(args: &PageArgs, selection: Selection<'_>) -> Outcome {
    let (kind, upload_dirs) = match selection {
        Selection::Open(open) => (Kind::Open, open.upload_dirs.as_slice()),
        Selection::Save(_) => (Kind::Save, &[][..]),
    };
    let roots = upload_dirs
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
    let picker =
        FilePicker::observe(pid, args.window, window.1, kind, deadline).map_err(refused)?;
    let dialog = picker.id();
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
    // Only the HTML upload needs the full list of controls to locate :open.
    // Native dialogs need the trusted document metadata and their own observed
    // OS controls, even when the page contains thousands of elements.
    let max_nodes = match selection {
        Selection::Open(open) if !open.native_file => aad_control::MAX_NODES,
        _ => 1,
    };
    let observed = desk.call(Operation::Observe {
        resource_id: id.into(),
        max_nodes,
        view: View::default(),
    })?;
    if observed["metadata"]["browser_instance_id"] != instance {
        return Err(refused("the browser instance changed"));
    }
    let open = match selection {
        Selection::Open(open) if !open.native_file => open,
        selection => {
            verify_page_binding(&mut browser, args, window.0, picker.process_only(), true)?;
            verify_document(args, &observed["metadata"], deadline)?;
            if page_window(&mut browser, &args.page)? != window {
                return Err(refused(
                    "the page moved to another window or changed bounds",
                ));
            }
            picker.revalidate(deadline).map_err(refused)?;
            let mut result =
                json!({"window":dialog,"browser_pid":pid,"page":args.page,"picker_closed":true});
            match selection {
                Selection::Save(destination) => {
                    destination.revalidate().map_err(refused)?;
                    picker
                        .save_new(destination.path(), deadline)
                        .map_err(|err| uncertain("CLI.SAVE_PICKER", err))?;
                    result["destination_selected"] = json!(true);
                    result["path"] = json!(destination.path());
                }
                Selection::Open(open) => {
                    let path = desk
                        .session
                        .native_upload_path(&open.files[0])
                        .map_err(super::automation)?;
                    picker
                        .open_file(&path, deadline)
                        .map_err(|err| uncertain("CLI.OPEN_PICKER", err))?;
                    result["native_file_selected"] = json!(true);
                    result["file_count"] = json!(1);
                }
            }
            return Ok(result);
        }
    };
    if observed["truncated"] != false {
        return Err(refused("incomplete observation of the page's file inputs"));
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
    if picker.process_only() {
        verify_page_binding(&mut browser, args, window.0, true, false)?;
    }
    picker.revalidate(deadline).map_err(refused)?;
    let result = desk.call(Operation::Act {
        target,
        action: Action::Upload {
            files: open.files.clone(),
        },
        verify: None,
    })?;
    // The page already received the files. A cleanup failure must never suggest
    // repeating the upload. Only the observed native dialog may be dismissed.
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
        json!({"files_submitted":true,"file_count":open.files.len(),"picker_closed":true,
        "window":dialog,"browser_pid":pid,"page":args.page}),
    )
}

fn uncertain(code: &str, err: impl std::fmt::Display) -> (Value, u8) {
    (
        json!({"status":"error","error":{"code":code,"effect":"unknown","retryable":false,
               "message":err.to_string()}}),
        crate::EXIT_FAILED,
    )
}

fn verify_document(args: &PageArgs, meta: &Value, deadline: Instant) -> Result<(), (Value, u8)> {
    let pages = Client::list_targets_until(args.cdp_port, deadline).map_err(refused)?;
    let page = pages
        .iter()
        .find(|p| p.id == args.page)
        .ok_or_else(|| refused("target page closed"))?;
    let mut client = Client::attach_bare_until(page, deadline).map_err(refused)?;
    client.set_deadline(deadline);
    let tree = client
        .command("Page.getFrameTree", json!({}))
        .map_err(refused)?;
    let frame = &tree["frameTree"]["frame"];
    if meta["loader_id"].as_str().is_none_or(str::is_empty)
        || frame["loaderId"] != meta["loader_id"]
        || frame["id"] != meta["frame_id"]
    {
        return Err(refused("the document that owned the native dialog changed"));
    }
    if tree["frameTree"]["childFrames"]
        .as_array()
        .is_some_and(|frames| !frames.is_empty())
    {
        return Err(refused("native file selection needs a page with one frame"));
    }
    Ok(())
}

/// A native panel identifies the window, but exposes no originating DOM node.
/// A portal can identify the browser process without exposing a native window ID.
fn verify_page_binding(
    browser: &mut Client,
    args: &PageArgs,
    window: i64,
    whole_process: bool,
    native_selection: bool,
) -> Result<(), (Value, u8)> {
    let targets = browser
        .command("Target.getTargets", json!({}))
        .map_err(refused)?;
    let pages = targets["targetInfos"]
        .as_array()
        .ok_or_else(|| refused("missing browser target list"))?;
    // Page.getFrameTree can omit out-of-process children. A target without
    // a proven parent cannot be excluded as the initiator of a native dialog.
    if native_selection && pages.iter().any(|t| t["type"] == "iframe") {
        return Err(refused(
            "native file selection requires a session without out-of-process frames",
        ));
    }
    let mut selected = false;
    for target in pages.iter().filter(|t| t["type"] == "page") {
        let id = target["targetId"]
            .as_str()
            .ok_or_else(|| refused("missing page identity"))?;
        if id == args.page {
            selected = true;
            continue;
        }
        let shares_owner = if whole_process {
            true
        } else {
            let other = browser
                .command("Browser.getWindowForTarget", json!({"targetId": id}))
                .map_err(refused)?;
            other["windowId"]
                .as_i64()
                .ok_or_else(|| refused("missing peer window identity"))?
                == window
        };
        if shares_owner {
            return Err(refused("the native dialog could belong to another tab; use a window with one page (a dedicated browser for a windowless portal binding)"));
        }
    }
    if !selected {
        return Err(refused("the selected page closed"));
    }
    Ok(())
}
