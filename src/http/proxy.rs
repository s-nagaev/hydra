//! OpenAI-compatible proxy handlers.
//!
//! `POST /v1/chat/completions` and `GET /v1/models` resolve the client key
//! to its bound channel, enforce the channel's model whitelist, and forward
//! to the upstream with the client `Authorization` dropped and the upstream
//! key substituted. The chat body is buffered once, bounded by
//! [`CHAT_BODY_LIMIT`] (32 MiB, well past axum's 2 MiB default because
//! vision requests embed base64 images), so `model` and `stream` can be
//! peeked, then the original bytes are forwarded verbatim. A body over the
//! limit fails through the [`ChatBody`] extractor as an OpenAI-shaped 413,
//! never axum's plain-text rejection, so client libraries always see a
//! parseable error. Responses pass through untouched; streaming ones use
//! `bytes_stream()` straight into `Body::from_stream` so SSE chunk semantics
//! survive. The upstream-to-client response conversion lives in one place
//! ([`upstream_to_response`]), which is also where the background request
//! logging tee hooks in: every chat completion produces exactly one
//! `request_logs` row written by a detached task that can never block or
//! fail the client flow. `/v1/models` is a lightweight discovery endpoint
//! and is not logged.

use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

use crate::db::channels::{self, Channel};
use crate::http::docs::OpenAiError;
use crate::http::openai_error::openai_error;
use crate::http::proxy_auth::ProxyAuth;
use crate::http::AppState;
use crate::logging::{self, ChatLogContext, ChatLogInit};

/// Maximum accepted chat request body, in bytes (32 MiB). Vision requests
/// embed base64-encoded images and long-context conversations run well past
/// axum's 2 MiB default; 32 MiB covers both while still bounding memory.
/// Applied to `/v1/chat/completions` via `DefaultBodyLimit`; an over-limit
/// body fails into [`ProxyError::PayloadTooLarge`].
pub const CHAT_BODY_LIMIT: usize = 32 * 1024 * 1024;

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
    #[error("the chat request body exceeded the body limit")]
    PayloadTooLarge,
    #[error("model {0} is not allowed by the channel whitelist")]
    ModelNotAllowed(String),
    #[error("the channel bound to this API key does not exist")]
    ChannelGone,
    #[error("the channel bound to this API key is disabled")]
    ChannelDisabled,
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
            ProxyError::PayloadTooLarge => openai_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!(
                    "Request body exceeds the maximum allowed size of {} MiB",
                    CHAT_BODY_LIMIT / (1024 * 1024)
                ),
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
            ProxyError::ChannelDisabled => openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The channel bound to this API key is currently disabled",
                "api_error",
                Some("channel_disabled"),
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

/// Buffered chat request body. Wrapping `Bytes` in a dedicated extractor is
/// what makes an over-limit body surface as an OpenAI-shaped 413 (via
/// [`ProxyError::PayloadTooLarge`]) instead of axum's default plain-text
/// rejection, which OpenAI client libraries cannot parse.
pub struct ChatBody(Bytes);

impl<S> FromRequest<S> for ChatBody
where
    S: Send + Sync,
{
    type Rejection = ProxyError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(ChatBody(bytes)),
            Err(rejection) => {
                // `Bytes` only rejects on the length limit (413) or an
                // unreadable body; the latter keeps the same 400 wording as
                // malformed JSON, which the handler reports after the peek.
                let status = rejection.into_response().status();
                if status == StatusCode::PAYLOAD_TOO_LARGE {
                    Err(ProxyError::PayloadTooLarge)
                } else {
                    Err(ProxyError::BadRequest(
                        "Invalid request body: expected a JSON object".to_string(),
                    ))
                }
            }
        }
    }
}

/// `POST /v1/chat/completions`: peek `model`/`stream`, check the whitelist,
/// forward the original bytes with the upstream key, pass the response
/// through byte-for-byte, and leave exactly one background log row behind
/// whatever the outcome.
#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "Proxy",
    operation_id = "chat_completions",
    summary = "Create a chat completion",
    description = "Forwards an OpenAI Chat Completions request to the \
        channel bound to the key, with the client Authorization dropped and \
        the upstream key substituted. The body passes through byte-for-byte, \
        so anything the upstream accepts works here; the two fields Hydra \
        itself reads are `model` (whitelist check) and `stream` (streaming \
        path). Upstream responses, errors included, come back with the \
        upstream status and body unchanged. Every call leaves exactly one \
        row in the request log.",
    security(("bearer_auth" = [])),
    request_body(
        content = Object,
        description = "Any OpenAI Chat Completions request body. `model` is \
            required; `stream: true` switches the response to \
            text/event-stream. Bodies above 32 MiB are rejected with 413.",
        examples(
            ("plain" = (
                summary = "Non-streaming request",
                value = json!({
                    "model": "gpt-4o",
                    "messages": [{ "role": "user", "content": "Say hello" }]
                })
            )),
            ("streaming" = (
                summary = "Streaming request (SSE response)",
                value = json!({
                    "model": "gpt-4o",
                    "messages": [{ "role": "user", "content": "Tell me a joke" }],
                    "stream": true
                })
            )),
            ("invalid" = (
                summary = "Rejected with 400: no model parameter",
                value = json!({
                    "messages": [{ "role": "user", "content": "Hi" }]
                })
            ))
        )
    ),
    responses(
        (status = 200, description = "The upstream response passed through \
            unchanged; with `stream: true` the content type is \
            text/event-stream and chunks arrive as the upstream sends them", body = Object, example = json!({
            "id": "chatcmpl-A1b2C3",
            "object": "chat.completion",
            "created": 1770000000,
            "model": "gpt-4o",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": "Hello!" },
                    "finish_reason": "stop"
                }
            ],
            "usage": { "prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46 }
        })),
        (status = 400, description = "Body is not JSON or has no string \
            `model` parameter", body = OpenAiError, example = json!({
            "error": { "message": "you must provide a model parameter", "type": "invalid_request_error", "code": null }
        })),
        (status = 401, description = "Missing, malformed, unknown or deleted \
            key", body = OpenAiError, example = json!({
            "error": { "message": "Invalid API key provided", "type": "invalid_request_error", "code": "invalid_api_key" }
        })),
        (status = 403, description = "The key exists but is disabled", body = OpenAiError, example = json!({
            "error": { "message": "This API key has been disabled", "type": "invalid_request_error", "code": "key_disabled" }
        })),
        (status = 404, description = "The requested model is not in the \
            channel's whitelist", body = OpenAiError, example = json!({
            "error": {
                "message": "The model `gpt-4` does not exist or you do not have access to it.",
                "type": "invalid_request_error",
                "code": "model_not_found"
            }
        })),
        (status = 413, description = "The request body exceeds 32 MiB", body = OpenAiError, example = json!({
            "error": { "message": "Request body exceeds the maximum allowed size of 32 MiB", "type": "invalid_request_error", "code": null }
        })),
        (status = 502, description = "The bound channel is gone, or the \
            upstream could not be reached; other upstream statuses pass \
            through unchanged", body = OpenAiError, example = json!({
            "error": { "message": "Failed to connect to the upstream provider", "type": "api_error", "code": null }
        })),
        (status = 503, description = "The channel bound to the key is \
            disabled; the key itself stays valid", body = OpenAiError, example = json!({
            "error": {
                "message": "The channel bound to this API key is currently disabled",
                "type": "api_error",
                "code": "channel_disabled"
            }
        }))
    )
)]
pub async fn chat_completions(
    State(state): State<AppState>,
    auth: ProxyAuth,
    headers: HeaderMap,
    ChatBody(body): ChatBody,
) -> Result<Response, ProxyError> {
    let start = Instant::now();
    let (model, stream_requested) = match peek_request(&body) {
        Ok(peeked) => peeked,
        Err(e) => {
            log_rejection(&state, &auth, None, start, &body, &e);
            return Err(e);
        }
    };
    let channel = match resolve_channel(&state, &auth).await {
        Ok(channel) => channel,
        // When the database itself is failing there is no point attempting a
        // log write; ChannelGone and ChannelDisabled are client-visible
        // routing failures that still get their audit row.
        Err(e @ (ProxyError::ChannelGone | ProxyError::ChannelDisabled)) => {
            log_rejection(&state, &auth, Some(model), start, &body, &e);
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    if let Err(e) = ensure_model_allowed(&channel, &model) {
        log_rejection(&state, &auth, Some(model), start, &body, &e);
        return Err(e);
    }

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

    // Built before the body is handed to the forwarder: it already holds
    // whatever request bytes body logging will ever capture.
    let ctx = chat_log_context(&state, &auth, Some(model), start, &body);
    let upstream = match request.body(body).send().await {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(error = %e, "upstream chat request failed");
            ctx.finish(
                None,
                None,
                None,
                None,
                Some(format!("upstream request failed: {e}")),
            );
            return Err(ProxyError::UpstreamUnavailable);
        }
    };
    upstream_to_response(upstream, stream_requested, Some(ctx)).await
}

/// `GET /v1/models`: a set whitelist is returned as a locally built OpenAI
/// list; a NULL or empty whitelist proxies the upstream list with the same
/// key substitution as chat.
#[utoipa::path(
    get,
    path = "/v1/models",
    tag = "Proxy",
    operation_id = "list_models",
    summary = "List models",
    description = "Serves model discovery for the key's channel. A channel \
        with a whitelist gets a locally built OpenAI list containing exactly \
        the whitelisted ids; a channel without one proxies the upstream \
        listing with the same key substitution as chat. This endpoint is \
        not logged.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "OpenAI model list, either the \
            whitelisted ids built locally or the upstream listing passed \
            through", body = ModelList, example = json!({
            "object": "list",
            "data": [
                { "id": "gpt-4o", "object": "model", "created": 0, "owned_by": "hydra" },
                { "id": "gpt-4o-mini", "object": "model", "created": 0, "owned_by": "hydra" }
            ]
        })),
        (status = 401, description = "Missing, malformed, unknown or deleted \
            key", body = OpenAiError, example = json!({
            "error": { "message": "Invalid API key provided", "type": "invalid_request_error", "code": "invalid_api_key" }
        })),
        (status = 403, description = "The key exists but is disabled", body = OpenAiError, example = json!({
            "error": { "message": "This API key has been disabled", "type": "invalid_request_error", "code": "key_disabled" }
        })),
        (status = 502, description = "The upstream models listing could not \
            be fetched, or the bound channel is gone", body = OpenAiError, example = json!({
            "error": { "message": "Failed to connect to the upstream provider", "type": "api_error", "code": null }
        })),
        (status = 503, description = "The channel bound to the key is \
            disabled", body = OpenAiError, example = json!({
            "error": {
                "message": "The channel bound to this API key is currently disabled",
                "type": "api_error",
                "code": "channel_disabled"
            }
        }))
    )
)]
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
            upstream_to_response(upstream, false, None).await
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ModelEntry {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ModelList {
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
/// A disabled channel keeps its keys and their log history but must not
/// serve traffic; its callers see a 503 so the key itself stays valid.
async fn resolve_channel(state: &AppState, auth: &ProxyAuth) -> Result<Channel, ProxyError> {
    let channel = channels::find(&state.db, auth.key.channel_id)
        .await?
        .ok_or(ProxyError::ChannelGone)?;
    if !channel.enabled {
        return Err(ProxyError::ChannelDisabled);
    }
    Ok(channel)
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
/// chunk (no buffering) or is forwarded as-is. With a log context this is
/// also the single point that finishes the request's log row: streaming
/// bodies are teed into the bounded capture buffer, buffered bodies are
/// captured directly after usage parsing. `None` (the `/v1/models` path)
/// skips logging entirely.
async fn upstream_to_response(
    upstream: reqwest::Response,
    stream_requested: bool,
    log: Option<ChatLogContext>,
) -> Result<Response, ProxyError> {
    let status = upstream.status();
    let status_code = i64::from(status.as_u16());
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
        let body = match log {
            Some(ctx) => Body::from_stream(logging::tee_stream(
                upstream.bytes_stream(),
                status_code,
                ctx,
            )),
            None => Body::from_stream(upstream.bytes_stream()),
        };
        Ok(response
            .body(body)
            // The status and header values all come from the already
            // parsed upstream response, so construction cannot fail.
            .expect("upstream status and headers are valid"))
    } else {
        let bytes = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "upstream response body read failed");
                if let Some(ctx) = log {
                    ctx.finish(
                        Some(status_code),
                        None,
                        None,
                        None,
                        Some("upstream response body read failed".to_string()),
                    );
                }
                return Err(ProxyError::UpstreamUnavailable);
            }
        };
        if let Some(ctx) = log {
            let usage = logging::parse_usage(&bytes);
            let response_body = ctx.capture_response_body(&bytes);
            ctx.finish(
                Some(status_code),
                usage.prompt_tokens,
                usage.completion_tokens,
                response_body,
                None,
            );
        }
        Ok(response
            .body(Body::from(bytes))
            .expect("upstream status and headers are valid"))
    }
}

/// Gathers the log context for one chat request from the request's own
/// pieces; the channel id comes from the authenticated key, which is what
/// the log row must reference even if channel resolution later fails.
fn chat_log_context(
    state: &AppState,
    auth: &ProxyAuth,
    model: Option<String>,
    start: Instant,
    body: &Bytes,
) -> ChatLogContext {
    ChatLogContext::new(ChatLogInit {
        db: &state.db,
        tracker: &state.log_tracker,
        log_bodies: state.log_bodies,
        api_key_id: auth.key.id,
        channel_id: auth.key.channel_id,
        model,
        start,
        request_body: body,
    })
}

/// Writes the audit row for an authenticated chat request rejected before
/// the upstream was contacted. Rejections from the auth layer (401/403) and
/// from the body extractor (413) happen before the handler and are not
/// logged: they have no key row or no readable body to reference.
fn log_rejection(
    state: &AppState,
    auth: &ProxyAuth,
    model: Option<String>,
    start: Instant,
    body: &Bytes,
    error: &ProxyError,
) {
    let ctx = chat_log_context(state, auth, model, start, body);
    ctx.finish(None, None, None, None, Some(error.to_string()));
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
