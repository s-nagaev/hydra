//! SQLite pool initialization and migration runner.
//!
//! WAL mode is enabled on every connection so background log inserts do not
//! contend with the single-writer lock against admin reads.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

/// Creates the connection pool for `db_path` (creating the file on first
/// run), enables WAL and foreign keys, and applies all pending migrations.
pub async fn init_pool(db_path: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(db_path)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new().connect_with(options).await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}
