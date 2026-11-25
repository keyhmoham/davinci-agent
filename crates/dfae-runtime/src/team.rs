//! Team orchestration: decomposing a request into sub-goals, assigning roles,
//! and the state a team shares.
//!
//! Everything here is deterministic. The *decision* to actually form a team is
//! made by the decision layer (`assess_decomposition`); this module supplies the
//! candidate split and the facts that decision is based on.
//!
//! Dependencies are inferred conservatively: a sub-goal that cues a prior one
//! ("then", "it", "if necessary", "based on") runs after it and receives its
//! summary; anything else runs in parallel. A false dependency only costs
//! parallelism; a false independence would cost correctness.

use dfae_core::*;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAX_SUBGOALS: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct SubGoal {
    pub id: String,
    pub text: String,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decomposition {
    pub subgoals: Vec<SubGoal>,
    /// A final "summarise / compare / combine" clause: a join step over all sub-goals.
    pub synthesis: Option<String>,
}

/// Clause-initial imperative verbs. Question words are deliberately absent:
/// "…and which should I use?" belongs to the same question, not a new task.
const VERBS: &[&str] = &[
    "find",
    "check",
    "read",
    "list",
    "fetch",
    "search",
    "write",
    "create",
    "patch",
    "deploy",
    "restart",
    "update",
    "upgrade",
    "install",
    "remove",
    "delete",
    "compute",
    "calculate",
    "summarize",
    "summarise",
    "compare",
    "explain",
    "analyze",
    "analyse",
    "verify",
    "test",
    "run",
    "show",
    "get",
    "look",
    "review",
    "report",
    "send",
    "open",
    "print",
    "display",
    "describe",
    "identify",
    "count",
    "measure",
    "scan",
    "download",
    "build",
    "fix",
    "configure",
    "monitor",
    "tell",
    "give",
    "save",
    "store",
    "remember",
    "recall",
    "combine",
    "merge",
    "rank",
    "evaluate",
    "validate",
    "confirm",
    "ensure",
    "inspect",
    "diagnose",
    "investigate",
    "research",
    "gather",
    "collect",
    "look",
];

static SEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:,\s*)?(?:\band\s+then\b|\bthen\b|\bafter\s+that\b|\bafterwards\b|\bfinally\b|\bnext\b|\band\b|,)\s*").unwrap());
static HARD_SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:;|\n|[.!?]\s+)").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:\d+[.)]|[-*•])\s+").unwrap());
static SEQUENCE_CUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(then|after that|afterwards|finally|next)\b").unwrap());
static ANAPHORA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(it|its|them|their|those|these|if necessary|if needed|if so|if any|if required|whether it|whether they|based on|using (?:that|those|the (?:result|output|findings))|the (?:result|results|output|findings)|the above|accordingly|from that|from there|whether|affected|impacted|applicable|relevant)\b").unwrap()
});
static SYNTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:then\s+|finally\s+|also\s+)?(?:summari[sz]e|compare|combine|merge|aggregate|consolidate|rank|conclude|report\s+on|give\s+me\s+an?\s+(?:overview|summary)|write\s+an?\s+(?:summary|report))\b").unwrap()
});
static COUPLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(compare|versus|vs\.?|difference|differences|between)\b").unwrap());
/// Clauses that fetch or observe something…
const ACQUIRE: &[&str] = &[
    "read", "list", "fetch", "search", "find", "get", "show", "run", "check", "scan", "open", "download", "look", "print", "display", "measure", "gather",
    "collect", "inspect", "monitor", "research",
];
/// …and clauses that think about it. An analysis clause right after an acquisition
/// clause almost always refers to what was acquired ("read X and tell me which …").
const ANALYZE: &[&str] = &[
    "explain",
    "tell",
    "describe",
    "count",
    "identify",
    "determine",
    "analyze",
    "analyse",
    "evaluate",
    "review",
    "interpret",
    "decide",
    "diagnose",
    "investigate",
    "extract",
    "give",
];
const MUTATE: &[&str] = &[
    "patch",
    "deploy",
    "restart",
    "update",
    "upgrade",
    "install",
    "remove",
    "delete",
    "write",
    "create",
    "save",
    "store",
    "fix",
    "configure",
    "build",
    "send",
    "remember",
    "mkdir",
];

fn first_word(s: &str) -> String {
    let mut s = s.trim_start();
    loop {
        let lower = s.to_lowercase();
        match ["then ", "also ", "please ", "finally ", "next ", "and "].iter().find(|a| lower.starts_with(*a)) {
            Some(a) => s = &s[a.len()..],
            None => break,
        }
    }
    s.chars().take_while(|c| c.is_alphabetic()).collect::<String>().to_lowercase()
}

static LEAD_ADVERB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:and|then|also|finally|next|please|and then)\s+").unwrap());
static ENUM_REST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[\s,&]*(?:and\s+)?[\s,&]*$").unwrap());

fn starts_with_verb(s: &str) -> bool {
    let mut s = s.trim_start();
    // skip adverbs between the separator and the verb
    loop {
        let lower = s.to_lowercase();
        let skip = ["then ", "also ", "please ", "finally ", "next "].iter().find(|a| lower.starts_with(*a)).map(|a| a.len());
        match skip {
            Some(n) => s = &s[n..],
            None => break,
        }
    }
    let first: String = s.chars().take_while(|c| c.is_alphabetic()).collect::<String>().to_lowercase();
    VERBS.contains(&first.as_str())
}

/// Split `goal` into clauses; each carries the separator that preceded it.
fn segments(goal: &str, binds: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
    // 1. hard boundaries (sentences, lists). A sentence ends at ". " only when the
    //    next character starts a new sentence (the regex crate has no look-ahead).
    let mut hard: Vec<(String, String)> = Vec::new();
    let mut last = 0;
    let mut prev_sep = String::new();
    for m in HARD_SEP.find_iter(goal) {
        if m.as_str().starts_with(['.', '!', '?']) && !goal[m.end()..].chars().next().map(|c| c.is_uppercase() || c.is_ascii_digit()).unwrap_or(false) {
            continue;
        }
        hard.push((goal[last..m.start()].to_string(), prev_sep.clone()));
        prev_sep = m.as_str().to_string();
        last = m.end();
    }
    hard.push((goal[last..].to_string(), prev_sep));
    // 2. soft boundaries: only where a new imperative clause starts
    let mut out: Vec<(String, String)> = Vec::new();
    for (chunk, sep0) in hard {
        let chunk = BULLET.replace(chunk.trim(), "").trim().trim_end_matches(['.', '!', '?']).to_string();
        if chunk.is_empty() {
            continue;
        }
        let mut start = 0;
        let mut sep = sep0;
        for m in SEP.find_iter(&chunk) {
            // a new clause starts at an imperative verb — or wherever a tool's own pattern matches
            let rest = &chunk[m.end()..];
            let head = SEP.find(rest).map(|n| &rest[..n.start()]).unwrap_or(rest); // just this clause
            if m.start() == 0 || !(starts_with_verb(rest) || binds(head)) {
                continue;
            }
            let piece = chunk[start..m.start()].trim();
            if !piece.is_empty() {
                out.push((piece.to_string(), sep.clone()));
            }
            sep = m.as_str().to_string();
            start = m.end();
        }
        let piece = chunk[start..].trim();
        if !piece.is_empty() {
            out.push((piece.to_string(), sep));
        }
    }
    out.into_iter().map(|(t, s)| (LEAD_ADVERB.replace(&t, "").trim().to_string(), s)).filter(|(t, _)| !t.is_empty()).collect()
}

/// "read a.txt, b.txt and c.txt" -> ["read a.txt", "read b.txt", "read c.txt"].
fn expand_enumeration(seg: &str, entities: &[Entity]) -> Option<Vec<String>> {
    for label in ["file path", "url"] {
        let mut items: Vec<&str> = Vec::new();
        for e in entities.iter().filter(|e| e.label == label) {
            if seg.contains(e.text.as_str()) && !items.contains(&e.text.as_str()) {
                items.push(&e.text);
            }
        }
        if items.len() < 2 || items.len() > 6 {
            continue;
        }
        items.sort_by_key(|t| seg.find(*t).unwrap_or(usize::MAX));
        let first = seg.find(items[0])?;
        let prefix = seg[..first].trim_end();
        if prefix.split_whitespace().count() == 0 {
            continue;
        }
        // everything after the first item must be just the other items and joiners
        let mut rest = seg[first + items[0].len()..].to_string();
        for it in &items[1..] {
            rest = rest.replacen(*it, "", 1);
        }
        if ENUM_REST.is_match(&rest) {
            return Some(items.iter().map(|it| format!("{prefix} {it}")).collect());
        }
    }
    None
}

/// Candidate split of `goal`, or `None` if it is a single task.
/// `binds(fragment)` says whether a fragment is a complete deterministic request
/// for some tool, which lets custom tools' verbs start a clause too.
pub fn decompose(goal: &str, entities: &[Entity], binds: &dyn Fn(&str) -> bool) -> Option<Decomposition> {
    let segs = segments(goal, binds);
    if segs.len() < 2 {
        // a lone enumeration ("read a, b and c") is still a fan-out
        let only = segs.first()?;
        let items = expand_enumeration(&only.0, entities)?;
        return build(items.into_iter().map(|t| (t, false)).collect(), None);
    }
    // a trailing "summarise / compare them" clause is the join step
    let mut segs = segs;
    let synthesis = if segs.last().is_some_and(|(last, _)| SYNTH.is_match(last)) { segs.pop().map(|(t, _)| t) } else { None };
    // coupled comparisons must stay in one head
    if segs.iter().any(|(t, _)| COUPLED.is_match(t)) {
        return None;
    }
    // (text, depends on the previous group)
    let mut groups: Vec<Vec<(String, bool)>> = Vec::new();
    for (i, (text, sep)) in segs.iter().enumerate() {
        let analysis_after_acquisition = i > 0 && ANALYZE.contains(&first_word(text).as_str()) && ACQUIRE.contains(&first_word(&segs[i - 1].0).as_str());
        let dependent = i > 0 && (SEQUENCE_CUE.is_match(sep) || SEQUENCE_CUE.is_match(text) || ANAPHORA.is_match(text) || analysis_after_acquisition);
        match expand_enumeration(text, entities) {
            Some(items) => groups.push(items.into_iter().map(|t| (t, dependent)).collect()),
            None => groups.push(vec![(text.clone(), dependent)]),
        }
    }
    build_groups(groups, synthesis)
}

fn build(items: Vec<(String, bool)>, synthesis: Option<String>) -> Option<Decomposition> {
    build_groups(vec![items], synthesis)
}

fn build_groups(groups: Vec<Vec<(String, bool)>>, synthesis: Option<String>) -> Option<Decomposition> {
    let mut subgoals: Vec<SubGoal> = Vec::new();
    let mut prev_ids: Vec<String> = Vec::new();
    for g in groups {
        let mut ids = Vec::new();
        for (text, dependent) in g {
            let id = format!("c{}", subgoals.len() + 1);
            subgoals.push(SubGoal { id: id.clone(), text, depends_on: if dependent { prev_ids.clone() } else { vec![] } });
            ids.push(id);
        }
        prev_ids = ids;
    }
    let n = subgoals.len();
    let words_ok = subgoals.iter().all(|s| s.text.split_whitespace().count() >= 2 && s.text.split_whitespace().count() <= 60);
    if !(2..=MAX_SUBGOALS).contains(&n) || !words_ok {
        return None;
    }
    Some(Decomposition { subgoals, synthesis })
}

/// The coordinator's plan: one `Agent` step per sub-goal (+ a reasoning join).
pub fn plan_from_decomposition(goal: &str, d: &Decomposition, direct: &[bool]) -> Plan {
    let mut steps: Vec<Step> = d
        .subgoals
        .iter()
        .zip(direct)
        .map(|(s, free)| {
            let mut st = Step::reasoning(&s.id, &s.text);
            st.kind = StepKind::Agent;
            st.depends_on = s.depends_on.clone();
            // a hint for the cost model: a deterministic agent needs no model
            st.args = json!({"direct": free});
            st
        })
        .collect();
    if let Some(text) = &d.synthesis {
        let mut j = Step::reasoning("join", text);
        j.depends_on = d.subgoals.iter().map(|s| s.id.clone()).collect();
        steps.push(j);
    }
    Plan { goal: goal.into(), steps, source: PlanSource::Team, revision: 0, estimate: None, report: None }
}

/// Facts the decision layer judges the split on.
pub fn team_facts(d: &Decomposition, direct: &[bool], models_available: bool, cost: &crate::economy::CostModel, external: bool) -> TeamFacts {
    let is_direct = |id: &str| d.subgoals.iter().position(|s| s.id == id).map(|i| direct[i]).unwrap_or(false);
    let mut parallel_model = 0;
    let mut model_after_direct = 0;
    for (s, dir) in d.subgoals.iter().zip(direct) {
        if *dir {
            continue;
        }
        if s.depends_on.is_empty() {
            parallel_model += 1;
        } else if s.depends_on.iter().all(|p| is_direct(p)) {
            model_after_direct += 1;
        }
    }
    let model_parts = direct.iter().filter(|free| !**free).count();
    let (flat, team) = cost.flat_vs_team(model_parts, d.synthesis.is_some(), external);
    TeamFacts {
        subgoals: d.subgoals.iter().map(|s| tokens::truncate_tokens(&s.text, 14)).collect(),
        direct: direct.iter().filter(|b| **b).count(),
        dependent: d.subgoals.iter().filter(|s| !s.depends_on.is_empty()).count(),
        parallel_model,
        model_after_direct,
        mutating: d.subgoals.iter().filter(|s| MUTATE.contains(&first_word(&s.text).as_str())).count(),
        has_synthesis: d.synthesis.is_some(),
        models_available,
        est_flat_tokens: flat,
        est_team_tokens: team,
    }
}

// ---------------------------------------------------------------- roles ----

static VERIFY_VERB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(?:please\s+)?(?:verify|check|confirm|validate|ensure|test|inspect)\b").unwrap());

/// Pick the capability class for a sub-task from what it will actually do.
///
/// * a deterministic binding that changes state (or runs a command that might) → operator
/// * risky / maintenance work → operator
/// * no tools needed → analyst
/// * "verify / check …" → verifier, otherwise researcher
pub fn pick_role(goal: &str, routing: &Routing, bound_tools: Option<&[ToolMetadata]>) -> AgentRole {
    if let Some(tools) = bound_tools {
        if tools.iter().any(|t| t.side_effects) {
            return AgentRole::Operator;
        }
        return if VERIFY_VERB.is_match(goal) { AgentRole::Verifier } else { AgentRole::Researcher };
    }
    // the decision model's judgement of the work, when it gave one
    if let Some(role) = routing.role.as_deref() {
        match role {
            "operator" => return AgentRole::Operator,
            "analyst" => return AgentRole::Analyst,
            "verifier" => return AgentRole::Verifier,
            "researcher" => return AgentRole::Researcher,
            _ => {}
        }
    }
    if routing.risk >= Risk::Moderate || routing.task_type == "maintenance" {
        AgentRole::Operator
    } else if !routing.needs_tools {
        AgentRole::Analyst
    } else if VERIFY_VERB.is_match(goal) {
        AgentRole::Verifier
    } else {
        AgentRole::Researcher
    }
}

/// Tools a role may use (`None` = unrestricted; policy still applies).
pub fn role_scope(role: AgentRole, metas: &[ToolMetadata]) -> Option<Vec<String>> {
    match role {
        AgentRole::Coordinator | AgentRole::Operator => None,
        AgentRole::Researcher | AgentRole::Verifier => Some(metas.iter().filter(|m| !m.side_effects).map(|m| m.name.clone()).collect()),
        AgentRole::Analyst => {
            Some(metas.iter().filter(|m| !m.side_effects && matches!(m.category.as_str(), "math" | "memory")).map(|m| m.name.clone()).collect())
        }
    }
}

// --------------------------------------------------------- shared state ----

/// State shared by every member of one team: a result cache for read-only
/// tools. Any side effect by any member invalidates all cached results.
#[derive(Default)]
pub struct TeamShared {
    cache: Mutex<HashMap<String, (u64, String)>>,
    generation: AtomicU64,
    pub hits: AtomicU32,
}

impl TeamShared {
    pub fn key(tool: &str, args: &serde_json::Value) -> String {
        sha256_hex(&format!("{tool}|{}", canonical_json(args)))
    }
    pub fn get(&self, key: &str) -> Option<String> {
        let now = self.generation.load(Ordering::SeqCst);
        let hit = self.cache.lock().get(key).filter(|(g, _)| *g == now).map(|(_, c)| c.clone());
        if hit.is_some() {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }
        hit
    }
    /// `started_at` is the generation observed *before* the tool ran, so a
    /// result computed across a concurrent write is never cached.
    pub fn put(&self, key: String, content: String, started_at: u64) {
        if self.generation.load(Ordering::SeqCst) == started_at {
            self.cache.lock().insert(key, (started_at, content));
        }
    }
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfae_extract::RuleExtractor;

    fn d(goal: &str) -> Option<Decomposition> {
        let ents = RuleExtractor::new().run(goal);
        decompose(goal, &ents, &|_| false)
    }

    fn texts(d: &Decomposition) -> Vec<&str> {
        d.subgoals.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn splits_independent_clauses() {
        let x = d("explain what a mutex is and explain what a semaphore is").unwrap();
        assert_eq!(texts(&x), vec!["explain what a mutex is", "explain what a semaphore is"]);
        assert!(x.subgoals.iter().all(|s| s.depends_on.is_empty()));
        assert!(x.synthesis.is_none());
    }

    #[test]
    fn analysis_after_acquisition_is_a_dependency() {
        let x = d("list the files in ./ and explain what a mutex is").unwrap();
        assert_eq!(x.subgoals[1].depends_on, vec!["c1"], "'tell/explain …' after 'read/list …' is about what was acquired");
        let y = d("read todo.md and tell me which tasks are still open").unwrap();
        assert_eq!(y.subgoals[1].depends_on, vec!["c1"]);
    }

    #[test]
    fn comma_lists_and_numbered_items() {
        let x = d("Check the disk usage, read todo.md, and count the open tasks").unwrap();
        assert_eq!(x.subgoals.len(), 3, "{x:?}");
        let y = d("1. check the disk usage\n2. read todo.md\n3. explain what a mutex is").unwrap();
        assert_eq!(y.subgoals.len(), 3);
        assert!(y.subgoals[0].text.starts_with("check"));
    }

    #[test]
    fn sequence_and_anaphora_make_a_pipeline() {
        let x = d("Find the latest PostgreSQL vulnerabilities, check whether my server is affected, and patch it if necessary").unwrap();
        assert_eq!(x.subgoals.len(), 3, "{x:?}");
        assert!(x.subgoals[0].depends_on.is_empty());
        assert_eq!(x.subgoals[1].depends_on, vec!["c1"], "'whether … affected' refers to what was found");
        assert_eq!(x.subgoals[2].depends_on, vec!["c2"], "'patch it if necessary' acts on the previous result");
    }

    #[test]
    fn then_is_a_dependency_and_plain_and_is_not() {
        let x = d("read todo.md then explain what it says").unwrap();
        assert_eq!(x.subgoals[1].depends_on, vec!["c1"]);
        let y = d("explain what a mutex is and check the disk usage").unwrap();
        assert!(y.subgoals[1].depends_on.is_empty());
    }

    #[test]
    fn enumerations_fan_out_and_a_trailing_summary_becomes_the_join() {
        let x = d("read a.txt, b.txt and c.txt and summarize them").unwrap();
        assert_eq!(texts(&x), vec!["read a.txt", "read b.txt", "read c.txt"]);
        assert!(x.subgoals.iter().all(|s| s.depends_on.is_empty()));
        assert_eq!(x.synthesis.as_deref(), Some("summarize them"));
        let lone = d("fetch https://example.com/a, https://example.com/b").unwrap();
        assert_eq!(lone.subgoals.len(), 2);
    }

    #[test]
    fn things_that_must_stay_whole() {
        assert!(d("what is 2 + 2").is_none());
        assert!(d("read a.txt and summarize it").is_none(), "one sub-task + a join is just one task");
        assert!(d("compare Rust and Go for systems programming").is_none());
        assert!(d("read a.txt and compare it with b.txt").is_none());
        assert!(d("What are the pros and cons of Rust, and which should I use?").is_none());
        assert!(d("salt and pepper").is_none());
        assert!(d("explain how a hash table works").is_none());
    }

    #[test]
    fn too_many_subgoals_are_refused() {
        let goal = (1..=10).map(|i| format!("read f{i}.txt")).collect::<Vec<_>>().join("; ");
        assert!(d(&goal).is_none());
    }

    #[test]
    fn plan_has_agent_steps_and_a_join() {
        let x = d("read a.txt, b.txt and summarize them").unwrap();
        let p = plan_from_decomposition("g", &x, &vec![true; x.subgoals.len()]);
        assert_eq!(p.source, PlanSource::Team);
        assert_eq!(p.steps.iter().filter(|s| s.kind == StepKind::Agent).count(), 2);
        let j = p.steps.last().unwrap();
        assert_eq!((j.kind, j.id.as_str()), (StepKind::Reasoning, "join"));
        assert_eq!(j.depends_on, vec!["c1", "c2"]);
    }

    #[test]
    fn roles_follow_what_the_task_will_do() {
        let r = |risk, needs, ty: &str| Routing::new(ty, 0.3, needs, risk, 0.8);
        let meta = |side| ToolMetadata {
            name: "t".into(),
            description: String::new(),
            category: "x".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: side,
            reversible: true,
            cost_class: "free".into(),
            params: vec![],
            keywords: vec![],
            direct_patterns: vec![],
            cacheable: false,
            contract: vec![],
        };
        assert_eq!(pick_role("write x", &r(Risk::Low, true, "x"), Some(&[meta(true)])), AgentRole::Operator);
        assert_eq!(pick_role("read x", &r(Risk::Low, true, "x"), Some(&[meta(false)])), AgentRole::Researcher);
        assert_eq!(pick_role("check disk", &r(Risk::Low, true, "x"), Some(&[meta(false)])), AgentRole::Verifier);
        assert_eq!(pick_role("patch the server", &r(Risk::Moderate, true, "maintenance"), None), AgentRole::Operator);
        assert_eq!(pick_role("explain mutexes", &r(Risk::Low, false, "conversation"), None), AgentRole::Analyst);
        assert_eq!(pick_role("find the report", &r(Risk::Low, true, "research"), None), AgentRole::Researcher);
    }

    #[test]
    fn shared_cache_is_invalidated_by_side_effects_and_races() {
        let t = TeamShared::default();
        let k = TeamShared::key("fs_read", &json!({"path": "a"}));
        let g0 = t.generation();
        t.put(k.clone(), "alpha".into(), g0);
        assert_eq!(t.get(&k).as_deref(), Some("alpha"));
        assert_eq!(t.hits.load(Ordering::SeqCst), 1);
        t.invalidate();
        assert!(t.get(&k).is_none(), "a write anywhere in the team drops cached reads");
        // a read that started before a write must not be cached after it
        t.put(k.clone(), "stale".into(), g0);
        assert!(t.get(&k).is_none());
    }
}
