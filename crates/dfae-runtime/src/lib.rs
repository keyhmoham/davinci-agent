//! The Decision-First Agentic Engine runtime.
//!
//! The runtime is the agent; models are specialised cognitive services inside
//! it. See `docs/architecture.md` for the pipeline.

mod agents;
mod answer;
mod bootstrap;
mod bus;
mod dispatch;
pub mod economy;
mod engine;
mod executor;
mod implement;
mod pipeline;
pub mod planner;
pub mod prompts;
mod session;
pub mod skills;
mod style;
pub mod team;
mod verify;
mod workspace;

pub use bootstrap::build_engine;
pub use bus::EventBus;
pub use dispatch::{DispatchSnapshot, Dispatcher, PoolStat};
pub use engine::{ComponentHealth, Engine, EngineDeps, HealthReport, StatsReport};
