//! Integration tests for the site root: `GET /` answers 303 to the admin
//! UI. The root is a navigation route, so the checks below pin what it must
//! not disturb: the proxied `/v1/*` surfaces, the admin JSON API, the UI
//! pages, the embedded static assets, and the request log, which stays
//! empty because nothing is proxied.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use sqlx::SqlitePool;
use tower::ServiceExt;

use common::{test_app, test_pool, within};

/// One response, flattened into what the assertions read.
struct Answer {
    status: StatusCode,
    location: Option<String>,
    www_authenticate: Option<String>,
}

/// Drives the full production router without credentials.
async fn send(db: &SqlitePool, method: &str, uri: &str) -> Answer {
    let response = within(
        hydra::http::router(test_app(db.clone())).oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let status = response.status();
    let header_value = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_string())
    };
    Answer {
        status,
        location: header_value(header::LOCATION),
        www_authenticate: header_value(header::WWW_AUTHENTICATE),
    }
}

async fn request_log_rows(db: &SqlitePool) -> i64 {
    within(sqlx::query_scalar("SELECT COUNT(*) FROM request_logs").fetch_one(db))
        .await
        .unwrap()
}

#[tokio::test]
async fn the_site_root_redirects_to_the_admin_ui() {
    let db = test_pool().await;
    let answer = send(&db, "GET", "/").await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some("/admin/ui"));
    assert!(
        answer.www_authenticate.is_none(),
        "a redirect must not carry a challenge"
    );
}

#[tokio::test]
async fn the_site_root_redirect_is_not_logged() {
    let db = test_pool().await;
    let answer = send(&db, "GET", "/").await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        request_log_rows(&db).await,
        0,
        "the root is a navigation route, not a proxied call"
    );
}

#[tokio::test]
async fn the_site_root_does_not_shadow_the_proxied_routes() {
    let db = test_pool().await;
    for (method, uri) in [
        ("POST", "/v1/chat/completions"),
        ("GET", "/v1/models"),
        ("POST", "/v1/responses"),
        ("POST", "/v1/messages"),
    ] {
        let answer = send(&db, method, uri).await;
        assert_eq!(
            answer.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} without a key"
        );
        assert!(
            answer.location.is_none(),
            "{method} {uri} must not follow the root redirect"
        );
    }
    assert_eq!(
        send(&db, "GET", "/v1/unknown").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_site_root_does_not_shadow_the_admin_routes() {
    let db = test_pool().await;

    let health = send(&db, "GET", "/admin/health").await;
    assert_eq!(health.status, StatusCode::OK);
    assert!(health.location.is_none());

    let api = send(&db, "GET", "/admin/channels").await;
    assert_eq!(api.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        api.www_authenticate.as_deref(),
        Some("Basic realm=\"hydra\"")
    );
    assert!(api.location.is_none());

    let ui = send(&db, "GET", "/admin/ui").await;
    assert_eq!(ui.status, StatusCode::SEE_OTHER);
    assert_eq!(ui.location.as_deref(), Some("/admin/ui/login"));

    let asset = send(&db, "GET", "/admin/ui/static/admin.css").await;
    assert_eq!(asset.status, StatusCode::OK);
    assert!(asset.location.is_none());
}
