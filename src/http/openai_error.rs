//! OpenAI error body construction for the proxy routes.
//!
//! Every failure surfaced by `/v1/*` uses the upstream-compatible shape
//! `{"error":{"message","type","code"}}` so existing OpenAI client libraries
//! can parse it. Admin routes deliberately keep their plain `{"error": "..."}`
//! shape. `code` serializes to `null` where OpenAI itself sends no code.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Builds an OpenAI-shaped error response with the given status code.
pub fn openai_error(
    status: StatusCode,
    message: &str,
    error_type: &str,
    code: Option<&str>,
) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": error_type,
                "code": code,
            }
        })),
    )
        .into_response()
}
