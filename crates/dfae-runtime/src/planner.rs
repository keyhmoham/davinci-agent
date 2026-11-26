//! Plan construction, parsing and validation. A plan is a *proposal*: it is
//! schema-validated, restricted to the tools selected for the task, and
//! budget-checked before anything executes.

use dfae_core::*;
use dfae_tools::{DirectCall, ToolRegistry};
use regex::Regex;
use serde_json::{Value, json};
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

static ANY_BRACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{([^{}]*)\}\}").unwrap());

/// `{{…}}` in string values that is not a reference the system can resolve (`{{s1}}`, `{{prev_code}}`): a small model's
/// invented placeholder such as `{{ENT.location}}`, which would otherwise be written literally into a file.
pub fn malformed_placeholders(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                for c in ANY_BRACES.captures_iter(s) {
                    let inner = c[1].trim();
                    let ident = !inner.is_empty()
                        && inner.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                        && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                    if !ident {
                        out.push(c[0].to_string());
                    }
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
    Plan { goal: goal.into(), steps, source: PlanSource::Direct, revision: 0, estimate: None, report: None }
}

/// Tools a planner needs next to `tool` to use it properly.
pub fn companions(tool: &str) -> &'static [&'static str] {
    match tool {
        "fs_edit" => &["fs_read", "fs_search", "fs_glob"],
        "fs_write" => &["fs_read"],
        "fs_delete" | "fs_move" | "fs_copy" => &["fs_list", "fs_glob"],
        "shell" => &["shell_output", "shell_kill"],
        "shell_output" | "shell_kill" => &["shell"],
        "fs_search" => &["fs_read"],
        "fs_glob" => &["fs_read"],
        _ => &[],
    }
}

/// A conversational reply: one reasoning step, no planning call. `gap` marks a request that
/// needs a capability none of the tools has, so the reply says so honestly.
pub fn chat_plan(goal: &str, gap: bool) -> Plan {
    let desc = if gap { "reply; no tool fits this request" } else { "reply" };
    Plan { goal: goal.into(), steps: vec![Step::reasoning("s1", desc)], source: PlanSource::Chat, revision: 0, estimate: None, report: None }
}

/// Parse a model-produced plan. `prefix` is prepended to every new step id
/// (replans); dependencies on ids outside the new set are kept verbatim.
pub fn parse_plan(v: &Value, goal: &str, source: PlanSource, prefix: &str, registry: &ToolRegistry) -> Result<Vec<Step>, String> {
    let arr = v.get("steps").and_then(|s| s.as_array()).ok_or("missing 'steps' array")?;
    // the replan prompt asks for ids that start with the prefix, so a model that obeys must not get it twice
    let strip = |id: &str| -> String { if prefix.is_empty() { id.to_string() } else { id.strip_prefix(prefix).unwrap_or(id).to_string() } };
    let raw_ids: Vec<String> =
        arr.iter().enumerate().map(|(i, s)| s.get("id").and_then(|x| x.as_str()).map(&strip).unwrap_or_else(|| format!("s{}", i + 1))).collect();
    // A re-plan's steps are numbered in order (`r1_1`, `r1_2`…) whatever ids the model wrote, so a model that repeats an
    // id or mixes `s2` with `r1_s2` can never produce duplicates. References resolve to the first step of that name.
    let new_ids: Vec<String> = raw_ids.iter().enumerate().map(|(i, raw)| if prefix.is_empty() { raw.clone() } else { format!("{prefix}{}", i + 1) }).collect();
    let mut by_raw: HashMap<&str, &String> = HashMap::new();
    for (raw, new) in raw_ids.iter().zip(&new_ids) {
        by_raw.entry(raw.as_str()).or_insert(new);
    }
    let rename = |id: &str| -> String {
        let bare = strip(id);
        by_raw.get(bare.as_str()).map(|n| (*n).clone()).unwrap_or_else(|| id.to_string())
    };
    let mut steps = Vec::new();
    for (i, s) in arr.iter().enumerate() {
        let id = new_ids[i].clone();
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
        for r in template_refs(&step.args).into_iter().filter(|r| !crate::session::ARTIFACT_NAMES.contains(&r.as_str())) {
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

/// Parse a *brief* plan (intents, no arguments): every step is marked `brief`, so the local system turns it into
/// an action just before it runs. Arguments the planner did give are kept as pre-bound hints.
pub fn parse_brief_plan(v: &Value, goal: &str, source: PlanSource, prefix: &str, registry: &ToolRegistry) -> Result<Vec<Step>, String> {
    let mut steps = parse_plan(v, goal, source, prefix, registry)?;
    let arr = v.get("steps").and_then(|s| s.as_array()).ok_or("missing 'steps' array")?;
    for (s, raw) in steps.iter_mut().zip(arr) {
        s.brief = true;
        s.expect = raw.get("expect").or_else(|| raw.get("success")).and_then(|e| e.as_str()).map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        s.planner = raw.get("planner").and_then(|b| b.as_bool()).unwrap_or(false);
    }
    Ok(steps)
}

/// The planner's pre-written final reply, if it is usable: every `{{id}}` in it names a step of the plan.
pub fn plan_report(v: &Value, steps: &[Step]) -> Option<String> {
    let r = v.get("report").and_then(|r| r.as_str()).map(str::trim).filter(|r| !r.is_empty())?;
    let ids: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    // ids in the report follow the same renumbering as the steps only for a first plan; a re-plan sets its own
    TEMPLATE.captures_iter(r).all(|c| ids.contains(&c[1])).then(|| r.to_string())
}

/// Apply a re-planning patch to the steps of a plan. Finished steps are never touched; an edited step runs again from a clean
/// state (its arguments are chosen anew by the worker); a failed step that is neither edited nor dropped is an issue, because the
/// plan could not finish. `Err` lists what to tell the planner.
pub fn apply_patch(current: &[Step], patch: &crate::plandoc::PatchDoc, prefix: &str, registry: &ToolRegistry) -> Result<Vec<Step>, Vec<String>> {
    let mut issues: Vec<String> = Vec::new();
    let mut steps: Vec<Step> = current.to_vec();
    let done = |s: &Step| s.status == StepStatus::Succeeded;
    for id in &patch.drop {
        match steps.iter().position(|s| &s.id == id) {
            Some(i) if done(&steps[i]) => issues.push(format!("{id} is finished and cannot be dropped")),
            Some(i) => {
                steps.remove(i);
            }
            None => {}
        }
    }
    // new steps get ids `prefix1`, `prefix2`… whatever the planner wrote; edits may refer to them by what it wrote
    let renumber: HashMap<String, String> = patch.add.iter().enumerate().map(|(i, s)| (s.id.clone(), format!("{prefix}{}", i + 1))).collect();
    let fix_dep = |d: &String| renumber.get(d).cloned().unwrap_or_else(|| d.clone());
    for e in &patch.edit {
        let Some(i) = steps.iter().position(|s| s.id == e.id) else { continue };
        if done(&steps[i]) {
            issues.push(format!("{} is finished: add a new step instead of editing it", e.id));
            continue;
        }
        let mut ns = match &e.tool {
            Some(t) => Step::tool(&e.id, t, &e.intent, json!({})),
            None => Step::reasoning(&e.id, &e.intent),
        };
        if let Some(m) = e.tool.as_deref().and_then(|t| registry.meta(t)) {
            ns.approval_required = m.requires_approval;
        }
        ns.brief = true;
        ns.expect = e.expect.clone();
        ns.planner = e.planner;
        ns.depends_on = e.after.iter().map(fix_dep).filter(|d| *d != e.id).collect();
        steps[i] = ns;
    }
    if !patch.add.is_empty() {
        let doc = json!({"steps": patch.add.iter().map(|s| s.to_value()).collect::<Vec<_>>()});
        match parse_brief_plan(&doc, "", PlanSource::Replan, prefix, registry) {
            Ok(new) => steps.extend(new),
            Err(e) => issues.push(e),
        }
    }
    // nothing may wait for a step that is gone
    let gone: HashSet<&String> = patch.drop.iter().collect();
    for s in &mut steps {
        s.depends_on.retain(|d| !gone.contains(d));
        // what was waiting on the old version of an edited step, or was cancelled with the run, runs again
        if matches!(s.status, StepStatus::Cancelled) {
            s.status = StepStatus::Pending;
        }
    }
    for s in &steps {
        if s.status == StepStatus::Failed {
            issues.push(format!("{} failed ({}) and is neither edited nor dropped", s.id, tokens::truncate_tokens(s.error.as_deref().unwrap_or("error"), 20)));
        }
    }
    if issues.is_empty() { Ok(steps) } else { Err(issues) }
}

static EXPECT_EXISTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^(?:the |a )?(?:(?:file|directory|dir|folder) )?[`"']?([\w./~\-]*[\w/])[`"']?(?: (?:file|directory|dir|folder))? (?:now |newly )?(?:exists?|is created|was created|is written|is present)$"#).unwrap()
});
static EXPECT_CONTAINS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^(?:the |a )?(?:(?:file|directory) )?[`"']?([\w./~\-]*[\w/])[`"']? (?:file )?(?:now |then )?contains? (?:the (?:text|line|string|word) )?(?:`([^`]+)`|"([^"]+)"|'([^']+)'|(.+))$"#).unwrap()
});
static EXPECT_OUTPUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^(?:the )?(?:output|result|response|listing) (?:also |now )?(?:contains?|shows?|lists?|includes?|mentions?|has) (?:the (?:text|line|word|entries?|string) )?(?:`([^`]+)`|"([^"]+)"|'([^']+)'|(.+))$"#).unwrap()
});
static EXPECT_OK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:the )?(?:command|step|tool|script|build|tests?|process|server|job)?\s*(?:exits?|exited|returns?|finish(?:es|ed)?)(?: with)?(?: (?:exit )?(?:code|status))? 0$|^(?:the )?(?:command |step |tool |script |build |tests? |process |server |job )?(?:succeeds|succeeded|runs successfully|ran successfully|starts|started|starts successfully|is running|runs)$").unwrap()
});

/// Turn what a planner expects of a step into checks code can run, when it is a condition code understands
/// ("site/index.html exists", "config.toml now contains port = 9090", "the command exits 0", "the output lists X").
/// The bool says whether *every* clause of the expectation was understood: if so the checks are the whole judgement and
/// no model has to be asked; otherwise the decision model still judges what code could not.
pub fn expect_checks(expect: &str) -> (Vec<Check>, bool) {
    let mut checks = Vec::new();
    let mut all = true;
    let text = expect.trim().trim_end_matches('.');
    // clauses: split on ";" and " and " but not inside quotes (a quoted value may contain either)
    let mut clauses: Vec<String> = Vec::new();
    let (mut cur, mut quote) = (String::new(), None::<char>);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            None if matches!(c, '"' | '`') => quote = Some(c),
            _ => {}
        }
        let rest: String = chars[i..].iter().take(5).collect();
        if quote.is_none() && (c == ';' || rest.eq_ignore_ascii_case(" and ")) {
            clauses.push(std::mem::take(&mut cur));
            i += if c == ';' { 1 } else { 5 };
            continue;
        }
        cur.push(c);
        i += 1;
    }
    clauses.push(cur);
    for clause in clauses.iter().map(|c| c.trim()).filter(|c| !c.is_empty()) {
        let pick =
            |c: &regex::Captures, from: usize| (from..c.len()).find_map(|i| c.get(i)).map(|m| m.as_str().trim().trim_matches(['"', '`', '\'']).to_string());
        if let Some(c) = EXPECT_EXISTS.captures(clause) {
            checks.push(Check::FileExists { path: c[1].to_string() });
        } else if let Some(c) = EXPECT_CONTAINS.captures(clause)
            && let Some(v) = pick(&c, 2).filter(|v| !v.is_empty())
            && (c[1].contains('.') || c[1].contains('/'))
        {
            checks.push(Check::FileContains { path: c[1].to_string(), value: v });
        } else if let Some(c) = EXPECT_OUTPUT.captures(clause)
            && let Some(v) = pick(&c, 1).filter(|v| !v.is_empty())
        {
            checks.push(Check::Contains { value: v });
        } else if EXPECT_OK.is_match(clause) {
            checks.push(Check::ExitOk);
        } else if clause.eq_ignore_ascii_case("the output is not empty") || clause.eq_ignore_ascii_case("non-empty output") {
            checks.push(Check::NonEmpty);
        } else {
            all = false;
        }
    }
    let understood = all && !checks.is_empty();
    (checks, understood)
}

/// Does every `{{id}}` in a report name a step of the plan?
pub fn report_refs_exist(report: &str, steps: &[Step]) -> bool {
    let ids: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    TEMPLATE.captures_iter(report).all(|c| ids.contains(&c[1]))
}

static PATHLIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\w.\-]+(?:/[\w.\-]+)+/?|[\w\-]+\.[A-Za-z0-9]{1,6}\b").unwrap());

fn norm_path(p: &str) -> String {
    p.trim().trim_start_matches("./").trim_end_matches('/').to_string()
}

fn path_tokens(text: &str) -> Vec<String> {
    PATHLIKE.find_iter(text).map(|m| norm_path(m.as_str())).filter(|p| !p.is_empty() && !p.starts_with("http")).collect()
}

fn string_values(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| string_values(x, out)),
        Value::Object(m) => m.values().for_each(|x| string_values(x, out)),
        _ => {}
    }
}

fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// What a step makes (paths it writes) and what it touches (paths it names anywhere: arguments, working directory,
/// command line, or — for a step not yet implemented — its sentence).
fn paths_of(step: &Step) -> (Vec<String>, Vec<String>) {
    let mut texts = vec![step.description.clone()];
    string_values(&step.args, &mut texts);
    let mut uses: Vec<String> = texts.iter().flat_map(|t| path_tokens(t)).collect();
    // arguments that are paths by name count even when they look like a bare word ("public")
    for k in ["path", "cwd", "dir", "directory", "from", "to"] {
        if let Some(p) = step.args.get(k).and_then(|v| v.as_str()).map(norm_path).filter(|p| !p.is_empty() && !p.contains("{{")) {
            uses.push(p);
        }
    }
    let arg = |k: &str| step.args.get(k).and_then(|v| v.as_str()).map(norm_path).filter(|p| !p.is_empty());
    let writes: Vec<String> = match step.tool.as_deref() {
        Some("fs_write" | "fs_edit" | "fs_mkdir" | "fs_delete") => arg("path").into_iter().collect(),
        Some("fs_move") => arg("from").into_iter().chain(arg("to")).collect(),
        Some("fs_copy") => arg("to").into_iter().collect(),
        _ => vec![],
    };
    // a step that has not been given arguments yet makes whatever its sentence names
    let writes =
        if writes.is_empty() && step.brief && matches!(step.tool.as_deref(), Some("fs_write" | "fs_edit" | "fs_mkdir" | "fs_delete" | "fs_move" | "fs_copy")) {
            path_tokens(&step.description)
        } else {
            writes
        };
    (writes, uses)
}

/// A planner (above all a small one) forgets to say that serving a directory must wait until the directory exists.
/// Code knows which steps change things and which paths each step names, so it adds the missing "after"s:
/// * steps that change things run in plan order, one after another (a shell command can change anything);
/// * a step that names a path an earlier step writes waits for that step.
///
/// Dependencies only point backwards, and never close a cycle. Returns what was added, for the log.
pub fn infer_dependencies(steps: &mut [Step], registry: &ToolRegistry) -> Vec<String> {
    let effect: Vec<bool> =
        steps.iter().map(|s| s.kind == StepKind::Tool && s.tool.as_deref().and_then(|t| registry.meta(t)).map(|m| m.side_effects).unwrap_or(false)).collect();
    let paths: Vec<(Vec<String>, Vec<String>)> = steps.iter().map(paths_of).collect();
    let reaches = |steps: &[Step], from: usize, target: &str| -> bool {
        // does `from` (transitively) depend on the step named `target`?
        let mut stack = vec![steps[from].id.clone()];
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if id == target {
                return true;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(s) = steps.iter().find(|s| s.id == id) {
                stack.extend(s.depends_on.iter().cloned());
            }
        }
        false
    };
    let mut notes = Vec::new();
    let mut prev_effect: Option<usize> = None;
    for i in 0..steps.len() {
        let mut wanted: Vec<(usize, &str)> = Vec::new();
        if effect[i] {
            if let Some(p) = prev_effect {
                wanted.push((p, "steps that change things run in order"));
            }
            prev_effect = Some(i);
        }
        for j in 0..i {
            if effect[j] && paths[j].0.iter().any(|w| paths[i].1.iter().any(|u| overlaps(w, u))) {
                wanted.push((j, "it uses what that step writes"));
            }
        }
        for (j, why) in wanted {
            let dep = steps[j].id.clone();
            if steps[i].depends_on.contains(&dep) || reaches(steps, j, &steps[i].id) || reaches(steps, i, &dep) {
                continue;
            }
            notes.push(format!("{} now waits for {dep} ({why})", steps[i].id));
            steps[i].depends_on.push(dep);
        }
    }
    notes
}

/// Drop dependencies on steps that do not exist (a model's slip, e.g. `after: ["r1_2"]` with no such step).
/// Data still flows only through `{{id}}` templates, which are checked when the step runs.
pub fn prune_unknown_deps(steps: &mut [Step]) -> Vec<String> {
    let ids: HashSet<String> = steps.iter().map(|s| s.id.clone()).collect();
    let mut dropped = Vec::new();
    for s in steps.iter_mut() {
        s.depends_on.retain(|d| {
            let known = ids.contains(d);
            if !known {
                dropped.push(format!("{} -> {d}", s.id));
            }
            known
        });
    }
    dropped
}

/// A request that changes something (create, write, delete…) must be planned with a tool that does.
/// `allowed` is what the planner was offered; the problem names the tools it should have used.
pub fn missing_effect(steps: &[Step], registry: &ToolRegistry, allowed: &[String]) -> Option<String> {
    let effect: Vec<&String> = allowed.iter().filter(|n| registry.meta(n).map(|m| m.side_effects).unwrap_or(false)).collect();
    if effect.is_empty() || steps.iter().any(|s| s.kind == StepKind::Agent || s.tool.as_ref().map(|t| effect.contains(&t)).unwrap_or(false)) {
        return None;
    }
    Some(format!(
        "the request asks to change something but no step does it; add a step using {}",
        effect.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" or ")
    ))
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
    // `budget.max_model_calls` is what is *left*: a plan that cannot be afforded is
    // rejected now, with the numbers, so the planner can shrink it — not discovered
    // half-way through execution after the tools have already run.
    let needed = crate::economy::plan_model_calls(steps);
    if needed > budget.max_model_calls {
        errs.push(format!("the plan needs about {needed} model calls but only {} remain; use fewer reasoning steps", budget.max_model_calls));
    }
    let agent_steps = steps.iter().filter(|s| s.kind == StepKind::Agent).count() as u32;
    if agent_steps > budget.max_children {
        errs.push(format!("plan delegates to {agent_steps} agents; the limit is {}", budget.max_children));
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
                        errs.push(format!("step {}: tool '{name}' is not available for this task; use only: {}", s.id, allowed.join(", ")));
                    }
                    let bad = malformed_placeholders(&s.args);
                    if !bad.is_empty() {
                        errs.push(format!(
                            "step {} ({name}): {} is not a reference the system can fill in; write the real text, or use {{{{<step id>}}}} for an earlier step's result",
                            s.id,
                            bad.join(", ")
                        ));
                    }
                    // arguments that contain templates are validated at run time; a brief step has none yet
                    let has_template = !template_refs(&s.args).is_empty();
                    if !has_template
                        && !s.brief
                        && let Err(e) = m.validate_args(&s.args)
                    {
                        errs.push(format!("step {} ({name}): {e}", s.id));
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
    use dfae_tools::{PolicyConfig, PolicyEngine, register_native};
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
        let steps = parse_plan(&v, "g", PlanSource::Worker, "", &r).unwrap();
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
            let steps = parse_plan(&v, "g", PlanSource::Worker, "", &r).unwrap();
            validate_plan(&steps, &r, &["fs_read".to_string()], &Budget::default()).unwrap_err()
        };
        assert!(bad(json!({"steps": [{"id": "s1", "tool": "rm_rf", "args": {}}]})).iter().any(|e| e.contains("unknown tool")));
        assert!(bad(json!({"steps": [{"id": "s1", "tool": "calc", "args": {"expression": "1"}}]})).iter().any(|e| e.contains("not available")));
        assert!(bad(json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {}}]})).iter().any(|e| e.contains("missing required")));
        assert!(
            bad(json!({"steps": [{"id": "s1", "tool": null, "after": ["s2"]}, {"id": "s2", "tool": null, "after": ["s1"]}]}))
                .iter()
                .any(|e| e.contains("cycle"))
        );
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
        assert_eq!(steps[0].id, "r1_1");
        assert_eq!(steps[1].depends_on, vec!["r1_1", "s9"]);
    }

    #[test]
    fn a_step_that_uses_what_another_writes_waits_for_it() {
        // the container's shell: a full bash, which can change anything
        let policy = Arc::new(PolicyEngine::new(PolicyConfig::from_toml("[shell]\nmode = \"sandbox\"\n").unwrap(), std::env::temp_dir()));
        let mut r = ToolRegistry::new();
        register_native(&mut r, policy, Arc::new(InMemoryStore::new()));
        // the screenshot case: write the page, then serve its directory — the model forgot the "after"
        let v = json!({"steps": [
            {"id": "s1", "tool": "fs_write", "args": {"path": "public/earth.html", "content": "<html></html>"}},
            {"id": "s2", "tool": "shell", "args": {"command": "python3 -m http.server 8000", "cwd": "public"}},
            {"id": "s3", "tool": "fs_read", "args": {"path": "other.txt"}},
            {"id": "s4", "tool": "fs_read", "args": {"path": "./public/earth.html"}}
        ]});
        let mut steps = parse_plan(&v, "g", PlanSource::Worker, "", &r).unwrap();
        let notes = infer_dependencies(&mut steps, &r);
        let deps = |id: &str| steps.iter().find(|s| s.id == id).unwrap().depends_on.clone();
        assert_eq!(deps("s2"), vec!["s1"], "{notes:?}");
        assert!(deps("s3").is_empty(), "an unrelated read stays parallel");
        assert_eq!(deps("s4"), vec!["s1"], "a read of the written file waits for the write");
        assert!(notes.iter().any(|n| n.starts_with("s2 now waits for s1")));
        // idempotent, and never a cycle
        assert!(infer_dependencies(&mut steps, &r).is_empty());
        validate_plan(&steps, &r, &allowed(&r), &Budget::default()).unwrap();
        // changes happen one after another in plan order
        let mut two = parse_plan(&json!({"steps": [{"id": "a", "tool": "fs_write", "args": {"path": "x.txt", "content": "1"}}, {"id": "b", "tool": "fs_write", "args": {"path": "y.txt", "content": "2"}}]}), "g", PlanSource::Worker, "", &r).unwrap();
        infer_dependencies(&mut two, &r);
        assert_eq!(two[1].depends_on, vec!["a"]);
        // an intent that has no arguments yet is judged by its sentence
        let mut brief = parse_brief_plan(&json!({"steps": [{"id": "s1", "do": "write the page to public/index.html", "tool": "fs_write"}, {"id": "s2", "do": "serve the public directory", "tool": "shell"}]}), "g", PlanSource::Planner, "", &r).unwrap();
        infer_dependencies(&mut brief, &r);
        assert_eq!(brief[1].depends_on, vec!["s1"]);
        // a model's own dependency that would make a cycle is left alone
        let mut cyc = parse_plan(&json!({"steps": [{"id": "a", "tool": "fs_write", "args": {"path": "x.txt", "content": "1"}, "after": ["b"]}, {"id": "b", "tool": "fs_write", "args": {"path": "x.txt", "content": "2"}}]}), "g", PlanSource::Worker, "", &r).unwrap();
        infer_dependencies(&mut cyc, &r);
        assert!(find_cycle(&cyc).is_none());
    }

    #[test]
    fn invented_placeholders_are_rejected_not_written_into_files() {
        let r = registry();
        let steps = parse_plan(
            &json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "a.html", "content": "<p>{{ENT.location}} and {{ENT.person}}</p>"}}]}),
            "g",
            PlanSource::Worker,
            "",
            &r,
        )
        .unwrap();
        let errs = validate_plan(&steps, &r, &allowed(&r), &Budget::default()).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("{{ENT.location}}") && e.contains("real text")), "{errs:?}");
        // real references are fine
        assert!(malformed_placeholders(&json!({"a": "{{s1}} and {{ prev_code }}", "b": ["no braces"]})).is_empty());
        assert_eq!(malformed_placeholders(&json!({"a": "{{}}"})), vec!["{{}}"]);
    }

    #[test]
    fn a_replan_with_repeated_or_mixed_ids_never_has_duplicates() {
        let r = registry();
        // the screenshot case: the model wrote both `r1_s2` and `s2`, twice over
        let v = json!({"steps": [
            {"id": "r1_s1", "tool": "fs_write", "args": {"path": "a.html", "content": "x"}},
            {"id": "r1_s2", "tool": "fs_list", "args": {"path": "."}, "after": ["r1_s1"]},
            {"id": "s2", "tool": null, "do": "report", "after": ["r1_s2"]},
            {"id": "r1_s2", "tool": null, "do": "again", "after": ["s2"]}
        ]});
        let steps = parse_plan(&v, "g", PlanSource::Replan, "r1_", &r).unwrap();
        let ids: Vec<&str> = steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["r1_1", "r1_2", "r1_3", "r1_4"]);
        validate_plan(&steps, &r, &allowed(&r), &Budget::default()).unwrap();
        assert_eq!(steps[2].depends_on, vec!["r1_2"]);
    }

    #[test]
    fn replan_ids_that_already_carry_the_prefix_are_not_prefixed_twice() {
        let r = registry();
        let v = json!({"steps": [
            {"id": "r1_1", "tool": "fs_list", "args": {"path": "."}},
            {"id": "r1_2", "tool": null, "after": ["r1_1", "r1_9"], "args": {}}
        ]});
        let mut steps = parse_plan(&v, "g", PlanSource::Replan, "r1_", &r).unwrap();
        assert_eq!((steps[0].id.as_str(), steps[1].id.as_str()), ("r1_1", "r1_2"));
        assert_eq!(steps[1].depends_on, vec!["r1_1", "r1_9"]);
        assert_eq!(prune_unknown_deps(&mut steps), vec!["r1_2 -> r1_9"]);
        assert_eq!(steps[1].depends_on, vec!["r1_1"]);
        validate_plan(&steps, &r, &allowed(&r), &Budget::default()).unwrap();
    }

    #[test]
    fn a_changing_request_needs_a_changing_step() {
        let r = registry();
        let allowed = vec!["fs_write".to_string(), "fs_list".to_string()];
        let list = parse_plan(&json!({"steps": [{"id": "s1", "tool": "fs_list", "args": {"path": "."}}]}), "g", PlanSource::Worker, "", &r).unwrap();
        assert!(missing_effect(&list, &r, &allowed).unwrap().contains("fs_write"));
        let write =
            parse_plan(&json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "a", "content": "b"}}]}), "g", PlanSource::Worker, "", &r).unwrap();
        assert!(missing_effect(&write, &r, &allowed).is_none());
        assert!(missing_effect(&list, &r, &["fs_list".to_string()]).is_none(), "nothing offered could change anything");
    }

    #[test]
    fn substitutes_templates() {
        let v = json!({"a": "x {{s1}} y", "b": ["{{s2}}"]});
        let out = substitute(&v, &|id| Some(format!("<{id}>")));
        assert_eq!(out, json!({"a": "x <s1> y", "b": ["<s2>"]}));
    }
}

#[cfg(test)]
mod expect_tests {
    use super::*;

    #[test]
    fn conditions_code_understands_become_checks() {
        let (c, all) = expect_checks("site/index.html exists");
        assert_eq!((c, all), (vec![Check::FileExists { path: "site/index.html".into() }], true));
        let (c, all) = expect_checks("the file config.toml now contains \"port = 9090\"");
        assert_eq!((c, all), (vec![Check::FileContains { path: "config.toml".into(), value: "port = 9090".into() }], true));
        let (c, all) = expect_checks("the command exits 0");
        assert_eq!((c, all), (vec![Check::ExitOk], true));
        let (c, all) = expect_checks("the output lists `index.html`");
        assert_eq!((c, all), (vec![Check::Contains { value: "index.html".into() }], true));
        let (c, all) = expect_checks("backup/config.toml exists and the command exits 0");
        assert_eq!(c.len(), 2);
        assert!(all);
    }

    #[test]
    fn what_code_cannot_read_is_left_to_the_decision_model() {
        let (c, all) = expect_checks("shows the server port");
        assert!(c.is_empty() && !all);
        let (c, all) = expect_checks("site/index.html exists and the page looks professional");
        assert_eq!(c.len(), 1);
        assert!(!all, "one clause was not understood, so the model still judges");
    }

    #[test]
    fn a_quoted_value_may_contain_and() {
        let (c, all) = expect_checks("notes.txt contains \"salt and pepper\"");
        assert_eq!((c, all), (vec![Check::FileContains { path: "notes.txt".into(), value: "salt and pepper".into() }], true));
    }
}
