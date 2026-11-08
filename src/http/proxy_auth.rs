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

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::db::channels::{self, Channel};
use crate::db::keys::{self, ApiKey};
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
    DisabledKey,
    Database(#[from] sqlx::Error),
}

impl IntoResponse for ProxyAuthError {
    fn into_response(self) -> Response {
        match self {
            ProxyAuthError::InvalidKey => openai_error(
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
