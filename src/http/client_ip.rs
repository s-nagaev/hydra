//! Client IP resolution for proxied requests.
//!
//! The direct peer address is the source of truth. Only when the peer is
//! inside a [`TrustedProxies`](crate::config::TrustedProxies) network is the
//! `X-Forwarded-For` chain consulted, taking the first untrusted hop
//! left-to-right: everything before it is the chain of proxies we trust.
//! Malformed or port-suffixed entries are skipped, never rejected. An
//! untrusted peer's `X-Forwarded-For` header is ignored entirely, so a
//! spoofed header cannot influence the logged IP. Without peer info
//! (in-process test requests) no IP is logged at all.

use std::net::{IpAddr, SocketAddr};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;

use crate::config::TrustedProxies;

/// Extractor for the connection's peer address. Absent when the request was
/// served without `ConnectInfo` (in-process `oneshot` test requests), which
/// also means no IP is logged. Kept as a dedicated extractor because axum's
/// `ConnectInfo` does not implement the optional-extraction trait that
/// `Option<ConnectInfo<T>>` in a handler would require.
#[derive(Clone, Copy, Debug, Default)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

/// Maximum length of a stored client IP. A parsed address is at most 45
/// characters, so this only guards against a future change storing raw
/// header text.
const MAX_STORED_LEN: usize = 64;

/// `X-Forwarded-For` header name, lowercased for lookup.
const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// Resolves the client IP for one request: the direct peer unless the peer
/// is a trusted proxy, in which case the first untrusted hop of the
/// `X-Forwarded-For` chain. `None` when the connection's peer address is
/// unavailable.
pub fn resolve_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted: &TrustedProxies,
) -> Option<IpAddr> {
    let peer = peer?;
    if !trusted.contains(peer.ip()) {
        return Some(peer.ip());
    }
    let untrusted = headers
        .get(X_FORWARDED_FOR)
        .and_then(|value| value.to_str().ok())
        .and_then(|chain| {
            chain.split(',').find_map(|hop| {
                let hop = hop.trim();
                if hop.is_empty() {
                    return None;
                }
                // A port-suffixed entry fails the IP parse and is skipped
                // like any other malformed hop; the header never rejects
                // the request.
                hop.parse::<IpAddr>()
                    .ok()
                    .filter(|ip| !trusted.contains(*ip))
            })
        });
    Some(untrusted.unwrap_or(peer.ip()))
}

/// Text form of [`resolve_ip`], capped in length. The returned value is
/// always a single parsed address string, never raw header content.
pub fn resolve(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted: &TrustedProxies,
) -> Option<String> {
    resolve_ip(headers, peer, trusted).map(|ip| capped(ip.to_string()))
}

/// Caps the stored value so nothing arbitrarily long can ever land in the
/// column.
fn capped(value: String) -> String {
    if value.len() <= MAX_STORED_LEN {
        value
    } else {
        value.chars().take(MAX_STORED_LEN).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(raw: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(raw) = raw {
            headers.insert(
                X_FORWARDED_FOR,
                raw.parse().expect("test header value must be valid"),
            );
        }
        headers
    }

    fn peer(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 45000)
    }

    #[test]
    fn no_peer_information_logs_nothing() {
        let trusted = TrustedProxies::default();
        assert_eq!(resolve(&header_map(None), None, &trusted), None);
        assert_eq!(resolve(&header_map(Some("9.9.9.9")), None, &trusted), None);
    }

    #[test]
    fn untrusted_peer_is_logged_directly_and_ignores_xff() {
        let trusted = TrustedProxies::default();
        let headers = header_map(Some("9.9.9.9, 8.8.8.8"));
        // A spoofed chain from an untrusted peer must not change the result.
        assert_eq!(
            resolve(&headers, Some(peer("203.0.113.10")), &trusted),
            Some("203.0.113.10".to_string())
        );
    }

    #[test]
    fn trusted_proxy_resolves_the_first_untrusted_hop() {
        let loopback_only = TrustedProxies::parse("127.0.0.1").unwrap();
        let headers = header_map(Some("10.0.0.9, 198.51.100.5, 203.0.113.7"));
        assert_eq!(
            resolve(&headers, Some(peer("127.0.0.1")), &loopback_only),
            Some("10.0.0.9".to_string()),
            "the first hop not in the trusted set is the client"
        );

        // 10.0.0.9 is trusted too: the chain continues past it.
        let loopback_and_private = TrustedProxies::parse("127.0.0.1, 10.0.0.0/8").unwrap();
        assert_eq!(
            resolve(&headers, Some(peer("127.0.0.1")), &loopback_and_private),
            Some("198.51.100.5".to_string())
        );

        // No trusted proxies: the peer itself is the client.
        let none = TrustedProxies::default();
        assert_eq!(
            resolve(&headers, Some(peer("127.0.0.1")), &none),
            Some("127.0.0.1".to_string())
        );
    }

    #[test]
    fn malformed_and_port_suffixed_hops_are_skipped() {
        let trusted = TrustedProxies::parse("127.0.0.1").unwrap();
        let headers = header_map(Some("10.0.0.1:8443, not-an-ip, , 198.51.100.5"));
        assert_eq!(
            resolve(&headers, Some(peer("127.0.0.1")), &trusted),
            Some("198.51.100.5".to_string())
        );
    }

    #[test]
    fn missing_or_unparseable_xff_falls_back_to_the_trusted_peer() {
        let trusted = TrustedProxies::parse("127.0.0.1").unwrap();
        assert_eq!(
            resolve(&header_map(None), Some(peer("127.0.0.1")), &trusted),
            Some("127.0.0.1".to_string())
        );
        let garbage = header_map(Some(", , ,"));
        assert_eq!(
            resolve(&garbage, Some(peer("127.0.0.1")), &trusted),
            Some("127.0.0.1".to_string())
        );
    }

    #[test]
    fn all_trusted_chain_falls_back_to_the_peer() {
        let trusted = TrustedProxies::parse("127.0.0.1, 10.0.0.0/8").unwrap();
        let headers = header_map(Some("10.0.0.9, 10.0.0.10"));
        assert_eq!(
            resolve(&headers, Some(peer("127.0.0.1")), &trusted),
            Some("127.0.0.1".to_string())
        );
    }

    #[test]
    fn ipv6_peers_and_cidrs_resolve() {
        let trusted = TrustedProxies::parse("::1, 2001:db8::/32").unwrap();
        let headers = header_map(Some("2001:db8::1, 198.51.100.5"));
        assert_eq!(
            resolve(&headers, Some(peer("::1")), &trusted),
            Some("198.51.100.5".to_string())
        );

        let none = TrustedProxies::default();
        assert_eq!(
            resolve(&header_map(None), Some(peer("2001:db8::1")), &none),
            Some("2001:db8::1".to_string())
        );
    }

    #[test]
    fn stored_value_stays_below_the_length_cap() {
        // Every text form an IP address can take is far below the cap, so
        // the cap can never truncate a real resolution.
        let longest_ip = "::ffff:255.255.255.255";
        assert!(longest_ip.len() < MAX_STORED_LEN);
        let trusted = TrustedProxies::default();
        let resolved = resolve(&header_map(None), Some(peer(longest_ip)), &trusted).unwrap();
        assert_eq!(resolved, longest_ip);
    }
}
