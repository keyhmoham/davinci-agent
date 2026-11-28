//! Events streamed to UIs and written to the audit log.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    TaskCreated {
        goal: String,
    },
    /// The task waits for a free slot in the dispatcher.
    Queued {
        position: usize,
    },
    /// The request was split across a team of agents.
    TeamFormed {
        goals: Vec<TeamGoal>,
        reason: String,
    },
    /// A subagent (child task) was dispatched for a plan step.
    AgentSpawned {
        step_id: String,
        child_id: String,
        goal: String,
        attempt: u32,
    },
    /// (On the child's own stream) its role and tool scope were fixed.
    RoleAssigned {
        role: AgentRole,
        tools: usize,
    },
    AgentDone {
        step_id: String,
        child_id: String,
        role: AgentRole,
        status: TaskStatus,
        mode: Option<ExecMode>,
        generative_tokens: u64,
    },
    Status {
        status: TaskStatus,
    },
    Extraction {
        entities: Vec<Entity>,
        backend: String,
        latency_ms: u64,
    },
    Retrieval {
        items: Vec<RetrievedRef>,
    },
    Decision {
        record: DecisionRecord,
    },
    Mode {
        mode: ExecMode,
        reason: String,
        skill_id: Option<String>,
    },
    PlanCreated {
        plan: Plan,
    },
    PlanRevised {
        plan: Plan,
        reason: String,
    },
    StepUpdate {
        step_id: String,
        status: StepStatus,
        tool: Option<String>,
        summary: Option<String>,
        error: Option<String>,
        attempts: u32,
    },
    ApprovalRequested {
        approval: Approval,
    },
    ApprovalResolved {
        approval_id: String,
        status: ApprovalStatus,
    },
    Model {
        component: Component,
        model: String,
        purpose: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        latency_ms: u64,
    },
    Verification {
        result: VerificationResult,
    },
    Ledger {
        summary: LedgerSummary,
    },
    /// The task moved to another phase: `planning` (a planner writes the brief plan), `acting` (the local system
    /// implements and runs each step), `reviewing` (the result is judged against the request).
    Phase {
        phase: String,
        detail: String,
    },
    /// The local system turned a planner's intent into an action.
    StepImplemented {
        step_id: String,
        /// `rules`, `rules+worker`, `rules+worker+planner`, `reasoning`, `plan` (the planner already gave valid arguments)…
        how: String,
        tool: Option<String>,
        args: String,
    },
    /// A step was repaired deterministically (no model call) and will be re-run.
    Repaired {
        step_id: String,
        note: String,
    },
    /// An MCP server's tools became available to this task.
    McpConnected {
        server: String,
        tools: Vec<String>,
        /// `config`, `registry` (connected earlier) or `glama` (found in the directory for this task).
        source: String,
        /// The server's own page in the MCP directory it was found in (the directory's licence asks for the link).
        listing: Option<String>,
    },
    /// The code this task wrote was tested (`round` 0 first, then after each repair).
    Validated {
        round: u32,
        checks: Vec<CheckResult>,
    },
    /// An MCP server this task needs wants the user to sign in: open `url` (mcpc's sign-in), sign in, and the task goes on by
    /// itself once mcpc has the tokens.
    McpSignInRequired {
        server: String,
        title: String,
        url: String,
        listing: Option<String>,
        /// How long the task waits, in seconds.
        wait_secs: u64,
    },
    /// The user signed in (`ok`), or the sign-in failed or was not done in time.
    McpSignedIn {
        server: String,
        ok: bool,
        detail: String,
    },
    Warning {
        message: String,
    },
    /// A routine was added, changed, removed, started a run, finished one or skipped one (`task_id` of the event is `routines`).
    Routine {
        routine_id: String,
        /// `added`, `proposed`, `updated`, `removed`, `started`, `finished`, `skipped`, `ended`.
        action: String,
        detail: String,
    },
    Final {
        answer: String,
        status: TaskStatus,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub task_id: String,
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}
