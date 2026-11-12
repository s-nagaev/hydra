//! Authentication for the admin UI: a session cookie or the Basic
//! credentials.
//!
//! The JSON admin API keeps [`crate::http::admin_auth::AdminAuth`] and its
//! `WWW-Authenticate` challenge. The UI must not answer a challenge, because
//! that is what makes the browser pop its Basic Auth dialog: [`UiAuth`]
//! accepts a valid session cookie or the same Basic credentials, and when
//! neither is present it sends the browser to the login page instead.
//!
//! Routes that have to work before a session exists — the login page and its
//! form, logout, and the static assets the login page needs to render — do
//! not take this extractor at all.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};

use crate::http::admin_session::{cookie_token, unix_now};
use crate::http::AppState;

/// The page unauthenticated requests are sent to.
pub const LOGIN_PATH: &str = "/admin/ui/login";

/// Extractor marker proving the request carried a session cookie or valid
/// admin Basic credentials.
pub struct UiAuth;

/// Rejection of [`UiAuth`]: a redirect for a page load, an htmx client-side
/// redirect for a fragment request.
#[derive(Debug, thiserror::Error)]
#[error("unauthenticated")]
pub struct UiAuthError {
    /// The request came from htmx; see [`IntoResponse`].
    htmx: bool,
}

impl IntoResponse for UiAuthError {
    fn into_response(self) -> Response {
        if self.htmx {
            // htmx would otherwise swap the login page into the fragment it
            // was about to replace; `HX-Redirect` navigates the whole page.
            (
                StatusCode::OK,
                [(
                    axum::http::header::HeaderName::from_static("hx-redirect"),
                    LOGIN_PATH,
                )],
            )
                .into_response()
        } else {
            Redirect::to(LOGIN_PATH).into_response()
        }
    }
}

impl FromRequestParts<AppState> for UiAuth {
    type Rejection = UiAuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if session_ok(parts, state) {
            Ok(UiAuth)
        } else {
            Err(UiAuthError {
                htmx: parts.headers.contains_key("hx-request"),
            })
        }
    }
}

/// Whether the request carries a valid session cookie or valid Basic
/// credentials. Both checks are constant-time in the compared secrets.
fn session_ok(parts: &Parts, state: &AppState) -> bool {
    if crate::http::admin_auth::basic_ok(&parts.headers, state) {
        return true;
    }
    match cookie_token(&parts.headers) {
        Some(token) => state.session.verify(token, unix_now()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// State without a database: the extractor only reads the admin
    /// credentials, and the pool is never touched.
    fn test_state() -> AppState {
        AppState::new(
            crate::db::Db::from(
                sqlx::sqlite::SqlitePoolOptions::new()
                    .connect_lazy("sqlite::memory:")
                    .unwrap(),
            ),
            "admin".to_string(),
            "secret".to_string(),
            false,
            crate::config::TrustedProxies::default(),
            crate::flood::FloodCap::default(),
        )
    }

    fn parts_with(header: (&str, &str)) -> Parts {
        axum::http::Request::builder()
            .uri("/admin/ui/channels")
            .header(header.0, header.1)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn basic_header(credentials: &str) -> String {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        )
    }

    #[tokio::test]
    async fn a_valid_session_cookie_authenticates_until_it_expires() {
        let state = test_state();
        let parts = parts_with((
            "cookie",
            &format!(
                "hydra_admin_session={}",
                state.session.mint(unix_now() + 60)
            ),
        ));
        assert!(session_ok(&parts, &state));

        let expired = parts_with((
            "cookie",
            &format!("hydra_admin_session={}", state.session.mint(unix_now() - 1)),
        ));
        assert!(!session_ok(&expired, &state));
    }

    #[tokio::test]
    async fn a_cookie_from_another_password_does_not_authenticate() {
        let token = test_state().session.mint(unix_now() + 60);
        let rotated = AppState::new(
            crate::db::Db::from(
                sqlx::sqlite::SqlitePoolOptions::new()
                    .connect_lazy("sqlite::memory:")
                    .unwrap(),
            ),
            "admin".to_string(),
            "new-password".to_string(),
            false,
            crate::config::TrustedProxies::default(),
            crate::flood::FloodCap::default(),
        );

        let parts = parts_with(("cookie", &format!("hydra_admin_session={token}")));
        assert!(!session_ok(&parts, &rotated));
    }

    #[tokio::test]
    async fn a_tampered_or_forged_cookie_does_not_authenticate() {
        let state = test_state();
        let mut tampered = state.session.mint(unix_now() + 60);
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });

        for token in [tampered.as_str(), "forged", ""] {
            let parts = parts_with(("cookie", &format!("hydra_admin_session={token}")));
            assert!(!session_ok(&parts, &state), "{token:?} must not verify");
        }
    }

    #[tokio::test]
    async fn basic_credentials_authenticate_without_a_cookie() {
        let state = test_state();
        let parts = parts_with(("authorization", &basic_header("admin:secret")));
        assert!(session_ok(&parts, &state));

        let wrong = parts_with(("authorization", &basic_header("admin:nope")));
        assert!(!session_ok(&wrong, &state));

        let absent = parts_with(("accept", "*/*"));
        assert!(!session_ok(&absent, &state));
    }

    #[test]
    fn the_rejection_is_a_redirect_for_pages_and_an_htmx_redirect_for_fragments() {
        let page = UiAuthError { htmx: false }.into_response();
        assert_eq!(page.status(), StatusCode::SEE_OTHER);
        assert_eq!(page.headers()[axum::http::header::LOCATION], LOGIN_PATH);

        let fragment = UiAuthError { htmx: true }.into_response();
        assert_eq!(fragment.status(), StatusCode::OK);
        assert_eq!(fragment.headers()["hx-redirect"], LOGIN_PATH);
    }
}
