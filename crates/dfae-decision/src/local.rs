//! Deterministic decision engine answering the same typed questions as a decision model.
//!
//! Used when no hosted decision model is configured and as the fallback when
//! it is unavailable. It is intentionally simple and inspectable: lexicons,
//! BM25 over tool metadata, and rules over facts the runtime computed.

use crate::questions::*;
use async_trait::async_trait;
use dfae_core::text::{bm25_scores, content_tokens, stem};
use dfae_core::*;
use regex::Regex;
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;
use std::time::Instant;

const TYPE_LEXICON: &[(&str, &[&str])] = &[
    (
        "research",
        &[
            "research",
            "search",
            "latest",
            "news",
            "web",
            "internet",
            "lookup",
            "documentation",
            "vulnerability",
            "cve",
            "article",
            "source",
            "wiki",
            "summarize",
            "papers",
            "weather",
            "temperature",
            "forecast",
            "online",
            "google",
            "browse",
            "website",
        ],
    ),
    (
        "coding",
        &[
            "code",
            "function",
            "script",
            "program",
            "implement",
            "bug",
            "compile",
            "refactor",
            "test",
            "rust",
            "python",
            "javascript",
            "git",
            "commit",
            "debug",
            "edit",
            "modify",
            "rename",
            "replace",
            "build",
            "lint",
            "install",
            "dependency",
            "class",
            "module",
        ],
    ),
    (
        "system_diagnostics",
        &[
            "system",
            "disk",
            "memory",
            "cpu",
            "load",
            "uptime",
            "process",
            "diagnostic",
            "health",
            "server",
            "log",
            "network",
            "port",
            "service",
            "usage",
            "resource",
            "time",
            "clock",
            "date",
            "hostname",
        ],
    ),
    (
        "file_operations",
        &["file", "directory", "folder", "path", "read", "list", "cat", "ls", "write", "save", "grep", "open", "workspace", "content", "contents", "text"],
    ),
    (
        "calculation",
        &[
            "calculate",
            "compute",
            "sum",
            "multiply",
            "divide",
            "plus",
            "minus",
            "times",
            "percent",
            "math",
            "evaluate",
            "equation",
            "square",
            "root",
            "product",
        ],
    ),
    ("data_analysis", &["analyze", "analyse", "statistic", "csv", "dataset", "trend", "correlation", "chart", "aggregate", "metric", "average", "compare"]),
    ("maintenance", &["patch", "deploy", "restart", "install", "upgrade", "update", "fix", "migrate", "rollback", "configure", "delete", "remove", "reboot"]),
    ("memory", &["remember", "recall", "memory", "forget", "store", "note", "know", "learned"]),
    ("conversation", &["hello", "hi", "thanks", "thank", "explain", "tell", "joke", "opinion", "chat", "meaning"]),
];

const HIGH_RISK: &[&str] =
    &["delete", "remove", "drop", "rm", "format", "shutdown", "truncate", "overwrite", "kill", "reboot", "destroy", "wipe", "erase", "purge"];
const MODERATE_RISK: &[&str] = &[
    "write",
    "create",
    "save",
    "deploy",
    "patch",
    "install",
    "restart",
    "update",
    "edit",
    "modify",
    "upgrade",
    "migrate",
    "remember",
    "append",
    "rename",
    "move",
    "chmod",
    "replace",
    "substitute",
    "change",
    "fix",
    "refactor",
    "rewrite",
    "correct",
    "insert",
    "mkdir",
    "copy",
];
const REASONING_VERBS: &[&str] = &[
    "analyze",
    "analyse",
    "investigate",
    "explain",
    "why",
    "compare",
    "design",
    "implement",
    "write",
    "summarize",
    "diagnose",
    "debug",
    "refactor",
    "evaluate",
    "recommend",
    "decide",
    "plan",
    "review",
];

/// "create a landing page", "build an API", "write a script": a deliverable, which lives in the workspace.
static CREATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:create|build|make|generate|write|design|develop|implement|code|scaffold|set\s+up)\b[^.?!\n]{0,60}\b(?:page|website|web\s*site|site|app|application|script|program|api|server|component|dashboard|game|project|tool|bot|module|service|landing|html|css|backend|frontend|library|cli|file|files)\b",
    )
    .unwrap()
});
/// "serve it", "run that", "persist it": an action on something made earlier.
static ACT_ON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:serve|host|run|start|launch|deploy|persist|save|store|put)\b[^.?!\n]{0,30}\b(?:it|that|this|them|these|the\s+(?:code|page|site|app|file|files|server|script))\b").unwrap()
});

// ---- the fallback's judgement of the extra routing questions (a decision model answers these itself) ----

static CHANGE_VERBS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:replace|substitute|swap|change|edit|modify|update|fix|correct|rewrite|refactor|rename|insert|append|prepend|create|make|write|save|add|remove|delete|erase|move|copy|mkdir|install|deploy|patch|restart|commit|overwrite|serve|start|launch)\b",
    )
    .unwrap()
});
static CONDITIONAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:if\s+(?:necessary|needed|required|any|appropriate|possible|applicable)|when\s+(?:necessary|needed)|as\s+needed|propose|suggest|draft|recommend|should\s+i)\b").unwrap()
});
static GREETING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:hi|hello|hey|hiya|howdy|yo|greetings|good\s+(?:morning|afternoon|evening|day)|sup)\b[\s,.!-]*").unwrap());
static HOW_ARE_YOU: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:so[,\s]+|and\s+)?(?:tell\s+me\s+|can\s+you\s+tell\s+me\s+)?(?:how\s+are\s+(?:you|u)(?:\s+(?:doing|today|going))*|how\s+do\s+you\s+do|how(?:'s|\s+is)\s+it\s+going|how\s+have\s+you\s+been|what'?s\s+up|how(?:'re|\s+are)\s+things)\s*[?!.]*$").unwrap()
});
static THANKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:thanks|thank\s+you|thx|ty|cheers|much\s+appreciated|great,?\s+thanks)(?:\s+(?:a\s+lot|so\s+much|very\s+much|again))?\s*[.!]*$")
        .unwrap()
});
static BYE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:bye|goodbye|see\s+you|see\s+ya|good\s*night|cya)(?:\s+\w+)?\s*[.!]*$").unwrap());
static ABOUT_YOU: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:who\s+are\s+you|what\s+are\s+you|what\s+can\s+you\s+do|what\s+do\s+you\s+do|help|what\s+are\s+your\s+(?:capabilities|abilities))\s*[?!.]*$",
    )
    .unwrap()
});
static VERIFY_VERB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(?:please\s+)?(?:verify|check|confirm|validate|ensure|test|inspect)\b").unwrap());
static REF_PLACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:my|our)\s+(?:current\s+)?(?:city|town|location|area|place|neighbou?rhood)\b|\bwhere\s+i\s+(?:live|am)\b").unwrap());
static REF_HERE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:here|outside)\b").unwrap());
static WEATHERISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:weather|temperature|forecast|rain\w*|snow\w*|hot|cold|warm)\b").unwrap());
static REF_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:that|this|the\s+same|the\s+above|the\s+previous|the\s+last)\s+file\b|\b(?:read|open|show|cat|print|display|view|delete|remove|edit|update|summari[sz]e|check|list|append\s+to|write\s+to)\s+(?:me\s+)?it\b").unwrap()
});
static REF_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:that|this|the\s+same)\s+(?:url|link|page|site|website)\b").unwrap());
static REF_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:that|this|these|those|the)\s+(?:code|snippet|script|page|html|program|function)\b|\b(?:save|persist|store|put|write|serve|run|host|deploy)\s+(?:it|that|this|them)\b").unwrap()
});

fn smalltalk_kind(req: &str) -> &'static str {
    let rest = GREETING.find(req).map(|m| &req[m.end()..]).unwrap_or(req).trim_matches(|c: char| c == '.' || c == ',' || c.is_whitespace());
    if rest.is_empty() {
        return if GREETING.is_match(req) { "greeting" } else { "none" };
    }
    if HOW_ARE_YOU.is_match(rest) {
        "how_are_you"
    } else if THANKS.is_match(rest) {
        "thanks"
    } else if BYE.is_match(rest) {
        "farewell"
    } else if ABOUT_YOU.is_match(rest) {
        "about_you"
    } else {
        "none"
    }
}

static FEEDBACK_NEG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:that(?:'s| is| was)? (?:wrong|not right|incorrect|not what)|not what i (?:asked|meant|wanted)|try again|do it again|doesn'?t work|does not work|didn'?t work|still (?:fails|broken|wrong|not working)|you (?:got it wrong|missed)|it(?:'s| is) (?:broken|wrong))\b").unwrap()
});
static FEEDBACK_POS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:perfect|great|that works|works now|exactly|nice|awesome|well done|looks good)\b").unwrap());

fn feedback_kind(req: &str) -> &'static str {
    if FEEDBACK_NEG.is_match(req) {
        "negative"
    } else if FEEDBACK_POS.is_match(req) {
        "positive"
    } else {
        "none"
    }
}

static ARITH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(\.\d+)?\s*[-+*/^%x×÷]\s*\d+").unwrap());
static CONJ: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(and then|then|after that|afterwards|and|also|but|unless|if|before|after|while|otherwise)\b|[;,]").unwrap());

fn stems(words: &[&str]) -> HashSet<String> {
    words.iter().map(|w| stem(w)).collect()
}

pub struct LocalDecider {
    thresholds: Thresholds,
}

impl LocalDecider {
    pub fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }

    fn task_type(&self, s: &DecisionState) -> (String, f64, Vec<(String, f64)>) {
        let toks: HashSet<String> = content_tokens(&s.request).into_iter().collect();
        let mut scores: Vec<(String, f64)> = TYPE_LEXICON
            .iter()
            .map(|(t, words)| {
                let hits = stems(words).intersection(&toks).count() as f64;
                (t.to_string(), hits)
            })
            .collect();
        if ARITH.is_match(&s.request)
            && let Some(e) = scores.iter_mut().find(|(t, _)| t == "calculation")
        {
            e.1 += 2.0;
        }
        for e in &s.entities {
            let boost = match e.label.as_str() {
                "file path" | "path" | "directory" => Some("file_operations"),
                "url" => Some("research"),
                "server" | "database" => Some("system_diagnostics"),
                "repository" => Some("coding"),
                _ => None,
            };
            if let Some(b) = boost
                && let Some(x) = scores.iter_mut().find(|(t, _)| t == b)
            {
                x.1 += 0.75;
            }
        }
        let total: f64 = scores.iter().map(|(_, v)| v).sum();
        if total == 0.0 {
            let probs = vec![("conversation".to_string(), 0.5), ("other".to_string(), 0.5)];
            return ("other".into(), 0.4, probs);
        }
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let top = scores[0].clone();
        let conf = (top.1 / (total + 0.5)).clamp(0.3, 0.95);
        let probs = scores.iter().map(|(t, v)| (t.clone(), v / total)).collect();
        (top.0, conf, probs)
    }

    fn complexity(&self, s: &DecisionState) -> f64 {
        let words = s.request.split_whitespace().count() as f64;
        let clauses = CONJ.find_iter(&s.request).count() as f64;
        let toks: HashSet<String> = tokenize_set(&s.request);
        let reasoning = stems(REASONING_VERBS).intersection(&toks).count() as f64;
        let c = 0.08 + 0.012 * words.min(60.0) + 0.1 * clauses.min(5.0) + 0.18 * reasoning.min(3.0) + 0.04 * (s.entities.len() as f64).min(5.0);
        c.clamp(0.0, 1.0)
    }

    fn risk(&self, s: &DecisionState) -> (Risk, f64) {
        let toks = tokenize_set(&s.request);
        let high = stems(HIGH_RISK).intersection(&toks).count();
        let moderate = stems(MODERATE_RISK).intersection(&toks).count();
        let env_prod = s.entities.iter().any(|e| e.label == "environment" && e.text.to_lowercase().contains("prod"));
        if high > 0 || (moderate > 0 && env_prod) {
            (Risk::High, 0.8)
        } else if moderate > 0 {
            (Risk::Moderate, 0.75)
        } else {
            (Risk::Low, 0.85)
        }
    }

    fn tool_scores(&self, s: &DecisionState) -> Vec<(String, f64)> {
        if s.tools.is_empty() {
            return vec![];
        }
        let docs: Vec<String> =
            s.tools.iter().map(|t| format!("{} {} {} {}", t.name.replace('_', " "), t.description, t.category, t.keywords.join(" "))).collect();
        // Score only the words the tools' vocabulary knows: chatter around the request ("hi, I'm Sam from
        // New York, tell me the weather") must not dilute a clear match.
        // File names, URLs and the like are data, not wording: `test.txt` must not make the `test` tool look relevant.
        let mut wording = s.request.clone();
        for e in s
            .entities
            .iter()
            .filter(|e| matches!(e.label.as_str(), "file path" | "path" | "directory" | "url" | "expression" | "version" | "ip address" | "email"))
        {
            wording = wording.replace(&e.text, " ");
        }
        let vocab: HashSet<String> = docs.iter().flat_map(|d| content_tokens(d)).collect();
        let query = content_tokens(&wording).into_iter().filter(|t| vocab.contains(t)).collect::<Vec<_>>().join(" ");
        let bm = if query.is_empty() { vec![0.0; docs.len()] } else { bm25_scores(&query, &docs) };
        s.tools
            .iter()
            .zip(bm)
            .map(|(t, b)| {
                let mut p = 1.0 - (-3.0 * b as f64).exp();
                for e in &s.entities {
                    let cat = match e.label.as_str() {
                        "url" => Some("web"),
                        "file path" | "path" | "directory" => Some("filesystem"),
                        "server" | "database" => Some("system"),
                        _ => None,
                    };
                    if cat == Some(t.category.as_str()) {
                        p += 0.25;
                    }
                }
                (t.name.clone(), p.clamp(0.0, 1.0))
            })
            .collect()
    }

    pub fn answer(&self, req: &DecisionRequest) -> BTreeMap<String, Answer> {
        let s = &req.state;
        let mut out = BTreeMap::new();
        match req.kind {
            DecisionKind::RouteTask => {
                let (tt, conf, probs) = self.task_type(s);
                let probs: Vec<(&str, f64)> = probs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
                out.insert("task_type".into(), Answer::choice(&tt, conf, &probs));
                let c = self.complexity(s);
                out.insert("complexity".into(), Answer::score(c * 2.0, 0.6));
                let best_tool = self.tool_scores(s).iter().map(|(_, p)| *p).fold(0.0, f64::max);
                let tool_type =
                    matches!(tt.as_str(), "file_operations" | "system_diagnostics" | "calculation" | "maintenance" | "memory" | "research" | "coding");
                let mut needs = if tool_type { best_tool.max(0.7) } else { best_tool * 0.9 };
                if CREATE.is_match(&s.request) || ACT_ON.is_match(&s.request) {
                    needs = needs.max(0.8);
                }
                out.insert("needs_tools".into(), Answer::noul(needs));
                let (r, rc) = self.risk(s);
                let key = match r {
                    Risk::Low => "low",
                    Risk::Moderate => "moderate",
                    Risk::High => "high",
                };
                out.insert("risk".into(), Answer::choice(key, rc, &[(key, rc)]));
                out.insert("changes_state".into(), Answer::noul(if CHANGE_VERBS.is_match(&s.request) { 0.85 } else { 0.1 }));
                out.insert("conditional_change".into(), Answer::noul(if CONDITIONAL.is_match(&s.request) { 0.85 } else { 0.1 }));
                let kind = smalltalk_kind(&s.request);
                out.insert("smalltalk".into(), Answer::choice(kind, 0.9, &[(kind, 0.9)]));
                let fb = feedback_kind(&s.request);
                out.insert("feedback".into(), Answer::choice(fb, 0.8, &[(fb, 0.8)]));
                let role = if r >= Risk::Moderate || tt == "maintenance" {
                    "operator"
                } else if needs < 0.5 {
                    "analyst"
                } else if VERIFY_VERB.is_match(&s.request) {
                    "verifier"
                } else {
                    "researcher"
                };
                out.insert("role".into(), Answer::choice(role, 0.7, &[(role, 0.7)]));
            }
            DecisionKind::ResolveReference => {
                // which of the known things does the request lean on? (a model reads the sentence; this fallback reads phrases)
                let has = |k: &str| s.known.iter().any(|x| x.starts_with(&format!("{k}:")));
                let req = &s.request;
                let pick = if has("place") && (REF_PLACE.is_match(req) || (WEATHERISH.is_match(req) && REF_HERE.is_match(req))) {
                    "place"
                } else if has("file") && REF_FILE.is_match(req) {
                    "file"
                } else if has("url") && REF_URL.is_match(req) {
                    "url"
                } else if has("code") && REF_CODE.is_match(req) {
                    "code"
                } else {
                    "none"
                };
                out.insert("refers_to".into(), Answer::choice(pick, 0.8, &[(pick, 0.8)]));
            }
            DecisionKind::SelectTools => {
                for (name, p) in self.tool_scores(s) {
                    out.insert(tool_key(&name), Answer::noul(p));
                }
            }
            DecisionKind::SelectMode => {
                let f = s.mode_facts.clone().unwrap_or_default();
                let (choice, conf) = if f.direct_bindable {
                    ("direct", if f.skill_score.unwrap_or(0.0) > 0.0 { 0.92 } else { 0.88 })
                } else if f.planner_available && f.skill_score.is_none() && f.complexity >= f.plan_min_complexity.max(0.0) && f.plan_min_complexity > 0.0 {
                    // the policy: medium and complex work is planned by the planner model
                    ("escalated", 0.9)
                } else if f.complexity >= 0.85 && f.planner_available {
                    ("escalated", 0.6)
                } else {
                    ("assisted", 0.8)
                };
                out.insert("mode".into(), Answer::choice(choice, conf, &[(choice, conf)]));
            }
            DecisionKind::AssessPlan => {
                let f = s.plan_facts.clone().unwrap_or_default();
                let p = if f.steps == 0 {
                    0.05
                } else if f.max_risk == Some(Risk::High) {
                    0.55
                } else {
                    0.88
                };
                out.insert("plan_ok".into(), Answer::noul(p));
            }
            DecisionKind::AssessRecovery => {
                let f = s.step_facts.clone().unwrap_or_default();
                let transient = matches!(f.error_kind.as_str(), "timeout" | "transient" | "io");
                let (c, conf) = if f.error_kind == "policy" || f.error_kind == "cancelled" {
                    ("fail", 0.97)
                } else if transient && f.attempts < f.max_attempts {
                    ("retry", 0.85)
                } else if f.optional {
                    ("skip", 0.8)
                } else if f.replans_used < f.max_replans {
                    ("replan", 0.7)
                } else if f.planner_available {
                    ("escalate", 0.6)
                } else {
                    ("fail", 0.75)
                };
                out.insert("recovery".into(), Answer::choice(c, conf, &[(c, conf)]));
            }
            DecisionKind::AssessCompletion => {
                let f = s.completion_facts.clone().unwrap_or_default();
                let p = if f.steps_total == 0 {
                    0.0
                } else {
                    let frac = f.steps_ok as f64 / f.steps_total as f64;
                    let penalty = 0.5 * f.checks_failed.min(2) as f64;
                    (0.95 * frac - penalty).max(0.0)
                };
                out.insert("complete".into(), Answer::noul(p));
            }
            DecisionKind::AssessDecomposition => {
                let f = s.team_facts.clone().unwrap_or_default();
                let n = f.subgoals.len();
                let p = if n < 2 || (f.direct == n && !f.has_synthesis) {
                    // nothing to split, or Direct mode already handles it for free
                    0.05
                } else if !f.models_available && (f.direct < n || f.has_synthesis) {
                    // the parts that need a model (incl. the join) could not run anyway
                    0.05
                } else {
                    // A split is worth it when it buys parallel model work, replaces planned
                    // tool steps with free deterministic agents, or separates read-only
                    // from state-changing work (least privilege + serialised side effects).
                    let mut p = 0.35f64;
                    if f.parallel_model >= 2 {
                        p += 0.30;
                    }
                    if f.direct >= 2 && f.has_synthesis {
                        p += 0.30;
                    }
                    if f.direct >= 1 && f.parallel_model >= 1 {
                        p += 0.10;
                    }
                    if f.dependent == 0 {
                        p += 0.05;
                    }
                    let capability_pipeline = n >= 3 && f.dependent >= 1 && f.mutating >= 1 && f.mutating < n;
                    if capability_pipeline {
                        p += 0.40;
                    } else if f.model_after_direct > 0 && f.parallel_model == 0 {
                        p -= 0.15; // one plan does this as well, without the handoff
                    }
                    // The learned cost model: every agent pays its own planning call, one plan
                    // pays once. A split must be worth the extra tokens it is priced at.
                    // (Not charged to a capability pipeline: its value is least privilege and
                    // serialised side effects, not tokens.)
                    if !capability_pipeline {
                        p -= (f.est_team_tokens.saturating_sub(f.est_flat_tokens) as f64 / 4000.0).min(0.2);
                    }
                    p.clamp(0.0, 0.95)
                };
                out.insert("split".into(), Answer::noul(p));
            }
            DecisionKind::AssessEscalation => {
                let f = s.step_facts.clone().unwrap_or_default();
                let p = (0.25 * f.replans_used as f64 + 0.2 * f.attempts as f64).min(0.95);
                out.insert("escalate".into(), Answer::noul(p));
            }
        }
        let _ = &self.thresholds;
        out
    }
}

fn tokenize_set(s: &str) -> HashSet<String> {
    dfae_core::text::tokenize(s).into_iter().map(|t| stem(&t)).collect()
}

#[async_trait]
impl DecisionProvider for LocalDecider {
    fn name(&self) -> &str {
        "local"
    }
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let started = Instant::now();
        let answers = self.answer(&request);
        Ok(DecisionResponse {
            answers,
            provider: "local".into(),
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec() -> LocalDecider {
        LocalDecider::new(Config::default().thresholds)
    }

    fn state(req: &str) -> DecisionState {
        DecisionState { request: req.into(), ..Default::default() }
    }

    #[test]
    fn routes_task_types() {
        let d = dec();
        let r = d.answer(&route_request(state("calculate 12 * (3 + 4)")));
        assert!(matches!(&r["task_type"], Answer::Choice { choice, .. } if choice == "calculation"));
        let r = d.answer(&route_request(state("show the disk usage and memory of the server")));
        assert!(matches!(&r["task_type"], Answer::Choice { choice, .. } if choice == "system_diagnostics"));
        let r = d.answer(&route_request(state("delete all files in /workspace")));
        assert!(matches!(&r["risk"], Answer::Choice { choice, .. } if choice == "high"));
    }

    #[test]
    fn complexity_grows_with_request_size() {
        let d = dec();
        let a = d.complexity(&state("list files"));
        let b = d.complexity(&state("investigate why the deployment failed, compare the logs and then write a fix and explain it"));
        assert!(b > a + 0.3, "{a} vs {b}");
    }

    #[test]
    fn recovery_never_retries_policy_denials() {
        let d = dec();
        let mut st = state("x");
        st.step_facts = Some(StepFacts { error_kind: "policy".into(), attempts: 0, max_attempts: 3, ..Default::default() });
        let r = d.answer(&recovery_request(st));
        assert!(matches!(&r["recovery"], Answer::Choice { choice, .. } if choice == "fail"));
    }

    fn facts(n: usize, direct: usize, dependent: usize, parallel_model: usize, model_after_direct: usize, mutating: usize, synth: bool) -> DecisionState {
        DecisionState {
            request: "x".into(),
            team_facts: Some(TeamFacts {
                est_flat_tokens: 0,
                est_team_tokens: 0,
                subgoals: vec!["g".into(); n],
                direct,
                dependent,
                parallel_model,
                model_after_direct,
                mutating,
                has_synthesis: synth,
                models_available: true,
            }),
            ..Default::default()
        }
    }

    fn split_p(st: DecisionState) -> f64 {
        match &dec().answer(&decomposition_request(st))["split"] {
            Answer::Noul { noul } => *noul,
            _ => panic!(),
        }
    }

    #[test]
    fn splits_only_when_it_helps() {
        // fan-out over deterministic agents + one join
        assert!(split_p(facts(3, 3, 0, 0, 0, 0, true)) >= 0.6);
        // two independent model-needing parts run in parallel
        assert!(split_p(facts(2, 0, 0, 2, 0, 0, false)) >= 0.6);
        // research -> verify -> patch: separates read-only from state-changing work
        assert!(split_p(facts(3, 0, 2, 0, 0, 1, false)) >= 0.6);
        // "read x and tell me what it says": one plan does this as well
        assert!(split_p(facts(2, 1, 1, 0, 1, 0, false)) < 0.6);
        // a free step next to a single model step gains nothing
        assert!(split_p(facts(2, 1, 0, 1, 0, 0, false)) < 0.6);
        // an all-read-only pipeline stays one task
        assert!(split_p(facts(3, 0, 2, 0, 0, 0, false)) < 0.6);
        // everything deterministic: Direct mode already handles it for free
        assert!(split_p(facts(3, 3, 0, 0, 0, 0, false)) < 0.1);
        // a split that needs a model that is not there
        let mut st = facts(2, 0, 0, 2, 0, 0, false);
        st.team_facts.as_mut().unwrap().models_available = false;
        assert!(split_p(st) < 0.1);
    }

    #[test]
    fn the_priced_cost_of_extra_planning_calls_counts_against_a_split() {
        let base = facts(2, 0, 0, 2, 0, 0, false);
        let free = split_p(base.clone());
        let mut priced = base;
        // two model agents: 2 x (plan + step) versus plan + 2 x step
        priced.team_facts.as_mut().unwrap().est_flat_tokens = 630;
        priced.team_facts.as_mut().unwrap().est_team_tokens = 960;
        let costly = split_p(priced);
        assert!(costly < free && free - costly < 0.1, "{free} -> {costly}: a modest penalty, not a veto");
        assert!(costly >= 0.6, "parallelism still justifies it");
        let mut huge = facts(2, 0, 0, 2, 0, 0, false);
        huge.team_facts.as_mut().unwrap().est_flat_tokens = 600;
        huge.team_facts.as_mut().unwrap().est_team_tokens = 9000;
        assert!(split_p(huge) < 0.6, "a split priced far above the flat plan is refused");
        // ...but a pipeline that separates read-only from state-changing work is worth its cost
        let mut pipeline = facts(3, 0, 2, 0, 0, 1, false);
        pipeline.team_facts.as_mut().unwrap().est_flat_tokens = 780;
        pipeline.team_facts.as_mut().unwrap().est_team_tokens = 1440;
        assert!(split_p(pipeline) >= 0.6);
    }

    #[test]
    fn selects_relevant_tools() {
        let d = dec();
        let mut st = state("what files are in the folder");
        st.tools = vec![
            ToolBrief {
                name: "fs_list".into(),
                description: "list files in a directory folder".into(),
                category: "filesystem".into(),
                risk: Risk::Low,
                side_effects: false,
                keywords: vec![],
            },
            ToolBrief {
                name: "calc".into(),
                description: "evaluate arithmetic expression".into(),
                category: "math".into(),
                risk: Risk::Low,
                side_effects: false,
                keywords: vec![],
            },
        ];
        let r = d.answer(&tools_request(st));
        let (a, b) = (r["tool:fs_list"].clone(), r["tool:calc"].clone());
        match (a, b) {
            (Answer::Noul { noul: a }, Answer::Noul { noul: b }) => assert!(a > 0.5 && b < 0.3, "{a} {b}"),
            _ => panic!(),
        }
    }

    #[test]
    fn chatter_around_a_request_does_not_hide_the_tool_it_needs() {
        let d = dec();
        let brief = |name: &str, desc: &str, cat: &str, kw: &[&str]| ToolBrief {
            name: name.into(),
            description: desc.into(),
            category: cat.into(),
            risk: Risk::Low,
            side_effects: false,
            keywords: kw.iter().map(|k| k.to_string()).collect(),
        };
        let mut st = state("hi. i'm keyhan from new york. tell me weather temperature in new york.");
        st.tools = vec![
            brief("http_get", "HTTP GET any public URL; current weather: https://wttr.in/<city>?format=3", "web", &["fetch", "weather", "temperature"]),
            brief("fs_list", "list files in a directory folder", "filesystem", &[]),
            brief("calc", "evaluate arithmetic expression", "math", &[]),
        ];
        let r = d.answer(&tools_request(st));
        let p = |k: &str| match &r[k] {
            Answer::Noul { noul } => *noul,
            _ => panic!(),
        };
        assert!(p("tool:http_get") > 0.5, "{}", p("tool:http_get"));
        assert!(p("tool:fs_list") < 0.3 && p("tool:calc") < 0.3);
    }

    #[test]
    fn building_and_serving_things_needs_tools_but_asking_about_them_does_not() {
        let d = dec();
        let needs = |req: &str| match d.answer(&route_request(state(req)))["needs_tools"] {
            Answer::Noul { noul } => noul >= 0.5,
            _ => panic!(),
        };
        for req in [
            "create a landing page for my coffee shop",
            "build a REST API for todos",
            "write a python script that renames files",
            "persist it to file and serve it",
            "save that code to index.html",
        ] {
            assert!(needs(req), "{req}");
        }
        for req in ["what is a landing page?", "why do people prefer REST over SOAP", "hi"] {
            assert!(!needs(req), "{req}");
        }
    }
}
