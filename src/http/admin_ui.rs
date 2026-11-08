//! Server-rendered admin UI: askama templates with htmx, no JS build step.
//!
//! Every page goes through `templates/base.html` (header nav, footer with
//! the crate version); the per-page templates extend it and keep a `section`
//! marker so the nav highlights the active area. Pages share the existing
//! `AdminAuth` extractor with the JSON admin API, and the whole router sits
//! behind the `csrf` guard, so mutating routes added in later tasks are
//! covered automatically. Static assets are compiled into the binary with
//! `include_bytes!`; `htmx.min.js` is vendored to keep the UI fully
//! self-hosted.

use askama::Template;
use askama_web::WebTemplate;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

use crate::http::admin_auth::AdminAuth;
use crate::http::AppState;

use crate::http::admin_ui_channels;
use crate::http::admin_ui_keys;
use crate::http::admin_ui_logs;

// Vendored htmx 2.0.11, self-hosted so the UI works without network access.
const HTMX_JS: &[u8] = include_bytes!("../../assets/htmx.min.js");
const ADMIN_CSS: &[u8] = include_bytes!("../../assets/admin.css");

const JS_CONTENT_TYPE: &str = "application/javascript; charset=utf-8";
const CSS_CONTENT_TYPE: &str = "text/css; charset=utf-8";

/// Htmx-ready fragment for flash-style error banners, returned by failed
/// form submissions in later tasks.
#[derive(Template, WebTemplate)]
#[template(path = "partials/error_banner.html")]
pub struct ErrorBanner {
    pub error: String,
}

async fn index(_auth: AdminAuth) -> Redirect {
    Redirect::to("/admin/ui/channels")
}

fn asset(bytes: &'static [u8], content_type: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

async fn htmx_js(_auth: AdminAuth) -> Response {
    asset(HTMX_JS, JS_CONTENT_TYPE)
}

async fn admin_css(_auth: AdminAuth) -> Response {
    asset(ADMIN_CSS, CSS_CONTENT_TYPE)
}

/// Builds the `/admin/ui` router: pages, embedded assets, and the CSRF
/// guard layered over everything, including the merged page routers, so
/// every POST route added here is protected by default.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/static/admin.css", get(admin_css))
        .merge(admin_ui_channels::router())
        .merge(admin_ui_keys::router())
        .merge(admin_ui_logs::router())
        .layer(axum::middleware::from_fn(crate::http::csrf::guard))
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
    use crate::http::router;

    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        AppState {
            db: pool.into(),
            admin_login: "admin".to_string(),
            admin_password: "secret".to_string(),
            client: crate::http::upstream_client(),
            log_bodies: false,
            log_tracker: crate::logging::LogTracker::default(),
            cache: crate::cache::ResolutionCache::default(),
        }
    }

    fn auth_header() -> String {
        format!("Basic {}", STANDARD.encode("admin:secret"))
    }

    async fn get_page(state: AppState, uri: &str, auth: Option<&str>) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::get(uri);
        if let Some(header) = auth {
            builder = builder.header("Authorization", header);
        }
        let response = router(state)
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn ui_root_redirects_to_channels_with_auth() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::get("/admin/ui")
                    .header("Authorization", auth_header())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/admin/ui/channels");
    }

    #[tokio::test]
    async fn ui_root_without_credentials_returns_401() {
        let state = test_state().await;
        let (status, _) = get_page(state, "/admin/ui", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn channels_page_renders_with_nav_and_version() {
        let state = test_state().await;
        let (status, body) = get_page(state, "/admin/ui/channels", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("/admin/ui/channels"));
        assert!(html.contains("/admin/ui/keys"));
        assert!(html.contains("/admin/ui/logs"));
        assert!(html.contains("class=\"active\""));
        assert!(html.contains(concat!("hydra ", env!("CARGO_PKG_VERSION"))));
    }

    #[tokio::test]
    async fn keys_and_logs_pages_render_with_auth() {
        let state = test_state().await;
        let (status, _) = get_page(state, "/admin/ui/keys", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        let state = test_state().await;
        let (status, _) = get_page(state, "/admin/ui/logs", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn ui_pages_without_credentials_return_401() {
        let state = test_state().await;
        let (status, _) = get_page(state, "/admin/ui/channels", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn htmx_asset_serves_with_javascript_content_type() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::get("/admin/ui/static/htmx.min.js")
                    .header("Authorization", auth_header())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/javascript; charset=utf-8"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.to_vec(), HTMX_JS.to_vec());
    }

    #[tokio::test]
    async fn css_asset_serves_with_css_content_type() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::get("/admin/ui/static/admin.css")
                    .header("Authorization", auth_header())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn static_assets_require_admin_credentials() {
        let state = test_state().await;
        let (status, _) = get_page(state, "/admin/ui/static/admin.css", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
