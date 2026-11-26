#![allow(clippy::field_reassign_with_default)]
//! The self-hosted model against realistic plans, for real. Ignored by default: it needs a running OpenAI-compatible
//! server (Ollama in the compose stack) — the external planner is scripted with the plans a good LLM writes in the
//! plan-line format, so what is under test is whether the *local* system (rules, extractor, SLM) can carry them out.
//!
//! ```bash
//! docker compose up -d ollama ollama-pull           # port 11434 is published on loopback
//! SLM_BASE_URL=http://localhost:11434/v1 SLM_MODEL=qwen2.5:3b \
//!   cargo test -p dfae-runtime --test live_slm -- --ignored --nocapture --test-threads=1
//! ```
//! `LIVE_TRACE=1` also prints every call to the SLM (what it was asked, how long it took, what it said).
//! Optional: `DECISION_BASE_URL` / `DECISION_MODEL` to use the open decision model as well (otherwise the built-in rules decide).

use dfae_core::*;
use dfae_decision::{Calibrator, Decider, OpenModelProvider};
use dfae_extract::HybridExtractor;
use dfae_memory::InMemoryStore;
use dfae_models::{ModelGateway, OpenAiCompat, ScriptedModel};
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use std::sync::Arc;
use std::time::Duration;

struct Case {
    name: &'static str,
    goal: &'static str,
    files: &'static [(&'static str, &'static str)],
    /// What a good external planner writes for the goal (plan lines).
    plan: &'static str,
    check: fn(&std::path::Path, &TaskRecord) -> Result<(), String>,
}

const PAGE_PLAN: &str = "s1 fs_mkdir: create the directory site\n\
s2 fs_write after=s1: write site/index.html, a complete professional landing page about planet Earth: a hero section with a title and tagline, three feature cards (oceans, atmosphere, life) and a footer, with modern embedded CSS\n  expect: site/index.html exists\n\
s3 fs_list after=s2: list the directory site";

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "landing page",
            goal: "create a landing page about earth in the site directory",
            files: &[],
            plan: PAGE_PLAN,
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
            plan: "s1 fs_read: read config.toml\ns2 fs_edit after=s1: in config.toml replace \"port = 8080\" with \"port = 9090\"\n  expect: config.toml now contains port = 9090",
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
            plan: "s1 fs_read: read changelog.md\ns2 - after=s1: say what changed in version 2.0, using only what the file says",
            check: |_, t| {
                let a = t.final_answer.clone().unwrap_or_default().to_lowercase();
                if a.contains("export") && !a.contains("python 2") { Ok(()) } else { Err(format!("answer: {a}")) }
            },
        },
        Case {
            name: "search and explain",
            goal: "Which files mention TODO, and what is the first one about?",
            files: &[("notes/a.md", "intro\nTODO: rotate the API keys before the audit\n"), ("notes/b.md", "nothing to do here\n")],
            plan: "s1 fs_search: search the workspace for TODO with 2 lines of context\ns2 - after=s1: say which files mention TODO and what the first one is about",
            check: |_, t| {
                let a = t.final_answer.clone().unwrap_or_default().to_lowercase();
                if a.contains("a.md") && a.contains("key") { Ok(()) } else { Err(format!("answer: {a}")) }
            },
        },
        Case {
            name: "two files from one request",
            goal: "Create README.md with a short project description for a tool called Brewster that tracks coffee orders, and a .gitignore for a Python project",
            files: &[],
            plan: "s1 fs_write: write README.md, a short README for Brewster, a tool that tracks coffee orders: a title, one paragraph, and a usage section\ns2 fs_write: write .gitignore for a Python project (pycache, virtualenvs, build output, editor folders)",
            check: |ws, _| {
                let r = std::fs::read_to_string(ws.join("README.md")).map_err(|e| format!("README.md: {e}"))?;
                let g = std::fs::read_to_string(ws.join(".gitignore")).map_err(|e| format!(".gitignore: {e}"))?;
                if r.contains("Brewster") && g.contains("__pycache__") { Ok(()) } else { Err(format!("README: {r}\n.gitignore: {g}")) }
            },
        },
    ]
}

/// `LIVE_TRACE=1` prints every call to the model: what it was asked, how long it took, what it said.
struct Traced(OpenAiCompat);

#[async_trait::async_trait]
impl LanguageModel for Traced {
    fn name(&self) -> &str {
        self.0.name()
    }
    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, ModelError> {
        let (purpose, ask) = (req.purpose.clone(), req.messages.last().map(|m| m.content.clone()).unwrap_or_default());
        let started = std::time::Instant::now();
        let r = self.0.generate(req).await;
        if std::env::var("LIVE_TRACE").is_ok() {
            let tail = |s: &str, n: usize| -> String { s.chars().take(n).collect::<String>().replace('\n', " ⏎ ") };
            match &r {
                Ok(g) => eprintln!(
                    "    [slm {purpose} {:.1}s in={} out={}] ASKED: {} …\n        SAID: {}",
                    started.elapsed().as_secs_f32(),
                    g.usage.prompt_tokens,
                    g.usage.completion_tokens,
                    tail(&ask, 260),
                    tail(&g.text, 260)
                ),
                Err(e) => eprintln!("    [slm {purpose} {:.1}s] ERROR {e}", started.elapsed().as_secs_f32()),
            }
        }
        r
    }
}

fn engine(ws: &std::path::Path, plan: &str) -> (Engine, ScriptedModel) {
    let mut cfg = Config::default();
    cfg.workspace = ws.to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    cfg.slm.provider = "openai".into();
    cfg.slm.base_url = std::env::var("SLM_BASE_URL").expect("SLM_BASE_URL");
    cfg.slm.model = std::env::var("SLM_MODEL").unwrap_or_else(|_| "qwen2.5:3b".into());
    let slm = Traced(OpenAiCompat::new(&cfg.slm.base_url, &cfg.slm.model, None, Duration::from_secs(600)));
    let strong = ScriptedModel::new("scripted-planner", vec![plan]).caching();
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
    let e = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(provider, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        store,
        models: ModelGateway::new(Some(Arc::new(slm)), Some(Arc::new(strong.clone()))),
        tools: Arc::new(reg),
        policy,
        config: cfg,
    });
    (e, strong)
}

#[tokio::test]
#[ignore = "needs a running model server: see the module docs"]
async fn the_local_system_carries_out_the_plans_a_good_llm_writes() {
    let mut failures = Vec::new();
    for c in cases() {
        let ws = tempfile::tempdir().unwrap();
        for (p, t) in c.files {
            let f = ws.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        }
        let (e, _) = engine(ws.path(), c.plan);
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
            "  slm calls {} ({} tokens), external {} tokens, answer: {}",
            s.slm_calls,
            s.slm_tokens,
            s.external_tokens,
            tokens::truncate_tokens(t.final_answer.as_deref().unwrap_or(""), 60).replace('\n', " ")
        );
        let verdict = if t.status != TaskStatus::Completed { Err(format!("{:?}: {:?}", t.status, t.error)) } else { (c.check)(ws.path(), &t) };
        if let Err(why) = verdict {
            eprintln!("  FAILED: {why}");
            failures.push(format!("{}: {why}", c.name));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), cases().len(), failures.join("\n"));
}
