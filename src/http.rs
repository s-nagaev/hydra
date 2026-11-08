//! Axum routers and handlers.
//!
//! Admin routes carry the Basic Auth extractor from `admin_auth`; only the
//! liveness check is public. Proxy routes under `/v1/*` authenticate client
//! keys through `proxy_auth` and forward to the bound channel's upstream.

mod admin_auth;
mod admin_channels;
mod admin_keys;
mod admin_logs;
mod admin_ui;
mod admin_ui_channels;
mod csrf;
mod docs;
mod openai_error;
mod proxy;
mod proxy_auth;

use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// Maximum buffered chat request body; re-exported so the integration
/// suite can drive the production limit without weakening it.
pub use proxy::CHAT_BODY_LIMIT;

/// Shared application state handed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub admin_login: String,
    pub admin_password: String,
    /// The single upstream HTTP client reused by every proxy request.
    pub client: reqwest::Client,
    /// Whether chat request/response bodies are captured into request_logs.
    pub log_bodies: bool,
    /// In-flight background log insert counter, drained on shutdown.
    pub log_tracker: crate::logging::LogTracker,
}

/// Builds the shared upstream client. Only the connect timeout lives here:
/// a client-level total or read timeout would cut long SSE generations
/// short, so handlers set per-request timeouts where they are safe
/// (non-streaming chat and `/v1/models` get one, streams get none).
pub fn upstream_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("client construction only fails when the TLS backend fails to initialize")
}

#[utoipa::path(
    get,
    path = "/admin/health",
    tag = "Admin",
    operation_id = "health",
    summary = "Liveness check",
    description = "Public heartbeat, no authentication. Docker HEALTHCHECK \
        and load balancers probe this route.",
    responses(
        (status = 200, description = "The service is up", body = Object, example = json!({ "status": "ok" }))
    )
)]
async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Builds the full application router.
pub fn router(state: AppState) -> Router {
    let swagger =
        SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", docs::ApiDoc::openapi());
    Router::new()
        .route("/admin/health", get(health))
        .route(
            "/admin/channels",
            post(admin_channels::create).get(admin_channels::list),
        )
        .route(
            "/admin/channels/{id}",
            get(admin_channels::get_one)
                .patch(admin_channels::patch)
                .delete(admin_channels::delete),
        )
        .route(
            "/admin/keys",
            post(admin_keys::create).get(admin_keys::list),
        )
        .route(
            "/admin/keys/{id}",
            patch(admin_keys::patch).delete(admin_keys::delete),
        )
        .route("/admin/logs", get(admin_logs::list))
        .nest("/admin/ui", admin_ui::router())
        .route(
            "/v1/chat/completions",
            post(proxy::chat_completions)
                // 32 MiB instead of axum's 2 MiB default: base64 vision
                // payloads and long conversations exceed the default. The
                // over-limit rejection is reshaped to OpenAI JSON by the
                // `ChatBody` extractor inside the handler.
                .layer(DefaultBodyLimit::max(proxy::CHAT_BODY_LIMIT)),
        )
        .route("/v1/models", get(proxy::models))
        .with_state(state)
        .merge(swagger)
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use super::*;

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
            client: upstream_client(),
            log_bodies: false,
            log_tracker: crate::logging::LogTracker::default(),
        }
    }

    async fn send(
        state: AppState,
        method: &str,
        uri: &str,
        auth: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(header) = auth {
            request = request.header("Authorization", header);
        }
        let response = router(state)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    fn auth_header() -> String {
        format!("Basic {}", STANDARD.encode("admin:secret"))
    }

    #[tokio::test]
    async fn health_route_returns_ok_with_status_body() {
        let state = test_state().await;
        let (status, body) = send(state, "GET", "/admin/health", None).await;
        assert_eq!(status, 200);
        assert_eq!(&body[..], br#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn health_route_stays_public_without_credentials() {
        let state = test_state().await;
        let (status, _) = send(state, "GET", "/admin/health", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn channels_without_credentials_return_401_with_challenge() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/admin/channels")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()["WWW-Authenticate"],
            r#"Basic realm="hydra""#
        );
    }

    #[tokio::test]
    async fn channels_with_wrong_credentials_return_401() {
        let state = test_state().await;
        let wrong = format!("Basic {}", STANDARD.encode("admin:wrong"));
        let (status, _) = send(state, "GET", "/admin/channels", Some(&wrong)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn channels_with_correct_credentials_return_200() {
        let state = test_state().await;
        let (status, body) = send(state, "GET", "/admin/channels", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"[]");
    }
}
