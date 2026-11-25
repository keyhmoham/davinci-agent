//! Tool subsystem: registry, deterministic policy engine, native tools and the
//! SLM-free argument binder.
//!
//! The policy engine is the security boundary — neither the decision model nor
//! the language model can bypass it, and it is evaluated again at execution
//! time with the final (template-resolved) arguments.

mod binder;
mod calc;
mod native;
mod policy;
mod registry;
mod sandbox;

pub use binder::{DirectCall, bind_direct};
pub use calc::{evaluate as evaluate_expression, format_number};
pub use native::register_native;
pub use policy::{PathGuard, PolicyConfig, PolicyDecision, PolicyEngine};
pub use registry::ToolRegistry;
