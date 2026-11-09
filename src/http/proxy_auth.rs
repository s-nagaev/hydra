//! Bearer-key authentication for the proxy routes.
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
//! Rejections use the
//! OpenAI error shape shared by all proxy routes: 401 for a missing or
//! malformed header and for an unknown key, 403 for a disabled key, 500 for
//! storage failures. The plaintext key never appears in a response or log
//! line.
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
//! [`ProxyAuthError::MonthlyLimitReached`], a 429 in the OpenAI error shape.
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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::db::channels::{self, Channel};
use crate::db::keys::{self, ApiKey};
use crate::flood::{Admission, FloodTicket};
use crate::http::client_ip;
use crate::http::openai_error::openai_error;
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

impl IntoResponse for ProxyAuthError {
    fn into_response(self) -> Response {
        match self {
            // The flood-capped response is deliberately indistinguishable
            // from the unknown-key one.
            ProxyAuthError::InvalidKey | ProxyAuthError::FloodCapped(_) => openai_error(
                StatusCode::UNAUTHORIZED,
                "Invalid API key provided",
                "invalid_request_error",
                Some("invalid_api_key"),
            ),
            ProxyAuthError::DisabledKey => openai_error(
                StatusCode::FORBIDDEN,
                "This API key has been disabled",
                "invalid_request_error",
                Some("key_disabled"),
            ),
            ProxyAuthError::MonthlyLimitReached { .. } => openai_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Monthly limit reached",
                "rate_limit_error",
                None,
            ),
            ProxyAuthError::Database(e) => {
                tracing::error!(error = %e, "proxy key lookup failed");
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

impl FromRequestParts<AppState> for ProxyAuth {
    type Rejection = ProxyAuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
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

        let resolution = Self::resolve_key(parts, state).await;
        match resolution {
            // An invalid key is exactly the failure the budget counts: its
            // admission slot settles into a recorded failure.
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
                Self::check_monthly_limit(state, &auth)?;
                Ok(auth)
            }
        }
    }
}

impl ProxyAuth {
    /// Header parsing, cache probe and database resolution for one request,
    /// after the flood gate has admitted it.
    async fn resolve_key(parts: &mut Parts, state: &AppState) -> Result<Self, ProxyAuthError> {
        let header_value = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(ProxyAuthError::InvalidKey)?;
        let token = parse_bearer(header_value).ok_or(ProxyAuthError::InvalidKey)?;
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
}
