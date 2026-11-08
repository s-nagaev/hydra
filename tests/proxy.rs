//! End-to-end tests for the proxy routes against a real in-process mock
//! upstream. Every behavior named in the module states what it verifies.

mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sqlx::Row;
use tower::ServiceExt;

use hydra::http::router;

use common::{fixed_response_router, sse_router, within, Recorder};

/// A valid chat completion request body for the given model.
fn chat_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": model,
        "stream": stream,
        "messages": [{ "role": "user", "content": "hi" }],
    }))
    .unwrap()
}

/// Sends a request through the real router and returns status, headers and
/// the full body.
async fn send(
    app: hydra::http::AppState,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = within(router(app).oneshot(request)).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

fn chat_request(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// Sends a chat request with one extra client header that must never reach
/// the upstream.
fn chat_request_with_leaky_header(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("x-internal-leak", "must-not-hop")
        .body(Body::from(body))
        .unwrap()
}

fn models_request(key: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .body(Body::empty())
        .unwrap()
}

fn error_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("proxy errors must be OpenAI-shaped JSON")
}

async fn unknown_key_returns_401_openai_error_impl(uri: &str, method: &str) {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let _ = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer hyd-not-a-real-key")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "invalid_request_error");
    assert_eq!(error["code"], "invalid_api_key");
    assert!(error["message"].as_str().unwrap().contains("API key"));
}

#[tokio::test]
async fn missing_authorization_header_returns_401_openai_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let _ = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .body(Body::from(chat_body("gpt-4o", false)))
        .unwrap();
    let (status, _, body) = send(app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_json(&body)["error"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn unknown_bearer_key_returns_401_openai_error_on_chat() {
    unknown_key_returns_401_openai_error_impl("/v1/chat/completions", "POST").await;
}

#[tokio::test]
async fn unknown_bearer_key_returns_401_openai_error_on_models() {
    unknown_key_returns_401_openai_error_impl("/v1/models", "GET").await;
}

#[tokio::test]
async fn disabled_bearer_key_returns_403_key_disabled() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    common::disable_key(&db, 1).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "invalid_request_error");
    assert_eq!(error["code"], "key_disabled");
}

#[tokio::test]
async fn chat_without_model_returns_400_openai_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let body = br#"{"messages":[]}"#.to_vec();
    let (status, _, body) = send(app, chat_request(&key, body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_json(&body)["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn malformed_chat_body_returns_400_openai_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, chat_request(&key, b"not json".to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_json(&body)["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn non_whitelisted_model_returns_404_model_not_found() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(
        &db,
        &base,
        Some(vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]),
    )
    .await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-3.5-turbo", false))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "invalid_request_error");
    assert_eq!(error["code"], "model_not_found");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("gpt-3.5-turbo"),
        "message must name the model: {error}"
    );
    assert!(
        message.contains("Allowed models for this channel: gpt-4o, gpt-4o-mini"),
        "message must list the channel's allowed models: {error}"
    );
}

#[tokio::test]
async fn whitelisted_model_passes_and_forwards_to_upstream() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let completion = br#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4o"}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        completion.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["gpt-4o".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request_body = chat_body("gpt-4o", false);
    let request = chat_request_with_leaky_header(&key, request_body.clone());
    let (status, headers, body) = send(app, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        &body[..],
        &completion[..],
        "body must pass through verbatim"
    );

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1, "exactly one upstream request");
    let recorded = &requests[0];
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/chat/completions");
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
async fn null_whitelist_allows_any_model() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let completion = br#"{"id":"chatcmpl-2","object":"chat.completion"}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        completion.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app,
        chat_request(&key, chat_body("totally-unknown-model", false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], &completion[..]);
    assert_eq!(recorder.requests().len(), 1);
}

#[tokio::test]
async fn empty_whitelist_allows_any_model() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let completion = br#"{"id":"chatcmpl-3","object":"chat.completion"}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        completion.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec![])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, _) = send(app, chat_request(&key, chat_body("any-model", false))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recorder.requests().len(), 1);
}

#[tokio::test]
async fn trailing_slash_base_url_forwards_to_the_correct_path() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"ok":true}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &format!("{base}/"), None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, _) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        recorder.requests()[0].path,
        "/v1/chat/completions",
        "a trailing slash on base_url must not produce a doubled path"
    );
}

#[tokio::test]
async fn sdk_style_base_url_with_v1_tail_forwards_to_the_correct_path() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"ok":true}"#.to_vec(),
    ))
    .await;
    // OpenAI-SDK style base URL: the stored URL already carries the `/v1`
    // that the forward path adds.
    let channel_id = common::seed_channel(&db, &format!("{base}/v1"), None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, _) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        recorder.requests()[0].path,
        "/v1/chat/completions",
        "a base_url ending in /v1 must not produce a doubled /v1/v1 path"
    );
}

#[tokio::test]
async fn chat_stream_passes_sse_events_through_unbuffered() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let chunks: Vec<axum::body::Bytes> = vec![
        br#"data: {"id":"x","choices":[{"delta":{"content":"He"}}]}"#
            .to_vec()
            .into(),
        br#"

"#
        .to_vec()
        .into(),
        br#"data: {"id":"x","choices":[{"delta":{"content":"llo"}}]}"#
            .to_vec()
            .into(),
        br#"

"#
        .to_vec()
        .into(),
        br#"data: [DONE]

"#
        .to_vec()
        .into(),
    ];
    let expected: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let base = common::spawn_upstream(sse_router(recorder.clone(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, headers, body) = send(app, chat_request(&key, chat_body("gpt-4o", true))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(body, expected, "the SSE event sequence must arrive intact");
    assert_eq!(
        recorder.requests()[0].header("authorization"),
        Some(format!("Bearer {}", common::UPSTREAM_KEY).as_str())
    );
}

#[tokio::test]
async fn upstream_500_error_passes_through_verbatim() {
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

    let (status, headers, body) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        &body[..],
        &upstream_error[..],
        "upstream error bodies must pass through verbatim"
    );
}

/// A whitelisted model that the upstream itself rejects with 404 must come
/// back as the upstream's own body verbatim, not Hydra's model_not_found:
/// the whitelist admitted the model, so the miss is the upstream's.
#[tokio::test]
async fn upstream_404_error_passes_through_verbatim() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let upstream_error = br#"{"error":{"message":"The model 'gpt-4o' does not exist","type":"invalid_request_error","param":null,"code":"model_not_found"}}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::NOT_FOUND,
        "application/json",
        upstream_error.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["gpt-4o".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        &body[..],
        &upstream_error[..],
        "an upstream 404 must pass through verbatim, not become Hydra's model_not_found"
    );
}

#[tokio::test]
async fn upstream_unreachable_returns_502_openai_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "api_error");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("upstream"),
        "message must say the upstream is unavailable: {error}"
    );
}

/// Route-level miss: a trailing slash never matches the route, so axum
/// answers a bare 404 with an empty body and no handler runs — nothing is
/// logged. This pins the third 404 class (distinct from Hydra's
/// model_not_found and from upstream passthrough).
#[tokio::test]
async fn trailing_slash_chat_uri_returns_bare_404_without_log_row() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["gpt-4o".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions/")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(chat_body("gpt-4o", false)))
        .unwrap();
    let (status, _, body) = send(app, request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.is_empty(),
        "the router-level 404 has no handler-produced body"
    );
    let rows: i64 = within(sqlx::query("SELECT COUNT(*) AS n FROM request_logs").fetch_one(&db))
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(rows, 0, "a route miss must not leave a log row");
}

/// Wrong method: GET on the chat route is a 405 from the method router,
/// also without a log row.
#[tokio::test]
async fn get_on_chat_uri_returns_405_without_log_row() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["gpt-4o".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let request = Request::builder()
        .method("GET")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(app, request).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(body.is_empty());
    let rows: i64 = within(sqlx::query("SELECT COUNT(*) AS n FROM request_logs").fetch_one(&db))
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(rows, 0, "a method miss must not leave a log row");
}

#[tokio::test]
async fn models_with_whitelist_returns_local_sorted_openai_list() {
    let db = common::test_pool().await;
    // The upstream must never be called for the whitelist case; point the
    // channel at an unreachable port so any proxying fails the request.
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(
        &db,
        &base,
        Some(vec!["zeta-model".to_string(), "alpha-model".to_string()]),
    )
    .await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, headers, body) = send(app, models_request(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["object"], "list");
    let data = value["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], "alpha-model", "models must be sorted");
    assert_eq!(data[1]["id"], "zeta-model");
    for entry in data {
        assert_eq!(entry["object"], "model");
        assert_eq!(entry["created"], 0);
        assert_eq!(entry["owned_by"], "hydra");
    }
}

#[tokio::test]
async fn models_with_null_whitelist_proxies_upstream_list() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let upstream_list = br#"{"object":"list","data":[{"id":"gpt-4o","object":"model","created":100,"owned_by":"openai"}]}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        upstream_list.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, headers, body) = send(app, models_request(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        &body[..],
        &upstream_list[..],
        "the upstream list must pass through verbatim"
    );

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/v1/models");
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {}", common::UPSTREAM_KEY).as_str())
    );
}

#[tokio::test]
async fn models_with_empty_whitelist_proxies_upstream_list() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let upstream_list = br#"{"object":"list","data":[]}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        upstream_list.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec![])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, models_request(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], &upstream_list[..]);
    assert_eq!(recorder.requests().len(), 1);
}

#[tokio::test]
async fn models_upstream_unreachable_returns_502_openai_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, models_request(&key)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(error_json(&body)["error"]["type"], "api_error");
}

#[tokio::test]
async fn oversized_chat_body_returns_413_openai_error() {
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
    let chunk: axum::body::Bytes = vec![b'x'; 1024 * 1024].into();
    let chunks_needed = hydra::http::CHAT_BODY_LIMIT / chunk.len() + 1;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
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

/// Seeds a channel pointing at a live recorder upstream plus a bound key,
/// with the channel's enabled flag flipped to `enabled`; returns the key.
/// The live upstream proves a disabled channel receives no traffic at all.
async fn seed_with_channel_flag(
    db: &sqlx::SqlitePool,
    recorder: &Recorder,
    completion: &'static [u8],
    channel_enabled: bool,
) -> (i64, String) {
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        completion.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(db, &base, None).await;
    let key = common::seed_key(db, channel_id).await;
    common::set_channel_enabled(db, channel_id, channel_enabled).await;
    (channel_id, key)
}

#[tokio::test]
async fn disabled_channel_returns_503_openai_error_on_chat() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let (_, key) =
        seed_with_channel_flag(&db, &recorder, br#"{"id":"chatcmpl-disabled"}"#, false).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "api_error");
    assert_eq!(error["code"], "channel_disabled");
    assert!(
        error["message"].as_str().unwrap().contains("disabled"),
        "message must say the channel is disabled: {error}"
    );
    assert_eq!(
        recorder.requests().len(),
        0,
        "a disabled channel must receive no traffic"
    );
}

#[tokio::test]
async fn disabled_channel_returns_503_openai_error_on_models() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let (_, key) =
        seed_with_channel_flag(&db, &recorder, br#"{"id":"chatcmpl-disabled"}"#, false).await;
    let app = common::test_app(db);

    let (status, _, body) = send(app, models_request(&key)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let error = error_json(&body)["error"].clone();
    assert_eq!(error["type"], "api_error");
    assert_eq!(error["code"], "channel_disabled");
    assert_eq!(recorder.requests().len(), 0);
}

#[tokio::test]
async fn re_enabling_the_channel_restores_proxy_access_for_bound_keys() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let completion = br#"{"id":"chatcmpl-back"}"#;
    let (channel_id, key) = seed_with_channel_flag(&db, &recorder, completion, false).await;
    let app = common::test_app(db.clone());

    let (status, _, _) = send(app.clone(), chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    common::set_channel_enabled(&db, channel_id, true).await;
    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o", false))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], &completion[..]);
    assert_eq!(
        recorder.requests().len(),
        1,
        "only the post-re-enable call reaches the upstream"
    );
}
