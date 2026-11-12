//! Integration tests for the admin UI login flow: the login page, the
//! credentials form, the session cookie it issues, what that cookie opens
//! and what it must not, logout, and the password-derived signing key. All
//! requests go through the real router.

mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hydra::http::AppState;
use sqlx::SqlitePool;
use tower::ServiceExt;

use common::{test_pool, within};

const LOGIN_PATH: &str = "/admin/ui/login";
const COOKIE_NAME: &str = "hydra_admin_session";

fn basic_header(credentials: &str) -> String {
    format!("Basic {}", STANDARD.encode(credentials))
}

/// Application state with the given admin password, mirroring
/// `common::test_app` but letting a test rotate the credentials.
fn state_with_password(db: &SqlitePool, password: &str) -> AppState {
    state_with(db, "admin", password, None)
}

fn state_with(
    db: &SqlitePool,
    login: &str,
    password: &str,
    session_secret: Option<&str>,
) -> AppState {
    AppState::new(
        hydra::db::Db::from(db.clone()),
        login.to_string(),
        password.to_string(),
        false,
        hydra::config::TrustedProxies::default(),
        hydra::flood::FloodCap::default(),
    )
    .with_session_secret(session_secret)
}

/// One response with the headers the assertions read.
struct Answer {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    www_authenticate: Option<String>,
    content_type: Option<String>,
    body: String,
}

impl Answer {
    /// The session token out of the `Set-Cookie` value.
    fn session_token(&self) -> String {
        self.set_cookie
            .as_ref()
            .expect("a session cookie")
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_string()
    }
}

async fn send(state: AppState, request: Request<Body>) -> Answer {
    let response = within(hydra::http::router(state).oneshot(request))
        .await
        .unwrap();
    let status = response.status();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_string())
    };
    let (location, set_cookie, www_authenticate, content_type) = (
        header("location"),
        header("set-cookie"),
        header("www-authenticate"),
        header("content-type"),
    );
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    Answer {
        status,
        location,
        set_cookie,
        www_authenticate,
        content_type,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get(state: AppState, uri: &str, headers: &[(&str, String)]) -> Answer {
    let mut builder = Request::get(uri);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    send(state, builder.body(Body::empty()).unwrap()).await
}

fn cookie_header(token: &str) -> (&'static str, String) {
    ("cookie", format!("{COOKIE_NAME}={token}"))
}

/// Submits the login form as a browser would: same-origin metadata and a
/// URL-encoded body.
async fn submit_login(state: AppState, login: &str, password: &str) -> Answer {
    let body = format!("login={login}&password={password}");
    let request = Request::post(LOGIN_PATH)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(body))
        .unwrap();
    send(state, request).await
}

/// Logs in and returns the session token.
async fn login(db: &SqlitePool, password: &str) -> String {
    submit_login(state_with_password(db, password), "admin", password)
        .await
        .session_token()
}

#[tokio::test]
async fn the_login_page_renders_without_credentials() {
    let db = test_pool().await;
    let answer = get(state_with_password(&db, "secret"), LOGIN_PATH, &[]).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.location, None);
    assert!(answer.body.contains("action=\"/admin/ui/login\""));
    assert!(answer.body.contains("name=\"login\""));
    assert!(answer.body.contains("type=\"password\""));
    assert!(answer.body.contains("/admin/ui/static/admin.css"));
    assert!(answer
        .body
        .contains(concat!("hydra ", env!("CARGO_PKG_VERSION"))));
}

#[tokio::test]
async fn valid_credentials_issue_a_session_cookie_and_redirect_to_the_ui() {
    let db = test_pool().await;
    let answer = submit_login(state_with_password(&db, "secret"), "admin", "secret").await;

    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some("/admin/ui"));
    let set_cookie = answer.set_cookie.expect("a login sets the session cookie");
    assert!(set_cookie.starts_with(&format!("{COOKIE_NAME}=")));
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/admin/ui",
        "Max-Age=604800",
    ] {
        assert!(
            set_cookie.contains(attribute),
            "{set_cookie} misses {attribute}"
        );
    }
}

#[tokio::test]
async fn invalid_credentials_re_render_the_form_with_401_and_issue_no_cookie() {
    let db = test_pool().await;
    for (login, password) in [("admin", "wrong"), ("root", "secret")] {
        let answer = submit_login(state_with_password(&db, "secret"), login, password).await;
        assert_eq!(
            answer.status,
            StatusCode::UNAUTHORIZED,
            "{login}/{password}"
        );
        assert!(answer.set_cookie.is_none());
        assert!(answer.body.contains("Invalid login or password."));
        assert!(
            !answer.body.contains(&format!("value=\"{password}\"")),
            "the form must never echo the submitted password"
        );
    }
}

#[tokio::test]
async fn the_session_cookie_opens_every_ui_page() {
    let db = test_pool().await;
    let token = login(&db, "secret").await;

    for uri in [
        "/admin/ui/channels",
        "/admin/ui/keys",
        "/admin/ui/logs",
        "/admin/ui/stats",
    ] {
        let answer = get(
            state_with_password(&db, "secret"),
            uri,
            &[cookie_header(&token)],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "session GET {uri}");
        assert!(
            answer
                .content_type
                .as_deref()
                .unwrap_or("")
                .contains("text/html"),
            "{uri} must render a page"
        );
    }

    let root = get(
        state_with_password(&db, "secret"),
        "/admin/ui",
        &[cookie_header(&token)],
    )
    .await;
    assert_eq!(root.status, StatusCode::SEE_OTHER);
    assert_eq!(root.location.as_deref(), Some("/admin/ui/channels"));
}

#[tokio::test]
async fn the_session_cookie_is_rejected_by_the_json_admin_api() {
    let db = test_pool().await;
    let token = login(&db, "secret").await;

    for uri in ["/admin/channels", "/admin/logs"] {
        let answer = get(
            state_with_password(&db, "secret"),
            uri,
            &[cookie_header(&token)],
        )
        .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "session GET {uri}");
        assert_eq!(
            answer.www_authenticate.as_deref(),
            Some("Basic realm=\"hydra\""),
            "{uri} must keep challenging for Basic credentials"
        );
    }
}

#[tokio::test]
async fn basic_credentials_keep_working_on_the_json_api_and_the_ui() {
    let db = test_pool().await;
    let state = state_with_password(&db, "secret");

    let json = get(
        state.clone(),
        "/admin/channels",
        &[("authorization", basic_header("admin:secret"))],
    )
    .await;
    assert_eq!(json.status, StatusCode::OK);

    let page = get(
        state,
        "/admin/ui/channels",
        &[("authorization", basic_header("admin:secret"))],
    )
    .await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "the UI still accepts Basic Auth"
    );
}

#[tokio::test]
async fn ui_requests_without_a_session_are_redirected_not_challenged() {
    let db = test_pool().await;
    let state = state_with_password(&db, "secret");

    for uri in ["/admin/ui", "/admin/ui/channels", "/admin/ui/logs"] {
        let answer = get(state.clone(), uri, &[]).await;
        assert_eq!(
            answer.status,
            StatusCode::SEE_OTHER,
            "unauthenticated {uri}"
        );
        assert_eq!(answer.location.as_deref(), Some(LOGIN_PATH));
        assert!(
            answer.www_authenticate.is_none(),
            "{uri} must not answer a challenge, or the browser shows its dialog"
        );
    }

    // A cookie that is not a session is treated as no session at all.
    let answer = get(state, "/admin/ui/channels", &[cookie_header("forged")]).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some(LOGIN_PATH));
}

#[tokio::test]
async fn logout_clears_the_session_cookie() {
    let db = test_pool().await;
    let token = login(&db, "secret").await;

    let answer = send(
        state_with_password(&db, "secret"),
        Request::post("/admin/ui/logout")
            .header("cookie", format!("{COOKIE_NAME}={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some(LOGIN_PATH));
    let set_cookie = answer.set_cookie.expect("logout clears the cookie");
    assert!(set_cookie.starts_with(&format!("{COOKIE_NAME}=;")));
    assert!(set_cookie.contains("Max-Age=0"));
    assert!(set_cookie.contains("Path=/admin/ui"));
    assert!(set_cookie.contains("HttpOnly"));

    // Logout is idempotent and session-exempt: with the cleared cookie the
    // request still lands on the login page instead of a 401.
    for cookie in [format!("{COOKIE_NAME}="), format!("{COOKIE_NAME}=cleared")] {
        let again = send(
            state_with_password(&db, "secret"),
            Request::post("/admin/ui/logout")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(again.status, StatusCode::SEE_OTHER);
        assert_eq!(again.location.as_deref(), Some(LOGIN_PATH));
    }
}

#[tokio::test]
async fn a_session_issued_before_a_password_change_stops_working() {
    let db = test_pool().await;
    let token = login(&db, "secret").await;

    let rotated = get(
        state_with_password(&db, "new-secret"),
        "/admin/ui/channels",
        &[cookie_header(&token)],
    )
    .await;
    assert_eq!(rotated.status, StatusCode::SEE_OTHER);
    assert_eq!(rotated.location.as_deref(), Some(LOGIN_PATH));

    // The same login still works under the new password.
    let fresh = submit_login(
        state_with_password(&db, "new-secret"),
        "admin",
        "new-secret",
    )
    .await;
    assert_eq!(fresh.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_session_secret_pins_sessions_across_a_password_change() {
    let db = test_pool().await;
    let login_state = state_with(&db, "admin", "secret", Some("pinned-secret"));
    let token = submit_login(login_state, "admin", "secret")
        .await
        .session_token();

    let rotated = state_with(&db, "admin", "new-secret", Some("pinned-secret"));
    let answer = get(rotated, "/admin/ui/channels", &[cookie_header(&token)]).await;
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "HYDRA_SESSION_SECRET must survive a password rotation"
    );
}

#[tokio::test]
async fn a_tampered_session_cookie_is_sent_to_the_login_page() {
    let db = test_pool().await;
    let token = login(&db, "secret").await;
    let mut tampered = token.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });

    for candidate in [tampered, format!("{token}."), format!("x.{token}")] {
        let answer = get(
            state_with_password(&db, "secret"),
            "/admin/ui/channels",
            &[cookie_header(&candidate)],
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::SEE_OTHER,
            "cookie {candidate:?} must not authenticate"
        );
    }
}

#[tokio::test]
async fn static_assets_are_served_without_a_session() {
    let db = test_pool().await;
    let state = state_with_password(&db, "secret");

    for (uri, content_type) in [
        ("/admin/ui/static/admin.css", "text/css"),
        ("/admin/ui/static/htmx.min.js", "application/javascript"),
        ("/admin/ui/static/admin.js", "application/javascript"),
    ] {
        let answer = get(state.clone(), uri, &[]).await;
        assert_eq!(answer.status, StatusCode::OK, "unauthenticated {uri}");
        assert!(
            answer
                .content_type
                .as_deref()
                .unwrap_or("")
                .contains(content_type),
            "{uri} serves {}",
            answer.content_type.as_deref().unwrap_or("no content type")
        );
    }
}

#[tokio::test]
async fn the_login_form_is_behind_the_same_origin_guard() {
    let db = test_pool().await;
    let request = Request::post(LOGIN_PATH)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("sec-fetch-site", "cross-site")
        .body(Body::from("login=admin&password=secret"))
        .unwrap();
    let answer = send(state_with_password(&db, "secret"), request).await;
    assert_eq!(
        answer.status,
        StatusCode::FORBIDDEN,
        "a cross-site login submission must not issue a session"
    );
    assert!(answer.set_cookie.is_none());
}
