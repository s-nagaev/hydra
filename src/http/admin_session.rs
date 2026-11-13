//! Signed session cookies for the admin UI.
//!
//! The admin UI authenticates either with the Basic credentials or with a
//! session cookie, so the browser stops asking for a password on every page
//! load. A session is a self-contained token: the payload
//! `{"exp": <unix_ts>, "nonce": "<random>"}` with an HMAC-SHA256 signature
//! over it. Nothing is stored server-side, so a session cannot be revoked
//! individually: it dies with its expiry, or all at once when the key it was
//! signed with changes.
//!
//! The signing key is HKDF-SHA256 over `HYDRA_ADMIN_PASSWORD` (or over
//! `HYDRA_SESSION_SECRET` when that optional variable is set), so changing
//! the admin password invalidates every session that was issued under the
//! old one. The key never leaves the process and is never logged; the
//! password it derives from is itself only compared in constant time.
//!
//! Token layout: `base64url(payload) "." base64url(hmac)`. Verification
//! checks the signature in constant time *before* parsing the payload, so a
//! forged token never reaches the JSON parser with attacker-controlled bytes.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::http::admin_auth::constant_time_eq;

/// Name of the session cookie.
pub const COOKIE_NAME: &str = "hydra_admin_session";

/// The cookie's scope: the admin UI and everything below it, so the login
/// and logout routes are covered too.
pub const COOKIE_PATH: &str = "/admin/ui";

/// Session lifetime in seconds (7 days).
pub const TTL_SECS: u64 = 7 * 24 * 60 * 60;

const SALT: &[u8] = b"hydra admin session v1";
const INFO: &[u8] = b"session-cookie";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 16;

/// The current wall-clock time in seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// HMAC key for session tokens. Debug output never reveals the key material.
#[derive(Clone)]
pub struct SessionKey([u8; KEY_LEN]);

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKey(<redacted>)")
    }
}

impl SessionKey {
    /// Derives the key from `secret`: the admin password, or the session
    /// secret that overrides it. The same secret always yields the same key,
    /// and a different one never does.
    pub fn derive(secret: &[u8]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(Some(SALT), secret);
        let mut key = [0u8; KEY_LEN];
        hkdf.expand(INFO, &mut key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        SessionKey(key)
    }

    /// Issues a token that expires at `expires_at`. The nonce makes every
    /// token unique, so two logins in the same second differ.
    pub fn mint(&self, expires_at: u64) -> String {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("OS RNG is unavailable");
        let payload = format!(
            r#"{{"exp":{},"nonce":"{}"}}"#,
            expires_at,
            URL_SAFE_NO_PAD.encode(nonce)
        );
        let signature = self.sign(payload.as_bytes());
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload.as_bytes()),
            URL_SAFE_NO_PAD.encode(signature)
        )
    }

    /// Whether `token` is a well-formed, correctly signed session that is
    /// still valid at `now`.
    pub fn verify(&self, token: &str, now: u64) -> bool {
        let Some((payload, signature)) = token.split_once('.') else {
            return false;
        };
        let (Ok(payload), Ok(signature)) = (
            URL_SAFE_NO_PAD.decode(payload),
            URL_SAFE_NO_PAD.decode(signature),
        ) else {
            return false;
        };
        // Signature first: the payload is untrusted until it verifies.
        if !constant_time_eq(&self.sign(&payload), &signature) {
            return false;
        }
        let Ok(claims) = serde_json::from_slice::<Claims>(&payload) else {
            return false;
        };
        claims.exp > now
    }

    fn sign(&self, payload: &[u8]) -> [u8; KEY_LEN] {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC accepts keys of any length");
        mac.update(payload);
        mac.finalize().into_bytes().into()
    }
}

/// The signed payload of a session token; unknown fields are ignored.
#[derive(serde::Deserialize)]
struct Claims {
    exp: u64,
}

/// `Set-Cookie` value handing a fresh session to the browser. `Secure` keeps
/// the cookie off plain-HTTP origins other than `localhost` and `127.0.0.1`,
/// which browsers treat as trustworthy; a deployment reached over plain HTTP
/// by IP address should sit behind TLS, as the Basic credentials it replaces
/// deserve the same protection.
pub fn session_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Max-Age={TTL_SECS}; Path={COOKIE_PATH}; HttpOnly; Secure; SameSite=Lax"
    )
}

/// `Set-Cookie` value removing the session cookie from the browser.
pub fn expired_cookie() -> String {
    format!("{COOKIE_NAME}=; Max-Age=0; Path={COOKIE_PATH}; HttpOnly; Secure; SameSite=Lax")
}

/// The session token carried by the request, read from the `Cookie` header
/// rather than from any other place a token could hide.
pub fn cookie_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    for value in headers.get_all(axum::http::header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            if let Some((name, token)) = pair.split_once('=') {
                if name.trim() == COOKIE_NAME {
                    return Some(token.trim());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_verifies_until_it_expires() {
        let key = SessionKey::derive(b"admin-password");
        let token = key.mint(1_000);
        assert!(key.verify(&token, 999));
        assert!(!key.verify(&token, 1_000));
        assert!(!key.verify(&token, 1_001));
    }

    #[test]
    fn a_token_from_another_key_never_verifies() {
        let token = SessionKey::derive(b"old-password").mint(u64::MAX);
        let rotated = SessionKey::derive(b"new-password");
        assert!(!rotated.verify(&token, 0));
    }

    #[test]
    fn one_secret_yields_one_key_and_every_token_is_unique() {
        let first = SessionKey::derive(b"admin-password").mint(u64::MAX);
        let second = SessionKey::derive(b"admin-password").mint(u64::MAX);
        assert_ne!(first, second, "the nonce must make every token unique");
        assert!(
            SessionKey::derive(b"admin-password").verify(&first, 0),
            "the same secret must derive the same key"
        );
    }

    #[test]
    fn the_signature_is_not_the_raw_password_hash() {
        // A token whose signature is the bare password bytes (the shape a
        // naive implementation would produce) must not verify.
        let forged = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"exp":18446744073709551615,"nonce":"x"}"#),
            URL_SAFE_NO_PAD.encode(b"admin-password")
        );
        assert!(!SessionKey::derive(b"admin-password").verify(&forged, 0));
    }

    #[test]
    fn a_tampered_payload_or_signature_is_rejected() {
        let key = SessionKey::derive(b"admin-password");
        let token = key.mint(u64::MAX);
        let (payload, signature) = token.split_once('.').unwrap();

        let decoded = URL_SAFE_NO_PAD.decode(payload).unwrap();
        let extended = String::from_utf8(decoded)
            .unwrap()
            .replace(u64::MAX.to_string().as_str(), "0");
        let tampered = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(extended.as_bytes()),
            signature
        );
        assert!(
            !key.verify(&tampered, 0),
            "an extended expiry must invalidate the signature"
        );

        let mut signature = signature.to_string();
        let last = signature.pop().unwrap();
        signature.push(if last == 'A' { 'B' } else { 'A' });
        assert!(!key.verify(&format!("{payload}.{signature}"), 0));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let key = SessionKey::derive(b"admin-password");
        for token in [
            "",
            ".",
            "no-dot",
            "eyJleHAiOjF9.",
            "!!!.???",
            &format!("{}.{}", URL_SAFE_NO_PAD.encode(b"not json"), "AAAA"),
        ] {
            assert!(!key.verify(token, 0), "{token:?} must not verify");
        }
    }

    #[test]
    fn the_cookie_carries_the_hardening_attributes() {
        let cookie = session_cookie("token-value");
        assert!(cookie.starts_with("hydra_admin_session=token-value"));
        for attribute in [
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Path=/admin/ui",
            &format!("Max-Age={TTL_SECS}"),
        ] {
            assert!(cookie.contains(attribute), "{cookie} misses {attribute}");
        }
    }

    #[test]
    fn the_expired_cookie_clears_the_same_scope() {
        let cookie = expired_cookie();
        assert!(cookie.starts_with("hydra_admin_session=;"));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("Path=/admin/ui"));
    }

    #[test]
    fn cookie_token_reads_only_the_session_cookie() {
        let headers = |raw: &str| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(axum::http::header::COOKIE, raw.parse().unwrap());
            headers
        };
        assert_eq!(
            cookie_token(&headers("hydra_admin_session=abc")),
            Some("abc")
        );
        assert_eq!(
            cookie_token(&headers("other=1; hydra_admin_session = abc ; more=2")),
            Some("abc")
        );
        assert_eq!(cookie_token(&headers("other=1; another=2")), None);
        assert_eq!(
            cookie_token(&headers("hydra_admin_session_suffix=abc")),
            None
        );
        assert_eq!(cookie_token(&axum::http::HeaderMap::new()), None);
    }
}
