//! Integration tests for `GET /admin/logs`: pagination, filters, time
//! windowing, validation, and body passthrough. Rows are seeded through
//! direct sqlx inserts against a real channel/key pair to satisfy the
//! foreign keys.

mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt;

use common::{test_app, test_pool, within};

fn auth_header() -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    format!("Basic {}", STANDARD.encode("admin:secret"))
}

async fn send_get(uri: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let db = test_pool().await;
    send_get_on(&db, uri, auth).await
}

async fn send_get_on(db: &SqlitePool, uri: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let app = test_app(db.clone());
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(header) = auth {
        request = request.header("Authorization", header);
    }
    let response = within(hydra::http::router(app).oneshot(request.body(Body::empty()).unwrap()))
        .await
        .unwrap();
    let status = response.status();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, parsed)
}

/// Seeds one channel and one client key; returns (channel_id, key_id).
async fn seed_channel_and_key(db: &SqlitePool) -> (i64, i64) {
    let channel_id = common::seed_channel(db, "http://127.0.0.1:1", None).await;
    common::seed_key(db, channel_id).await;
    let key_id: i64 = within(
        sqlx::query_scalar("SELECT id FROM api_keys WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(db),
    )
    .await
    .unwrap();
    (channel_id, key_id)
}

#[allow(clippy::too_many_arguments)]
async fn insert_log(
    db: &SqlitePool,
    key_id: i64,
    channel_id: i64,
    model: Option<&str>,
    request_body: Option<&str>,
    response_body: Option<&str>,
    error: Option<&str>,
    created_at: &str,
) -> i64 {
    insert_status_log(
        db,
        key_id,
        channel_id,
        model,
        Some(200),
        Some(10),
        Some(5),
        request_body,
        response_body,
        error,
        created_at,
    )
    .await
}

/// Inserts one row with full control over status and tokens; the shared
/// shape the other helpers build on.
#[allow(clippy::too_many_arguments)]
async fn insert_status_log(
    db: &SqlitePool,
    key_id: i64,
    channel_id: i64,
    model: Option<&str>,
    status: Option<i64>,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    request_body: Option<&str>,
    response_body: Option<&str>,
    error: Option<&str>,
    created_at: &str,
) -> i64 {
    within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              duration_ms, request_body, response_body, error, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind(model)
        .bind(status)
        .bind(prompt_tokens)
        .bind(completion_tokens)
        .bind(42i64)
        .bind(request_body)
        .bind(response_body)
        .bind(error)
        .bind(created_at)
        .execute(db),
    )
    .await
    .unwrap()
    .last_insert_rowid()
}

fn ids_of(body: &Value) -> Vec<i64> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn logs_without_credentials_return_401() {
    let (status, _) = send_get("/admin/logs", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn empty_log_table_returns_200_with_empty_items_and_zero_total() {
    let (status, body) = send_get("/admin/logs", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["total"], 0);
    assert_eq!(body["page"], 1);
    assert_eq!(body["page_size"], 50);
}

#[tokio::test]
async fn logs_are_paginated_newest_first_and_beyond_range_still_report_total() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let mut expected = Vec::new();
    for i in 0..7 {
        let id = insert_log(
            &db,
            key_id,
            channel_id,
            Some("gpt-4o"),
            None,
            None,
            None,
            &format!("2026-11-07 10:00:{i:02}"),
        )
        .await;
        expected.push(id);
    }

    let (status, page1) =
        send_get_on(&db, "/admin/logs?page_size=3&page=1", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids_of(&page1),
        vec![expected[6], expected[5], expected[4]],
        "newest first"
    );
    assert_eq!(page1["total"], 7);
    assert_eq!(page1["page"], 1);
    assert_eq!(page1["page_size"], 3);

    let (_, page2) = send_get_on(&db, "/admin/logs?page_size=3&page=2", Some(&auth_header())).await;
    assert_eq!(ids_of(&page2), vec![expected[3], expected[2], expected[1]]);

    let (_, page3) = send_get_on(&db, "/admin/logs?page_size=3&page=3", Some(&auth_header())).await;
    assert_eq!(ids_of(&page3), vec![expected[0]]);

    let (_, page4) = send_get_on(&db, "/admin/logs?page_size=3&page=4", Some(&auth_header())).await;
    assert_eq!(page4["items"], json!([]), "page beyond range is empty");
    assert_eq!(page4["total"], 7, "total stays correct beyond range");
}

#[tokio::test]
async fn page_size_clamps_to_the_maximum() {
    let db = test_pool().await;
    let (status, body) =
        send_get_on(&db, "/admin/logs?page_size=10000", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page_size"], 500);
}

#[tokio::test]
async fn page_and_page_size_defaults_apply_and_zero_page_size_floors_at_one() {
    let db = test_pool().await;
    let (_, body) = send_get_on(&db, "/admin/logs?page_size=0&page=0", Some(&auth_header())).await;
    assert_eq!(body["page"], 1);
    assert_eq!(body["page_size"], 1);
}

#[tokio::test]
async fn filters_narrow_by_key_channel_and_model() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let other_channel_id = common::seed_channel(&db, "http://127.0.0.1:2", None).await;
    common::seed_key(&db, other_channel_id).await;
    let other_key_id: i64 = within(
        sqlx::query_scalar("SELECT id FROM api_keys WHERE channel_id = ?")
            .bind(other_channel_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();

    let mine_gpt4o = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-11-07 10:00:00",
    )
    .await;
    let mine_gpt35 = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-3.5-turbo"),
        None,
        None,
        None,
        "2026-11-07 10:01:00",
    )
    .await;
    let other_gpt4o = insert_log(
        &db,
        other_key_id,
        other_channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-11-07 10:02:00",
    )
    .await;

    let (_, by_key) = send_get_on(
        &db,
        &format!("/admin/logs?key_id={key_id}"),
        Some(&auth_header()),
    )
    .await;
    assert_eq!(ids_of(&by_key), vec![mine_gpt35, mine_gpt4o]);
    assert_eq!(by_key["total"], 2);

    let (_, by_channel) = send_get_on(
        &db,
        &format!("/admin/logs?channel_id={other_channel_id}"),
        Some(&auth_header()),
    )
    .await;
    assert_eq!(ids_of(&by_channel), vec![other_gpt4o]);

    let (_, by_model) = send_get_on(&db, "/admin/logs?model=gpt-4o", Some(&auth_header())).await;
    assert_eq!(ids_of(&by_model), vec![other_gpt4o, mine_gpt4o]);

    let (_, combined) = send_get_on(
        &db,
        &format!("/admin/logs?key_id={key_id}&model=gpt-4o"),
        Some(&auth_header()),
    )
    .await;
    assert_eq!(ids_of(&combined), vec![mine_gpt4o]);
}

#[tokio::test]
async fn from_to_filters_use_inclusive_utc_bounds() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let at_10 = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-11-07 10:00:00",
    )
    .await;
    let at_11 = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-11-07 11:00:00",
    )
    .await;
    let at_12 = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-11-07 12:00:00",
    )
    .await;

    // `from` is inclusive: the 10:00 row is part of the window.
    let (_, body) = send_get_on(
        &db,
        "/admin/logs?from=2026-11-07T10:00:00Z",
        Some(&auth_header()),
    )
    .await;
    assert_eq!(ids_of(&body), vec![at_12, at_11, at_10]);

    // `to` is inclusive too.
    let (_, body) = send_get_on(
        &db,
        "/admin/logs?to=2026-11-07T11:00:00Z",
        Some(&auth_header()),
    )
    .await;
    assert_eq!(ids_of(&body), vec![at_11, at_10]);

    // Both bounds together.
    let (_, body) = send_get_on(
        &db,
        "/admin/logs?from=2026-11-07T10:30:00Z&to=2026-11-07T11:30:00Z",
        Some(&auth_header()),
    )
    .await;
    assert_eq!(ids_of(&body), vec![at_11]);

    // A non-UTC offset is normalized to UTC before comparing:
    // 12:00+01:00 is 11:00Z, so the 10:00 row is excluded.
    let (_, body) = send_get_on(
        &db,
        "/admin/logs?from=2026-11-07T12:00:00%2B01:00",
        Some(&auth_header()),
    )
    .await;
    assert_eq!(ids_of(&body), vec![at_12, at_11]);
}

#[tokio::test]
async fn invalid_from_and_to_return_400() {
    let db = test_pool().await;
    let (status, body) =
        send_get_on(&db, "/admin/logs?from=not-a-date", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("from"));

    let (status, body) = send_get_on(&db, "/admin/logs?to=2026-11-07", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("to"));

    let (status, _) = send_get_on(&db, "/admin/logs?page=abc", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn stored_bodies_are_returned_only_when_present() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let with_bodies = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some("{\"model\":\"gpt-4o\"}"),
        Some("{\"id\":\"chatcmpl-x\"}"),
        None,
        "2026-11-07 10:00:00",
    )
    .await;
    let without_bodies = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-11-07 10:01:00",
    )
    .await;

    let (_, body) = send_get_on(&db, "/admin/logs", Some(&auth_header())).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);

    let captured = items
        .iter()
        .find(|i| i["id"].as_i64() == Some(with_bodies))
        .unwrap();
    assert_eq!(
        captured["request_body"],
        json!("{\"model\":\"gpt-4o\"}"),
        "stored bodies pass through verbatim"
    );
    assert_eq!(captured["response_body"], json!("{\"id\":\"chatcmpl-x\"}"));

    let bare = items
        .iter()
        .find(|i| i["id"].as_i64() == Some(without_bodies))
        .unwrap();
    assert!(
        bare.get("request_body").is_none(),
        "null body must omit the key"
    );
    assert!(bare.get("response_body").is_none());

    // Non-body fields stay complete on both rows.
    for item in items {
        assert_eq!(item["status"], 200);
        assert_eq!(item["prompt_tokens"], 10);
        assert_eq!(item["completion_tokens"], 5);
        assert!(
            item["cached_input_tokens"].is_null(),
            "a row without cache hits reports null, not zero"
        );
        assert!(item["cached_output_tokens"].is_null());
        assert_eq!(item["duration_ms"], 42);
        assert!(item["created_at"].is_string());
    }
}

#[tokio::test]
async fn cached_token_columns_are_exposed_in_the_listing_and_the_detail() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let cached: i64 = within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              cached_input_tokens, cached_output_tokens, duration_ms, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind("gpt-4o")
        .bind(200)
        .bind(100)
        .bind(20)
        .bind(64)
        .bind(8)
        .bind(42i64)
        .bind("2026-11-07 10:00:00")
        .execute(&db),
    )
    .await
    .unwrap()
    .last_insert_rowid();

    let (_, list) = send_get_on(&db, "/admin/logs", Some(&auth_header())).await;
    let item = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"].as_i64() == Some(cached))
        .expect("the cached row must be listed");
    assert_eq!(item["cached_input_tokens"], 64);
    assert_eq!(item["cached_output_tokens"], 8);

    let (_, detail) =
        send_get_on(&db, &format!("/admin/logs/{cached}"), Some(&auth_header())).await;
    assert_eq!(detail["cached_input_tokens"], 64);
    assert_eq!(detail["cached_output_tokens"], 8);
}

#[tokio::test]
async fn error_rows_keep_their_error_field_in_the_listing() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        Some("upstream connection refused"),
        "2026-11-07 10:00:00",
    )
    .await;

    let (_, body) = send_get_on(&db, "/admin/logs", Some(&auth_header())).await;
    assert_eq!(
        body["items"][0]["error"],
        json!("upstream connection refused")
    );
}

#[tokio::test]
async fn malformed_numeric_query_params_return_400() {
    let db = test_pool().await;
    let (status, _) = send_get_on(&db, "/admin/logs?key_id=zero", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send_get_on(&db, "/admin/logs?channel_id=-1.5", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn status_class_filter_buckets_rows_by_upstream_status() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let ok_row = insert_status_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        Some(1),
        Some(1),
        None,
        None,
        None,
        "2026-11-07 10:00:00",
    )
    .await;
    let client_error_row = insert_status_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(404),
        None,
        None,
        None,
        None,
        Some("upstream returned 404"),
        "2026-11-07 10:01:00",
    )
    .await;
    let server_error_row = insert_status_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(502),
        None,
        None,
        None,
        None,
        Some("upstream returned 502"),
        "2026-11-07 10:02:00",
    )
    .await;
    // A transport failure: the request never reached the upstream, so the
    // status is null but the row is still an error.
    let transport_error_row = insert_status_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        None,
        None,
        Some("upstream connection refused"),
        "2026-11-07 10:03:00",
    )
    .await;

    let (_, body) = send_get_on(&db, "/admin/logs?status_class=2xx", Some(&auth_header())).await;
    assert_eq!(ids_of(&body), vec![ok_row]);

    let (_, body) = send_get_on(&db, "/admin/logs?status_class=4xx", Some(&auth_header())).await;
    assert_eq!(ids_of(&body), vec![client_error_row]);

    let (_, body) = send_get_on(&db, "/admin/logs?status_class=5xx", Some(&auth_header())).await;
    assert_eq!(ids_of(&body), vec![server_error_row]);

    let (_, body) = send_get_on(&db, "/admin/logs?status_class=errors", Some(&auth_header())).await;
    assert_eq!(
        ids_of(&body),
        vec![transport_error_row, server_error_row, client_error_row]
    );

    // Without the filter every row comes back.
    let (_, body) = send_get_on(&db, "/admin/logs", Some(&auth_header())).await;
    assert_eq!(body["total"], 4);
}

#[tokio::test]
async fn invalid_status_class_returns_400() {
    let db = test_pool().await;
    let (status, body) =
        send_get_on(&db, "/admin/logs?status_class=3xx", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("status_class"));
}

#[tokio::test]
async fn log_detail_returns_the_full_row_including_bodies_and_client_ip() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let id: i64 = within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              duration_ms, request_body, response_body, error, client_ip, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind("gpt-4o")
        .bind(200)
        .bind(12)
        .bind(34)
        .bind(845)
        .bind(r#"{"model":"gpt-4o","messages":[]}"#)
        .bind(r#"{"id":"chatcmpl-1"}"#)
        .bind(Option::<String>::None)
        .bind("203.0.113.7")
        .bind("2026-11-07 23:59:59")
        .execute(&db),
    )
    .await
    .unwrap()
    .last_insert_rowid();

    let (status, body) = send_get_on(&db, &format!("/admin/logs/{id}"), Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id);
    assert_eq!(body["api_key_id"], key_id);
    assert_eq!(body["channel_id"], channel_id);
    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["status"], 200);
    assert_eq!(body["prompt_tokens"], 12);
    assert_eq!(body["completion_tokens"], 34);
    assert_eq!(body["cached_input_tokens"], Value::Null);
    assert_eq!(body["cached_output_tokens"], Value::Null);
    assert_eq!(body["duration_ms"], 845);
    assert_eq!(body["error"], Value::Null);
    assert_eq!(body["created_at"], "2026-11-07 23:59:59");
    assert_eq!(body["client_ip"], "203.0.113.7");
    assert_eq!(
        body["request_body"],
        json!(r#"{"model":"gpt-4o","messages":[]}"#)
    );
    assert_eq!(body["response_body"], json!(r#"{"id":"chatcmpl-1"}"#));
}

#[tokio::test]
async fn log_detail_bodies_are_explicit_null_when_not_captured() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    let id: i64 = within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              duration_ms, request_body, response_body, error, client_ip, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind(Option::<String>::None)
        .bind(200)
        .bind(1)
        .bind(1)
        .bind(5)
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind("2026-11-07 10:00:00")
        .execute(&db),
    )
    .await
    .unwrap()
    .last_insert_rowid();

    let (status, body) = send_get_on(&db, &format!("/admin/logs/{id}"), Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    // Unlike the listing, the detail view keeps the body keys present so a
    // consumer can tell "not captured" apart from "missing field".
    assert!(body.get("request_body").is_some());
    assert_eq!(body["request_body"], Value::Null);
    assert!(body.get("response_body").is_some());
    assert_eq!(body["response_body"], Value::Null);
    assert_eq!(body["client_ip"], Value::Null);
    assert_eq!(body["error"], Value::Null);
}

#[tokio::test]
async fn unknown_log_id_returns_404() {
    let db = test_pool().await;
    let (status, body) = send_get_on(&db, "/admin/logs/999999", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "request log not found");
}

#[tokio::test]
async fn log_detail_without_credentials_returns_401_with_challenge() {
    let db = test_pool().await;
    let app = test_app(db.clone());
    let response = within(
        hydra::http::router(app).oneshot(
            Request::builder()
                .uri("/admin/logs/1")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()["WWW-Authenticate"],
        r#"Basic realm="hydra""#
    );
}

/// Inserts one rejection row with no key or channel reference, the shape
/// an unknown-key 401 produces.
async fn insert_rejection_log(db: &SqlitePool, error: &str, created_at: &str) -> i64 {
    within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              duration_ms, request_body, response_body, error, created_at) \
             VALUES (NULL, NULL, NULL, 401, NULL, NULL, 1, ?, NULL, ?, ?)",
        )
        .bind(r#"{"model":"gpt-4o","messages":[]}"#)
        .bind(error)
        .bind(created_at)
        .execute(db),
    )
    .await
    .unwrap()
    .last_insert_rowid()
}

#[tokio::test]
async fn rejection_rows_with_null_ids_are_listed_with_null_key_and_channel() {
    let db = test_pool().await;
    let (channel_id, key_id) = seed_channel_and_key(&db).await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-11-08 10:00:00",
    )
    .await;
    let rejection_id =
        insert_rejection_log(&db, "Invalid API key provided", "2026-11-08 10:01:00").await;

    let (status, body) = send_get_on(&db, "/admin/logs", Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["id"], rejection_id, "newest first");
    assert_eq!(items[0]["api_key_id"], Value::Null);
    assert_eq!(items[0]["channel_id"], Value::Null);
    assert_eq!(items[0]["status"], 401);
    assert_eq!(items[0]["error"], "Invalid API key provided");
    assert_eq!(items[1]["api_key_id"], key_id);
    assert_eq!(items[1]["channel_id"], channel_id);
}

#[tokio::test]
async fn log_detail_of_a_rejection_row_returns_null_ids() {
    let db = test_pool().await;
    let id = insert_rejection_log(&db, "Invalid API key provided", "2026-11-08 10:01:00").await;

    let (status, body) = send_get_on(&db, &format!("/admin/logs/{id}"), Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id);
    assert_eq!(body["api_key_id"], Value::Null);
    assert_eq!(body["channel_id"], Value::Null);
    assert_eq!(body["status"], 401);
    assert_eq!(body["error"], "Invalid API key provided");
    assert_eq!(
        body["request_body"],
        json!(r#"{"model":"gpt-4o","messages":[]}"#)
    );
}
