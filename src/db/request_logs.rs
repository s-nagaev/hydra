//! Runtime sqlx queries for the `request_logs` table.
//!
//! Rows are written only by the background logger in `crate::logging`; the
//! insert is fire-and-forget from the client flow's point of view.

use sqlx::SqlitePool;

/// A new request log row. `status` is the upstream HTTP status, or NULL when
/// the request never reached the upstream; `created_at` is stamped by SQLite.
pub struct NewRequestLog {
    pub api_key_id: i64,
    pub channel_id: i64,
    pub model: Option<String>,
    pub status: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub duration_ms: i64,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub error: Option<String>,
}

/// Inserts one log row. The caller runs this on a detached task; failures
/// are reported through tracing, never to the proxied client.
pub async fn insert(pool: &SqlitePool, log: &NewRequestLog) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO request_logs \
         (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
          duration_ms, request_body, response_body, error, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))",
    )
    .bind(log.api_key_id)
    .bind(log.channel_id)
    .bind(&log.model)
    .bind(log.status)
    .bind(log.prompt_tokens)
    .bind(log.completion_tokens)
    .bind(log.duration_ms)
    .bind(&log.request_body)
    .bind(&log.response_body)
    .bind(&log.error)
    .execute(pool)
    .await?;
    Ok(())
}
