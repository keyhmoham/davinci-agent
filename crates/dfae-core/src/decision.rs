//! The decision contract.
//!
//! `Question`/`Answer` mirror the wire format of typed decision models such as
//! typed decision models (`noul` yes/no probability, `choice`, `score`).
//! Engine-level decisions (route, mode, tools, recovery,
//! completion, escalation) are expressed as a set of questions over a compact,
//! typed `DecisionState`, which a local rule-based decider can also answer —
//! that is what makes the decision layer pluggable and gives a fallback when
//! the decision model is unavailable.

use crate::{DecisionError, Risk};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    RouteTask,
    SelectMode,
    SelectTools,
    AssessPlan,
    AssessRecovery,
    AssessCompletion,
    AssessEscalation,
    AssessDecomposition,
    /// Does the request refer to something said earlier ("it", "that file", "my city")? What?
    ResolveReference,
}

impl DecisionKind {
    pub const ALL: [DecisionKind; 9] = [
        Self::RouteTask,
        Self::SelectMode,
        Self::SelectTools,
        Self::AssessPlan,
        Self::AssessRecovery,
        Self::AssessCompletion,
        Self::AssessEscalation,
        Self::AssessDecomposition,
        Self::ResolveReference,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s.trim())
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RouteTask => "route_task",
            Self::SelectMode => "select_mode",
            Self::SelectTools => "select_tools",
            Self::AssessPlan => "assess_plan",
            Self::AssessRecovery => "assess_recovery",
            Self::AssessCompletion => "assess_completion",
            Self::AssessEscalation => "assess_escalation",
            Self::AssessDecomposition => "assess_decomposition",
            Self::ResolveReference => "resolve_reference",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    pub yes: Option<String>,
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    pub no: Option<String>,
}

/// A typed question.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    Score {
        instructions: String,
        /// Ordered levels, lowest first (2..=10).
        criteria: Vec<String>,
    },
}

impl Question {
    pub fn noul(instructions: impl Into<String>) -> Self {
        Question::Noul { instructions: instructions.into(), criteria: None }
    }
    pub fn choice<const N: usize>(instructions: impl Into<String>, options: [(&str, &str); N]) -> Self {
        Question::Choice { instructions: instructions.into(), criteria: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }
    pub fn score<const N: usize>(instructions: impl Into<String>, levels: [&str; N]) -> Self {
        Question::Score { instructions: instructions.into(), criteria: levels.iter().map(|s| s.to_string()).collect() }
    }
}

/// A typed answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legend: Option<serde_json::Value>,
    },
}

impl Answer {
    pub fn noul(p: f64) -> Self {
        Answer::Noul { noul: p.clamp(0.0, 1.0) }
    }
    pub fn choice(choice: &str, confidence: f64, probs: &[(&str, f64)]) -> Self {
        Answer::Choice {
            choice: choice.into(),
            confidence: confidence.clamp(0.0, 1.0),
            probabilities: probs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }
    pub fn score(score: f64, confidence: f64) -> Self {
        Answer::Score { score, confidence: confidence.clamp(0.0, 1.0), probabilities: BTreeMap::new(), legend: None }
    }
    /// Compact rendering for logs and events.
    pub fn brief(&self) -> serde_json::Value {
        match self {
            Answer::Noul { noul } => serde_json::json!(round2(*noul)),
            Answer::Choice { choice, confidence, .. } => serde_json::json!(format!("{choice} ({:.2})", confidence)),
            Answer::Score { score, confidence, .. } => serde_json::json!(format!("{:.2} ({:.2})", score, confidence)),
        }
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// ---------------------------------------------------------------- state -----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityBrief {
    pub label: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolBrief {
    pub name: String,
    pub description: String,
    pub category: String,
    pub risk: Risk,
    pub side_effects: bool,
    /// Lexical hints for local tool selection (not sent to hosted models).
    #[serde(default, skip_serializing)]
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModeFacts {
    /// Every needed tool call can be bound deterministically (no worker model needed).
    pub direct_bindable: bool,
    pub direct_tools: Vec<String>,
    /// Cosine score of the best matching stored skill, if any.
    pub skill_score: Option<f32>,
    pub planner_available: bool,
    pub worker_available: bool,
    pub complexity: f32,
    pub risk: Option<Risk>,
    pub task_type: String,
    /// Policy: from this complexity, medium and complex work is planned by the planner model (if there is one).
    #[serde(default)]
    pub plan_min_complexity: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanFacts {
    pub steps: usize,
    pub tool_steps: usize,
    pub reasoning_steps: usize,
    pub side_effect_steps: usize,
    pub max_risk: Option<Risk>,
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepFacts {
    pub step_id: String,
    pub tool: Option<String>,
    pub error: String,
    pub error_kind: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub replans_used: u32,
    pub max_replans: u32,
    pub optional: bool,
    pub planner_available: bool,
}

/// Facts about a candidate split of a request into sub-tasks for a team of agents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TeamFacts {
    /// Sub-goal texts (already shortened).
    pub subgoals: Vec<String>,
    /// How many bind deterministically (no generative model needed).
    pub direct: usize,
    /// How many depend on an earlier sub-goal (pipeline, not parallel).
    pub dependent: usize,
    /// Model-needing sub-goals with no dependencies (they can run in parallel).
    pub parallel_model: usize,
    /// Model-needing sub-goals that wait on a deterministic one — a single plan
    /// handles those just as well, so splitting them gains nothing.
    pub model_after_direct: usize,
    /// Sub-goals that change state (patch, write, deploy, …).
    pub mutating: usize,
    pub has_synthesis: bool,
    /// A generative model exists for the sub-goals that need one.
    pub models_available: bool,
    /// Learned-cost estimates (tokens) of doing the model-needing work as one plan
    /// versus as a team: every agent pays its own planning call, one plan pays once.
    #[serde(default)]
    pub est_flat_tokens: u64,
    #[serde(default)]
    pub est_team_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionFacts {
    pub steps_ok: usize,
    pub steps_failed: usize,
    pub steps_total: usize,
    pub checks_passed: usize,
    pub checks_failed: usize,
    pub evidence: String,
}

/// Compact, typed state handed to a decision provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionState {
    pub request: String,
    /// The last few turns of the conversation (oldest first), so "now do the same for b.txt" can be judged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<String>,
    /// Things named earlier in the conversation that a request could refer to (`file: a.txt`, `place: Paris`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<EntityBrief>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolBrief>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_facts: Option<ModeFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_facts: Option<PlanFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_facts: Option<StepFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_facts: Option<CompletionFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_facts: Option<TeamFacts>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub kind: DecisionKind,
    pub state: DecisionState,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, Default)]
pub struct DecisionResponse {
    pub answers: BTreeMap<String, Answer>,
    pub provider: String,
    /// The model that answered (empty for the local rules).
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
}

impl DecisionResponse {
    pub fn noul(&self, key: &str) -> Option<f64> {
        match self.answers.get(key)? {
            Answer::Noul { noul } => Some(*noul),
            _ => None,
        }
    }
    pub fn choice(&self, key: &str) -> Option<(&str, f64)> {
        match self.answers.get(key)? {
            Answer::Choice { choice, confidence, .. } => Some((choice.as_str(), *confidence)),
            _ => None,
        }
    }
    pub fn score(&self, key: &str) -> Option<(f64, f64)> {
        match self.answers.get(key)? {
            Answer::Score { score, confidence, .. } => Some((*score, *confidence)),
            _ => None,
        }
    }
}

#[async_trait]
pub trait DecisionProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError>;
    /// `Some(true/false)` if the provider can tell whether its model is reachable and loaded.
    async fn health(&self) -> Option<bool> {
        None
    }
}
