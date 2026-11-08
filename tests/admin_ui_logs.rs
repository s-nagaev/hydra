//! Integration tests for the admin UI logs page: auth gate, row rendering
//! with status badges and token dashes, body-capture protection, filter
//! and URL-state round-trips, pagination bounds, and error banners for
//! invalid query values. Rows are seeded through direct sqlx inserts.

mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use sqlx::SqlitePool;
use tower::ServiceExt;

use common::{test_app, test_pool, within};

fn auth_header() -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    format!("Basic {}", STANDARD.encode("admin:secret"))
}

type Send = (StatusCode, String);

async fn send(db: &SqlitePool, uri: &str, auth: Option<&str>) -> Send {
    let app = test_app(db.clone());
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(auth) = auth {
        builder = builder.header("Authorization", auth);
    }
    let response = within(hydra::http::router(app).oneshot(builder.body(Body::empty()).unwrap()))
        .await
        .unwrap();
    let status = response.status();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn get_page(db: &SqlitePool, uri: &str) -> Send {
    send(db, uri, Some(&auth_header())).await
}

/// Seeds one channel and one key; returns (channel_id, key_id, key_prefix).
async fn seed_channel_and_key(db: &SqlitePool) -> (i64, i64, String) {
    let channel_id = common::seed_channel(db, "http://127.0.0.1:1", None).await;
    common::seed_key(db, channel_id).await;
    let (key_id, prefix): (i64, String) = within(
        sqlx::query_as("SELECT id, display_prefix FROM api_keys WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(db),
    )
    .await
    .unwrap();
    (channel_id, key_id, prefix)
}

#[allow(clippy::too_many_arguments)]
async fn insert_log(
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

/// Extracts the value attribute of the input with the given id.
fn input_value(html: &str, id: &str) -> String {
    let marker = format!("id=\"{id}\"");
    let after = html.split(&marker).nth(1).expect("input must be present");
    let prefix = "value=\"";
    let start = after.find(prefix).expect("value attribute") + prefix.len();
    after[start..].split('"').next().unwrap().to_string()
}

// --- Auth gate ------------------------------------------------------------

#[tokio::test]
async fn logs_page_without_credentials_returns_401() {
    let db = test_pool().await;
    let (status, body) = send(&db, "/admin/ui/logs", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.is_empty());
}

#[tokio::test]
async fn logs_page_renders_with_auth() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Request logs"));
    assert!(body.contains("class=\"active\""));
}

// --- Empty state ----------------------------------------------------------

#[tokio::test]
async fn empty_table_shows_the_friendly_hint() {
    let db = test_pool().await;
    let (_, body) = get_page(&db, "/admin/ui/logs").await;
    assert!(body.contains("No requests logged yet"));
}

#[tokio::test]
async fn filters_without_matches_show_the_filtered_message() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/logs?model=never-seen").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No requests match the current filters"));
}

// --- Row rendering --------------------------------------------------------

#[tokio::test]
async fn rows_render_status_badges_token_dashes_and_labels() {
    let db = test_pool().await;
    let (channel_id, key_id, prefix) = seed_channel_and_key(&db).await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        Some(10),
        Some(5),
        None,
        None,
        None,
        "2026-11-07 10:00:00",
    )
    .await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(404),
        None,
        None,
        None,
        None,
        None,
        "2026-11-07 10:01:00",
    )
    .await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(500),
        Some(7),
        None,
        None,
        None,
        None,
        "2026-11-07 10:02:00",
    )
    .await;
    insert_log(
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

    let (status, body) = get_page(&db, "/admin/ui/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge status-2xx"));
    assert!(body.contains("badge status-4xx"));
    assert!(body.contains("badge status-5xx"));
    assert!(body.contains("badge status-err"));
    assert!(body.contains("10 / 5"));
    assert!(body.contains("7 / -"));
    assert!(body.contains("upstream connection refused"));
    assert!(body.contains(prefix.as_str()));
    assert!(body.contains("of 4"));
}

#[tokio::test]
async fn captured_bodies_show_a_badge_but_never_their_content() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        Some(1),
        Some(1),
        Some("{\"prompt\":\"super-secret-prompt-42\"}"),
        Some("{\"reply\":\"super-secret-reply-42\"}"),
        None,
        "2026-11-07 10:00:00",
    )
    .await;

    let (status, body) = get_page(&db, "/admin/ui/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge bodies"));
    assert!(!body.contains("super-secret-prompt-42"));
    assert!(!body.contains("super-secret-reply-42"));
}

// --- URL state and filters ------------------------------------------------

#[tokio::test]
async fn filter_widgets_reflect_the_query_params() {
    let db = test_pool().await;
    let (channel_id, key_id, prefix) = seed_channel_and_key(&db).await;

    let uri = format!(
        "/admin/ui/logs?key_id={key_id}&channel_id={channel_id}&model=gpt-4o&status_class=2xx&preset=24h"
    );
    let (status, body) = get_page(&db, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!("<option value=\"{key_id}\" selected>")),
        "key select must preselect the URL key"
    );
    assert!(
        body.contains(&format!("<option value=\"{channel_id}\" selected>")),
        "channel select must preselect the URL channel"
    );
    assert!(body.contains("name=\"model\" value=\"gpt-4o\""));
    assert!(body.contains("<option value=\"2xx\" selected>"));
    assert!(body.contains("<option value=\"24h\" selected>"));
    assert!(
        body.contains(prefix.as_str()),
        "select shows the key prefix"
    );
}

#[tokio::test]
async fn preset_1h_computes_a_from_bound_near_now_minus_an_hour() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/logs?preset=1h").await;
    assert_eq!(status, StatusCode::OK);

    let value = input_value(&body, "log-from");
    let parsed = time::OffsetDateTime::parse(
        &format!("{value}Z"),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let expected = time::OffsetDateTime::now_utc() - time::Duration::minutes(60);
    assert!(
        (parsed - expected).abs() < time::Duration::seconds(30),
        "rendered from bound {value} is too far from now minus an hour"
    );
    // The preset stays in the URL (refresh link), so reload restores it.
    assert!(body.contains("preset=1h"));
}

#[tokio::test]
async fn custom_range_filters_rows_and_echoes_the_inputs() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("model-morning"),
        Some(200),
        None,
        None,
        None,
        None,
        None,
        "2026-11-07 10:30:00",
    )
    .await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("model-evening"),
        Some(200),
        None,
        None,
        None,
        None,
        None,
        "2026-11-07 12:00:00",
    )
    .await;

    let (status, body) = get_page(
        &db,
        "/admin/ui/logs?preset=custom&from=2026-11-07T10:00&to=2026-11-07T11:00",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("model-morning"));
    assert!(!body.contains("model-evening"));
    assert_eq!(input_value(&body, "log-from"), "2026-11-07T10:00");
    assert_eq!(input_value(&body, "log-to"), "2026-11-07T11:00");
    assert!(body.contains("of 1"));
}

#[tokio::test]
async fn unknown_preset_is_ignored_and_renders_everything() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        None,
        None,
        None,
        None,
        None,
        "2026-11-07 10:00:00",
    )
    .await;

    let (status, body) = get_page(&db, "/admin/ui/logs?preset=bogus").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("gpt-4o"));
    assert_eq!(input_value(&body, "log-from"), "");
}

// --- Pagination -----------------------------------------------------------

#[tokio::test]
async fn pagination_links_follow_the_current_page_and_bounds() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    for i in 0..7 {
        insert_log(
            &db,
            key_id,
            channel_id,
            Some("gpt-4o"),
            Some(200),
            None,
            None,
            None,
            None,
            None,
            &format!("2026-11-07 10:0{i}:00"),
        )
        .await;
    }

    let (status, body) = get_page(&db, "/admin/ui/logs?page=2&page_size=3").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("of 7"));
    assert!(body.contains("class=\"current\">2</span>"));
    assert!(body.contains("page=1"), "prev link");
    assert!(body.contains("page=3"), "next link");

    // First page: prev disabled, next is a link.
    let (_, body) = get_page(&db, "/admin/ui/logs?page_size=3").await;
    assert!(body.contains("<span class=\"disabled\">Prev</span>"));
    assert!(body.contains(">Next</a>"));
    // Second page: both are links.
    let (_, body) = get_page(&db, "/admin/ui/logs?page=2&page_size=3").await;
    assert!(body.contains(">Prev</a>"));
    assert!(body.contains(">Next</a>"));
}

#[tokio::test]
async fn page_size_selector_carries_the_url_value() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/logs?page_size=100").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<option value=\"100\" selected>"));
}

// --- Invalid input --------------------------------------------------------

#[tokio::test]
async fn invalid_query_values_render_an_error_banner_not_a_crash() {
    let db = test_pool().await;

    let (status, body) = get_page(&db, "/admin/ui/logs?from=not-a-date").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("from must be an RFC3339 timestamp"));

    let (status, body) = get_page(&db, "/admin/ui/logs?page=abc").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("page must be a positive integer"));

    let (status, body) = get_page(&db, "/admin/ui/logs?key_id=zero").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("key_id must be a row id"));

    let (status, body) = get_page(&db, "/admin/ui/logs?status_class=bogus").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("status_class must be one of"));
}
