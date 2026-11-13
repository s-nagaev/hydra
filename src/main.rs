//! Hydra: a self-hosted OpenAI-compatible API router (POC).
//!
//! Startup wires config, tracing, the SQLite pool and the axum router, then
//! serves until SIGINT or SIGTERM. All logic lives in the library target so
//! integration tests can drive the same router.

use std::error::Error;
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};

use hydra::http::{self, AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    load_dotenv();
    let cfg = hydra::config::Config::from_env()?;
    init_tracing();

    let db = hydra::db::init_pool(&cfg.db).await?;
    tracing::info!(backend = %cfg.db.redacted(), "database ready");
    let state = AppState::new(
        db,
        cfg.admin_login.clone(),
        cfg.admin_password.clone(),
        cfg.log_bodies,
        cfg.trusted_proxies,
        hydra::flood::FloodCap::new(cfg.flood_limit, cfg.flood_window_secs),
    )
    // Session cookies are signed with a key derived from the admin password
    // unless an explicit secret pins them across password rotations.
    .with_session_secret(cfg.session_secret.as_deref());
    let app = http::router(state.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;

    tracing::info!(addr = %cfg.bind_addr, "hydra listening");
    // ConnectInfo makes the peer address available to handlers, which is
    // what client-IP logging and trusted-proxy XFF resolution read from.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // In-flight requests have finished; give their detached log inserts a
    // brief window to land before the process exits. Whatever misses the
    // window is a lost log row, never a lost request.
    hydra::logging::drain(&state.log_tracker, Duration::from_secs(5)).await;
    Ok(())
}

fn load_dotenv() {
    if let Err(err) = dotenvy::dotenv() {
        // A missing .env is the normal case (environment-only config); a
        // present but unreadable one deserves a warning before validation.
        if !err.not_found() {
            eprintln!("warning: failed to load .env file: {err}");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // dotenv and chdir touch process-global state; this binary target has
    // one test and it runs under this lock.
    static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn real_environment_overrides_dotenv_values() {
        let _guard = STATE_LOCK.lock().unwrap();

        let dir = std::env::temp_dir().join(format!("hydra-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "HYDRA_BIND_ADDR=127.0.0.1:9099\n\
             HYDRA_DB_PATH=./from-file.db\n\
             HYDRA_ADMIN_LOGIN=file-login\n\
             HYDRA_ADMIN_PASSWORD=file-secret\n\
             HYDRA_LOG_BODIES=true\n",
        )
        .unwrap();

        // One variable set in the real environment must win over the file;
        // the rest come from .env.
        std::env::set_var("HYDRA_DB_PATH", "./from-env.db");
        std::env::remove_var("HYDRA_BIND_ADDR");
        std::env::remove_var("HYDRA_ADMIN_LOGIN");
        std::env::remove_var("HYDRA_ADMIN_PASSWORD");
        std::env::remove_var("HYDRA_LOG_BODIES");

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        load_dotenv();
        std::env::set_current_dir(previous).unwrap();

        let cfg = hydra::config::Config::from_env().unwrap();

        // dotenv sets variables for the whole process; undo them so later
        // runs of this binary's test target start clean.
        std::env::remove_var("HYDRA_DB_PATH");
        std::env::remove_var("HYDRA_BIND_ADDR");
        std::env::remove_var("HYDRA_ADMIN_LOGIN");
        std::env::remove_var("HYDRA_ADMIN_PASSWORD");
        std::env::remove_var("HYDRA_LOG_BODIES");
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            cfg.bind_addr,
            "127.0.0.1:9099".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(
            cfg.db,
            hydra::config::DbTarget::Sqlite("./from-env.db".to_string())
        );
        assert_eq!(cfg.admin_login, "file-login");
        assert_eq!(cfg.admin_password, "file-secret");
        assert!(cfg.log_bodies);
    }
}
