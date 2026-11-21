use crate::browser_script::PageArgs;
use aad_control::{Policy, ViewMode};
use aad_mcp::browser_script::{read_page, PageRead};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    page: PageArgs,
    /// Open --url in a fresh WebKit WebDriver session on localhost. Supports text and outline.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..), requires="url",
        conflicts_with_all=["resource_id","browser_port","browser_bidi_port","fields"])]
    webkit_port: Option<u16>,
    /// Page view. Chromium defaults to text, Firefox to outline.
    #[arg(long, value_parser=["text","outline","nodes","overview"], conflicts_with="fields")]
    view: Option<String>,
    /// One JSON array of {as, locator, property} fields, all read from the same observation.
    #[arg(long, value_name = "JSON")]
    fields: Option<String>,
}

pub fn run(args: &Args) -> Result<Value, Value> {
    if let Some(port) = args.webkit_port {
        let outline = match args.view.as_deref() {
            None | Some("text") => false,
            Some("outline") => true,
            _ => {
                return Err(
                    json!({"code":"BROWSER.READ_INVALID","message":"WebKit supports --view text or outline"}),
                )
            }
        };
        return aad_browser::webkit::read(
            port,
            args.page.url.as_deref().unwrap_or_default(),
            outline,
        )
        .map_err(|err| json!({"code":"BROWSER.WEBKIT_READ_FAILED","message":err.to_string()}));
    }
    let (provider, session) = args.page.session(Policy::default());
    let read = match &args.fields {
        Some(raw) => PageRead::Fields(serde_json::from_str(raw)
            .map_err(|err| json!({"code":"BROWSER.READ_INVALID","message":format!("invalid --fields: {err}")}))?),
        None => PageRead::View(match args.view.as_deref() {
            Some("outline") => ViewMode::Outline,
            Some("nodes") => ViewMode::Nodes,
            Some("overview") => ViewMode::Overview,
            None if provider == aad_control::browser_bidi::PROVIDER => ViewMode::Outline,
            _ => ViewMode::Text,
        }),
    };
    read_page(
        Arc::new(session),
        provider,
        args.page.resource_id.as_deref(),
        args.page.url.as_deref(),
        read,
    )
}
