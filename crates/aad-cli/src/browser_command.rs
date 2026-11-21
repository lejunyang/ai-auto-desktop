use crate::{browser_record, browser_script, failure, table_data, EXIT_FAILED, EXIT_OK};
use clap::Subcommand;
use serde_json::Value;
mod download;
mod read;

#[derive(Subcommand)]
pub enum Command {
    /// Select local files in a Chromium file chooser that is already open.
    ChooseFiles(crate::desk::open_picker::Args),
    /// Choose a new destination in a Chromium Save As dialog that is already open.
    SaveAs(crate::desk::open_picker::SaveArgs),
    /// Read a page's text, outline, or several named fields in one observation.
    Read(read::Args),
    /// Download and verify the ready file from one identified task in a session-owned browser.
    DownloadExport(download::Args),
    /// List open Chromium pages and their page IDs.
    Pages {
        #[arg(long, default_value_t = 9222)]
        cdp_port: u16,
    },
    /// Run JavaScript using the AAD page API.
    Script(browser_script::Args),
    /// Record actions, page changes, screenshots and network evidence.
    #[command(subcommand)]
    Record(browser_record::Command),
    /// Read a paginated or scrolling Chromium table, optionally writing a CSV.
    CollectTable(table_data::CollectArgs),
}

pub fn run(command: &Command) -> (Value, u8) {
    match command {
        Command::ChooseFiles(args) => crate::desk::open_picker::choose(args),
        Command::SaveAs(args) => crate::desk::open_picker::save(args),
        Command::Read(args) => match read::run(args) {
            Ok(value) => (value, EXIT_OK),
            Err(error) => (error, EXIT_FAILED),
        },
        Command::DownloadExport(args) => match download::run(args) {
            Ok(report) => (report, EXIT_OK),
            Err(error) => (error, EXIT_FAILED),
        },
        Command::Pages { cdp_port } => record(&browser_record::Command::Targets {
            cdp_port: *cdp_port,
        }),
        Command::Script(args) => script(args),
        Command::Record(args) => record(args),
        Command::CollectTable(args) => table_data::outcome(table_data::collect(args)),
    }
}

pub fn script(args: &browser_script::Args) -> (Value, u8) {
    match browser_script::run(args) {
        Ok(value) => (value, EXIT_OK),
        Err(error) => (error, EXIT_FAILED),
    }
}

pub fn record(command: &browser_record::Command) -> (Value, u8) {
    match browser_record::run(command) {
        Ok(value) => (value, EXIT_OK),
        Err(error) => (
            failure("BROWSER.RECORDING_FAILED", &error.to_string(), None),
            EXIT_FAILED,
        ),
    }
}
