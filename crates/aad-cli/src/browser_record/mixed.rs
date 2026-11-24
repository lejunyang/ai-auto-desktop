use aad_browser::recording::{self, Result};
use aad_control::{Operation, Policy, Request, Session};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod native;
mod portable;
mod sources;
mod timeline;
mod workflow;

#[derive(Subcommand)]
pub enum Command {
    /// Attach both observers. Wait for "Mixed recording ready" before interacting.
    Start(Start),
    /// End both observers and let the start process publish workflow.json.
    Stop {
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Args)]
pub struct Start {
    /// Exact native resource_id from aad apps (AT-SPI, UIA, or AX).
    #[arg(long)]
    native: String,
    /// Exact accessible name of the native top-level window.
    #[arg(long)]
    window: String,
    /// Include another native application's main window. Repeat for more applications.
    #[arg(long, num_args = 2, value_names = ["RESOURCE", "WINDOW"], action = clap::ArgAction::Append)]
    also_native: Vec<String>,
    /// Also capture this named dialog of the same application when it appears. Repeatable.
    #[arg(long, value_name = "TITLE")]
    dialog: Vec<String>,
    /// Exact target ID from aad browser-record targets.
    #[arg(long)]
    page: String,
    #[arg(long, default_value_t = 9222)]
    cdp_port: u16,
    /// New directory for workflow.json, capture status and browser evidence.
    #[arg(long)]
    out: PathBuf,
    /// End after this many seconds, or earlier with mixed stop or Ctrl+C.
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..=3600))]
    seconds: u64,
}

pub fn run(command: &Command) -> Result<Value> {
    match command {
        Command::Stop { out } => {
            recording::control(&out.join("browser"), "stop", None)?;
            Ok(json!({"stop_requested":true,"out":out}))
        }
        Command::Start(args) => {
            fs::create_dir(&args.out)?;
            let result = record(args);
            if let Err(error) = &result {
                write_json(
                    &args.out,
                    "capture.json",
                    &json!({"status":"incomplete","error":error.to_string()}),
                )?;
            }
            result
        }
    }
}

fn write_json(out: &Path, name: &str, data: &Value) -> Result<()> {
    let tmp = out.join(format!("{name}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(data)?)?;
    fs::rename(tmp, out.join(name))?;
    Ok(())
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn record(args: &Start) -> Result<Value> {
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))?;
    let session = Session::native(Policy::default());
    let provider = session
        .providers()
        .iter()
        .map(|p| p.manifest().name.as_str())
        .find(|name| {
            matches!(
                *name,
                native::PROVIDER | "desktop.windows_uia" | "desktop.macos_ax"
            )
        })
        .ok_or("native desktop recording provider unavailable")?;
    let listed = session.execute(&Request::new(
        Some(session.id().to_string()),
        Operation::Targets {
            provider: Some(provider.into()),
        },
    ));
    let data = listed
        .data
        .ok_or_else(|| format!("cannot list native applications: {:?}", listed.error))?;
    let resources = data["resources"]
        .as_array()
        .ok_or("missing native resources")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut browser = aad_browser::cdp::Client::browser_until(args.cdp_port, deadline)?;
    browser.set_deadline(deadline);
    let info = browser.command("SystemInfo.getProcessInfo", json!({}))?;
    let processes = info["processInfo"]
        .as_array()
        .filter(|p| !p.is_empty())
        .ok_or("missing browser process identities")?;
    let selections = std::iter::once((&args.native, &args.window, args.dialog.as_slice())).chain(
        args.also_native
            .chunks_exact(2)
            .map(|pair| (&pair[0], &pair[1], &[][..])),
    );
    let mut watches = Vec::new();
    let mut selected = Vec::new();
    let mut pids = HashSet::new();
    for (id, window, dialogs) in selections {
        let resource = resources
            .iter()
            .find(|r| r["resource_id"].as_str() == Some(id))
            .ok_or_else(|| {
                format!("unknown native resource {id}; use the exact ID from aad apps")
            })?;
        let pid = resource["metadata"]["process_id"]
            .as_u64()
            .filter(|pid| *pid != 0)
            .ok_or("missing native process identity")?;
        if processes.iter().any(|p| p["id"].as_u64() == Some(pid)) {
            return Err(
                "the selected browser owns the native target; select a separate native application"
                    .into(),
            );
        }
        if !pids.insert(pid) {
            return Err(format!("native process {pid} was selected twice").into());
        }
        watches.push(sources::Watch::start(&session, resource, window, dialogs)?);
        selected.push(json!({"resource_id":id,"window":window,"dialogs":dialogs}));
    }
    let browser_dir = args.out.join("browser");
    let mut recorder = aad_control::browser::export::start_recording(
        recording::Options {
            port: args.cdp_port,
            page: args.page.clone(),
            out: browser_dir.clone(),
            seconds: args.seconds,
            redact_network: true,
            screenshots: false,
            body_limit: 0,
            max_bytes: 64 * 1024 * 1024,
            follow_popups: false,
        },
        None,
    )?;
    // On Linux Chromium TimeTicks and Watch both use CLOCK_MONOTONIC. Bracket
    // the sample to report the precision of alignment with DOM Date.now().
    let (offset, browser_precision) = if provider == native::PROVIDER {
        let before = wall_ms();
        let ticks = recorder.monotonic_ms()?;
        let interval = wall_ms()
            .checked_sub(before)
            .ok_or("wall clock stepped during alignment")?;
        let offset = (before + interval / 2)
            .checked_sub(ticks)
            .ok_or("incompatible native and browser clocks")?;
        (offset, interval + 1)
    } else {
        (0, 1)
    };
    let started = wall_ms();
    let elapsed = Instant::now();
    let mut stopped = started;
    let mut timeline = timeline::Timeline::default();
    write_json(
        &args.out,
        "capture.json",
        &json!({"status":"recording","started_ms":started}),
    )?;
    eprintln!("Mixed recording ready in {}", args.out.display());
    recorder.run_with_poll(&cancel, |final_poll| {
        if final_poll {
            stopped = wall_ms();
        }
        for (index, watch) in watches.iter_mut().enumerate() {
            watch.collect(&mut timeline, offset, index)?;
        }
        Ok(())
    })?;
    for (index, watch) in watches.iter_mut().enumerate() {
        watch.collect(&mut timeline, offset, index)?;
        watch.release()?;
    }
    // A dialog attached mid-capture can have a wider callback clock bracket.
    // Two independently aligned native clocks can err in opposite directions.
    let mut precision = vec![browser_precision];
    precision.extend(watches.iter().map(sources::Watch::precision_ms));
    precision.sort_unstable_by(|a, b| b.cmp(a));
    let precision = precision[0].saturating_add(precision[1]);
    if wall_ms().abs_diff(started + elapsed.elapsed().as_millis() as u64) > precision.max(50) {
        return Err("wall clock changed during capture; cannot order both sources".into());
    }
    timeline.read_browser(&browser_dir, &args.page)?;
    let (steps, windows) = timeline.steps(started, stopped, precision)?;
    let file = args.out.join("workflow.json");
    let summary = json!({
        "status":if !timeline.gaps.is_empty() {"needs_review"} else if steps.is_empty() {"empty"} else {"complete"},
        "workflow":(!steps.is_empty()).then_some(&file),"actions":steps.len(),"gaps":timeline.gaps,"filtered":timeline.filtered,
        "started_ms":started,"stopped_ms":stopped,"clock_alignment_ms":precision,
        "page":args.page,"native":args.native,"window":args.window,"dialogs":args.dialog,"native_sources":selected,
    });
    if !steps.is_empty() {
        let mut workflow = workflow::compile(&watches, &steps, &windows)?;
        // Keep the capture caveats with the replay file when it is copied elsewhere.
        workflow["metadata"]["annotations"]["capture"] = json!({
            "status":summary["status"], "gaps":timeline.gaps, "filtered":timeline.filtered,
            "started_ms":started, "stopped_ms":stopped, "clock_alignment_ms":precision,
        });
        write_json(&args.out, "workflow.json", &workflow)?;
    }
    write_json(&args.out, "capture.json", &summary)?;
    Ok(summary)
}
