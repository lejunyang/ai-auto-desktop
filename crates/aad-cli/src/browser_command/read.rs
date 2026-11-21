use crate::browser_script::PageArgs;
use aad_control::{Policy, ViewMode};
use aad_mcp::browser_script::{read_page, PageRead};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    page: PageArgs,
    /// Page view (defaults to text).
    #[arg(long, value_parser=["text","outline","nodes","overview"], conflicts_with="fields")]
    view: Option<String>,
    /// One JSON array of {as, locator, property} fields, all read from the same observation.
    #[arg(long, value_name = "JSON")]
    fields: Option<String>,
}

pub fn run(args: &Args) -> Result<Value, Value> {
    let read = match &args.fields {
        Some(raw) => PageRead::Fields(serde_json::from_str(raw)
            .map_err(|err| json!({"code":"BROWSER.READ_INVALID","message":format!("invalid --fields: {err}")}))?),
        None => PageRead::View(match args.view.as_deref() {
            Some("outline") => ViewMode::Outline,
            Some("nodes") => ViewMode::Nodes,
            Some("overview") => ViewMode::Overview,
            _ => ViewMode::Text,
        }),
    };
    let (provider, session) = args.page.session(Policy::default());
    read_page(
        Arc::new(session),
        provider,
        args.page.resource_id.as_deref(),
        args.page.url.as_deref(),
        read,
    )
}
