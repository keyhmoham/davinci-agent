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
    /// Output limit for ordinary work (choosing arguments, short reasoning, answers): kept small, it is billed.
    pub max_tokens: u32,
    /// Output limit when the model writes a file's content (a page, code, data): generous, the content is the product.
    pub content_max_tokens: u32,
    pub timeout_secs: u64,
    /// `<PREFIX>_PROMPT_CACHE=on|off`: whether an OpenAI-compatible endpoint is known to bill a repeated prompt prefix at a
    /// fraction (Anthropic and api.openai.com are known to; `None` leaves it to the provider's own knowledge).
    pub prompt_cache: Option<bool>,
    /// `<PREFIX>_REASONING`: `off`, `minimal`, `low`, `medium`, `high` or `default` (the provider's own). A reasoning model otherwise
    /// spends most of a small output limit on hidden thinking: the planner thinks a little, the worker not at all.
    pub reasoning: Option<String>,
    /// `<PREFIX>_REASONING_HARD`: the thinking effort when the model repairs something (a plan that hit a failure): debugging is the one
    /// job where thinking longer pays for itself. Default `medium` for the planner; unset for the worker (it keeps `reasoning`).
    pub reasoning_hard: Option<String>,
}

/// Where hosted models are reached unless a deployment says otherwise (an OpenAI-compatible gateway).
pub const DEFAULT_MODEL_BASE_URL: &str = "https://ai-gateway.vercel.sh/v1";
pub const DEFAULT_PLANNER_MODEL: &str = "gpt-6-luna";
pub const DEFAULT_WORKER_MODEL: &str = "glm-5.3-flash";

impl ModelConfig {
    /// A hosted model from `<PREFIX>_PROVIDER` (`openai` | `anthropic` | `none`; default `openai` once a key is set),
    /// `_BASE_URL`, `_MODEL`, `_API_KEY`, `_MAX_TOKENS`, `_CONTENT_MAX_TOKENS`, `_TIMEOUT_SECS`. The planner and the worker
    /// model are configured the same way.
    fn from_env(prefix: &str, default_model: &str, max_tokens: u32, content_max_tokens: u32, timeout_secs: u64, reasoning: &str, hard: &str) -> Self {
        let var = |k: &str| env(&format!("{prefix}_{k}"));
        let provider = var("PROVIDER").unwrap_or_else(|| if var("API_KEY").is_some() { "openai".into() } else { "none".into() });
        Self {
            base_url: var("BASE_URL")
                .unwrap_or_else(|| if provider == "anthropic" { "https://api.anthropic.com".into() } else { DEFAULT_MODEL_BASE_URL.into() }),
            provider,
            model: var("MODEL").unwrap_or_else(|| default_model.into()),
            api_key: var("API_KEY"),
            max_tokens: var("MAX_TOKENS").and_then(|v| v.parse().ok()).unwrap_or(max_tokens),
            content_max_tokens: var("CONTENT_MAX_TOKENS").and_then(|v| v.parse().ok()).unwrap_or(content_max_tokens),
            timeout_secs: var("TIMEOUT_SECS").and_then(|v| v.parse().ok()).unwrap_or(timeout_secs),
            reasoning: Some(var("REASONING").unwrap_or_else(|| reasoning.into())).map(|r| r.to_ascii_lowercase()).filter(|r| r != "default"),
            reasoning_hard: Some(var("REASONING_HARD").unwrap_or_else(|| hard.into()))
                .map(|r| r.to_ascii_lowercase())
                .filter(|r| r != "default" && !r.is_empty()),
            prompt_cache: var("PROMPT_CACHE").map(|v| matches!(v.to_ascii_lowercase().as_str(), "on" | "true" | "1" | "yes")),
        }
    }
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
    pub plan_min_complexity: f32,
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
    /// The planner model's system prompt carries the whole stable tool catalogue and examples, so a provider
    /// with prompt caching bills it at a fraction after the first call (`DFAE_PLAN_PREFIX=narrow` lists only the
    /// tools judged relevant instead).
    pub plan_prefix: bool,
    /// Send the prefix on every planner call, not only while traffic keeps the provider's cache warm (`DFAE_PLAN_PREFIX=always`).
    pub plan_prefix_always: bool,
    /// The plan carries the final reply as a template; when every step succeeds it is rendered and no answer call is made.
    /// Off by default: the planner pays for the reply in (five-times-priced) output tokens even when a local model would
    /// have written an equally good one for free. For deployments whose self-hosted model writes poor answers.
    pub plan_report: bool,
    /// Planning is always the planner model's. When it cannot be reached the task fails with the reason — unless
    /// `DFAE_PLAN_FALLBACK=worker`, which lets the self-hosted model plan instead.
    pub plan_fallback_worker: bool,
    /// The final answer is composed by the self-hosted model and checked against the evidence; the planner model
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
    pub mcp: McpConfig,
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
    pub worker: ModelConfig,
    pub planner: ModelConfig,
    pub budget: Budget,
    pub thresholds: Thresholds,
    pub team: TeamConfig,
    pub economy: EconomyConfig,
    /// Append every decision and what became of it to this JSONL file (`DFAE_DECISION_LOG`): the training and evaluation
    /// set for the decision model (and for the thresholds around it).
    pub decision_log: Option<PathBuf>,
    /// Share of requests that the planner model also answers the routing questions for, to measure the decision model
    /// against it (`DFAE_SHADOW_EVAL`, 0..1; default 0 = off). It spends external tokens.
    pub shadow_eval: f32,
    pub max_parallel: usize,
    pub approval_ttl_secs: u64,
    pub log_json: bool,
}

/// MCP (Model Context Protocol) servers as tools, and the directory they are found in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// `GLAMA_API_KEY`: the key of the Glama MCP directory. With it, the engine finds, connects and uses servers on demand.
    #[serde(skip_serializing)]
    pub glama_api_key: Option<String>,
    /// `GLAMA_BASE_URL` (default `https://glama.ai/api/mcp`).
    pub glama_base_url: String,
    /// `DFAE_MCP_AUTO` (default on): find servers in the directory when a request needs one. Only used with a key.
    pub auto: bool,
    /// `DFAE_MCP_MAX_AUTO` (default 2): servers added from the directory for one request.
    pub max_auto: usize,
    /// `DFAE_MCP_SERVERS`: a JSON file in the usual `{"mcpServers": {...}}` shape (stdio commands and remote URLs) of servers the
    /// operator wants. Only the operator's own file can start a local command.
    pub servers_file: Option<PathBuf>,
    /// `DFAE_MCP_STATE`: where servers found in the directory are remembered between runs (default: `<workspace>/.dfae/mcp.json`).
    pub state_path: Option<PathBuf>,
    /// `DFAE_MCP_TIMEOUT_SECS` (default 60): one tool call.
    pub call_timeout_secs: u64,
}

impl McpConfig {
    pub fn discovery(&self) -> bool {
        self.auto && self.glama_api_key.is_some()
    }
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
        let planner = ModelConfig::from_env("PLANNER", DEFAULT_PLANNER_MODEL, 4096, 4096, 180, "low", "medium");
        let mut worker = ModelConfig::from_env("WORKER", DEFAULT_WORKER_MODEL, 800, 16_000, 90, "off", "");
        // one gateway, one key: a worker with no key of its own uses the planner's when they share an endpoint
        if worker.api_key.is_none() && worker.base_url == planner.base_url && planner.api_key.is_some() {
            worker.api_key = planner.api_key.clone();
            if env("WORKER_PROVIDER").is_none() {
                worker.provider = planner.provider.clone();
            }
        }
        Self {
            mcp: McpConfig {
                glama_api_key: env("GLAMA_API_KEY"),
                glama_base_url: env_or("GLAMA_BASE_URL", "https://glama.ai/api/mcp").trim_end_matches('/').to_string(),
                auto: env_bool("DFAE_MCP_AUTO", true),
                max_auto: env_parse("DFAE_MCP_MAX_AUTO", 2usize).clamp(1, 5),
                servers_file: env("DFAE_MCP_SERVERS").map(PathBuf::from),
                state_path: env("DFAE_MCP_STATE").map(PathBuf::from),
                call_timeout_secs: env_parse("DFAE_MCP_TIMEOUT_SECS", 60u64).clamp(5, 600),
            },
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
            worker,
            planner,
            budget: Budget {
                max_steps: env_parse("DFAE_MAX_STEPS", 30),
                max_model_calls: env_parse("DFAE_MAX_MODEL_CALLS", 12),
                max_worker_calls: env_parse("DFAE_MAX_WORKER_CALLS", 200),
                max_tool_calls: env_parse("DFAE_MAX_TOOL_CALLS", 25),
                max_replans: env_parse("DFAE_MAX_REPLANS", 4),
                max_seconds: env_parse("DFAE_MAX_SECONDS", 600),
                max_children: env_parse("DFAE_MAX_CHILDREN", 8),
            },
            thresholds: Thresholds {
                direct_min_confidence: env_parse("DFAE_DIRECT_MIN_CONF", 0.6),
                skill_min_similarity: env_parse("DFAE_SKILL_MIN_SIM", 0.82),
                escalate_below: env_parse("DFAE_ESCALATE_BELOW", 0.35),
                tool_min_relevance: env_parse("DFAE_TOOL_MIN_REL", 0.5),
                plan_min_complexity: env_parse("DFAE_PLAN_MIN_COMPLEXITY", 0.3),
            },
            economy: EconomyConfig {
                repair: !matches!(env("DFAE_REPAIR").as_deref(), Some("off" | "false" | "0")),
                lazy_context: env_bool("DFAE_LAZY_CONTEXT", false),
                plan_prefix: !matches!(env("DFAE_PLAN_PREFIX").as_deref(), Some("narrow" | "off")),
                plan_prefix_always: matches!(env("DFAE_PLAN_PREFIX").as_deref(), Some("always")),
                plan_fallback_worker: matches!(env("DFAE_PLAN_FALLBACK").as_deref(), Some("worker")),
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
        c.worker.provider = "none".into();
        c.planner.provider = "none".into();
        c.api_token = None;
        c.mcp.glama_api_key = None;
        c.mcp.servers_file = None;
        c
    }
}

#[cfg(test)]
mod model_config_tests {
    use super::*;

    // Environment variables are process-wide: this is the only test that touches PLANNER_*/WORKER_*.
    #[test]
    fn planner_and_worker_are_configured_the_same_way_with_hosted_defaults() {
        // SAFETY: single-threaded access to variables no other test reads
        unsafe {
            for k in [
                "PLANNER_API_KEY",
                "PLANNER_MODEL",
                "PLANNER_PROVIDER",
                "PLANNER_BASE_URL",
                "WORKER_API_KEY",
                "WORKER_MODEL",
                "WORKER_PROVIDER",
                "WORKER_BASE_URL",
                "WORKER_MAX_TOKENS",
            ] {
                std::env::remove_var(k);
            }
        }
        let c = Config::from_env();
        assert_eq!((c.planner.provider.as_str(), c.worker.provider.as_str()), ("none", "none"), "no key, no hosted model");
        assert_eq!((c.planner.model.as_str(), c.worker.model.as_str()), ("gpt-6-luna", "glm-5.3-flash"));
        assert_eq!(c.planner.base_url, c.worker.base_url);
        assert!(c.worker.max_tokens < c.worker.content_max_tokens, "ordinary work is held to a small limit, file content is not");
        unsafe {
            std::env::set_var("PLANNER_API_KEY", "k");
        }
        let c = Config::from_env();
        assert_eq!((c.planner.provider.as_str(), c.worker.provider.as_str()), ("openai", "openai"), "the worker shares the planner's gateway and key");
        assert_eq!(c.worker.api_key.as_deref(), Some("k"));
        unsafe {
            std::env::set_var("WORKER_BASE_URL", "https://other.example/v1");
            std::env::set_var("WORKER_MAX_TOKENS", "300");
        }
        let c = Config::from_env();
        assert_eq!((c.worker.provider.as_str(), c.worker.api_key.is_none(), c.worker.max_tokens), ("none", true, 300), "another endpoint needs its own key");
        unsafe {
            for k in ["PLANNER_API_KEY", "WORKER_BASE_URL", "WORKER_MAX_TOKENS"] {
                std::env::remove_var(k);
            }
        }
    }
}
