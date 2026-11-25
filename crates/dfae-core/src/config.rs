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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionConfig {
    /// OpenAI-compatible endpoint of the self-hosted decision model (it must return
    /// log-probabilities). `None` => the built-in local decision rules are used.
    pub base_url: Option<String>,
    pub model: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub timeout_ms: u64,
    /// How many questions of one decision are asked at the same time.
    pub parallel: usize,
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
    pub max_parallel: usize,
    pub approval_ttl_secs: u64,
    pub log_json: bool,
}

/// `researcher=4,operator=1` -> map. Malformed entries are ignored.
fn parse_slots(s: &str) -> std::collections::BTreeMap<String, usize> {
    s.split(',').filter_map(|kv| kv.split_once('=')).filter_map(|(k, v)| v.trim().parse::<usize>().ok().map(|n| (k.trim().to_string(), n))).collect()
}

impl Config {
    pub fn from_env() -> Self {
        let slm_provider = env_or("SLM_PROVIDER", if env("SLM_BASE_URL").is_some() { "openai" } else { "none" });
        Self {
            bind: env_or("DFAE_BIND", "0.0.0.0:8080"),
            api_token: env("DFAE_API_TOKEN"),
            allow_auto_approve: env_bool("DFAE_ALLOW_AUTO_APPROVE", false),
            database_url: env("DATABASE_URL"),
            workspace: PathBuf::from(env_or("DFAE_WORKSPACE", "./workspace")),
            policy_path: env("DFAE_POLICY").map(PathBuf::from),
            gliner_url: env("GLINER_URL"),
            decision: DecisionConfig {
                // `DECISION_PROVIDER=local` switches the model off even when a URL is set
                base_url: env("DECISION_BASE_URL").filter(|_| env("DECISION_PROVIDER").map(|p| p != "local").unwrap_or(true)),
                model: env_or("DECISION_MODEL", "qwen3:4b-instruct-2507-q8_0"),
                api_key: env("DECISION_API_KEY"),
                timeout_ms: env_parse("DECISION_TIMEOUT_MS", 10_000),
                parallel: env_parse("DECISION_PARALLEL", 4),
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
            },
            economy: EconomyConfig {
                repair: !matches!(env("DFAE_REPAIR").as_deref(), Some("off" | "false" | "0")),
                lazy_context: env_bool("DFAE_LAZY_CONTEXT", false),
            },
            team: TeamConfig {
                enabled: !matches!(env("DFAE_TEAM").as_deref(), Some("off" | "false" | "0")),
                max_concurrent_tasks: env_parse("DFAE_MAX_CONCURRENT_TASKS", 8).max(1),
                max_queued_tasks: env_parse("DFAE_MAX_QUEUED_TASKS", 64),
                max_agents: env_parse("DFAE_MAX_AGENTS", 8).max(1),
                role_slots: parse_slots(&env_or("DFAE_ROLE_SLOTS", "researcher=4,operator=1,analyst=4,verifier=2")),
                agent_timeout_secs: env_parse("DFAE_AGENT_TIMEOUT_SECS", 600),
            },
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
        c.slm.provider = "none".into();
        c.strong.provider = "none".into();
        c.api_token = None;
        c
    }
}
