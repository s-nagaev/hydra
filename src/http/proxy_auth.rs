//! Proxy-key authentication for the proxy routes.
//!
//! [`ProxyAuth`] extracts `Authorization: Bearer <key>`, hashes the full
//! key string (prefix included) and resolves the alive key row plus its
//! bound channel. Soft-deleted keys never authenticate. A fresh
//! [`ResolutionCache`](crate::cache::ResolutionCache) hit answers both
//! lookups without touching the database; a miss (or expired entry) reads
//! the database and, when the key is alive and its channel enabled, fills
//! the cache. Only successful resolutions are cached: unknown or disabled
//! keys and gone or disabled channels keep hitting the database, so admin
//! disables — which invalidate explicitly — are the only staleness risk,
//! and the TTL only bounds out-of-band database edits. A refill is stored
//! only when the cache epoch observed before the read is still current: an
//! admin mutation that completed while the read was in flight invalidates
//! the cache and advances the epoch, so the stale pre-write entry is
//! dropped rather than re-inserted after the mutation's invalidation.
//! Rejections
//! use the error shape of the surface that produced them ([`ErrorShape`]):
//! 401 for a missing
//! or malformed header and for an unknown key, 403 for a disabled key, 500
//! for storage failures, in the OpenAI envelope everywhere except the
//! Messages surface, which answers its Anthropic clients in the Anthropic
//! envelope. The plaintext key never appears in a response or log line.
//!
//! The Messages route authenticates through [`MessagesAuth`], which reads the
//! same key from the Anthropic-native `x-api-key` header or from a bearer
//! header and then runs the identical sequence ([`authenticate`]): the two
//! surfaces differ only in where the client writes the credential. The chat,
//! Responses and models routes stay [`ProxyAuth`] and therefore Bearer-only.
//!
//! Before any key lookup, requests carrying a resolvable client IP pass
//! through the per-IP flood cap ([`crate::flood`]): admission reserves one
//! budget slot in a single atomic step, so a parallel burst of requests
//! from one IP can never all pass the gate against a stale counter: at
//! most `HYDRA_FLOOD_LIMIT` requests per window are past the gate at the
//! same time, and once the budget is spent every request from that IP,
//! valid keys included, is answered [`ProxyAuthError::FloodCapped`] without
//! touching the cache or the database. A reservation is released unless the
//! request fails with an invalid key, so successes, disabled keys and
//! storage failures leave the settled counter untouched. The capped
//! response is byte-identical to the unknown-key 401, so the cap is not an
//! oracle for key validity.
//!
//! A resolved key whose monthly spending limit is used up is answered
//! [`ProxyAuthError::MonthlyLimitReached`], a 429 in the surface's error
//! envelope.
//! The decision is one in-memory lookup ([`crate::limit_flags`]); the
//! verdict itself is recomputed off the request path by
//! [`crate::usage::recompute`] after every logged response. It is only ever
//! `true` for a key that has a limit and whose channel is limitable, so a
//! limit stored against a channel the router cannot price stays stored and
//! unenforced, and an absent verdict (a fresh process, an invalidated entry,
//! an evicted one) lets the request through until the next logged response
//! repopulates it. The check runs on every bearer-authenticated proxy route,
//! `/v1/models` included: a key over budget cannot list models either, which
//! is deliberate.

use std::net::{IpAddr, SocketAddr};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::db::channels::{self, Channel};
use crate::db::keys::{self, ApiKey};
use crate::flood::{Admission, FloodTicket};
use crate::http::client_ip;
use crate::http::openai_error::ErrorShape;
use crate::http::AppState;
use crate::keys::sha256_hex;

/// Extractor carrying the authenticated client key row plus its resolved
/// bound channel: `channel` is `None` only when the channel row is gone.
pub struct ProxyAuth {
    pub key: ApiKey,
    pub channel: Option<Channel>,
}

#[derive(Debug, thiserror::Error)]
#[error("proxy authentication failed")]
pub enum ProxyAuthError {
    InvalidKey,
    /// The key's monthly spending limit is used up. Carries the resolved key
    /// and channel ids so the audit row of the rejection keeps the real key
    /// instead of the NULL-key shape a rejection before resolution would
    /// produce.
    #[error("monthly limit reached")]
    MonthlyLimitReached {
        key_id: i64,
        channel_id: i64,
    },
    /// The client IP exhausted its failed-authentication budget; the
    /// response is the same 401 an unknown key gets. The ticket binds the
    /// rejection to the window the cap decision was made in, for the
    /// single per-window log row.
    FloodCapped(FloodTicket),
    DisabledKey,
    Database(#[from] sqlx::Error),
}

impl ProxyAuthError {
    /// Renders this rejection in the envelope of the surface that produced
    /// it: the status, the message and the OpenAI taxonomy type are the same
    /// everywhere, the envelope is per surface (see [`ErrorShape`]). The
    /// pipeline renders through this so an Anthropic SDK gets an Anthropic
    /// envelope, and the bare [`IntoResponse`] below stays the OpenAI
    /// default for the routes that have no surface of their own.
    pub fn into_response_with(self, shape: ErrorShape) -> Response {
        let (status, message, error_type, code) = self.parts();
        shape.render(status, &message, error_type, code)
    }

    /// The surface-independent pieces of this rejection: status, the message
    /// the client reads, and the OpenAI taxonomy type and code, which only
    /// the OpenAI envelope carries. The Database arm logs the internal
    /// failure here, before the body is reduced to the generic 500 text.
    fn parts(self) -> (StatusCode, String, &'static str, Option<&'static str>) {
        match self {
            // The flood-capped response is deliberately indistinguishable
            // from the unknown-key one.
            ProxyAuthError::InvalidKey | ProxyAuthError::FloodCapped(_) => (
                StatusCode::UNAUTHORIZED,
                "Invalid API key provided".to_string(),
                "invalid_request_error",
                Some("invalid_api_key"),
            ),
            ProxyAuthError::DisabledKey => (
                StatusCode::FORBIDDEN,
                "This API key has been disabled".to_string(),
                "invalid_request_error",
                Some("key_disabled"),
            ),
            ProxyAuthError::MonthlyLimitReached { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "Monthly limit reached".to_string(),
                "rate_limit_error",
                None,
            ),
            ProxyAuthError::Database(e) => {
                tracing::error!(error = %e, "proxy key lookup failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                    "server_error",
                    None,
                )
            }
        }
    }
}

impl IntoResponse for ProxyAuthError {
    /// The OpenAI envelope: what the `/v1/models` extractor rejection
    /// renders and what the proxied surfaces fall back to. The proxied POST
    /// surfaces render through [`ProxyAuthError::into_response_with`] with
    /// their own surface's shape.
    fn into_response(self) -> Response {
        self.into_response_with(ErrorShape::OpenAi)
    }
}

/// The Anthropic credential header: the Messages surface reads the client
/// key from it, and the proxy pipeline writes the upstream key into it. The
/// `http` crate defines no constant for it.
pub(crate) const X_API_KEY: &str = "x-api-key";

/// The `/v1/messages` auth extractor: the same resolution, flood gate and
/// monthly-limit gate as [`ProxyAuth`], from a wider credential source.
pub struct MessagesAuth(ProxyAuth);

impl MessagesAuth {
    /// The resolved key and channel, for the shared proxy pipeline, which
    /// needs the data and not the header the credential arrived in.
    pub fn into_inner(self) -> ProxyAuth {
        self.0
    }

    /// The key this request presents, if it presents one: the value of the
    /// `x-api-key` header, or else the token of an `Authorization: Bearer`
    /// header. A blank or non-ASCII `x-api-key` falls through to the bearer
    /// header, so an unusable header cannot lock out a client that also sent
    /// a usable one. A well-formed `x-api-key` that simply does not resolve
    /// does not fall back: a wrong key is a failed authentication, not a
    /// missing one, and the two must stay distinguishable in the flood
    /// counter.
    fn client_token(headers: &HeaderMap) -> Option<String> {
        match headers.get(X_API_KEY).and_then(|value| value.to_str().ok()) {
            Some(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
            _ => bearer_token(headers),
        }
    }
}

impl FromRequestParts<AppState> for ProxyAuth {
    type Rejection = ProxyAuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(&parts.headers);
        authenticate(parts, state, token).await
    }
}

impl FromRequestParts<AppState> for MessagesAuth {
    type Rejection = ProxyAuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = Self::client_token(&parts.headers);
        authenticate(parts, state, token).await.map(MessagesAuth)
    }
}

/// The authentication sequence every proxy extractor runs, given the
/// credential the request carried (`None` when it carried none): the per-IP
/// flood gate, resolution of the token (cache first, database on a miss) and
/// the monthly-limit gate. Both extractors call this, so [`ProxyAuth`] and
/// [`MessagesAuth`] cannot drift apart in anything but where the token is
/// read from.
async fn authenticate(
    parts: &mut Parts,
    state: &AppState,
    token: Option<String>,
) -> Result<ProxyAuth, ProxyAuthError> {
    // Flood gate first: admission reserves one budget slot in a single
    // atomic step, before any cache probe or database read. A parallel
    // burst therefore cannot all pass the gate against a stale counter,
    // and a capped IP costs neither a cache probe nor a database read.
    // The gate needs the same client-IP resolution the log rows use, so
    // it shares the trusted-proxy logic.
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let flood_ip: Option<IpAddr> = if state.flood.is_enabled() {
        client_ip::resolve_ip(&parts.headers, peer, &state.trusted_proxies)
    } else {
        None
    };
    let flood_ticket = match flood_ip {
        Some(ip) => match state.flood.admit(ip) {
            Admission::Allowed(ticket) => Some(ticket),
            Admission::Capped(ticket) => return Err(ProxyAuthError::FloodCapped(ticket)),
        },
        None => None,
    };

    let resolution = match token {
        Some(token) => ProxyAuth::resolve_token(state, &token).await,
        None => Err(ProxyAuthError::InvalidKey),
    };
    match resolution {
        // An invalid key is exactly the failure the budget counts: its
        // admission slot settles into a recorded failure. A request that
        // carried no credential at all settles the same way, which keeps
        // the capped response indistinguishable from the unknown-key one.
        Err(ProxyAuthError::InvalidKey) => Err(ProxyAuthError::InvalidKey),
        // Every other outcome releases the slot first: successes, a
        // disabled key (403), a spent monthly limit (429) and storage
        // failures must not feed the counter. Only then is the monthly
        // limit checked, so a rejected request cannot starve the IP's
        // budget for the requests that follow it.
        other => {
            if let Some(ticket) = &flood_ticket {
                state.flood.release(ticket);
            }
            let auth = other?;
            ProxyAuth::check_monthly_limit(state, &auth)?;
            Ok(auth)
        }
    }
}

/// The token of an `Authorization: Bearer` header, if the request carries a
/// usable one.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let header_value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    parse_bearer(header_value).map(str::to_string)
}

impl ProxyAuth {
    /// Cache probe and database resolution for one presented key, after the
    /// flood gate has admitted the request. Both extractors call this with
    /// the token they read, so resolution is identical whichever credential
    /// header carried it.
    async fn resolve_token(state: &AppState, token: &str) -> Result<Self, ProxyAuthError> {
        let key_hash = sha256_hex(token.as_bytes());

        // Fresh hit: both the key and the channel come from memory and the
        // database is not consulted at all. A cached entry exists only for
        // an alive key on an enabled channel, so no further checks apply.
        if let Some(cached) = state.cache.get(&key_hash) {
            return Ok(ProxyAuth {
                key: cached.key,
                channel: Some(cached.channel),
            });
        }

        // Capture the epoch before reading: if an admin mutation completes
        // while the reads below are in flight, its invalidation advances the
        // epoch and the insert at the end is rejected, so a pre-write entry
        // can never land in the cache after the mutation.
        let seen_epoch = state.cache.epoch();
        let key = keys::find_alive_by_hash(&state.db, &key_hash)
            .await?
            .ok_or(ProxyAuthError::InvalidKey)?;
        if !key.enabled {
            return Err(ProxyAuthError::DisabledKey);
        }
        let channel = channels::find(&state.db, key.channel_id).await?;
        if let Some(channel) = &channel {
            if channel.enabled {
                state
                    .cache
                    .insert_if_current(seen_epoch, key_hash, key.clone(), channel.clone());
            }
        }
        Ok(ProxyAuth { key, channel })
    }

    /// Rejects a key whose monthly-limit verdict says it is over budget.
    ///
    /// The read is one hash lookup on the in-memory verdict store; the
    /// aggregate it stands for is computed off the request path. A key
    /// without a limit is never gated, and a verdict that is `false` or
    /// absent passes. Absent is the state of a fresh process, of an entry an
    /// admin mutation invalidated and of an evicted one, and letting those
    /// through is the accepted consequence of a bounded in-memory store (see
    /// [`crate::limit_flags`]). A non-limitable channel never produces a
    /// `true` verdict, so its keys keep their limits stored and unenforced.
    fn check_monthly_limit(state: &AppState, auth: &ProxyAuth) -> Result<(), ProxyAuthError> {
        if auth.key.monthly_limit_micro_usd.is_none() {
            return Ok(());
        }
        let month = crate::month::Month::current();
        if state.limit_flags.get(auth.key.id, month.bucket()) != Some(true) {
            return Ok(());
        }
        Err(ProxyAuthError::MonthlyLimitReached {
            key_id: auth.key.id,
            channel_id: auth.key.channel_id,
        })
    }
}

/// Parses `Bearer <token>` with any casing of the scheme; rejects empty
/// tokens and non-bearer schemes. The token is hashed, never stored.
fn parse_bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use crate::flood::FloodCap;

    use super::*;

    #[test]
    fn parse_bearer_accepts_scheme_with_any_casing() {
        assert_eq!(parse_bearer("Bearer hyd-abc"), Some("hyd-abc"));
        assert_eq!(parse_bearer("bearer hyd-abc"), Some("hyd-abc"));
        assert_eq!(parse_bearer("BEARER hyd-abc"), Some("hyd-abc"));
        assert_eq!(parse_bearer("Bearer   hyd-abc  "), Some("hyd-abc"));
    }

    #[test]
    fn parse_bearer_rejects_other_schemes_and_empty_tokens() {
        assert_eq!(parse_bearer("Basic dXNlcjpwYXNz"), None);
        assert_eq!(parse_bearer("Bearer"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer("hyd-abc"), None);
    }

    #[test]
    fn messages_client_token_reads_the_api_key_header() {
        let mut headers = HeaderMap::new();
        headers.insert(X_API_KEY, HeaderValue::from_static("hyd-from-api-key"));
        assert_eq!(
            MessagesAuth::client_token(&headers).as_deref(),
            Some("hyd-from-api-key")
        );
    }

    /// Both headers at once: the Anthropic-native one wins, so a client that
    /// keeps its OpenAI-style header alongside cannot authenticate as someone
    /// else by accident.
    #[test]
    fn messages_client_token_prefers_the_api_key_header_over_the_bearer_one() {
        let mut headers = HeaderMap::new();
        headers.insert(X_API_KEY, HeaderValue::from_static("hyd-from-api-key"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer hyd-from-bearer"),
        );
        assert_eq!(
            MessagesAuth::client_token(&headers).as_deref(),
            Some("hyd-from-api-key")
        );
    }

    #[test]
    fn messages_client_token_falls_back_to_the_bearer_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("bearer hyd-from-bearer"),
        );
        assert_eq!(
            MessagesAuth::client_token(&headers).as_deref(),
            Some("hyd-from-bearer")
        );

        // A blank api-key header is not a credential, so the bearer token it
        // was sent next to still authenticates.
        headers.insert(X_API_KEY, HeaderValue::from_static("   "));
        assert_eq!(
            MessagesAuth::client_token(&headers).as_deref(),
            Some("hyd-from-bearer")
        );
    }

    #[test]
    fn messages_client_token_is_none_without_any_usable_credential() {
        assert_eq!(MessagesAuth::client_token(&HeaderMap::new()), None);

        let mut basic = HeaderMap::new();
        basic.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(MessagesAuth::client_token(&basic), None);

        let mut empty_bearer = HeaderMap::new();
        empty_bearer.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(MessagesAuth::client_token(&empty_bearer), None);
    }

    /// Reads a rendered rejection back, so the envelope is asserted on the
    /// bytes a client would receive.
    async fn body_of(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Every auth-layer rejection in both envelopes: same status and same
    /// message, the OpenAI envelope on the OpenAI surfaces and the Anthropic
    /// envelope on the Messages surface. The flood-capped 401 is built
    /// through the cap itself, its ticket having no public constructor, and
    /// it must render exactly like the unknown-key 401 it imitates.
    #[tokio::test]
    async fn every_auth_rejection_renders_in_the_surface_envelope() {
        type Build = Box<dyn Fn() -> ProxyAuthError>;
        let cap = FloodCap::new(1, 60);
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        assert!(matches!(cap.admit(ip), Admission::Allowed(_)));
        let Admission::Capped(ticket) = cap.admit(ip) else {
            panic!("a spent budget must cap the next admission")
        };

        let cases: [(Build, StatusCode, &str, &str, Option<&str>); 5] = [
            (
                Box::new(|| ProxyAuthError::InvalidKey),
                StatusCode::UNAUTHORIZED,
                "invalid_request_error",
                "authentication_error",
                Some("invalid_api_key"),
            ),
            (
                Box::new(move || ProxyAuthError::FloodCapped(ticket.clone())),
                StatusCode::UNAUTHORIZED,
                "invalid_request_error",
                "authentication_error",
                Some("invalid_api_key"),
            ),
            (
                Box::new(|| ProxyAuthError::DisabledKey),
                StatusCode::FORBIDDEN,
                "invalid_request_error",
                "permission_error",
                Some("key_disabled"),
            ),
            (
                Box::new(|| ProxyAuthError::MonthlyLimitReached {
                    key_id: 1,
                    channel_id: 2,
                }),
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "rate_limit_error",
                None,
            ),
            (
                Box::new(|| ProxyAuthError::Database(sqlx::Error::RowNotFound)),
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "api_error",
                None,
            ),
        ];

        for (build, status, openai_type, anthropic_type, code) in cases {
            let (rendered_status, openai) =
                body_of(build().into_response_with(ErrorShape::OpenAi)).await;
            assert_eq!(rendered_status, status, "OpenAI status: {openai}");
            assert_eq!(openai["error"]["type"], openai_type, "body: {openai}");
            assert_eq!(
                openai["error"]["code"],
                serde_json::json!(code),
                "body: {openai}"
            );
            let message = openai["error"]["message"].clone();
            assert!(
                message.as_str().is_some_and(|message| !message.is_empty()),
                "every rejection must carry a message: {openai}"
            );

            let (rendered_status, anthropic) =
                body_of(build().into_response_with(ErrorShape::Anthropic)).await;
            assert_eq!(rendered_status, status, "Anthropic status: {anthropic}");
            assert_eq!(anthropic["type"], "error", "body: {anthropic}");
            assert_eq!(
                anthropic["error"]["type"], anthropic_type,
                "body: {anthropic}"
            );
            assert_eq!(
                anthropic["error"]["message"], message,
                "the message must not depend on the envelope"
            );
            assert_eq!(
                anthropic["error"].as_object().unwrap().len(),
                2,
                "the Anthropic error object carries type and message only: {anthropic}"
            );
        }
    }

    /// The `IntoResponse` default is what the `/v1/models` extractor
    /// rejection renders, and it stays the OpenAI envelope.
    #[tokio::test]
    async fn the_default_envelope_stays_openai() {
        let (status, body) = body_of(ProxyAuthError::InvalidKey.into_response()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert!(body.get("type").is_none(), "body: {body}");
    }
}
