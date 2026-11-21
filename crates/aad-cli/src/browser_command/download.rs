use aad_browser::launch::{Browser, Options};
use aad_control::{browser::PROVIDER, Operation, Policy, Request, Session};
use aad_mcp::browser_script;
use serde_json::{json, Value};
use std::{io::Read, path::PathBuf, sync::Arc};

#[derive(clap::Args)]
pub struct Args {
    /// Exact HTTP(S) task-list URL to open in a new Chromium session.
    #[arg(long)]
    url: String,
    /// JSON task, download trigger and expected-file specification.
    #[arg(long)]
    spec: PathBuf,
    /// Reconcile the verified download as CSV against this complete expected collection JSON.
    #[arg(long, value_name = "FILE")]
    compare_collection: Option<PathBuf>,
    /// Compare and total this non-key column as an exact decimal. Repeatable.
    #[arg(long, requires = "compare_collection", value_name = "COLUMN")]
    decimal: Vec<String>,
    /// Grant the single verified download-trigger click.
    #[arg(long)]
    allow_input: bool,
    #[arg(long, default_value="chrome",value_parser=["chrome","edge","brave"])]
    browser: String,
    #[arg(long)]
    browser_executable: Option<PathBuf>,
    /// Optional reusable working profile (for an already authenticated site).
    #[arg(long)]
    profile_dir: Option<PathBuf>,
    #[arg(long)]
    headless: bool,
    /// Time to wait for this task and its verified file (up to 30 minutes).
    #[arg(long,default_value_t=30000,value_parser=clap::value_parser!(u64).range(1..=1800000))]
    timeout_ms: u64,
}

pub fn run(args: &Args) -> Result<Value, Value> {
    let mut report = download(args)?;
    let Some(collection) = &args.compare_collection else {
        return Ok(report);
    };
    let comparison = report["result"]["file"]["path"]
        .as_str()
        .filter(|_| report["result"]["verified"] == true)
        .ok_or_else(|| "missing verified download path".to_owned())
        .and_then(|path| {
            crate::table_data::compare_download(
                collection,
                std::path::Path::new(path),
                &args.decimal,
            )
        });
    match comparison {
        Ok(comparison) => {
            let matches = comparison["matches"] == true;
            report["result"]["comparison"] = comparison;
            if matches {
                return Ok(report);
            }
            report["error"] = json!({"code":"BROWSER.EXPORT_CONTENT_MISMATCH","message":"downloaded CSV differs from the expected collection"});
        }
        Err(message) => {
            report["error"] = json!({"code":"BROWSER.EXPORT_COMPARISON_FAILED","message":message});
        }
    }
    Err(report)
}

fn download(args: &Args) -> Result<Value, Value> {
    if !args.allow_input {
        return Err(
            json!({"code":"POLICY.DENIED","message":"download-export requires --allow-input to click the observed task control"}),
        );
    }
    if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
        return Err(
            json!({"code":"BROWSER.EXPORT_INVALID","message":"supply the exact HTTP(S) task page URL"}),
        );
    }
    let invalid = |message: String| json!({"code":"BROWSER.EXPORT_INVALID","message":message});
    let mut source = Vec::new();
    std::fs::File::open(&args.spec)
        .and_then(|file| {
            file.take(browser_script::SOURCE_LIMIT as u64 + 1)
                .read_to_end(&mut source)
        })
        .map_err(|err| invalid(err.to_string()))?;
    if source.len() > browser_script::SOURCE_LIMIT {
        return Err(invalid("spec exceeds 64 KiB".into()));
    }
    let inputs = serde_json::from_slice(&source)
        .map_err(|err| invalid(format!("invalid export spec JSON: {err}")))?;
    let stop = aad_runtime::CancellationToken::new();
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.cancel()).map_err(|err| invalid(err.to_string()))?;
    let browser = Browser::start(Options {
        browser: args.browser.clone(),
        executable: args.browser_executable.clone(),
        profile_dir: args.profile_dir.clone(),
        copy_from: None,
        profile_id: "Default".into(),
        kill_browser: false,
        headless: args.headless,
    })
    .map_err(|err| json!({"code":"BROWSER.LAUNCH_FAILED","message":err.to_string()}))?;
    let session = Session::native_with_browser(
        Policy {
            allow_input: true,
            providers: [PROVIDER.into()].into(),
            ..Default::default()
        },
        None,
    );
    session
        .attach_launched_browser(browser.port)
        .map_err(|err| err.to_json())?;
    session
        .download_cursor(std::time::Duration::from_secs(2))
        .map_err(|err| err.to_json())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(args.timeout_ms);
    let pending_navigation = match browser.open_url(&args.url) {
        Ok(()) => None,
        Err(err)
            if matches!(err.downcast_ref::<aad_browser::cdp::CdpError>(),
            Some(aad_browser::cdp::CdpError::Protocol(message)) if message == "command deadline elapsed") =>
        {
            Some(err)
        }
        Err(err) => {
            return Err(json!({"code":"BROWSER.NAVIGATION_FAILED","message":err.to_string()}))
        }
    };
    let id = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            if let Some(err) = pending_navigation {
                return Err(
                    json!({"code":"BROWSER.NAVIGATION_FAILED","message":format!("navigation did not acknowledge or reach the exact task URL: {err}")}),
                );
            }
            return Err(
                json!({"code":"BROWSER.PAGE_NOT_FOUND","message":"the launched main frame did not reach the exact requested URL"}),
            );
        }
        let mut request = Request::new(
            Some(session.id().into()),
            Operation::Targets {
                provider: Some(PROVIDER.into()),
            },
        );
        request.timeout_ms = left.as_millis().clamp(1, 30000) as u64;
        let reply = session.execute_cancellable(&request, stop.clone());
        if let Some(error) = reply.error {
            return Err(error);
        }
        let data = reply.data.unwrap_or_default();
        if let Some(error) = data["unavailable"].get(PROVIDER) {
            return Err(error.clone());
        }
        if let Some(id) = data["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| {
                row["metadata"]["page_id"] == browser.page
                    && row["metadata"]["main_frame"] == true
                    && row["metadata"]["url"] == args.url
                    && row["metadata"]["access_error"].is_null()
                    && row["metadata"]["responsive"] != false
            })
            .and_then(|row| row["resource_id"].as_str())
        {
            break id.to_string();
        }
        std::thread::sleep(std::time::Duration::from_millis(75).min(left));
    };
    browser_script::run_download_export(
        Arc::new(session),
        id,
        inputs,
        deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .max(1) as u64,
        stop,
    )
}
