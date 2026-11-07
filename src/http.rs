//! Axum routers and handlers.
//!
//! Admin routes carry the Basic Auth extractor from `admin_auth`; only the
//! liveness check is public. The admin CRUD and proxy routes slot in here in
//! later tasks.

mod admin_auth;

use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use sqlx::SqlitePool;

/// Shared application state handed to all handlers.
#[derive(Clone)]
pub struct AppState {
    // Consumed by the admin CRUD handlers landing with the channels task.
    #[allow(dead_code)]
    pub db: SqlitePool,
    // Consumed by the Basic Auth extractor once protected routes land.
    #[allow(dead_code)]
    pub admin_login: String,
    #[allow(dead_code)]
    pub admin_password: String,
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Builds the full application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/admin/health", get(health))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
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

    #[tokio::test]
    async fn health_route_returns_ok_with_status_body() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/admin/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], br#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn health_route_stays_public_without_credentials() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/admin/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
