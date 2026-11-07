//! Admin CRUD handlers for client API keys.
//!
//! Error style matches `admin_channels`: plain JSON bodies
//! `{"error": "..."}`, not the OpenAI shape reserved for proxy routes.
//! Status mapping: 400 invalid input or disabled channel, 401 handled by
//! `admin_auth`, 404 unknown channel or key, 409 deleted key edited or
//! re-deleted, 500 internal storage failure.
//!
//! Security invariant: the full key is returned exactly once, by the
//! creation response. Every other view carries `display_prefix` only.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::channels;
use crate::db::keys::{self, ApiKey, DeleteOutcome, KeyPatch, NewKey, PatchOutcome};
use crate::http::admin_auth::AdminAuth;
use crate::http::AppState;
use crate::keys as keygen;

#[derive(Debug, thiserror::Error)]
pub enum KeysError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for KeysError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            KeysError::NotFound(message) => (StatusCode::NOT_FOUND, message),
            KeysError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            KeysError::Conflict(message) => (StatusCode::CONFLICT, message),
            KeysError::Database(e) => {
                tracing::error!(error = %e, "admin key query failed");
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
pub struct CreateKeyRequest {
    pub channel_id: i64,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchKeyRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub name: Option<Option<String>>,
}

/// View without the key material; safe everywhere except creation.
#[derive(Serialize)]
pub struct KeyView {
    pub id: i64,
    pub display_prefix: String,
    pub name: Option<String>,
    pub channel_id: i64,
    pub enabled: bool,
    pub deleted_at: Option<String>,
    pub created_at: String,
}

impl From<ApiKey> for KeyView {
    fn from(key: ApiKey) -> Self {
        Self {
            id: key.id,
            display_prefix: key.display_prefix,
            name: key.name,
            channel_id: key.channel_id,
            enabled: key.enabled,
            deleted_at: key.deleted_at,
            created_at: key.created_at,
        }
    }
}

/// Creation response; the only place the full key is ever exposed.
#[derive(Serialize)]
pub struct CreatedKeyView {
    pub id: i64,
    pub key: String,
    pub display_prefix: String,
    pub channel_id: i64,
    pub name: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

impl CreatedKeyView {
    fn new(key: ApiKey, full: String) -> Self {
        Self {
            id: key.id,
            key: full,
            display_prefix: key.display_prefix,
            channel_id: key.channel_id,
            name: key.name,
            enabled: key.enabled,
            created_at: key.created_at,
        }
    }
}

const CREATION_ATTEMPTS: usize = 5;

pub async fn create(
    State(state): State<AppState>,
    _auth: AdminAuth,
    payload: Result<Json<CreateKeyRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreatedKeyView>), KeysError> {
    let Json(request) =
        payload.map_err(|rejection| KeysError::BadRequest(rejection.body_text()))?;
    let channel = channels::find(&state.db, request.channel_id)
        .await?
        .ok_or_else(|| KeysError::NotFound("channel not found".to_string()))?;
    if !channel.enabled {
        return Err(KeysError::BadRequest("channel is disabled".to_string()));
    }

    // A key_hash UNIQUE collision means two OS-RNG draws agreed; regenerate
    // rather than fail the admin request.
    let mut last_error: Option<sqlx::Error> = None;
    for _ in 0..CREATION_ATTEMPTS {
        let generated = keygen::generate();
        let result = keys::create(
            &state.db,
            &NewKey {
                key_hash: generated.key_hash,
                display_prefix: generated.display_prefix,
                name: request.name.clone(),
                channel_id: request.channel_id,
            },
        )
        .await;
        match result {
            Ok(stored) => {
                return Ok((
                    StatusCode::CREATED,
                    Json(CreatedKeyView::new(stored, generated.full)),
                ))
            }
            Err(e) if is_unique_violation(&e) => last_error = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(KeysError::Database(
        last_error.expect("at least one attempt"),
    ))
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|db_error| db_error.is_unique_violation())
}

pub async fn list(
    State(state): State<AppState>,
    _auth: AdminAuth,
) -> Result<Json<Vec<KeyView>>, KeysError> {
    let key_list = keys::list(&state.db).await?;
    Ok(Json(key_list.into_iter().map(Into::into).collect()))
}

pub async fn patch(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
    payload: Result<Json<PatchKeyRequest>, JsonRejection>,
) -> Result<Json<KeyView>, KeysError> {
    let Json(request) =
        payload.map_err(|rejection| KeysError::BadRequest(rejection.body_text()))?;
    if let Some(Some(name)) = &request.name {
        if name.trim().is_empty() {
            return Err(KeysError::BadRequest("name must not be empty".to_string()));
        }
    }
    match keys::patch(
        &state.db,
        id,
        &KeyPatch {
            name: request.name,
            enabled: request.enabled,
        },
    )
    .await?
    {
        PatchOutcome::Updated(key) => Ok(Json(key.into())),
        PatchOutcome::NotFound => Err(KeysError::NotFound("key not found".to_string())),
        PatchOutcome::Deleted => Err(KeysError::Conflict(
            "key is deleted and cannot be edited".to_string(),
        )),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
) -> Result<StatusCode, KeysError> {
    match keys::delete(&state.db, id).await? {
        DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        DeleteOutcome::AlreadyDeleted => {
            Err(KeysError::Conflict("key is already deleted".to_string()))
        }
        DeleteOutcome::NotFound => Err(KeysError::NotFound("key not found".to_string())),
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
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde_json::{json, Value};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::db::keys;
    use crate::http::{router, AppState};
    use crate::keys as keygen;

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

    async fn send_raw(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Vec<u8>>,
        auth: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("Content-Type", "application/json");
        if let Some(header) = auth {
            request = request.header("Authorization", header);
        }
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

    async fn send_auth_json(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        send_raw(
            state,
            method,
            uri,
            Some(body.to_string().into_bytes()),
            Some(&auth_header()),
        )
        .await
    }

    // The channel creation endpoint has no `enabled` field; disabling goes
    // through the channel PATCH route.
    async fn create_channel(state: &AppState, enabled: bool) -> i64 {
        let (status, created) = send_auth_json(
            state,
            "POST",
            "/admin/channels",
            json!({
                "name": "primary",
                "base_url": "https://api.example.com",
                "api_key": "sk-upstream-secret-1234567890",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let channel_id = created["id"].as_i64().unwrap();
        if !enabled {
            let (status, _) = send_auth_json(
                state,
                "PATCH",
                &format!("/admin/channels/{channel_id}"),
                json!({ "enabled": false }),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        channel_id
    }

    async fn create_key(state: &AppState, channel_id: i64) -> (String, Value) {
        let (status, created) = send_auth_json(
            state,
            "POST",
            "/admin/keys",
            json!({ "channel_id": channel_id }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let full = created["key"].as_str().unwrap().to_string();
        (full, created)
    }

    #[tokio::test]
    async fn created_key_returns_full_key_once_with_matching_hash_and_prefix() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let (full, created) = create_key(&state, channel_id).await;

        assert!(full.starts_with("hyd-"));
        assert_eq!(created["display_prefix"], json!(full[..12]));
        assert_eq!(created["channel_id"], channel_id);
        assert_eq!(created["enabled"], true);
        assert_eq!(created["deleted_at"], Value::Null);

        let stored_hash: String = sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE id = ?")
            .bind(created["id"].as_i64().unwrap())
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(stored_hash, keygen::sha256_hex(full.as_bytes()));
        assert_ne!(stored_hash, full);
    }

    #[tokio::test]
    async fn key_listing_never_exposes_the_full_key() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let (full, created) = create_key(&state, channel_id).await;
        let _ = created;

        let (status, listed) = send_auth_json(&state, "GET", "/admin/keys", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert!(!listed.to_string().contains(&full));
        assert!(listed[0]["display_prefix"]
            .as_str()
            .unwrap()
            .starts_with("hyd-"));
    }

    #[tokio::test]
    async fn key_for_unknown_channel_returns_404() {
        let state = test_state().await;
        let (status, error) =
            send_auth_json(&state, "POST", "/admin/keys", json!({ "channel_id": 999 })).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(error["error"].as_str().unwrap().contains("channel"));
    }

    #[tokio::test]
    async fn key_for_disabled_channel_returns_400() {
        let state = test_state().await;
        let channel_id = create_channel(&state, false).await;
        let (status, error) = send_auth_json(
            &state,
            "POST",
            "/admin/keys",
            json!({ "channel_id": channel_id }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"].as_str().unwrap().contains("disabled"));
    }

    #[tokio::test]
    async fn patch_toggles_enabled_on_alive_key() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let (_, created) = create_key(&state, channel_id).await;
        let id = created["id"].as_i64().unwrap();

        let (status, patched) = send_auth_json(
            &state,
            "PATCH",
            &format!("/admin/keys/{id}"),
            json!({ "enabled": false, "name": "ci-bot" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(patched["enabled"], false);
        assert_eq!(patched["name"], "ci-bot");

        let (_, reread) = send_auth_json(&state, "GET", "/admin/keys", Value::Null).await;
        assert_eq!(reread[0]["enabled"], false);
        assert_eq!(reread[0]["deleted_at"], Value::Null);
    }

    #[tokio::test]
    async fn patch_can_clear_name_with_explicit_null() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let (_, created) = send_auth_json(
            &state,
            "POST",
            "/admin/keys",
            json!({ "channel_id": channel_id, "name": "temporary" }),
        )
        .await;
        assert_eq!(created["name"], "temporary");
        let id = created["id"].as_i64().unwrap();

        let (status, patched) = send_auth_json(
            &state,
            "PATCH",
            &format!("/admin/keys/{id}"),
            json!({ "name": null }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(patched["name"], Value::Null);
    }

    #[tokio::test]
    async fn patch_on_soft_deleted_key_returns_409() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let (_, created) = create_key(&state, channel_id).await;
        let id = created["id"].as_i64().unwrap();
        let (status, _) = send_raw(
            &state,
            "DELETE",
            &format!("/admin/keys/{id}"),
            None,
            Some(&auth_header()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _) = send_auth_json(
            &state,
            "PATCH",
            &format!("/admin/keys/{id}"),
            json!({ "enabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn patch_on_unknown_key_returns_404() {
        let state = test_state().await;
        let (status, _) = send_auth_json(
            &state,
            "PATCH",
            "/admin/keys/999",
            json!({ "enabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_sets_deleted_at_and_second_delete_returns_409() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let (full, created) = create_key(&state, channel_id).await;
        let id = created["id"].as_i64().unwrap();

        let (status, body) = send_raw(
            &state,
            "DELETE",
            &format!("/admin/keys/{id}"),
            None,
            Some(&auth_header()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);

        let (second_status, _) = send_raw(
            &state,
            "DELETE",
            &format!("/admin/keys/{id}"),
            None,
            Some(&auth_header()),
        )
        .await;
        assert_eq!(second_status, StatusCode::CONFLICT);

        let (_, listed) = send_auth_json(&state, "GET", "/admin/keys", Value::Null).await;
        assert!(listed[0]["deleted_at"].is_string());
        assert!(!listed.to_string().contains(&full));

        // Soft delete removes the row from the proxy auth lookup.
        let hash = keygen::sha256_hex(full.as_bytes());
        let lookup = keys::find_alive_by_hash(&state.db, &hash).await.unwrap();
        assert!(lookup.is_none());
    }

    #[tokio::test]
    async fn delete_unknown_key_returns_404() {
        let state = test_state().await;
        let (status, _) = send_raw(
            &state,
            "DELETE",
            "/admin/keys/999",
            None,
            Some(&auth_header()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn find_alive_by_hash_resolves_created_key_and_ignores_deleted() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let (full, _) = create_key(&state, channel_id).await;

        let hash = keygen::sha256_hex(full.as_bytes());
        let found = keys::find_alive_by_hash(&state.db, &hash).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().channel_id, channel_id);

        keys::delete(&state.db, 1).await.unwrap();
        let gone = keys::find_alive_by_hash(&state.db, &hash).await.unwrap();
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn key_routes_require_authentication() {
        let state = test_state().await;
        let channel_id = create_channel(&state, true).await;
        let body = json!({ "channel_id": channel_id }).to_string().into_bytes();

        let (status, _) = send_raw(&state, "POST", "/admin/keys", Some(body), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = send_raw(&state, "GET", "/admin/keys", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = send_raw(
            &state,
            "PATCH",
            "/admin/keys/1",
            Some(json!({ "enabled": true }).to_string().into_bytes()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = send_raw(&state, "DELETE", "/admin/keys/1", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
