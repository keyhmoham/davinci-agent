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
    q.insert("needs_tools".into(), Question::noul("Does fulfilling the request require tools (files, shell, web, calculation)? Creating, building, saving, running or serving anything (code, a web page, an app, a script, a document) does: deliverables are files in the workspace, not text in the chat. Only a question or conversation that needs no action does not."));
    q.insert(
        "risk".into(),
        Question::choice(
            "How risky are the actions the request implies?",
            [("low", "read-only"), ("moderate", "reversible changes"), ("high", "destructive or irreversible")],
        ),
    );
    q.insert("changes_state".into(), Question::noul("Does the request ask for something to be created, written, changed, moved, deleted, installed or started (files, code, settings, a server)? Reading, listing, searching and questions do not count."));
    q.insert(
        "conditional_change".into(),
        Question::noul("Is the change only conditional or a proposal — \"if necessary\", \"if needed\", \"suggest a fix\", \"propose a patch\" — so that doing nothing can be a correct outcome?"),
    );
    q.insert(
        "smalltalk".into(),
        Question::choice(
            "Is the message only social, with nothing to do or answer beyond a courtesy?",
            [
                ("none", "no: it asks for something, even if it starts with a greeting"),
                ("greeting", "only a greeting (hi, hello, good morning)"),
                ("thanks", "only thanks"),
                ("farewell", "only a goodbye"),
                ("how_are_you", "asks how the assistant is, and nothing else"),
                ("about_you", "asks who or what the assistant is or what it can do"),
            ],
        ),
    );
    q.insert(
        "feedback".into(),
        Question::choice(
            "Does the message judge the assistant's previous reply or result?",
            [
                ("none", "no: it is a new request or a plain follow-up"),
                (
                    "negative",
                    "yes, it was wrong, incomplete or not what was asked (\"that's wrong\", \"try again\", \"it doesn't work\", \"not what I meant\")",
                ),
                ("positive", "yes, it was right or good (\"perfect\", \"that works\", \"great, thanks\")"),
            ],
        ),
    );
    q.insert(
        "role".into(),
        Question::choice(
            "Which kind of agent fits this work best?",
            [
                ("researcher", "finds and reads information without changing anything"),
                ("operator", "changes things: writes files, runs commands that modify, deploys, patches"),
                ("analyst", "reasons or writes from information already at hand, no tools"),
                ("verifier", "checks, validates or tests that something is true or works"),
            ],
        ),
    );
    DecisionRequest { kind: DecisionKind::RouteTask, state, questions: q }
}

/// Does the request refer to something said earlier, and which of the known things is it? `options` are
/// `(key, description)` of what exists (`file`, `url`, `place`); `none` is always possible.
pub fn resolve_request(state: DecisionState, options: &[(String, String)]) -> DecisionRequest {
    let mut criteria: BTreeMap<String, String> = BTreeMap::new();
    criteria.insert("none".into(), "nothing: the request stands on its own".into());
    for (k, d) in options {
        criteria.insert(k.clone(), d.clone());
    }
    let mut q = BTreeMap::new();
    q.insert(
        "refers_to".into(),
        Question::Choice { instructions: "Does the request rely on something named earlier in the conversation instead of saying it (\"it\", \"that file\", \"the page above\", \"my city\", \"there\")? Which one?".into(), criteria },
    );
    DecisionRequest { kind: DecisionKind::ResolveReference, state, questions: q }
}

pub fn mode_request(state: DecisionState) -> DecisionRequest {
    let mut q = BTreeMap::new();
    q.insert(
        "mode".into(),
        Question::choice(
            "How should the request be handled? All planning is done by the external model, so decide whether the request needs a plan at all. direct when it binds to tools outright; escalated when it needs planning (see POLICY); assisted when it is one quick action or a reply that the local models can do alone.",
            [
                ("direct", "deterministic tools only; arguments bind straight from the text; no language model"),
                ("assisted", "quick: a single action or a reply the local models (small language model, extractor, decision model) handle alone, with no plan"),
                ("escalated", "needs a plan: several dependent steps, something to build, write and run or serve, work whose next step depends on what earlier steps find, anything open-ended or high-stakes; the external model plans it and the local system carries it out"),
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
