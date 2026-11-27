//! Custom subagents (plan item A2): profiles an agent or a person defines — a name, what it is for, how it works (its skill, as
//! instructions) and which tools it may use — and that plans use by name (`"agent": "reviewer"`) or inline, once or as several
//! parallel instances (`"each": [...]`).
//!
//! What the literature says, and what follows from it here: parallel subagents pay off for breadth (independent parts) and cost a lot
//! of tokens (Anthropic's research system: ~15× a chat); agents that each make decisions on shared state conflict (Cognition). So the
//! planner decides when to delegate; instances of one step get only their part; readers run in parallel and writers take the single
//! operator slot; an agent cannot spawn agents. A saved profile is an instruction that outlives the task, so a task that has read
//! outside content cannot define or change one.

use crate::engine::{Engine, Inner};
use dfae_core::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

const KV_KEY: &str = "agents";
const MAX_PROFILES: usize = 50;

#[derive(Default)]
pub(crate) struct ProfileBook {
    map: Mutex<BTreeMap<String, AgentProfile>>,
    loaded: tokio::sync::OnceCell<()>,
}

impl ProfileBook {
    /// What is in memory now (read from the store at startup: `Engine::restore`, or at the first use).
    pub fn snapshot(&self) -> Vec<AgentProfile> {
        self.map.lock().values().cloned().collect()
    }
}

impl Inner {
    async fn profiles(&self) -> &ProfileBook {
        self.profiles
            .loaded
            .get_or_init(|| async {
                if let Ok(Some(v)) = self.store.kv_get(KV_KEY).await
                    && let Ok(list) = serde_json::from_value::<Vec<AgentProfile>>(v)
                {
                    let mut m = self.profiles.map.lock();
                    for p in list {
                        m.entry(p.name.clone()).or_insert(p);
                    }
                }
            })
            .await;
        &self.profiles
    }

    async fn save_profiles(&self) {
        let list: Vec<AgentProfile> = self.profiles.map.lock().values().cloned().collect();
        if let Err(e) = self.store.kv_set(KV_KEY, serde_json::to_value(list).unwrap_or(Value::Null)).await {
            tracing::warn!(error = %e, "saving agent profiles failed");
        }
    }

    pub(crate) async fn list_profiles(&self) -> Vec<AgentProfile> {
        self.profiles().await.map.lock().values().cloned().collect()
    }

    pub(crate) async fn define_profile(&self, p: AgentProfile) -> Result<AgentProfile, String> {
        p.validate()?;
        if p.instructions.trim().is_empty() {
            return Err("a saved agent needs instructions (how it works)".into());
        }
        let unknown: Vec<&String> = p.tools.iter().filter(|t| self.tools.meta(t).is_none()).collect();
        if !unknown.is_empty() {
            return Err(format!("unknown tools: {} (see GET /v1/tools)", unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
        {
            let mut m = self.profiles().await.map.lock();
            if !m.contains_key(&p.name) && m.len() >= MAX_PROFILES {
                return Err(format!("there are already {MAX_PROFILES} agents: remove one first"));
            }
            m.insert(p.name.clone(), p.clone());
        }
        self.save_profiles().await;
        Ok(p)
    }

    pub(crate) async fn remove_profile(&self, name: &str) -> Result<AgentProfile, String> {
        let p = self.profiles().await.map.lock().remove(name).ok_or_else(|| format!("no agent '{name}'"))?;
        self.save_profiles().await;
        Ok(p)
    }

    /// The profile a plan step names: a saved one by name (an inline object fills in or overrides its fields), or the inline one.
    pub(crate) async fn resolve_profile(&self, v: &Value) -> Result<AgentProfile, String> {
        let p = AgentProfile::from_value(v)?;
        let saved = self.profiles().await.map.lock().get(&p.name).cloned();
        match (saved, p.is_reference()) {
            (Some(s), true) => Ok(s),
            (None, true) => Err(format!("there is no saved agent '{}': define it first (agent_define), or give its instructions in the step", p.name)),
            (Some(s), false) => Ok(AgentProfile {
                description: if p.description.is_empty() { s.description } else { p.description },
                instructions: if p.instructions.is_empty() { s.instructions } else { p.instructions },
                tools: if p.tools.is_empty() { s.tools } else { p.tools },
                writes: p.writes || s.writes,
                name: p.name,
            }),
            (None, false) => Ok(p),
        }
    }

    /// The tools an agent of this profile may use: its list (or every read-only tool), never a writing one unless it `writes`, and never
    /// the tools that manage routines or agents (an agent does its part; the main agent keeps the book).
    pub(crate) fn profile_scope(&self, p: &AgentProfile) -> Vec<String> {
        let metas = self.tools.metas();
        metas
            .iter()
            .filter(|m| p.tools.is_empty() || p.tools.contains(&m.name))
            .filter(|m| p.writes || !m.side_effects)
            .filter(|m| !matches!(m.category.as_str(), "routines" | "agents"))
            .map(|m| m.name.clone())
            .collect()
    }
}

impl Engine {
    pub async fn agent_profiles(&self) -> Vec<AgentProfile> {
        self.inner.list_profiles().await
    }
    pub async fn define_agent(&self, p: AgentProfile) -> Result<AgentProfile, String> {
        self.inner.define_profile(p).await
    }
    pub async fn remove_agent(&self, name: &str) -> Result<AgentProfile, String> {
        self.inner.remove_profile(name).await
    }
}

// --------------------------------------------------------- agent tools ----

struct ProfileTool {
    inner: Weak<Inner>,
    name: &'static str,
}

#[async_trait::async_trait]
impl Tool for ProfileTool {
    fn metadata(&self) -> ToolMetadata {
        let p = |n: &str, t: ParamType, r: bool, d: &str| ParamSpec::new(n, t, r, d);
        let (desc, params, side): (&str, Vec<ParamSpec>, bool) = match self.name {
            "agent_define" => (
                "save a custom subagent that plans can delegate to by name: its instructions (how it works) and the tools it may use",
                vec![
                    p("name", ParamType::String, true, "short name: letters, digits, - or _"),
                    p("instructions", ParamType::String, true, "how the agent works: its method, what to look for, what to return"),
                    p("description", ParamType::String, false, "what it is for, one line"),
                    p("tools", ParamType::String, false, "comma-separated tool names (default: every read-only tool)"),
                    p("writes", ParamType::Boolean, false, "true if it may change files or run changing commands"),
                ],
                true,
            ),
            "agent_list" => ("list the saved custom subagents", vec![], false),
            _ => ("delete a saved custom subagent", vec![p("name", ParamType::String, true, "its name")], true),
        };
        ToolMetadata {
            name: self.name.into(),
            description: desc.into(),
            category: "agents".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: side,
            reversible: true,
            cost_class: "free".into(),
            params,
            keywords: ["agent", "subagent", "delegate", "specialist", "worker"].iter().map(|s| s.to_string()).collect(),
            direct_patterns: vec![],
            cacheable: false,
            contract: vec![],
        }
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let inner = self.inner.upgrade().ok_or_else(|| ToolError::new(ErrorKind::Other, "the engine is shutting down"))?;
        let bad = |e: String| ToolError::new(ErrorKind::InvalidArgs, e);
        match self.name {
            "agent_list" => {
                let all = inner.list_profiles().await;
                Ok(ToolOutput::ok(if all.is_empty() {
                    "no saved agents".into()
                } else {
                    all.iter()
                        .map(|p| {
                            format!(
                                "{} — {} [{}{}]",
                                p.name,
                                p.description,
                                if p.tools.is_empty() { "read-only tools".into() } else { p.tools.join(",") },
                                if p.writes { ", writes" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }))
            }
            name => {
                // a saved agent is an instruction that outlives the task: not for a page to write
                let live = inner.live.lock().get(&ctx.task_id).cloned();
                let exposed = match live {
                    Some(c) => {
                        let r = c.rec.lock().await;
                        r.exposed || r.request.routine.is_some()
                    }
                    None => false,
                };
                if exposed {
                    return Err(ToolError::new(
                        ErrorKind::Withheld,
                        "refused: saved agents are defined or removed only by a request that has not read outside content (nor by a routine's run)",
                    ));
                }
                if name == "agent_define" {
                    let p = AgentProfile::from_value(&json!({
                        "name": input.get("name").cloned().unwrap_or(Value::Null),
                        "description": input.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                        "instructions": input.get("instructions").and_then(|d| d.as_str()).unwrap_or(""),
                        "tools": input.get("tools").cloned().map(|t| if t.is_string() { json!([]) } else { t }).unwrap_or(json!([])),
                        "writes": input.get("writes").and_then(|w| w.as_bool().or_else(|| w.as_str().map(|s| s == "true"))).unwrap_or(false),
                    }))
                    .map_err(bad)?;
                    let tools: Vec<String> = match input.get("tools") {
                        Some(Value::String(s)) => s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
                        _ => p.tools.clone(),
                    };
                    let p = inner.define_profile(AgentProfile { tools, ..p }).await.map_err(bad)?;
                    Ok(ToolOutput::ok(format!(
                        "saved agent '{}' ({} tools{})",
                        p.name,
                        inner.profile_scope(&p).len(),
                        if p.writes { ", writes" } else { ", read-only" }
                    )))
                } else {
                    let n = arg_str(&input, "name")?;
                    let p = inner.remove_profile(n.trim()).await.map_err(bad)?;
                    Ok(ToolOutput::ok(format!("removed agent '{}'", p.name)))
                }
            }
        }
    }
}

pub(crate) fn register_tools(inner: &Arc<Inner>) {
    for name in ["agent_define", "agent_list", "agent_remove"] {
        inner.tools.register(Arc::new(ProfileTool { inner: Arc::downgrade(inner), name }));
    }
}
