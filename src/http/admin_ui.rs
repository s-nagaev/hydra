//! Server-rendered admin UI: askama templates with htmx, no JS build step.
//!
//! Every page goes through `templates/base.html` (header nav, footer with
//! the crate version); the per-page templates extend it and keep a `section`
//! marker so the nav highlights the active area. Pages authenticate with a
//! session cookie or the Basic credentials (`UiAuth`), and the whole router
//! sits behind the `csrf` guard, so mutating routes added in later tasks are
//! covered automatically. Static assets are compiled into the binary with
//! `include_bytes!`; `htmx.min.js` is vendored to keep the UI fully
//! self-hosted.
//!
//! Three routes have to work before a session exists and therefore carry no
//! auth extractor: the login page, the login form, and logout. The static
//! assets are public too, because the login page itself needs the
//! stylesheet; they hold no secrets.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Form, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::http::admin_auth::credentials_match;
use crate::http::admin_session;
use crate::http::ui_auth::{UiAuth, LOGIN_PATH};
use crate::http::AppState;

use crate::http::admin_ui_channels;
use crate::http::admin_ui_keys;
use crate::http::admin_ui_logs;
use crate::http::admin_ui_stats;

// Vendored htmx 2.0.11, self-hosted so the UI works without network access.
const HTMX_JS: &[u8] = include_bytes!("../../assets/htmx.min.js");
const ADMIN_CSS: &[u8] = include_bytes!("../../assets/admin.css");
// The admin UI's own vanilla-JS helpers (log body JSON highlighting).
const ADMIN_JS: &[u8] = include_bytes!("../../assets/admin.js");

const JS_CONTENT_TYPE: &str = "application/javascript; charset=utf-8";
const CSS_CONTENT_TYPE: &str = "text/css; charset=utf-8";

/// Htmx-ready fragment for flash-style error banners, returned by failed
/// form submissions in later tasks.
#[derive(Template, WebTemplate)]
#[template(path = "partials/error_banner.html")]
pub struct ErrorBanner {
    pub error: String,
}

/// The login page. Standalone rather than extending `base.html`: the nav it
/// carries links to pages that would bounce straight back here, and the
/// header nav includes the logout form.
#[derive(Template, WebTemplate)]
#[template(path = "login.html")]
pub struct LoginPage {
    /// Empty when the form has not been submitted yet; otherwise the
    /// message shown above the fields.
    pub error: &'static str,
    pub version: &'static str,
}

/// Fields of the login form.
#[derive(Deserialize)]
struct LoginForm {
    login: String,
    password: String,
}

async fn index(_auth: UiAuth) -> Redirect {
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

async fn htmx_js() -> Response {
    asset(HTMX_JS, JS_CONTENT_TYPE)
}

async fn admin_css() -> Response {
    asset(ADMIN_CSS, CSS_CONTENT_TYPE)
}

async fn admin_js() -> Response {
    asset(ADMIN_JS, JS_CONTENT_TYPE)
}

/// The login page itself: reachable without a session, or the browser could
/// never get one.
async fn login_page() -> LoginPage {
    LoginPage {
        error: "",
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// Verifies the submitted credentials and, on success, answers with the
/// session cookie and a redirect to the default page. A failure re-renders
/// the form with 401 and a message, which is deliberately the same message
/// for an unknown login and for a wrong password.
async fn login_submit(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    if !credentials_match(&form.login, &form.password, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            LoginPage {
                error: "Invalid login or password.",
                version: env!("CARGO_PKG_VERSION"),
            },
        )
            .into_response();
    }

    let now = admin_session::unix_now();
    let token = state.session.mint(now + admin_session::TTL_SECS);
    (
        [(header::SET_COOKIE, admin_session::session_cookie(&token))],
        Redirect::to("/admin/ui"),
    )
        .into_response()
}

/// Drops the session cookie. Idempotent and session-exempt: an expired or
/// missing session still ends up on the login page.
async fn logout() -> Response {
    (
        [(header::SET_COOKIE, admin_session::expired_cookie())],
        Redirect::to(LOGIN_PATH),
    )
        .into_response()
}

/// Builds the `/admin/ui` router: pages, embedded assets, and the CSRF
/// guard layered over everything, including the merged page routers, so
/// every POST route added here is protected by default.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/static/admin.css", get(admin_css))
        .route("/static/admin.js", get(admin_js))
        .merge(admin_ui_channels::router())
        .merge(admin_ui_keys::router())
        .merge(admin_ui_logs::router())
        .merge(admin_ui_stats::router())
        .layer(axum::middleware::from_fn(crate::http::csrf::guard))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
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
        state_for(pool.into(), "secret")
    }

    fn state_for(db: crate::db::Db, password: &str) -> AppState {
        AppState::new(
            db,
            "admin".to_string(),
            password.to_string(),
            false,
            crate::config::TrustedProxies::default(),
            crate::flood::FloodCap::default(),
        )
    }

    fn auth_header() -> String {
        format!("Basic {}", STANDARD.encode("admin:secret"))
    }

    /// One response, flattened into what the assertions read.
    struct Answer {
        status: StatusCode,
        location: Option<String>,
        set_cookie: Option<String>,
        www_authenticate: Option<String>,
        hx_redirect: Option<String>,
        body: String,
    }

    async fn send(state: AppState, method: &str, uri: &str, headers: &[(&str, String)]) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        if method == "POST" && headers.iter().all(|(name, _)| *name != "content-type") {
            builder = builder.header("content-type", "application/x-www-form-urlencoded");
        }
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let response = router(state)
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|value| value.to_str().unwrap().to_string())
        };
        let (location, set_cookie, www_authenticate, hx_redirect) = (
            header("location"),
            header("set-cookie"),
            header("www-authenticate"),
            header("hx-redirect"),
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Answer {
            status,
            location,
            set_cookie,
            www_authenticate,
            hx_redirect,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get_page(state: AppState, uri: &str, auth: Option<&str>) -> (StatusCode, String) {
        let headers: Vec<(&str, String)> = match auth {
            Some(header) => vec![("authorization", header.to_string())],
            None => vec![],
        };
        let answer = send(state, "GET", uri, &headers).await;
        (answer.status, answer.body)
    }

    /// The session token out of a `Set-Cookie` value.
    fn cookie_value(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_string()
    }

    fn cookie_header(token: &str) -> String {
        format!("{}={token}", admin_session::COOKIE_NAME)
    }

    /// Submits the login form the way the browser does, with same-origin
    /// fetch metadata and a URL-encoded body.
    async fn login_with_credentials(state: AppState, login: &str, password: &str) -> Answer {
        let body = format!("login={login}&password={password}");
        let mut builder = Request::builder()
            .method("POST")
            .uri("/admin/ui/login")
            .header("content-type", "application/x-www-form-urlencoded");
        builder = builder.header("sec-fetch-site", "same-origin");
        let response = router(state)
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get("location")
            .map(|value| value.to_str().unwrap().to_string());
        let set_cookie = response
            .headers()
            .get("set-cookie")
            .map(|value| value.to_str().unwrap().to_string());
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Answer {
            status,
            location,
            set_cookie,
            www_authenticate: None,
            hx_redirect: None,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    // --- Login page and form ------------------------------------------------

    #[tokio::test]
    async fn the_login_page_renders_without_credentials() {
        let state = test_state().await;
        let answer = send(state, "GET", "/admin/ui/login", &[]).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert!(answer.body.contains("action=\"/admin/ui/login\""));
        assert!(answer.body.contains("name=\"login\""));
        assert!(answer.body.contains("name=\"password\""));
        assert!(answer.body.contains("type=\"password\""));
        // The page must render styled, so the stylesheet has to load without
        // a session.
        assert!(answer.body.contains("/admin/ui/static/admin.css"));
        assert!(answer
            .body
            .contains(concat!("hydra ", env!("CARGO_PKG_VERSION"))));
    }

    #[tokio::test]
    async fn valid_credentials_issue_a_hardened_session_cookie_and_redirect() {
        let state = test_state().await;
        let answer = login_with_credentials(state, "admin", "secret").await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER);
        assert_eq!(answer.location.as_deref(), Some("/admin/ui"));

        let set_cookie = answer.set_cookie.expect("a session cookie");
        for attribute in [
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Path=/admin/ui",
            &format!("Max-Age={}", admin_session::TTL_SECS),
        ] {
            assert!(
                set_cookie.contains(attribute),
                "{set_cookie} misses {attribute}"
            );
        }
    }

    #[tokio::test]
    async fn invalid_credentials_re_render_the_form_with_401_and_no_cookie() {
        let state = test_state().await;
        for (login, password) in [("admin", "wrong"), ("nobody", "secret")] {
            let answer = login_with_credentials(state.clone(), login, password).await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
            assert!(answer.set_cookie.is_none(), "no session on a failed login");
            assert!(answer.body.contains("Invalid login or password."));
            assert!(
                !answer.body.contains("password="),
                "never echo the password"
            );
        }
    }

    // --- Session cookie: the UI accepts it, the JSON API does not ----------

    #[tokio::test]
    async fn the_session_cookie_authenticates_every_ui_page() {
        let state = test_state().await;
        let token = cookie_value(
            &login_with_credentials(state.clone(), "admin", "secret")
                .await
                .set_cookie
                .unwrap(),
        );

        for uri in [
            "/admin/ui/channels",
            "/admin/ui/keys",
            "/admin/ui/logs",
            "/admin/ui/stats",
        ] {
            let answer = send(
                state.clone(),
                "GET",
                uri,
                &[("cookie", cookie_header(&token))],
            )
            .await;
            assert_eq!(answer.status, StatusCode::OK, "session GET {uri}");
        }

        let answer = send(
            state,
            "GET",
            "/admin/ui",
            &[("cookie", cookie_header(&token))],
        )
        .await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER);
        assert_eq!(answer.location.as_deref(), Some("/admin/ui/channels"));
    }

    #[tokio::test]
    async fn the_session_cookie_does_not_authenticate_the_json_admin_api() {
        let state = test_state().await;
        let token = cookie_value(
            &login_with_credentials(state.clone(), "admin", "secret")
                .await
                .set_cookie
                .unwrap(),
        );
        let answer = send(
            state,
            "GET",
            "/admin/channels",
            &[("cookie", cookie_header(&token))],
        )
        .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            answer.www_authenticate.as_deref(),
            Some("Basic realm=\"hydra\""),
            "the JSON API keeps its challenge"
        );
    }

    #[tokio::test]
    async fn basic_credentials_still_authenticate_the_ui() {
        let state = test_state().await;
        let answer = send(
            state,
            "GET",
            "/admin/ui/channels",
            &[("authorization", auth_header())],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK);
    }

    // --- Rejections: a redirect, never a challenge --------------------------

    #[tokio::test]
    async fn a_page_without_credentials_redirects_to_the_login_page() {
        let state = test_state().await;
        for uri in [
            "/admin/ui",
            "/admin/ui/channels",
            "/admin/ui/logs",
            "/admin/ui/stats",
        ] {
            let answer = send(state.clone(), "GET", uri, &[]).await;
            assert_eq!(
                answer.status,
                StatusCode::SEE_OTHER,
                "unauthenticated GET {uri}"
            );
            assert_eq!(answer.location.as_deref(), Some(LOGIN_PATH));
            assert!(
                answer.www_authenticate.is_none(),
                "no challenge on a UI route, or the browser pops its dialog"
            );
        }
    }

    #[tokio::test]
    async fn an_unauthenticated_htmx_request_gets_a_client_side_redirect() {
        let state = test_state().await;
        let answer = send(
            state,
            "POST",
            "/admin/ui/channels/1/toggle",
            &[("hx-request", "true".to_string())],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.hx_redirect.as_deref(), Some(LOGIN_PATH));
    }

    #[tokio::test]
    async fn expired_tampered_and_foreign_sessions_redirect_to_login() {
        let state = test_state().await;
        let now = admin_session::unix_now();
        let expired = state.session.mint(now - 1);
        let mut tampered = state.session.mint(now + 60);
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        let foreign = state_for(state.db.clone(), "rotated")
            .session
            .mint(now + 60);

        for token in [expired, tampered, foreign, "forged".to_string()] {
            let answer = send(
                state.clone(),
                "GET",
                "/admin/ui/channels",
                &[("cookie", cookie_header(&token))],
            )
            .await;
            assert_eq!(
                answer.status,
                StatusCode::SEE_OTHER,
                "token {token:?} must not authenticate"
            );
            assert_eq!(answer.location.as_deref(), Some(LOGIN_PATH));
        }
    }

    // --- Logout -------------------------------------------------------------

    #[tokio::test]
    async fn logout_clears_the_cookie_and_redirects_to_login() {
        let state = test_state().await;
        let answer = send(state, "POST", "/admin/ui/logout", &[]).await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER);
        assert_eq!(answer.location.as_deref(), Some(LOGIN_PATH));
        let set_cookie = answer.set_cookie.expect("a clearing cookie");
        assert!(set_cookie.contains("Max-Age=0"));
        assert!(set_cookie.contains("Path=/admin/ui"));
    }

    // --- Pages and assets ---------------------------------------------------

    #[tokio::test]
    async fn channels_page_renders_with_nav_and_version() {
        let state = test_state().await;
        let (status, body) = get_page(state, "/admin/ui/channels", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/admin/ui/channels"));
        assert!(body.contains("/admin/ui/keys"));
        assert!(body.contains("/admin/ui/logs"));
        assert!(body.contains("/admin/ui/stats"));
        assert!(body.contains("class=\"active\""));
        assert!(body.contains(concat!("hydra ", env!("CARGO_PKG_VERSION"))));
    }

    #[tokio::test]
    async fn stats_page_renders_with_auth_and_a_complete_nav() {
        let state = test_state().await;
        let (status, body) = get_page(state, "/admin/ui/stats", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Token usage"));
        for section in ["channels", "keys", "logs", "stats"] {
            assert!(
                body.contains(&format!("/admin/ui/{section}")),
                "the nav must link {section}"
            );
        }
        assert!(body.contains("href=\"/admin/ui/stats\""));
    }

    #[tokio::test]
    async fn keys_and_logs_pages_render_with_auth() {
        let state = test_state().await;
        let (status, _) = get_page(state.clone(), "/admin/ui/keys", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get_page(state, "/admin/ui/logs", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn pages_carry_a_logout_form() {
        let state = test_state().await;
        let (status, body) = get_page(state, "/admin/ui/channels", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("action=\"/admin/ui/logout\""), "{body}");
    }

    #[tokio::test]
    async fn htmx_asset_serves_with_javascript_content_type() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::get("/admin/ui/static/htmx.min.js")
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
    async fn static_assets_are_public_so_the_login_page_renders() {
        let state = test_state().await;
        for uri in [
            "/admin/ui/static/admin.css",
            "/admin/ui/static/htmx.min.js",
            "/admin/ui/static/admin.js",
        ] {
            let answer = send(state.clone(), "GET", uri, &[]).await;
            assert_eq!(answer.status, StatusCode::OK, "unauthenticated GET {uri}");
        }
    }

    #[tokio::test]
    async fn admin_js_asset_serves_with_javascript_content_type() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::get("/admin/ui/static/admin.js")
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
        assert_eq!(bytes.to_vec(), ADMIN_JS.to_vec());
    }

    #[tokio::test]
    async fn base_template_loads_the_admin_js_asset() {
        let state = test_state().await;
        let (status, body) = get_page(state, "/admin/ui/channels", Some(&auth_header())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("/admin/ui/static/admin.js"),
            "every page must load the body highlighter: {body}"
        );
    }

    #[test]
    fn the_body_highlighter_never_builds_html_from_body_text() {
        let source = std::str::from_utf8(ADMIN_JS).unwrap();
        assert!(
            !source.contains("innerHTML") && !source.contains("outerHTML"),
            "log bodies are attacker-influenced: they must be handled as text"
        );
        assert!(source.contains("textContent"));
    }
}
