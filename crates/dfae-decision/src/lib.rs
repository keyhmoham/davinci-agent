//! Decision layer: typed decisions over compact state.
//!
//! * [`JevProvider`] — adapter for the hosted Jev decision model
//!   (`POST /v1/decisions`, typed `noul` / `choice` / `score` answers).
//! * [`LocalDecider`] — deterministic, rule-based decider answering the same
//!   questions; used when no Jev key is configured and as the fallback when
//!   the hosted model errors or times out.
//! * [`Calibrator`] — learns how far to trust each decision from verified
//!   outcomes (decision accuracy on isolated questions does not guarantee
//!   agent task success, so trust is measured on the agent's own trajectories).
//! * [`Decider`] — the facade the runtime uses: caching, fallback, ledger and
//!   record bookkeeping, and the engine-level decision contracts
//!   (`route_task`, `select_mode`, `select_tools`, `assess_*`).

mod calibrator;
mod decider;
mod jev;
mod local;
mod questions;

pub use calibrator::Calibrator;
pub use decider::*;
pub use jev::JevProvider;
pub use local::LocalDecider;
pub use questions::*;
