//! Builders for the engine's decision questions (shared by every provider).

use dfae_core::*;
use std::collections::BTreeMap;

pub const TASK_TYPES: [&str; 10] =
    ["research", "coding", "system_diagnostics", "file_operations", "calculation", "data_analysis", "maintenance", "memory", "conversation", "other"];

pub fn route_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert(
        "task_type".into(),
        Question::choice(
            "Which kind of task is the request?",
            [
                ("research", "look up or summarise information"),
                ("coding", "write, read or change code"),
                ("system_diagnostics", "inspect a machine or service state"),
                ("file_operations", "list, read, search or write files"),
                ("calculation", "arithmetic or numeric evaluation"),
                ("data_analysis", "analyse or compare data"),
                ("maintenance", "change, patch, deploy or restart something"),
                ("memory", "recall or store knowledge"),
                ("conversation", "chat or explain without tools"),
                ("other", "none of the above"),
            ],
        ),
    );
    q.insert(
        "complexity".into(),
        Question::score(
            "How complex is the request?",
            ["simple: one deterministic step", "intermediate: a few dependent steps", "complex: open-ended multi-step reasoning"],
        ),
    );
    q.insert("needs_tools".into(), Question::noul("Does fulfilling the request require running tools (files, shell, web, calculation)?"));
    q.insert(
        "risk".into(),
        Question::choice(
            "How risky are the actions the request implies?",
            [("low", "read-only"), ("moderate", "reversible changes"), ("high", "destructive or irreversible")],
        ),
    );
    DecisionRequest { kind: DecisionKind::RouteTask, state, questions: q }
}

pub fn mode_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert(
        "mode".into(),
        Question::choice(
            "Choose the cheapest execution mode that can do this reliably.",
            [
                ("direct", "deterministic tools only; arguments bind straight from the text; no language model"),
                ("assisted", "a small language model must plan or write"),
                ("escalated", "complex, ambiguous or high-stakes; needs a stronger model"),
            ],
        ),
    );
    DecisionRequest { kind: DecisionKind::SelectMode, state, questions: q }
}

pub fn tool_key(name: &str) -> String {
    format!("tool:{name}")
}

pub fn tools_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    for t in &state.tools {
        q.insert(tool_key(&t.name), Question::noul(format!("Is the tool `{}` ({}) needed for the request?", t.name, t.description)));
    }
    DecisionRequest { kind: DecisionKind::SelectTools, state, questions: q }
}

pub fn plan_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert("plan_ok".into(), Question::noul("Is the plan sufficient to achieve the request, and safe?"));
    DecisionRequest { kind: DecisionKind::AssessPlan, state, questions: q }
}

pub const RECOVERY_OPTIONS: [&str; 5] = ["retry", "replan", "skip", "escalate", "fail"];

pub fn recovery_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert(
        "recovery".into(),
        Question::choice(
            "A step failed. What is the best recovery?",
            [
                ("retry", "the failure looks transient"),
                ("replan", "the approach is wrong; produce a new plan"),
                ("skip", "the step is optional"),
                ("escalate", "needs a stronger model"),
                ("fail", "cannot be recovered or was forbidden"),
            ],
        ),
    );
    DecisionRequest { kind: DecisionKind::AssessRecovery, state, questions: q }
}

pub fn completion_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert("complete".into(), Question::noul("Does the evidence show the request has been fully accomplished?"));
    DecisionRequest { kind: DecisionKind::AssessCompletion, state, questions: q }
}

pub fn decomposition_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert(
        "split".into(),
        Question::noul("Should the request be split into the listed sub-tasks, each handled by its own specialist agent, instead of planned as one task? Prefer splitting when parts are independent or some need no language model."),
    );
    DecisionRequest { kind: DecisionKind::AssessDecomposition, state, questions: q }
}

pub fn escalation_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert("escalate".into(), Question::noul("Should this task be escalated to a stronger model?"));
    DecisionRequest { kind: DecisionKind::AssessEscalation, state, questions: q }
}
