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
        conflicts_with_all=["resource_id","browser_port","browser_bidi_port","app_documents","fields"])]
    webkit_port: Option<u16>,
    /// Page view. Chromium defaults to text, Firefox to outline.
    #[arg(long, value_parser=["text","outline","nodes","overview"], conflicts_with="fields")]
    view: Option<String>,
    /// One JSON array of {as, locator, property} fields, all read from the same observation.
    #[arg(long, value_name = "JSON")]
    fields: Option<String>,
    /// Read an equal-cell canvas grid. JSON contains locator, rows, and ordered columns.
    #[arg(long, value_name = "SPEC", conflicts_with_all=["view","fields","webkit_port","browser_bidi_port"])]
    canvas_grid: Option<String>,
    /// Read painted words and their locations in a visible Chromium canvas. Takes a JSON locator.
    #[arg(long, value_name = "LOCATOR", conflicts_with_all=["view","fields","canvas_grid","webkit_port","browser_bidi_port"])]
    canvas_text: Option<String>,
    /// Read shapes, labels and directed connections in an observed Chromium canvas.
    #[arg(long, value_name = "LOCATOR", conflicts_with_all=["view","fields","canvas_text","canvas_grid","webkit_port","browser_bidi_port"])]
    canvas_objects: Option<String>,
    /// Choose a recognition source. Auto uses a published editor scene first, then flowchart recognition.
    #[arg(long, value_parser=["auto","editor","flowchart","vision"], requires="canvas_objects")]
    source: Option<String>,
    /// Locate whiteboard objects by description using the configured local vision provider.
    #[arg(long, requires = "canvas_objects")]
    description: Option<String>,
    /// Save the verified canvas with numbered candidate boxes to a new SVG file.
    #[arg(long, value_name = "FILE.svg", requires = "canvas_objects")]
    preview: Option<std::path::PathBuf>,
    /// OCR language such as en-US or zh-Hans. Requires its installed language pack.
    #[arg(long)]
    language: Option<String>,
}

pub fn run(args: &Args) -> Result<Value, Value> {
    if args.language.is_some() && args.canvas_text.is_none() && args.canvas_objects.is_none() {
        return Err(
            json!({"code":"BROWSER.READ_INVALID","message":"--language requires --canvas-text or --canvas-objects"}),
        );
    }
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
    let (provider, session) = args.page.session(Policy::default())?;
    let read = match (&args.canvas_objects, &args.canvas_text, &args.canvas_grid, &args.fields) {
        (Some(locator), _, _, _) => {
            use aad_control::canvas_objects::{Options, Source};
            if locator.len() > 16_384 {
                return Err(json!({"code":"BROWSER.READ_INVALID","message":"canvas locator exceeds 16 KiB"}));
            }
            let source = match args.source.as_deref() {
                Some("editor") => Source::Editor,
                Some("flowchart") => Source::Flowchart,
                Some("vision") => Source::Vision,
                _ => Source::Auto,
            };
            PageRead::CanvasObjects { locator: serde_json::from_str(locator)
                .map_err(|e| json!({"code":"BROWSER.READ_INVALID","message":format!("invalid --canvas-objects: {e}")}))?,
                options: Options {source, language: args.language.clone(), description: args.description.clone(), preview:args.preview.is_some()} }
        }
        (_, Some(locator), _, _) => {
            if locator.len() > 16_384 {
                return Err(json!({"code":"BROWSER.READ_INVALID","message":"canvas locator exceeds 16 KiB"}));
            }
            PageRead::CanvasText { locator: serde_json::from_str(locator)
                .map_err(|err| json!({"code":"BROWSER.READ_INVALID","message":format!("invalid --canvas-text: {err}")}))?,
                language:args.language.clone() }
        }
        (_, _, Some(spec), _) => {
            if spec.len() > 16_384 {
                return Err(json!({"code":"BROWSER.READ_INVALID","message":"canvas grid spec exceeds 16 KiB"}));
            }
            PageRead::CanvasGrid(serde_json::from_str(spec)
                .map_err(|err| json!({"code":"BROWSER.READ_INVALID","message":format!("invalid --canvas-grid: {err}")}))?)
        }
        (_, _, _, Some(raw)) => PageRead::Fields(serde_json::from_str(raw)
            .map_err(|err| json!({"code":"BROWSER.READ_INVALID","message":format!("invalid --fields: {err}")}))?),
        _ => PageRead::View(match args.view.as_deref() {
            Some("outline") => ViewMode::Outline,
            Some("nodes") => ViewMode::Nodes,
            Some("overview") => ViewMode::Overview,
            None if provider == aad_control::browser_bidi::PROVIDER => ViewMode::Outline,
            _ => ViewMode::Text,
        }),
    };
    let mut result = read_page(
        Arc::new(session),
        provider,
        args.page.resource_id.as_deref(),
        args.page.url.as_deref(),
        read,
    )?;
    if let Some(path) = &args.preview {
        use std::io::Write as _;
        let svg = result
            .as_object_mut()
            .and_then(|r| r.remove("preview_svg"))
            .ok_or_else(
                || json!({"code":"BROWSER.READ_INVALID","message":"missing canvas preview"}),
            )?;
        let output = svg.as_str().ok_or_else(
            || json!({"code":"BROWSER.READ_INVALID","message":"invalid canvas preview"}),
        )?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let io_error =
            |e: std::io::Error| json!({"code":"BROWSER.PREVIEW_FAILED","message":e.to_string()});
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
        file.write_all(output.as_bytes()).map_err(io_error)?;
        file.persist_noclobber(path)
            .map_err(|e| io_error(e.error))?;
        result["preview"] = json!(path);
    }
    Ok(result)
}
