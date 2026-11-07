//! Runtime sqlx queries for the `request_logs` table.
//!
//! Rows are written only by the background logger in `crate::logging`; the
//! insert is fire-and-forget from the client flow's point of view.

use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::{QueryBuilder, Row};

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

/// One stored request log row, as returned by the admin listing.
#[derive(Debug)]
pub struct RequestLog {
    pub id: i64,
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
    pub created_at: String,
}

/// Filters for the admin listing; every field is optional. `from` and `to`
/// are inclusive bounds already normalized to the `YYYY-MM-DD HH:MM:SS` UTC
/// text format the `created_at` column stores, so string comparison against
/// the column is correct and can use the time-ordered index.
pub struct LogFilters {
    pub key_id: Option<i64>,
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub page: u64,
    pub page_size: u64,
}

/// Adds the filter conditions to a query builder. Values always travel as
/// bind parameters; user input never lands in the SQL text itself.
fn push_conditions(builder: &mut QueryBuilder<'_, sqlx::Sqlite>, filters: &LogFilters) {
    builder.push("1=1");
    if let Some(key_id) = filters.key_id {
        builder.push(" AND api_key_id = ").push_bind(key_id);
    }
    if let Some(channel_id) = filters.channel_id {
        builder.push(" AND channel_id = ").push_bind(channel_id);
    }
    if let Some(model) = &filters.model {
        builder.push(" AND model = ").push_bind(model.clone());
    }
    if let Some(from) = &filters.from {
        builder.push(" AND created_at >= ").push_bind(from.clone());
    }
    if let Some(to) = &filters.to {
        builder.push(" AND created_at <= ").push_bind(to.clone());
    }
}

fn map_row(row: &SqliteRow) -> sqlx::Result<RequestLog> {
    Ok(RequestLog {
        id: row.try_get("id")?,
        api_key_id: row.try_get("api_key_id")?,
        channel_id: row.try_get("channel_id")?,
        model: row.try_get("model")?,
        status: row.try_get("status")?,
        prompt_tokens: row.try_get("prompt_tokens")?,
        completion_tokens: row.try_get("completion_tokens")?,
        duration_ms: row.try_get("duration_ms")?,
        request_body: row.try_get("request_body")?,
        response_body: row.try_get("response_body")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
    })
}

/// One count query plus one page query, both sharing the same dynamically
/// built WHERE clause. Rows are ordered by id DESC (newest first).
pub async fn list(pool: &SqlitePool, filters: &LogFilters) -> sqlx::Result<(Vec<RequestLog>, i64)> {
    let mut count_builder = QueryBuilder::new("SELECT COUNT(*) FROM request_logs WHERE ");
    push_conditions(&mut count_builder, filters);
    let total: i64 = count_builder.build_query_scalar().fetch_one(pool).await?;

    let mut page_builder = QueryBuilder::new(
        "SELECT id, api_key_id, channel_id, model, status, prompt_tokens, \
         completion_tokens, duration_ms, request_body, response_body, error, created_at \
         FROM request_logs WHERE ",
    );
    push_conditions(&mut page_builder, filters);
    page_builder
        .push(" ORDER BY id DESC LIMIT ")
        .push_bind(filters.page_size as i64)
        .push(" OFFSET ")
        .push_bind(
            filters
                .page
                .saturating_sub(1)
                .saturating_mul(filters.page_size)
                .min(i64::MAX as u64) as i64,
        );
    let rows = page_builder.build().fetch_all(pool).await?;
    let items = rows.iter().map(map_row).collect::<sqlx::Result<Vec<_>>>()?;
    Ok((items, total))
}
