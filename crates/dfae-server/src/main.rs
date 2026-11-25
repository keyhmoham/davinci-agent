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
    std::fs::create_dir_all(&cfg.workspace)?;
    tracing::info!(
        decision = if cfg.decision.base_url.is_some() { "open-model" } else { "local" },
        slm = %cfg.slm.provider,
        strong = %cfg.strong.provider,
        gliner = cfg.gliner_url.is_some(),
        database = cfg.database_url.is_some(),
        workspace = %cfg.workspace.display(),
        "starting dfae"
    );
    let engine = build_engine(cfg.clone()).await.map_err(anyhow::Error::msg)?;
    let resumed = engine.resume_unfinished().await;
    if !resumed.is_empty() {
        tracing::info!(count = resumed.len(), "resumed unfinished tasks");
    }
    if cfg.api_token.is_none() {
        tracing::warn!("DFAE_API_TOKEN is not set: the API is unauthenticated. Do not expose it beyond a trusted network.");
    }
    let app = dfae_server::router(engine, cfg.api_token.clone());
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}
