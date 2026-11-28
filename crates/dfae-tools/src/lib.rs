//! Tool subsystem: registry, deterministic policy engine, native tools and the
//! worker model-free argument binder.
//!
//! The policy engine is the security boundary — neither the decision model nor
//! the language model can bypass it, and it is evaluated again at execution
//! time with the final (template-resolved) arguments.

mod binder;
mod calc;
pub mod changes;
mod coding;
mod native;
mod policy;
mod registry;
mod sandbox;
mod toolbox;
mod web;

pub use binder::{DirectCall, bind_direct};
pub use calc::{evaluate as evaluate_expression, format_number};
pub use coding::{RunningServer, job_tail, running_servers};
pub use native::register_native;
pub use policy::{PathGuard, PolicyConfig, PolicyDecision, PolicyEngine, ShellMode};
pub use registry::ToolRegistry;
pub use toolbox::shell_env;
