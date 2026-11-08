//! Integration tests for client-IP capture and trusted-proxy
//! `X-Forwarded-For` resolution: the spoofed header of an untrusted peer
//! never changes the logged IP, a trusted proxy chain resolves to the
//! first untrusted hop, and requests without ConnectInfo log no IP.

mod common;

use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body};
use axum::http::Request;
use sqlx::SqlitePool;
use tower::ServiceExt;

use common::{test_app, test_app_with_trusted_proxies, test_pool, within};
use hydra::config::TrustedProxies;

const LOOPBACK: &str = "127.0.0.1";

/// A valid non-streaming chat request body.
fn chat_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": "gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }],
    }))
    .unwrap()
}

/// Seeds an upstream (fixed 200) plus a key; returns the key plaintext.
async fn seed_ready_channel(db: &SqlitePool) -> String {
    let base = common::spawn_upstream(common::fixed_response_router(
        common::Recorder::default(),
        axum::http::StatusCode::OK,
        "application/json",
        r#"{"id":"x","object":"chat.completion","model":"gpt-4o"}"#,
    ))
    .await;
    let channel_id = common::seed_channel(db, &base, None).await;
    common::seed_key(db, channel_id).await
}

/// Sends one chat request through the served router (so ConnectInfo is
/// present) with an optional X-Forwarded-For header.
async fn send_chat(base_url: &str, key: &str, xff: Option<&str>) -> reqwest::StatusCode {
    let mut request = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(chat_body());
    if let Some(xff) = xff {
        request = request.header("x-forwarded-for", xff);
    }
    let response = within(request.send()).await.expect("request completes");
    let status = response.status();
    // Drain the body so the response is fully read before assertions.
    let _ = within(async { response.bytes().await }).await;
    status
}

/// Waits for the background log insert and returns the first row's
/// `client_ip` (the insert is detached, so the row lands a moment later).
async fn logged_client_ip(db: &SqlitePool) -> Option<String> {
    let deadline = Instant::now() + common::TEST_TIMEOUT;
    loop {
        let ip: Option<Option<String>> = within(
            sqlx::query_scalar("SELECT client_ip FROM request_logs ORDER BY id LIMIT 1")
                .fetch_optional(db),
        )
        .await
        .unwrap();
        if let Some(ip) = ip {
            return ip;
        }
        assert!(Instant::now() < deadline, "no request_logs row was written");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn spoofed_xff_from_untrusted_peer_is_ignored() {
    let db = test_pool().await;
    let key = seed_ready_channel(&db).await;
    // No trusted proxies: the direct peer is logged, never the header.
    let base = common::spawn_hydra_with_connect_info(test_app(db.clone())).await;

    let status = send_chat(&base, &key, Some("9.9.9.9, 8.8.8.8")).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    assert_eq!(
        logged_client_ip(&db).await,
        Some(LOOPBACK.to_string()),
        "the direct peer must be logged, not the spoofed XFF entries"
    );
}

#[tokio::test]
async fn trusted_proxy_chain_resolves_the_first_untrusted_hop() {
    let db = test_pool().await;
    let key = seed_ready_channel(&db).await;
    let trusted = TrustedProxies::parse("127.0.0.1/32, 10.0.0.0/8").unwrap();
    let base =
        common::spawn_hydra_with_connect_info(test_app_with_trusted_proxies(db.clone(), trusted))
            .await;

    let status = send_chat(&base, &key, Some("10.0.0.9, 198.51.100.5, 203.0.113.7")).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    assert_eq!(
        logged_client_ip(&db).await,
        Some("198.51.100.5".to_string()),
        "the first hop outside the trusted set is the client"
    );

    // The admin JSON list exposes the same value.
    let response = within(
        reqwest::Client::new()
            .get(format!("{base}/admin/logs"))
            .basic_auth("admin", Some("secret"))
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = within(async { response.json().await.expect("json body") }).await;
    assert_eq!(
        body["items"][0]["client_ip"], "198.51.100.5",
        "the admin list must carry the resolved client IP"
    );
}

#[tokio::test]
async fn trusted_proxy_skips_malformed_and_port_suffixed_hops() {
    let db = test_pool().await;
    let key = seed_ready_channel(&db).await;
    let trusted = TrustedProxies::parse("127.0.0.1").unwrap();
    let base =
        common::spawn_hydra_with_connect_info(test_app_with_trusted_proxies(db.clone(), trusted))
            .await;

    let status = send_chat(
        &base,
        &key,
        Some("10.0.0.1:8443, not-an-ip, , 198.51.100.5"),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "a malformed XFF header must never reject the request"
    );

    assert_eq!(
        logged_client_ip(&db).await,
        Some("198.51.100.5".to_string())
    );
}

#[tokio::test]
async fn all_trusted_chain_falls_back_to_the_peer_address() {
    let db = test_pool().await;
    let key = seed_ready_channel(&db).await;
    let trusted = TrustedProxies::parse("127.0.0.1").unwrap();
    let base =
        common::spawn_hydra_with_connect_info(test_app_with_trusted_proxies(db.clone(), trusted))
            .await;

    let status = send_chat(&base, &key, Some("127.0.0.1")).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    assert_eq!(
        logged_client_ip(&db).await,
        Some(LOOPBACK.to_string()),
        "nothing untrusted in the chain: the trusted peer itself is logged"
    );
}

#[tokio::test]
async fn ipv6_cidrs_in_the_trusted_list_keep_v4_resolution_working() {
    let db = test_pool().await;
    let key = seed_ready_channel(&db).await;
    // IPv6 networks alongside the loopback v4 host: parsing them must not
    // disturb the v4 match or the XFF resolution.
    let trusted = TrustedProxies::parse("fe80::/10, 2001:db8::/32, 127.0.0.1/32").unwrap();
    let base =
        common::spawn_hydra_with_connect_info(test_app_with_trusted_proxies(db.clone(), trusted))
            .await;

    let status = send_chat(&base, &key, Some("198.51.100.5")).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    assert_eq!(
        logged_client_ip(&db).await,
        Some("198.51.100.5".to_string())
    );
}

#[tokio::test]
async fn oneshot_request_without_connect_info_logs_no_ip() {
    let db = test_pool().await;
    let key = seed_ready_channel(&db).await;
    let app = test_app(db.clone());

    // oneshot serves the request without ConnectInfo: no peer address, so
    // no IP may be logged even with an X-Forwarded-For header present.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("x-forwarded-for", "9.9.9.9")
        .body(Body::from(chat_body()))
        .unwrap();
    let response = within(hydra::http::router(app).oneshot(request))
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let _ = within(to_bytes(response.into_body(), usize::MAX)).await;

    assert_eq!(
        logged_client_ip(&db).await,
        None,
        "without ConnectInfo no client IP is logged"
    );
}
