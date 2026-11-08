//! Environment-driven configuration with fail-fast validation.
//!
//! Required variables abort startup with an error naming the offending
//! variable. Optional variables have documented defaults that are applied
//! explicitly, never silently on parse errors.

use std::env;
use std::fmt;
use std::net::SocketAddr;

/// Which database backend to connect to, resolved from the environment.
///
/// `HYDRA_DATABASE_URL` decides when set: `sqlite:<path>` selects SQLite,
/// `postgres://...` selects PostgreSQL, anything else is a startup error.
/// When the variable is absent, `HYDRA_DB_PATH` (or its default) selects
/// SQLite, preserving the pre-PostgreSQL configuration shape.
///
/// The PostgreSQL URL carries credentials: never log it, never put it in an
/// error message. Use [`DbTarget::redacted`] anywhere a human might see the
/// database target.
#[derive(Clone, PartialEq, Eq)]
pub enum DbTarget {
    Sqlite(String),
    Postgres(String),
}

impl DbTarget {
    /// Log-safe display: the PostgreSQL URL is masked, only the scheme shows.
    /// A `sqlite:` URL is shown without doubling the scheme prefix.
    pub fn redacted(&self) -> String {
        match self {
            DbTarget::Sqlite(url) => {
                format!("sqlite:{}", url.strip_prefix("sqlite:").unwrap_or(url))
            }
            DbTarget::Postgres(_) => "postgres://<redacted>".to_string(),
        }
    }
}

impl fmt::Debug for DbTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never expose the plaintext URL, credentials included.
        write!(f, "DbTarget({})", self.redacted())
    }
}

/// Runtime configuration parsed from environment variables.
#[derive(Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub db: DbTarget,
    // Consumed by the admin Basic Auth; validated here from day one so the
    // config shape is final.
    #[allow(dead_code)]
    pub admin_login: String,
    #[allow(dead_code)]
    pub admin_password: String,
    pub log_bodies: bool,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The database URL (via DbTarget's own redacted Debug) and the admin
        // password must never appear in Debug output.
        f.debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("db", &self.db)
            .field("admin_login", &self.admin_login)
            .field("admin_password", &"<redacted>")
            .field("log_bodies", &self.log_bodies)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("environment variable {0} is required but not set")]
    Missing(&'static str),
    #[error("environment variable {0} must not be empty")]
    Empty(&'static str),
    #[error("environment variable {0} has an invalid value: {1}")]
    Invalid(&'static str, String),
    #[error("environment variable {0} has an unsupported scheme (expected sqlite: or postgres://); the value is never echoed to keep credentials out of logs")]
    UnsupportedScheme(&'static str),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = match env::var("HYDRA_BIND_ADDR") {
            Ok(raw) => raw
                .parse::<SocketAddr>()
                .map_err(|e| ConfigError::Invalid("HYDRA_BIND_ADDR", e.to_string()))?,
            Err(_) => SocketAddr::from(([127, 0, 0, 1], 8080)),
        };
        let db = resolve_db_target()?;
        let admin_login = required_non_empty("HYDRA_ADMIN_LOGIN")?;
        let admin_password = required_non_empty("HYDRA_ADMIN_PASSWORD")?;
        let log_bodies = match env::var("HYDRA_LOG_BODIES") {
            Ok(raw) => raw
                .parse::<bool>()
                .map_err(|e| ConfigError::Invalid("HYDRA_LOG_BODIES", e.to_string()))?,
            Err(_) => false,
        };

        Ok(Self {
            bind_addr,
            db,
            admin_login,
            admin_password,
            log_bodies,
        })
    }
}

/// Scheme dispatch for the database target: `HYDRA_DATABASE_URL` wins when
/// set, otherwise the legacy `HYDRA_DB_PATH` (or its default) keeps SQLite
/// the default backend. An unknown scheme aborts startup without echoing
/// the URL.
fn resolve_db_target() -> Result<DbTarget, ConfigError> {
    match env::var("HYDRA_DATABASE_URL") {
        Ok(url) if url.starts_with("sqlite:") => Ok(DbTarget::Sqlite(url)),
        Ok(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => {
            Ok(DbTarget::Postgres(url))
        }
        Ok(_) => Err(ConfigError::UnsupportedScheme("HYDRA_DATABASE_URL")),
        Err(_) => Ok(DbTarget::Sqlite(
            env::var("HYDRA_DB_PATH").unwrap_or_else(|_| "./hydra.db".to_string()),
        )),
    }
}

fn required_non_empty(var: &'static str) -> Result<String, ConfigError> {
    match env::var(var) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) => Err(ConfigError::Empty(var)),
        Err(_) => Err(ConfigError::Missing(var)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // Env vars are process-global, so tests touching them share one lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn missing_required_variable_names_the_variable() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::remove_var("HYDRA_ADMIN_LOGIN");
        env::remove_var("HYDRA_ADMIN_PASSWORD");
        env::remove_var("HYDRA_DATABASE_URL");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HYDRA_ADMIN_LOGIN"));
    }

    #[test]
    fn empty_password_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::remove_var("HYDRA_DATABASE_URL");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HYDRA_ADMIN_PASSWORD"));
    }

    #[test]
    fn defaults_fill_optional_variables() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::remove_var("HYDRA_DB_PATH");
        env::remove_var("HYDRA_DATABASE_URL");
        env::remove_var("HYDRA_LOG_BODIES");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        let cfg = Config::from_env().unwrap();
        assert_eq!(
            cfg.bind_addr,
            "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(cfg.db, DbTarget::Sqlite("./hydra.db".to_string()));
        assert!(!cfg.log_bodies);
    }

    #[test]
    fn invalid_bind_addr_is_rejected_with_reason() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var("HYDRA_BIND_ADDR", "not-an-addr");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::remove_var("HYDRA_DATABASE_URL");
        env::remove_var("HYDRA_LOG_BODIES");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HYDRA_BIND_ADDR"));
    }

    #[test]
    fn sqlite_database_url_selects_the_sqlite_backend() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::set_var("HYDRA_DATABASE_URL", "sqlite:/tmp/other.db");
        env::remove_var("HYDRA_LOG_BODIES");
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.db, DbTarget::Sqlite("sqlite:/tmp/other.db".to_string()));
    }

    #[test]
    fn postgres_database_url_selects_the_postgres_backend() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::set_var(
            "HYDRA_DATABASE_URL",
            "postgres://postgres:hydra@127.0.0.1:54329/postgres",
        );
        env::remove_var("HYDRA_LOG_BODIES");
        let cfg = Config::from_env().unwrap();
        assert_eq!(
            cfg.db,
            DbTarget::Postgres("postgres://postgres:hydra@127.0.0.1:54329/postgres".to_string())
        );
    }

    #[test]
    fn postgresql_scheme_is_accepted_for_postgres() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::set_var("HYDRA_DATABASE_URL", "postgresql://db.internal/hydra");
        env::remove_var("HYDRA_LOG_BODIES");
        let cfg = Config::from_env().unwrap();
        assert_eq!(
            cfg.db,
            DbTarget::Postgres("postgresql://db.internal/hydra".to_string())
        );
    }

    #[test]
    fn unknown_scheme_fails_naming_the_variable_without_echoing_the_url() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::set_var("HYDRA_DATABASE_URL", "mysql://user:pw@host/db");
        env::remove_var("HYDRA_LOG_BODIES");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HYDRA_DATABASE_URL"));
        assert!(!err.to_string().contains("mysql://"));
        assert!(!err.to_string().contains("pw"));
    }

    #[test]
    fn missing_database_url_falls_back_to_db_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::remove_var("HYDRA_DATABASE_URL");
        env::set_var("HYDRA_DB_PATH", "./legacy.db");
        env::remove_var("HYDRA_LOG_BODIES");
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.db, DbTarget::Sqlite("./legacy.db".to_string()));
    }

    #[test]
    fn postgres_url_is_redacted_in_display() {
        let postgres = DbTarget::Postgres("postgres://user:sekret@host:5432/db".to_string());
        let shown = postgres.redacted();
        assert!(!shown.contains("sekret"));
        assert!(!shown.contains("user"));
        assert!(shown.starts_with("postgres://"));

        let sqlite = DbTarget::Sqlite("./hydra.db".to_string());
        assert_eq!(sqlite.redacted(), "sqlite:./hydra.db");

        let sqlite_url = DbTarget::Sqlite("sqlite:/tmp/hydra.db".to_string());
        assert_eq!(sqlite_url.redacted(), "sqlite:/tmp/hydra.db");
    }

    #[test]
    fn debug_output_never_contains_the_plaintext_url_or_password() {
        let cfg = Config {
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            db: DbTarget::Postgres("postgres://user:sekret@host:5432/db".to_string()),
            admin_login: "admin".to_string(),
            admin_password: "sekret".to_string(),
            log_bodies: false,
        };
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("sekret"));
        assert!(!shown.contains("user@"));
        assert!(!shown.contains("postgres://user"));
        assert!(shown.contains("<redacted>"));
    }
}
