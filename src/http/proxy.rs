//! OpenAI-compatible proxy handlers.
//!
//! `POST /v1/chat/completions`, `POST /v1/responses`, `POST /v1/messages`
//! and `GET /v1/models` resolve the client key to its bound channel, enforce
//! the channel's model whitelist, and forward to the upstream with the client
//! credential dropped and the upstream key substituted. The three POST
//! surfaces run one shared pipeline ([`forward_proxied`]) and differ only in
//! the upstream path the bytes are forwarded to, in the usage shape their
//! response is read in (see [`Surface`]) and in the header policy that puts
//! the credential on the outbound request (see [`HeaderPolicy`]). The request
//! body is buffered once, bounded by
//! [`CHAT_BODY_LIMIT`] (32 MiB, well past axum's 2 MiB default because
//! vision requests embed base64 images), so `model` and `stream` can be
//! peeked, then the original bytes are forwarded verbatim. A body over the
//! limit fails through the [`ChatBody`] extractor as an OpenAI-shaped 413,
//! never axum's plain-text rejection, so client libraries always see a
//! parseable error. Responses pass through untouched; streaming ones use
//! `bytes_stream()` straight into `Body::from_stream` so SSE chunk semantics
//! survive. The upstream-to-client response conversion lives in one place
//! ([`upstream_to_response`]), which is also where the background request
//! logging tee hooks in: every proxied completion, on any POST surface,
//! produces exactly one `request_logs` row, written by a detached task that
//! can never block or fail the client flow. Rejections are logged too, auth-layer ones
//! included (they resolve to no key, so their rows carry NULL ids); the
//! only unloggable case is the 413 body-limit rejection, which happens in
//! the body extractor before any handler code runs. A request rejected by
//! the monthly-limit gate is logged through its own path, which keeps the
//! key that asked and the model it asked for, so per-key usage reporting
//! stays complete. Anonymous 401 spam is
//! additionally flood-capped per client IP before the key lookup
//! ([`crate::flood`]); the capped phase answers the same 401 without
//! touching the database and contributes at most one log row per window
//! per IP. `/v1/models` is a lightweight discovery endpoint and is not
//! logged.
//!
//! The Messages surface is the one whose client key need not arrive in a
//! bearer header and whose upstream does not speak the OpenAI usage shape:
//! its key comes from `x-api-key` or from a bearer header ([`MessagesAuth`]),
//! its upstream answers in the Anthropic usage shape
//! (`UsageFormat::Anthropic`), and its upstream needs the `anthropic-version`
//! header, which Hydra therefore requires from the client instead of
//! inventing one.

use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

use crate::db::channels::Channel;
use crate::flood::FloodTicket;
use crate::http::client_ip;
use crate::http::docs::OpenAiError;
use crate::http::openai_error::openai_error;
use crate::http::proxy_auth::{MessagesAuth, ProxyAuth, ProxyAuthError, X_API_KEY};
use crate::http::AppState;
use crate::logging::{self, ChatLogContext, ChatLogInit, UsageFormat};

/// Maximum accepted proxied request body, in bytes (32 MiB). Vision
/// requests embed base64-encoded images and long-context conversations run
/// well past axum's 2 MiB default; 32 MiB covers both while still bounding
/// memory. Each proxied POST route mounts it via `DefaultBodyLimit`
/// (`/v1/chat/completions`, `/v1/responses` and `/v1/messages`; the layer is
/// per-route, so a route without it would silently fall back to axum's
/// default); an over-limit body fails into [`ProxyError::PayloadTooLarge`].
pub const CHAT_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Total-request cap for non-streaming proxied calls (every POST surface).
/// Generous on purpose: reasoning models can think for minutes before the
/// single response body arrives.
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
    #[error("model {model} is not allowed by the channel whitelist (allowed: {allowed})", allowed = allowed.join(", "))]
    ModelNotAllowed { model: String, allowed: Vec<String> },
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
            ProxyError::ModelNotAllowed { model, allowed } => openai_error(
                StatusCode::NOT_FOUND,
                &format!(
                    "The model `{model}` does not exist or you do not have access to it. \
                     Allowed models for this channel: {}",
                    allowed.join(", ")
                ),
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

/// Buffered proxied request body, shared by every proxied POST route (chat,
/// Responses, Messages). Wrapping `Bytes` in a dedicated extractor is what
/// makes an over-limit body surface as an OpenAI-shaped 413 (via
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

/// `POST /v1/chat/completions`: the Chat Completions surface of the shared
/// proxy pipeline ([`forward_proxied`]): peek `model`/`stream`, check the
/// whitelist, forward the original bytes with the upstream key, pass the
/// response through byte-for-byte, and leave a background log row behind
/// whatever the outcome (only the 413 body-limit rejection stays unlogged).
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
        upstream status and body unchanged. Every call, rejected ones \
        included, leaves a row in the request log; the one exception is a \
        413 over-limit body, which cannot be logged.",
    security(("bearer_auth" = [])),
    request_body(
        content = Object,
        description = "Any OpenAI Chat Completions request body. `model` is \
            required and must be admitted by the channel's whitelist (a \
            channel without a whitelist admits any model); GET /v1/models \
            lists exactly what the key's channel admits, and the example \
            model below only works when the channel admits it. \
            `stream: true` switches the response to text/event-stream. \
            Bodies above 32 MiB are rejected with 413.",
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
            channel's whitelist; the message lists the allowed models so \
            the caller can fix the request without an admin round-trip", body = OpenAiError, example = json!({
            "error": {
                "message": "The model `gpt-4` does not exist or you do not have access to it. Allowed models for this channel: gpt-4o-mini, gpt-4o",
                "type": "invalid_request_error",
                "code": "model_not_found"
            }
        })),
        (status = 413, description = "The request body exceeds 32 MiB", body = OpenAiError, example = json!({
            "error": { "message": "Request body exceeds the maximum allowed size of 32 MiB", "type": "invalid_request_error", "code": null }
        })),
        (status = 429, description = "The key's monthly spending limit is \
            reached. Enforced for a key that has a limit and whose channel \
            is priceable (every whitelisted model carries all four prices); \
            the verdict is recomputed in the background after each logged \
            response, so the first requests after a restart pass", body = OpenAiError, example = json!({
            "error": { "message": "Monthly limit reached", "type": "rate_limit_error", "code": null }
        })),
        (status = 502, description = "The bound channel is gone, or the \
            upstream could not be reached; other upstream statuses pass \
            through unchanged", body = OpenAiError, examples(
            ("upstream_unavailable" = (
                summary = "The upstream request failed",
                value = json!({
                    "error": { "message": "Failed to connect to the upstream provider", "type": "api_error", "code": null }
                })
            )),
            ("channel_gone" = (
                summary = "The channel bound to the key no longer exists",
                value = json!({
                    "error": { "message": "The channel bound to this API key is unavailable", "type": "api_error", "code": null }
                })
            ))
        )),
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
    // Extracted as a Result so an auth failure reaches this handler with
    // the request body already buffered: the rejection row needs the same
    // body capture a successful call gets.
    auth: Result<ProxyAuth, ProxyAuthError>,
    headers: HeaderMap,
    // Absent ConnectInfo (in-process oneshot requests) simply logs no IP.
    peer: client_ip::PeerAddr,
    ChatBody(body): ChatBody,
) -> Result<Response, ProxyError> {
    forward_proxied(&state, auth, &headers, peer, body, CHAT_SURFACE).await
}

/// `POST /v1/responses`: the OpenAI Responses surface of the same proxy
/// pipeline as chat ([`forward_proxied`]). Bearer auth, the 32 MiB body
/// limit, the whitelist check and both gates are the shared ones, so an
/// unknown key is answered 401, a disabled one 403, an unlisted model 404,
/// an over-budget key 429 and a flood-capped IP the same 401 as everywhere
/// else. What is specific to this route is the upstream path its bytes are
/// forwarded to (`<base>/v1/responses`) and the usage shape its response is
/// logged in (the Responses `input_tokens`/`output_tokens` mapping), so a
/// Responses call prices and gates exactly like a chat call.
pub async fn responses(
    State(state): State<AppState>,
    // Buffered as a Result for the same reason as chat: a rejected request
    // still gets its audit row with the body it carried.
    auth: Result<ProxyAuth, ProxyAuthError>,
    headers: HeaderMap,
    peer: client_ip::PeerAddr,
    ChatBody(body): ChatBody,
) -> Result<Response, ProxyError> {
    forward_proxied(&state, auth, &headers, peer, body, RESPONSES_SURFACE).await
}

/// `POST /v1/messages`: the Anthropic Messages surface of the same proxy
/// pipeline as chat and Responses ([`forward_proxied`]). The client key is
/// read from `x-api-key` or from a bearer header ([`MessagesAuth`]); the 32
/// MiB body limit, the model whitelist, the flood cap and the monthly-limit
/// gate are the shared ones, so an unknown key is answered 401, a disabled
/// one 403, an unlisted model 404, an over-budget key 429 and a flood-capped
/// IP the same 401 as everywhere else. What is specific to this surface is
/// the upstream path (`<base>/v1/messages`), the header policy
/// ([`HeaderPolicy::Anthropic`]: the upstream key rides `x-api-key`, the
/// required `anthropic-version` and the optional `anthropic-beta` hop across)
/// and the usage shape its response is logged in, so a Messages call prices
/// and gates exactly like a chat call.
pub async fn messages(
    State(state): State<AppState>,
    // Same Result shape as the other surfaces: an auth failure reaches the
    // handler with the body already buffered, and the rejection still gets
    // its audit row.
    auth: Result<MessagesAuth, ProxyAuthError>,
    headers: HeaderMap,
    peer: client_ip::PeerAddr,
    ChatBody(body): ChatBody,
) -> Result<Response, ProxyError> {
    forward_proxied(
        &state,
        auth.map(MessagesAuth::into_inner),
        &headers,
        peer,
        body,
        MESSAGES_SURFACE,
    )
    .await
}

/// What differs between the proxied POST surfaces: the upstream path the
/// request is forwarded to, the usage shape the response is read in, and the
/// header policy that carries the credential upstream. Everything else about
/// a proxied POST (body limit, model whitelist, both gates, response piping,
/// logging) is identical across surfaces and lives in [`forward_proxied`].
struct Surface {
    /// Path appended to the channel's base URL, with the same leading-`/v1`
    /// semantics as chat.
    path: &'static str,
    /// Usage shape the surface's upstream answers in, handed to the log
    /// context so the response body is read correctly.
    usage_format: UsageFormat,
    /// How this surface's upstream expects the credential, and which of the
    /// client's own headers it needs beyond the two every surface forwards.
    headers: HeaderPolicy,
}

/// Outbound header policy of one proxied surface: which header carries the
/// upstream key, and which client headers hop across on top of the
/// `Content-Type` and `Accept` every surface forwards. It is per-surface
/// because the Messages API is the one upstream that authenticates on
/// `x-api-key` and states its version per request through `anthropic-version`.
/// The OpenAI surfaces keep exactly the hygiene rules chat has always had.
enum HeaderPolicy {
    /// OpenAI surfaces: the upstream key replaces the client Authorization
    /// as `Authorization: Bearer`; nothing else of the client's hops across.
    OpenAi,
    /// Anthropic Messages: the upstream key replaces the client credentials
    /// as `x-api-key`, and the version header the Messages API requires plus
    /// the beta opt-in list, when the client sent one, are forwarded. The
    /// client's own credential header is never among them, whichever header
    /// it arrived in.
    Anthropic,
}

/// The Messages API version header, required on every Messages request.
const ANTHROPIC_VERSION: &str = "anthropic-version";
/// The Messages API beta opt-in list, optional and forwarded as sent.
const ANTHROPIC_BETA: &str = "anthropic-beta";

impl HeaderPolicy {
    /// Rejects a request this surface cannot forward upstream. Only the
    /// Messages surface has a header contract of its own: the Messages API
    /// requires `anthropic-version` on every request and would answer
    /// without it only with an error, so Hydra reports the missing header at
    /// its own boundary rather than spending an upstream round-trip to
    /// relay it. A present value is forwarded verbatim: which versions an
    /// account may use is the upstream's business, not the router's.
    fn check(&self, headers: &HeaderMap) -> Result<(), ProxyError> {
        match self {
            HeaderPolicy::OpenAi => Ok(()),
            HeaderPolicy::Anthropic if headers.contains_key(ANTHROPIC_VERSION) => Ok(()),
            HeaderPolicy::Anthropic => Err(ProxyError::BadRequest(format!(
                "you must provide an {ANTHROPIC_VERSION} header"
            ))),
        }
    }

    /// Puts the surface's credential header on the outbound request and
    /// copies the surface's own extra headers across. The credential the
    /// client sent is never forwarded: the upstream always sees the bound
    /// channel's key under the surface's own header name.
    fn apply(
        &self,
        request: reqwest::RequestBuilder,
        headers: &HeaderMap,
        upstream_key: &str,
    ) -> reqwest::RequestBuilder {
        match self {
            HeaderPolicy::OpenAi => {
                request.header(header::AUTHORIZATION, format!("Bearer {upstream_key}"))
            }
            HeaderPolicy::Anthropic => {
                let mut request = request.header(X_API_KEY, upstream_key);
                if let Some(version) = headers.get(ANTHROPIC_VERSION) {
                    request = request.header(ANTHROPIC_VERSION, version);
                }
                if let Some(beta) = headers.get(ANTHROPIC_BETA) {
                    request = request.header(ANTHROPIC_BETA, beta);
                }
                request
            }
        }
    }
}

/// OpenAI Chat Completions.
const CHAT_SURFACE: Surface = Surface {
    path: "/v1/chat/completions",
    usage_format: UsageFormat::Chat,
    headers: HeaderPolicy::OpenAi,
};

/// OpenAI Responses.
const RESPONSES_SURFACE: Surface = Surface {
    path: "/v1/responses",
    usage_format: UsageFormat::Responses,
    headers: HeaderPolicy::OpenAi,
};

/// Anthropic Messages.
const MESSAGES_SURFACE: Surface = Surface {
    path: "/v1/messages",
    usage_format: UsageFormat::Anthropic,
    headers: HeaderPolicy::Anthropic,
};

/// The thin-passthrough pipeline every proxied POST surface runs. Every
/// rejection keeps the audit row it has always had: a flood-capped or
/// auth-layer one carries NULL ids, a monthly-limit one names the key that
/// asked, a raw client error keeps the caller's key id. Then the buffered
/// bytes are forwarded verbatim to the surface's upstream path with the
/// client credential replaced by the upstream key through the surface's
/// header policy, and the upstream response goes back untouched through
/// [`upstream_to_response`], which is where the response is tee'd and the
/// surface's usage shape is read.
async fn forward_proxied(
    state: &AppState,
    auth: Result<ProxyAuth, ProxyAuthError>,
    headers: &HeaderMap,
    peer: client_ip::PeerAddr,
    body: Bytes,
    surface: Surface,
) -> Result<Response, ProxyError> {
    let facts = RejectionFacts {
        start: Instant::now(),
        client_ip: client_ip::resolve(headers, peer.0, &state.trusted_proxies),
        usage_format: surface.usage_format,
    };
    let auth = match auth {
        Ok(auth) => auth,
        Err(ProxyAuthError::FloodCapped(ticket)) => {
            log_flood_cap_rejection(state, &facts, &body, &ticket);
            return Ok(ProxyAuthError::FloodCapped(ticket).into_response());
        }
        // The limit gate runs after the key resolved, so this rejection
        // knows who asked and gets its own row rather than the NULL-key one.
        Err(e @ ProxyAuthError::MonthlyLimitReached { key_id, channel_id }) => {
            log_limit_rejection(state, &facts, &body, key_id, channel_id);
            return Ok(e.into_response());
        }
        Err(e) => {
            log_auth_rejection(state, &facts, &body, &e);
            return Ok(e.into_response());
        }
    };
    let (model, stream_requested) = match peek_request(&body) {
        Ok(peeked) => peeked,
        Err(e) => {
            log_rejection(state, &auth, None, &facts, &body, &e);
            return Err(e);
        }
    };
    let channel = match resolve_channel(&auth) {
        Ok(channel) => channel,
        // When the database itself is failing there is no point attempting a
        // log write; ChannelGone and ChannelDisabled are client-visible
        // routing failures that still get their audit row.
        Err(e @ (ProxyError::ChannelGone | ProxyError::ChannelDisabled)) => {
            log_rejection(state, &auth, Some(model), &facts, &body, &e);
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    if let Err(e) = ensure_model_allowed(&channel, &model) {
        log_rejection(state, &auth, Some(model), &facts, &body, &e);
        return Err(e);
    }
    // The surface's own header contract is the last gate, right where the
    // outbound request is built: the routing rejections above keep their
    // documented statuses (a whitelist miss stays a 404, a gone channel a
    // 502), and this one fires only for a request that would otherwise be
    // forwarded without a header its upstream requires.
    if let Err(e) = surface.headers.check(headers) {
        log_rejection(state, &auth, Some(model), &facts, &body, &e);
        return Err(e);
    }
    let url = join_url(&channel.base_url, surface.path);
    let mut request = surface
        .headers
        .apply(state.client.post(&url), headers, &channel.api_key);
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
    let ctx = chat_log_context(
        state,
        &auth,
        Some(model),
        facts.usage_format,
        facts.start,
        &body,
        facts.client_ip,
    );
    let upstream = match request.body(body).send().await {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(error = %e, "upstream proxy request failed");
            ctx.finish(
                None,
                logging::UsageExtraction::default(),
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
        listing with the same key substitution as chat. The monthly-limit \
        gate runs here too, so a key over budget is answered 429. This \
        endpoint is not logged.",
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
        (status = 429, description = "The key's monthly spending limit is \
            reached; the gate is shared with the chat route, so a key over \
            budget cannot list models either", body = OpenAiError, example = json!({
            "error": { "message": "Monthly limit reached", "type": "rate_limit_error", "code": null }
        })),
        (status = 502, description = "The upstream models listing could not \
            be fetched, or the bound channel is gone", body = OpenAiError, examples(
            ("upstream_unavailable" = (
                summary = "The upstream request failed",
                value = json!({
                    "error": { "message": "Failed to connect to the upstream provider", "type": "api_error", "code": null }
                })
            )),
            ("channel_gone" = (
                summary = "The channel bound to the key no longer exists",
                value = json!({
                    "error": { "message": "The channel bound to this API key is unavailable", "type": "api_error", "code": null }
                })
            ))
        )),
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
    let channel = resolve_channel(&auth)?;
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
/// model must match a whitelist entry exactly. A miss carries the whitelist
/// into the error so the client sees which models would work.
fn ensure_model_allowed(channel: &Channel, model: &str) -> Result<(), ProxyError> {
    match channel.allowed_models.as_deref() {
        Some(models) if !models.is_empty() => {
            if models.iter().any(|allowed| allowed == model) {
                Ok(())
            } else {
                Err(ProxyError::ModelNotAllowed {
                    model: model.to_string(),
                    allowed: models.to_vec(),
                })
            }
        }
        _ => Ok(()),
    }
}

/// Returns the channel resolved during authentication (cache hit or the
/// database read that filled the cache). The FK plus the bound-keys delete
/// guard make a missing row nearly impossible; if it does happen the caller
/// sees a 502 rather than a panic or silent fallback. A disabled channel
/// keeps its keys and their log history but must not serve traffic; its
/// callers see a 503 so the key itself stays valid.
fn resolve_channel(auth: &ProxyAuth) -> Result<Channel, ProxyError> {
    let channel = auth.channel.clone().ok_or(ProxyError::ChannelGone)?;
    if !channel.enabled {
        return Err(ProxyError::ChannelDisabled);
    }
    Ok(channel)
}

/// Joins a channel base URL with an API path so a trailing slash on the
/// stored URL or a leading slash on the path never produces a double slash
/// or drops the path. A base URL written in OpenAI-SDK style (ending in
/// `/v1`) does not double the version segment when the path already starts
/// with `/v1/`: `https://api.example.com/v1` forwards to
/// `/v1/chat/completions`, not `/v1/v1/chat/completions`.
fn join_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let base = if path.starts_with("/v1/") {
        base.strip_suffix("/v1").unwrap_or(base)
    } else {
        base
    };
    format!("{}/{}", base, path.trim_start_matches('/'))
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
                        logging::UsageExtraction::default(),
                        None,
                        Some("upstream response body read failed".to_string()),
                    );
                }
                return Err(ProxyError::UpstreamUnavailable);
            }
        };
        if let Some(ctx) = log {
            let usage = ctx.buffered_usage(&bytes);
            let response_body = ctx.capture_response_body(&bytes);
            ctx.finish(Some(status_code), usage, response_body, None);
        }
        Ok(response
            .body(Body::from(bytes))
            .expect("upstream status and headers are valid"))
    }
}

/// Gathers the log context for one request from the request's own pieces;
/// the channel id comes from the authenticated key, which is what the log row
/// must reference even if channel resolution later fails. The usage format
/// names the shape this request's upstream will answer in, so each surface's
/// handler passes its own.
fn chat_log_context(
    state: &AppState,
    auth: &ProxyAuth,
    model: Option<String>,
    usage_format: UsageFormat,
    start: Instant,
    body: &Bytes,
    client_ip: Option<String>,
) -> ChatLogContext {
    ChatLogContext::new(ChatLogInit {
        db: &state.db,
        tracker: &state.log_tracker,
        log_bodies: state.log_bodies,
        api_key_id: Some(auth.key.id),
        channel_id: Some(auth.key.channel_id),
        model,
        usage_format,
        start,
        request_body: body,
        client_ip,
        flags: &state.limit_flags,
    })
}

/// The request-scoped pieces every rejection row of one proxied request
/// needs, whichever stage rejected it: when the request started (the logged
/// duration is measured to the finished row), the resolved client IP, and
/// the surface's usage shape. The format is stored but never read on these
/// rows: a rejection parses no response body, because no upstream answered.
struct RejectionFacts {
    start: Instant,
    client_ip: Option<String>,
    usage_format: UsageFormat,
}

/// Writes the audit row for a request rejected by the auth layer (401
/// unknown key, 403 disabled key, key lookup failure). The caller never
/// resolved to a key row, so the row carries no key or channel ids. The
/// stored failure text mirrors the message the client body carries for the
/// InvalidKey and DisabledKey arms; the Database arm stores the internal
/// error detail (`proxy key lookup failed: ...`) while the client only
/// ever sees a generic 500 body, so the log keeps the diagnostic detail
/// the response deliberately omits.
fn log_auth_rejection(
    state: &AppState,
    facts: &RejectionFacts,
    body: &Bytes,
    error: &ProxyAuthError,
) {
    let ctx = ChatLogContext::new(ChatLogInit {
        db: &state.db,
        tracker: &state.log_tracker,
        log_bodies: state.log_bodies,
        api_key_id: None,
        channel_id: None,
        model: None,
        usage_format: facts.usage_format,
        start: facts.start,
        request_body: body,
        client_ip: facts.client_ip.clone(),
        flags: &state.limit_flags,
    });
    let (status, message) = match error {
        ProxyAuthError::InvalidKey | ProxyAuthError::FloodCapped(_) => {
            (Some(401), "Invalid API key provided".to_string())
        }
        ProxyAuthError::DisabledKey => (Some(403), "This API key has been disabled".to_string()),
        // A storage failure answers 500 without reaching the upstream.
        ProxyAuthError::Database(e) => (None, format!("proxy key lookup failed: {e}")),
        // Never reached: the limit rejection has its own path
        // ([`log_limit_rejection`]) precisely so the row keeps the caller's
        // key. This arm exists so the match stays exhaustive.
        ProxyAuthError::MonthlyLimitReached { .. } => {
            (Some(429), "Monthly limit reached".to_string())
        }
    };
    ctx.finish(
        status,
        logging::UsageExtraction::default(),
        None,
        Some(message),
    );
}

/// Writes the audit row for a flood-capped request: at most one row per
/// window per IP, claimed through the flood cap with the ticket the capped
/// decision produced, so a sustained spam wave cannot produce a log row per
/// request and a claim can never leak into the next window. The row carries
/// the 401 the client received (identical to the unknown-key response), no
/// key or channel ids, and the resolved client IP. The row does not reset
/// or otherwise touch the failure counter; the window expiry is the only
/// reset.
fn log_flood_cap_rejection(
    state: &AppState,
    facts: &RejectionFacts,
    body: &Bytes,
    ticket: &FloodTicket,
) {
    if !state.flood.claim_log_slot(ticket) {
        return;
    }
    let ctx = ChatLogContext::new(ChatLogInit {
        db: &state.db,
        tracker: &state.log_tracker,
        log_bodies: state.log_bodies,
        api_key_id: None,
        channel_id: None,
        model: None,
        usage_format: facts.usage_format,
        start: facts.start,
        request_body: body,
        client_ip: facts.client_ip.clone(),
        flags: &state.limit_flags,
    });
    ctx.finish(
        Some(401),
        logging::UsageExtraction::default(),
        None,
        Some("flood cap: too many failed authentications from this address".to_string()),
    );
}

/// Writes the audit row for a request rejected because the key's monthly
/// spending limit is used up. The gate runs after the key resolved, so
/// unlike the auth-layer rejections this row names the real key and channel
/// and the model the caller asked for: a limit rejection is usage of that
/// key, and the per-key reporting must see it. The row is written by the
/// detached logging task like any other, and the model is read from the
/// already buffered request body, which was never forwarded.
fn log_limit_rejection(
    state: &AppState,
    facts: &RejectionFacts,
    body: &Bytes,
    key_id: i64,
    channel_id: i64,
) {
    let model = peek_request(body).ok().map(|(model, _)| model);
    let ctx = ChatLogContext::new(ChatLogInit {
        db: &state.db,
        tracker: &state.log_tracker,
        log_bodies: state.log_bodies,
        api_key_id: Some(key_id),
        channel_id: Some(channel_id),
        model,
        usage_format: facts.usage_format,
        start: facts.start,
        request_body: body,
        client_ip: facts.client_ip.clone(),
        flags: &state.limit_flags,
    });
    ctx.finish(
        Some(429),
        logging::UsageExtraction::default(),
        None,
        Some("Monthly limit reached".to_string()),
    );
}

/// Writes the audit row for an authenticated proxied request rejected
/// before the upstream was contacted: the row keeps the caller's key id. The
/// 413 body-limit rejection happens in the extractor instead and stays
/// unlogged; auth-layer rejections are logged by [`log_auth_rejection`].
fn log_rejection(
    state: &AppState,
    auth: &ProxyAuth,
    model: Option<String>,
    facts: &RejectionFacts,
    body: &Bytes,
    error: &ProxyError,
) {
    // No response body is ever parsed for a rejection, so this row carries
    // no usage and the format the context stores is only a placeholder.
    let ctx = chat_log_context(
        state,
        auth,
        model,
        facts.usage_format,
        facts.start,
        body,
        facts.client_ip.clone(),
    );
    ctx.finish(
        None,
        logging::UsageExtraction::default(),
        None,
        Some(error.to_string()),
    );
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers_with_version() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ANTHROPIC_VERSION, HeaderValue::from_static("2023-06-01"));
        headers
    }

    /// Builds the outbound request one policy produces, so the header rules
    /// are asserted on what would actually leave the process.
    fn outbound(policy: &HeaderPolicy, headers: &HeaderMap) -> reqwest::Request {
        policy
            .apply(
                reqwest::Client::new().post("http://127.0.0.1:9/v1/messages"),
                headers,
                "sk-upstream-secret",
            )
            .build()
            .unwrap()
    }

    #[test]
    fn only_the_messages_policy_requires_the_version_header() {
        assert!(HeaderPolicy::OpenAi.check(&HeaderMap::new()).is_ok());
        assert!(matches!(
            HeaderPolicy::Anthropic.check(&HeaderMap::new()),
            Err(ProxyError::BadRequest(message)) if message.contains("anthropic-version")
        ));
        assert!(HeaderPolicy::Anthropic
            .check(&headers_with_version())
            .is_ok());
    }

    #[test]
    fn the_messages_policy_substitutes_the_api_key_header() {
        let mut headers = headers_with_version();
        headers.insert(X_API_KEY, HeaderValue::from_static("hyd-client-key"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer hyd-client-key"),
        );
        let request = outbound(&HeaderPolicy::Anthropic, &headers);

        assert_eq!(request.headers()[X_API_KEY], "sk-upstream-secret");
        assert!(
            !request.headers().contains_key(header::AUTHORIZATION),
            "the client credential must not hop across: {:?}",
            request.headers()
        );
        assert_eq!(request.headers()[ANTHROPIC_VERSION], "2023-06-01");
    }

    #[test]
    fn the_messages_policy_forwards_the_beta_header_only_when_sent() {
        assert!(!outbound(&HeaderPolicy::Anthropic, &headers_with_version())
            .headers()
            .contains_key(ANTHROPIC_BETA));

        let mut with_beta = headers_with_version();
        with_beta.insert(
            ANTHROPIC_BETA,
            HeaderValue::from_static("prompt-caching-2024-07-31"),
        );
        assert_eq!(
            outbound(&HeaderPolicy::Anthropic, &with_beta).headers()[ANTHROPIC_BETA],
            "prompt-caching-2024-07-31"
        );
    }

    /// The OpenAI policy is what chat has always done: a bearer substitution
    /// and nothing else, whatever the client sent alongside it.
    #[test]
    fn the_openai_policy_keeps_the_bearer_substitution_and_forwards_nothing_else() {
        let mut headers = headers_with_version();
        headers.insert(X_API_KEY, HeaderValue::from_static("hyd-client-key"));
        headers.insert(
            ANTHROPIC_BETA,
            HeaderValue::from_static("prompt-caching-2024-07-31"),
        );
        let request = outbound(&HeaderPolicy::OpenAi, &headers);

        assert_eq!(
            request.headers()[header::AUTHORIZATION],
            "Bearer sk-upstream-secret"
        );
        assert!(!request.headers().contains_key(X_API_KEY));
        assert!(!request.headers().contains_key(ANTHROPIC_VERSION));
        assert!(!request.headers().contains_key(ANTHROPIC_BETA));
    }

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
    fn join_url_drops_a_doubled_v1_segment() {
        // OpenAI-SDK style base URL: the path already carries the version.
        assert_eq!(
            join_url("https://api.example.com/v1", "/v1/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://api.example.com/v1/", "/v1/models"),
            "https://api.example.com/v1/models"
        );
        // A nested prefix keeps everything except the redundant tail.
        assert_eq!(
            join_url(
                "https://proxy.example.com/openai/v1",
                "/v1/chat/completions"
            ),
            "https://proxy.example.com/openai/v1/chat/completions"
        );
        // Version-less bases stay untouched.
        assert_eq!(
            join_url("https://api.example.com", "/v1/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        // Only an exact `/v1` tail is dropped; lookalikes stay.
        assert_eq!(
            join_url("https://api.example.com/v11", "/v1/chat/completions"),
            "https://api.example.com/v11/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://api.example.com/v1beta", "/v1/chat/completions"),
            "https://api.example.com/v1beta/v1/chat/completions"
        );
        // Non-`/v1` paths never trigger the strip.
        assert_eq!(
            join_url("https://api.example.com/v1", "/other"),
            "https://api.example.com/v1/other"
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
            Err(ProxyError::ModelNotAllowed { model, allowed })
                if model == "gpt-4" && allowed == vec!["gpt-4o".to_string()]
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
