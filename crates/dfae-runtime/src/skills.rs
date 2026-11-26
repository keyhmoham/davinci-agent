//! Procedural memory: verified plans become reusable, parameterised skills.
//!
//! After a task is *verified*, its plan is generalised: entity values that
//! appear in the request and in step arguments become slots. A later request
//! whose masked text is close enough replays the plan with the new entity
//! values — no planning call, and (if the plan has no reasoning step) no
//! generative call at all. Replays still go through policy, approval and
//! verification like any other plan.

use dfae_core::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub masked_request: String,
    /// Slot names in order, e.g. `file_path`, `file_path_2`.
    pub slots: Vec<String>,
    pub template: Plan,
    pub successes: u32,
    pub failures: u32,
    /// The template is a plan of *intents* (written by the planner model, steps carried out by the local system), not of
    /// concrete calls: replaying it spends no external tokens and produces fresh content for the new request. Because the
    /// intents are phrased for the request they came from, such a skill only matches a request that differs from the
    /// original in its slot values.
    #[serde(default)]
    pub intent: bool,
}

/// A request reduced to its words, for the exact-shape match an intent skill requires.
pub fn shape(masked: &str) -> String {
    masked.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '{' && c != '}' && c != '_').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

const SKIP_LABELS: &[&str] = &["action", "constraint", "quoted", "deadline"];

/// `(slot name, entity text)` in order of appearance in the goal.
pub fn slot_values(goal: &str, entities: &[Entity]) -> Vec<(String, String)> {
    let mut ents: Vec<&Entity> = entities.iter().filter(|e| !SKIP_LABELS.contains(&e.label.as_str()) && goal.contains(&e.text)).collect();
    ents.sort_by_key(|e| goal.find(&e.text).unwrap_or(usize::MAX));
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    let mut out: Vec<(String, String)> = Vec::new();
    for e in ents {
        if out.iter().any(|(_, t)| t == &e.text) {
            continue;
        }
        let base = e.label.replace(' ', "_");
        let n = counts.entry(base.clone()).or_insert(0);
        *n += 1;
        out.push((if *n == 1 { base } else { format!("{base}_{n}") }, e.text.clone()));
    }
    out
}

pub fn mask_request(goal: &str, slots: &[(String, String)]) -> String {
    let mut s = goal.to_string();
    let mut by_len: Vec<&(String, String)> = slots.iter().collect();
    by_len.sort_by_key(|(_, t)| std::cmp::Reverse(t.len()));
    for (name, text) in by_len {
        s = s.replace(text.as_str(), &format!("{{{name}}}"));
    }
    s
}

fn replace_in(v: &Value, pairs: &[(String, String)]) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.clone();
            let mut sorted: Vec<&(String, String)> = pairs.iter().collect();
            sorted.sort_by_key(|(_, t)| std::cmp::Reverse(t.len()));
            for (from, to) in sorted {
                out = out.replace(from.as_str(), to.as_str());
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| replace_in(x, pairs)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), replace_in(x, pairs))).collect()),
        other => other.clone(),
    }
}

/// Generalise a verified plan into a skill, or `None` if it is not worth keeping.
pub fn crystallize(goal: &str, entities: &[Entity], plan: &Plan) -> Option<Skill> {
    if plan.tool_steps() == 0 || plan.steps.iter().any(|s| s.status != StepStatus::Succeeded) {
        return None;
    }
    let slots = slot_values(goal, entities);
    let to_placeholder: Vec<(String, String)> = slots.iter().map(|(n, t)| (t.clone(), format!("{{{{slot:{n}}}}}"))).collect();
    let mut template = plan.clone();
    template.goal = String::new();
    template.report = None;
    // a plan the planner model wrote as intents is kept as intents: what the local system generated while carrying it out
    // (the page it wrote, the code it composed) belongs to that one request and is generated again for the next
    let intent = plan.source == PlanSource::Planner && plan.steps.iter().all(|s| s.how.is_some());
    for s in &mut template.steps {
        if intent {
            s.args = serde_json::json!({});
            s.brief = true;
            s.how = None;
            s.kind = if s.tool.is_some() { StepKind::Tool } else { StepKind::Reasoning };
        }
        s.args = replace_in(&s.args, &to_placeholder);
        s.description = replace_in(&Value::String(s.description.clone()), &to_placeholder).as_str().unwrap_or_default().to_string();
        s.status = StepStatus::Pending;
        s.attempts = 0;
        s.output = None;
        s.summary = None;
        s.error = None;
        s.evidence.clear();
        s.started_at = None;
        s.finished_at = None;
    }
    Some(Skill { masked_request: mask_request(goal, &slots), slots: slots.into_iter().map(|(n, _)| n).collect(), template, successes: 1, failures: 0, intent })
}

/// Instantiate a skill for a new request. Fails if a slot cannot be filled.
pub fn instantiate(skill: &Skill, goal: &str, entities: &[Entity]) -> Option<Plan> {
    let values = slot_values(goal, entities);
    let mut pairs = Vec::new();
    for name in &skill.slots {
        let (_, text) = values.iter().find(|(n, _)| n == name)?;
        pairs.push((format!("{{{{slot:{name}}}}}"), text.clone()));
    }
    let mut plan = skill.template.clone();
    plan.goal = goal.to_string();
    plan.source = PlanSource::Skill;
    for s in &mut plan.steps {
        s.args = replace_in(&s.args, &pairs);
        s.description = replace_in(&Value::String(s.description.clone()), &pairs).as_str().unwrap_or_default().to_string();
    }
    Some(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ent(l: &str, t: &str) -> Entity {
        Entity { label: l.into(), text: t.into(), start: None, end: None, score: 1.0, source: "t".into() }
    }

    fn done_plan() -> Plan {
        let mut s1 = Step::tool("s1", "fs_read", "read /workspace/a.txt", json!({"path": "/workspace/a.txt"}));
        s1.status = StepStatus::Succeeded;
        Plan { goal: "g".into(), steps: vec![s1], source: PlanSource::Worker, revision: 0, estimate: None, report: None }
    }

    #[test]
    fn crystallize_then_instantiate_with_new_values() {
        let goal = "show me /workspace/a.txt please";
        let ents = vec![ent("file path", "/workspace/a.txt")];
        let skill = crystallize(goal, &ents, &done_plan()).unwrap();
        assert_eq!(skill.masked_request, "show me {file_path} please");
        assert_eq!(skill.slots, vec!["file_path"]);
        assert_eq!(skill.template.steps[0].args, json!({"path": "{{slot:file_path}}"}));

        let goal2 = "show me /workspace/b.md please";
        let plan = instantiate(&skill, goal2, &[ent("file path", "/workspace/b.md")]).unwrap();
        assert_eq!(plan.steps[0].args, json!({"path": "/workspace/b.md"}));
        assert_eq!(plan.source, PlanSource::Skill);
        assert_eq!(plan.steps[0].status, StepStatus::Pending);

        // a request that lacks the slot cannot use the skill
        assert!(instantiate(&skill, "show me something", &[]).is_none());
    }

    #[test]
    fn unfinished_or_toolless_plans_are_not_skills() {
        let mut p = done_plan();
        p.steps[0].status = StepStatus::Failed;
        assert!(crystallize("g", &[], &p).is_none());
        let p = Plan { goal: "g".into(), steps: vec![Step::reasoning("s1", "think")], source: PlanSource::Worker, revision: 0, estimate: None, report: None };
        assert!(crystallize("g", &[], &p).is_none());
    }
}
