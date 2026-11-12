//! End-to-end tests for the `POST /v1/messages` proxy route against a real
//! in-process mock upstream. The route runs the same thin-passthrough
//! pipeline as chat and Responses, so what is verified here is what differs
//! or could silently differ: the two credential headers the Messages surface
//! accepts, the outbound header policy (the upstream key as `x-api-key`
//! instead of a bearer token, the required `anthropic-version` and the
//! optional `anthropic-beta`), the route-specific 32 MiB body limit, the
//! shared rejections rendered in the Anthropic error envelope that this
//! route's clients parse, with the OpenAI surfaces checked alongside for the
//! same rejection, and the Anthropic usage shape the response is logged in,
//! additively normalized, on both the streaming and the buffered path.

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

/// The Messages API version header value this suite's clients send.
const VERSION: &str = "2023-06-01";

/// A valid Messages request body for the given model: the surface's own
/// `messages`/`max_tokens` fields.
fn messages_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "max_tokens": 64,
        "stream": stream,
        "messages": [{ "role": "user", "content": "hi" }],
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

/// A Messages request the way the Anthropic SDKs send one: the key in
/// `x-api-key`, the version header present.
fn messages_request(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", VERSION)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// The same request with a bearer token instead of the API-key header, for
/// OpenAI-style clients pointed at this route.
fn messages_bearer_request(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-version", VERSION)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// Polls the log until a row matching `predicate` is visible; the inserts run
/// detached.
async fn wait_for_row(predicate: impl Fn(&LogRow) -> bool, db: &SqlitePool) -> LogRow {
    let deadline = Instant::now() + common::TEST_TIMEOUT;
    loop {
        let rows = within(
            sqlx::query(
                "SELECT status, model, error, prompt_tokens, completion_tokens, \
                 cached_input_tokens, cached_output_tokens, request_body, response_body \
                 FROM request_logs ORDER BY id",
            )
            .fetch_all(db),
        )
        .await
        .unwrap();
        let rows: Vec<LogRow> = rows
            .iter()
            .map(|row| LogRow {
                status: row.try_get("status").unwrap(),
                model: row.try_get("model").unwrap(),
                error: row.try_get("error").unwrap(),
                prompt_tokens: row.try_get("prompt_tokens").unwrap(),
                completion_tokens: row.try_get("completion_tokens").unwrap(),
                cached_input_tokens: row.try_get("cached_input_tokens").unwrap(),
                cached_output_tokens: row.try_get("cached_output_tokens").unwrap(),
                request_body: row.try_get("request_body").unwrap(),
                response_body: row.try_get("response_body").unwrap(),
            })
            .collect();
        if let Some(row) = rows.into_iter().find(|row| predicate(row)) {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "no matching request_logs row landed for the messages call"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The `request_logs` columns these tests read back.
struct LogRow {
    status: Option<i64>,
    model: Option<String>,
    error: Option<String>,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cached_output_tokens: Option<i64>,
    request_body: Option<String>,
    response_body: Option<String>,
}

fn error_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("proxy errors must be JSON")
}

/// Asserts the Anthropic envelope an Anthropic SDK pointed at this route
/// parses: `type: "error"` at the top level and an `error` object carrying
/// exactly the taxonomy `type` and a non-empty `message`. Returns that inner
/// object for message assertions.
fn anthropic_error(body: &[u8], expected_type: &str) -> Value {
    let value = error_json(body);
    assert_eq!(
        value["type"], "error",
        "a rejection of this route must be Anthropic-shaped: {value}"
    );
    let error = value["error"].clone();
    assert_eq!(error["type"], expected_type, "body: {value}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "every rejection carries a message: {value}"
    );
    assert_eq!(
        error.as_object().unwrap().len(),
        2,
        "the Anthropic error object has no `code` field: {value}"
    );
    error
}

/// The same rejection as the OpenAI surfaces render it: the envelope stays
/// `{"error":{"message","type","code"}}`, `code` included, so the change
/// that gives this route its own envelope cannot leak onto chat.
fn openai_error(body: &[u8], expected_type: &str) -> Value {
    let value = error_json(body);
    assert!(
        value.get("type").is_none(),
        "an OpenAI surface must not grow the Anthropic envelope: {value}"
    );
    let error = value["error"].clone();
    assert_eq!(error["type"], expected_type, "body: {value}");
    assert!(
        error.get("code").is_some(),
        "the OpenAI envelope keeps its `code`, `null` included: {value}"
    );
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "every rejection carries a message: {value}"
    );
    error
}

/// A chat body for the given model, for the paired assertion that the
/// OpenAI surfaces keep their envelope for the same rejection.
fn chat_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
    }))
    .unwrap()
}

/// The same body sent to the chat route with a bearer credential.
fn chat_request(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// A body one chunk past the production limit, streamed so the router stops
/// reading at the limit instead of the test materializing it.
fn oversized_stream_body(uri: &str) -> Request<Body> {
    use tokio_stream::StreamExt as _;
    let chunk: Bytes = vec![b'x'; 1024 * 1024].into();
    let chunks_needed = hydra::http::CHAT_BODY_LIMIT / chunk.len() + 1;
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from_stream(
            tokio_stream::iter(std::iter::repeat_n(chunk, chunks_needed))
                .map(Ok::<_, std::convert::Infallible>),
        ))
        .unwrap()
}

/// A key with a monthly limit, returned with its row id. The limit verdict
/// is what the gate reads, so this seeds the key the gate needs and nothing
/// else: what these tests pin is the envelope of a rejected request, not the
/// gate, which has its own suite.
async fn seed_limited_key(db: &SqlitePool, channel_id: i64) -> (i64, String) {
    let generated = hydra::keys::generate();
    let key = within(hydra::db::keys::create(
        &hydra::db::Db::from(db.clone()),
        &hydra::db::keys::NewKey {
            key_hash: generated.key_hash,
            display_prefix: generated.display_prefix,
            name: None,
            channel_id,
            monthly_limit_micro_usd: Some(1),
        },
    ))
    .await
    .unwrap();
    (key.id, generated.full)
}

/// A pool whose every query fails: connected, then closed, so any acquire
/// errors out. Swapped into an app's state to reach the storage-failure
/// arms.
async fn dead_pool() -> SqlitePool {
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

/// An app whose key lookup cannot succeed, for the 500 arms.
fn app_with_dead_db(app: &AppState, db: SqlitePool) -> AppState {
    let mut offline = app.clone();
    offline.db = hydra::db::Db::from(db);
    offline
}

#[tokio::test]
async fn messages_forwards_verbatim_to_the_upstream_messages_path_with_the_upstream_api_key() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let response_body = br#"{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":3,"output_tokens":1}}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        response_body.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["claude-3".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request_body = messages_body("claude-3", false);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &key)
        .header("anthropic-version", VERSION)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("x-internal-leak", "must-not-hop")
        .body(Body::from(request_body.clone()))
        .unwrap();
    let (status, headers, body) = send(app, request).await;

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
        recorded.path, "/v1/messages",
        "the route's own upstream path, not chat's"
    );
    assert_eq!(
        recorded.header("x-api-key"),
        Some(common::UPSTREAM_KEY),
        "the upstream key must be substituted in the Messages credential header"
    );
    assert!(
        recorded.header("authorization").is_none(),
        "the Messages surface must not send a bearer header upstream: {:?}",
        recorded.headers
    );
    assert_eq!(
        recorded.header("anthropic-version"),
        Some(VERSION),
        "the client's version header is forwarded"
    );
    assert!(
        recorded.header("anthropic-beta").is_none(),
        "no beta header is invented for a client that sent none"
    );
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert_eq!(recorded.header("accept"), Some("application/json"));
    assert!(
        recorded.header("x-internal-leak").is_none(),
        "arbitrary client headers must not hop to the upstream"
    );
    assert!(
        !recorded
            .headers
            .iter()
            .any(|(_, value)| value.contains(&key)),
        "the client key must never leak upstream: {:?}",
        recorded.headers
    );
    assert_eq!(
        recorded.body, request_body,
        "request bytes must be forwarded as-is"
    );
}

#[tokio::test]
async fn messages_accepts_a_bearer_token_as_the_client_credential() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"msg_2"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, _) = send(
        app,
        messages_bearer_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a bearer token must authenticate this route too"
    );

    let recorded = recorder.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].header("x-api-key"),
        Some(common::UPSTREAM_KEY),
        "whichever header the client used, the upstream sees x-api-key"
    );
    assert!(recorded[0].header("authorization").is_none());
}

#[tokio::test]
async fn messages_prefers_the_api_key_header_over_a_bearer_header() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"msg_3"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    // The bearer header is a key that resolves to nothing: if it were read
    // instead of the API-key header, this request would be a 401.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &key)
        .header("authorization", "Bearer hyd-not-a-real-key")
        .header("anthropic-version", VERSION)
        .header("content-type", "application/json")
        .body(Body::from(messages_body("claude-3", false)))
        .unwrap();
    let (status, _, _) = send(app, request).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the Anthropic-native header is the one that authenticates"
    );
    assert_eq!(recorder.requests().len(), 1);
}

#[tokio::test]
async fn messages_without_any_credential_returns_401_anthropic_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let _ = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("anthropic-version", VERSION)
        .header("content-type", "application/json")
        .body(Body::from(messages_body("claude-3", false)))
        .unwrap();
    let (status, _, body) = send(app.clone(), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = anthropic_error(&body, "authentication_error");
    assert_eq!(error["message"], "Invalid API key provided");

    // The same rejection on chat, the surface that has always answered in
    // the OpenAI envelope.
    let chat_without_credentials = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(chat_body("gpt-4o")))
        .unwrap();
    let (status, _, body) = send(app, chat_without_credentials).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = openai_error(&body, "invalid_request_error");
    assert_eq!(error["code"], "invalid_api_key");
}

#[tokio::test]
async fn messages_with_an_unknown_api_key_returns_401_anthropic_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let _ = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app.clone(),
        messages_request("hyd-not-a-real-key", messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = anthropic_error(&body, "authentication_error");
    assert!(error["message"].as_str().unwrap().contains("API key"));

    let (status, _, body) =
        send(app, chat_request("hyd-not-a-real-key", chat_body("gpt-4o"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = openai_error(&body, "invalid_request_error");
    assert_eq!(error["code"], "invalid_api_key");
}

#[tokio::test]
async fn messages_with_a_disabled_key_returns_403_anthropic_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    common::disable_key(&db, 1).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app.clone(),
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let error = anthropic_error(&body, "permission_error");
    assert_eq!(error["message"], "This API key has been disabled");

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let error = openai_error(&body, "invalid_request_error");
    assert_eq!(error["code"], "key_disabled");
}

#[tokio::test]
async fn messages_with_a_model_outside_the_whitelist_returns_404_anthropic_error() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"never"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, Some(vec!["claude-3".to_string()])).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app.clone(),
        messages_request(&key, messages_body("gpt-4o", false)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = anthropic_error(&body, "invalid_request_error");
    assert!(
        error["message"].as_str().unwrap().contains("claude-3"),
        "the message must list the whitelist: {error}"
    );

    // Chat rejects the very same model against the very same whitelist, in
    // the OpenAI envelope it has always used.
    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = openai_error(&body, "invalid_request_error");
    assert_eq!(error["code"], "model_not_found");

    assert_eq!(
        recorder.requests().len(),
        0,
        "a whitelist miss must never reach the upstream"
    );
}

/// The Messages API requires the version header, so Hydra answers the
/// missing header at its own boundary instead of spending a round-trip to
/// relay the upstream's 400.
#[tokio::test]
async fn messages_without_the_version_header_returns_400_anthropic_error() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"never"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &key)
        .header("content-type", "application/json")
        .body(Body::from(messages_body("claude-3", false)))
        .unwrap();
    let (status, _, body) = send(app.clone(), request).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = anthropic_error(&body, "invalid_request_error");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("anthropic-version"),
        "the 400 must name the header: {error}"
    );
    assert_eq!(
        recorder.requests().len(),
        0,
        "a request Hydra rejects must not reach the upstream"
    );

    // The chat 400 for the same class of request, a body Hydra cannot
    // dispatch, keeps the OpenAI envelope.
    let (status, _, body) = send(app, chat_request(&key, br#"{"messages":[]}"#.to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    openai_error(&body, "invalid_request_error");

    // The rejection is logged like every other rejection on this route: a
    // client-level rejection carries no status, and the row keeps the model
    // the caller asked for.
    let row = wait_for_row(
        |row| {
            row.error
                .as_deref()
                .is_some_and(|error| error.contains("anthropic-version"))
        },
        &db,
    )
    .await;
    assert_eq!(row.status, None);
    assert_eq!(row.model.as_deref(), Some("claude-3"));
    assert_eq!(row.prompt_tokens, None);
}

#[tokio::test]
async fn messages_forwards_the_beta_header_only_when_the_client_sent_it() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"msg_4"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let beta = "prompt-caching-2024-07-31";
    let with_beta = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &key)
        .header("anthropic-version", VERSION)
        .header("anthropic-beta", beta)
        .header("content-type", "application/json")
        .body(Body::from(messages_body("claude-3", false)))
        .unwrap();
    let (status, _, _) = send(app.clone(), with_beta).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = send(
        app,
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let requests = recorder.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("anthropic-beta"), Some(beta));
    assert_eq!(requests[0].header("anthropic-version"), Some(VERSION));
    assert!(
        requests[1].header("anthropic-beta").is_none(),
        "the second client sent no beta header and must not get one"
    );
}

#[tokio::test]
async fn sdk_style_base_url_with_v1_tail_forwards_to_the_single_messages_path() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"msg_5"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &format!("{base}/v1"), None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, _) = send(
        app,
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        recorder.requests()[0].path,
        "/v1/messages",
        "a base_url ending in /v1 must not produce a doubled /v1/v1 path"
    );
}

/// The route mounts its own `DefaultBodyLimit`, and this is the test that
/// pins it: a body past axum's 2 MiB default but under the 32 MiB production
/// limit must be forwarded, not answered 413.
#[tokio::test]
async fn messages_body_above_the_axum_default_limit_is_accepted() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"msg_big"}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let request_body = serde_json::to_vec(&json!({
        "model": "claude-3",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "x".repeat(3 * 1024 * 1024) }],
    }))
    .unwrap();
    assert!(request_body.len() > 2 * 1024 * 1024);
    assert!(request_body.len() < hydra::http::CHAT_BODY_LIMIT);

    let (status, _, _) = send(app, messages_request(&key, request_body.clone())).await;
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
async fn oversized_messages_body_returns_413_anthropic_error() {
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

    // The body-limit rejection happens in the extractor, before any handler
    // code runs, so it is the one rejection that reaches the client without
    // an audit row: the envelope it is rendered in still belongs to the
    // surface the client called.
    let mut request = oversized_stream_body("/v1/messages");
    request
        .headers_mut()
        .insert("x-api-key", key.parse().unwrap());
    request
        .headers_mut()
        .insert("anthropic-version", VERSION.parse().unwrap());
    let (status, _, body) = send(app.clone(), request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let error = anthropic_error(&body, "invalid_request_error");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("size"),
        "message must say the payload is too large: {error}"
    );

    // Chat answers the same oversized body in the OpenAI envelope, which is
    // what its clients parse.
    let mut request = oversized_stream_body("/v1/chat/completions");
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    let (status, _, body) = send(app, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let error = openai_error(&body, "invalid_request_error");
    assert!(
        error["code"].is_null(),
        "OpenAI sends no code here: {error}"
    );

    assert_eq!(
        recorder.requests().len(),
        0,
        "an oversized body must never reach the upstream"
    );
}

#[tokio::test]
async fn messages_upstream_unreachable_returns_502_anthropic_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app.clone(),
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let error = anthropic_error(&body, "overloaded_error");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("upstream"));

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o"))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let error = openai_error(&body, "api_error");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("upstream"));
}

/// The monthly-limit gate rejects after the key resolved, and the surface
/// the request was sent to decides the envelope: `rate_limit_error` under
/// both, inside each shape.
#[tokio::test]
async fn messages_over_the_monthly_limit_returns_429_anthropic_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let (key_id, key) = seed_limited_key(&db, channel_id).await;
    let app = common::test_app(db);
    app.limit_flags
        .set(key_id, hydra::month::Month::current().bucket(), true);

    let (status, _, body) = send(
        app.clone(),
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let error = anthropic_error(&body, "rate_limit_error");
    assert_eq!(error["message"], "Monthly limit reached");

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o"))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let error = openai_error(&body, "rate_limit_error");
    assert_eq!(error["message"], "Monthly limit reached");
}

/// A storage failure while resolving the key is answered 500 on every
/// surface, and the envelope is the only difference.
#[tokio::test]
async fn messages_key_lookup_failure_returns_500_anthropic_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);
    let offline = app_with_dead_db(&app, dead_pool().await);

    let (status, _, body) = send(
        offline.clone(),
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let error = anthropic_error(&body, "api_error");
    assert_eq!(error["message"], "internal server error");

    let (status, _, body) = send(offline, chat_request(&key, chat_body("gpt-4o"))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let error = openai_error(&body, "server_error");
    assert_eq!(error["message"], "internal server error");
}

/// A channel disabled after its keys were issued rejects on both surfaces
/// with 503; the Anthropic envelope calls that an overload, the OpenAI one
/// an `api_error`.
#[tokio::test]
async fn messages_with_a_disabled_channel_returns_503_anthropic_error() {
    let db = common::test_pool().await;
    let base = common::unreachable_base_url().await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    common::set_channel_enabled(&db, channel_id, false).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app.clone(),
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let error = anthropic_error(&body, "overloaded_error");
    assert_eq!(
        error["message"],
        "The channel bound to this API key is currently disabled"
    );

    let (status, _, body) = send(app, chat_request(&key, chat_body("gpt-4o"))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let error = openai_error(&body, "api_error");
    assert_eq!(error["code"], "channel_disabled");
}

/// An Anthropic error body is an Anthropic error body: Hydra passes it
/// through unchanged rather than re-shaping it (the per-route envelope is a
/// separate change).
#[tokio::test]
async fn upstream_anthropic_error_passes_through_verbatim_on_messages() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let upstream_error =
        br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let base = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::SERVICE_UNAVAILABLE,
        "application/json",
        upstream_error.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, _, body) = send(
        app,
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        &body[..],
        &upstream_error[..],
        "an upstream error body must not be re-shaped"
    );
}

#[tokio::test]
async fn messages_stream_passes_sse_events_through_unbuffered() {
    let db = common::test_pool().await;
    let recorder = Recorder::default();
    let chunks: Vec<Bytes> = vec![
        b"event: message_start\n".to_vec().into(),
        br#"data: {"type":"message_start","message":{"id":"msg_6","usage":{"input_tokens":5,"output_tokens":1}}}"#
            .to_vec()
            .into(),
        b"\n\n".to_vec().into(),
        b"event: content_block_delta\n".to_vec().into(),
        br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"He"}}"#
            .to_vec()
            .into(),
        b"\n\n".to_vec().into(),
        b"event: message_stop\n".to_vec().into(),
        br#"data: {"type":"message_stop"}"#.to_vec().into(),
        b"\n\n".to_vec().into(),
    ];
    let expected: Vec<u8> = chunks
        .iter()
        .flat_map(|chunk| chunk.iter().copied())
        .collect();
    let base = common::spawn_upstream(sse_router(recorder.clone(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db);

    let (status, headers, body) =
        send(app, messages_request(&key, messages_body("claude-3", true))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(body, expected, "the SSE event sequence must arrive intact");
    assert_eq!(
        recorder.requests()[0].path,
        "/v1/messages",
        "a streaming Messages request goes to the same path"
    );
}

/// The additive prompt formula, pinned end to end: the Messages API reports
/// `input_tokens` without the cached parts, so the logged prompt is
/// input + cache reads + cache writes. The cache-write count has no column
/// of its own yet; it is inside the prompt sum, which is what the price
/// math reads.
#[tokio::test]
async fn non_stream_messages_logs_the_additive_prompt_formula() {
    let db = common::test_pool().await;
    let completion = json!({
        "id": "msg_log",
        "type": "message",
        "role": "assistant",
        "model": "claude-3",
        "content": [{ "type": "text", "text": "hi" }],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 5,
            "output_tokens": 7
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

    let (status, _, _) = send(
        app,
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = wait_for_row(|row| row.status == Some(200), &db).await;
    assert_eq!(
        row.prompt_tokens,
        Some(125),
        "the Anthropic prompt is input + cache reads + cache writes"
    );
    assert_eq!(row.completion_tokens, Some(7));
    assert_eq!(row.cached_input_tokens, Some(20));
    assert_eq!(
        row.cached_output_tokens, None,
        "a Messages object has no cached-output dimension"
    );
}

/// A buffered Messages body with no usage reports NULLs, never zeros: the
/// upstream may simply not have reported anything.
#[tokio::test]
async fn non_stream_messages_without_usage_leaves_tokens_null() {
    let db = common::test_pool().await;
    let base = common::spawn_upstream(fixed_response_router(
        Recorder::default(),
        StatusCode::OK,
        "application/json",
        br#"{"id":"msg_nousage","type":"message","content":[]}"#.to_vec(),
    ))
    .await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app(db.clone());

    let (status, _, _) = send(
        app,
        messages_request(&key, messages_body("claude-3", false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = wait_for_row(|row| row.status == Some(200), &db).await;
    assert_eq!(row.prompt_tokens, None);
    assert_eq!(row.completion_tokens, None);
    assert_eq!(row.cached_input_tokens, None);
}

/// The streaming capture this route depends on: the input counters ride the
/// stream's FIRST event (`message_start`) while the authoritative
/// `output_tokens` closes it in `message_delta`, so a tail window would lose
/// the inputs on any stream longer than the window. This stream runs past
/// both the 64 KiB bound the plan calls out and the 1 MiB body capture cap,
/// and the tokens must still land while body capture stops at its cap.
#[tokio::test]
async fn messages_stream_logs_usage_from_message_start_and_message_delta_past_the_body_cap() {
    let db = common::test_pool().await;
    let filler = "x".repeat(256 * 1024);
    let mut chunks: Vec<Bytes> = vec![
        b"event: message_start\n".to_vec().into(),
        br#"data: {"type":"message_start","message":{"id":"msg_long","usage":{"input_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":1}}}"#
            .to_vec()
            .into(),
        b"\n\n".to_vec().into(),
    ];
    for _ in 0..5 {
        chunks.push(
            format!(
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{filler}\"}}}}\n\n"
            )
            .into_bytes()
            .into(),
        );
    }
    chunks.push(b"event: message_delta\n".to_vec().into());
    chunks.push(
        br#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":80}}"#
            .to_vec()
            .into(),
    );
    chunks.push(b"\n\n".to_vec().into());
    let streamed: usize = chunks.iter().map(|chunk| chunk.len()).sum();
    assert!(
        streamed > BODY_CAPTURE_LIMIT,
        "the fixture must stream past the body capture cap"
    );
    assert!(
        streamed > 64 * 1024,
        "the fixture must stream past the tail window the plan calls out"
    );

    let base = common::spawn_upstream(sse_router(Recorder::default(), chunks)).await;
    let channel_id = common::seed_channel(&db, &base, None).await;
    let key = common::seed_key(&db, channel_id).await;
    let app = common::test_app_logging(db.clone());

    let request_body = messages_body("claude-3", true);
    let (status, _, body) = send(app, messages_request(&key, request_body.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), streamed, "the client stream must be complete");

    let row = wait_for_row(|row| row.status == Some(200), &db).await;
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
        Some(125),
        "the inputs ride the opening event and must survive a stream this long"
    );
    assert_eq!(
        row.completion_tokens,
        Some(80),
        "the closing message_delta carries the authoritative output count"
    );
    assert_eq!(row.cached_input_tokens, Some(20));
    assert_eq!(row.cached_output_tokens, None);
}
