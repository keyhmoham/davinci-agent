//! Events streamed to UIs and written to the audit log.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    TaskCreated { goal: String },
    Status { status: TaskStatus },
    Extraction { entities: Vec<Entity>, backend: String, latency_ms: u64 },
    Retrieval { items: Vec<RetrievedRef> },
    Decision { record: DecisionRecord },
    Mode { mode: ExecMode, reason: String, skill_id: Option<String> },
    PlanCreated { plan: Plan },
    PlanRevised { plan: Plan, reason: String },
    StepUpdate { step_id: String, status: StepStatus, tool: Option<String>, summary: Option<String>, error: Option<String>, attempts: u32 },
    ApprovalRequested { approval: Approval },
    ApprovalResolved { approval_id: String, status: ApprovalStatus },
    Model { component: Component, model: String, purpose: String, prompt_tokens: u64, completion_tokens: u64, latency_ms: u64 },
    Verification { result: VerificationResult },
    Ledger { summary: LedgerSummary },
    Warning { message: String },
    Final { answer: String, status: TaskStatus },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub task_id: String,
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}
