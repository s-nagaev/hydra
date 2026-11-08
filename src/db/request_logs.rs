//! Runtime sqlx queries for the `request_logs` table.
//!
//! Rows are written only by the background logger in `crate::logging`; the
//! insert is fire-and-forget from the client flow's point of view.
//!
//! One SQL body per query: fragments are written against the backend-aware
//! [`Sql`] builder, which picks the placeholder per backend.

use super::timestamp;
use super::{Arg, Db, Fields, Row, Sql};

/// A new request log row. `status` is the upstream HTTP status, or NULL when
/// the request never reached the upstream; `created_at` is stamped in the
/// canonical UTC text format at insert time.
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
    pub client_ip: Option<String>,
}

/// Inserts one log row. The caller runs this on a detached task; failures
/// are reported through tracing, never to the proxied client.
pub async fn insert(db: &Db, log: &NewRequestLog) -> sqlx::Result<()> {
    let mut sql = Sql::for_db(db);
    sql.push("INSERT INTO request_logs (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, duration_ms, request_body, response_body, error, client_ip, created_at) VALUES (")
        .bind(Arg::Int(log.api_key_id))
        .push(", ")
        .bind(Arg::Int(log.channel_id))
        .push(", ")
        .bind(Arg::Text(log.model.clone()))
        .push(", ")
        .bind(Arg::OptInt(log.status))
        .push(", ")
        .bind(Arg::OptInt(log.prompt_tokens))
        .push(", ")
        .bind(Arg::OptInt(log.completion_tokens))
        .push(", ")
        .bind(Arg::Int(log.duration_ms))
        .push(", ")
        .bind(Arg::Text(log.request_body.clone()))
        .push(", ")
        .bind(Arg::Text(log.response_body.clone()))
        .push(", ")
        .bind(Arg::Text(log.error.clone()))
        .push(", ")
        .bind(Arg::Text(log.client_ip.clone()))
        .push(", ")
        .bind(Arg::Text(Some(timestamp::now_utc())))
        .push(")");
    db.execute(sql).await?;
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
    pub client_ip: Option<String>,
    pub created_at: String,
}

/// Coarse bucket over the upstream HTTP status, used by the `status_class`
/// filter of the admin listing. A row whose request never reached the
/// upstream (null status) counts as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Ok2xx,
    Client4xx,
    Server5xx,
    Errors,
}

impl StatusClass {
    /// Parses the URL value; `None` means the value is not a known bucket.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "2xx" => Some(Self::Ok2xx),
            "4xx" => Some(Self::Client4xx),
            "5xx" => Some(Self::Server5xx),
            "errors" => Some(Self::Errors),
            _ => None,
        }
    }
}

/// Filters for the admin listing; every field is optional. `from` and `to`
/// are inclusive bounds already normalized to the `YYYY-MM-DD HH:MM:SS` UTC
/// text format the `created_at` column stores, so string comparison against
/// the column is correct and can use the time-ordered index.
pub struct LogFilters {
    pub key_id: Option<i64>,
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub status_class: Option<StatusClass>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub page: u64,
    pub page_size: u64,
}

/// Adds the filter conditions to a SQL body. Values always travel as bind
/// parameters; user input never lands in the SQL text itself.
fn push_conditions(sql: &mut Sql, filters: &LogFilters) {
    sql.push("1=1");
    if let Some(key_id) = filters.key_id {
        sql.push(" AND api_key_id = ").bind(Arg::Int(key_id));
    }
    if let Some(channel_id) = filters.channel_id {
        sql.push(" AND channel_id = ").bind(Arg::Int(channel_id));
    }
    if let Some(model) = &filters.model {
        sql.push(" AND model = ")
            .bind(Arg::Text(Some(model.clone())));
    }
    if let Some(status_class) = filters.status_class {
        match status_class {
            StatusClass::Ok2xx => {
                sql.push(" AND status >= ").bind(Arg::Int(200));
                sql.push(" AND status < ").bind(Arg::Int(300));
            }
            StatusClass::Client4xx => {
                sql.push(" AND status >= ").bind(Arg::Int(400));
                sql.push(" AND status < ").bind(Arg::Int(500));
            }
            StatusClass::Server5xx => {
                sql.push(" AND status >= ").bind(Arg::Int(500));
                sql.push(" AND status < ").bind(Arg::Int(600));
            }
            // Failed transports have a null status and still count.
            StatusClass::Errors => {
                sql.push(" AND (status IS NULL OR status >= ")
                    .bind(Arg::Int(400));
                sql.push(")");
            }
        }
    }
    if let Some(from) = &filters.from {
        sql.push(" AND created_at >= ")
            .bind(Arg::Text(Some(from.clone())));
    }
    if let Some(to) = &filters.to {
        sql.push(" AND created_at <= ")
            .bind(Arg::Text(Some(to.clone())));
    }
}

fn map_row(row: &Row) -> sqlx::Result<RequestLog> {
    Ok(RequestLog {
        id: row.int("id")?,
        api_key_id: row.int("api_key_id")?,
        channel_id: row.int("channel_id")?,
        model: row.opt_text("model")?,
        status: row.opt_int("status")?,
        prompt_tokens: row.opt_int("prompt_tokens")?,
        completion_tokens: row.opt_int("completion_tokens")?,
        duration_ms: row.int("duration_ms")?,
        request_body: row.opt_text("request_body")?,
        response_body: row.opt_text("response_body")?,
        error: row.opt_text("error")?,
        client_ip: row.opt_text("client_ip")?,
        created_at: row.text("created_at")?,
    })
}

/// One count query plus one page query, both sharing the same dynamically
/// built WHERE clause. Rows are ordered by id DESC (newest first).
pub async fn list(db: &Db, filters: &LogFilters) -> sqlx::Result<(Vec<RequestLog>, i64)> {
    let mut count = Sql::for_db(db);
    count.push("SELECT COUNT(*) FROM request_logs WHERE ");
    push_conditions(&mut count, filters);
    let total = db.scalar(count).await?;

    let mut page = Sql::for_db(db);
    page.push(
        "SELECT id, api_key_id, channel_id, model, status, prompt_tokens, \
         completion_tokens, duration_ms, request_body, response_body, error, \
         client_ip, created_at \
         FROM request_logs WHERE ",
    );
    push_conditions(&mut page, filters);
    page.push(" ORDER BY id DESC LIMIT ")
        .bind(Arg::Int(filters.page_size as i64))
        .push(" OFFSET ")
        .bind(Arg::Int(
            filters
                .page
                .saturating_sub(1)
                .saturating_mul(filters.page_size)
                .min(i64::MAX as u64) as i64,
        ));
    let rows = db.fetch_all(page).await?;
    let items = rows.iter().map(map_row).collect::<sqlx::Result<Vec<_>>>()?;
    Ok((items, total))
}
