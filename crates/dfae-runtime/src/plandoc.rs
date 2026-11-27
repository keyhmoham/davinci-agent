//! The plan as a typed document.
//!
//! The planner model answers with one JSON object whose shape is published as a JSON Schema (`plan_schema`), so a provider
//! that supports structured output cannot produce anything else — tools are an `enum` of the tools that exist. A reply is
//! still never trusted: [`PlanDoc::from_value`] reads it leniently into typed steps, repairs what code can repair (tool
//! spellings, duplicate ids, dependencies on steps that do not exist, a model that wraps or renames the list), and reports
//! what it cannot as typed issues, which are what the planner is told in the repair round.
//!
//! Re-planning uses a *patch* ([`PatchDoc`]): edit a step, add steps, drop steps. Only what changes is paid for.

use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub struct StepDoc {
    pub id: String,
    /// `None`: a step that thinks or writes (the worker model does it).
    pub tool: Option<String>,
    /// The intent, one sentence with every concrete value in it.
    pub intent: String,
    pub after: Vec<String>,
    pub expect: Option<String>,
    /// The planner keeps this step for itself (it needs the planner's own reasoning).
    pub planner: bool,
    /// Arguments the planner volunteered (not part of the schema): kept as hints for the worker, never required.
    pub args: Option<Value>,
}

impl StepDoc {
    pub fn to_value(&self) -> Value {
        let mut v = json!({"id": self.id, "tool": self.tool, "do": self.intent, "after": self.after, "expect": self.expect, "planner": self.planner});
        if let Some(a) = self.args.as_ref().filter(|a| a.as_object().is_some_and(|o| !o.is_empty())) {
            v["args"] = a.clone();
        }
        v
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanDoc {
    pub steps: Vec<StepDoc>,
    pub report: Option<String>,
}

impl PlanDoc {
    pub fn to_value(&self) -> Value {
        let mut v = json!({"steps": self.steps.iter().map(StepDoc::to_value).collect::<Vec<_>>()});
        if let Some(r) = &self.report {
            v["report"] = json!(r);
        }
        v
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PatchDoc {
    pub edit: Vec<StepDoc>,
    pub add: Vec<StepDoc>,
    pub drop: Vec<String>,
}

/// What was repaired (for the log) and what could not be (for the planner).
#[derive(Debug, Default, Clone)]
pub struct Reading {
    pub repairs: Vec<String>,
    pub issues: Vec<String>,
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn first<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k)).filter(|x| !x.is_null())
}

fn is_nothing(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "" | "null" | "none" | "-" | "n/a" | "reasoning" | "think" | "llm" | "model")
}

/// The longest intent a step may have.
const MAX_INTENT_CHARS: usize = 700;

/// `cat > f <<'EOF'`, `echo … > f`, `tee f`, `printf … > f`, "write … using a heredoc": a file written by the shell.
fn writes_a_file_through_the_shell(intent: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)<<-?\s*['\x22]?[A-Z_]+['\x22]?|\bcat\s*>{1,2}\s*\S|\btee\s+(?:-a\s+)?\S+\.\w+|\b(?:echo|printf)\b[^\n|;&]*>{1,2}\s*[\w./\-]+\.\w+|\bheredoc\b").unwrap()
    });
    RE.is_match(intent)
}

fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1])));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// A tool name as the planner wrote it, mapped to one that exists: `fs-write`, `FS_WRITE`, `functions.fs_write`, `fswrite`, a
/// one-letter slip. `None` when nothing is close enough to be sure.
pub fn resolve_tool(written: &str, tools: &[String]) -> Option<String> {
    let w = written.trim().trim_start_matches("functions.").to_ascii_lowercase().replace(['-', ' ', '.'], "_");
    if let Some(t) = tools.iter().find(|t| **t == w) {
        return Some(t.clone());
    }
    let squashed = w.replace('_', "");
    if let Some(t) = tools.iter().find(|t| t.replace('_', "") == squashed) {
        return Some(t.clone());
    }
    let mut near: Vec<(&String, usize)> = tools.iter().map(|t| (t, lev(t, &w))).filter(|(_, d)| *d <= 2 && w.len() >= 5).collect();
    near.sort_by_key(|(_, d)| *d);
    match near.as_slice() {
        [(t, _)] => Some((*t).clone()),
        [(t, d1), (_, d2), ..] if d1 < d2 => Some((*t).clone()),
        _ => None,
    }
}

fn step_from(v: &Value, position: usize, tools: &[String], r: &mut Reading) -> Option<StepDoc> {
    if !v.is_object() {
        r.issues.push(format!("step {} is not an object", position + 1));
        return None;
    }
    let id = first(v, &["id", "step", "name"]).and_then(text).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("s{}", position + 1));
    let mut intent = first(v, &["do", "intent", "description", "action", "task"]).and_then(text).unwrap_or_default();
    let args = v.get("args").filter(|a| a.is_object()).cloned();
    let tool = match first(v, &["tool", "tool_name", "action_tool"]).and_then(text).filter(|t| !is_nothing(t)) {
        None => None,
        Some(t) => match resolve_tool(&t, tools) {
            Some(known) => {
                if known != t {
                    r.repairs.push(format!("step {id}: tool \"{t}\" is {known}"));
                }
                Some(known)
            }
            None => {
                let mut near: Vec<&String> = tools.iter().filter(|k| lev(k, &t.to_ascii_lowercase()) <= 5 || k.contains(t.as_str())).take(4).collect();
                near.sort();
                r.issues.push(format!(
                    "step {id}: there is no tool \"{t}\" (tools: {}); use one of them, or null for a step that thinks or writes",
                    if near.is_empty() { tools.join(", ") } else { near.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ") }
                ));
                return None;
            }
        },
    };
    if intent.is_empty() {
        // a step that names its tool and arguments but forgot the sentence: the sentence is made from them
        match (&tool, &args) {
            (Some(t), Some(a)) if a.as_object().is_some_and(|o| !o.is_empty()) => {
                intent = format!("{t} {a}");
                r.repairs.push(format!("step {id} had no \"do\": its sentence is made from its tool and arguments"));
            }
            _ => {
                r.issues.push(format!("step {id} has no \"do\" (the intent, one sentence with every concrete value in it)"));
                return None;
            }
        }
    }
    // A step is an intent, not a payload: content written into the plan is paid for at the planner's price, and a shell heredoc
    // is a file written the wrong way. Both go back to the planner as typed issues.
    if intent.chars().count() > MAX_INTENT_CHARS {
        r.issues.push(format!(
            "step {id}: \"do\" is {} characters long. Describe what the file or command must do or contain in one or two sentences; the worker model writes the content",
            intent.chars().count()
        ));
        return None;
    }
    if tool.as_deref() == Some("shell") && writes_a_file_through_the_shell(&intent) {
        r.issues.push(format!(
            "step {id}: files are written with fs_write (or changed with fs_edit), not with shell redirects or heredocs; use shell only to run things"
        ));
        return None;
    }
    let after = match first(v, &["after", "depends_on", "needs", "requires"]) {
        Some(Value::Array(a)) => a.iter().filter_map(text).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => vec![],
    };
    let expect = first(v, &["expect", "success", "expected"]).and_then(text).filter(|e| !is_nothing(e));
    let planner = first(v, &["planner", "strong", "own"]).and_then(|b| b.as_bool()).unwrap_or(false);
    Some(StepDoc { id, tool, intent, after, expect, planner, args })
}

/// The array of steps, wherever a model put it.
fn step_array(v: &Value) -> Option<&Vec<Value>> {
    if let Some(a) = v.as_array() {
        return Some(a);
    }
    ["steps", "plan", "tasks", "actions", "plan_steps"].iter().find_map(|k| v.get(*k)).and_then(|a| a.as_array())
}

/// Make ids unique and dependencies real. `known` are ids that exist outside this list (steps already in the plan).
fn tidy(steps: &mut [StepDoc], known: &HashSet<String>, r: &mut Reading) {
    let mut seen: HashSet<String> = HashSet::new();
    for s in steps.iter_mut() {
        if !seen.insert(s.id.clone()) {
            let mut n = 2;
            while seen.contains(&format!("{}_{n}", s.id)) {
                n += 1;
            }
            r.repairs.push(format!("id {} was used twice; the second is {}_{n}", s.id, s.id));
            s.id = format!("{}_{n}", s.id);
            seen.insert(s.id.clone());
        }
    }
    let all: HashSet<String> = seen.union(known).cloned().collect();
    for s in steps.iter_mut() {
        let before = s.after.len();
        let own = s.id.clone();
        s.after.retain(|d| all.contains(d) && *d != own);
        s.after.dedup();
        if s.after.len() != before {
            r.repairs.push(format!("step {}: dropped a dependency on a step that does not exist", s.id));
        }
    }
}

impl PlanDoc {
    /// Read a planner's reply into a typed plan, repairing what code can.
    pub fn from_value(v: &Value, tools: &[String]) -> Result<(PlanDoc, Reading), Reading> {
        let mut r = Reading::default();
        let Some(arr) = step_array(v) else {
            r.issues.push("the reply has no \"steps\" array".into());
            return Err(r);
        };
        let mut steps: Vec<StepDoc> = arr.iter().enumerate().filter_map(|(i, s)| step_from(s, i, tools, &mut r)).collect();
        if steps.is_empty() && r.issues.is_empty() {
            r.issues.push("the plan has no steps".into());
        }
        tidy(&mut steps, &HashSet::new(), &mut r);
        let report = v.get("report").and_then(text).filter(|s| !s.is_empty());
        if r.issues.is_empty() { Ok((PlanDoc { steps, report }, r)) } else { Err(r) }
    }
}

impl PatchDoc {
    /// Read a re-planning reply. `existing` are the ids already in the plan. A reply that is a whole plan (`steps` and none of
    /// `edit`/`add`/`drop`) is read as `add`: a model that ignores the patch format still gets its steps run.
    pub fn from_value(v: &Value, tools: &[String], existing: &HashSet<String>) -> Result<(PatchDoc, Reading), Reading> {
        let mut r = Reading::default();
        let list = |key: &str, r: &mut Reading| -> Vec<StepDoc> {
            v.get(key).and_then(|a| a.as_array()).map(|a| a.iter().enumerate().filter_map(|(i, s)| step_from(s, i, tools, r)).collect()).unwrap_or_default()
        };
        let (mut edit, mut add) = (list("edit", &mut r), list("add", &mut r));
        let drop: Vec<String> = v.get("drop").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(text).collect()).unwrap_or_default();
        let has_patch_keys = ["edit", "add", "drop"].iter().any(|k| v.get(*k).is_some());
        if !has_patch_keys && let Some(arr) = step_array(v) {
            add = arr.iter().enumerate().filter_map(|(i, s)| step_from(s, i, tools, &mut r)).collect();
            r.repairs.push("the reply was a plan, not a patch: its steps are added".into());
        }
        // an "edit" of a step that is not in the plan is an addition
        let (known_edits, new_edits): (Vec<StepDoc>, Vec<StepDoc>) = edit.drain(..).partition(|s| existing.contains(&s.id));
        edit = known_edits;
        if !new_edits.is_empty() {
            r.repairs.push(format!("edit of {} did not match a step: added instead", new_edits.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")));
            add.extend(new_edits);
        }
        let mut ids: Vec<StepDoc> = Vec::new();
        ids.extend(add.iter().cloned());
        let mut tmp = add.clone();
        tidy(&mut tmp, existing, &mut r);
        add = tmp;
        for e in &mut edit {
            let own = e.id.clone();
            let before = e.after.len();
            e.after.retain(|d| (existing.contains(d) || add.iter().any(|a| &a.id == d)) && *d != own);
            if e.after.len() != before {
                r.repairs.push(format!("step {}: dropped a dependency on a step that does not exist", e.id));
            }
        }
        let drop: Vec<String> = drop.into_iter().filter(|d| existing.contains(d)).collect();
        if edit.is_empty() && add.is_empty() && drop.is_empty() && r.issues.is_empty() {
            r.issues.push("the patch changes nothing (edit, add and drop are all empty)".into());
        }
        if r.issues.is_empty() { Ok((PatchDoc { edit, add, drop }, r)) } else { Err(r) }
    }
}

fn step_schema(tools: &[String]) -> Value {
    let mut tool_values: Vec<Value> = tools.iter().map(|t| json!(t)).collect();
    tool_values.sort_by_key(|a| a.as_str().map(String::from));
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "tool", "do", "after", "expect", "planner"],
        "properties": {
            "id": {"type": "string", "description": "s1, s2, … (a re-plan's new steps: the given prefix + 1, 2, …)"},
            "tool": {"anyOf": [{"type": "string", "enum": tool_values}, {"type": "null"}], "description": "the tool for this step, or null for a step that thinks or writes from earlier results"},
            "do": {"type": "string", "description": "the intent in one sentence, with every concrete value (path, name, pattern, text, command) in it"},
            "after": {"type": "array", "items": {"type": "string"}, "description": "ids of the steps whose output or effect this one needs"},
            "expect": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "what any correct result shows, e.g. \"site/index.html exists\"; null for exploratory steps"},
            "planner": {"type": "boolean", "description": "true only if this step needs the planner's own reasoning"}
        }
    })
}

/// The JSON Schema of a plan (strict structured output: every property required, none extra).
///
/// It is part of the provider's cached prompt prefix, so nothing in it varies from task to task: the step limit is in the request
/// (`MAX_STEPS`) and validated in code (no minItems/maxItems either: strict structured-output modes of several providers reject them).
pub fn plan_schema(tools: &[String], with_report: bool) -> Value {
    let mut props = json!({"steps": {"type": "array", "items": step_schema(tools), "description": "the steps, at most MAX_STEPS"}});
    let mut required = vec!["steps"];
    if with_report {
        props["report"] = json!({"anyOf": [{"type": "string"}, {"type": "null"}], "description": "the final reply to the user with {{sN}} where step sN's output belongs, or null"});
        required.push("report");
    }
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": props})
}

/// The JSON Schema of a re-planning patch.
pub fn patch_schema(tools: &[String]) -> Value {
    let list = |desc: &str| json!({"type": "array", "items": step_schema(tools), "description": desc});
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["edit", "add", "drop"],
        "properties": {
            "edit": list("steps of the current plan to change (same id; the step runs again)"),
            "add": list("new steps"),
            "drop": {"type": "array", "items": {"type": "string"}, "description": "ids of steps that are no longer needed"}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<String> {
        ["fs_read", "fs_write", "fs_mkdir", "shell", "web_search"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_well_formed_plan_reads_as_typed_steps() {
        let v = json!({"steps": [
            {"id": "s1", "tool": "fs_mkdir", "do": "create site", "after": [], "expect": "site exists", "planner": false},
            {"id": "s2", "tool": null, "do": "say what was made", "after": ["s1"], "expect": null, "planner": true}]});
        let (doc, r) = PlanDoc::from_value(&v, &tools()).unwrap();
        assert!(r.repairs.is_empty());
        assert_eq!(doc.steps[0].tool.as_deref(), Some("fs_mkdir"));
        assert_eq!((doc.steps[1].tool.clone(), doc.steps[1].after.clone(), doc.steps[1].planner), (None, vec!["s1".to_string()], true));
        assert_eq!(doc.to_value()["steps"][1]["planner"], true);
    }

    #[test]
    fn what_code_can_repair_is_repaired_and_said() {
        let v = json!({"plan": [
            {"id": 1, "tool": "FS-Write", "intent": "write a.txt", "depends_on": "2, 9"},
            {"id": 2, "tool": "fswrite", "description": "write b.txt"},
            {"id": 2, "tool": "none", "do": "think", "after": ["2"]}]});
        let (doc, r) = PlanDoc::from_value(&v, &tools()).unwrap();
        assert_eq!(doc.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["1", "2", "2_2"]);
        assert_eq!(doc.steps[0].tool.as_deref(), Some("fs_write"));
        assert_eq!(doc.steps[0].after, vec!["2"], "the dependency on 9 does not exist");
        assert_eq!(doc.steps[2].tool, None);
        assert!(r.repairs.iter().any(|m| m.contains("used twice")) && r.repairs.iter().any(|m| m.contains("does not exist")));
    }

    #[test]
    fn what_it_cannot_repair_is_a_typed_issue_the_planner_is_told() {
        let v = json!({"steps": [{"id": "s1", "tool": "teleport", "do": "go there"}, {"id": "s2", "tool": "shell"}]});
        let e = PlanDoc::from_value(&v, &tools()).unwrap_err();
        assert!(e.issues.iter().any(|m| m.contains("no tool \"teleport\"")), "{:?}", e.issues);
        assert!(e.issues.iter().any(|m| m.contains("step s2 has no \"do\"")), "{:?}", e.issues);
        assert!(PlanDoc::from_value(&json!({"hello": 1}), &tools()).unwrap_err().issues[0].contains("no \"steps\""));
        assert!(PlanDoc::from_value(&json!({"steps": []}), &tools()).is_err());
    }

    #[test]
    fn content_in_the_plan_and_files_written_by_the_shell_are_typed_issues() {
        let long = "x".repeat(900);
        let v = json!({"steps": [
            {"id": "s1", "tool": "fs_write", "do": long},
            {"id": "s2", "tool": "shell", "do": "write index.html with cat > index.html <<'EOF' ... EOF"},
            {"id": "s3", "tool": "shell", "do": "start `python3 -m http.server 8000` in the background with cwd site"}]});
        let e = PlanDoc::from_value(&v, &tools()).unwrap_err();
        assert!(e.issues.iter().any(|m| m.starts_with("step s1") && m.contains("900 characters")), "{:?}", e.issues);
        assert!(e.issues.iter().any(|m| m.starts_with("step s2") && m.contains("fs_write")), "{:?}", e.issues);
        assert!(!e.issues.iter().any(|m| m.starts_with("step s3")), "running a server is what shell is for: {:?}", e.issues);
    }

    #[test]
    fn a_patch_edits_adds_and_drops_and_a_whole_plan_in_its_place_is_still_understood() {
        let existing: HashSet<String> = ["s1", "s2", "s3"].iter().map(|s| s.to_string()).collect();
        let v = json!({"edit": [{"id": "s2", "tool": "shell", "do": "run it with cwd site", "after": ["s1", "zz"]}, {"id": "s9", "tool": null, "do": "new one"}],
                       "add": [{"id": "r1_1", "tool": "fs_read", "do": "read a.txt", "after": ["s1"]}], "drop": ["s3", "nope"]});
        let (p, r) = PatchDoc::from_value(&v, &tools(), &existing).unwrap();
        assert_eq!((p.edit.len(), p.add.len(), p.drop.clone()), (1, 2, vec!["s3".to_string()]));
        assert_eq!(p.edit[0].after, vec!["s1"]);
        assert!(r.repairs.iter().any(|m| m.contains("did not match a step")));
        let (p, r) = PatchDoc::from_value(&json!({"steps": [{"id": "r1_1", "tool": "fs_read", "do": "read"}]}), &tools(), &existing).unwrap();
        assert_eq!((p.edit.len(), p.add.len()), (0, 1));
        assert!(r.repairs.iter().any(|m| m.contains("a plan, not a patch")));
        assert!(PatchDoc::from_value(&json!({"edit": [], "add": [], "drop": []}), &tools(), &existing).is_err());
    }

    #[test]
    fn the_schemas_are_strict_and_name_the_tools() {
        let s = plan_schema(&tools(), false);
        assert_eq!(s["additionalProperties"], false);
        let item = &s["properties"]["steps"]["items"];
        assert_eq!(item["required"].as_array().unwrap().len(), 6);
        let e = item["properties"]["tool"]["anyOf"][0]["enum"].as_array().unwrap();
        assert!(e.contains(&json!("fs_write")) && !e.contains(&json!("teleport")));
        assert!(plan_schema(&tools(), true)["required"].as_array().unwrap().contains(&json!("report")));
        assert_eq!(patch_schema(&tools())["required"], json!(["edit", "add", "drop"]));
    }

    #[test]
    fn a_tool_is_found_through_spelling_slips_only_when_it_is_unambiguous() {
        let t = tools();
        assert_eq!(resolve_tool("functions.fs_read", &t).as_deref(), Some("fs_read"));
        assert_eq!(resolve_tool("fs_wrte", &t).as_deref(), Some("fs_write"));
        assert_eq!(resolve_tool("web search", &t).as_deref(), Some("web_search"));
        assert_eq!(resolve_tool("fs_", &t), None);
        assert_eq!(resolve_tool("teleport", &t), None);
    }
}
