//! Wire a complete engine from configuration.

use crate::engine::{Engine, EngineDeps};
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, OpenModelProvider};
use dfae_extract::{GlinerClient, HybridExtractor};
use dfae_memory::{InMemoryStore, PgStore};
use dfae_models::{ModelGateway, build_model};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use std::sync::Arc;

pub async fn build_engine(config: Config) -> Result<Engine, String> {
    // memory
    let store: Arc<dyn MemoryStore> = match &config.database_url {
        Some(url) => Arc::new(PgStore::connect(url).await.map_err(|e| e.to_string())?),
        None => {
            tracing::warn!("DATABASE_URL not set: using volatile in-memory storage");
            Arc::new(InMemoryStore::new())
        }
    };
    // policy
    let mut policy_cfg = match &config.policy_path {
        Some(p) => PolicyConfig::from_toml(&std::fs::read_to_string(p).map_err(|e| format!("reading policy {}: {e}", p.display()))?)
            .map_err(|e| format!("invalid policy file: {e}"))?,
        None => PolicyConfig::default(),
    };
    policy_cfg.approvals = config.approvals;
    let policy = Arc::new(PolicyEngine::new(policy_cfg, config.workspace.clone()));
    // tools
    let mut registry = ToolRegistry::new();
    register_native(&mut registry, policy.clone(), store.clone());
    // extraction
    let semantic: Option<Arc<dyn EntityExtractor>> = config.gliner_url.as_deref().map(|u| Arc::new(GlinerClient::new(u)) as Arc<dyn EntityExtractor>);
    let extractor: Arc<dyn EntityExtractor> = Arc::new(HybridExtractor::new(semantic));
    // decisions
    let calibrator = Arc::new(Calibrator::new());
    if let Ok(Some(v)) = store.kv_get("calibration").await {
        calibrator.load(&v);
    }
    let open = OpenModelProvider::new(&config.decision).map(Arc::new);
    if let Some(p) = &open {
        p.start_warmup(); // load the model in the background; decisions use the local rules until it answers
    }
    let primary: Option<Arc<dyn DecisionProvider>> = open.map(|p| p as Arc<dyn DecisionProvider>);
    let decider = Arc::new(Decider::new(primary, calibrator, config.thresholds.clone()).with_model_kinds(&config.decision.kinds));
    // models
    let models = ModelGateway::new(build_model(&config.worker), build_model(&config.planner));

    // MCP: the operator's servers, and the directory to find more in
    let mcp_cfg = config.mcp.clone();
    let workspace = config.workspace.clone();
    let registry = Arc::new(registry);
    let hub = (mcp_cfg.servers_file.is_some() || mcp_cfg.discovery()).then(|| {
        let glama = mcp_cfg.glama_api_key.as_deref().map(|k| dfae_mcp::Glama::new(&mcp_cfg.glama_base_url, k));
        let state_path = mcp_cfg.state_path.clone().or_else(|| default_mcp_state(&workspace));
        dfae_mcp::McpHub::new(
            (*registry).clone(),
            dfae_mcp::HubConfig {
                call_timeout: std::time::Duration::from_secs(mcp_cfg.call_timeout_secs),
                max_auto: mcp_cfg.max_auto,
                keys: dfae_mcp::HubConfig::keys_from_env(),
                state_path,
                ..Default::default()
            },
            glama,
        )
    });
    let engine = Engine::new(EngineDeps { config, store, extractor, decider, models, tools: registry, policy });
    if let Some(hub) = hub {
        if let Some(file) = &mcp_cfg.servers_file {
            let text = std::fs::read_to_string(file).map_err(|e| format!("reading DFAE_MCP_SERVERS {}: {e}", file.display()))?;
            let n = hub.load_config(&text).map_err(|e| format!("DFAE_MCP_SERVERS {}: {e}", file.display()))?;
            tracing::info!("{n} MCP server(s) from {}", file.display());
        }
        if mcp_cfg.discovery() {
            hub.restore();
            tracing::info!("MCP discovery on: servers are found in the Glama directory when a request needs one");
        }
        engine.set_mcp(hub);
    }
    engine.restore().await;
    Ok(engine)
}

/// Servers found in the directory are remembered outside the workspace (the agent works in it, and must not be able to add servers
/// by writing a file): `$XDG_DATA_HOME/dfae/mcp.json`, else `~/.local/share/dfae/mcp.json`.
fn default_mcp_state(_workspace: &std::path::Path) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))?;
    Some(base.join("dfae/mcp.json"))
}
