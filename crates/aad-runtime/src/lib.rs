//! Workflow execution engine for ai-auto-desktop.
//!
//! The engine takes a compiled [`aad_core::WorkflowDescriptor`] and executes it
//! against a set of providers, producing a journal of everything that happened
//! and a [`RunResult`] describing how the run ended.

pub mod engine;
pub mod journal;
pub mod provider;
pub mod script;
pub mod template;

pub use engine::{plan_digest, run, RunOptions, RUNTIME_VERSION};
pub use journal::{
    EventSink, Journal, NdjsonSink, NullSink, RunEvent, RunResult, RunStatus,
};
pub use provider::{PluginProvider, Provider, ProviderRegistry};
