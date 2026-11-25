//! Token economy: mechanisms that spend **zero** generative tokens to save some.
//!
//! * [`cost`] — a cost model learned from the ledger. It prices a plan *before*
//!   it runs (budget gate, skill-vs-fresh, team-vs-flat) and is refined by every
//!   finished task.
//! * [`repair`] — deterministic fixes for argument and path problems, applied when
//!   a plan is parsed and when a step fails, so a model is asked only for what
//!   code cannot fix.
//! * [`contracts`] — zero-cost pre/post-conditions: empty template inputs, malformed
//!   or degenerate model output, tool-declared output contracts. Bad data is stopped
//!   at the node that produced it instead of being paid for downstream.
//! * [`lazy`] — optional lazy context expansion: short upstream summaries, with the
//!   model able to ask for more (`NEED <step>`).
//!
//! The ideas follow the "compile, validate, repair-before-LLM" discipline of
//! structured workflow runtimes (cf. the Dagwood preprint), adapted to a runtime
//! whose plans are already typed and validated.

mod apply;
pub mod contracts;
pub mod cost;
pub mod lazy;
pub mod repair;

pub use cost::{CostModel, plan_model_calls};
