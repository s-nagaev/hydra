//! Cross-site request guard for mutating admin UI requests.
//!
//! Basic Auth credentials are attached automatically by the browser to any
//! request the page origin can trigger, including form POSTs issued by a
//! hostile page, so authentication alone does not stop cross-site writes.
//! This layer rejects mutating requests (POST, PUT, PATCH, DELETE) whose
//! browser-provided metadata marks them as cross-site: a `Sec-Fetch-Site`
//! header other than `same-origin`, or an `Origin` header that disagrees
//! with the request `Host`. Requests carrying neither header (plain HTTP
//! clients) pass through; the auth extractors still gate them. The guard is
//! applied to the whole `/admin/ui` router, so every mutating route added
//! there is covered without per-handler wiring, the login form included.

use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Minimal static page for rejected cross-site requests.
const FORBIDDEN_PAGE: &str = "<!DOCTYPE html>\
<html lang=\"en\">\
<head><meta charset=\"utf-8\"><title>403 Forbidden</title></head>\
<body>\
<h1>403 Forbidden</h1>\
<p>Cross-site request rejected. Admin UI forms accept same-origin requests only.</p>\
</body>\
</html>";

/// Middleware guard: rejects cross-site mutating requests with 403.
pub async fn guard(req: Request, next: Next) -> Response {
    if is_mutating(req.method()) && !same_origin_ok(req.headers()) {
        return (StatusCode::FORBIDDEN, FORBIDDEN_PAGE).into_response();
    }
    next.run(req).await
}

fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// Decides, from the request headers, whether a mutating request is
/// same-origin: `Sec-Fetch-Site` wins when present, otherwise the `Origin`
/// header must name the request `Host`.
fn same_origin_ok(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site == "same-origin";
    }
    match (headers.get(header::ORIGIN), headers.get(header::HOST)) {
        (Some(origin), Some(host)) => origin_matches_host(origin, host),
        // No Origin at all: a plain client, not a browser form submission.
        (None, _) => true,
        // Mutating request with Origin but no Host: malformed, reject.
        (Some(_), None) => false,
    }
}

/// Compares an `Origin` header value against a `Host` header value,
/// ignoring default ports implied by the origin scheme.
fn origin_matches_host(origin: &HeaderValue, host: &HeaderValue) -> bool {
    let (origin, host) = match (origin.to_str(), host.to_str()) {
        (Ok(origin), Ok(host)) => (origin, host),
        _ => return false,
    };
    if origin.eq_ignore_ascii_case("null") {
        return false;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    strip_default_port(authority, default_port)
        .eq_ignore_ascii_case(strip_default_port(host, default_port))
}

/// Drops a trailing default port so `example.com:80` and `example.com`
/// compare equal; non-default ports are kept.
fn strip_default_port(authority: &str, default_port: Option<u16>) -> &str {
    let Some(port) = default_port else {
        return authority;
    };
    let Some((host, tail)) = authority.rsplit_once(':') else {
        return authority;
    };
    if tail.parse::<u16>() == Ok(port) {
        host
    } else {
        authority
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn post_request(sec_fetch_site: Option<&str>, origin: Option<&str>) -> Request {
        let mut builder = HttpRequest::post("/probe");
        if let Some(site) = sec_fetch_site {
            builder = builder.header("Sec-Fetch-Site", site);
        }
        if let Some(origin) = origin {
            builder = builder.header("Origin", origin);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn mutating_methods_are_post_put_patch_delete() {
        assert!(is_mutating(&Method::POST));
        assert!(is_mutating(&Method::PUT));
        assert!(is_mutating(&Method::PATCH));
        assert!(is_mutating(&Method::DELETE));
        assert!(!is_mutating(&Method::GET));
        assert!(!is_mutating(&Method::HEAD));
        assert!(!is_mutating(&Method::OPTIONS));
    }

    #[test]
    fn same_origin_ok_accepts_same_origin_fetch_metadata() {
        let headers = HeaderMap::new();
        // No metadata at all: plain client.
        assert!(same_origin_ok(&headers));

        let mut headers = HeaderMap::new();
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(same_origin_ok(&headers));

        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!same_origin_ok(&headers));

        headers.insert("sec-fetch-site", "same-site".parse().unwrap());
        assert!(!same_origin_ok(&headers));

        headers.insert("sec-fetch-site", "none".parse().unwrap());
        assert!(!same_origin_ok(&headers));
    }

    #[test]
    fn same_origin_ok_checks_origin_against_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        headers.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        assert!(same_origin_ok(&headers));

        headers.insert(header::HOST, "evil.example".parse().unwrap());
        assert!(!same_origin_ok(&headers));
    }

    #[test]
    fn origin_matches_host_ignores_default_ports_and_case() {
        let origin = HeaderValue::from_static("http://example.com");
        let host = HeaderValue::from_static("example.com:80");
        assert!(origin_matches_host(&origin, &host));

        let origin = HeaderValue::from_static("https://Example.com:443");
        let host = HeaderValue::from_static("example.com");
        assert!(origin_matches_host(&origin, &host));

        let origin = HeaderValue::from_static("http://example.com:8080");
        let host = HeaderValue::from_static("example.com");
        assert!(!origin_matches_host(&origin, &host));

        let origin = HeaderValue::from_static("null");
        let host = HeaderValue::from_static("example.com");
        assert!(!origin_matches_host(&origin, &host));
    }

    async fn guard_response(request: Request) -> (axum::http::StatusCode, Vec<u8>) {
        let router = axum::Router::new()
            .route(
                "/probe",
                axum::routing::get(|| async { "fetched" }).post(|| async { "created" }),
            )
            .layer(axum::middleware::from_fn(guard));
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn guard_rejects_cross_site_post_with_403_html() {
        let request = post_request(Some("cross-site"), None);
        let (status, body) = guard_response(request).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        assert!(String::from_utf8(body).unwrap().contains("same-origin"));
    }

    #[tokio::test]
    async fn guard_lets_same_origin_post_through() {
        let request = post_request(Some("same-origin"), None);
        let (status, body) = guard_response(request).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body, b"created");
    }

    #[tokio::test]
    async fn guard_rejects_mismatched_origin_post() {
        let mut request = post_request(None, Some("http://evil.example"));
        request
            .headers_mut()
            .insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        let (status, _) = guard_response(request).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn guard_lets_cross_site_get_through() {
        let mut request = post_request(Some("cross-site"), None);
        *request.method_mut() = Method::GET;
        let (status, _) = guard_response(request).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }
}
