//! Axum routers and handlers.
//!
//! Admin routes carry the Basic Auth extractor from `admin_auth`; only the
//! liveness check is public. Proxy routes slot in here in later tasks.

mod admin_auth;
mod admin_channels;

use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use sqlx::SqlitePool;

/// Shared application state handed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub admin_login: String,
    pub admin_password: String,
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Builds the full application router.
pub fn router(state: AppState) -> Router {
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
        .with_state(state)
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
