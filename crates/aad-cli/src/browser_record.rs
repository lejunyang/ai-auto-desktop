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
    /// List existing browser profile directory IDs and their display names.
    Profiles {
        #[arg(long, value_parser = ["edge", "chrome", "brave"])]
        browser: String,
        /// Custom source user-data directory; default is this browser's daily data.
        #[arg(long)]
        profile_source: Option<PathBuf>,
    },
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
    /// Browser family. Select an existing profile separately with --existing-profile.
    #[arg(long, value_parser = ["edge", "chrome", "brave"])]
    browser: Option<String>,
    /// Open this URL AFTER recording listeners are installed.
    #[arg(long, requires = "browser")]
    url: Option<String>,
    /// Reuse/create a persistent working user-data directory; default is fresh/temp.
    #[arg(long, requires = "browser")]
    profile_dir: Option<PathBuf>,
    /// Copy an existing profile's login files. Omit ID only when exactly one exists.
    #[arg(long, requires = "browser", num_args = 0..=1, value_name = "ID")]
    existing_profile: Option<Option<String>>,
    /// Source user-data directory; defaults to the selected browser's daily data.
    #[arg(long, requires = "existing_profile")]
    profile_source: Option<PathBuf>,
    /// If occupied, close the browser using this user-data directory before starting.
    #[arg(long, requires = "browser")]
    kill_browser: bool,
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
        Command::Profiles {
            browser,
            profile_source,
        } => {
            let root = source_root(browser, profile_source)?;
            Ok(
                json!({"browser":browser,"profile_source":root,"profiles":aad_cdp::launch::list_profiles(&root)?}),
            )
        }
        Command::Targets { cdp_port } => Ok(
            json!({"pages":aad_cdp::Client::list_targets(*cdp_port)?.iter().map(|p|json!({"id":p.id,"title":p.title,"url":p.url})).collect::<Vec<_>>()}),
        ),
        Command::Start(args) => {
            if args.out.exists() {
                return Err("recording output must be a new directory".into());
            }
            if args.body_limit > 1024 * 1024 || args.max_bytes < 1024 * 1024 {
                return Err("body limit must be <= 1 MiB; session budget must be >= 1 MiB".into());
            }
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))?;
            let browser = if let Some(name) = &args.browser {
                let source = if args.existing_profile.is_some() {
                    Some(source_root(name, &args.profile_source)?)
                } else {
                    None
                };
                let profile_id = match (&source, &args.existing_profile) {
                    (Some(root), Some(id)) => aad_cdp::launch::select_profile(root, id.as_deref())?,
                    _ => "Default".into(),
                };
                if let Some(root) = &source {
                    eprintln!(
                        "Using existing profile {profile_id} from {} via a working login copy.",
                        root.display()
                    );
                }
                Some(aad_cdp::launch::Browser::start(aad_cdp::launch::Options {
                    browser: name.clone(),
                    executable: args.browser_executable.clone(),
                    profile_dir: args.profile_dir.clone(),
                    copy_from: source,
                    profile_id,
                    kill_browser: args.kill_browser,
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

fn source_root(browser: &str, explicit: &Option<PathBuf>) -> recording::Result<PathBuf> {
    explicit
        .clone()
        .or_else(|| aad_cdp::launch::real_user_data(browser))
        .ok_or_else(|| {
            "browser profile source not found; use --profile-source <user-data-dir>".into()
        })
}
