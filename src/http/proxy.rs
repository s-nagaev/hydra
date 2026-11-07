//! OpenAI-compatible proxy handlers.
//!
//! `POST /v1/chat/completions` and `GET /v1/models` resolve the client key
//! to its bound channel, enforce the channel's model whitelist, and forward
//! to the upstream with the client `Authorization` dropped and the upstream
//! key substituted. The chat body is buffered once (chat bodies are small)
//! so `model` and `stream` can be peeked, then the original bytes are
//! forwarded verbatim. Responses pass through untouched; streaming ones use
//! `bytes_stream()` straight into `Body::from_stream` so SSE chunk semantics
//! survive. The upstream-to-client response conversion lives in one place so
//! the background logging task can tee into it later without touching the
//! forwarding paths.

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::db::channels::{self, Channel};
use crate::http::openai_error::openai_error;
use crate::http::proxy_auth::ProxyAuth;
use crate::http::AppState;

/// Total-request cap for non-streaming chat calls. Generous on purpose:
/// reasoning models can think for minutes before the single response body
/// arrives.
const NON_STREAM_TIMEOUT: Duration = Duration::from_secs(300);
/// Total-request cap for the `/v1/models` proxy path; model listings are
/// quick and never stream.
const MODELS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("{0}")]
    BadRequest(String),
    #[error("model {0} is not allowed by the channel whitelist")]
    ModelNotAllowed(String),
    #[error("the channel bound to this API key does not exist")]
    ChannelGone,
    #[error("upstream request failed")]
    UpstreamUnavailable,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        match self {
            ProxyError::BadRequest(message) => openai_error(
                StatusCode::BAD_REQUEST,
                &message,
                "invalid_request_error",
                None,
            ),
            ProxyError::ModelNotAllowed(model) => openai_error(
                StatusCode::NOT_FOUND,
                &format!("The model `{model}` does not exist or you do not have access to it."),
                "invalid_request_error",
                Some("model_not_found"),
            ),
            ProxyError::ChannelGone => openai_error(
                StatusCode::BAD_GATEWAY,
                "The channel bound to this API key is unavailable",
                "api_error",
                None,
            ),
            ProxyError::UpstreamUnavailable => openai_error(
                StatusCode::BAD_GATEWAY,
                "Failed to connect to the upstream provider",
                "api_error",
                None,
            ),
            ProxyError::Database(e) => {
                tracing::error!(error = %e, "proxy database query failed");
                openai_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error",
                    "server_error",
                    None,
                )
            }
        }
    }
}

/// `POST /v1/chat/completions`: peek `model`/`stream`, check the whitelist,
/// forward the original bytes with the upstream key, pass the response
/// through byte-for-byte.
pub async fn chat_completions(
    State(state): State<AppState>,
    auth: ProxyAuth,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ProxyError> {
    let (model, stream_requested) = peek_request(&body)?;
    let channel = resolve_channel(&state, &auth).await?;
    ensure_model_allowed(&channel, &model)?;

    let url = join_url(&channel.base_url, "/v1/chat/completions");
    let mut request = state
        .client
        .post(&url)
        .header(header::AUTHORIZATION, format!("Bearer {}", channel.api_key));
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    if let Some(accept) = headers.get(header::ACCEPT) {
        request = request.header(header::ACCEPT, accept);
    }
    // Only the non-streaming path gets a total timeout; a cap on a stream
    // would cut long generations short between chunks.
    if !stream_requested {
        request = request.timeout(NON_STREAM_TIMEOUT);
    }

    let upstream = request.body(body).send().await.map_err(|e| {
        tracing::warn!(error = %e, "upstream chat request failed");
        ProxyError::UpstreamUnavailable
    })?;
    upstream_to_response(upstream, stream_requested).await
}

/// `GET /v1/models`: a set whitelist is returned as a locally built OpenAI
/// list; a NULL or empty whitelist proxies the upstream list with the same
/// key substitution as chat.
pub async fn models(
    State(state): State<AppState>,
    auth: ProxyAuth,
    headers: HeaderMap,
) -> Result<Response, ProxyError> {
    let channel = resolve_channel(&state, &auth).await?;
    match channel.allowed_models.as_deref() {
        Some(models) if !models.is_empty() => Ok(local_model_list(models)),
        _ => {
            let url = join_url(&channel.base_url, "/v1/models");
            let mut request = state
                .client
                .get(&url)
                .header(header::AUTHORIZATION, format!("Bearer {}", channel.api_key))
                .timeout(MODELS_TIMEOUT);
            if let Some(accept) = headers.get(header::ACCEPT) {
                request = request.header(header::ACCEPT, accept);
            }
            let upstream = request.send().await.map_err(|e| {
                tracing::warn!(error = %e, "upstream models request failed");
                ProxyError::UpstreamUnavailable
            })?;
            upstream_to_response(upstream, false).await
        }
    }
}

#[derive(Serialize)]
struct ModelEntry {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

#[derive(Serialize)]
struct ModelList {
    object: &'static str,
    data: Vec<ModelEntry>,
}

/// Builds the local OpenAI model list from the whitelist, sorted for stable
/// output regardless of the stored order.
fn local_model_list(models: &[String]) -> Response {
    let mut sorted: Vec<&String> = models.iter().collect();
    sorted.sort();
    let data = sorted
        .into_iter()
        .map(|model| ModelEntry {
            id: model.clone(),
            object: "model",
            created: 0,
            owned_by: "hydra",
        })
        .collect();
    Json(ModelList {
        object: "list",
        data,
    })
    .into_response()
}

/// Extracts `model` and `stream` from the buffered body without rewriting
/// it; the original bytes are what gets forwarded. A missing or non-string
/// `model` mirrors OpenAI's 400; `stream` defaults to false.
fn peek_request(body: &Bytes) -> Result<(String, bool), ProxyError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        ProxyError::BadRequest("Invalid request body: expected a JSON object".to_string())
    })?;
    let model = value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ProxyError::BadRequest("you must provide a model parameter".to_string()))?;
    let stream = value
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok((model.to_string(), stream))
}

/// A NULL or empty whitelist allows every model; otherwise the requested
/// model must match a whitelist entry exactly.
fn ensure_model_allowed(channel: &Channel, model: &str) -> Result<(), ProxyError> {
    match channel.allowed_models.as_deref() {
        Some(models) if !models.is_empty() => models
            .iter()
            .any(|allowed| allowed == model)
            .then_some(())
            .ok_or_else(|| ProxyError::ModelNotAllowed(model.to_string())),
        _ => Ok(()),
    }
}

/// Loads the channel bound to the authenticated key. The FK plus the
/// bound-keys delete guard make a missing row nearly impossible; if it does
/// happen the caller sees a 502 rather than a panic or silent fallback.
async fn resolve_channel(state: &AppState, auth: &ProxyAuth) -> Result<Channel, ProxyError> {
    channels::find(&state.db, auth.key.channel_id)
        .await?
        .ok_or(ProxyError::ChannelGone)
}

/// Joins a channel base URL with an API path so a trailing slash on the
/// stored URL or a leading slash on the path never produces a double slash
/// or drops the path.
fn join_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Converts an upstream response into the client response: status and
/// Content-Type pass through verbatim, the body either streams chunk by
/// chunk (no buffering) or is forwarded as-is. This is the single point
/// where the future background logging tee will hook in.
async fn upstream_to_response(
    upstream: reqwest::Response,
    stream_requested: bool,
) -> Result<Response, ProxyError> {
    let status = upstream.status();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    // Even when the client did not ask for SSE, an event-stream upstream
    // must not be buffered.
    let is_event_stream = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    let streaming = stream_requested || is_event_stream;

    let mut response = Response::builder().status(status);
    if let Some(content_type) = content_type {
        response = response.header(header::CONTENT_TYPE, content_type);
    }
    if streaming {
        Ok(response
            .body(Body::from_stream(upstream.bytes_stream()))
            // The status and header values all come from the already
            // parsed upstream response, so construction cannot fail.
            .expect("upstream status and headers are valid"))
    } else {
        let bytes = upstream.bytes().await.map_err(|e| {
            tracing::warn!(error = %e, "upstream response body read failed");
            ProxyError::UpstreamUnavailable
        })?;
        Ok(response
            .body(Body::from(bytes))
            .expect("upstream status and headers are valid"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_url_handles_trailing_slashes() {
        assert_eq!(
            join_url("http://127.0.0.1:9", "/v1/chat/completions"),
            "http://127.0.0.1:9/v1/chat/completions"
        );
        assert_eq!(
            join_url("http://127.0.0.1:9/", "/v1/chat/completions"),
            "http://127.0.0.1:9/v1/chat/completions"
        );
        assert_eq!(
            join_url("http://127.0.0.1:9///", "/v1/models"),
            "http://127.0.0.1:9/v1/models"
        );
    }

    #[test]
    fn peek_request_extracts_model_and_stream_without_defaults_leaking() {
        let body = Bytes::from(r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#);
        let (model, stream) = peek_request(&body).unwrap();
        assert_eq!(model, "gpt-4o");
        assert!(!stream);

        let body = Bytes::from(r#"{"model":"gpt-4o","stream":true,"messages":[]}"#);
        let (model, stream) = peek_request(&body).unwrap();
        assert_eq!(model, "gpt-4o");
        assert!(stream);
    }

    #[test]
    fn peek_request_rejects_missing_model_and_bad_json() {
        let missing = Bytes::from(r#"{"messages":[]}"#);
        assert!(matches!(
            peek_request(&missing),
            Err(ProxyError::BadRequest(_))
        ));
        let not_string = Bytes::from(r#"{"model":42}"#);
        assert!(matches!(
            peek_request(&not_string),
            Err(ProxyError::BadRequest(_))
        ));
        let garbage = Bytes::from_static(b"not json at all");
        assert!(matches!(
            peek_request(&garbage),
            Err(ProxyError::BadRequest(_))
        ));
    }

    #[test]
    fn whitelist_semantics_allow_null_empty_and_exact_match() {
        let mut channel = Channel {
            id: 1,
            name: "test".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "secret".to_string(),
            allowed_models: None,
            enabled: true,
            created_at: String::new(),
        };
        assert!(ensure_model_allowed(&channel, "anything").is_ok());

        channel.allowed_models = Some(vec![]);
        assert!(ensure_model_allowed(&channel, "anything").is_ok());

        channel.allowed_models = Some(vec!["gpt-4o".to_string()]);
        assert!(ensure_model_allowed(&channel, "gpt-4o").is_ok());
        assert!(matches!(
            ensure_model_allowed(&channel, "gpt-4"),
            Err(ProxyError::ModelNotAllowed(model)) if model == "gpt-4"
        ));
    }

    #[tokio::test]
    async fn local_model_list_sorts_entries_and_uses_openai_shape() {
        let models = vec!["zeta".to_string(), "alpha".to_string()];
        let response = local_model_list(&models);
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["object"], "list");
        assert_eq!(value["data"][0]["id"], "alpha");
        assert_eq!(value["data"][0]["object"], "model");
        assert_eq!(value["data"][0]["created"], 0);
        assert_eq!(value["data"][0]["owned_by"], "hydra");
        assert_eq!(value["data"][1]["id"], "zeta");
    }
}
