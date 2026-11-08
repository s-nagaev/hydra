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

/// Trusted reverse-proxy networks, parsed from `HYDRA_TRUSTED_PROXIES`
/// (comma-separated IPv4/IPv6 CIDRs; a bare address means a single host).
/// An empty list (the default) means no peer is ever treated as a proxy:
/// every request is attributed to its direct peer address, so a spoofed
/// `X-Forwarded-For` header cannot influence the logged IP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedProxies {
    networks: Vec<(std::net::IpAddr, u8)>,
}

impl TrustedProxies {
    /// Parses the comma-separated CIDR list. Fails naming the first
    /// malformed entry so a typo aborts startup instead of silently
    /// disabling proxy trust.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut networks = Vec::new();
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (addr_text, prefix) = match entry.split_once('/') {
                Some((addr, prefix)) => (addr, Some(prefix)),
                None => (entry, None),
            };
            let addr: std::net::IpAddr = addr_text
                .parse()
                .map_err(|_| format!("invalid IP address in CIDR entry: {addr_text}"))?;
            let max_prefix = match addr {
                std::net::IpAddr::V4(_) => 32,
                std::net::IpAddr::V6(_) => 128,
            };
            let prefix = match prefix {
                Some(prefix) => prefix
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|value| *value <= max_prefix)
                    .ok_or_else(|| format!("invalid prefix length in CIDR entry: {entry}"))?,
                None => max_prefix,
            };
            networks.push((addr, prefix));
        }
        Ok(Self { networks })
    }

    /// Whether `ip` falls inside any trusted network.
    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        self.networks
            .iter()
            .any(|(network_addr, prefix)| match (network_addr, ip) {
                (std::net::IpAddr::V4(net), std::net::IpAddr::V4(ip)) => {
                    mask_v4(net, *prefix) == mask_v4(&ip, *prefix)
                }
                (std::net::IpAddr::V6(net), std::net::IpAddr::V6(ip)) => {
                    mask_v6(net, *prefix) == mask_v6(&ip, *prefix)
                }
                _ => false,
            })
    }

    /// Whether no proxy is trusted at all (the default configuration).
    pub fn is_empty(&self) -> bool {
        self.networks.is_empty()
    }
}

/// Zeroes the host bits of an IPv4 address, keeping only the `prefix`
/// network bits.
fn mask_v4(addr: &std::net::Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(*addr);
    if prefix == 0 {
        0
    } else {
        bits & (u32::MAX << (32 - prefix as u32))
    }
}

/// Zeroes the host bits of an IPv6 address, keeping only the `prefix`
/// network bits.
fn mask_v6(addr: &std::net::Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(*addr);
    if prefix == 0 {
        0
    } else {
        bits & (u128::MAX << (128 - prefix as u32))
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
    /// Lifetime of a key→channel cache entry, in seconds; 0 disables the
    /// cache. Semantics live in [`crate::cache`].
    pub cache_ttl_secs: u64,
    /// Reverse-proxy networks trusted for `X-Forwarded-For` resolution.
    /// Empty by default: every client is then attributed to its direct
    /// peer address.
    pub trusted_proxies: TrustedProxies,
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
            .field("cache_ttl_secs", &self.cache_ttl_secs)
            .field("trusted_proxies", &self.trusted_proxies)
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
        let cache_ttl_secs = match env::var("HYDRA_CACHE_TTL_SECS") {
            Ok(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|e| ConfigError::Invalid("HYDRA_CACHE_TTL_SECS", e.to_string()))?,
            Err(_) => crate::cache::DEFAULT_TTL_SECS,
        };
        let trusted_proxies = match env::var("HYDRA_TRUSTED_PROXIES") {
            Ok(raw) => TrustedProxies::parse(&raw)
                .map_err(|reason| ConfigError::Invalid("HYDRA_TRUSTED_PROXIES", reason))?,
            Err(_) => TrustedProxies::default(),
        };

        Ok(Self {
            bind_addr,
            db,
            admin_login,
            admin_password,
            log_bodies,
            cache_ttl_secs,
            trusted_proxies,
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
        env::remove_var("HYDRA_CACHE_TTL_SECS");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        let cfg = Config::from_env().unwrap();
        assert_eq!(
            cfg.bind_addr,
            "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(cfg.db, DbTarget::Sqlite("./hydra.db".to_string()));
        assert!(!cfg.log_bodies);
        assert_eq!(cfg.cache_ttl_secs, 3600);
        assert!(
            cfg.trusted_proxies.is_empty(),
            "no trusted proxies by default"
        );
    }

    #[test]
    fn invalid_cache_ttl_is_rejected_with_reason() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::remove_var("HYDRA_DATABASE_URL");
        env::set_var("HYDRA_CACHE_TTL_SECS", "soon");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HYDRA_CACHE_TTL_SECS"));

        env::set_var("HYDRA_CACHE_TTL_SECS", "-1");
        assert!(Config::from_env().is_err());
        env::remove_var("HYDRA_CACHE_TTL_SECS");
    }

    #[test]
    fn zero_cache_ttl_is_accepted_and_disables_the_cache() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::remove_var("HYDRA_DATABASE_URL");
        env::set_var("HYDRA_CACHE_TTL_SECS", "0");
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.cache_ttl_secs, 0);
        env::remove_var("HYDRA_CACHE_TTL_SECS");
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
        env::remove_var("HYDRA_BIND_ADDR");
    }

    #[test]
    fn trusted_proxies_default_to_empty_and_parse_from_the_environment() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("HYDRA_BIND_ADDR");
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::remove_var("HYDRA_DATABASE_URL");
        env::remove_var("HYDRA_TRUSTED_PROXIES");
        let cfg = Config::from_env().unwrap();
        assert!(cfg.trusted_proxies.is_empty());

        env::set_var(
            "HYDRA_TRUSTED_PROXIES",
            "127.0.0.1/32, 10.0.0.0/8, fe80::/10",
        );
        let cfg = Config::from_env().unwrap();
        assert!(cfg.trusted_proxies.contains("127.0.0.1".parse().unwrap()));
        assert!(cfg.trusted_proxies.contains("10.1.2.3".parse().unwrap()));
        assert!(cfg.trusted_proxies.contains("fe80::1".parse().unwrap()));
        assert!(!cfg.trusted_proxies.contains("11.0.0.1".parse().unwrap()));
        env::remove_var("HYDRA_TRUSTED_PROXIES");
    }

    #[test]
    fn invalid_trusted_proxies_abort_naming_the_variable() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var("HYDRA_ADMIN_LOGIN", "admin");
        env::set_var("HYDRA_ADMIN_PASSWORD", "secret");
        env::remove_var("HYDRA_DATABASE_URL");

        for bad in [
            "not-a-cidr",
            "10.0.0.0/40",
            "2001:db8::/200",
            "10.0.0.0/oops",
        ] {
            env::set_var("HYDRA_TRUSTED_PROXIES", bad);
            let err = Config::from_env().unwrap_err();
            assert!(
                err.to_string().contains("HYDRA_TRUSTED_PROXIES"),
                "entry {bad} must abort with the variable named"
            );
        }
        env::remove_var("HYDRA_TRUSTED_PROXIES");
    }

    #[test]
    fn trusted_proxies_match_v4_and_v6_networks_exactly() {
        let proxies = TrustedProxies::parse("10.0.0.0/8, 2001:db8::/32").unwrap();
        assert!(proxies.contains("10.255.255.255".parse().unwrap()));
        assert!(!proxies.contains("11.0.0.0".parse().unwrap()));
        assert!(proxies.contains("2001:db8:dead::1".parse().unwrap()));
        assert!(!proxies.contains("2001:db9::1".parse().unwrap()));

        // A bare address means a single host.
        let single = TrustedProxies::parse("127.0.0.1").unwrap();
        assert!(single.contains("127.0.0.1".parse().unwrap()));
        assert!(!single.contains("127.0.0.2".parse().unwrap()));

        // An empty list trusts nobody.
        let empty = TrustedProxies::parse("").unwrap();
        assert!(empty.is_empty());
        assert!(!empty.contains("127.0.0.1".parse().unwrap()));
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
            cache_ttl_secs: 3600,
            trusted_proxies: TrustedProxies::default(),
        };
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("sekret"));
        assert!(!shown.contains("user@"));
        assert!(!shown.contains("postgres://user"));
        assert!(shown.contains("<redacted>"));
    }
}
