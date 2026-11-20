use aad_control::{Policy, Session};
use aad_mcp::browser_script::{Script, SOURCE_LIMIT};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("page").required(true).args(["resource_id","url"])))]
pub struct Args {
    /// JavaScript function body. Use page and inputs, then return a JSON value.
    file: PathBuf,
    /// Exact URL of one open page.
    #[arg(long)]
    url: Option<String>,
    /// Browser resource id returned by aad apps or control_targets.
    #[arg(long)]
    resource_id: Option<String>,
    /// Chromium CDP port on localhost (default 9222).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..), conflicts_with="browser_bidi_port")]
    browser_port: Option<u16>,
    /// Firefox WebDriver BiDi port on localhost.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    browser_bidi_port: Option<u16>,
    /// Allow actions such as clicking or filling controls.
    #[arg(long)]
    allow_input: bool,
    /// A JSON value available to the script as inputs.
    #[arg(long)]
    inputs: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=120000))]
    timeout_ms: Option<u64>,
}

pub fn run(args: &Args) -> Result<Value, Value> {
    let invalid = |message: String| json!({"code":"BROWSER.SCRIPT_INVALID","message":message});
    let mut source = String::new();
    std::fs::File::open(&args.file)
        .and_then(|file| {
            file.take(SOURCE_LIMIT as u64 + 1)
                .read_to_string(&mut source)
        })
        .map_err(|error| invalid(error.to_string()))?;
    let inputs = args
        .inputs
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| invalid(error.to_string()))?
        .unwrap_or(Value::Null);
    let (provider, cdp) = if args.browser_bidi_port.is_some() {
        (aad_control::browser_bidi::PROVIDER, None)
    } else {
        (
            aad_control::browser::PROVIDER,
            Some(args.browser_port.unwrap_or(9222)),
        )
    };
    let session = Session::native_with_browsers(
        Policy {
            allow_input: args.allow_input,
            providers: [provider.to_owned()].into(),
            ..Policy::default()
        },
        cdp,
        args.browser_bidi_port,
    );
    aad_mcp::browser_script::run(
        Arc::new(session),
        Script {
            source,
            inputs,
            provider: Some(provider.to_owned()),
            resource_id: args.resource_id.clone(),
            url: args.url.clone(),
            timeout_ms: args.timeout_ms,
        },
        aad_runtime::CancellationToken::new(),
    )
}
