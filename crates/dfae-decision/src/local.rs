//! Deterministic decision engine answering the same typed questions as Jev.
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
    ("research", &["research", "search", "latest", "news", "web", "internet", "lookup", "documentation", "vulnerability", "cve", "article", "source", "wiki", "summarize", "papers"]),
    ("coding", &["code", "function", "script", "program", "implement", "bug", "compile", "refactor", "test", "rust", "python", "javascript", "git", "commit", "debug", "class", "module"]),
    ("system_diagnostics", &["system", "disk", "memory", "cpu", "load", "uptime", "process", "diagnostic", "health", "server", "log", "network", "port", "service", "usage", "resource", "time", "clock", "date", "hostname"]),
    ("file_operations", &["file", "directory", "folder", "path", "read", "list", "cat", "ls", "write", "save", "grep", "open", "workspace", "content", "contents", "text"]),
    ("calculation", &["calculate", "compute", "sum", "multiply", "divide", "plus", "minus", "times", "percent", "math", "evaluate", "equation", "square", "root", "product"]),
    ("data_analysis", &["analyze", "analyse", "statistic", "csv", "dataset", "trend", "correlation", "chart", "aggregate", "metric", "average", "compare"]),
    ("maintenance", &["patch", "deploy", "restart", "install", "upgrade", "update", "fix", "migrate", "rollback", "configure", "delete", "remove", "reboot"]),
    ("memory", &["remember", "recall", "memory", "forget", "store", "note", "know", "learned"]),
    ("conversation", &["hello", "hi", "thanks", "thank", "explain", "tell", "joke", "opinion", "chat", "meaning"]),
];

const HIGH_RISK: &[&str] = &["delete", "remove", "drop", "rm", "format", "shutdown", "truncate", "overwrite", "kill", "reboot", "destroy", "wipe", "erase", "purge"];
const MODERATE_RISK: &[&str] = &["write", "create", "save", "deploy", "patch", "install", "restart", "update", "edit", "modify", "upgrade", "migrate", "remember", "append", "rename", "move", "chmod"];
const REASONING_VERBS: &[&str] = &["analyze", "analyse", "investigate", "explain", "why", "compare", "design", "implement", "write", "summarize", "diagnose", "debug", "refactor", "evaluate", "recommend", "decide", "plan", "review"];

static ARITH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(\.\d+)?\s*[-+*/^%x×÷]\s*\d+").unwrap());
static CONJ: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(and then|then|after that|afterwards|and|also|but|unless|if|before|after|while|otherwise)\b|[;,]").unwrap());

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
        if ARITH.is_match(&s.request) {
            if let Some(e) = scores.iter_mut().find(|(t, _)| t == "calculation") {
                e.1 += 2.0;
            }
        }
        for e in &s.entities {
            let boost = match e.label.as_str() {
                "file path" | "path" | "directory" => Some("file_operations"),
                "url" => Some("research"),
                "server" | "database" => Some("system_diagnostics"),
                "repository" => Some("coding"),
                _ => None,
            };
            if let Some(b) = boost {
                if let Some(x) = scores.iter_mut().find(|(t, _)| t == b) {
                    x.1 += 0.75;
                }
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
        let docs: Vec<String> = s.tools.iter().map(|t| format!("{} {} {} {}", t.name.replace('_', " "), t.description, t.category, t.keywords.join(" "))).collect();
        let bm = bm25_scores(&s.request, &docs);
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
                let tool_type = matches!(tt.as_str(), "file_operations" | "system_diagnostics" | "calculation" | "maintenance" | "memory");
                let needs = if tool_type { best_tool.max(0.7) } else { best_tool * 0.9 };
                out.insert("needs_tools".into(), Answer::noul(needs));
                let (r, rc) = self.risk(s);
                let key = match r {
                    Risk::Low => "low",
                    Risk::Moderate => "moderate",
                    Risk::High => "high",
                };
                out.insert("risk".into(), Answer::choice(key, rc, &[(key, rc)]));
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
                } else if f.complexity >= 0.85 && f.strong_available {
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
                } else if f.strong_available {
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

    #[test]
    fn selects_relevant_tools() {
        let d = dec();
        let mut st = state("what files are in the folder");
        st.tools = vec![
            ToolBrief { name: "fs_list".into(), description: "list files in a directory folder".into(), category: "filesystem".into(), risk: Risk::Low, side_effects: false, keywords: vec![] },
            ToolBrief { name: "calc".into(), description: "evaluate arithmetic expression".into(), category: "math".into(), risk: Risk::Low, side_effects: false, keywords: vec![] },
        ];
        let r = d.answer(&tools_request(st));
        let (a, b) = (r["tool:fs_list"].clone(), r["tool:calc"].clone());
        match (a, b) {
            (Answer::Noul { noul: a }, Answer::Noul { noul: b }) => assert!(a > 0.5 && b < 0.3, "{a} {b}"),
            _ => panic!(),
        }
    }
}
