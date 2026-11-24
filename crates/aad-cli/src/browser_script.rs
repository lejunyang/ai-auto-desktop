use aad_control::{Policy, Session};
use aad_mcp::browser_script::{Script, SOURCE_LIMIT};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("page").required(true).args(["resource_id","url"])))]
pub struct PageArgs {
    /// Exact URL of one open page.
    #[arg(long)]
    pub url: Option<String>,
    /// Browser resource id returned by aad apps or control_targets.
    #[arg(long)]
    pub resource_id: Option<String>,
    /// Chromium CDP port on localhost (default 9222).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..), conflicts_with="browser_bidi_port")]
    browser_port: Option<u16>,
    /// Attach to one open Electron renderer with this exact app document URL. Repeatable.
    #[arg(
        long = "allow-app-document",
        value_name = "URL",
        conflicts_with = "browser_bidi_port"
    )]
    app_documents: Vec<String>,
    /// Firefox WebDriver BiDi port on localhost.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    browser_bidi_port: Option<u16>,
}

impl PageArgs {
    pub fn session(&self, mut policy: Policy) -> Result<(&'static str, Session), Value> {
        policy.browser_access = policy
            .browser_access
            .with_app_documents(&self.app_documents)
            .map_err(|e| e.to_json())?;
        let (provider, cdp) = if self.browser_bidi_port.is_some() {
            (aad_control::browser_bidi::PROVIDER, None)
        } else {
            (
                aad_control::browser::PROVIDER,
                Some(self.browser_port.unwrap_or(9222)),
            )
        };
        policy.providers = [provider.to_owned()].into();
        Ok((
            provider,
            Session::native_with_browsers(policy, cdp, self.browser_bidi_port),
        ))
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// JavaScript function body. Use page and inputs, then return a JSON value.
    file: PathBuf,
    #[command(flatten)]
    page: PageArgs,
    /// Allow actions such as clicking or filling controls.
    #[arg(long)]
    allow_input: bool,
    /// Permit selection of existing files under this directory. Repeatable.
    #[arg(long = "allow-upload-dir", value_name = "DIR")]
    upload_dirs: Vec<PathBuf>,
    /// Allow this exact directory for one Chromium webkitdirectory upload (Linux).
    #[arg(long, value_name="DIR", requires_all=["allow_input","upload_tree_stage"], conflicts_with="browser_bidi_port")]
    allow_upload_tree: Option<PathBuf>,
    /// Create a private snapshot here. Keep it until the page finishes reading; then remove it.
    #[arg(long, value_name = "NEW_DIR", requires = "allow_upload_tree")]
    upload_tree_stage: Option<PathBuf>,
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
    let (provider, session) = args.page.session(Policy {
        allow_input: args.allow_input,
        upload_roots: args.upload_dirs.clone(),
        ..Policy::default()
    })?;
    let session = match (&args.allow_upload_tree, &args.upload_tree_stage) {
        (Some(source), Some(stage)) => session
            .with_upload_tree(source, stage)
            .map_err(|err| err.to_json())?,
        _ => session,
    };
    aad_mcp::browser_script::run(
        Arc::new(session),
        Script {
            source,
            inputs,
            provider: Some(provider.to_owned()),
            resource_id: args.page.resource_id.clone(),
            url: args.page.url.clone(),
            timeout_ms: args.timeout_ms,
        },
        aad_runtime::CancellationToken::new(),
    )
}
