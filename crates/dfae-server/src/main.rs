use dfae_core::Config;
use dfae_runtime::build_engine;

fn init_tracing(json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=warn".into());
    let b = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        b.json().init();
    } else {
        b.init();
    }
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env();
    // `dfae-server healthcheck` — used by the container HEALTHCHECK (no curl needed)
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let port = cfg.bind.rsplit(':').next().unwrap_or("8080");
        let ok = reqwest::get(format!("http://127.0.0.1:{port}/health")).await.map(|r| r.status().is_success()).unwrap_or(false);
        std::process::exit(if ok { 0 } else { 1 });
    }
    init_tracing(cfg.log_json);
    // `dfae-server mcpc-broker` — mcpc in a container of its own (see dfae_server::broker)
    if std::env::args().nth(1).as_deref() == Some("mcpc-broker") {
        return mcpc_broker().await;
    }
    // The agent's commands run as this same user: without this, any of them could read this process's environment (every API key)
    // and memory (MCP sign-ins) from /proc. A non-dumpable process's /proc files belong to root.
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        tracing::warn!("could not make the engine non-dumpable: its keys are readable by commands it runs");
    }
    std::fs::create_dir_all(&cfg.workspace)?;
    tracing::info!(
        decision = if cfg.decision.base_url.is_some() { "open-model" } else { "local" },
        worker = %cfg.worker.provider,
        planner = %cfg.planner.provider,
        gliner = cfg.gliner_url.is_some(),
        database = cfg.database_url.is_some(),
        workspace = %cfg.workspace.display(),
        "starting dfae"
    );
    let engine = build_engine(cfg.clone()).await.map_err(anyhow::Error::msg)?;
    // say now, in the log, which model is unusable and why (a red dot in the TUI cannot)
    {
        let e = engine.clone();
        tokio::spawn(async move {
            for (name, why) in e.model_problems().await {
                tracing::warn!(model = %name, "{why}");
            }
        });
    }
    let resumed = engine.resume_unfinished().await;
    if !resumed.is_empty() {
        tracing::info!(count = resumed.len(), "resumed unfinished tasks");
    }
    engine.start_routines().await;
    let routines = engine.routines();
    tracing::info!(routines = routines.len(), enabled = routines.iter().filter(|r| r.enabled).count(), "routines scheduler started");
    if cfg.api_token.is_none() {
        tracing::warn!("DFAE_API_TOKEN is not set: the API is unauthenticated. Do not expose it beyond a trusted network.");
    }
    let app = dfae_server::router(engine, cfg.api_token.clone());
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

/// The mcpc broker: the API for the engine, and the relay for the user's browser. Configured from the environment:
/// `DFAE_MCPC_BROKER_TOKEN` (required), `DFAE_MCPC_BROKER_BIND` (0.0.0.0:8091), `DFAE_MCPC_RELAY_BIND` (0.0.0.0:8092, `off`),
/// `DFAE_MCPC_BIN`, `DFAE_MCPC_HOME` (~/.mcpc), `DFAE_MCPC_PROXY_HOST` (how the engine reaches this container: `mcpc`),
/// `DFAE_MCPC_CALLBACK_PORT`.
async fn mcpc_broker() -> anyhow::Result<()> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let token = var("DFAE_MCPC_BROKER_TOKEN").ok_or_else(|| anyhow::anyhow!("DFAE_MCPC_BROKER_TOKEN is not set: the broker serves no one without it"))?;
    anyhow::ensure!(token.len() >= 24, "DFAE_MCPC_BROKER_TOKEN is too short (at least 24 characters; `openssl rand -hex 32`)");
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    let (bin, version) = dfae_mcp::find_mcpc(var("DFAE_MCPC_BIN").map(std::path::PathBuf::from).as_deref())
        .ok_or_else(|| anyhow::anyhow!("mcpc is not installed (npm install -g @apify/mcpc@{})", dfae_mcp::mcpc::MCPC_VERSION))?;
    let home = var("DFAE_MCPC_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".mcpc")))
        .ok_or_else(|| anyhow::anyhow!("no DFAE_MCPC_HOME and no HOME"))?;
    let mut mc = dfae_mcp::McpcConfig::new(bin.clone(), home.clone());
    // the proxies listen on every interface of this container (the engine is in another one); each wants its own bearer token
    mc.proxy_bind = "0.0.0.0".into();
    mc.proxy_host = var("DFAE_MCPC_PROXY_HOST").unwrap_or_else(|| "mcpc".into());
    mc.callback_port = var("DFAE_MCPC_CALLBACK_PORT").and_then(|p| p.parse().ok());
    let proxy_host = mc.proxy_host.clone();
    let broker: std::sync::Arc<dyn dfae_mcp::Broker> = dfae_mcp::McpcBroker::new(mc);
    let bind = var("DFAE_MCPC_BROKER_BIND").unwrap_or_else(|| "0.0.0.0:8091".into());
    let relay_bind = var("DFAE_MCPC_RELAY_BIND").unwrap_or_else(|| "0.0.0.0:8092".into());
    tracing::info!(mcpc = %bin.display(), %version, home = %home.display(), api = %bind, relay = %relay_bind, proxies = %proxy_host, "mcpc broker");
    if relay_bind != "off" {
        let relay = dfae_server::broker::relay(broker.clone());
        let l = tokio::net::TcpListener::bind(&relay_bind).await?;
        tokio::spawn(async move {
            if let Err(e) = axum::serve(l, relay).await {
                tracing::error!("the sign-in relay stopped: {e}");
            }
        });
    }
    let l = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(l, dfae_server::broker::api(broker, token)).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}
