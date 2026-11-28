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
    let semantic = config
        .embeddings
        .as_ref()
        .and_then(|e| dfae_memory::RemoteEmbedder::new(&e.base_url, &e.model, e.api_key.clone(), e.dim, std::time::Duration::from_secs(20)));
    if let Some(m) = &semantic {
        tracing::info!(model = m.model(), dim = m.dim(), "memory: semantic embeddings");
    }
    let store: Arc<dyn MemoryStore> = match &config.database_url {
        Some(url) => {
            let pg = Arc::new(PgStore::connect(url).await.map_err(|e| e.to_string())?.with_semantic(semantic).await.map_err(|e| e.to_string())?);
            // rows stored before (or while the model was unreachable) get their semantic embedding in the background
            let bg = pg.clone();
            tokio::spawn(async move {
                match bg.backfill_semantic().await {
                    Ok(n) if n > 0 => tracing::info!(rows = n, "memory: semantic embeddings backfilled"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "memory: semantic backfill stopped"),
                }
            });
            pg
        }
        None => {
            tracing::warn!("DATABASE_URL not set: using volatile in-memory storage");
            Arc::new(InMemoryStore::new().with_semantic(semantic))
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
    // a self-hosted model when one is configured; otherwise the hosted Decisions API when it is (the rules are the fallback of both)
    let hosted = config
        .decision
        .hosted
        .as_ref()
        .and_then(|h| dfae_decision::HostedDecisions::new(&h.base_url, &h.model, h.api_key.clone(), std::time::Duration::from_millis(h.timeout_ms)));
    let primary: Option<Arc<dyn DecisionProvider>> = match (open, hosted) {
        (Some(p), _) => Some(p as Arc<dyn DecisionProvider>),
        (None, Some(h)) => {
            tracing::info!(model = %config.decision.hosted.as_ref().map(|h| h.model.as_str()).unwrap_or(""), "decisions: hosted Decisions API");
            Some(Arc::new(h) as Arc<dyn DecisionProvider>)
        }
        (None, None) => None,
    };
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
        let broker = mcp_broker(&mcp_cfg, state_path.as_deref());
        dfae_mcp::McpHub::with_broker(
            (*registry).clone(),
            dfae_mcp::HubConfig {
                call_timeout: std::time::Duration::from_secs(mcp_cfg.call_timeout_secs),
                max_auto: mcp_cfg.max_auto,
                keys: dfae_mcp::HubConfig::keys_from_env(),
                state_path,
                sign_in: mcp_cfg.sign_in,
                ..Default::default()
            },
            glama,
            broker,
        )
    });
    let pii_ner = config.pii_ner_url.as_deref().map(|u| Arc::new(GlinerClient::new(u)) as Arc<dyn EntityExtractor>);
    let engine = Engine::new(EngineDeps { config, store, extractor, decider, models, tools: registry, policy });
    if let Some(ner) = pii_ner {
        engine.set_pii_ner(ner);
    }
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

/// Who authenticates and opens remote MCP servers: the mcpc broker in its own container, else mcpc on this machine, else nobody (the
/// engine connects to servers that need no credentials itself, and offers none that wants a sign-in).
fn mcp_broker(cfg: &dfae_core::config::McpConfig, state_path: Option<&std::path::Path>) -> std::sync::Arc<dyn dfae_mcp::Broker> {
    use std::sync::Arc;
    let direct = |why: &str| {
        tracing::info!("MCP: no mcpc ({why}): servers that need no credentials or take an API key work, none that wants a sign-in is offered");
        Arc::new(dfae_mcp::DirectBroker) as Arc<dyn dfae_mcp::Broker>
    };
    if cfg.mcpc == "off" {
        return direct("DFAE_MCPC=off");
    }
    if let Some(url) = &cfg.broker_url {
        let Some(token) = &cfg.broker_token else {
            tracing::warn!("DFAE_MCPC_BROKER_URL is set without DFAE_MCPC_BROKER_TOKEN: the mcpc broker is not used");
            return direct("no broker token");
        };
        tracing::info!(broker = %url, "MCP: servers are authenticated and opened by the mcpc broker");
        return Arc::new(dfae_mcp::RemoteBroker::new(url, token));
    }
    let Some((bin, version)) = dfae_mcp::find_mcpc(cfg.mcpc_bin.as_deref()) else {
        return direct(if cfg.mcpc_bin.is_some() { "DFAE_MCPC_BIN does not run" } else { "not installed: npm install -g @apify/mcpc" });
    };
    if version != dfae_mcp::mcpc::MCPC_VERSION {
        tracing::warn!(%version, tested = dfae_mcp::mcpc::MCPC_VERSION, "this mcpc release is not the one the engine is tested with");
    }
    let home = cfg.mcpc_home.clone().or_else(|| state_path.and_then(|p| p.parent()).map(|d| d.join("mcpc")));
    let Some(home) = home else { return direct("no place for its state (set DFAE_MCPC_HOME)") };
    let mut mc = dfae_mcp::McpcConfig::new(bin.clone(), home.clone());
    mc.callback_port = cfg.mcpc_callback_port;
    tracing::info!(mcpc = %bin.display(), %version, home = %home.display(), "MCP: servers are authenticated and opened by mcpc");
    tracing::warn!(
        "mcpc runs as the engine's user: on a host without a keyring its sign-ins are files the agent's commands could read. \
         In a container, run it as its own broker (DFAE_MCPC_BROKER_URL; compose does)"
    );
    dfae_mcp::McpcBroker::new(mc)
}

/// Servers found in the directory are remembered outside the workspace (the agent works in it, and must not be able to add servers
/// by writing a file): `$XDG_DATA_HOME/dfae/mcp.json`, else `~/.local/share/dfae/mcp.json`.
fn default_mcp_state(_workspace: &std::path::Path) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))?;
    Some(base.join("dfae/mcp.json"))
}
