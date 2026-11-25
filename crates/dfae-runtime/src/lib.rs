//! The Decision-First Agentic Engine runtime.
//!
//! The runtime is the agent; models are specialised cognitive services inside
//! it. See `docs/architecture.md` for the pipeline.

mod answer;
mod bootstrap;
mod bus;
mod engine;
mod executor;
mod pipeline;
pub mod planner;
pub mod prompts;
pub mod skills;
mod verify;

pub use bootstrap::build_engine;
pub use bus::EventBus;
pub use engine::{ComponentHealth, Engine, EngineDeps, HealthReport, StatsReport};
