//! Wire a complete engine from configuration.

use crate::engine::{Engine, EngineDeps};
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, JevProvider};
use dfae_extract::{GlinerClient, HybridExtractor};
use dfae_memory::{InMemoryStore, PgStore};
use dfae_models::{build_model, ModelGateway};
use dfae_tools::{register_native, PolicyConfig, PolicyEngine, ToolRegistry};
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
    let policy_cfg = match &config.policy_path {
        Some(p) => PolicyConfig::from_toml(&std::fs::read_to_string(p).map_err(|e| format!("reading policy {}: {e}", p.display()))?).map_err(|e| format!("invalid policy file: {e}"))?,
        None => PolicyConfig::default(),
    };
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
    let primary: Option<Arc<dyn DecisionProvider>> = JevProvider::new(&config.jev).map(|p| Arc::new(p) as Arc<dyn DecisionProvider>);
    let decider = Arc::new(Decider::new(primary, calibrator, config.thresholds.clone()));
    // models
    let models = ModelGateway::new(build_model(&config.slm), build_model(&config.strong));

    Ok(Engine::new(EngineDeps { config, store, extractor, decider, models, tools: Arc::new(registry), policy }))
}
