//! Team orchestration types: roles, delegation links and the team view of a task.

use crate::{Entity, TaskStatus};
use serde::{Deserialize, Serialize};

/// What a subagent is allowed and expected to do. A role is a *capability
/// class*: it narrows the tools a child can use (least privilege) and selects
/// the worker pool that bounds its concurrency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Decomposes the request, dispatches agents, joins their results.
    Coordinator,
    /// Read-only information gathering (files, web, memory, diagnostics, maths).
    Researcher,
    /// May change things (writes, mutating commands). Pool size 1 by default:
    /// side-effecting agents never run concurrently.
    Operator,
    /// Reasoning only — no tools.
    Analyst,
    /// Independent read-only check of something that was claimed or changed.
    Verifier,
}

impl AgentRole {
    pub const WORKERS: [AgentRole; 4] = [AgentRole::Researcher, AgentRole::Operator, AgentRole::Analyst, AgentRole::Verifier];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Researcher => "researcher",
            Self::Operator => "operator",
            Self::Analyst => "analyst",
            Self::Verifier => "verifier",
        }
    }
    pub fn letter(self) -> char {
        match self {
            Self::Coordinator => 'T',
            Self::Researcher => 'R',
            Self::Operator => 'O',
            Self::Analyst => 'A',
            Self::Verifier => 'V',
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "coordinator" => Self::Coordinator,
            "researcher" => Self::Researcher,
            "operator" => Self::Operator,
            "analyst" => Self::Analyst,
            "verifier" => Self::Verifier,
            _ => return None,
        })
    }
}

/// Result of an upstream agent, handed to a dependent one as compact context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Upstream {
    pub label: String,
    pub summary: String,
}

/// Present on a child task: who spawned it and what it inherits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamLink {
    pub parent_id: String,
    pub parent_step: String,
    pub depth: u8,
    #[serde(default)]
    pub upstream: Vec<Upstream>,
    /// Entities sliced from the parent (so children skip extraction).
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub inherited_taint: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildRef {
    pub task_id: String,
    pub step_id: String,
    pub goal: String,
    #[serde(default)]
    pub role: Option<AgentRole>,
    #[serde(default)]
    pub mode: Option<crate::ExecMode>,
    pub status: TaskStatus,
}

/// Team view of a task: coordinator (has `children`) or worker (has `link`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamInfo {
    pub role: AgentRole,
    #[serde(default)]
    pub link: Option<TeamLink>,
    #[serde(default)]
    pub children: Vec<ChildRef>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub cache_hits: u32,
}

impl TeamInfo {
    pub fn coordinator(reason: impl Into<String>) -> Self {
        Self { role: AgentRole::Coordinator, link: None, children: vec![], reason: Some(reason.into()), cache_hits: 0 }
    }
}

/// One sub-goal of a decomposition (for events and the UI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamGoal {
    pub id: String,
    pub goal: String,
    pub depends_on: Vec<String>,
    /// Binds deterministically: the agent will need no generative model.
    pub direct: bool,
    /// The final join/synthesis step.
    pub synth: bool,
}
