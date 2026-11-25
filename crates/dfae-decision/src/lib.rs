//! Decision layer: typed decisions over compact state.
//!
//! * [`OpenModelProvider`] — a self-hosted open-weight instruct model served over an
//!   OpenAI-compatible API; typed answers are read from the model's token
//!   log-probabilities, so they are real probabilities and always schema-valid.
//! * [`LocalDecider`] — deterministic, rule-based decider answering the same
//!   questions; used when no decision model is configured and as the fallback when
//!   the hosted model errors or times out.
//! * [`Calibrator`] — learns how far to trust each decision from verified
//!   outcomes (decision accuracy on isolated questions does not guarantee
//!   agent task success, so trust is measured on the agent's own trajectories).
//! * [`Decider`] — the facade the runtime uses: caching, fallback, ledger and
//!   record bookkeeping, and the engine-level decision contracts
//!   (`route_task`, `select_mode`, `select_tools`, `assess_*`).

mod calibrator;
mod decider;
mod local;
mod open;
mod questions;

pub use calibrator::Calibrator;
pub use decider::*;
pub use local::LocalDecider;
pub use open::OpenModelProvider;
pub use questions::*;
