//! End-to-end tests for the `POST /v1/responses` proxy route against a real
//! in-process mock upstream. The route runs the same thin-passthrough
//! pipeline as chat, so what is verified here is what differs or could
//! silently differ: the upstream path it forwards to, the route-specific
//! 32 MiB body limit (the layer is per-route, and without it the route would
//! fall back to axum's 2 MiB default), the shared rejection mapping, and the
//! Responses usage shape landing in the request log on both the streaming
//! and the buffered path.

mod common;

use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use tower::ServiceExt;

use hydra::http::{router, AppState};
use hydra::logging::BODY_CAPTURE_LIMIT;

use common::{fixed_response_router, sse_router, within, Recorder};

/// A valid Responses request body for the given model: the surface's own
/// `input` field, not chat's `messages`.
fn responses_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "stream": stream,
        "input": [{ "role": "user", "content": "hi" }],
    }))
    .unwrap()
}

/// Sends a request through the real router and returns status, headers and
/// the full body.
async fn send(app: AppState, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = within(router(app).oneshot(request)).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

fn responses_request(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// A Responses request carrying one extra client header that must never
/// reach the upstream.
fn responses_request_with_leaky_header(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("x-internal-leak", "must-not-hop")
        .body(Body::from(body))
        .unwrap()
}

fn error_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("proxy errors must be OpenAI-shaped JSON")
}

/// The `request_logs` columns these tests read back.
struct LogRow {
    status: Option<i64>,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cached_output_tokens: Option<i64>,
    request_body: Option<String>,
    response_body: Option<String>,
}

/// The first logged row, polled for: the insert runs on a detached task, so
/// it appears some time after the response returned.
async fn wait_for_row(db: &SqlitePool) -> LogRow {
    let deadline = Instant::now() + common::TEST_TIMEOUT;
    loop {
        let rows = within(
            sqlx::query(
                "SELECT status, prompt_tokens, completion_tokens, cached_input_tokens, \
                 cached_output_tokens, request_body, response_body \
                 FROM request_logs ORDER BY id",
            )
            .fetch_all(db),
        )
        .await
        .unwrap();
        if let Some(row) = rows.first() {
            return LogRow {
                status: row.try_get("status").unwrap(),
                prompt_tokens: row.try_get("prompt_tokens").unwrap(),
                completion_tokens: row.try_get("completion_tokens").unwrap(),
                cached_input_tokens: row.try_get("cached_input_tokens").unwrap(),
                cached_output_tokens: row.try_get("cached_output_tokens").unwrap(),
                request_body: row.try_get("request_body").unwrap(),
                response_body: row.try_get("response_body").unwrap(),
            };
        }
        assert!(
            Instant::now() < deadline,
            "no request_logs row landed for the responses call"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn responses_route_forwards_verbatim_to_the_upstream_responses_path() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let response_body = br#"{"id":"resp_1","object":"response","status":"completed","output":[]}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        response_body.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["gpt-5".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request_body = responses_body("gpt-5", false);
    let (status, headers, body) = send(
        app,
        responses_request_with_leaky_header(&key, request_body.clone()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        &body[..],
        &response_body[..],
        "the upstream response must pass through verbatim"
    );

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1, "exactly one upstream request");
    let recorded = &requests[0];
    assert_eq!(recorded.method, "POST");
    assert_eq!(
        recorded.path, "/v1/responses",
        "the route's own upstream path, not chat's"
    );
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {}", common::UPSTREAM_KEY).as_str()),
        "the upstream key must be substituted for the hydra key"
    );
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert_eq!(recorded.header("accept"), Some("application/json"));
    assert!(
        recorded.header("x-internal-leak").is_none(),
        "arbitrary client headers must not hop to the upstream"
    );
    assert_eq!(
        recorded.body, request_body,
        "request bytes must be forwarded as-is"
    );
}

#[tokio::test]
async fn sdk_style_base_url_with_v1_tail_forwards_to_the_correct_responses_path() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"resp_2"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &format!("{base}/v1"), None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, _) = send(app, responses_request(&key, responses_body("gpt-5", false))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        recorder.requests()[0].path,
        "/v1/responses",
        "a base_url ending in /v1 must not produce a doubled /v1/v1 path"
    );
}

#[tokio::test]
async fn missing_authorization_header_returns_401_openai_error_on_responses() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let _ = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(responses_body("gpt-5", false)))
        .unwrap();
    let (status, _, body) = send(app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "invalid_request_error");
    assert_eq!(error["code"], "invalid_api_key");
}

#[tokio::test]
async fn unknown_bearer_key_returns_401_openai_error_on_responses() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let _ = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app,
        responses_request("hyd-not-a-real-key", responses_body("gpt-5", false)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["code"], "invalid_api_key");
    assert!(error["message"].as_str().unwrap().contains("API key"));
}

#[tokio::test]
async fn disabled_bearer_key_returns_403_key_disabled_on_responses() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    common::disable_key(&db, 1).await;
    let app = common::test_app(db);

    let (status, _, body) =
        send(app, responses_request(&key, responses_body("gpt-5", false))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "invalid_request_error");
    assert_eq!(error["code"], "key_disabled");
}

#[tokio::test]
async fn non_whitelisted_model_returns_404_model_not_found_on_responses() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"never"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["gpt-5".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app,
        responses_request(&key, responses_body("gpt-4o", false)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["code"], "model_not_found");
    assert!(
        error["message"].as_str().unwrap().contains("gpt-5"),
        "the message must list the whitelist: {error}"
    );
    assert_eq!(
        recorder.requests().len(),
        0,
        "a whitelist miss must never reach the upstream"
    );
}

#[tokio::test]
async fn responses_without_model_returns_400_openai_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let body = serde_json::to_vec(&json!({ "input": "hi" })).unwrap();
    let (status, _, response) = send(app, responses_request(&key, body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = error_json(&response)["error"].clone();
    assert_eq!(error["type"], "invalid_request_error");
    assert!(
        error["message"].as_str().unwrap().contains("model"),
        "the 400 must name the missing parameter: {error}"
    );
}

#[tokio::test]
async fn malformed_responses_body_returns_400_openai_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, responses_request(&key, b"not json at all".to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_json(&body)["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn upstream_unreachable_returns_502_openai_error_on_responses() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) =
        send(app, responses_request(&key, responses_body("gpt-5", false))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "api_error");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("upstream"));
}

#[tokio::test]
async fn upstream_500_error_passes_through_verbatim_on_responses() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let upstream_error =
        br#"{"error":{"message":"The server had an error while processing your request.","type":"server_error","code":null}}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "application/json",
        upstream_error.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) =
        send(app, responses_request(&key, responses_body("gpt-5", false))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        &body[..],
        &upstream_error[..],
        "an upstream error body must not be re-shaped"
    );
}

/// The route mounts its own `DefaultBodyLimit`, and this is the test that
/// pins it: a body past axum's 2 MiB default but under the 32 MiB production
/// limit must be forwarded, not answered 413.
#[tokio::test]
async fn responses_body_above_the_axum_default_limit_is_accepted() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"resp_big"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    // 3 MiB of valid JSON: past axum's default, well inside the route's own
    // limit, and shaped like the long-input requests the limit exists for.
    let request_body = serde_json::to_vec(&json!({
        "model": "gpt-5",
        "input": "x".repeat(3 * 1024 * 1024),
    }))
    .unwrap();
    assert!(request_body.len() > 2 * 1024 * 1024);
    assert!(request_body.len() < hydra::http::CHAT_BODY_LIMIT);

    let (status, _, _) = send(app, responses_request(&key, request_body.clone())).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the route must carry the 32 MiB limit, not axum's 2 MiB default"
    );
    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body.len(),
        request_body.len(),
        "the oversized-but-allowed body must arrive whole"
    );
}

#[tokio::test]
async fn oversized_responses_body_returns_413_openai_error() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    // A live upstream so the test proves the oversized body never leaves.
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"never"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    // One 1 MiB chunk more than the production limit, streamed so the
    // request is produced lazily; the router stops reading at the limit.
    use tokio_stream::StreamExt as _;
    let chunk: Bytes = vec![b'x'; 1024 * 1024].into();
    let chunks_needed = hydra::http::CHAT_BODY_LIMIT / chunk.len() + 1;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from_stream(
            tokio_stream::iter(std::iter::repeat_n(chunk, chunks_needed))
                .map(Ok::<_, std::convert::Infallible>),
        ))
        .unwrap();

    let (status, _, body) = send(app, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "invalid_request_error");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("size"),
        "message must say the payload is too large: {error}"
    );
    assert_eq!(
        recorder.requests().len(),
        0,
        "an oversized body must never reach the upstream"
    );
}

#[tokio::test]
async fn responses_stream_passes_sse_events_through_unbuffered() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let chunks: Vec<Bytes> = vec![
        br#"data: {"type":"response.created","response":{"id":"resp_3"}}"#
            .to_vec()
            .into(),
        b"\n\n".to_vec().into(),
        br#"data: {"type":"response.output_text.delta","delta":"He"}"#
            .to_vec()
            .into(),
        b"\n\n".to_vec().into(),
        br#"data: {"type":"response.completed","response":{"id":"resp_3"}}"#
            .to_vec()
            .into(),
        b"\n\n".to_vec().into(),
    ];
    let expected: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let base = common::spawn_upstream(sse_router(recorder.clone(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, headers, body) =
        send(app, responses_request(&key, responses_body("gpt-5", true))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(body, expected, "the SSE event sequence must arrive intact");
    assert_eq!(
        recorder.requests()[0].path,
        "/v1/responses",
        "a streaming Responses request goes to the same path"
    );
}

#[tokio::test]
async fn non_stream_responses_logs_usage_in_the_responses_shape() {
    let db = common::test_pool().await;
    let completion = serde_json::json!({
        "id": "resp_log",
        "object": "response",
        "status": "completed",
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "input_tokens_details": { "cached_tokens": 4 },
            "output_tokens_details": { "reasoning_tokens": 2 }
        }
    })
    .to_string();
    let base = common::spawn_upstream(fixed_response_router(
        Recorder::default(),
        StatusCode::OK,
        "application/json",
        completion,
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let (status, _, _) = send(app, responses_request(&key, responses_body("gpt-5", false))).await;
    assert_eq!(status, StatusCode::OK);

    let row = wait_for_row(&db).await;
    assert_eq!(row.status, Some(200));
    assert_eq!(
        row.prompt_tokens,
        Some(10),
        "the Responses prompt column reads input_tokens"
    );
    assert_eq!(row.completion_tokens, Some(5));
    assert_eq!(row.cached_input_tokens, Some(4));
    assert_eq!(
        row.cached_output_tokens, None,
        "a Responses object has no cached-output dimension"
    );
}

/// A buffered Responses body with no usage reports NULLs, never zeros: the
/// upstream may simply not have reported anything.
#[tokio::test]
async fn non_stream_responses_without_usage_leaves_tokens_null() {
    let db = common::test_pool().await;
    let base = common::spawn_upstream(fixed_response_router(
        Recorder::default(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"resp_nousage","object":"response"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let (status, _, _) = send(app, responses_request(&key, responses_body("gpt-5", false))).await;
    assert_eq!(status, StatusCode::OK);

    let row = wait_for_row(&db).await;
    assert_eq!(row.prompt_tokens, None);
    assert_eq!(row.completion_tokens, None);
    assert_eq!(row.cached_input_tokens, None);
}

/// The streaming capture this task has to pin: the only usage of the stream
/// rides the terminal `response.completed` event, one level down under
/// `data.response.usage`, and the stream runs past the 1 MiB body capture
/// cap. The tokens must still land, with body capture stopped at its cap.
#[tokio::test]
async fn responses_stream_logs_usage_from_the_terminal_event_past_the_body_cap() {
    let db = common::test_pool().await;
    let filler = "x".repeat(256 * 1024);
    let mut chunks: Vec<Bytes> = (0..5)
        .map(|_| {
            format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{filler}\"}}\n\n")
                .into_bytes()
                .into()
        })
        .collect();
    chunks.push(
        br#"data: {"type":"response.completed","response":{"usage":{"input_tokens":21,"output_tokens":42,"input_tokens_details":{"cached_tokens":13}}}}"#
            .to_vec()
            .into(),
    );
    chunks.push(b"\n\n".to_vec().into());
    let streamed: usize = chunks.iter().map(|chunk| chunk.len()).sum();
    assert!(
        streamed > BODY_CAPTURE_LIMIT,
        "the fixture must stream past the body capture cap"
    );

    let base = common::spawn_upstream(sse_router(Recorder::default(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app_logging(db.clone());

    let request_body = responses_body("gpt-5", true);
    let (status, _, body) = send(app, responses_request(&key, request_body.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), streamed, "the client stream must be complete");

    let row = wait_for_row(&db).await;
    assert_eq!(row.status, Some(200));
    assert_eq!(
        row.request_body.as_deref(),
        Some(std::str::from_utf8(&request_body).unwrap()),
        "body logging captures the request bytes on this surface too"
    );
    assert_eq!(
        row.response_body.as_deref().map(str::len),
        Some(BODY_CAPTURE_LIMIT),
        "body capture still stops at the cap"
    );
    assert_eq!(
        row.prompt_tokens,
        Some(21),
        "the terminal event carries the only usage of the stream"
    );
    assert_eq!(row.completion_tokens, Some(42));
    assert_eq!(row.cached_input_tokens, Some(13));
    assert_eq!(row.cached_output_tokens, None);
}
