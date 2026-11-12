//! Integration tests for the admin UI stats page: the auth gate, month
//! selection and navigation, per key and per model grouping with summed
//! tokens and priced cost, the inactive-limit warning, the omission of keys
//! without usage, soft-deleted keys, rows whose upstream reported no usage
//! and the empty and malformed month states.
//!
//! Rows are seeded through the database layer with explicit timestamps, and
//! the page itself is driven through the real router, so the tests pin the
//! rendered table rather than any internal helper.

mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use sqlx::SqlitePool;
use tower::ServiceExt;

use common::{test_app, test_pool, within};
use hydra::db::channels::{self, NewChannel};
use hydra::db::keys::{self, NewKey};
use hydra::db::request_logs::{self, NewRequestLog};
use hydra::db::Db;
use hydra::month::Month;
use hydra::prices::ModelPrices;

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

fn current_month_label() -> String {
    String::from_utf8_lossy(&Month::current().bucket()).to_string()
}

/// A channel whitelisted to `gpt-4o` and priced for it, so it is limitable:
/// this is the shape a key with a monthly limit is enforced against.
async fn seed_priced_channel(db: &SqlitePool) -> i64 {
    let prices = ModelPrices::from_usd_json(&serde_json::json!({
        "gpt-4o": {
            "input": "2.50",
            "output": "10.00",
            "cached_input": "1.25",
            "cached_output": "5.00"
        }
    }))
    .unwrap();
    within(channels::create_with_prices(
        &Db::from(db.clone()),
        &NewChannel {
            name: "priced".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: common::UPSTREAM_KEY.to_string(),
            allowed_models: Some(vec!["gpt-4o".to_string()]),
        },
        Some(&prices),
    ))
    .await
    .unwrap()
    .channel
    .id
}

/// A channel with an open whitelist and no prices: it can never be limited,
/// whatever limit its keys carry.
async fn seed_unlimitable_channel(db: &SqlitePool) -> i64 {
    within(channels::create(
        &Db::from(db.clone()),
        &NewChannel {
            name: "open".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: common::UPSTREAM_KEY.to_string(),
            allowed_models: None,
        },
    ))
    .await
    .unwrap()
    .id
}

/// Inserts one key bound to `channel_id`; returns its id and display prefix.
async fn seed_key(
    db: &SqlitePool,
    channel_id: i64,
    name: &str,
    limit: Option<i64>,
) -> (i64, String) {
    let prefix = format!("hyd-{name}");
    let key = within(keys::create(
        &Db::from(db.clone()),
        &NewKey {
            key_hash: format!("hash-{name}"),
            display_prefix: prefix.clone(),
            name: Some(name.to_string()),
            channel_id,
            monthly_limit_micro_usd: limit,
        },
    ))
    .await
    .unwrap();
    (key.id, prefix)
}

/// One request log row at an explicit instant. Every token dimension is
/// explicit, so a `None` token column means the upstream reported nothing.
#[allow(clippy::too_many_arguments)]
async fn log_at(
    db: &SqlitePool,
    key_id: i64,
    channel_id: i64,
    model: Option<&str>,
    prompt: Option<i64>,
    completion: Option<i64>,
    cached_input: Option<i64>,
    at: &str,
) {
    within(request_logs::insert_at(
        &Db::from(db.clone()),
        &NewRequestLog {
            api_key_id: Some(key_id),
            channel_id: Some(channel_id),
            model: model.map(str::to_string),
            status: Some(200),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_input_tokens: cached_input,
            cached_output_tokens: None,
            cache_write_input_tokens: None,
            duration_ms: 1,
            request_body: None,
            response_body: None,
            error: None,
            client_ip: None,
        },
        at,
    ))
    .await
    .unwrap();
}

/// 1000 prompt tokens with 400 of them cached and 200 completion tokens at
/// the seeded 2.50/1.25/10.00 USD per 1M table, which the pinned formula
/// prices at 4000 micro-USD.
async fn log_priced_request(db: &SqlitePool, key_id: i64, channel_id: i64, at: &str) {
    log_at(
        db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(1000),
        Some(200),
        Some(400),
        at,
    )
    .await;
}

// --- Auth gate and navigation --------------------------------------------

#[tokio::test]
async fn stats_page_without_a_session_redirects_to_the_login_page() {
    let db = test_pool().await;
    let (status, body) = send(&db, "/admin/ui/stats", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(body.is_empty());
}

#[tokio::test]
async fn stats_page_renders_the_active_nav_entry_and_the_footnote() {
    let db = test_pool().await;
    let (status, html) = get_page(&db, "/admin/ui/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Token usage"), "{html}");
    assert!(html.contains("href=\"/admin/ui/stats\""), "{html}");
    assert!(html.contains(">Stat</a>"), "{html}");
    assert!(
        html.contains("class=\"active\""),
        "the Stat entry is active"
    );
    assert!(
        html.contains("Requests without reported usage are not counted"),
        "the NULL-usage footnote must be on the page"
    );
    assert!(html.contains("Month (UTC)"));
}

// --- Month selection ------------------------------------------------------

#[tokio::test]
async fn stats_page_defaults_to_the_current_month() {
    let db = test_pool().await;
    let (status, html) = get_page(&db, "/admin/ui/stats").await;
    assert_eq!(status, StatusCode::OK);
    let label = current_month_label();
    assert!(
        html.contains(&format!(
            "id=\"stats-month\" name=\"month\" value=\"{label}\""
        )),
        "the month input must default to {label}: {html}"
    );
    assert!(html.contains(&format!("Token usage <span class=\"text-muted\">{label}")));
}

#[tokio::test]
async fn the_selected_month_drives_the_rows_and_the_navigation_links() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "jan", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-01-05 10:00:00").await;
    log_priced_request(&db, key_id, channel_id, "2026-02-05 10:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-01").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&prefix), "{html}");
    assert!(
        html.contains("/admin/ui/stats?month=2025-12"),
        "the previous month link must step back a month"
    );
    assert!(
        html.contains("/admin/ui/stats?month=2026-02"),
        "the next month link must step forward a month"
    );
    // One request that month: 1000 prompt, 200 completion, 400 cached input.
    assert!(html.contains(">1,000<"), "{html}");
    assert!(html.contains(">400<"), "{html}");
    assert!(html.contains(">$0.004<"), "one priced request: {html}");

    // The February view carries its own numbers and does not leak January
    // into them.
    let (_, february) = get_page(&db, "/admin/ui/stats?month=2026-02").await;
    assert!(february.contains(">$0.004<"));
    assert!(
        february.contains("/admin/ui/stats?month=2026-01"),
        "February links back to January"
    );
    assert!(
        february.contains("/admin/ui/stats?month=2026-03"),
        "February links forward to March"
    );

    // A month after the logged rows renders the empty state.
    let (_, empty) = get_page(&db, "/admin/ui/stats?month=2026-05").await;
    assert!(empty.contains("No requests logged in 2026-05"), "{empty}");
    assert!(
        !empty.contains(&prefix),
        "no rows for a month without usage"
    );
}

#[tokio::test]
async fn a_malformed_month_falls_back_to_the_current_month_behind_a_banner() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "now", None).await;
    // A request inside the current month, built from the same label the page
    // defaults to.
    let now = format!("{}-01 10:00:00", current_month_label());
    log_priced_request(&db, key_id, channel_id, &now).await;

    let (status, html) = get_page(&db, "/admin/ui/stats?month=January").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("is not a YYYY-MM month"),
        "the banner must explain the expected shape: {html}"
    );
    assert!(
        html.contains("January"),
        "the banner must name the offending value: {html}"
    );
    assert!(html.contains(&prefix), "the current month still renders");
    assert!(html.contains(&format!("value=\"{}\"", current_month_label())));
}

// --- Grouping, cost and warnings -----------------------------------------

#[tokio::test]
async fn rows_group_by_key_and_model_with_totals_and_a_grand_total() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (first_id, first_prefix) = seed_key(&db, channel_id, "first", None).await;
    let (second_id, second_prefix) = seed_key(&db, channel_id, "second", None).await;
    // Two models on the first key, one priced and one absent from the price
    // table, plus one request on the second key.
    log_priced_request(&db, first_id, channel_id, "2026-06-01 10:00:00").await;
    log_priced_request(&db, first_id, channel_id, "2026-06-02 10:00:00").await;
    log_at(
        &db,
        first_id,
        channel_id,
        Some("gpt-5-mini"),
        Some(100),
        Some(50),
        None,
        "2026-06-03 10:00:00",
    )
    .await;
    log_priced_request(&db, second_id, channel_id, "2026-06-04 10:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-06").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&first_prefix) && html.contains(&second_prefix));
    assert!(html.contains("gpt-4o") && html.contains("gpt-5-mini"));
    // The first key: two priced requests at 4000 and one unpriced request
    // that costs nothing.
    assert!(
        html.contains(">$0.008<"),
        "the first key total is 8000 micro-USD: {html}"
    );
    assert!(
        html.contains(">$0.00<"),
        "the gpt-5-mini row is unpriced and costs zero: {html}"
    );
    assert_eq!(
        html.matches(">total<").count(),
        2,
        "one total row per key: {html}"
    );
    assert!(html.contains(">All keys<"), "the grand total line renders");
    // Three priced requests at 4000 micro-USD each, the unpriced one adding
    // nothing: 12000 micro-USD across both keys.
    assert!(html.contains(">$0.012<"), "the grand total: {html}");
    // Request column: 2 + 1 on the first key, 1 on the second.
    assert!(html.contains(">3<") && html.contains(">1<"));
}

#[tokio::test]
async fn a_key_without_usage_in_the_month_is_omitted() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (used_id, used_prefix) = seed_key(&db, channel_id, "used", None).await;
    let (idle_id, idle_prefix) = seed_key(&db, channel_id, "idle", None).await;
    log_priced_request(&db, used_id, channel_id, "2026-07-01 10:00:00").await;
    assert!(idle_id > 0);

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-07").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&used_prefix));
    assert!(
        !html.contains(&idle_prefix),
        "a key without usage in the month is omitted: {html}"
    );
}

#[tokio::test]
async fn a_limited_key_on_an_unlimitable_channel_carries_the_warning_badge() {
    let db = test_pool().await;
    let limited_id = seed_unlimitable_channel(&db).await;
    let (key_id, _) = seed_key(&db, limited_id, "warned", Some(50_000_000)).await;
    log_priced_request(&db, key_id, limited_id, "2026-08-01 10:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-08").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("limit inactive: channel prices incomplete"),
        "the keys-panel warning wording must appear: {html}"
    );
    // The channel has no prices, so its usage costs nothing.
    assert!(html.contains(">$0.00<"));
}

#[tokio::test]
async fn a_priced_channel_never_shows_the_warning() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "enforced", Some(1)).await;
    log_priced_request(&db, key_id, channel_id, "2026-09-01 10:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-09").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !html.contains("limit inactive"),
        "a limitable channel needs no warning: {html}"
    );
}

#[tokio::test]
async fn a_deleted_key_keeps_its_rows_and_is_marked() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "gone", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-10-02 10:00:00").await;
    within(keys::delete(&Db::from(db.clone()), key_id))
        .await
        .unwrap();

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-10").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&prefix), "the deleted key keeps its usage");
    assert!(html.contains("<span class=\"badge deleted\">deleted</span>"));
}

#[tokio::test]
async fn requests_without_reported_usage_count_but_add_no_tokens() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "mixed", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-04-01 10:00:00").await;
    // A stream that reported no usage at all: it counts as a request and
    // contributes no tokens.
    log_at(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-04-02 10:00:00",
    )
    .await;

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-04").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(">2<"),
        "both requests count in the request column: {html}"
    );
    assert!(
        html.contains(">1,000<"),
        "only the reported request contributes tokens: {html}"
    );
    assert!(
        html.contains(">$0.004<"),
        "the usage-less request costs nothing: {html}"
    );
}

#[tokio::test]
async fn usage_rejected_before_a_key_resolved_is_attributed_to_no_key() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "real", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-03-01 10:00:00").await;
    // An unknown-key rejection: no key id, so it belongs to no row.
    within(request_logs::insert_at(
        &Db::from(db.clone()),
        &NewRequestLog {
            api_key_id: None,
            channel_id: None,
            model: None,
            status: Some(401),
            prompt_tokens: None,
            completion_tokens: None,
            cached_input_tokens: None,
            cached_output_tokens: None,
            cache_write_input_tokens: None,
            duration_ms: 1,
            request_body: None,
            response_body: None,
            error: Some("unknown key".to_string()),
            client_ip: None,
        },
        "2026-03-02 10:00:00",
    ))
    .await
    .unwrap();

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-03").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&prefix));
    assert!(
        html.contains(">$0.004<"),
        "only the resolved request is priced: {html}"
    );
    assert!(
        html.contains(">All keys<") && !html.contains("unknown key"),
        "the NULL-key rejection never becomes a row: {html}"
    );
}
// --- Cache writes ---------------------------------------------------------

/// A channel whitelisted to `claude` and priced for it with the cache-write
/// dimension set at 1.25 times the input rate.
async fn seed_write_priced_channel(db: &SqlitePool) -> i64 {
    let prices = ModelPrices::from_usd_json(&serde_json::json!({
        "claude": {
            "input": "2.50",
            "output": "10.00",
            "cached_input": "1.25",
            "cached_output": "5.00",
            "cache_write_input": "3.125"
        }
    }))
    .unwrap();
    within(channels::create_with_prices(
        &Db::from(db.clone()),
        &NewChannel {
            name: "write-priced".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: common::UPSTREAM_KEY.to_string(),
            allowed_models: Some(vec!["claude".to_string()]),
        },
        Some(&prices),
    ))
    .await
    .unwrap()
    .channel
    .id
}

#[tokio::test]
async fn the_cache_write_column_shows_the_written_tokens_and_their_cost() {
    let db = test_pool().await;
    let channel_id = seed_write_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "anthropic", None).await;
    // An Anthropic row: 1300 prompt tokens (1000 uncached, 250 read, 50
    // written) and 200 completions. At 2.50/1.25/3.125/10.00 USD per 1M
    // tokens the pinned formula charges 4968 micro-USD, against 4812 if the
    // write dimension were ignored.
    within(request_logs::insert_at(
        &Db::from(db.clone()),
        &NewRequestLog {
            api_key_id: Some(key_id),
            channel_id: Some(channel_id),
            model: Some("claude".to_string()),
            status: Some(200),
            prompt_tokens: Some(1300),
            completion_tokens: Some(200),
            cached_input_tokens: Some(250),
            cached_output_tokens: None,
            cache_write_input_tokens: Some(50),
            duration_ms: 1,
            request_body: None,
            response_body: None,
            error: None,
            client_ip: None,
        },
        "2026-08-01 10:00:00",
    ))
    .await
    .unwrap();

    let (status, html) = get_page(&db, "/admin/ui/stats?month=2026-08").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&prefix), "{html}");
    assert!(
        html.contains("<th>Cache write</th>"),
        "the column has its own header: {html}"
    );
    assert!(
        html.contains(">50<"),
        "the written tokens are shown: {html}"
    );
    assert!(
        html.contains(">1,300<"),
        "the prompt total still includes the writes: {html}"
    );
    assert!(
        html.contains(">$0.004968<"),
        "the cost is the write-aware figure: {html}"
    );
    assert!(
        !html.contains(">$0.004812<"),
        "the write-blind figure must not appear: {html}"
    );
}
