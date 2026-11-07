//! Basic Auth protection for admin routes.
//!
//! The [`AdminAuth`] extractor validates the `Authorization` header against
//! the credentials from `HYDRA_ADMIN_LOGIN` / `HYDRA_ADMIN_PASSWORD` using a
//! constant-time comparison. Every admin handler takes it as its first
//! extractor; `GET /admin/health` deliberately does not. Failed
//! authentication responds with 401 and a `WWW-Authenticate: Basic
//! realm="hydra"` challenge and an empty body.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use subtle::ConstantTimeEq;

use crate::http::AppState;

/// Extractor marker proving the request carried valid admin credentials.
pub struct AdminAuth;

#[derive(Debug, thiserror::Error)]
#[error("unauthorized")]
pub struct AdminAuthError;

impl IntoResponse for AdminAuthError {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [("WWW-Authenticate", r#"Basic realm="hydra""#)],
        )
            .into_response()
    }
}

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = AdminAuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (login, password) = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_basic)
            .ok_or(AdminAuthError)?;

        let login_ok = constant_time_eq(login.as_bytes(), state.admin_login.as_bytes());
        let password_ok = constant_time_eq(password.as_bytes(), state.admin_password.as_bytes());
        if login_ok && password_ok {
            Ok(AdminAuth)
        } else {
            Err(AdminAuthError)
        }
    }
}

fn parse_basic(header: &str) -> Option<(String, String)> {
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (login, password) = decoded.split_once(':')?;
    Some((login.to_string(), password.to_string()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    #[test]
    fn parse_basic_splits_credentials_on_first_colon() {
        let header = format!("Basic {}", STANDARD.encode("admin:se:cret"));
        let (login, password) = parse_basic(&header).unwrap();
        assert_eq!(login, "admin");
        assert_eq!(password, "se:cret");
    }

    #[test]
    fn parse_basic_rejects_missing_prefix_and_bad_base64() {
        assert!(parse_basic("Bearer abc").is_none());
        assert!(parse_basic("Basic !!!not-base64!!!").is_none());
        assert!(parse_basic(&format!("Basic {}", STANDARD.encode("no-colon"))).is_none());
    }

    #[test]
    fn constant_time_eq_matches_only_identical_slices() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }
}
