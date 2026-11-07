//! Environment-driven configuration with fail-fast validation.
//!
//! Required variables abort startup with an error naming the offending
//! variable. Optional variables have documented defaults that are applied
//! explicitly, never silently on parse errors.

use std::env;
use std::net::SocketAddr;

/// Runtime configuration parsed from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub db_path: String,
    // Consumed by the admin Basic Auth and proxy logging tasks; validated
    // here from day one so the config shape is final.
    #[allow(dead_code)]
    pub admin_login: String,
    #[allow(dead_code)]
    pub admin_password: String,
    #[allow(dead_code)]
    pub log_bodies: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("environment variable {0} is required but not set")]
    Missing(&'static str),
    #[error("environment variable {0} must not be empty")]
    Empty(&'static str),
    #[error("environment variable {0} has an invalid value: {1}")]
    Invalid(&'static str, String),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = match env::var("HYDRA_BIND_ADDR") {
            Ok(raw) => raw
                .parse::<SocketAddr>()
                .map_err(|e| ConfigError::Invalid("HYDRA_BIND_ADDR", e.to_string()))?,
            Err(_) => SocketAddr::from(([127, 0, 0, 1], 8080)),
        };
        let db_path = env::var("HYDRA_DB_PATH").unwrap_or_else(|_| "./hydra.db".to_string());
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
            db_path,
            admin_login,
            admin_password,
            log_bodies,
        })
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
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HYDRA_ADMIN_LOGIN"));
    }

    #[test]
    fn empty_password_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
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
        env::remove_var("HYDRA_LOG_BODIES");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        let cfg = Config::from_env().unwrap();
        assert_eq!(
            cfg.bind_addr,
            "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(cfg.db_path, "./hydra.db");
        assert!(!cfg.log_bodies);
    }

    #[test]
    fn invalid_bind_addr_is_rejected_with_reason() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var("HYDRA_BIND_ADDR", "not-an-addr");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::remove_var("HYDRA_LOG_BODIES");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HYDRA_BIND_ADDR"));
    }
}
