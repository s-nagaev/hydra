//! Admin listing of request logs.
//!
//! Read-only view over `request_logs` with pagination and optional filters
//! (key, channel, model, inclusive UTC time window). Bodies are passed
//! through exactly as stored: `request_body`/`response_body` appear in the
//! response only when capture was on for that request. Errors are plain
//! JSON, matching the other admin routes.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::db::request_logs::{self, LogFilters, RequestLog};
use crate::http::admin_auth::AdminAuth;
use crate::http::AppState;

const DEFAULT_PAGE_SIZE: u64 = 50;
const MAX_PAGE_SIZE: u64 = 500;

#[derive(Debug, thiserror::Error)]
pub enum LogsError {
    #[error("{0}")]
    BadRequest(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for LogsError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            LogsError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            LogsError::Database(e) => {
                tracing::error!(error = %e, "admin log query failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

#[derive(Deserialize)]
pub struct LogListQuery {
    #[serde(default)]
    page: Option<u64>,
    #[serde(default)]
    page_size: Option<u64>,
    #[serde(default)]
    key_id: Option<i64>,
    #[serde(default)]
    channel_id: Option<i64>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

/// Listing view; the stored bodies are exposed only when they exist.
#[derive(Serialize)]
pub struct RequestLogView {
    pub id: i64,
    pub api_key_id: i64,
    pub channel_id: i64,
    pub model: Option<String>,
    pub status: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub duration_ms: i64,
    pub error: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

impl From<RequestLog> for RequestLogView {
    fn from(log: RequestLog) -> Self {
        Self {
            id: log.id,
            api_key_id: log.api_key_id,
            channel_id: log.channel_id,
            model: log.model,
            status: log.status,
            prompt_tokens: log.prompt_tokens,
            completion_tokens: log.completion_tokens,
            duration_ms: log.duration_ms,
            error: log.error,
            created_at: log.created_at,
            request_body: log.request_body,
            response_body: log.response_body,
        }
    }
}

#[derive(Serialize)]
pub struct LogListResponse {
    pub items: Vec<RequestLogView>,
    pub page: u64,
    pub page_size: u64,
    pub total: i64,
}

/// Normalizes an RFC3339 timestamp to the `YYYY-MM-DD HH:MM:SS` UTC text
/// format `created_at` stores, so the filter compares correctly and keeps
/// the time-ordered index usable.
fn sqlite_timestamp(raw: &str, label: &str) -> Result<String, LogsError> {
    let parsed = OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|_| LogsError::BadRequest(format!("{label} must be an RFC3339 timestamp")))?;
    let utc = parsed.to_offset(UtcOffset::UTC);
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second()
    ))
}

pub async fn list(
    State(state): State<AppState>,
    _auth: AdminAuth,
    query: Result<Query<LogListQuery>, QueryRejection>,
) -> Result<Json<LogListResponse>, LogsError> {
    let Query(query) = query.map_err(|rejection| LogsError::BadRequest(rejection.body_text()))?;
    let from = query
        .from
        .as_deref()
        .map(|raw| sqlite_timestamp(raw, "from"))
        .transpose()?;
    let to = query
        .to
        .as_deref()
        .map(|raw| sqlite_timestamp(raw, "to"))
        .transpose()?;
    let filters = LogFilters {
        key_id: query.key_id,
        channel_id: query.channel_id,
        model: query.model,
        from,
        to,
        page: query.page.unwrap_or(1).max(1),
        page_size: query
            .page_size
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE),
    };

    let (items, total) = request_logs::list(&state.db, &filters).await?;
    Ok(Json(LogListResponse {
        items: items.into_iter().map(Into::into).collect(),
        page: filters.page,
        page_size: filters.page_size,
        total,
    }))
}
