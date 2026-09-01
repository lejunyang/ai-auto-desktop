//! Core descriptor, expression and compilation types for ai-auto-desktop.
//!
//! This crate is the foundation of the runtime: it owns the error contract,
//! the constrained expression language, the compiled descriptor model and the
//! strict compiler.  It performs no I/O beyond reading a descriptor file and
//! has no knowledge of plugins, so it can be reused by the CLI, the GUI and
//! the MCP server alike.

pub mod compiler;
pub mod errors;
pub mod expression;
pub mod model;

pub use compiler::{
    compile_descriptor, load_descriptor, parse_descriptor_text, API_VERSION, KIND,
};
pub use errors::{AutomationError, DescriptorIssue, ErrorLocation, Result};
pub use expression::{compile_expression, evaluate_expression, CompiledExpression, ExpressionError};
pub use model::{
    parse_duration, Budgets, CompiledStep, ErrorHandler, HandlerMode, NamedValue, StepType,
    SwitchCase, WorkflowDescriptor,
};
