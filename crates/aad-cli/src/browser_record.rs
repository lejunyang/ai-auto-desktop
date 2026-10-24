use aad_cdp::recording::{self, Options};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Subcommand)]
pub enum Command {
    /// List pages for explicit selection (does not start or restart a browser).
    Targets {
        #[arg(long, default_value_t = 9222)]
        cdp_port: u16,
    },
    /// Launch a browser or attach to an explicit page and record until stop/Ctrl+C.
    Start(Start),
    /// Request a graceful stop from another terminal.
    Stop {
        #[arg(long)]
        out: PathBuf,
    },
    /// Add a task note and request a screenshot at the next checkpoint.
    Mark {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        note: String,
    },
    /// Rebuild the readable walkthrough from a completed recording's steps.
    Export {
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Args)]
pub struct Start {
    #[arg(long, default_value_t = 9222, conflicts_with = "browser")]
    cdp_port: u16,
    /// Exact page ID returned by browser-record targets. Never guessed by title.
    #[arg(long, required_unless_present = "browser", conflicts_with = "browser")]
    page: Option<String>,
    /// Start a browser with CDP automatically. -login copies the closed daily profile.
    #[arg(long, value_parser = ["edge", "chrome", "brave", "edge-login", "chrome-login", "brave-login"])]
    browser: Option<String>,
    /// Open this URL AFTER recording listeners are installed.
    #[arg(long, requires = "browser")]
    url: Option<String>,
    /// Reuse/create a persistent working user-data directory; default is fresh/temp.
    #[arg(long, requires = "browser")]
    profile_dir: Option<PathBuf>,
    /// Copy login files from the chosen browser's closed default profile.
    #[arg(long, requires = "browser", conflicts_with = "copy_profile_from")]
    copy_login: bool,
    /// Copy login files from a closed user-data directory into a NEW working profile.
    #[arg(long, requires = "browser")]
    copy_profile_from: Option<PathBuf>,
    /// Override executable discovery (use the matching --browser family).
    #[arg(long, requires = "browser")]
    browser_executable: Option<PathBuf>,
    /// Launch without a visible window, mainly for automated fixtures.
    #[arg(long, requires = "browser")]
    headless: bool,
    /// New directory; existing paths are never overwritten.
    #[arg(long)]
    out: PathBuf,
    /// Zero records until Ctrl+C or browser-record stop.
    #[arg(long, default_value_t = 0)]
    seconds: u64,
    /// Opt in to network secret filtering. Default: retain captured raw values.
    #[arg(long)]
    redact_network: bool,
    /// Disable viewport screenshots; otherwise capture key checkpoints.
    #[arg(long)]
    no_screenshots: bool,
    /// Maximum bytes per captured text request/response body. Zero skips bodies.
    #[arg(long, default_value_t = 32768)]
    body_limit: usize,
    /// Stop with an explicit incomplete status at the session storage budget.
    #[arg(long, default_value_t = 104857600)]
    max_bytes: u64,
    /// Stay on the selected page and its frames, without following login popups.
    #[arg(long)]
    no_follow_popups: bool,
}

pub fn run(command: &Command) -> recording::Result<Value> {
    match command {
        Command::Targets { cdp_port } => Ok(
            json!({"pages":aad_cdp::Client::list_targets(*cdp_port)?.iter().map(|p|json!({"id":p.id,"title":p.title,"url":p.url})).collect::<Vec<_>>()}),
        ),
        Command::Start(args) => {
            if args.out.exists() {
                return Err("recording output must be a new directory".into());
            }
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))?;
            let browser = if let Some(name) = &args.browser {
                let choice = crate::browser_launch::parse(name).ok_or("unsupported browser")?;
                if choice.copy_login && args.copy_profile_from.is_some() {
                    return Err(
                        "use either a -login browser or --copy-profile-from, not both".into(),
                    );
                }
                let source = if choice.copy_login || args.copy_login {
                    Some(aad_cdp::launch::real_user_data(&choice.browser).ok_or("daily browser profile not found; use --copy-profile-from for a custom user-data directory")?)
                } else {
                    args.copy_profile_from.clone()
                };
                Some(aad_cdp::launch::Browser::start(aad_cdp::launch::Options {
                    browser: choice.browser,
                    executable: args.browser_executable.clone(),
                    profile_dir: args.profile_dir.clone(),
                    copy_from: source,
                    headless: args.headless,
                })?)
            } else {
                None
            };
            let (port, page) = match &browser {
                Some(browser) => {
                    eprintln!("Started browser on port {} with profile {}; it closes when recording ends.", browser.port, browser.profile_dir().display());
                    (browser.port, browser.page.clone())
                }
                None => (
                    args.cdp_port,
                    args.page.clone().ok_or("--page or --browser is required")?,
                ),
            };
            let recorder = recording::Recorder::start(Options {
                port,
                page: page.clone(),
                out: args.out.clone(),
                seconds: args.seconds,
                redact_network: args.redact_network,
                screenshots: !args.no_screenshots,
                body_limit: args.body_limit,
                max_bytes: args.max_bytes,
                follow_popups: !args.no_follow_popups,
            })?;
            if let (Some(browser), Some(url)) = (&browser, &args.url) {
                browser.open_url(url)?;
            }
            eprintln!("Recording page {} into {}; Ctrl+C or browser-record stop finishes the walkthrough.",page,args.out.display());
            let result = recorder.run(&cancel);
            drop(browser);
            result
        }
        Command::Stop { out } => {
            recording::control(out, "stop", None)?;
            Ok(json!({"stop_requested":true,"out":out}))
        }
        Command::Mark { out, note } => {
            recording::control(out, "mark", Some(note))?;
            Ok(json!({"mark_requested":true,"out":out}))
        }
        Command::Export { out } => {
            recording::export(out)?;
            Ok(json!({"exported":true,"out":out.join("walkthrough.md")}))
        }
    }
}
