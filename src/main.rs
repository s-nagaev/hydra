//! Hydra: a self-hosted OpenAI-compatible API router (POC).
//!
//! Startup wires config, tracing, the SQLite pool and the axum router, then
//! serves until SIGINT or SIGTERM. All logic lives in the library target so
//! integration tests can drive the same router.

use std::error::Error;
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};

use hydra::http::{self, AppState};
use hydra::logging::LogTracker;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cfg = hydra::config::Config::from_env()?;
    init_tracing();

    let pool = hydra::db::init_pool(&cfg.db_path).await?;
    let state = AppState {
        db: pool,
        admin_login: cfg.admin_login.clone(),
        admin_password: cfg.admin_password.clone(),
        client: http::upstream_client(),
        log_bodies: cfg.log_bodies,
        log_tracker: LogTracker::default(),
    };
    let app = http::router(state.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;

    tracing::info!(addr = %cfg.bind_addr, "hydra listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // In-flight requests have finished; give their detached log inserts a
    // brief window to land before the process exits. Whatever misses the
    // window is a lost log row, never a lost request.
    hydra::logging::drain(&state.log_tracker, Duration::from_secs(5)).await;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("hydra=info,tower_http=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install SIGINT handler");
    };

    let terminate = async {
        signal(SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining");
}
