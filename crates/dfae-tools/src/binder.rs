//! Deterministic argument binding — the engine behind Direct mode.
//!
//! Each tool declares regex patterns with named groups. If every clause of a
//! request binds to exactly one tool with all required arguments present, the
//! whole task can run without any generative model. Anything that smells like
//! reasoning ("explain", "why", long multi-part requests) is refused so the
//! SLM gets it instead.

use dfae_core::*;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

#[derive(Debug, Clone)]
pub struct DirectCall {
    pub tool: String,
    pub args: Value,
    pub clause: String,
    /// Must run after the previous call ("then", ";").
    pub after_prev: bool,
    pub score: f32,
}

static CACHE: LazyLock<Mutex<HashMap<String, Option<Arc<Regex>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn regex(p: &str) -> Option<Arc<Regex>> {
    let mut c = CACHE.lock();
    c.entry(p.to_string()).or_insert_with(|| Regex::new(p).ok().map(Arc::new)).clone()
}

static SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s*,?\s*(\band\s+then\b|\bthen\b|\bafter\s+that\b|\bafterwards\b|;|\n|\band\b(?:\s+(?:then\s+)?)(?:list|show|read|cat|run|fetch|calculate|compute|search|find|get|check|print|display|remember|write|save|open|what|how)\b)",
    )
    .unwrap()
});

static REASONING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(explain|why|summari[sz]e|analy[sz]e|compare|describe|recommend|fix|improve|review|translate|rewrite|interpret|decide|should|suggest|investigate|diagnose|debug|optimi[sz]e|plan|tell|which|identify|extract|count|determine|whether|figure|find out|how many|what's wrong)\b").unwrap()
});

fn split_clauses(req: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut last = 0;
    let mut after_prev = false;
    for m in SPLIT.find_iter(req) {
        let piece = req[last..m.start()].trim();
        if !piece.is_empty() {
            out.push((piece.to_string(), after_prev));
        }
        let sep = m.as_str().to_lowercase();
        after_prev = sep.contains("then") || sep.contains("after") || sep.contains(';') || sep.contains('\n');
        // a trailing "and <verb>" separator swallowed the verb: put it back
        let verb = sep.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        let keep = verb.strip_prefix("and").map(|v| v.trim()).filter(|v| !v.is_empty() && !v.starts_with("then"));
        last = match keep {
            Some(v) if !v.is_empty() => m.end() - v.len(),
            _ => m.end(),
        };
    }
    let tail = req[last..].trim();
    if !tail.is_empty() {
        out.push((tail.to_string(), after_prev));
    }
    out
}

fn clean(v: &str) -> String {
    let t = v.trim();
    let c = t.trim_start_matches(['"', '\'', '`']).trim_end_matches([',', '.', ';', ':', '!', '?', '"', '\'', '`']).trim();
    if c.is_empty() { t.to_string() } else { c.to_string() }
}

fn coerce(p: &ParamSpec, raw: &str) -> Option<Value> {
    match p.ty {
        ParamType::String => Some(json!(raw)),
        ParamType::Integer => raw.parse::<i64>().ok().map(|n| json!(n)),
        ParamType::Number => raw.parse::<f64>().ok().map(|n| json!(n)),
        ParamType::Boolean => match raw.to_lowercase().as_str() {
            "true" | "yes" => Some(json!(true)),
            "false" | "no" => Some(json!(false)),
            _ => None,
        },
    }
}

/// Try to bind one clause to a tool.
fn bind_clause(metas: &[ToolMetadata], clause: &str, entities: &[Entity]) -> Option<(String, Value, f32)> {
    let mut best: Option<(String, Value, f32)> = None;
    for m in metas {
        for (idx, pat) in m.direct_patterns.iter().enumerate() {
            let Some(re) = regex(pat) else { continue };
            let Some(caps) = re.captures(clause) else { continue };
            let mut args = Map::new();
            let (mut explicit, mut defaulted, mut ok) = (0u32, 0u32, true);
            for p in &m.params {
                let from_group = caps.name(&p.name).map(|g| clean(g.as_str())).filter(|s| !s.is_empty());
                if let Some(raw) = from_group {
                    match coerce(p, &raw) {
                        Some(v) => {
                            args.insert(p.name.clone(), v);
                            explicit += 1;
                            continue;
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                let from_entity = p
                    .entity_hints
                    .iter()
                    .find_map(|h| entities.iter().find(|e| &e.label == h && clause.contains(&e.text)))
                    .and_then(|e| coerce(p, &e.text));
                if let Some(v) = from_entity {
                    args.insert(p.name.clone(), v);
                    explicit += 1;
                } else if let Some(d) = &p.default {
                    args.insert(p.name.clone(), d.clone());
                    defaulted += 1;
                } else if p.required {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            let score = 1.0 + 0.5 * explicit as f32 - 0.3 * defaulted as f32 - 0.05 * idx as f32;
            if best.as_ref().map(|b| score > b.2).unwrap_or(true) {
                best = Some((m.name.clone(), Value::Object(args), score));
            }
            break; // first matching pattern of a tool wins
        }
    }
    best
}

/// Bind a whole request to tool calls without any model. Returns `None` if any
/// clause cannot be bound or the request looks like it needs reasoning.
pub fn bind_direct(metas: &[ToolMetadata], request: &str, entities: &[Entity]) -> Option<Vec<DirectCall>> {
    let req = request.trim();
    if req.is_empty() || req.split_whitespace().count() > 40 {
        return None;
    }
    let clauses = split_clauses(req);
    if clauses.is_empty() || clauses.len() > 4 {
        return None;
    }
    let mut calls = Vec::new();
    for (clause, after_prev) in clauses {
        if REASONING.is_match(&clause) {
            return None;
        }
        let (tool, args, score) = bind_clause(metas, &clause, entities)?;
        calls.push(DirectCall { tool, args, clause, after_prev, score });
    }
    Some(calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{register_native, PolicyConfig, PolicyEngine, ToolRegistry};
    use dfae_memory::InMemoryStore;

    fn metas() -> Vec<ToolMetadata> {
        let d = std::env::temp_dir();
        let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), d));
        let mut r = ToolRegistry::new();
        register_native(&mut r, policy, Arc::new(InMemoryStore::new()));
        r.metas()
    }

    fn bind(req: &str) -> Option<Vec<DirectCall>> {
        bind_direct(&metas(), req, &[])
    }

    fn one(req: &str) -> (String, Value) {
        let c = bind(req).unwrap_or_else(|| panic!("no binding for {req:?}"));
        assert_eq!(c.len(), 1, "{c:?}");
        (c[0].tool.clone(), c[0].args.clone())
    }

    #[test]
    fn binds_single_tool_requests() {
        assert_eq!(one("what is 12 * (3 + 4)?"), ("calc".into(), json!({"expression": "12 * (3 + 4)"})));
        assert_eq!(one("calculate 2^10"), ("calc".into(), json!({"expression": "2^10"})));
        assert_eq!(one("17 * 23").0, "calc");
        assert_eq!(one("what time is it?").0, "time_now");
        assert_eq!(one("list the files in /workspace/notes").1, json!({"path": "/workspace/notes"}));
        assert_eq!(one("list files").1, json!({"path": "."}));
        assert_eq!(one("read notes/todo.md"), ("fs_read".into(), json!({"path": "notes/todo.md"})));
        assert_eq!(one("show the contents of config.toml").0, "fs_read");
        assert_eq!(one("fetch https://example.com/docs").1, json!({"url": "https://example.com/docs"}));
        assert_eq!(one("run `df -h`").1, json!({"command": "df -h"}));
        assert_eq!(one("show system status").0, "system_info");
        assert_eq!(one("search for 'TODO' in ./src").1, json!({"pattern": "TODO", "path": "./src"}));
        assert_eq!(one("remember that the staging db is db-staging").0, "memory_store");
        assert_eq!(one("what do you remember about the staging db?").0, "memory_search");
        assert_eq!(one("write 'hello' to out.txt").1, json!({"content": "hello", "path": "out.txt"}));
    }

    #[test]
    fn splits_compound_requests() {
        let c = bind("what time is it and list the files in /workspace").unwrap();
        assert_eq!(c.iter().map(|c| c.tool.as_str()).collect::<Vec<_>>(), vec!["time_now", "fs_list"]);
        assert!(!c[1].after_prev);
        let c = bind("calculate 6*7 then read notes.txt").unwrap();
        assert_eq!(c.len(), 2);
        assert!(c[1].after_prev);
    }

    #[test]
    fn refuses_reasoning_and_unbindable_requests() {
        assert!(bind("read report.txt and explain why the deployment failed").is_none());
        assert!(bind("Find the latest PostgreSQL security vulnerabilities, check whether my server is affected, and patch it if necessary").is_none());
        assert!(bind("tell me a joke").is_none());
        assert!(bind("what is the meaning of life").is_none());
        assert!(bind("read the notes").is_none());
    }
}
