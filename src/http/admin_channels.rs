//! Admin CRUD handlers for channels ("redirects").
//!
//! Error style: admin routes use plain JSON bodies `{"error": "..."}`, not
//! the OpenAI shape reserved for proxy routes. Status mapping: 400 invalid
//! input, 401 handled by `admin_auth`, 404 unknown channel, 409 channel has
//! bound API keys, 500 internal storage failure.
//!
//! Security invariant: the upstream `api_key` never appears in a response.
//! Handlers return `ChannelView`, which carries only a masked preview and a
//! `has_api_key` flag.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use url::Url;
use utoipa::ToSchema;

use crate::db::channels::{self, Channel, ChannelPatch, DeleteOutcome, NewChannel};
use crate::http::admin_auth::AdminAuth;
use crate::http::docs::AdminErrorBody;
use crate::http::AppState;

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("channel not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AdminError::NotFound => (StatusCode::NOT_FOUND, "channel not found".to_string()),
            AdminError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            AdminError::Conflict(message) => (StatusCode::CONFLICT, message),
            AdminError::Database(e) => {
                tracing::error!(error = %e, "admin channel query failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

/// Body for creating a channel.
#[derive(Deserialize, ToSchema)]
pub struct CreateChannelRequest {
    /// Human-readable channel label, shown in listings and logs.
    #[schema(example = "openai")]
    pub name: String,
    /// Absolute http(s) URL of the upstream; the API path is appended.
    #[schema(example = "https://api.openai.com")]
    pub base_url: String,
    /// Upstream API key, stored server-side and never returned in full.
    #[schema(example = "sk-upstream-secret-1234567890")]
    pub api_key: String,
    /// Exact-match model whitelist. Omitted or null allows every model the
    /// upstream serves; an empty array also means allow-all.
    #[serde(default)]
    #[schema(example = json!(["gpt-4o", "gpt-4o-mini"]))]
    pub allowed_models: Option<Vec<String>>,
}

/// Body for editing a channel; only the sent fields change.
#[derive(Deserialize, ToSchema)]
pub struct PatchChannelRequest {
    #[serde(default)]
    #[schema(example = "renamed")]
    pub name: Option<String>,
    #[serde(default)]
    #[schema(example = "https://api.openai.com/v1")]
    pub base_url: Option<String>,
    #[serde(default)]
    #[schema(example = "sk-new-upstream-secret-1234567890")]
    pub api_key: Option<String>,
    /// `null` (JSON null) clears the whitelist back to allow-all; the field
    /// being absent leaves it unchanged.
    #[serde(default, deserialize_with = "double_option")]
    pub allowed_models: Option<Option<Vec<String>>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Public view of a channel; the upstream key is masked.
#[derive(Serialize, ToSchema)]
pub struct ChannelView {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key_masked: String,
    pub has_api_key: bool,
    pub allowed_models: Option<Vec<String>>,
    pub enabled: bool,
    pub created_at: String,
}

impl From<Channel> for ChannelView {
    fn from(channel: Channel) -> Self {
        Self {
            id: channel.id,
            name: channel.name,
            base_url: channel.base_url,
            api_key_masked: mask_api_key(&channel.api_key),
            has_api_key: true,
            allowed_models: channel.allowed_models,
            enabled: channel.enabled,
            created_at: channel.created_at,
        }
    }
}

/// Masks an upstream key for display: first and last four characters around
/// an ellipsis, or full asterisks when the key is too short to halve safely.
/// Shared by the JSON views and the admin UI table.
pub(crate) fn mask_api_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 8 {
        return "*".repeat(len);
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(len - 4).collect();
    format!("{head}...{tail}")
}

/// Channel name must be a non-empty-after-trim label. Shared by the JSON
/// handlers and the admin UI form.
pub(crate) fn validate_channel_name(name: &str) -> Result<(), AdminError> {
    if name.trim().is_empty() {
        return Err(AdminError::BadRequest("name must not be empty".to_string()));
    }
    Ok(())
}

/// Upstream key must be non-empty after trim. Shared by the JSON handlers
/// and the admin UI form.
pub(crate) fn validate_channel_api_key(api_key: &str) -> Result<(), AdminError> {
    if api_key.trim().is_empty() {
        return Err(AdminError::BadRequest(
            "api_key must not be empty".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_base_url(raw: &str) -> Result<(), AdminError> {
    let parsed =
        Url::parse(raw).map_err(|_| AdminError::BadRequest(format!("invalid base_url: {raw}")))?;
    if parsed.cannot_be_a_base() || !matches!(parsed.scheme(), "http" | "https") {
        return Err(AdminError::BadRequest(format!(
            "base_url must be an absolute http(s) URL: {raw}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_allowed_models(
    models: Option<&[String]>,
) -> Result<Option<Vec<String>>, AdminError> {
    let Some(models) = models else {
        return Ok(None);
    };
    if models.iter().any(|m| m.trim().is_empty()) {
        return Err(AdminError::BadRequest(
            "allowed_models must not contain empty strings".to_string(),
        ));
    }
    // An empty whitelist means "allow all" per plan semantics.
    if models.is_empty() {
        Ok(None)
    } else {
        Ok(Some(models.to_vec()))
    }
}

fn validate_common(
    name: &str,
    base_url: &str,
    api_key: &str,
    allowed_models: Option<&[String]>,
) -> Result<Option<Vec<String>>, AdminError> {
    validate_channel_name(name)?;
    validate_channel_api_key(api_key)?;
    validate_base_url(base_url)?;
    validate_allowed_models(allowed_models)
}

#[utoipa::path(
    post,
    path = "/admin/channels",
    tag = "Admin",
    operation_id = "create_channel",
    summary = "Create a channel",
    description = "Registers an upstream provider that client keys can be \
        bound to. The upstream key is stored server-side; every response \
        returns only a masked preview, never the full secret. New channels \
        start enabled.",
    security(("basic_auth" = [])),
    request_body(
        content = CreateChannelRequest,
        description = "Channel definition. Omit `allowed_models` to allow \
            every model the upstream serves.",
        examples(
            ("whitelist" = (
                summary = "Channel restricted to two models",
                value = json!({
                    "name": "openai",
                    "base_url": "https://api.openai.com",
                    "api_key": "sk-upstream-secret-1234567890",
                    "allowed_models": ["gpt-4o", "gpt-4o-mini"]
                })
            )),
            ("allow_all" = (
                summary = "Channel serving every upstream model",
                value = json!({
                    "name": "openai",
                    "base_url": "https://api.openai.com",
                    "api_key": "sk-upstream-secret-1234567890"
                })
            )),
            ("invalid" = (
                summary = "Rejected with 400: base_url is not a URL",
                value = json!({
                    "name": "broken",
                    "base_url": "not a url",
                    "api_key": "sk-upstream-secret-1234567890"
                })
            ))
        )
    ),
    responses(
        (status = 201, description = "Channel created", body = ChannelView, example = json!({
            "id": 1,
            "name": "openai",
            "base_url": "https://api.openai.com",
            "api_key_masked": "sk-u...7890",
            "has_api_key": true,
            "allowed_models": ["gpt-4o", "gpt-4o-mini"],
            "enabled": true,
            "created_at": "2026-11-07 23:59:59"
        })),
        (status = 400, description = "Invalid input: empty name or api_key, \
            a non-absolute or non-http base_url, or an empty string inside \
            allowed_models", body = AdminErrorBody, example = json!({
            "error": "invalid base_url: not a url"
        })),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body")
    )
)]
pub async fn create(
    State(state): State<AppState>,
    _auth: AdminAuth,
    payload: Result<Json<CreateChannelRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ChannelView>), AdminError> {
    let Json(request) =
        payload.map_err(|rejection| AdminError::BadRequest(rejection.body_text()))?;
    let allowed_models = validate_common(
        &request.name,
        &request.base_url,
        &request.api_key,
        request.allowed_models.as_deref(),
    )?;
    let channel = channels::create(
        &state.db,
        &NewChannel {
            name: request.name,
            base_url: request.base_url,
            api_key: request.api_key,
            allowed_models,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(channel.into())))
}

#[utoipa::path(
    get,
    path = "/admin/channels",
    tag = "Admin",
    operation_id = "list_channels",
    summary = "List channels",
    description = "Returns every channel, including disabled ones, with the \
        upstream key masked in each row.",
    security(("basic_auth" = [])),
    responses(
        (status = 200, description = "All channels, oldest first", body = Vec<ChannelView>, example = json!([
            {
                "id": 1,
                "name": "openai",
                "base_url": "https://api.openai.com",
                "api_key_masked": "sk-u...7890",
                "has_api_key": true,
                "allowed_models": null,
                "enabled": true,
                "created_at": "2026-11-07 23:59:59"
            }
        ])),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body")
    )
)]
pub async fn list(
    State(state): State<AppState>,
    _auth: AdminAuth,
) -> Result<Json<Vec<ChannelView>>, AdminError> {
    let channel_list = channels::list(&state.db).await?;
    Ok(Json(channel_list.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    get,
    path = "/admin/channels/{id}",
    tag = "Admin",
    operation_id = "get_channel",
    summary = "Fetch one channel",
    description = "Returns the channel by id, with the upstream key masked.",
    security(("basic_auth" = [])),
    params(("id" = i64, Path, description = "Channel id", example = 1)),
    responses(
        (status = 200, description = "The channel", body = ChannelView, example = json!({
            "id": 1,
            "name": "openai",
            "base_url": "https://api.openai.com",
            "api_key_masked": "sk-u...7890",
            "has_api_key": true,
            "allowed_models": ["gpt-4o", "gpt-4o-mini"],
            "enabled": true,
            "created_at": "2026-11-07 23:59:59"
        })),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body"),
        (status = 404, description = "No channel with this id", body = AdminErrorBody, example = json!({
            "error": "channel not found"
        }))
    )
)]
pub async fn get_one(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
) -> Result<Json<ChannelView>, AdminError> {
    let channel = channels::find(&state.db, id)
        .await?
        .ok_or(AdminError::NotFound)?;
    Ok(Json(channel.into()))
}

#[utoipa::path(
    patch,
    path = "/admin/channels/{id}",
    tag = "Admin",
    operation_id = "patch_channel",
    summary = "Edit a channel",
    description = "Updates only the fields present in the body. Sending JSON \
        null as `allowed_models` clears the whitelist back to allow-all; \
        an empty array does the same. This is also how a channel is enabled \
        or disabled.",
    security(("basic_auth" = [])),
    params(("id" = i64, Path, description = "Channel id", example = 1)),
    request_body(
        content = PatchChannelRequest,
        description = "Fields to change; everything absent stays as it is.",
        examples(
            ("rename_and_restrict" = (
                summary = "Rename and set a whitelist",
                value = json!({
                    "name": "renamed",
                    "allowed_models": ["gpt-4o"],
                    "enabled": false
                })
            )),
            ("clear_whitelist" = (
                summary = "Clear the whitelist with an explicit null",
                value = json!({ "allowed_models": null })
            )),
            ("invalid" = (
                summary = "Rejected with 400: ftp scheme",
                value = json!({ "base_url": "ftp://api.example.com" })
            ))
        )
    ),
    responses(
        (status = 200, description = "The updated channel", body = ChannelView, example = json!({
            "id": 1,
            "name": "renamed",
            "base_url": "https://api.openai.com",
            "api_key_masked": "sk-u...7890",
            "has_api_key": true,
            "allowed_models": ["gpt-4o"],
            "enabled": false,
            "created_at": "2026-11-07 23:59:59"
        })),
        (status = 400, description = "Invalid field value, same rules as on \
            create", body = AdminErrorBody, example = json!({
            "error": "base_url must be an absolute http(s) URL: ftp://api.example.com"
        })),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body"),
        (status = 404, description = "No channel with this id", body = AdminErrorBody, example = json!({
            "error": "channel not found"
        }))
    )
)]
pub async fn patch(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
    payload: Result<Json<PatchChannelRequest>, JsonRejection>,
) -> Result<Json<ChannelView>, AdminError> {
    let Json(request) =
        payload.map_err(|rejection| AdminError::BadRequest(rejection.body_text()))?;
    if let Some(name) = &request.name {
        validate_channel_name(name)?;
    }
    if let Some(api_key) = &request.api_key {
        validate_channel_api_key(api_key)?;
    }
    if let Some(base_url) = &request.base_url {
        validate_base_url(base_url)?;
    }
    let allowed_models = match &request.allowed_models {
        None => None,
        Some(inner) => Some(validate_allowed_models(inner.as_deref())?),
    };
    let channel = channels::update(
        &state.db,
        id,
        &ChannelPatch {
            name: request.name,
            base_url: request.base_url,
            api_key: request.api_key,
            allowed_models,
            enabled: request.enabled,
        },
    )
    .await?
    .ok_or(AdminError::NotFound)?;
    Ok(Json(channel.into()))
}

#[utoipa::path(
    delete,
    path = "/admin/channels/{id}",
    tag = "Admin",
    operation_id = "delete_channel",
    summary = "Delete a channel",
    description = "Removes the channel. A channel that still has client keys \
        bound to it refuses deletion with 409; disable it instead.",
    security(("basic_auth" = [])),
    params(("id" = i64, Path, description = "Channel id", example = 1)),
    responses(
        (status = 204, description = "Channel deleted, no body"),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body"),
        (status = 404, description = "No channel with this id", body = AdminErrorBody, example = json!({
            "error": "channel not found"
        })),
        (status = 409, description = "The channel still has bound keys", body = AdminErrorBody, example = json!({
            "error": "channel has bound API keys; disable it instead"
        }))
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
) -> Result<StatusCode, AdminError> {
    match channels::delete(&state.db, id).await? {
        DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        DeleteOutcome::NotFound => Err(AdminError::NotFound),
        DeleteOutcome::HasBoundKeys => Err(AdminError::Conflict(
            "channel has bound API keys; disable it instead".to_string(),
        )),
    }
}

// `null` in JSON maps to `Some(None)`, a missing field to the outer default.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde_json::{json, Value};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::http::{router, AppState};

    const UPSTREAM_KEY: &str = "sk-upstream-secret-1234567890";

    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        AppState {
            db: pool,
            admin_login: "admin".to_string(),
            admin_password: "secret".to_string(),
            client: crate::http::upstream_client(),
            log_bodies: false,
            log_tracker: crate::logging::LogTracker::default(),
        }
    }

    fn auth_header() -> String {
        format!("Basic {}", STANDARD.encode("admin:secret"))
    }

    async fn send_json(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        send_raw(state, method, uri, Some(body.to_string().into_bytes())).await
    }

    async fn send_raw(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", auth_header())
            .header("Content-Type", "application/json");
        let request = match body {
            Some(bytes) => request.body(Body::from(bytes)),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let parsed = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, parsed)
    }

    fn create_body() -> Value {
        json!({
            "name": "primary",
            "base_url": "https://api.example.com",
            "api_key": UPSTREAM_KEY,
        })
    }

    #[tokio::test]
    async fn create_then_list_get_patch_returns_channel_state() {
        let state = test_state().await;
        let (status, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["name"], "primary");
        assert_eq!(created["enabled"], true);
        assert_eq!(created["allowed_models"], Value::Null);

        let (status, listed) = send_json(&state, "GET", "/admin/channels", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed.as_array().unwrap().len(), 1);

        let id = created["id"].as_i64().unwrap();
        let (status, fetched) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fetched["name"], "primary");

        let patch = json!({
            "name": "renamed",
            "allowed_models": ["gpt-4o", "gpt-4o-mini"],
            "enabled": false,
        });
        let (status, patched) =
            send_json(&state, "PATCH", &format!("/admin/channels/{id}"), patch).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(patched["name"], "renamed");
        assert_eq!(patched["allowed_models"], json!(["gpt-4o", "gpt-4o-mini"]));
        assert_eq!(patched["enabled"], false);

        let (_, reread) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert_eq!(reread["name"], "renamed");
        assert_eq!(reread["enabled"], false);
    }

    #[tokio::test]
    async fn patch_can_clear_allowed_models_with_explicit_null() {
        let state = test_state().await;
        let mut body = create_body();
        body["allowed_models"] = json!(["gpt-4o"]);
        let (_, created) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(created["allowed_models"], json!(["gpt-4o"]));

        let id = created["id"].as_i64().unwrap();
        let (status, patched) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({ "allowed_models": null }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(patched["allowed_models"], Value::Null);
    }

    #[tokio::test]
    async fn invalid_base_url_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["base_url"] = json!("not a url");
        let (status, error) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"].as_str().unwrap().contains("base_url"));
    }

    #[tokio::test]
    async fn non_http_scheme_base_url_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["base_url"] = json!("ftp://api.example.com");
        let (status, _) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn allowed_models_with_empty_string_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["allowed_models"] = json!(["gpt-4o", ""]);
        let (status, _) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn allowed_models_non_array_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["allowed_models"] = json!("gpt-4o");
        let (status, _) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_without_bound_keys_removes_channel() {
        let state = test_state().await;
        let (_, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        let id = created["id"].as_i64().unwrap();
        let (status, body) =
            send_raw(&state, "DELETE", &format!("/admin/channels/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);

        let (status, _) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_with_bound_key_returns_conflict() {
        let state = test_state().await;
        let (_, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        let channel_id = created["id"].as_i64().unwrap();
        sqlx::query(
            "INSERT INTO api_keys (key_hash, display_prefix, channel_id, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind("deadbeefhash")
        .bind("hyd-abcd")
        .bind(channel_id)
        .bind(crate::db::timestamp::now_utc())
        .execute(&state.db)
        .await
        .unwrap();

        let (status, error) = send_raw(
            &state,
            "DELETE",
            &format!("/admin/channels/{channel_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(error["error"].as_str().unwrap().contains("bound"));

        let (status, _) = send_json(
            &state,
            "GET",
            &format!("/admin/channels/{channel_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_channel_returns_not_found() {
        let state = test_state().await;
        let (status, _) = send_json(&state, "GET", "/admin/channels/999", Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send_json(
            &state,
            "PATCH",
            "/admin/channels/999",
            json!({ "enabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send_raw(&state, "DELETE", "/admin/channels/999", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_key_is_masked_in_every_response() {
        let state = test_state().await;
        let (status, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        assert_eq!(status, StatusCode::CREATED);
        let raw = created.to_string();
        assert!(!raw.contains(UPSTREAM_KEY));
        assert_eq!(created["has_api_key"], true);
        assert_eq!(created["api_key_masked"], json!("sk-u...7890"));

        let (_, listed) = send_json(&state, "GET", "/admin/channels", Value::Null).await;
        assert!(!listed.to_string().contains(UPSTREAM_KEY));

        let id = created["id"].as_i64().unwrap();
        let (_, patched) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({ "api_key": UPSTREAM_KEY }),
        )
        .await;
        assert!(!patched.to_string().contains(UPSTREAM_KEY));

        let (_, fetched) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert!(!fetched.to_string().contains(UPSTREAM_KEY));
    }
}
