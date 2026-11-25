//! Task state, plans, checks and verification records.

use crate::ledger::Ledger;
use crate::team::TeamInfo;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Running,
    WaitingForTool,
    WaitingForApproval,
    WaitingForAgent,
    Recovering,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// The runtime validates every transition against this table.
    pub fn can_transition_to(self, next: TaskStatus) -> bool {
        use TaskStatus::*;
        if self == next {
            return true;
        }
        match self {
            Created => matches!(next, Running | Cancelled | Failed),
            Running => matches!(next, WaitingForTool | WaitingForApproval | WaitingForAgent | Recovering | Verifying | Completed | Failed | Cancelled),
            WaitingForTool => matches!(next, Running | WaitingForApproval | Recovering | Verifying | Failed | Cancelled),
            WaitingForApproval => matches!(next, Running | WaitingForTool | Recovering | Failed | Cancelled),
            WaitingForAgent => matches!(next, Running | Failed | Cancelled),
            Recovering => matches!(next, Running | WaitingForTool | WaitingForApproval | Verifying | Failed | Cancelled),
            Verifying => matches!(next, Running | Recovering | Completed | Failed | Cancelled),
            Completed | Failed | Cancelled => false,
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        f.write_str(&s)
    }
}

/// How a task is executed — the three tiers of the intelligence cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    /// Deterministic: decision model + code + tools. No generative model call.
    Direct,
    /// Decision model + SLM + tools.
    Assisted,
    /// Decision model + stronger model + verification.
    Escalated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeRequest {
    #[default]
    Auto,
    Direct,
    Assisted,
    Escalated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Moderate,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextProfile {
    Minimal,
    #[default]
    Standard,
    Reasoning,
}

impl ContextProfile {
    pub fn token_budget(self) -> usize {
        match self {
            Self::Minimal => 450,
            Self::Standard => 1300,
            Self::Reasoning => 3200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskRequest {
    pub goal: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub mode: ModeRequest,
    #[serde(default)]
    pub profile: Option<ContextProfile>,
    /// Approve every approval gate automatically (benchmarks/CI only; the
    /// server refuses it unless `DFAE_ALLOW_AUTO_APPROVE=true`).
    #[serde(default)]
    pub auto_approve: bool,
    /// Subagent delegation: `None` = decided automatically, `Some(false)` = never.
    #[serde(default)]
    pub delegate: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub label: String,
    pub text: String,
    #[serde(default)]
    pub start: Option<usize>,
    #[serde(default)]
    pub end: Option<usize>,
    #[serde(default)]
    pub score: f32,
    /// Which extractor produced it (`gliner`, `rules`, ...).
    #[serde(default)]
    pub source: String,
}

// ---------------------------------------------------------------- plans ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Tool,
    Reasoning,
    /// Delegation: the step is executed by a subagent (a child task).
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    WaitingApproval,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}

impl StepStatus {
    pub fn is_done(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 2, backoff_ms: 300 }
    }
}

/// A deterministic, evidence-producing check attached to a step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Check {
    NonEmpty,
    Contains {
        value: String,
    },
    NotContains {
        value: String,
    },
    Matches {
        pattern: String,
    },
    ExitOk,
    FileExists {
        path: String,
    },
    /// A file (not the step's output) contains a text: what an edit leaves behind.
    FileContains {
        path: String,
        value: String,
    },
}

impl Check {
    /// Parse the compact form the planner emits: `nonempty`, `contains:x`,
    /// `!contains:x`, `matches:re`, `exit0`, `exists:/path`.
    pub fn parse_compact(s: &str) -> Option<Check> {
        let s = s.trim();
        let (head, rest) = match s.split_once(':') {
            Some((h, r)) => (h.trim().to_lowercase(), Some(r.trim().to_string())),
            None => (s.to_lowercase(), None),
        };
        match (head.as_str(), rest) {
            ("nonempty", _) | ("non_empty", _) => Some(Check::NonEmpty),
            ("exit0", _) | ("exitok", _) | ("ok", _) => Some(Check::ExitOk),
            ("contains", Some(v)) if !v.is_empty() => Some(Check::Contains { value: v }),
            ("!contains", Some(v)) | ("notcontains", Some(v)) if !v.is_empty() => Some(Check::NotContains { value: v }),
            ("matches", Some(v)) if regex::Regex::new(&v).is_ok() => Some(Check::Matches { pattern: v }),
            ("exists", Some(v)) | ("fileexists", Some(v)) if !v.is_empty() => Some(Check::FileExists { path: v }),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Check::NonEmpty => "output is non-empty".into(),
            Check::Contains { value } => format!("output contains '{value}'"),
            Check::NotContains { value } => format!("output does not contain '{value}'"),
            Check::Matches { pattern } => format!("output matches /{pattern}/"),
            Check::ExitOk => "tool reported success".into(),
            Check::FileExists { path } => format!("file exists: {path}"),
            Check::FileContains { path, value } => format!("{path} contains '{value}'"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    #[serde(default)]
    pub tool: Option<String>,
    pub description: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub approval_required: bool,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default = "default_step_status")]
    pub status: StepStatus,
    #[serde(default)]
    pub attempts: u32,
    /// Full output (size-capped by the tool).
    #[serde(default)]
    pub output: Option<String>,
    /// Compressed output used when building prompts.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Class of the failure (drives recovery; survives delegation).
    #[serde(default)]
    pub error_kind: Option<crate::ErrorKind>,
    /// Deterministic repairs applied to this step (no model involved).
    #[serde(default)]
    pub repairs: u32,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// An intent from a planner that has not been turned into an action yet: the program (rules, GLiNER, the
    /// decision model, the SLM) decides the tool and its arguments right before the step runs.
    #[serde(default)]
    pub brief: bool,
    /// What success looks like, in words (from the planner); the decision model judges the result against it.
    #[serde(default)]
    pub expect: Option<String>,
    /// The planner wants the external model to do this reasoning step itself.
    #[serde(default)]
    pub strong: bool,
    /// How the local system turned the intent into an action (provenance), e.g. `rules`, `rules+slm`.
    #[serde(default)]
    pub how: Option<String>,
}

fn default_step_status() -> StepStatus {
    StepStatus::Pending
}

impl Step {
    /// Work for the self-hosted models: a brief step (not yet implemented) or one the local system implemented,
    /// unless the planner asked for the external model.
    pub fn runs_local(&self) -> bool {
        (self.brief || self.how.is_some()) && !self.strong
    }

    pub fn tool(id: &str, tool: &str, description: &str, args: Value) -> Self {
        Self {
            id: id.into(),
            kind: StepKind::Tool,
            tool: Some(tool.into()),
            description: description.into(),
            args,
            depends_on: vec![],
            approval_required: false,
            checks: vec![],
            retry: RetryPolicy::default(),
            status: StepStatus::Pending,
            attempts: 0,
            output: None,
            summary: None,
            error: None,
            error_kind: None,
            repairs: 0,
            evidence: vec![],
            started_at: None,
            finished_at: None,
            brief: false,
            expect: None,
            strong: false,
            how: None,
        }
    }

    pub fn reasoning(id: &str, description: &str) -> Self {
        let mut s = Self::tool(id, "", description, Value::Null);
        s.kind = StepKind::Reasoning;
        s.tool = None;
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSource {
    Direct,
    Skill,
    Slm,
    Strong,
    Replan,
    /// Sub-goals delegated to a team of agents.
    Team,
    /// A conversational reply: one step, no planning call.
    Chat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<Step>,
    pub source: PlanSource,
    #[serde(default)]
    pub revision: u32,
    /// What finishing this plan is expected to cost (set when the plan is accepted).
    #[serde(default)]
    pub estimate: Option<CostEstimate>,
    /// The final reply as the planner wrote it ahead of time, with `{{sN}}` where a step's output belongs.
    /// When every step succeeds it is rendered and used as the answer: no answer call is made.
    #[serde(default)]
    pub report: Option<String>,
}

/// Expected *remaining* generative cost of a plan, priced by the learned cost model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CostEstimate {
    pub model_calls: u32,
    pub tokens: u64,
    /// The calls go to the external (paid) model; otherwise they run on a self-hosted one and are free.
    #[serde(default)]
    pub external: bool,
}

impl Plan {
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }
    pub fn step_mut(&mut self, id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.id == id)
    }
    pub fn has_reasoning(&self) -> bool {
        self.steps.iter().any(|s| s.kind == StepKind::Reasoning)
    }
    pub fn tool_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.kind == StepKind::Tool).count()
    }
}

// ----------------------------------------------------- decisions, approval --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: String,
    pub kind: String,
    pub ts: DateTime<Utc>,
    /// `open-model`, `local`, or `local-fallback`.
    pub provider: String,
    /// Compact map `question -> answer` (choice/score/probability).
    pub answers: Value,
    pub latency_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routing {
    pub task_type: String,
    pub complexity: f32,
    pub needs_tools: bool,
    pub risk: Risk,
    pub confidence: f32,
    /// The request asks for something to be created, changed or deleted.
    #[serde(default)]
    pub changes: bool,
    /// ...but only if needed, or as a proposal ("patch it if necessary", "suggest a fix").
    #[serde(default)]
    pub conditional: bool,
    /// `greeting`, `thanks`, `farewell`, `how_are_you`, `about_you`: nothing to do, just a social reply.
    #[serde(default)]
    pub smalltalk: Option<String>,
    /// The kind of agent that fits the work: `researcher`, `operator`, `analyst`, `verifier`.
    #[serde(default)]
    pub role: Option<String>,
}

impl Routing {
    pub fn new(task_type: &str, complexity: f32, needs_tools: bool, risk: Risk, confidence: f32) -> Self {
        Self { task_type: task_type.into(), complexity, needs_tools, risk, confidence, changes: false, conditional: false, smalltalk: None, role: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub step_id: String,
    pub tool: String,
    pub args: Value,
    /// Approval is bound to *this exact* action.
    pub args_hash: String,
    pub reason: String,
    pub risk: Risk,
    pub status: ApprovalStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub decided_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub max_steps: u32,
    /// Calls to the **external** (paid, strong) model. Local models cost nothing, so they are not limited by this.
    pub max_model_calls: u32,
    /// Calls to the self-hosted small model: only a runaway guard.
    #[serde(default = "default_max_local_calls")]
    pub max_local_calls: u32,
    pub max_tool_calls: u32,
    pub max_replans: u32,
    pub max_seconds: u64,
    /// Subagents one task may spawn (retries included).
    #[serde(default = "default_max_children")]
    pub max_children: u32,
}

fn default_max_children() -> u32 {
    8
}

fn default_max_local_calls() -> u32 {
    200
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_steps: 30, max_model_calls: 10, max_local_calls: 200, max_tool_calls: 25, max_replans: 2, max_seconds: 600, max_children: 8 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetUsage {
    /// External-model calls.
    pub model_calls: u32,
    /// Self-hosted model calls.
    pub local_calls: u32,
    pub tool_calls: u32,
    pub replans: u32,
    pub children: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievedRef {
    pub id: String,
    pub kind: String,
    pub score: f32,
    pub preview: String,
}

// ---------------------------------------------------------- verification ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub check_id: String,
    pub description: String,
    pub status: CheckStatus,
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Verified,
    Partial,
    Unverified,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub status: VerificationStatus,
    pub checks: Vec<CheckResult>,
    pub failed_checks: Vec<String>,
    pub unverified_claims: Vec<String>,
    /// Completion judgment by the decision model (probability), if asked.
    #[serde(default)]
    pub completion_confidence: Option<f32>,
}

// ----------------------------------------------------------- task record ----

/// The durable, resumable state of one task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub schema_version: u32,
    pub task_id: String,
    pub session_id: String,
    pub status: TaskStatus,
    pub goal: String,
    pub request: TaskRequest,
    pub mode: Option<ExecMode>,
    pub mode_reason: Option<String>,
    pub route: Option<Routing>,
    pub entities: Vec<Entity>,
    pub retrieved: Vec<RetrievedRef>,
    pub decisions: Vec<DecisionRecord>,
    pub plan: Option<Plan>,
    pub approvals: Vec<Approval>,
    pub budget: Budget,
    pub usage: BudgetUsage,
    pub ledger: Ledger,
    pub verification: Option<VerificationResult>,
    pub skill_id: Option<String>,
    /// Set when untrusted tool output looked like a prompt injection.
    pub tainted: bool,
    pub final_answer: Option<String>,
    pub error: Option<String>,
    /// Team view: set on coordinators and on the agents they spawn.
    #[serde(default)]
    pub team: Option<TeamInfo>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const TASK_SCHEMA_VERSION: u32 = 1;

impl TaskRecord {
    pub fn new(request: TaskRequest, budget: Budget) -> Self {
        let now = Utc::now();
        Self {
            schema_version: TASK_SCHEMA_VERSION,
            task_id: crate::new_id(),
            session_id: request.session_id.clone().unwrap_or_else(crate::new_id),
            status: TaskStatus::Created,
            goal: request.goal.clone(),
            request,
            mode: None,
            mode_reason: None,
            route: None,
            entities: vec![],
            retrieved: vec![],
            decisions: vec![],
            plan: None,
            approvals: vec![],
            budget,
            usage: BudgetUsage::default(),
            ledger: Ledger::default(),
            verification: None,
            skill_id: None,
            tainted: false,
            final_answer: None,
            error: None,
            team: None,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_are_validated() {
        use TaskStatus::*;
        assert!(Created.can_transition_to(Running));
        assert!(Running.can_transition_to(Completed));
        assert!(!Completed.can_transition_to(Running));
        assert!(!Created.can_transition_to(Completed));
        assert!(WaitingForApproval.can_transition_to(Running));
    }

    #[test]
    fn parses_compact_checks() {
        assert_eq!(Check::parse_compact("nonempty"), Some(Check::NonEmpty));
        assert_eq!(Check::parse_compact("contains: hi"), Some(Check::Contains { value: "hi".into() }));
        assert_eq!(Check::parse_compact("exit0"), Some(Check::ExitOk));
        assert!(Check::parse_compact("matches:(").is_none());
        assert!(Check::parse_compact("bogus").is_none());
    }
}
