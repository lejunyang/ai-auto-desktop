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
    /// Record an explicit page and related frames/popups until stop or Ctrl+C.
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
    #[arg(long, default_value_t = 9222)]
    cdp_port: u16,
    /// Exact page ID returned by browser-record targets. Never guessed by title.
    #[arg(long)]
    page: String,
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
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))?;
            let recorder = recording::Recorder::start(Options {
                port: args.cdp_port,
                page: args.page.clone(),
                out: args.out.clone(),
                seconds: args.seconds,
                redact_network: args.redact_network,
                screenshots: !args.no_screenshots,
                body_limit: args.body_limit,
                max_bytes: args.max_bytes,
                follow_popups: !args.no_follow_popups,
            })?;
            eprintln!("Recording page {} into {}; Ctrl+C or browser-record stop finishes the walkthrough.",args.page,args.out.display());
            recorder.run(&cancel)
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
