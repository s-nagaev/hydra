//! Database backends: SQLite (default) and PostgreSQL.
//!
//! One SQL body per query: the fragments are written once and the
//! backend-aware [`Sql`] builder appends the right placeholder (`?` for
//! SQLite, `$n` for PostgreSQL) at each bind. Rows flow through [`Row`],
//! whose [`Fields`] accessors let the per-table mapping code exist once.
//! Both backends keep timestamps as TEXT in the canonical
//! `YYYY-MM-DD HH:MM:SS` UTC format (see [`timestamp`]): the admin log
//! filters string-compare against `created_at`, and that must hold on both.

pub mod channels;
pub mod keys;
pub mod request_logs;
pub mod timestamp;

use std::fmt::Write as _;
use std::str::FromStr;

use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{PgPool, Row as _, SqlitePool};

use crate::config::DbTarget;

/// Creates the connection pool for the configured backend (creating the
/// SQLite file on first run) and applies the matching migrations: WAL mode
/// and foreign keys for SQLite, the `migrations_pg` set for PostgreSQL.
pub async fn init_pool(target: &DbTarget) -> sqlx::Result<Db> {
    match target {
        DbTarget::Sqlite(path) => {
            let options = SqliteConnectOptions::from_str(path)?
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .foreign_keys(true);
            let pool = SqlitePoolOptions::new().connect_with(options).await?;
            sqlx::migrate!().run(&pool).await?;
            Ok(Db::Sqlite(pool))
        }
        DbTarget::Postgres(url) => {
            let pool = PgPoolOptions::new().connect(url).await?;
            sqlx::migrate!("./migrations_pg").run(&pool).await?;
            Ok(Db::Postgres(pool))
        }
    }
}

/// Connection pool for one of the supported backends.
#[derive(Clone, Debug)]
pub enum Db {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

impl From<SqlitePool> for Db {
    fn from(pool: SqlitePool) -> Self {
        Db::Sqlite(pool)
    }
}

impl From<PgPool> for Db {
    fn from(pool: PgPool) -> Self {
        Db::Postgres(pool)
    }
}

impl Db {
    /// The SQLite pool, for call sites that hold one directly. Only valid
    /// on the SQLite backend; panics otherwise so a mixup surfaces in
    /// development instead of silently querying the wrong database.
    pub fn as_sqlite(&self) -> &SqlitePool {
        match self {
            Db::Sqlite(pool) => pool,
            Db::Postgres(_) => panic!("as_sqlite called on the PostgreSQL backend"),
        }
    }

    /// The PostgreSQL pool when this is the PostgreSQL backend.
    pub fn as_postgres(&self) -> Option<&PgPool> {
        match self {
            Db::Sqlite(_) => None,
            Db::Postgres(pool) => Some(pool),
        }
    }

    /// Runs a single-row query.
    pub async fn fetch_one(&self, sql: Sql) -> sqlx::Result<Row> {
        let (text, args) = sql.into_parts();
        match self {
            Db::Sqlite(pool) => Ok(Row::Sqlite(
                sqlite_exec::fetch_one(pool, &text, args).await?,
            )),
            Db::Postgres(pool) => Ok(Row::Postgres(pg_exec::fetch_one(pool, &text, args).await?)),
        }
    }

    /// Runs a query returning zero or one row.
    pub async fn fetch_optional(&self, sql: Sql) -> sqlx::Result<Option<Row>> {
        let (text, args) = sql.into_parts();
        match self {
            Db::Sqlite(pool) => Ok(sqlite_exec::fetch_optional(pool, &text, args)
                .await?
                .map(Row::Sqlite)),
            Db::Postgres(pool) => Ok(pg_exec::fetch_optional(pool, &text, args)
                .await?
                .map(Row::Postgres)),
        }
    }

    /// Runs a query returning many rows.
    pub async fn fetch_all(&self, sql: Sql) -> sqlx::Result<Vec<Row>> {
        let (text, args) = sql.into_parts();
        match self {
            Db::Sqlite(pool) => Ok(sqlite_exec::fetch_all(pool, &text, args)
                .await?
                .into_iter()
                .map(Row::Sqlite)
                .collect()),
            Db::Postgres(pool) => Ok(pg_exec::fetch_all(pool, &text, args)
                .await?
                .into_iter()
                .map(Row::Postgres)
                .collect()),
        }
    }

    /// Single-value integer query (counts, existence probes).
    pub async fn scalar(&self, sql: Sql) -> sqlx::Result<i64> {
        let (text, args) = sql.into_parts();
        match self {
            Db::Sqlite(pool) => sqlite_exec::scalar(pool, &text, args).await,
            Db::Postgres(pool) => pg_exec::scalar(pool, &text, args).await,
        }
    }

    /// Single-value integer query that may return no row.
    pub async fn scalar_optional(&self, sql: Sql) -> sqlx::Result<Option<i64>> {
        let (text, args) = sql.into_parts();
        match self {
            Db::Sqlite(pool) => sqlite_exec::scalar_optional(pool, &text, args).await,
            Db::Postgres(pool) => pg_exec::scalar_optional(pool, &text, args).await,
        }
    }

    /// Runs a statement and discards its result rows.
    pub async fn execute(&self, sql: Sql) -> sqlx::Result<()> {
        let (text, args) = sql.into_parts();
        match self {
            Db::Sqlite(pool) => sqlite_exec::execute(pool, &text, args).await,
            Db::Postgres(pool) => pg_exec::execute(pool, &text, args).await,
        }
    }
}

/// A value bound into a shared SQL body. Booleans travel as bools and the
/// backend stores them natively (SQLite keeps them as 0/1 integers).
#[derive(Debug, Clone)]
pub enum Arg {
    Int(i64),
    OptInt(Option<i64>),
    Text(Option<String>),
    Bool(bool),
}

/// Builds one SQL body plus its ordered bind values for a specific backend.
/// `bind` appends the backend's placeholder at the bind site, so the query
/// functions never carry dialect-specific SQL.
#[derive(Debug)]
pub struct Sql {
    numbered_placeholders: bool,
    text: String,
    args: Vec<Arg>,
}

impl Sql {
    /// Builder matching the backend the pool belongs to.
    pub fn for_db(db: &Db) -> Self {
        Sql {
            numbered_placeholders: matches!(db, Db::Postgres(_)),
            text: String::new(),
            args: Vec::new(),
        }
    }

    /// Appends a static SQL fragment. Placeholders only enter via [`bind`].
    pub fn push(&mut self, fragment: &str) -> &mut Self {
        self.text.push_str(fragment);
        self
    }

    /// Appends the backend's placeholder and records the value in bind
    /// order.
    pub fn bind(&mut self, arg: Arg) -> &mut Self {
        if self.numbered_placeholders {
            let _ = write!(self.text, "${}", self.args.len() + 1);
        } else {
            self.text.push('?');
        }
        self.args.push(arg);
        self
    }

    fn into_parts(self) -> (String, Vec<Arg>) {
        (self.text, self.args)
    }
}

/// A row from either backend, exposing exactly the field types the table
/// mappers read: TEXT, INTEGER/BIGINT and BOOLEAN.
pub enum Row {
    Sqlite(sqlx::sqlite::SqliteRow),
    Postgres(sqlx::postgres::PgRow),
}

/// Field access shared by the backend row types, so mapping code is written
/// once per table, not once per backend.
pub trait Fields {
    fn text(&self, name: &str) -> sqlx::Result<String>;
    fn opt_text(&self, name: &str) -> sqlx::Result<Option<String>>;
    fn int(&self, name: &str) -> sqlx::Result<i64>;
    fn opt_int(&self, name: &str) -> sqlx::Result<Option<i64>>;
    fn boolean(&self, name: &str) -> sqlx::Result<bool>;
}

impl Fields for Row {
    fn text(&self, name: &str) -> sqlx::Result<String> {
        match self {
            Row::Sqlite(row) => row.try_get(name),
            Row::Postgres(row) => row.try_get(name),
        }
    }

    fn opt_text(&self, name: &str) -> sqlx::Result<Option<String>> {
        match self {
            Row::Sqlite(row) => row.try_get(name),
            Row::Postgres(row) => row.try_get(name),
        }
    }

    fn int(&self, name: &str) -> sqlx::Result<i64> {
        match self {
            Row::Sqlite(row) => row.try_get(name),
            Row::Postgres(row) => row.try_get(name),
        }
    }

    fn opt_int(&self, name: &str) -> sqlx::Result<Option<i64>> {
        match self {
            Row::Sqlite(row) => row.try_get(name),
            Row::Postgres(row) => row.try_get(name),
        }
    }

    fn boolean(&self, name: &str) -> sqlx::Result<bool> {
        match self {
            Row::Sqlite(row) => row.try_get(name),
            Row::Postgres(row) => row.try_get(name),
        }
    }
}

/// Binds an ordered list of [`Arg`]s into any sqlx query shape (`Query` or
/// `QueryScalar`); the backend is fixed by the query's own type.
macro_rules! bind_values {
    ($query:expr, $args:expr) => {{
        let mut query = $query;
        for arg in $args {
            query = match arg {
                super::Arg::Int(v) => query.bind(v),
                super::Arg::OptInt(v) => query.bind(v),
                super::Arg::Text(v) => query.bind(v),
                super::Arg::Bool(v) => query.bind(v),
            };
        }
        query
    }};
}

/// Generates the per-backend execution module: identical shapes, only the
/// pool, row and bind types differ.
macro_rules! executors {
    ($name:ident, $pool:ty, $row:ty) => {
        mod $name {

            pub(super) async fn fetch_one(
                pool: &$pool,
                text: &str,
                args: Vec<super::Arg>,
            ) -> sqlx::Result<$row> {
                let query = bind_values!(sqlx::query(text), args);
                query.fetch_one(pool).await
            }

            pub(super) async fn fetch_optional(
                pool: &$pool,
                text: &str,
                args: Vec<super::Arg>,
            ) -> sqlx::Result<Option<$row>> {
                let query = bind_values!(sqlx::query(text), args);
                query.fetch_optional(pool).await
            }

            pub(super) async fn fetch_all(
                pool: &$pool,
                text: &str,
                args: Vec<super::Arg>,
            ) -> sqlx::Result<Vec<$row>> {
                let query = bind_values!(sqlx::query(text), args);
                query.fetch_all(pool).await
            }

            pub(super) async fn scalar(
                pool: &$pool,
                text: &str,
                args: Vec<super::Arg>,
            ) -> sqlx::Result<i64> {
                let query = bind_values!(sqlx::query_scalar(text), args);
                query.fetch_one(pool).await
            }

            pub(super) async fn scalar_optional(
                pool: &$pool,
                text: &str,
                args: Vec<super::Arg>,
            ) -> sqlx::Result<Option<i64>> {
                let query = bind_values!(sqlx::query_scalar(text), args);
                query.fetch_optional(pool).await
            }

            pub(super) async fn execute(
                pool: &$pool,
                text: &str,
                args: Vec<super::Arg>,
            ) -> sqlx::Result<()> {
                let query = bind_values!(sqlx::query(text), args);
                query.execute(pool).await?;
                Ok(())
            }
        }
    };
}

executors!(sqlite_exec, sqlx::SqlitePool, sqlx::sqlite::SqliteRow);
executors!(pg_exec, sqlx::PgPool, sqlx::postgres::PgRow);
