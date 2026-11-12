//! Error envelopes for the proxy routes.
//!
//! Every rejection Hydra itself produces on `/v1/*` is JSON in one of two
//! shapes: the OpenAI shape `{"error":{"message","type","code"}}`, which the
//! chat, Responses and models clients parse, and the Anthropic shape
//! `{"type":"error","error":{"type","message"}}`, which the Anthropic SDKs
//! pointed at `/v1/messages` expect. Which shape a request gets is a
//! property of the surface it was sent to (`Surface::errors`); the status
//! and the message of a rejection are the same in both. An upstream error
//! body is never re-shaped: it passes through as the upstream wrote it.
//! Admin routes deliberately keep their plain `{"error": "..."}` shape, and
//! `code` serializes to `null` where OpenAI itself sends no code.
//!
//! The Anthropic error type is derived from the rejection status, because
//! the status is already Hydra's own classification of what failed. The
//! mapping the Messages surface renders:
//!
//! | rejection | status | Anthropic `error.type` |
//! | --- | --- | --- |
//! | malformed or incomplete request | 400 | `invalid_request_error` |
//! | missing, malformed or unknown key | 401 | `authentication_error` |
//! | disabled key | 403 | `permission_error` |
//! | model outside the channel whitelist | 404 | `invalid_request_error` |
//! | body over the limit | 413 | `invalid_request_error` |
//! | flood cap, spent monthly limit | 429 | `rate_limit_error` |
//! | storage failure | 500 | `api_error` |
//! | channel gone, upstream unreachable | 502 | `overloaded_error` |
//! | channel disabled | 503 | `overloaded_error` |
//!
//! The Anthropic envelope carries no `code` field: Anthropic error objects
//! have `type` and `message` only, and the OpenAI `code` values are not part
//! of that taxonomy.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// The envelope one proxy surface renders its own rejections in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorShape {
    /// `{"error":{"message","type","code"}}`: the shape OpenAI client
    /// libraries parse, used by chat, Responses and models.
    OpenAi,
    /// `{"type":"error","error":{"type","message"}}`: the shape the
    /// Anthropic SDKs parse, used by the Messages surface.
    Anthropic,
}

impl ErrorShape {
    /// Renders one rejection in this shape. Status and message are the
    /// surface-independent part of a rejection, so both shapes are built
    /// from the same pieces; the Anthropic side maps the status to its own
    /// error type ([`anthropic_error_type`]) and drops the OpenAI `code`.
    pub fn render(
        self,
        status: StatusCode,
        message: &str,
        error_type: &str,
        code: Option<&str>,
    ) -> Response {
        match self {
            ErrorShape::OpenAi => openai_error(status, message, error_type, code),
            ErrorShape::Anthropic => anthropic_error(status, message),
        }
    }
}

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

/// Builds an Anthropic-shaped error response with the given status code.
pub fn anthropic_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": anthropic_error_type(status),
                "message": message,
            }
        })),
    )
        .into_response()
}

/// The Anthropic error type for a rejection status, per the table in the
/// module docs. A status outside the map falls back to `api_error`, the
/// Anthropic family for a failure on the server side: no unmapped status can
/// then be reported to the client as its own mistake.
fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::PAYLOAD_TOO_LARGE => {
            "invalid_request_error"
        }
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::INTERNAL_SERVER_ERROR => "api_error",
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE => "overloaded_error",
        _ => "api_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// The OpenAI envelope is unchanged: the same three fields, `code`
    /// serialized even when it is `null`.
    #[tokio::test]
    async fn the_openai_envelope_keeps_its_three_fields() {
        let response = ErrorShape::OpenAi.render(
            StatusCode::UNAUTHORIZED,
            "Invalid API key provided",
            "invalid_request_error",
            Some("invalid_api_key"),
        );
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body,
            json!({
                "error": {
                    "message": "Invalid API key provided",
                    "type": "invalid_request_error",
                    "code": "invalid_api_key"
                }
            })
        );
        assert_eq!(body.as_object().unwrap().len(), 1, "no `type` at the top");
    }

    /// The Anthropic envelope: `type: "error"` at the top, a nested `error`
    /// object with exactly `type` and `message`, and the status preserved.
    #[tokio::test]
    async fn the_anthropic_envelope_has_the_error_type_and_message_only() {
        let response = ErrorShape::Anthropic.render(
            StatusCode::UNAUTHORIZED,
            "Invalid API key provided",
            "invalid_request_error",
            Some("invalid_api_key"),
        );
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["message"], "Invalid API key provided");
        assert_eq!(
            body["error"].as_object().unwrap().len(),
            2,
            "no `code` field in the Anthropic envelope: {body}"
        );
    }

    /// One test per status the Messages surface can answer with, so the
    /// documented mapping table cannot drift from the code.
    #[tokio::test]
    async fn the_anthropic_error_type_follows_the_documented_status_mapping() {
        let cases = [
            (StatusCode::BAD_REQUEST, "invalid_request_error"),
            (StatusCode::UNAUTHORIZED, "authentication_error"),
            (StatusCode::FORBIDDEN, "permission_error"),
            (StatusCode::NOT_FOUND, "invalid_request_error"),
            (StatusCode::PAYLOAD_TOO_LARGE, "invalid_request_error"),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
            (StatusCode::BAD_GATEWAY, "overloaded_error"),
            (StatusCode::SERVICE_UNAVAILABLE, "overloaded_error"),
            // A status outside the map never comes from a rejection Hydra
            // renders, and it must not be reported as the client's fault.
            (StatusCode::IM_A_TEAPOT, "api_error"),
        ];
        for (status, expected) in cases {
            let (rendered_status, body) =
                body_of(ErrorShape::Anthropic.render(status, "message", "server_error", None))
                    .await;
            assert_eq!(rendered_status, status);
            assert_eq!(
                body["error"]["type"], expected,
                "status {status} must map to {expected}: {body}"
            );
        }
    }
}
