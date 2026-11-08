//! Integration tests for the key→channel resolution cache.
//!
//! The DB-skip proof deliberately avoids deleting rows (the
//! `request_logs.api_key_id` FK is `ON DELETE RESTRICT`): the state's pool is
//! swapped for a closed pool after the cache is warmed, so a second proxied
//! call succeeding proves that no query ran at all. The security-flip tests
//! drive the admin API, which invalidates explicitly, so disabling and
//! rotating take effect on the very next call regardless of the TTL.

mod common;

use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use hydra::cache::ResolutionCache;
use hydra::db::Db;
use hydra::http::{router, AppState};

use common::{fixed_response_router, within, Recorder};

/// Sends a request through the real router and returns status and body.
async fn send(app: AppState, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = within(router(app).oneshot(request)).await.unwrap();
    let status = response.status();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    (status, bytes.to_vec())
}

fn chat_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": "gpt-4o",
        "stream": false,
        "messages": [{ "role": "user", "content": "hi" }],
    }))
    .unwrap()
}

fn chat_request(key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(Body::from(chat_body()))
        .unwrap()
}

fn admin_header() -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    format!("Basic {}", STANDARD.encode("admin:secret"))
}

async fn admin_json(app: &AppState, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", admin_header())
        .header("content-type", "application/json")
        .body(match body {
            Value::Null => Body::empty(),
            other => Body::from(other.to_string()),
        })
        .unwrap();
    let (status, bytes) = send(app.clone(), request).await;
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, parsed)
}

/// The id of the single seeded key, read back through the admin API.
async fn seeded_key_id(app: &AppState) -> i64 {
    let (_, listed) = admin_json(app, "GET", "/admin/keys", Value::Null).await;
    listed[0]["id"].as_i64().expect("seeded key must be listed")
}

/// A pool whose every query fails: connected, then closed, so any acquire
/// errors out. The cache tests swap this in to prove no query ran.
async fn dead_pool() -> sqlx::SqlitePool {
    let pool = within(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:"),
    )
    .await
    .unwrap();
    pool.close().await;
    pool
}

/// Seeds a channel on `base_url` with one key, wires `cache` into the app
/// state and warms it with one successful proxied call; returns the app and
/// the plaintext key.
async fn warmed_app(recorder: &Recorder, base: &str, cache: ResolutionCache) -> (AppState, String) {
    let db = common::test_pool().await;
    let channel_id = common::seed_channel(&db, base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let mut app = common::test_app(db);
    app.cache = cache;
    let (status, _) = send(app.clone(), chat_request(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recorder.requests().len(), 1);
    (app, key)
}

#[tokio::test]
async fn fresh_cache_hit_serves_proxy_calls_without_touching_the_database() {
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-cached"}"#.to_vec(),
    ))
    .await;
    let (app, key) = warmed_app(&recorder, &base, ResolutionCache::default()).await;

    // Every query against this pool fails; a served call proves the
    // resolution came from the cache, not the database.
    let mut offline = app.clone();
    offline.db = Db::from(dead_pool().await);

    let (status, body) = send(offline, chat_request(&key)).await;
    assert_eq!(status, StatusCode::OK, "fresh cache hit must skip the DB");
    assert_eq!(&body[..], &br#"{"id":"chatcmpl-cached"}"#[..]);
    assert_eq!(
        recorder.requests().len(),
        2,
        "both calls must reach the upstream"
    );
}

#[tokio::test]
async fn ttl_zero_consults_the_database_on_every_call() {
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-uncached"}"#.to_vec(),
    ))
    .await;
    let (app, key) = warmed_app(&recorder, &base, ResolutionCache::new(0)).await;

    // With the cache off, the resolution must hit the database again: the
    // dead pool turns the second call into a storage failure instead of a
    // served response.
    let mut offline = app.clone();
    offline.db = Db::from(dead_pool().await);
    let (status, _) = send(offline, chat_request(&key)).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "cache off must mean every call consults the (failing) database"
    );
}

#[tokio::test]
async fn admin_disabled_key_rejects_the_next_proxied_call_immediately() {
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-disabled"}"#.to_vec(),
    ))
    .await;
    let (app, key) = warmed_app(&recorder, &base, ResolutionCache::default()).await;
    let key_id = seeded_key_id(&app).await;

    let (status, _) = admin_json(
        &app,
        "PATCH",
        &format!("/admin/keys/{key_id}"),
        serde_json::json!({ "enabled": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(app, chat_request(&key)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "disable must take effect despite the warm cache"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
        "key_disabled"
    );
}

#[tokio::test]
async fn admin_deleted_key_rejects_the_next_proxied_call_immediately() {
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-deleted"}"#.to_vec(),
    ))
    .await;
    let (app, key) = warmed_app(&recorder, &base, ResolutionCache::default()).await;
    let key_id = seeded_key_id(&app).await;

    let (status, _) = admin_json(
        &app,
        "DELETE",
        &format!("/admin/keys/{key_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(app, chat_request(&key)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "soft delete must stop authentication despite the warm cache"
    );
}

#[tokio::test]
async fn admin_disabled_channel_rejects_the_next_proxied_call_immediately() {
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-ch-disabled"}"#.to_vec(),
    ))
    .await;
    let (app, key) = warmed_app(&recorder, &base, ResolutionCache::default()).await;
    let (_, created) = admin_json(&app, "GET", "/admin/channels", Value::Null).await;
    let channel_id = created[0]["id"].as_i64().unwrap();

    let (status, _) = admin_json(
        &app,
        "PATCH",
        &format!("/admin/channels/{channel_id}"),
        serde_json::json!({ "enabled": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(app, chat_request(&key)).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "channel disable must take effect despite the warm cache"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
        "channel_disabled"
    );
}

#[tokio::test]
async fn channel_patch_of_base_url_redirects_the_next_proxied_call() {
    let upstream_a = Recorder::default();
    let upstream_b = Recorder::default();
    let base_a = common::spawn_upstream(fixed_response_router(
        upstream_a.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-a"}"#.to_vec(),
    ))
    .await;
    let base_b = common::spawn_upstream(fixed_response_router(
        upstream_b.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-b"}"#.to_vec(),
    ))
    .await;

    let db = common::test_pool().await;
    let channel_id = common::seed_channel(&db, &base_a, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    // Warm the cache against upstream A.
    let (status, body) = send(app.clone(), chat_request(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], &br#"{"id":"chatcmpl-a"}"#[..]);

    // Rotate the upstream through the admin API; the cached entry must be
    // dropped, not served until the TTL runs out.
    let (status, _) = admin_json(
        &app,
        "PATCH",
        &format!("/admin/channels/{channel_id}"),
        serde_json::json!({ "base_url": base_b }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(app, chat_request(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], &br#"{"id":"chatcmpl-b"}"#[..]);
    assert_eq!(
        upstream_a.requests().len(),
        1,
        "the rotated-away upstream must see no further traffic"
    );
    assert_eq!(upstream_b.requests().len(), 1);
}

#[tokio::test]
async fn out_of_band_key_disable_stays_stale_until_the_ttl_expires() {
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"chatcmpl-ttl"}"#.to_vec(),
    ))
    .await;
    let (app, key) = warmed_app(&recorder, &base, ResolutionCache::new(1)).await;
    let key_id = seeded_key_id(&app).await;

    // Out-of-band flip: a direct database write that bypasses the admin API
    // and therefore bypasses explicit invalidation. This is exactly the
    // drift the TTL exists to bound.
    let db_pool = app.db.as_sqlite().clone();
    common::disable_key(&db_pool, key_id).await;

    let (status, _) = send(app.clone(), chat_request(&key)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "within the TTL the out-of-band flip is still stale"
    );

    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (status, _) = send(app, chat_request(&key)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "after the TTL the database is consulted again and the flip applies"
    );
}
