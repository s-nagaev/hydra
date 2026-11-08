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
use utoipa::{IntoParams, ToSchema};

use crate::db::request_logs::{self, LogFilters, RequestLog, StatusClass};
use crate::http::admin_auth::AdminAuth;
use crate::http::docs::AdminErrorBody;
use crate::http::AppState;

/// Default rows per page, shared with the admin UI logs page.
pub(crate) const DEFAULT_PAGE_SIZE: u64 = 50;
const MAX_PAGE_SIZE: u64 = 500;

/// Valid values of the status-class filter, phrased for error messages.
pub(crate) const STATUS_CLASS_VALUES: &str = "`2xx`, `4xx`, `5xx`, `errors`";

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

// The route template has no {param} placeholders; without the explicit
// location utoipa's IntoParams default (path) would document the filters as
// required path parameters that Swagger UI cannot even send.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogListQuery {
    /// Result page, 1-based; values below 1 clamp to 1.
    #[serde(default)]
    #[param(example = 1)]
    page: Option<u64>,
    /// Rows per page; default 50, capped at 500.
    #[serde(default)]
    #[param(example = 50)]
    page_size: Option<u64>,
    /// Only rows produced by this key.
    #[serde(default)]
    #[param(example = 1)]
    key_id: Option<i64>,
    /// Only rows routed through this channel.
    #[serde(default)]
    #[param(example = 1)]
    channel_id: Option<i64>,
    /// Only rows that requested this model.
    #[serde(default)]
    #[param(example = "gpt-4o")]
    model: Option<String>,
    /// Coarse status bucket: `2xx`, `4xx`, `5xx` or `errors` (which also
    /// includes rows that never reached the upstream, i.e. null status).
    #[serde(default)]
    #[param(example = "2xx")]
    status_class: Option<String>,
    /// Inclusive lower bound, RFC3339; compared as UTC.
    #[serde(default)]
    #[param(example = "2026-11-01T00:00:00Z")]
    from: Option<String>,
    /// Inclusive upper bound, RFC3339; compared as UTC.
    #[serde(default)]
    #[param(example = "2026-11-07T23:59:59Z")]
    to: Option<String>,
}

/// Listing view; the stored bodies are exposed only when they exist.
#[derive(Serialize, ToSchema)]
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

#[derive(Serialize, ToSchema)]
pub struct LogListResponse {
    pub items: Vec<RequestLogView>,
    pub page: u64,
    pub page_size: u64,
    pub total: i64,
}

/// Raw query values of the listing, shared by the JSON endpoint and the
/// admin UI logs page so both validate and normalize identically.
pub(crate) struct RawLogQuery {
    pub page: Option<u64>,
    pub page_size: Option<u64>,
    pub key_id: Option<i64>,
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub status_class: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Validates the raw values and turns them into db-layer filters: time
/// bounds are normalized to the stored UTC text format, the status class
/// must be a known bucket, and page/page_size get defaults and caps.
pub(crate) fn build_filters(query: RawLogQuery) -> Result<LogFilters, LogsError> {
    let status_class = match query.status_class.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(StatusClass::parse(raw).ok_or_else(|| {
            LogsError::BadRequest(format!("status_class must be one of {STATUS_CLASS_VALUES}"))
        })?),
    };
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
    Ok(LogFilters {
        key_id: query.key_id,
        channel_id: query.channel_id,
        model: query.model,
        status_class,
        from,
        to,
        page: query.page.unwrap_or(1).max(1),
        page_size: query
            .page_size
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE),
    })
}

/// Normalizes an RFC3339 timestamp to the `YYYY-MM-DD HH:MM:SS` UTC text
/// format `created_at` stores, so the filter compares correctly and keeps
/// the time-ordered index usable.
fn sqlite_timestamp(raw: &str, label: &str) -> Result<String, LogsError> {
    let parsed = OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|_| LogsError::BadRequest(format!("{label} must be an RFC3339 timestamp")))?;
    Ok(crate::db::timestamp::format_utc(
        parsed.to_offset(UtcOffset::UTC),
    ))
}

#[utoipa::path(
    get,
    path = "/admin/logs",
    tag = "Admin",
    operation_id = "list_request_logs",
    summary = "List request logs",
    description = "Reads the stored request audit rows, newest first, with \
        pagination and optional filters. `request_body` and `response_body` \
        are present on a row only when body capture (HYDRA_LOG_BODIES) was \
        on for that request; a row that never reached the upstream has a \
        null `status` and its failure in `error`. `status_class` buckets \
        rows by upstream status (`2xx`, `4xx`, `5xx`, or `errors`, which \
        also includes the rows with a null status).",
    security(("basic_auth" = [])),
    params(LogListQuery),
    responses(
        (status = 200, description = "One page of log rows", body = LogListResponse, example = json!({
            "items": [
                {
                    "id": 42,
                    "api_key_id": 1,
                    "channel_id": 1,
                    "model": "gpt-4o",
                    "status": 200,
                    "prompt_tokens": 12,
                    "completion_tokens": 34,
                    "duration_ms": 845,
                    "error": null,
                    "created_at": "2026-11-07 23:59:59"
                }
            ],
            "page": 1,
            "page_size": 50,
            "total": 128
        })),
        (status = 400, description = "A query parameter has a bad value, \
            e.g. a non-RFC3339 time bound or a non-integer page", body = AdminErrorBody, example = json!({
            "error": "from must be an RFC3339 timestamp"
        })),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body")
    )
)]
pub async fn list(
    State(state): State<AppState>,
    _auth: AdminAuth,
    query: Result<Query<LogListQuery>, QueryRejection>,
) -> Result<Json<LogListResponse>, LogsError> {
    let Query(query) = query.map_err(|rejection| LogsError::BadRequest(rejection.body_text()))?;
    let filters = build_filters(RawLogQuery {
        page: query.page,
        page_size: query.page_size,
        key_id: query.key_id,
        channel_id: query.channel_id,
        model: query.model,
        status_class: query.status_class,
        from: query.from,
        to: query.to,
    })?;

    let (items, total) = request_logs::list(&state.db, &filters).await?;
    Ok(Json(LogListResponse {
        items: items.into_iter().map(Into::into).collect(),
        page: filters.page,
        page_size: filters.page_size,
        total,
    }))
}
