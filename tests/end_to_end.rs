//! Full-stack end-to-end tests: complete admin-to-proxy flows driven
//! through the real router, not seeded rows. The admin creates a channel
//! and issues a client key over HTTP, a client then calls the proxy with
//! that key, and the background log row is verified. Admin CRUD smoke and
//! the channel re-point flow ride the same wiring.

mod common;

use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use tower::ServiceExt;

use hydra::http::{router, AppState};

use common::{fixed_response_router, within, Recorder};

/// Basic auth matching the credentials `common::test_app` wires in.
fn admin_auth() -> String {
    format!("Basic {}", STANDARD.encode("admin:secret"))
}

/// Sends a JSON request through the real router and returns status and
/// raw body bytes, so secret-masking assertions see exactly what a client
/// would receive.
async fn send_raw(
    app: AppState,
    method: &str,
    uri: &str,
    authorization: Option<&str>,
    body: Option<Vec<u8>>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(authorization) = authorization {
        request = request.header("authorization", authorization);
    }
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let request = match body {
        Some(bytes) => request.body(Body::from(bytes)),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = within(router(app).oneshot(request)).await.unwrap();
    let status = response.status();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn admin_json(app: AppState, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let (status, bytes) = send_raw(
        app,
        method,
        uri,
        Some(&admin_auth()),
        Some(body.to_string().into_bytes()),
    )
    .await;
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("admin responses are JSON")
    };
    (status, parsed)
}

/// Polls until `count` request_logs rows exist and returns them; the log
/// insert runs on a detached task after the response returned.
async fn wait_for_logs(db: &SqlitePool, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + common::TEST_TIMEOUT;
    loop {
        let rows = within(
            sqlx::query(
                "SELECT api_key_id, channel_id, model, status, prompt_tokens, \
                 completion_tokens, request_body, response_body, error \
                 FROM request_logs ORDER BY id",
            )
            .fetch_all(db),
        )
        .await
        .unwrap();
        if rows.len() >= count {
            return rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "api_key_id": row.try_get::<i64, _>("api_key_id").unwrap(),
                        "channel_id": row.try_get::<i64, _>("channel_id").unwrap(),
                        "model": row.try_get::<Option<String>, _>("model").unwrap(),
                        "status": row.try_get::<Option<i64>, _>("status").unwrap(),
                        "prompt_tokens": row.try_get::<Option<i64>, _>("prompt_tokens").unwrap(),
                        "completion_tokens":
                            row.try_get::<Option<i64>, _>("completion_tokens").unwrap(),
                        "request_body":
                            row.try_get::<Option<String>, _>("request_body").unwrap(),
                        "response_body":
                            row.try_get::<Option<String>, _>("response_body").unwrap(),
                        "error": row.try_get::<Option<String>, _>("error").unwrap(),
                    })
                })
                .collect();
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} request_logs rows, got {}",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A valid chat completion request body for the given model.
fn chat_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
    }))
    .unwrap()
}

#[tokio::test]
async fn admin_issues_key_that_proxies_and_logs_end_to_end() {
    let db = common::test_pool().await;
    // The mock upstream returns a completion with a usage object so the
    // log row can be verified down to the parsed tokens.
    let recorder = Recorder::default();
    let completion = serde_json::json!({
        "id": "chatcmpl-e2e",
        "object": "chat.completion",
        "model": "gpt-4o",
        "usage": { "prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12 },
    })
    .to_string();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        completion.clone(),
    ))
    .await;
    let app = common::test_app_logging(db.clone());

    // Admin creates the channel over HTTP; the upstream secret must never
    // appear in the response.
    let (status, created_channel) = admin_json(
        app.clone(),
        "POST",
        "/admin/channels",
        serde_json::json!({
            "name": "primary",
            "base_url": base,
            "api_key": common::UPSTREAM_KEY,
            "allowed_models": ["gpt-4o"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let raw = created_channel.to_string();
    assert!(
        !raw.contains(common::UPSTREAM_KEY),
        "the upstream key must be masked in the creation response"
    );
    let channel_id = created_channel["id"].as_i64().unwrap();
    assert_eq!(created_channel["enabled"], true);
    assert_eq!(
        created_channel["allowed_models"],
        serde_json::json!(["gpt-4o"])
    );

    // Admin issues a client key; the full key is revealed exactly once.
    let (status, created_key) = admin_json(
        app.clone(),
        "POST",
        "/admin/keys",
        serde_json::json!({ "channel_id": channel_id, "name": "e2e key" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let full_key = created_key["key"].as_str().unwrap().to_string();
    assert!(full_key.starts_with("hyd-"), "issued keys carry the prefix");
    let key_id = created_key["id"].as_i64().unwrap();
    let display_prefix = created_key["display_prefix"].as_str().unwrap().to_string();
    assert!(full_key.starts_with(&display_prefix));

    // The key listing must not repeat the full key.
    let (status, listed) = admin_json(app.clone(), "GET", "/admin/keys", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let listed_raw = listed.to_string();
    assert!(
        !listed_raw.contains(&full_key),
        "listings expose the display prefix only"
    );
    assert_eq!(listed[0]["display_prefix"], display_prefix.as_str());

    // A client calls chat completions with the issued key.
    let request_body = chat_body("gpt-4o");
    let (status, body) = send_raw(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(&format!("Bearer {full_key}")),
        Some(request_body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        completion.as_bytes(),
        "the completion must pass through verbatim"
    );
    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {}", common::UPSTREAM_KEY).as_str()),
        "the upstream key must be substituted for the hydra key"
    );

    // The same key drives /v1/models, which serves the whitelist locally.
    let (status, models_body) = send_raw(
        app.clone(),
        "GET",
        "/v1/models",
        Some(&format!("Bearer {full_key}")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let models: Value = serde_json::from_slice(&models_body).unwrap();
    assert_eq!(models["data"][0]["id"], "gpt-4o");

    // The background writer leaves exactly one audit row for the chat call.
    let logs = wait_for_logs(&db, 1).await;
    assert_eq!(logs.len(), 1);
    let row = &logs[0];
    assert_eq!(row["api_key_id"], key_id);
    assert_eq!(row["channel_id"], channel_id);
    assert_eq!(row["model"], "gpt-4o");
    assert_eq!(row["status"], 200);
    assert_eq!(row["prompt_tokens"], 5);
    assert_eq!(row["completion_tokens"], 7);
    assert_eq!(
        row["request_body"],
        String::from_utf8(request_body).unwrap(),
        "body logging captures the forwarded request bytes"
    );
    assert_eq!(row["response_body"], completion);
    assert_eq!(row["error"], Value::Null);
}

#[tokio::test]
async fn patching_channel_base_url_repoints_traffic_without_new_keys() {
    let db = common::test_pool().await;
    // Two live upstreams with distinguishable bodies.
    let recorder_a = Recorder::default();
    let recorder_b = Recorder::default();
    let body_a = br#"{"id":"chatcmpl-from-a"}"#;
    let body_b = br#"{"id":"chatcmpl-from-b"}"#;
    let base_a = common::spawn_upstream(fixed_response_router(
        recorder_a.clone(),
        StatusCode::OK,
        "application/json",
        body_a.to_vec(),
    ))
    .await;
    let base_b = common::spawn_upstream(fixed_response_router(
        recorder_b.clone(),
        StatusCode::OK,
        "application/json",
        body_b.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base_a, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());
    let bearer = format!("Bearer {key}");

    let (status, first) = send_raw(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(&bearer),
        Some(chat_body("gpt-4o")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&first[..], &body_a[..]);

    // The admin re-points the channel; the already-issued key is unchanged.
    let (status, patched) = admin_json(
        app.clone(),
        "PATCH",
        &format!("/admin/channels/{channel_id}"),
        serde_json::json!({ "base_url": base_b }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["base_url"], base_b.as_str());

    let (status, second) = send_raw(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(&bearer),
        Some(chat_body("gpt-4o")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        &second[..],
        &body_b[..],
        "traffic must follow the new upstream"
    );
    assert_eq!(recorder_a.requests().len(), 1);
    assert_eq!(recorder_b.requests().len(), 1);
}
