//! Axum routers and handlers.
//!
//! Admin routes (CRUD, Basic Auth) and proxy routes slot in here in later
//! tasks; only the public liveness check exists for now.

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};
use sqlx::SqlitePool;

/// Shared application state handed to all handlers.
#[derive(Clone)]
pub struct AppState {
    // Unused until the admin CRUD and proxy tasks land; kept so the state
    // shape is final from the start.
    #[allow(dead_code)]
    pub db: SqlitePool,
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
    use axum::http::Request;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use super::*;

    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        AppState { db: pool }
    }

    #[tokio::test]
    async fn health_route_returns_ok_with_status_body() {
        let app = router(test_state().await);
        let response = app
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
}
