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
async fn logs_page_without_a_session_redirects_to_the_login_page() {
    let db = test_pool().await;
    let (status, body) = send(&db, "/admin/ui/logs", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
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

#[tokio::test]
async fn page_beyond_the_last_one_renders_the_empty_state_with_correct_meta() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    for i in 0..2 {
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

    // Two rows, page size 25: page 2 exists in the URL but has no rows.
    let (status, body) = get_page(&db, "/admin/ui/logs?page=2&page_size=25").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No rows on this page. Go back to an earlier page."));
    // Meta reflects reality: one page total, no next link, prev works, and
    // the range display is omitted because the page holds no rows.
    assert!(!body.contains("of 2"));
    assert!(body.contains("<span class=\"disabled\">Next</span>"));
    assert!(body.contains(">Prev</a>"));
    // The strip clamps to real pages: only page 1 renders, as a plain link
    // (the requested page 2 exists in the URL but not in the strip).
    assert!(body.contains(">1</a>"));
    assert!(!body.contains("class=\"current\">2"));
}

#[tokio::test]
async fn client_ip_column_renders_the_ip_or_a_dash() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, duration_ms, client_ip, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind("gpt-4o")
        .bind(200i64)
        .bind(42i64)
        .bind("198.51.100.5")
        .bind("2026-11-07 10:00:00")
        .execute(&db),
    )
    .await
    .unwrap();
    within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, duration_ms, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind("gpt-4o")
        .bind(200i64)
        .bind(42i64)
        .bind("2026-11-07 10:01:00")
        .execute(&db),
    )
    .await
    .unwrap();

    let (status, body) = get_page(&db, "/admin/ui/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Client IP"));
    assert!(body.contains("198.51.100.5"));
    // The row without a captured IP shows the muted dash placeholder.
    assert!(body.contains(r#"<td class="mono"><span class="text-muted">-</span></td>"#));
}

#[tokio::test]
async fn rejection_rows_with_null_ids_render_placeholder_labels() {
    let db = test_pool().await;
    seed_channel_and_key(&db).await;
    within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              duration_ms, request_body, response_body, error, created_at) \
             VALUES (NULL, NULL, NULL, 401, NULL, NULL, 1, NULL, NULL, ?, ?)",
        )
        .bind("Invalid API key provided")
        .bind("2026-11-08 10:00:00")
        .execute(&db),
    )
    .await
    .unwrap();

    let (status, body) = get_page(&db, "/admin/ui/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("unknown key"),
        "a NULL key id must render as a placeholder, not a lookup panic: {body}"
    );
    assert!(body.contains("unknown channel"));
    assert!(body.contains("Invalid API key provided"));
}

/// Inserts a rejection-style row with NULL key and channel ids.
async fn insert_rejection_log(db: &SqlitePool, error: &str, created_at: &str) -> i64 {
    within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              duration_ms, request_body, response_body, error, created_at) \
             VALUES (NULL, NULL, NULL, 401, NULL, NULL, 1, NULL, NULL, ?, ?)",
        )
        .bind(error)
        .bind(created_at)
        .execute(db),
    )
    .await
    .unwrap()
    .last_insert_rowid()
}

#[tokio::test]
async fn detail_page_renders_metadata_and_pretty_json_bodies() {
    let db = test_pool().await;
    let (channel_id, key_id, prefix) = seed_channel_and_key(&db).await;
    let log_id = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        Some(10),
        Some(5),
        Some(r#"{"prompt":"super-secret-prompt-42"}"#),
        Some(r#"{"reply":"super-secret-reply-42"}"#),
        None,
        "2026-11-07 10:00:00",
    )
    .await;

    let (status, body) = get_page(&db, &format!("/admin/ui/logs/{log_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("Request log #{log_id}")));
    assert!(body.contains(prefix.as_str()));
    assert!(body.contains("gpt-4o"));
    assert!(body.contains("badge status-2xx"));
    assert!(body.contains("10 / 5"));
    // Pretty-printed JSON: the newline, indentation and the space after
    // the colon exist only in the rendered form, the stored text is flat.
    assert!(body.contains("{\n  &#34;prompt&#34;: &#34;super-secret-prompt-42&#34;\n}"));
    assert!(body.contains("&#34;reply&#34;: &#34;super-secret-reply-42&#34;"));
    assert!(!body.contains("Bodies were not captured"));
}

#[tokio::test]
async fn detail_page_without_bodies_shows_the_capture_placeholder() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    let log_id = insert_log(
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

    let (status, body) = get_page(&db, &format!("/admin/ui/logs/{log_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.matches("Bodies were not captured").count(),
        2,
        "both body sections must carry the placeholder: {body}"
    );
}

#[tokio::test]
async fn detail_page_wraps_each_captured_body_in_a_collapsible_section() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    let log_id = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        None,
        None,
        Some(r#"{"prompt":"hi"}"#),
        Some("data: {\"choices\":[]}\n\ndata: [DONE]\n\n"),
        None,
        "2026-11-07 10:00:00",
    )
    .await;

    let (status, body) = get_page(&db, &format!("/admin/ui/logs/{log_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<details class=\"body-block\" open>"),
        "the request body opens by default: {body}"
    );
    assert!(
        body.contains("<details class=\"body-block\">"),
        "the response body starts collapsed: {body}"
    );
    assert!(body.contains("<summary class=\"snippet-label\">Request body</summary>"));
    assert!(body.contains("<summary class=\"snippet-label\">Response body</summary>"));
    // The SSE capture arrives frame by frame, each data payload pretty-printed.
    assert!(
        body.contains("data: {\n  &#34;choices&#34;: []\n}\n\ndata: [DONE]"),
        "the streamed body must be rendered frame by frame: {body}"
    );
    assert!(body.contains("/admin/ui/static/admin.js"));
}

#[tokio::test]
async fn detail_page_decodes_unicode_escapes_in_bodies_that_are_not_json() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    // A capture cut at the 1 MiB cap: no longer valid JSON, still worth reading.
    let log_id = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        None,
        None,
        Some(r#"{"prompt":"\u041f\u0440\u0438\u0432\u0435\u0442"#),
        Some(r"upstream error: \u041e\u0448\u0438\u0431\u043a\u0430"),
        None,
        "2026-11-07 10:00:00",
    )
    .await;

    let (status, body) = get_page(&db, &format!("/admin/ui/logs/{log_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Привет"),
        "a truncated body must still read as Cyrillic: {body}"
    );
    assert!(body.contains("Ошибка"));
    assert!(
        !body.contains(r"\u041f"),
        "the escapes must not survive: {body}"
    );
}

#[tokio::test]
async fn detail_page_shows_error_and_client_ip() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    let log_id = within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, duration_ms, error, client_ip, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind("gpt-4o")
        .bind(502i64)
        .bind(42i64)
        .bind("upstream returned 502")
        .bind("198.51.100.5")
        .bind("2026-11-07 10:00:00")
        .execute(&db),
    )
    .await
    .unwrap()
    .last_insert_rowid();

    let (status, body) = get_page(&db, &format!("/admin/ui/logs/{log_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge status-5xx"));
    assert!(body.contains("upstream returned 502"));
    assert!(body.contains("198.51.100.5"));
}

#[tokio::test]
async fn detail_page_with_null_key_and_channel_renders_placeholders() {
    let db = test_pool().await;
    seed_channel_and_key(&db).await;
    let log_id = insert_rejection_log(&db, "Invalid API key provided", "2026-11-08 10:00:00").await;

    let (status, body) = get_page(&db, &format!("/admin/ui/logs/{log_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("unknown key"));
    assert!(body.contains("unknown channel"));
    assert!(body.contains("Invalid API key provided"));
    assert!(body.contains("badge status-4xx"));
}

#[tokio::test]
async fn unknown_log_id_renders_the_not_found_banner() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/logs/99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("request log not found"));
}

#[tokio::test]
async fn malformed_log_id_renders_the_bad_request_banner() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/logs/not-a-number").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid log id"));
}

#[tokio::test]
async fn detail_page_without_a_session_redirects_to_the_login_page() {
    let db = test_pool().await;
    let (status, body) = send(&db, "/admin/ui/logs/1", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(body.is_empty());
}

#[tokio::test]
async fn list_rows_link_to_detail_and_the_back_link_restores_the_view() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    let log_id = insert_log(
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

    // The list row carries the whole view state in the detail link.
    let (status, body) = get_page(&db, "/admin/ui/logs?page_size=25&model=gpt-4o").await;
    assert_eq!(status, StatusCode::OK);
    // Askama escapes the ampersands inside the href attribute.
    assert!(
        body.contains(&format!(
            "href=\"/admin/ui/logs/{log_id}?page=1&#38;page_size=25&#38;model=gpt-4o\""
        )),
        "row must link to the detail page with the view state: {body}"
    );

    // Round-trip: the detail page's back link restores the same view.
    let (status, body) = get_page(
        &db,
        &format!("/admin/ui/logs/{log_id}?page=1&page_size=25&model=gpt-4o"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("href=\"/admin/ui/logs?page=1&#38;page_size=25&#38;model=gpt-4o\""));
}

#[tokio::test]
async fn detail_page_escapes_markup_in_bodies() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    let log_id = insert_log(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        None,
        None,
        Some("<script>alert('xss-request')</script>"),
        Some("<img src=x onerror=alert('xss-response')>"),
        None,
        "2026-11-07 10:00:00",
    )
    .await;

    let (status, body) = get_page(&db, &format!("/admin/ui/logs/{log_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("<script>alert('xss-request')</script>"),
        "raw script must never reach the page: {body}"
    );
    assert!(!body.contains("<img src=x"));
    // The escaped text is still shown, inert.
    assert!(body.contains("&#60;script&#62;alert(&#39;xss-request&#39;)&#60;/script&#62;"));
}

// --- Cached tokens --------------------------------------------------------

/// Inserts one row with explicit cached token counts; every other column
/// stays on a fixed value so the rendered cells are predictable.
async fn insert_cached_log(
    db: &SqlitePool,
    key_id: i64,
    channel_id: i64,
    cached_input: Option<i64>,
    cached_output: Option<i64>,
    created_at: &str,
) -> i64 {
    within(
        sqlx::query(
            "INSERT INTO request_logs \
             (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, \
              cached_input_tokens, cached_output_tokens, duration_ms, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key_id)
        .bind(channel_id)
        .bind("gpt-4o")
        .bind(200i64)
        .bind(10i64)
        .bind(5i64)
        .bind(cached_input)
        .bind(cached_output)
        .bind(42i64)
        .bind(created_at)
        .execute(db),
    )
    .await
    .unwrap()
    .last_insert_rowid()
}

#[tokio::test]
async fn cached_tokens_render_beside_the_plain_tokens_with_a_dash_for_nulls() {
    let db = test_pool().await;
    let (channel_id, key_id, _) = seed_channel_and_key(&db).await;
    let both = insert_cached_log(
        &db,
        key_id,
        channel_id,
        Some(6),
        Some(2),
        "2026-11-07 10:00:00",
    )
    .await;
    let mixed = insert_cached_log(
        &db,
        key_id,
        channel_id,
        Some(4),
        None,
        "2026-11-07 10:01:00",
    )
    .await;
    let none = insert_cached_log(&db, key_id, channel_id, None, None, "2026-11-07 10:02:00").await;

    let (status, body) = get_page(&db, "/admin/ui/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Cached tokens (in / out)"));
    assert!(body.contains("6 / 2"));

    let (status, detail) = get_page(&db, &format!("/admin/ui/logs/{both}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("Cached tokens (in / out)"));
    assert!(detail.contains("6 / 2"));

    // Half a report renders the known half and a dash for the other.
    let (_, detail) = get_page(&db, &format!("/admin/ui/logs/{mixed}")).await;
    assert!(detail.contains("4 / -"));

    // No report at all renders the same dash the empty cells use.
    let (_, detail) = get_page(&db, &format!("/admin/ui/logs/{none}")).await;
    assert!(detail.contains(r#"<td class="mono">-</td>"#));
}
