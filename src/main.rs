//! Hydra: a self-hosted OpenAI-compatible API router (POC).
//!
//! Startup wires config, tracing, the SQLite pool and the axum router, then
//! serves until SIGINT or SIGTERM. All logic lives in the library target so
//! integration tests can drive the same router.

use std::error::Error;

use tokio::signal::unix::{signal, SignalKind};

use hydra::http::{self, AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cfg = hydra::config::Config::from_env()?;
    init_tracing();

    let pool = hydra::db::init_pool(&cfg.db_path).await?;
    let app = http::router(AppState {
        db: pool,
        admin_login: cfg.admin_login.clone(),
        admin_password: cfg.admin_password.clone(),
        client: http::upstream_client(),
    });
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;

    tracing::info!(addr = %cfg.bind_addr, "hydra listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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
