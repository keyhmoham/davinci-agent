#![allow(clippy::field_reassign_with_default)]
//! The worker system against realistic plans, for real. Ignored by default: it calls the hosted worker model (and, with
//! `LIVE_PLANNER=1`, the hosted planner model too), so it needs the same configuration as the server:
//!
//! ```bash
//! PLANNER_API_KEY=… WORKER_API_KEY=… \
//!   cargo test -p dfae-runtime --test live_worker -- --ignored --nocapture --test-threads=1
//! ```
//!
//! By default the planner is *scripted* with the plans a good planner writes (typed JSON), so what is under test is whether the
//! worker system — rules, extractor, decision model, worker model — can carry them out. `LIVE_PLANNER=1` uses the real planner
//! model for the plans as well (the whole system end to end). `LIVE_TRACE=1` prints every model call (what it was asked, how long
//! it took, what it said). `DECISION_BASE_URL` / `DECISION_MODEL` add the open decision model (otherwise the built-in rules decide).

use dfae_core::*;
use dfae_decision::{Calibrator, Decider, OpenModelProvider};
use dfae_extract::HybridExtractor;
use dfae_memory::InMemoryStore;
use dfae_models::{ModelGateway, ScriptedModel, build_model};
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use std::sync::Arc;
use std::time::Duration;

type StepSpec = (&'static str, Option<&'static str>, &'static str, &'static [&'static str], Option<&'static str>);

/// A plan in the typed shape the planner writes.
fn plan_json(steps: &[StepSpec]) -> String {
    let steps: Vec<_> = steps
        .iter()
        .map(|(id, tool, doc, after, expect)| serde_json::json!({"id": id, "tool": tool, "do": doc, "after": after, "expect": expect, "planner": false}))
        .collect();
    serde_json::json!({"steps": steps}).to_string()
}

struct Case {
    name: &'static str,
    goal: &'static str,
    files: &'static [(&'static str, &'static str)],
    /// What a good planner writes for the goal.
    plan: &'static [StepSpec],
    check: fn(&std::path::Path, &TaskRecord) -> Result<(), String>,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "landing page",
            goal: "create a landing page about earth in the site directory",
            files: &[],
            plan: &[
                ("s1", Some("fs_mkdir"), "create the directory site", &[], Some("site exists")),
                (
                    "s2",
                    Some("fs_write"),
                    "write site/index.html, a complete professional landing page about planet Earth: a hero section with a title and tagline, three feature cards (oceans, atmosphere, life) and a footer, with modern embedded CSS",
                    &["s1"],
                    Some("site/index.html exists"),
                ),
                ("s3", Some("fs_list"), "list the directory site", &["s2"], None),
            ],
            check: |ws, _| {
                let html = std::fs::read_to_string(ws.join("site/index.html")).map_err(|e| format!("no page: {e}"))?;
                let l = html.to_lowercase();
                if !l.contains("</html>") || !l.contains("earth") || html.len() < 600 {
                    return Err(format!("the page is not a real page ({} bytes): {}", html.len(), html.chars().take(200).collect::<String>()));
                }
                Ok(())
            },
        },
        Case {
            name: "edit a config value",
            goal: "In config.toml change the port from 8080 to 9090",
            files: &[("config.toml", "[server]\nhost = \"0.0.0.0\"\nport = 8080\n")],
            plan: &[
                ("s1", Some("fs_read"), "read config.toml", &[], None),
                ("s2", Some("fs_edit"), "in config.toml replace \"port = 8080\" with \"port = 9090\"", &["s1"], Some("config.toml now contains port = 9090")),
            ],
            check: |ws, _| {
                let t = std::fs::read_to_string(ws.join("config.toml")).map_err(|e| e.to_string())?;
                if t.contains("port = 9090") && !t.contains("8080") { Ok(()) } else { Err(format!("config.toml is now: {t}")) }
            },
        },
        Case {
            name: "read and summarise",
            goal: "Read changelog.md and tell me what changed in version 2",
            files: &[(
                "changelog.md",
                "# 3.0\n- dropped python 2\n# 2.0\n- added the export command\n- fixed a crash on empty input\n# 1.0\n- first release\n",
            )],
            plan: &[
                ("s1", Some("fs_read"), "read changelog.md", &[], None),
                ("s2", None, "say what changed in version 2.0, using only what the file says", &["s1"], None),
            ],
            check: |_, t| {
                let a = t.final_answer.clone().unwrap_or_default().to_lowercase();
                if a.contains("export") && !a.contains("python 2") { Ok(()) } else { Err(format!("answer: {a}")) }
            },
        },
        Case {
            name: "search and explain",
            goal: "Which files mention TODO, and what is the first one about?",
            files: &[("notes/a.md", "intro\nTODO: rotate the API keys before the audit\n"), ("notes/b.md", "nothing to do here\n")],
            plan: &[
                ("s1", Some("fs_search"), "search the workspace for TODO with 2 lines of context", &[], None),
                ("s2", None, "say which files mention TODO and what the first one is about", &["s1"], None),
            ],
            check: |_, t| {
                let a = t.final_answer.clone().unwrap_or_default().to_lowercase();
                if a.contains("a.md") && a.contains("key") { Ok(()) } else { Err(format!("answer: {a}")) }
            },
        },
        Case {
            name: "two files from one request",
            goal: "Create README.md with a short project description for a tool called Brewster that tracks coffee orders, and a .gitignore for a Python project",
            files: &[],
            plan: &[
                (
                    "s1",
                    Some("fs_write"),
                    "write README.md, a short README for Brewster, a tool that tracks coffee orders: a title, one paragraph, and a usage section",
                    &[],
                    None,
                ),
                ("s2", Some("fs_write"), "write .gitignore for a Python project (pycache, virtualenvs, build output, editor folders)", &[], None),
            ],
            check: |ws, _| {
                let r = std::fs::read_to_string(ws.join("README.md")).map_err(|e| format!("README.md: {e}"))?;
                let g = std::fs::read_to_string(ws.join(".gitignore")).map_err(|e| format!(".gitignore: {e}"))?;
                if r.contains("Brewster") && g.contains("__pycache__") { Ok(()) } else { Err(format!("README: {r}\n.gitignore: {g}")) }
            },
        },
    ]
}

/// `LIVE_TRACE=1` prints every call to a model: what it was asked, how long it took, what it said.
struct Traced(&'static str, Arc<dyn LanguageModel>);

#[async_trait::async_trait]
impl LanguageModel for Traced {
    fn name(&self) -> &str {
        self.1.name()
    }
    fn caches_prompts(&self) -> bool {
        self.1.caches_prompts()
    }
    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, ModelError> {
        let (purpose, ask) = (req.purpose.clone(), req.messages.last().map(|m| m.content.clone()).unwrap_or_default());
        let started = std::time::Instant::now();
        let r = self.1.generate(req).await;
        if std::env::var("LIVE_TRACE").is_ok() {
            let tail = |s: &str, n: usize| -> String { s.chars().take(n).collect::<String>().replace('\n', " ⏎ ") };
            match &r {
                Ok(g) => eprintln!(
                    "    [{} {purpose} {:.1}s in={} out={}] ASKED: {} …\n        SAID: {}",
                    self.0,
                    started.elapsed().as_secs_f32(),
                    g.usage.prompt_tokens,
                    g.usage.completion_tokens,
                    tail(&ask, 260),
                    tail(&g.text, 260)
                ),
                Err(e) => eprintln!("    [{} {purpose} {:.1}s] ERROR {e}", self.0, started.elapsed().as_secs_f32()),
            }
        }
        r
    }
}

fn engine(ws: &std::path::Path, plan: &str) -> Engine {
    let mut cfg = Config::from_env();
    cfg.workspace = ws.to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    let worker: Arc<dyn LanguageModel> =
        Arc::new(Traced("worker", build_model(&cfg.worker).expect("the worker model is not configured (WORKER_API_KEY / PLANNER_API_KEY)")));
    let planner: Arc<dyn LanguageModel> = if std::env::var("LIVE_PLANNER").is_ok() {
        Arc::new(Traced("planner", build_model(&cfg.planner).expect("the planner model is not configured (PLANNER_API_KEY)")))
    } else {
        Arc::new(Traced("planner", Arc::new(ScriptedModel::new("scripted-planner", vec![plan]).caching())))
    };
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let provider = std::env::var("DECISION_BASE_URL").ok().map(|u| {
        let mut d = cfg.decision.clone();
        d.base_url = Some(u);
        if let Ok(m) = std::env::var("DECISION_MODEL") {
            d.model = m;
        }
        Arc::new(OpenModelProvider::new(&d).expect("decision model")) as Arc<dyn DecisionProvider>
    });
    Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(provider, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        store,
        models: ModelGateway::new(Some(worker), Some(planner)),
        tools: Arc::new(reg),
        policy,
        config: cfg,
    })
}

#[tokio::test]
#[ignore = "calls the hosted models: see the module docs"]
async fn the_worker_system_carries_out_the_plans_a_good_planner_writes() {
    let mut failures = Vec::new();
    // `LIVE_CASES=landing,config` runs only the cases whose name contains one of the words
    let only: Vec<String> = std::env::var("LIVE_CASES").map(|v| v.split(',').map(|w| w.trim().to_lowercase()).collect()).unwrap_or_default();
    for c in cases().into_iter().filter(|c| only.is_empty() || only.iter().any(|w| c.name.contains(w.as_str()))) {
        let ws = tempfile::tempdir().unwrap();
        for (p, t) in c.files {
            let f = ws.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        }
        let e = engine(ws.path(), &plan_json(c.plan));
        let id = e.submit(TaskRequest { goal: c.goal.into(), mode: ModeRequest::Escalated, delegate: Some(false), ..Default::default() }).await.unwrap();
        let t = e.wait(&id, Duration::from_secs(900)).await.unwrap();
        eprintln!("\n=== {} — {:?} in {}s", c.name, t.status, (t.updated_at - t.created_at).num_seconds());
        if let Some(p) = &t.plan {
            for s in &p.steps {
                eprintln!(
                    "  {} {:<10} {:<9} how={:<14} {}{}",
                    s.id,
                    s.tool.as_deref().unwrap_or("reasoning"),
                    format!("{:?}", s.status),
                    s.how.as_deref().unwrap_or("-"),
                    s.error.as_deref().map(|e| format!("ERROR {e} ")).unwrap_or_default(),
                    tokens::truncate_tokens(s.summary.as_deref().unwrap_or(""), 30).replace('\n', " ")
                );
            }
        }
        let s = t.ledger.summary();
        eprintln!(
            "  planner {} tokens ({} calls), worker {} tokens ({} calls: {:?}), answer: {}",
            s.planner_tokens,
            s.planner_calls,
            s.worker_tokens,
            s.worker_calls,
            s.worker_by_purpose,
            tokens::truncate_tokens(t.final_answer.as_deref().unwrap_or(""), 60).replace('\n', " ")
        );
        eprintln!(
            "  cost ${:.6} (planner ${:.6}, worker ${:.6}; unpriced calls {}), reasoning planner {} / worker {}, cache planner read {} write {} / worker read {} write {}",
            s.cost_usd,
            s.planner_cost_usd,
            s.worker_cost_usd,
            s.unpriced_calls,
            s.planner_reasoning_tokens,
            s.worker_reasoning_tokens,
            s.planner_cached_tokens,
            s.planner_cache_write_tokens,
            s.worker_cached_tokens,
            s.worker_cache_write_tokens
        );
        let verdict = if t.status != TaskStatus::Completed { Err(format!("{:?}: {:?}", t.status, t.error)) } else { (c.check)(ws.path(), &t) };
        if let Err(why) = verdict {
            eprintln!("  FAILED: {why}");
            failures.push(format!("{}: {why}", c.name));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), cases().len(), failures.join("\n"));
}
