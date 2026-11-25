//! Plan construction, parsing and validation. A plan is a *proposal*: it is
//! schema-validated, restricted to the tools selected for the task, and
//! budget-checked before anything executes.

use dfae_core::*;
use dfae_tools::{DirectCall, ToolRegistry};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static TEMPLATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{\s*([A-Za-z][A-Za-z0-9_]*)\s*\}\}").unwrap());

/// Step ids referenced from `{{id}}` templates anywhere in `v`.
pub fn template_refs(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                for c in TEMPLATE.captures_iter(s) {
                    out.push(c[1].to_string());
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(m) => m.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

/// Substitute `{{id}}` templates using `lookup`.
pub fn substitute(v: &Value, lookup: &dyn Fn(&str) -> Option<String>) -> Value {
    match v {
        Value::String(s) => Value::String(TEMPLATE.replace_all(s, |c: &regex::Captures| lookup(&c[1]).unwrap_or_default()).into_owned()),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, lookup)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), substitute(x, lookup))).collect()),
        other => other.clone(),
    }
}

/// Plan from deterministic bindings (Direct mode).
pub fn plan_from_calls(goal: &str, calls: &[DirectCall]) -> Plan {
    let mut steps: Vec<Step> = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        let mut s = Step::tool(&format!("s{}", i + 1), &c.tool, &c.clause, c.args.clone());
        s.checks = vec![Check::ExitOk];
        if c.after_prev && i > 0 {
            s.depends_on = vec![format!("s{i}")];
        }
        steps.push(s);
    }
    Plan { goal: goal.into(), steps, source: PlanSource::Direct, revision: 0 }
}

/// Parse a model-produced plan. `prefix` is prepended to every new step id
/// (replans); dependencies on ids outside the new set are kept verbatim.
pub fn parse_plan(v: &Value, goal: &str, source: PlanSource, prefix: &str, registry: &ToolRegistry) -> Result<Vec<Step>, String> {
    let arr = v.get("steps").and_then(|s| s.as_array()).ok_or("missing 'steps' array")?;
    let raw_ids: Vec<String> = arr
        .iter()
        .enumerate()
        .map(|(i, s)| s.get("id").and_then(|x| x.as_str()).map(String::from).unwrap_or_else(|| format!("s{}", i + 1)))
        .collect();
    let new_ids: HashSet<&String> = raw_ids.iter().collect();
    let rename = |id: &str| -> String { if new_ids.contains(&id.to_string()) { format!("{prefix}{id}") } else { id.to_string() } };
    let mut steps = Vec::new();
    for (i, s) in arr.iter().enumerate() {
        let id = format!("{prefix}{}", raw_ids[i]);
        let tool = s.get("tool").and_then(|t| t.as_str()).map(str::trim).filter(|t| !t.is_empty() && *t != "null" && *t != "none");
        let desc = s.get("do").or_else(|| s.get("description")).or_else(|| s.get("action")).and_then(|d| d.as_str()).unwrap_or("").to_string();
        let args = s.get("args").cloned().unwrap_or_else(|| json!({}));
        let args = if args.is_null() { json!({}) } else { args };
        let mut step = match tool {
            Some(t) => {
                let mut st = Step::tool(&id, t, if desc.is_empty() { t } else { &desc }, args);
                if let Some(m) = registry.meta(t) {
                    st.approval_required = m.requires_approval;
                }
                st
            }
            None => Step::reasoning(&id, if desc.is_empty() { "reason about the inputs" } else { &desc }),
        };
        let mut deps: Vec<String> = s
            .get("after")
            .or_else(|| s.get("depends_on"))
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(&rename).collect())
            .unwrap_or_default();
        // template references imply dependencies
        for r in template_refs(&step.args) {
            let r = rename(&r);
            if !deps.contains(&r) {
                deps.push(r);
            }
        }
        // args may reference ids relative to the new set: rewrite them too
        if !prefix.is_empty() {
            step.args = substitute_ids(&step.args, &rename);
        }
        deps.retain(|d| d != &id);
        step.depends_on = deps;
        if let Some(cs) = s.get("check").or_else(|| s.get("checks")).and_then(|c| c.as_array()) {
            step.checks = cs.iter().filter_map(|c| c.as_str()).filter_map(Check::parse_compact).collect();
        }
        if s.get("approval").and_then(|a| a.as_bool()).unwrap_or(false) {
            step.approval_required = true;
        }
        steps.push(step);
    }
    let _ = (goal, source);
    Ok(steps)
}

fn substitute_ids(v: &Value, rename: &dyn Fn(&str) -> String) -> Value {
    match v {
        Value::String(s) => Value::String(TEMPLATE.replace_all(s, |c: &regex::Captures| format!("{{{{{}}}}}", rename(&c[1]))).into_owned()),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute_ids(x, rename)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), substitute_ids(x, rename))).collect()),
        other => other.clone(),
    }
}

/// Validate a full plan. Returns every problem found (so the repair prompt can
/// list them).
pub fn validate_plan(steps: &[Step], registry: &ToolRegistry, allowed: &[String], budget: &Budget) -> Result<(), Vec<String>> {
    let mut errs = Vec::new();
    if steps.len() as u32 > budget.max_steps {
        errs.push(format!("plan has {} steps; the limit is {}", steps.len(), budget.max_steps));
    }
    let tool_steps = steps.iter().filter(|s| s.kind == StepKind::Tool).count() as u32;
    if tool_steps > budget.max_tool_calls {
        errs.push(format!("plan has {tool_steps} tool steps; the limit is {}", budget.max_tool_calls));
    }
    let mut ids = HashSet::new();
    for s in steps {
        if !ids.insert(s.id.as_str()) {
            errs.push(format!("duplicate step id '{}'", s.id));
        }
    }
    for s in steps {
        for d in &s.depends_on {
            if !ids.contains(d.as_str()) {
                errs.push(format!("step {} depends on unknown step '{d}'", s.id));
            }
        }
        if s.kind == StepKind::Tool {
            let name = s.tool.as_deref().unwrap_or("");
            match registry.meta(name) {
                None => errs.push(format!("step {}: unknown tool '{name}'", s.id)),
                Some(m) => {
                    if !allowed.iter().any(|a| a == name) {
                        errs.push(format!("step {}: tool '{name}' is not available for this task", s.id));
                    }
                    // arguments that contain templates are validated at run time
                    let has_template = !template_refs(&s.args).is_empty();
                    if !has_template {
                        if let Err(e) = m.validate_args(&s.args) {
                            errs.push(format!("step {} ({name}): {e}", s.id));
                        }
                    }
                }
            }
        }
    }
    if let Some(cycle) = find_cycle(steps) {
        errs.push(format!("dependency cycle involving {cycle}"));
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

/// Kahn's algorithm; returns an id on a cycle.
fn find_cycle(steps: &[Step]) -> Option<String> {
    let mut indeg: HashMap<&str, usize> = steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    for s in steps {
        for d in &s.depends_on {
            if indeg.contains_key(d.as_str()) {
                *indeg.get_mut(s.id.as_str()).unwrap() += 1;
            }
        }
    }
    let mut queue: Vec<&str> = indeg.iter().filter(|(_, n)| **n == 0).map(|(k, _)| *k).collect();
    let mut seen = 0;
    while let Some(n) = queue.pop() {
        seen += 1;
        for s in steps {
            if s.depends_on.iter().any(|d| d == n) {
                let e = indeg.get_mut(s.id.as_str()).unwrap();
                *e -= 1;
                if *e == 0 {
                    queue.push(s.id.as_str());
                }
            }
        }
    }
    if seen == steps.len() { None } else { steps.iter().find(|s| indeg[s.id.as_str()] > 0).map(|s| s.id.clone()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfae_memory::InMemoryStore;
    use dfae_tools::{register_native, PolicyConfig, PolicyEngine};
    use std::sync::Arc;

    fn registry() -> ToolRegistry {
        let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), std::env::temp_dir()));
        let mut r = ToolRegistry::new();
        register_native(&mut r, policy, Arc::new(InMemoryStore::new()));
        r
    }

    fn allowed(r: &ToolRegistry) -> Vec<String> {
        r.names()
    }

    #[test]
    fn parses_and_validates_a_good_plan() {
        let r = registry();
        let v = json!({"steps": [
            {"id": "s1", "tool": "fs_read", "do": "read", "args": {"path": "a.txt"}, "check": ["nonempty"]},
            {"id": "s2", "tool": null, "do": "summarise", "after": ["s1"]},
            {"id": "s3", "tool": "calc", "args": {"expression": "{{s1}}"}}
        ]});
        let steps = parse_plan(&v, "g", PlanSource::Slm, "", &r).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1].kind, StepKind::Reasoning);
        assert_eq!(steps[2].depends_on, vec!["s1"], "template reference implies a dependency");
        assert_eq!(steps[0].checks, vec![Check::NonEmpty]);
        validate_plan(&steps, &r, &allowed(&r), &Budget::default()).unwrap();
    }

    #[test]
    fn rejects_bad_plans() {
        let r = registry();
        let bad = |v: Value| {
            let steps = parse_plan(&v, "g", PlanSource::Slm, "", &r).unwrap();
            validate_plan(&steps, &r, &["fs_read".to_string()], &Budget::default()).unwrap_err()
        };
        assert!(bad(json!({"steps": [{"id": "s1", "tool": "rm_rf", "args": {}}]})).iter().any(|e| e.contains("unknown tool")));
        assert!(bad(json!({"steps": [{"id": "s1", "tool": "calc", "args": {"expression": "1"}}]})).iter().any(|e| e.contains("not available")));
        assert!(bad(json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {}}]})).iter().any(|e| e.contains("missing required")));
        assert!(bad(json!({"steps": [{"id": "s1", "tool": null, "after": ["s2"]}, {"id": "s2", "tool": null, "after": ["s1"]}]})).iter().any(|e| e.contains("cycle")));
        assert!(bad(json!({"steps": [{"id": "s1", "tool": null, "after": ["zzz"]}]})).iter().any(|e| e.contains("unknown step")));
    }

    #[test]
    fn replan_prefixes_new_ids_but_keeps_done_refs() {
        let r = registry();
        let v = json!({"steps": [
            {"id": "s1", "tool": "fs_list", "args": {"path": "."}},
            {"id": "s2", "tool": null, "after": ["s1", "s9"]}
        ]});
        let steps = parse_plan(&v, "g", PlanSource::Replan, "r1_", &r).unwrap();
        assert_eq!(steps[0].id, "r1_s1");
        assert_eq!(steps[1].depends_on, vec!["r1_s1", "s9"]);
    }

    #[test]
    fn substitutes_templates() {
        let v = json!({"a": "x {{s1}} y", "b": ["{{s2}}"]});
        let out = substitute(&v, &|id| Some(format!("<{id}>")));
        assert_eq!(out, json!({"a": "x <s1> y", "b": ["<s2>"]}));
    }
}
