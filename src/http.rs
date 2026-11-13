//! Axum routers and handlers.
//!
//! JSON admin routes carry the Basic Auth extractor from `admin_auth`; the
//! admin UI pages accept a session cookie as well (`ui_auth`), and only the
//! liveness check is public. Proxy routes under `/v1/*` authenticate client
//! keys through `proxy_auth` and forward to the bound channel's upstream.
//! The site root is the one navigation route: it redirects to the admin UI
//! and shares no handler with anything else.

mod admin_auth;
mod admin_channels;
mod admin_keys;
mod admin_logs;
mod admin_session;
mod admin_ui;
mod admin_ui_channels;
mod admin_ui_keys;
mod admin_ui_logs;
mod admin_ui_stats;
mod client_ip;
mod csrf;
mod docs;
mod openai_error;
mod proxy;
mod proxy_auth;
mod ui_auth;

use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::response::Redirect;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// Maximum buffered chat request body; re-exported so the integration
/// suite can drive the production limit without weakening it.
pub use proxy::CHAT_BODY_LIMIT;

/// Shared application state handed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: crate::db::Db,
    pub admin_login: String,
    pub admin_password: String,
    /// The single upstream HTTP client reused by every proxy request.
    pub client: reqwest::Client,
    /// Whether chat request/response bodies are captured into request_logs.
    pub log_bodies: bool,
    /// In-flight background log insert counter, drained on shutdown.
    pub log_tracker: crate::logging::LogTracker,
    /// In-memory key→channel resolution cache; see `crate::cache`.
    pub cache: crate::cache::ResolutionCache,
    /// Reverse-proxy networks trusted for `X-Forwarded-For` resolution;
    /// empty means every client is its direct peer address.
    pub trusted_proxies: crate::config::TrustedProxies,
    /// Per-IP flood cap on failed authentications; see `crate::flood`.
    pub flood: crate::flood::FloodCap,
    /// In-memory verdicts of the monthly key limits; see `crate::limit_flags`
    /// and `crate::usage` for how they are computed.
    pub limit_flags: crate::limit_flags::LimitFlags,
    /// Signing key of the admin UI session cookies; see
    /// `crate::http::admin_session` for how it is derived.
    pub(crate) session: admin_session::SessionKey,
}

impl AppState {
    /// Builds state with the production wiring for everything that is not a
    /// parameter: one shared upstream client, a fresh resolution cache, an
    /// empty log tracker, an empty monthly-limit flag store, and the session
    /// key derived from the admin password. Every construction site goes
    /// through this, so a future state field lands here once instead of at
    /// each call site.
    pub fn new(
        db: crate::db::Db,
        admin_login: String,
        admin_password: String,
        log_bodies: bool,
        trusted_proxies: crate::config::TrustedProxies,
        flood: crate::flood::FloodCap,
    ) -> Self {
        let session = admin_session::SessionKey::derive(admin_password.as_bytes());
        AppState {
            db,
            admin_login,
            admin_password,
            client: upstream_client(),
            log_bodies,
            log_tracker: crate::logging::LogTracker::default(),
            cache: crate::cache::ResolutionCache::default(),
            trusted_proxies,
            flood,
            limit_flags: crate::limit_flags::LimitFlags::default(),
            session,
        }
    }

    /// Replaces the password-derived session key with one derived from
    /// `HYDRA_SESSION_SECRET`, keeping sessions valid across an admin
    /// password rotation when the operator asked for that.
    pub fn with_session_secret(mut self, session_secret: Option<&str>) -> Self {
        if let Some(secret) = session_secret.filter(|secret| !secret.is_empty()) {
            self.session = admin_session::SessionKey::derive(secret.as_bytes());
        }
        self
    }
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

/// Where the site root sends a browser; the admin UI decides on its own
/// whether the visitor has a session (login page) or not (default tab).
const ADMIN_UI_PATH: &str = "/admin/ui";

/// The site root: a browser landing on the bare host gets the admin UI
/// instead of a 404. It is a navigation aid, so it is public like the login
/// page, sends a 303 (the same status the UI auth flow uses), and writes no
/// request-log row, being no proxied call.
async fn root() -> Redirect {
    Redirect::to(ADMIN_UI_PATH)
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
        // The site root matches the root path alone: the proxied surfaces,
        // the admin JSON API, the UI pages and its static assets keep their
        // own routes and are untouched by it.
        .route("/", get(root))
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
        .route("/admin/logs/{id}", get(admin_logs::get_one))
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
        // The Responses surface runs the same passthrough pipeline as chat
        // and needs its own copy of the layer, `DefaultBodyLimit` being
        // per-route: without it this route would answer requests between 2
        // and 32 MiB with a 413 the chat route accepts.
        .route(
            "/v1/responses",
            post(proxy::responses).layer(DefaultBodyLimit::max(proxy::CHAT_BODY_LIMIT)),
        )
        // The Anthropic Messages surface: the same pipeline again, with its
        // own body limit for the same reason and its credential in the
        // `x-api-key` header the Messages SDKs send.
        .route(
            "/v1/messages",
            post(proxy::messages).layer(DefaultBodyLimit::max(proxy::CHAT_BODY_LIMIT)),
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
        AppState::new(
            pool.into(),
            "admin".to_string(),
            "secret".to_string(),
            false,
            crate::config::TrustedProxies::default(),
            crate::flood::FloodCap::default(),
        )
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
