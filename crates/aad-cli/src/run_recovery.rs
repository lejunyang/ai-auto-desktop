use aad_core::{AutomationError, WorkflowDescriptor};
use aad_runtime::{RunOptions, RunResult};
use clap::Args;
use std::io::Read;
use std::path::PathBuf;

#[derive(Args, Default)]
pub struct RecoveryArgs {
    /// Start a reconcilable run in a NEW private SQLite journal (records public action outputs).
    #[arg(long, value_name="FILE", conflicts_with_all=["recover_from","journal","dry_run"])]
    recovery_log: Option<PathBuf>,
    /// Continue this original recovery journal under the same workflow and inputs.
    #[arg(long, value_name="FILE", conflicts_with_all=["journal","dry_run"])]
    recover_from: Option<PathBuf>,
    /// Business lookup identity, e.g. submit=/inputs/order or submit=/row/order in a loop.
    #[arg(long, value_name = "STEP=POINTER", requires = "recovery_log")]
    business_key: Option<String>,
    /// Trusted JSON receipt for the pending ticket, or an array of keyed business receipts.
    #[arg(long, value_name = "FILE", requires = "recover_from")]
    receipt: Option<PathBuf>,
}

impl RecoveryArgs {
    pub fn run(
        &self,
        workflow: &WorkflowDescriptor,
        options: RunOptions,
    ) -> Result<RunResult, AutomationError> {
        let Some(path) = self.recovery_log.as_ref().or(self.recover_from.as_ref()) else {
            return Ok(aad_runtime::run(workflow, options));
        };
        let invalid = |message: String| {
            AutomationError::new("CLI.INVALID_ARGUMENTS", message).with_effect("not_applied")
        };
        let key = self
            .business_key
            .as_deref()
            .map(|raw| {
                raw.split_once('=')
                    .ok_or_else(|| invalid("--business-key expects STEP=POINTER".into()))
            })
            .transpose()?;
        let proof = self
            .receipt
            .as_ref()
            .map(|path| {
                let mut raw = Vec::new();
                std::fs::File::open(path)
                    .and_then(|file| file.take(4 * 1024 * 1024 + 1).read_to_end(&mut raw))
                    .map_err(|err| invalid(err.to_string()))?;
                if raw.len() > 4 * 1024 * 1024 {
                    return Err(invalid("receipt exceeds 4 MiB".into()));
                }
                serde_json::from_slice(&raw).map_err(|err| invalid(err.to_string()))
            })
            .transpose()?;
        aad_runtime::recovery::execute(
            workflow,
            options,
            path,
            self.recovery_log.is_some(),
            key,
            proof,
        )
    }
}
