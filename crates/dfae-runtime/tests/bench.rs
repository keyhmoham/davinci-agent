#![allow(clippy::field_reassign_with_default)]
//! Token benchmark. The same five tasks run twice through the whole engine, with a scripted "external planner" that
//! makes the *same* decisions in both runs (it answers in whatever format its prompt asks for):
//!
//! * baseline: JSON plans, a narrow per-task tool list, the external model writes the final answer;
//! * optimised: compact plan lines, the whole tool catalogue as a cached prefix, a plan-time reply template,
//!   local-first answers, a workspace snapshot.
//!
//! The external model is metered like a provider with prompt caching. The benchmark measures what the prompts, formats
//! and flow cost — not how smart a real model is — and it checks that outcomes (files, answers) are identical.

use dfae_core::*;
use dfae_decision::{Calibrator, Decider};
use dfae_extract::HybridExtractor;
use dfae_memory::InMemoryStore;
use dfae_models::{MeteredModel, ModelGateway};
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct P {
    id: &'static str,
    tool: Option<&'static str>,
    doc: &'static str,
    after: &'static [&'static str],
    expect: Option<&'static str>,
}

struct Scenario {
    goal: &'static str,
    files: &'static [(&'static str, &'static str)],
    plan: Vec<P>,
    report: Option<&'static str>,
    /// What the external model would write as the final answer when no report was planned.
    answer: &'static str,
    /// What the local model writes for a reasoning step, and for a tool's arguments.
    reasoning: &'static str,
    bind: serde_json::Value,
    outcome: fn(&std::path::Path),
}

/// Things earlier rounds created, removed before a scenario runs again.
const CLEAN: &[&str] = &["site", "backup", "ports.txt"];

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            goal: "Create the directory site, write a small landing page for a coffee shop into site/index.html, and list the site directory",
            files: &[],
            plan: vec![
                P { id: "s1", tool: Some("fs_mkdir"), doc: "create the directory site", after: &[], expect: Some("site exists") },
                P {
                    id: "s2",
                    tool: Some("fs_write"),
                    doc: "write site/index.html: a small landing page for a coffee shop",
                    after: &["s1"],
                    expect: Some("site/index.html exists"),
                },
                P { id: "s3", tool: Some("fs_list"), doc: "list the directory site", after: &["s2"], expect: None },
            ],
            report: Some("Created site/index.html (a coffee shop landing page). The site directory holds:\n{{s3}}"),
            answer: "Created site/index.html, a coffee shop landing page. The site directory holds: index.html",
            reasoning: "",
            bind: json!({"content": "<!doctype html><html><body><h1>Bean There</h1></body></html>"}),
            outcome: |ws| assert!(std::fs::read_to_string(ws.join("site/index.html")).unwrap().contains("Bean There")),
        },
        Scenario {
            goal: "Read config.toml and tell me which port the service listens on",
            files: &[("config.toml", "[server]\nhost = \"0.0.0.0\"\nport = 8080\n")],
            plan: vec![
                P { id: "s1", tool: Some("fs_read"), doc: "read config.toml", after: &[], expect: Some("shows the server port") },
                P { id: "s2", tool: None, doc: "say which port the service listens on", after: &["s1"], expect: None },
            ],
            report: None,
            answer: "The service listens on port 8080.",
            reasoning: "The service listens on port 8080.",
            bind: json!({}),
            outcome: |_| {},
        },
        Scenario {
            goal: "In config.toml replace \"port = 8080\" with \"port = 9090\"",
            files: &[],
            plan: vec![
                P { id: "s1", tool: Some("fs_read"), doc: "read config.toml", after: &[], expect: None },
                P {
                    id: "s2",
                    tool: Some("fs_edit"),
                    doc: "in config.toml replace \"port = 8080\" with \"port = 9090\"",
                    after: &["s1"],
                    expect: Some("config.toml contains port = 9090"),
                },
            ],
            report: Some("Changed the port to 9090 in config.toml."),
            answer: "Changed the port to 9090 in config.toml.",
            reasoning: "",
            bind: json!({"path": "config.toml", "old_string": "port = 8080", "new_string": "port = 9090"}),
            outcome: |ws| assert!(std::fs::read_to_string(ws.join("config.toml")).unwrap().contains("port = 9090")),
        },
        Scenario {
            goal: "Find which files mention TODO and say what the first one is about",
            files: &[("notes/a.md", "intro\nTODO: rotate the API keys\n"), ("notes/b.md", "nothing here\n")],
            plan: vec![
                P { id: "s1", tool: Some("fs_search"), doc: "search the workspace for TODO", after: &[], expect: None },
                P { id: "s2", tool: None, doc: "say which files mention TODO and what the first one is about", after: &["s1"], expect: None },
            ],
            report: None,
            answer: "notes/a.md mentions TODO: rotate the API keys.",
            reasoning: "notes/a.md mentions TODO: rotate the API keys.",
            bind: json!({"pattern": "TODO"}),
            outcome: |_| {},
        },
        Scenario {
            goal: "Copy config.toml to backup/config.toml and then show me the backup directory",
            files: &[],
            plan: vec![
                P { id: "s1", tool: Some("fs_copy"), doc: "copy config.toml to backup/config.toml", after: &[], expect: Some("backup/config.toml exists") },
                P { id: "s2", tool: Some("fs_list"), doc: "list the directory backup", after: &["s1"], expect: None },
            ],
            report: Some("Copied config.toml to backup/config.toml:\n{{s2}}"),
            answer: "Copied config.toml to backup/config.toml; backup holds config.toml.",
            reasoning: "",
            bind: json!({"from": "config.toml", "to": "backup/config.toml"}),
            outcome: |ws| assert!(ws.join("backup/config.toml").exists()),
        },
        Scenario {
            goal: "Read config.toml, work out which port the service listens on, and write only that port number into ports.txt",
            files: &[("config.toml", "[server]\nhost = \"0.0.0.0\"\nport = 8080\n")],
            plan: vec![
                P { id: "s1", tool: Some("fs_read"), doc: "read config.toml", after: &[], expect: Some("shows the server port") },
                P { id: "s2", tool: None, doc: "extract the port number from the content of config.toml", after: &["s1"], expect: None },
                P { id: "s3", tool: Some("fs_write"), doc: "write the port number to ports.txt", after: &["s2"], expect: Some("ports.txt exists") },
            ],
            report: Some("Wrote the port ({{s2}}) to ports.txt."),
            answer: "Wrote the port 8080 to ports.txt.",
            reasoning: "8080",
            bind: json!({"content": "{{s2}}"}),
            outcome: |ws| assert_eq!(std::fs::read_to_string(ws.join("ports.txt")).unwrap(), "8080"),
        },
    ]
}

fn render(sc: &Scenario, lines: bool, with_report: bool) -> String {
    if lines {
        let mut out = String::new();
        for p in &sc.plan {
            let after = if p.after.is_empty() { String::new() } else { format!(" after={}", p.after.join(",")) };
            out.push_str(&format!("{} {}{}: {}\n", p.id, p.tool.unwrap_or("-"), after, p.doc));
            if let Some(e) = p.expect {
                out.push_str(&format!("  expect: {e}\n"));
            }
        }
        if let (true, Some(r)) = (with_report, sc.report) {
            out.push_str(&format!("report: {r}\n"));
        }
        out
    } else {
        let steps: Vec<_> = sc
            .plan
            .iter()
            .map(|p| {
                let mut s = json!({"id": p.id, "do": p.doc, "tool": p.tool, "after": p.after});
                if let Some(e) = p.expect {
                    s["expect"] = json!(e);
                }
                s
            })
            .collect();
        json!({"steps": steps}).to_string()
    }
}

#[derive(Default, Debug)]
struct Totals {
    external: u64,
    effective: u64,
    cached: u64,
    calls: u32,
    by_purpose: std::collections::BTreeMap<String, u64>,
}

struct Arm {
    totals: Totals,
    answers: Vec<String>,
}

#[derive(Clone, Copy)]
struct Flags {
    lines: bool,
    prefix: bool,
    report: bool,
    local_answer: bool,
    workspace: bool,
}

impl Flags {
    const NONE: Flags = Flags { lines: false, prefix: false, report: false, local_answer: false, workspace: false };
    /// What ships by default (the reply template is opt-in).
    const ALL: Flags = Flags { lines: true, prefix: true, report: false, local_answer: true, workspace: true };
}

async fn run_arm(f: Flags, rounds: usize, provider_caches: bool) -> Arm {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    let eco = &mut cfg.economy;
    (eco.plan_lines, eco.plan_prefix, eco.plan_report, eco.answer_local_first, eco.workspace_snapshot) =
        (f.lines, f.prefix, f.report, f.local_answer, f.workspace);
    let current = Arc::new(AtomicUsize::new(0));
    let scs = Arc::new(scenarios());
    let (c1, s1) = (current.clone(), scs.clone());
    let strong = MeteredModel::new("strong", move |req| {
        let sc = &s1[c1.load(Ordering::SeqCst)];
        match req.purpose.as_str() {
            "plan" => Ok(render(sc, req.system.contains("plan lines"), req.system.contains("report:"))),
            "answer" => Ok(sc.answer.to_string()),
            other => Err(format!("unexpected external call: {other}")),
        }
    });
    let strong = if provider_caches { strong } else { strong.without_cache() };
    let (c2, s2) = (current.clone(), scs.clone());
    let slm = MeteredModel::new("slm", move |req| {
        let sc = &s2[c2.load(Ordering::SeqCst)];
        match req.purpose.as_str() {
            "bind" => Ok(sc.bind.to_string()),
            "step" | "step_expand" => Ok(sc.reasoning.to_string()),
            "answer_local" | "answer" => Ok(sc.answer.to_string()),
            other => Err(format!("unexpected local call: {other}")),
        }
    });
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(slm)), Some(Arc::new(strong))),
        tools: Arc::new(reg),
        policy,
    });
    let mut arm = Arm { totals: Totals::default(), answers: vec![] };
    for (i, sc) in scs.iter().enumerate().cycle().take(scs.len() * rounds) {
        for c in CLEAN {
            let _ = std::fs::remove_dir_all(ws.path().join(c));
            let _ = std::fs::remove_file(ws.path().join(c));
        }
        current.store(i, Ordering::SeqCst);
        for (path, text) in sc.files {
            let p = ws.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let id = engine.submit(TaskRequest { goal: sc.goal.into(), delegate: Some(false), mode: ModeRequest::Escalated, ..Default::default() }).await.unwrap();
        let t = engine.wait(&id, Duration::from_secs(30)).await.unwrap();
        assert_eq!(t.status, TaskStatus::Completed, "[{}] {:?} / {:?}", sc.goal, t.error, t.final_answer);
        (sc.outcome)(ws.path());
        let s = t.ledger.summary();
        arm.totals.external += s.external_tokens;
        arm.totals.effective += s.external_effective_tokens;
        arm.totals.cached += s.external_cached_tokens;
        arm.totals.calls += s.strong_calls;
        for (k, v) in s.external_by_purpose {
            *arm.totals.by_purpose.entry(k).or_default() += v;
        }
        arm.answers.push(t.final_answer.clone().unwrap_or_default());
    }
    arm
}

const MUST: [&str; 6] = ["site/index.html", "8080", "9090", "notes/a.md", "backup/config.toml", "8080"];

#[tokio::test]
async fn the_optimised_flow_costs_far_fewer_external_tokens_for_the_same_outcomes() {
    let rounds = 4;
    let tasks = (scenarios().len() * rounds) as f64;
    let arms: Vec<(&str, Flags)> = vec![
        ("baseline (json, narrow, answer call)", Flags::NONE),
        ("+ compact plan lines", Flags { lines: true, ..Flags::NONE }),
        ("+ plan-time reply template", Flags { lines: true, report: true, ..Flags::NONE }),
        ("+ local-first answer", Flags { local_answer: true, ..Flags::NONE }),
        ("+ workspace snapshot", Flags { workspace: true, ..Flags::NONE }),
        ("+ cached catalogue prefix", Flags { prefix: true, ..Flags::NONE }),
        ("all", Flags::ALL),
        ("all, on a provider without caching", Flags::ALL),
    ];
    let mut results = Vec::new();
    eprintln!("\n{:<40} {:>5} {:>8} {:>8} {:>10} {:>9}", "arm", "calls", "tokens", "cached", "effective", "eff/task");
    for (name, f) in &arms {
        let arm = run_arm(*f, rounds, !name.contains("without caching")).await;
        let t = &arm.totals;
        eprintln!("{:<40} {:>5} {:>8} {:>8} {:>10} {:>9.0}   {:?}", name, t.calls, t.external, t.cached, t.effective, t.effective as f64 / tasks, t.by_purpose);
        results.push((*name, arm));
    }
    let (base, all) = (&results[0].1, &results[results.len() - 2].1);
    let nocache = &results.last().unwrap().1;
    let cut = |a: u64, b: u64| 100.0 * (a as f64 - b as f64) / a as f64;
    eprintln!(
        "\nall vs baseline: external calls -{:.0}%, raw tokens -{:.0}%, effective tokens -{:.0}%",
        cut(base.totals.calls as u64, all.totals.calls as u64),
        cut(base.totals.external, all.totals.external),
        cut(base.totals.effective, all.totals.effective)
    );
    // every arm produced answers that carry the facts of the task (same outcomes), not only the cheapest one
    for (name, arm) in &results {
        for (i, a) in arm.answers.iter().enumerate() {
            assert!(a.contains(MUST[i % 6]), "[{name}] answer {i} lacks {}: {a}", MUST[i % 6]);
        }
    }
    eprintln!(
        "no-cache provider vs baseline: raw tokens -{:.0}%, effective tokens -{:.0}% (the prefix is switched off by itself)",
        cut(base.totals.external, nocache.totals.external),
        cut(base.totals.effective, nocache.totals.effective)
    );
    assert!(nocache.totals.external < base.totals.external, "without caching the optimised flow must still not cost more");
    assert!(all.totals.calls < base.totals.calls, "fewer external calls");
    assert!(all.totals.effective < base.totals.effective, "fewer effective external tokens");
}
