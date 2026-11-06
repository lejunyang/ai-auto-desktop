//! Explicit cooperative cancellation carried across provider boundaries.
//!
//! Cancellation never undoes a submitted effect. A native provider must stop
//! subsequent dispatch and release its owned input before returning; its
//! deadline and release watchdog remain independent safeguards.

use aad_core::AutomationError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Adapter plumbing for platform crates that do not depend on the runtime.
    /// Consumers must only read this flag; resetting it would lose cancellation.
    pub fn shared_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// A pre-dispatch check. Providers that already submitted input must change
    /// the error's effect to `unknown`, after completing owned-input cleanup.
    pub fn check(&self) -> Result<(), AutomationError> {
        if self.is_cancelled() {
            Err(
                AutomationError::new("DRIVER.CANCELLED", "provider invocation was cancelled")
                    .with_category("driver")
                    .with_effect("not_applied"),
            )
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_and_platform_watchdogs_see_the_same_cancel_signal() {
        let token = CancellationToken::new();
        let clone = token.clone();
        let platform = clone.shared_flag();
        assert!(!platform.load(Ordering::SeqCst));
        token.cancel();
        token.cancel();
        assert!(clone.is_cancelled());
        assert!(platform.load(Ordering::SeqCst));
        let error = clone.check().unwrap_err();
        assert_eq!(error.code, "DRIVER.CANCELLED");
        assert_eq!(error.effect, "not_applied");
    }
}
