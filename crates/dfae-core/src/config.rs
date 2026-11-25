//! Runtime configuration, read from environment variables.

use crate::Budget;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
fn env_or(k: &str, d: &str) -> String {
    env(k).unwrap_or_else(|| d.to_string())
}
fn env_parse<T: std::str::FromStr>(k: &str, d: T) -> T {
    env(k).and_then(|s| s.parse().ok()).unwrap_or(d)
}
fn env_bool(k: &str, d: bool) -> bool {
    env(k).map(|s| matches!(s.to_lowercase().as_str(), "1" | "true" | "yes" | "on")).unwrap_or(d)
}

/// Whether a tool call can wait for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Risky calls (writes, mutating commands, unlisted hosts…) wait for a human decision.
    #[default]
    Ask,
    /// Nothing waits for anyone: a call the policy allows runs, one it forbids is refused.
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionConfig {
    /// OpenAI-compatible endpoint of the self-hosted decision model (it must return
    /// log-probabilities). `None` => the built-in local decision rules are used.
    pub base_url: Option<String>,
    pub model: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub timeout_ms: u64,
    /// How many questions of one decision are asked at the same time (1 lets the server reuse
    /// the cached state prefix between them, which is cheapest on a CPU).
    pub parallel: usize,
    /// Which decisions the model makes (`all` or a list of kinds); the rest use the built-in rules.
    pub kinds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// `openai` (any OpenAI-compatible server), `anthropic`, `mock`, `none`.
    pub provider: String,
    pub base_url: String,
    pub model: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub max_tokens: u32,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thresholds {
    /// Minimum calibrated confidence to trust a `direct` mode decision.
    pub direct_min_confidence: f32,
    /// Minimum similarity for replaying a stored skill.
    pub skill_min_similarity: f32,
    /// Below this calibrated confidence the engine escalates (if it can).
    pub escalate_below: f32,
    /// Tool relevance threshold (probability) for including a tool.
    pub tool_min_relevance: f32,
    /// From this complexity (0..1, from the decision model) a task is "medium" and is planned by the external
    /// model when one is configured; below it the self-hosted models handle it alone. 2.0 turns this off.
    pub external_min_complexity: f32,
}

/// Token-economy switches (the zero-cost mechanisms that save generative tokens).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EconomyConfig {
    /// Fix bad arguments / missing paths deterministically before asking a model.
    pub repair: bool,
    /// Give reasoning steps short upstream summaries and let the model ask for
    /// more (`NEED <step>`). Off by default: it trades a possible extra call for
    /// smaller prompts and should be measured on your workload before enabling.
    pub lazy_context: bool,
    /// The external planner answers in a compact line format instead of JSON (`DFAE_PLAN_FORMAT=json` for JSON).
    pub plan_lines: bool,
    /// The external planner's system prompt carries the whole stable tool catalogue and examples, so a provider
    /// with prompt caching bills it at a fraction after the first call (`DFAE_PLAN_PREFIX=narrow` lists only the
    /// tools judged relevant instead).
    pub plan_prefix: bool,
    /// Send the prefix on every planner call, not only while traffic keeps the provider's cache warm (`DFAE_PLAN_PREFIX=always`).
    pub plan_prefix_always: bool,
    /// The plan carries the final reply as a template; when every step succeeds it is rendered and no answer call is made.
    /// Off by default: the planner pays for the reply in (five-times-priced) output tokens even when a local model would
    /// have written an equally good one for free. For deployments whose self-hosted model writes poor answers.
    pub plan_report: bool,
    /// Planning is always the external model's. When it cannot be reached the task fails with the reason — unless
    /// `DFAE_PLAN_FALLBACK=slm`, which lets the self-hosted model plan instead.
    pub plan_fallback_slm: bool,
    /// The final answer is composed by the self-hosted model and checked against the evidence; the external model
    /// is asked only when that check fails.
    pub answer_local_first: bool,
    /// The plan prompt shows what the workspace contains, so the planner does not spend steps exploring it.
    pub workspace_snapshot: bool,
    /// A big tool output is condensed by the self-hosted model into what matters for the task before the external
    /// model reads it (`DFAE_CONDENSE=off` keeps the mechanical summary).
    pub condense: bool,
}

/// Team orchestration and dispatch limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    /// Automatic delegation to subagents (`DFAE_TEAM=off` disables it).
    pub enabled: bool,
    /// Top-level tasks running at once; the rest wait in the dispatch queue.
    pub max_concurrent_tasks: usize,
    /// Waiting tasks beyond this are rejected (load shedding).
    pub max_queued_tasks: usize,
    /// Subagents running at once, across all teams.
    pub max_agents: usize,
    /// Per-role worker pool sizes. `operator=1` serialises side-effecting agents.
    pub role_slots: std::collections::BTreeMap<String, usize>,
    /// Longest wait for one subagent.
    pub agent_timeout_secs: u64,
}

impl TeamConfig {
    pub fn slots_for(&self, role: &str) -> usize {
        self.role_slots.get(role).copied().unwrap_or(2).max(1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub bind: String,
    #[serde(skip_serializing)]
    pub api_token: Option<String>,
    pub allow_auto_approve: bool,
    /// `DFAE_APPROVALS=ask` brings the human approval step back (default: never ask).
    pub approvals: ApprovalMode,
    #[serde(skip_serializing)]
    pub database_url: Option<String>,
    pub workspace: PathBuf,
    pub policy_path: Option<PathBuf>,
    pub gliner_url: Option<String>,
    pub decision: DecisionConfig,
    pub slm: ModelConfig,
    pub strong: ModelConfig,
    pub budget: Budget,
    pub thresholds: Thresholds,
    pub team: TeamConfig,
    pub economy: EconomyConfig,
    /// Append every decision and what became of it to this JSONL file (`DFAE_DECISION_LOG`): the training and evaluation
    /// set for the decision model (and for the thresholds around it).
    pub decision_log: Option<PathBuf>,
    /// Share of requests that the external model also answers the routing questions for, to measure the decision model
    /// against it (`DFAE_SHADOW_EVAL`, 0..1; default 0 = off). It spends external tokens.
    pub shadow_eval: f32,
    pub max_parallel: usize,
    pub approval_ttl_secs: u64,
    pub log_json: bool,
}

/// `researcher=4,operator=1` -> map. Malformed entries are ignored.
fn parse_slots(s: &str) -> std::collections::BTreeMap<String, usize> {
    s.split(',').filter_map(|kv| kv.split_once('=')).filter_map(|(k, v)| v.trim().parse::<usize>().ok().map(|n| (k.trim().to_string(), n))).collect()
}

impl Config {
    pub fn economy_shadow_on(&self) -> bool {
        self.shadow_eval > 0.0
    }

    pub fn from_env() -> Self {
        let slm_provider = env_or("SLM_PROVIDER", if env("SLM_BASE_URL").is_some() { "openai" } else { "none" });
        Self {
            bind: env_or("DFAE_BIND", "0.0.0.0:8080"),
            api_token: env("DFAE_API_TOKEN"),
            allow_auto_approve: env_bool("DFAE_ALLOW_AUTO_APPROVE", false),
            approvals: if env("DFAE_APPROVALS").map(|v| v.eq_ignore_ascii_case("ask")).unwrap_or(false) { ApprovalMode::Ask } else { ApprovalMode::Never },
            database_url: env("DATABASE_URL"),
            workspace: PathBuf::from(env_or("DFAE_WORKSPACE", "./workspace")),
            policy_path: env("DFAE_POLICY").map(PathBuf::from),
            gliner_url: env("GLINER_URL"),
            decision: DecisionConfig {
                // `DECISION_PROVIDER=local` switches the model off even when a URL is set
                base_url: env("DECISION_BASE_URL").filter(|_| env("DECISION_PROVIDER").map(|p| p != "local").unwrap_or(true)),
                model: env_or("DECISION_MODEL", "qwen3:4b-instruct-2507-q4_K_M"),
                api_key: env("DECISION_API_KEY"),
                timeout_ms: env_parse("DECISION_TIMEOUT_MS", 15_000),
                parallel: env_parse("DECISION_PARALLEL", 3),
                // the model is self-hosted and free per token, so it makes every decision; narrow it to save latency
                kinds: env_or("DECISION_KINDS", "all").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            },
            slm: ModelConfig {
                provider: slm_provider,
                base_url: env_or("SLM_BASE_URL", "http://ollama:11434/v1"),
                model: env_or("SLM_MODEL", "qwen2.5:3b"),
                api_key: env("SLM_API_KEY"),
                max_tokens: env_parse("SLM_MAX_TOKENS", 1024),
                timeout_secs: env_parse("SLM_TIMEOUT_SECS", 180),
            },
            strong: ModelConfig {
                provider: env_or("STRONG_PROVIDER", if env("STRONG_API_KEY").is_some() { "anthropic" } else { "none" }),
                base_url: env_or("STRONG_BASE_URL", "https://api.anthropic.com"),
                model: env_or("STRONG_MODEL", "claude-sonnet-5-5"),
                api_key: env("STRONG_API_KEY"),
                max_tokens: env_parse("STRONG_MAX_TOKENS", 2048),
                timeout_secs: env_parse("STRONG_TIMEOUT_SECS", 120),
            },
            budget: Budget {
                max_steps: env_parse("DFAE_MAX_STEPS", 30),
                max_model_calls: env_parse("DFAE_MAX_MODEL_CALLS", 12),
                max_local_calls: env_parse("DFAE_MAX_LOCAL_CALLS", 200),
                max_tool_calls: env_parse("DFAE_MAX_TOOL_CALLS", 25),
                max_replans: env_parse("DFAE_MAX_REPLANS", 2),
                max_seconds: env_parse("DFAE_MAX_SECONDS", 600),
                max_children: env_parse("DFAE_MAX_CHILDREN", 8),
            },
            thresholds: Thresholds {
                direct_min_confidence: env_parse("DFAE_DIRECT_MIN_CONF", 0.6),
                skill_min_similarity: env_parse("DFAE_SKILL_MIN_SIM", 0.82),
                escalate_below: env_parse("DFAE_ESCALATE_BELOW", 0.35),
                tool_min_relevance: env_parse("DFAE_TOOL_MIN_REL", 0.5),
                external_min_complexity: env_parse("DFAE_EXTERNAL_MIN_COMPLEXITY", 0.3),
            },
            economy: EconomyConfig {
                repair: !matches!(env("DFAE_REPAIR").as_deref(), Some("off" | "false" | "0")),
                lazy_context: env_bool("DFAE_LAZY_CONTEXT", false),
                plan_lines: !matches!(env("DFAE_PLAN_FORMAT").as_deref(), Some("json")),
                plan_prefix: !matches!(env("DFAE_PLAN_PREFIX").as_deref(), Some("narrow" | "off")),
                plan_prefix_always: matches!(env("DFAE_PLAN_PREFIX").as_deref(), Some("always")),
                plan_fallback_slm: matches!(env("DFAE_PLAN_FALLBACK").as_deref(), Some("slm")),
                plan_report: matches!(env("DFAE_PLAN_REPORT").as_deref(), Some("on" | "true" | "1")),
                answer_local_first: !matches!(env("DFAE_ANSWER_LOCAL_FIRST").as_deref(), Some("off" | "false" | "0")),
                workspace_snapshot: !matches!(env("DFAE_WORKSPACE_SNAPSHOT").as_deref(), Some("off" | "false" | "0")),
                condense: !matches!(env("DFAE_CONDENSE").as_deref(), Some("off" | "false" | "0")),
            },
            team: TeamConfig {
                enabled: !matches!(env("DFAE_TEAM").as_deref(), Some("off" | "false" | "0")),
                max_concurrent_tasks: env_parse("DFAE_MAX_CONCURRENT_TASKS", 8).max(1),
                max_queued_tasks: env_parse("DFAE_MAX_QUEUED_TASKS", 64),
                max_agents: env_parse("DFAE_MAX_AGENTS", 8).max(1),
                role_slots: parse_slots(&env_or("DFAE_ROLE_SLOTS", "researcher=4,operator=1,analyst=4,verifier=2")),
                agent_timeout_secs: env_parse("DFAE_AGENT_TIMEOUT_SECS", 600),
            },
            decision_log: env("DFAE_DECISION_LOG").map(PathBuf::from),
            shadow_eval: env_parse("DFAE_SHADOW_EVAL", 0.0f32).clamp(0.0, 1.0),
            max_parallel: env_parse("DFAE_MAX_PARALLEL", 4),
            approval_ttl_secs: env_parse("DFAE_APPROVAL_TTL_SECS", 900),
            log_json: env_bool("DFAE_LOG_JSON", false),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        // Hermetic defaults for tests: no network services.
        let mut c = Self::from_env();
        c.database_url = None;
        c.gliner_url = None;
        c.decision.base_url = None;
        c.approvals = ApprovalMode::Ask; // the approval machinery is what the hermetic tests exercise
        c.slm.provider = "none".into();
        c.strong.provider = "none".into();
        c.api_token = None;
        c
    }
}
