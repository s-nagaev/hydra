//! End-to-end tests for the background request logging of the chat proxy:
//! one `request_logs` row per authenticated chat request, written after the
//! client flow finishes, with opt-in body capture and token parsing.

mod common;

use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use sqlx::{Row, SqlitePool};
use tower::ServiceExt;

use hydra::http::{router, AppState};
use hydra::logging::BODY_CAPTURE_LIMIT;

use common::{fixed_response_router, sse_router, within, Recorder};

/// One `request_logs` row, as read back through a runtime sqlx query.
struct LogRow {
    api_key_id: i64,
    channel_id: i64,
    model: Option<String>,
    status: Option<i64>,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    duration_ms: i64,
    request_body: Option<String>,
    response_body: Option<String>,
    error: Option<String>,
}

fn map_log_row(row: &sqlx::sqlite::SqliteRow) -> LogRow {
    LogRow {
        api_key_id: row.try_get("api_key_id").unwrap(),
        channel_id: row.try_get("channel_id").unwrap(),
        model: row.try_get("model").unwrap(),
        status: row.try_get("status").unwrap(),
        prompt_tokens: row.try_get("prompt_tokens").unwrap(),
        completion_tokens: row.try_get("completion_tokens").unwrap(),
        duration_ms: row.try_get("duration_ms").unwrap(),
        request_body: row.try_get("request_body").unwrap(),
        response_body: row.try_get("response_body").unwrap(),
        error: row.try_get("error").unwrap(),
    }
}

async fn fetch_logs(db: &SqlitePool) -> Vec<LogRow> {
    let rows = within(
        sqlx::query(
            "SELECT api_key_id, channel_id, model, status, prompt_tokens, \
             completion_tokens, duration_ms, request_body, response_body, error \
             FROM request_logs ORDER BY id",
        )
        .fetch_all(db),
    )
    .await
    .unwrap();
    rows.iter().map(map_log_row).collect()
}

/// The log insert runs on a detached task, so the row appears some time
/// after the response returned; poll until the expected count is there.
async fn wait_for_logs(db: &SqlitePool, count: usize) -> Vec<LogRow> {
    let deadline = Instant::now() + common::TEST_TIMEOUT;
    loop {
        let logs = fetch_logs(db).await;
        if logs.len() >= count {
            return logs;
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} request_logs rows, got {}",
            logs.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn key_id(db: &SqlitePool, channel_id: i64) -> i64 {
    within(
        sqlx::query_scalar("SELECT id FROM api_keys WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(db),
    )
    .await
    .unwrap()
}

/// A valid chat completion request body for the given model.
fn chat_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": model,
        "stream": stream,
        "messages": [{ "role": "user", "content": "hi" }],
    }))
    .unwrap()
}

async fn send_chat(app: AppState, key: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = within(router(app).oneshot(request)).await.unwrap();
    let status = response.status();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// Seeds a channel pointing at a fixed 200 JSON upstream carrying a usage
/// object, plus a client key; returns (base_url, channel_id, key, body).
async fn seed_usage_upstream(db: &SqlitePool) -> (String, i64, String, String) {
    let completion = serde_json::json!({
        "id": "chatcmpl-log",
        "object": "chat.completion",
        "model": "gpt-4o",
        "usage": { "prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46 },
    })
    .to_string();
    let base = common::spawn_upstream(fixed_response_router(
        Recorder::default(),
        StatusCode::OK,
        "application/json",
        completion.clone(),
    ))
    .await;
    let channel_id = common::seed_channel(db, &base, None).await;
    let key = common::seed_key(db, channel_id).await;
    (base, channel_id, key, completion)
}

#[tokio::test]
async fn non_stream_chat_logs_one_row_with_tokens_and_null_bodies() {
    let db = common::test_pool().await;
    let (_, channel_id, key, _) = seed_usage_upstream(&db).await;
    let app = common::test_app(db.clone());

    let (status, _) = send_chat(app, &key, chat_body("gpt-4o", false)).await;
    assert_eq!(status, StatusCode::OK);

    let logs = wait_for_logs(&db, 1).await;
    assert_eq!(logs.len(), 1, "exactly one row per request");
    let row = &logs[0];
    assert_eq!(row.api_key_id, key_id(&db, channel_id).await);
    assert_eq!(row.channel_id, channel_id);
    assert_eq!(row.model.as_deref(), Some("gpt-4o"));
    assert_eq!(row.status, Some(200));
    assert_eq!(row.prompt_tokens, Some(12));
    assert_eq!(row.completion_tokens, Some(34));
    assert!(row.duration_ms > 0);
    assert!(row.request_body.is_none(), "bodies off: no capture");
    assert!(row.response_body.is_none(), "bodies off: no capture");
    assert!(row.error.is_none());
}

#[tokio::test]
async fn non_stream_chat_with_body_logging_captures_request_and_response() {
    let db = common::test_pool().await;
    let (_, channel_id, key, completion) = seed_usage_upstream(&db).await;
    let app = common::test_app_logging(db.clone());

    let request_body = chat_body("gpt-4o", false);
    let (status, response_bytes) = send_chat(app, &key, request_body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response_bytes, completion.as_bytes());

    let logs = wait_for_logs(&db, 1).await;
    let row = &logs[0];
    assert_eq!(row.api_key_id, key_id(&db, channel_id).await);
    assert_eq!(
        row.request_body.as_deref(),
        Some(String::from_utf8(request_body).unwrap().as_str()),
        "the peeked original request bytes must be stored verbatim"
    );
    assert_eq!(row.response_body.as_deref(), Some(completion.as_str()));
    assert_eq!(row.prompt_tokens, Some(12));
    assert_eq!(row.completion_tokens, Some(34));
}

#[tokio::test]
async fn streaming_chat_with_body_logging_captures_sse_buffer_without_usage() {
    let db = common::test_pool().await;
    let chunks: Vec<Bytes> = vec![
        br#"data: {"choices":[{"delta":{"content":"He"}}]}"#.to_vec().into(),
        b"\n\n".to_vec().into(),
        br#"data: {"choices":[{"delta":{"content":"llo"}}]}"#.to_vec().into(),
        b"\n\n".to_vec().into(),
        b"data: [DONE]\n\n".to_vec().into(),
    ];
    let expected: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let base = common::spawn_upstream(sse_router(Recorder::default(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app_logging(db.clone());

    let (status, body) = send_chat(app, &key, chat_body("gpt-4o", true)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, expected, "the client stream must be unaffected");

    let logs = wait_for_logs(&db, 1).await;
    let row = &logs[0];
    assert_eq!(row.status, Some(200));
    assert_eq!(
        row.response_body.as_deref(),
        Some(String::from_utf8(expected.clone()).unwrap().as_str()),
        "the teed SSE byte buffer must be captured"
    );
    assert_eq!(row.prompt_tokens, None, "no usage chunk was sent");
    assert_eq!(row.completion_tokens, None);
    assert!(row.error.is_none());
    assert!(row.duration_ms > 0);
}

#[tokio::test]
async fn streaming_chat_with_usage_chunk_parses_tokens() {
    let db = common::test_pool().await;
    let chunks: Vec<Bytes> = vec![
        br#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#.to_vec().into(),
        b"\n\n".to_vec().into(),
        br#"data: {"choices":[],"usage":null}"#.to_vec().into(),
        b"\n\n".to_vec().into(),
        br#"data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":9}}"#
            .to_vec()
            .into(),
        b"\n\n".to_vec().into(),
        b"data: [DONE]\n\n".to_vec().into(),
    ];
    let base = common::spawn_upstream(sse_router(Recorder::default(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app_logging(db.clone());

    let (status, _) = send_chat(app, &key, chat_body("gpt-4o", true)).await;
    assert_eq!(status, StatusCode::OK);

    let logs = wait_for_logs(&db, 1).await;
    let row = &logs[0];
    assert_eq!(row.status, Some(200));
    assert_eq!(row.prompt_tokens, Some(7));
    assert_eq!(row.completion_tokens, Some(9));
}

#[tokio::test]
async fn streaming_chat_without_body_logging_leaves_bodies_and_tokens_null() {
    let db = common::test_pool().await;
    let chunks: Vec<Bytes> = vec![
        br#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#.to_vec().into(),
        b"\n\n".to_vec().into(),
        b"data: [DONE]\n\n".to_vec().into(),
    ];
    let base = common::spawn_upstream(sse_router(Recorder::default(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let (status, _) = send_chat(app, &key, chat_body("gpt-4o", true)).await;
    assert_eq!(status, StatusCode::OK);

    let logs = wait_for_logs(&db, 1).await;
    let row = &logs[0];
    assert_eq!(row.status, Some(200));
    assert!(row.request_body.is_none());
    assert!(row.response_body.is_none());
    assert!(row.prompt_tokens.is_none());
    assert!(row.completion_tokens.is_none());
}

#[tokio::test]
async fn upstream_connection_failure_logs_row_with_null_status_and_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let (status, _) = send_chat(app, &key, chat_body("gpt-4o", false)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    let logs = wait_for_logs(&db, 1).await;
    let row = &logs[0];
    assert_eq!(row.api_key_id, key_id(&db, channel_id).await);
    assert_eq!(row.status, None, "the request never reached the upstream");
    let error = row
        .error
        .as_deref()
        .expect("transport failures carry an error");
    assert!(
        error.to_lowercase().contains("upstream"),
        "error must name the upstream failure: {error}"
    );
    assert!(row.request_body.is_none());
    assert!(row.response_body.is_none());
}

#[tokio::test]
async fn whitelisted_out_model_rejection_is_logged_without_upstream_status() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["gpt-4o".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let (status, _) = send_chat(app, &key, chat_body("gpt-3.5-turbo", false)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let logs = wait_for_logs(&db, 1).await;
    let row = &logs[0];
    assert_eq!(row.model.as_deref(), Some("gpt-3.5-turbo"));
    assert_eq!(row.status, None);
    assert!(
        row.error
            .as_deref()
            .unwrap()
            .to_lowercase()
            .contains("whitelist"),
        "rejection reason must be recorded: {:?}",
        row.error
    );
}

#[tokio::test]
async fn log_rows_survive_key_soft_delete() {
    let db = common::test_pool().await;
    let (_, channel_id, key, _) = seed_usage_upstream(&db).await;
    let app = common::test_app(db.clone());
    let key_row_id = key_id(&db, channel_id).await;

    let (status, _) = send_chat(app, &key, chat_body("gpt-4o", false)).await;
    assert_eq!(status, StatusCode::OK);
    wait_for_logs(&db, 1).await;

    // Soft delete the key the way the admin endpoint does; the log rows
    // must keep referencing it (the FK stays satisfied precisely because
    // the row is not physically removed).
    let outcome = within(hydra::db::keys::delete(&db, key_row_id))
        .await
        .unwrap();
    assert!(matches!(outcome, hydra::db::keys::DeleteOutcome::Deleted));

    let logs = fetch_logs(&db).await;
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].api_key_id, key_row_id);
    let deleted_at: Option<String> = within(
        sqlx::query_scalar("SELECT deleted_at FROM api_keys WHERE id = ?")
            .bind(key_row_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert!(deleted_at.is_some(), "the key must be soft-deleted");
}

#[tokio::test]
async fn captured_response_body_is_truncated_at_the_limit() {
    let db = common::test_pool().await;
    // A valid JSON body larger than the capture cap; its huge string value
    // makes truncation measurable without depending on JSON framing.
    let oversized = format!("{{\"padding\":\"{}\"}}", "a".repeat(BODY_CAPTURE_LIMIT));
    let base = common::spawn_upstream(fixed_response_router(
        Recorder::default(),
        StatusCode::OK,
        "application/json",
        oversized.clone(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app_logging(db.clone());

    let (status, _) = send_chat(app, &key, chat_body("gpt-4o", false)).await;
    assert_eq!(status, StatusCode::OK);

    let logs = wait_for_logs(&db, 1).await;
    let row = &logs[0];
    let captured = row.response_body.as_deref().expect("bodies on");
    assert_eq!(
        captured.len(),
        BODY_CAPTURE_LIMIT,
        "capture stops at the cap"
    );
    assert!(captured.starts_with("{\"padding\":\"a"));
}
