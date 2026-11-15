//! Integration tests for the admin UI stats pages: the auth gate, date-range
//! selection with its shortcuts, per key and per model grouping with summed
//! tokens and priced cost, the inactive-limit warning, the omission of keys
//! without usage, soft-deleted keys, rows whose upstream reported no usage,
//! the filters and charts of the Tokens page, the per-status-code chart and
//! table of the Requests page, the split between the two pages and the empty
//! and malformed range states.
//!
//! Rows are seeded through the database layer with explicit timestamps, and
//! the pages themselves are driven through the real router, so the tests pin
//! the rendered markup rather than any internal helper.

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

/// The `YYYY-MM` label of the current month, the prefix of the day range the
/// page defaults to.
fn current_month_label() -> String {
    String::from_utf8_lossy(&Month::current().bucket()).to_string()
}

/// The `YYYY-MM-DD` label of the current UTC day, the default `to` bound.
fn today_label() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// The rows of the rendered table, between its opening and closing tags. The
/// key and model filters list every key and every model of the range in the
/// select boxes above it, so an assertion that a key or a model has no row
/// has to look at the table rather than at the whole page.
fn table_rows(html: &str) -> &str {
    let (_, rest) = html.split_once("<table").expect("the stats table renders");
    let (table, _) = rest.split_once("</table>").expect("the table is closed");
    table
}

/// The filter bar markup, from its opening form to its closing tag, so an
/// assertion about the controls cannot be satisfied by the table below it.
fn filter_bar(html: &str) -> &str {
    let (_, rest) = html
        .split_once("<form class=\"filter-bar")
        .expect("the filter bar renders");
    let (bar, _) = rest
        .split_once("</form>")
        .expect("the filter bar is closed");
    bar
}

/// One entry of the top navigation, from its href through its closing tag, so
/// an assertion about its label or its active state cannot be satisfied by a
/// different entry of the same list.
fn nav_entry<'a>(html: &'a str, href: &str) -> &'a str {
    let needle = format!("href=\"{href}\"");
    let at = html
        .find(&needle)
        .unwrap_or_else(|| panic!("the nav must link {href}: {html}"));
    let end = html[at..].find("</a>").expect("the nav entry is closed");
    &html[at..at + end + "</a>".len()]
}

/// The chart panel, which is the part of the page after the table panel. A
/// page with nothing to plot has no second panel, so the returned text then
/// never carries an `aria-label` or a bar title.
fn chart_panel(html: &str) -> &str {
    match html.split_once("</section>") {
        Some((_, rest)) => rest,
        None => "",
    }
}

/// The status code panel, found by its heading. A page whose filters select
/// no request renders no such panel, so the returned text is then empty.
fn status_code_panel(html: &str) -> &str {
    match html.split_once("<h2>Status codes") {
        Some((_, rest)) => match rest.split_once("</section>") {
            Some((panel, _)) => panel,
            None => rest,
        },
        None => "",
    }
}

/// The `(code, class, requests, share)` cells of the status code table, in
/// the order the panel renders them.
fn status_code_rows(html: &str) -> Vec<[String; 4]> {
    let panel = status_code_panel(html);
    let body = panel
        .split_once("<tbody>")
        .expect("the status code table renders")
        .1;
    let body = body.split_once("</tbody>").expect("the table is closed").0;
    body.split("<tr>")
        .skip(1)
        .map(|row| {
            let mut cells = row.split("<td").skip(1).map(|cell| {
                cell.split_once('>')
                    .expect("a cell opens")
                    .1
                    .split_once("</td>")
                    .expect("a cell closes")
                    .0
                    .trim()
                    .to_string()
            });
            let code = cells.next().expect("a code cell");
            let class = cells.next().expect("a class cell");
            let requests = cells.next().expect("a requests cell");
            let share = cells.next().expect("a share cell");
            assert!(
                cells.next().is_none(),
                "the table carries four columns: {row}"
            );
            [code, class, requests, share]
        })
        .collect()
}

/// Whether the option carrying `value` is the selected one.
fn option_is_selected(html: &str, value: &str) -> bool {
    html.contains(&format!("<option value=\"{value}\" selected>"))
}

/// The markup of one filter select, so an assertion about a selected option
/// cannot be satisfied by the other select's own default.
fn select_markup<'a>(html: &'a str, id: &str) -> &'a str {
    let (_, rest) = html
        .split_once(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("the {id} select must render"));
    let (markup, _) = rest
        .split_once("</select>")
        .expect("the select must be closed");
    markup
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

/// One request log row with an explicit upstream status; `None` is a request
/// that never reached an upstream.
#[allow(clippy::too_many_arguments)]
async fn log_with_status(
    db: &SqlitePool,
    key_id: i64,
    channel_id: i64,
    model: Option<&str>,
    status: Option<i64>,
    prompt: Option<i64>,
    completion: Option<i64>,
    at: &str,
) {
    within(request_logs::insert_at(
        &Db::from(db.clone()),
        &NewRequestLog {
            api_key_id: Some(key_id),
            channel_id: Some(channel_id),
            model: model.map(str::to_string),
            status,
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_input_tokens: None,
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

// --- Auth gate and navigation --------------------------------------------

#[tokio::test]
async fn both_stats_pages_without_a_session_redirect_to_the_login_page() {
    let db = test_pool().await;
    for uri in ["/admin/ui/stats", "/admin/ui/stats/requests"] {
        let (status, body) = send(&db, uri, None).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert!(body.is_empty());
    }
}

#[tokio::test]
async fn the_tokens_page_renders_its_nav_entry_and_the_footnote() {
    let db = test_pool().await;
    let (status, html) = get_page(&db, "/admin/ui/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Token usage"), "{html}");
    let entry = nav_entry(&html, "/admin/ui/stats");
    assert!(
        entry.contains(">Tokens</a>"),
        "the entry is labeled: {entry}"
    );
    assert!(
        entry.contains("class=\"active\""),
        "the Tokens entry is the active one: {entry}"
    );
    let requests = nav_entry(&html, "/admin/ui/stats/requests");
    assert!(
        requests.contains(">Requests</a>"),
        "the Requests entry is next to it: {requests}"
    );
    assert!(
        !requests.contains("class=\"active\""),
        "exactly one entry is active: {requests}"
    );
    assert!(
        html.contains("Requests without reported usage are not counted"),
        "the NULL-usage footnote must be on the page"
    );
    assert!(html.contains("From (UTC)") && html.contains("To (UTC)"));
}

#[tokio::test]
async fn the_requests_page_renders_its_own_nav_entry_and_none_of_the_usage_content() {
    let db = test_pool().await;
    let (status, html) = get_page(&db, "/admin/ui/stats/requests").await;
    assert_eq!(status, StatusCode::OK);
    let entry = nav_entry(&html, "/admin/ui/stats/requests");
    assert!(
        entry.contains(">Requests</a>"),
        "the entry is labeled: {entry}"
    );
    assert!(
        entry.contains("class=\"active\""),
        "the Requests entry is the active one: {entry}"
    );
    let tokens = nav_entry(&html, "/admin/ui/stats");
    assert!(tokens.contains(">Tokens</a>"), "{tokens}");
    assert!(
        !tokens.contains("class=\"active\""),
        "exactly one entry is active: {tokens}"
    );
    assert!(
        !html.contains("Token usage") && !html.contains("Input tokens"),
        "the usage table and its heading stay on the Tokens page: {html}"
    );
    assert!(html.contains("From (UTC)") && html.contains("To (UTC)"));
    assert!(
        filter_bar(&html).contains("name=\"status_class\""),
        "the status class filter belongs to this page: {html}"
    );
}

// --- Date range selection -------------------------------------------------

#[tokio::test]
async fn the_tokens_filter_bar_offers_two_day_pickers_three_shortcuts_and_no_status_filter() {
    let db = test_pool().await;
    let (status, html) = get_page(&db, "/admin/ui/stats").await;
    assert_eq!(status, StatusCode::OK);
    let bar = filter_bar(&html);
    assert!(
        bar.contains("id=\"stats-from\" name=\"from\" value=\""),
        "the From day picker must be in the bar: {bar}"
    );
    assert!(
        bar.contains("type=\"date\""),
        "the day pickers are date inputs: {bar}"
    );
    assert!(
        bar.contains("id=\"stats-to\" name=\"to\" value=\""),
        "the To day picker must be in the bar: {bar}"
    );
    assert!(
        !bar.contains("type=\"month\"") && !bar.contains("name=\"month\""),
        "the month field is gone: {bar}"
    );
    assert!(
        !html.contains("Previous month") && !html.contains("Next month"),
        "the month navigation links are gone: {html}"
    );
    for (value, label) in [
        ("today", "Today"),
        ("week", "Current week"),
        ("month", "Current month"),
    ] {
        assert!(
            bar.contains(&format!(
                "<button type=\"submit\" name=\"preset\" value=\"{value}\">{label}</button>"
            )),
            "the {label} shortcut must submit preset={value}: {bar}"
        );
    }
    assert!(
        !bar.contains("name=\"status_class\""),
        "the status class filter is not a filter of this page: {bar}"
    );

    // Reading order: the two day fields, the shortcuts, the key and model
    // filters. The submit button is not in this list, because it leads the
    // markup although it reads last on screen; the test below pins that.
    let order = [
        "stats-from",
        "stats-to",
        "preset\" value=\"today\"",
        "preset\" value=\"week\"",
        "preset\" value=\"month\"",
        "stats-key",
        "stats-model",
    ];
    let mut previous = 0;
    for needle in order {
        let at = bar
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} must be in the bar: {bar}"));
        assert!(at >= previous, "{needle} is out of the bar's order: {bar}");
        previous = at;
    }
}

#[tokio::test]
async fn the_requests_filter_bar_is_the_same_bar_plus_the_status_class_filter() {
    let db = test_pool().await;
    let (status, html) = get_page(&db, "/admin/ui/stats/requests").await;
    assert_eq!(status, StatusCode::OK);
    let bar = filter_bar(&html);
    assert!(
        bar.contains("action=\"/admin/ui/stats/requests\""),
        "the bar submits to its own page: {bar}"
    );
    assert!(
        bar.contains("id=\"stats-from\" name=\"from\" value=\"")
            && bar.contains("id=\"stats-to\" name=\"to\" value=\""),
        "both day pickers are here: {bar}"
    );
    for (value, label) in [
        ("today", "Today"),
        ("week", "Current week"),
        ("month", "Current month"),
    ] {
        assert!(
            bar.contains(&format!(
                "<button type=\"submit\" name=\"preset\" value=\"{value}\">{label}</button>"
            )),
            "the {label} shortcut must submit preset={value}: {bar}"
        );
    }
    assert!(
        select_markup(&html, "stats-status").contains(">All statuses</option>"),
        "the class filter is offered: {html}"
    );

    let order = [
        "stats-from",
        "stats-to",
        "preset\" value=\"today\"",
        "preset\" value=\"week\"",
        "preset\" value=\"month\"",
        "stats-key",
        "stats-model",
        "stats-status",
    ];
    let mut previous = 0;
    for needle in order {
        let at = bar
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} must be in the bar: {bar}"));
        assert!(at >= previous, "{needle} is out of the bar's order: {bar}");
        previous = at;
    }
}

/// The markup of the first submit button of a form, which is the control a
/// browser activates when the user presses Enter in one of its fields.
fn first_submit_button(bar: &str) -> &str {
    let at = bar.find("<button").expect("the bar carries buttons");
    let (button, _) = bar[at..]
        .split_once("</button>")
        .expect("the first button is closed");
    button
}

#[tokio::test]
async fn enter_in_a_day_picker_applies_the_typed_range() {
    let db = test_pool().await;
    for uri in ["/admin/ui/stats", "/admin/ui/stats/requests"] {
        let (status, html) = get_page(&db, uri).await;
        assert_eq!(status, StatusCode::OK);
        let bar = filter_bar(&html);

        let apply = first_submit_button(bar);
        assert!(
            apply.contains(">Apply"),
            "the first submit button of the form is Apply, so Enter applies the typed range: {bar}"
        );
        assert!(
            !apply.contains("name=") && !apply.contains("value="),
            "Apply submits no parameter of its own, so Enter cannot fire a shortcut: {apply}"
        );
        assert!(
            !bar.contains("<script") && !bar.contains("onkeydown") && !bar.contains("onsubmit"),
            "the bar needs no script for Enter to apply the range: {bar}"
        );

        // The wrapper of the moved button leads the markup, and the sheet
        // orders it back to the end of the line, so the controls read exactly
        // as before.
        let apply_field = bar
            .find("filter-field--apply")
            .expect("the apply field is marked for ordering");
        let from_field = bar
            .find("id=\"stats-from\"")
            .expect("the From picker is in the bar");
        assert!(
            apply_field < from_field,
            "the apply field leads the bar's markup: {bar}"
        );
    }

    let (status, css) = send(&db, "/admin/ui/static/admin.css", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, rule) = css
        .split_once(".filter-field--apply")
        .expect("the sheet knows the apply field");
    let (rule, _) = rule.split_once('}').expect("the rule is closed");
    assert!(
        rule.contains("order: 10"),
        "the sheet moves the leading button to the end of the bar: {rule}"
    );
}

#[tokio::test]
async fn both_pages_default_to_the_current_month_to_date() {
    let db = test_pool().await;
    let month = current_month_label();
    let today = today_label();
    for (uri, heading) in [
        ("/admin/ui/stats", "Token usage"),
        ("/admin/ui/stats/requests", "Requests"),
    ] {
        let (status, html) = get_page(&db, uri).await;
        assert_eq!(status, StatusCode::OK);
        let bar = filter_bar(&html);
        assert!(
            bar.contains(&format!("name=\"from\" value=\"{month}-01\"")),
            "an empty from is the first day of the current month on {uri}: {bar}"
        );
        assert!(
            bar.contains(&format!("name=\"to\" value=\"{today}\"")),
            "an empty to is today on {uri}: {bar}"
        );
        assert!(
            html.contains(&format!(
                "{heading} <span class=\"text-muted\">{month}-01 to {today}"
            )),
            "the heading of {uri} names the days the view covers: {html}"
        );
    }
}

#[tokio::test]
async fn the_selected_range_drives_the_rows_and_leaves_other_days_out() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "jan", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-01-05 10:00:00").await;
    log_priced_request(&db, key_id, channel_id, "2026-02-05 10:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-01-01&to=2026-01-31").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&prefix), "{html}");
    assert!(
        filter_bar(&html).contains("name=\"from\" value=\"2026-01-01\""),
        "the from field echoes the URL: {html}"
    );
    // One request in the range: 1000 prompt, 200 completion, 400 cached
    // input.
    assert!(html.contains(">1,000<"), "{html}");
    assert!(html.contains(">400<"), "{html}");
    assert!(html.contains(">$0.004<"), "one priced request: {html}");

    // The February view carries its own numbers and does not leak January
    // into them, and the heading names its own days.
    let (_, february) = get_page(&db, "/admin/ui/stats?from=2026-02-01&to=2026-02-28").await;
    assert!(february.contains(">$0.004<"));
    assert!(february.contains("2026-02-01 to 2026-02-28"), "{february}");
    assert!(!february.contains("2026-01-01 to 2026-01-31"));

    // A range before the logged rows renders the empty state, with the key
    // still offered by the filter select but carrying no row.
    let (_, empty) = get_page(&db, "/admin/ui/stats?from=2026-05-01&to=2026-05-31").await;
    assert!(
        empty.contains("No requests logged between 2026-05-01 and 2026-05-31"),
        "{empty}"
    );
    assert!(
        !table_rows(&empty).contains(&prefix),
        "no rows for a range without usage"
    );
    assert!(!empty.contains("<svg"), "no data means no chart: {empty}");
}
#[tokio::test]
async fn the_requests_page_reads_the_same_range_as_the_tokens_page() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "ranged", None).await;
    log_with_status(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        Some(10),
        Some(1),
        "2026-01-05 10:00:00",
    )
    .await;
    log_with_status(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(500),
        Some(10),
        Some(1),
        "2026-02-12 10:00:00",
    )
    .await;

    let (status, january) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-01-01&to=2026-01-31",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(january.contains("2026-01-01 to 2026-01-31"), "{january}");
    let charts = chart_panel(&january);
    assert!(
        charts.contains("Day 5: 1 requests (1 200)"),
        "the range picks the January request alone: {charts}"
    );
    assert!(
        !charts.contains("Day 12:"),
        "the February request stays outside the range: {charts}"
    );
    assert_eq!(
        status_code_rows(&january),
        vec![[
            "200".to_string(),
            "2xx".to_string(),
            "1".to_string(),
            "100.0%".to_string()
        ]],
        "{january}"
    );

    // The key select of this page lists the keys of the range like the other
    // page does.
    assert!(
        select_markup(&january, "stats-key").contains(&prefix),
        "the key filter is offered here too: {january}"
    );

    // The shortcuts fill the day pickers of this page as well.
    let (_, shortcut) = get_page(&db, "/admin/ui/stats/requests?preset=today").await;
    let today = today_label();
    assert!(
        filter_bar(&shortcut).contains(&format!("name=\"from\" value=\"{today}\"")),
        "the shortcut fills the day fields: {shortcut}"
    );
    assert!(
        filter_bar(&shortcut).contains(&format!("name=\"to\" value=\"{today}\"")),
        "{shortcut}"
    );
}

#[tokio::test]
async fn a_range_that_cuts_through_a_month_keeps_its_own_days() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "cut", None).await;
    // Two requests inside the range, the second one late on the inclusive
    // `to` day, and one in the first second after it.
    log_priced_request(&db, key_id, channel_id, "2026-06-03 10:00:00").await;
    log_priced_request(&db, key_id, channel_id, "2026-06-10 23:59:59").await;
    log_priced_request(&db, key_id, channel_id, "2026-06-11 00:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-06-01&to=2026-06-10").await;
    assert_eq!(status, StatusCode::OK);
    let table = table_rows(&html);
    assert!(
        table.contains(">2,000<") && table.contains(">$0.008<"),
        "the two requests inside the range are the whole table: {table}"
    );
    assert!(
        !table.contains(">3,000<") && !table.contains(">$0.012<"),
        "the request of the day after `to` stays out: {table}"
    );
    let charts = chart_panel(&html);
    assert!(charts.contains("Day 3:"), "{charts}");
    assert!(
        charts.contains("Day 10:"),
        "the to day itself is plotted: {charts}"
    );
    assert!(
        !charts.contains("Day 11:"),
        "the day after the to day is not: {charts}"
    );

    // A single-day range covers exactly that day.
    let (_, one_day) = get_page(&db, "/admin/ui/stats?from=2026-06-03&to=2026-06-03").await;
    assert!(one_day.contains(">$0.004<"), "{one_day}");
    assert!(!one_day.contains(">$0.008<"), "{one_day}");
}

#[tokio::test]
async fn a_shortcut_fills_the_day_pickers_and_decides_the_rows() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "short", None).await;
    // One request today and one on the first day of the current month: the
    // day shortcut sees the first alone, the month shortcut both.
    log_priced_request(
        &db,
        key_id,
        channel_id,
        &format!("{} 10:00:00", today_label()),
    )
    .await;
    log_priced_request(
        &db,
        key_id,
        channel_id,
        &format!("{}-01 00:00:01", current_month_label()),
    )
    .await;

    let (status, html) = get_page(&db, "/admin/ui/stats?preset=today").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&prefix), "{html}");
    let bar = filter_bar(&html);
    let today = today_label();
    assert!(
        bar.contains(&format!("name=\"from\" value=\"{today}\""))
            && bar.contains(&format!("name=\"to\" value=\"{today}\"")),
        "the shortcut fills both day fields: {bar}"
    );
    assert!(
        !table_rows(&html).contains(">$0.008<"),
        "one request falls on today: {html}"
    );
    assert!(html.contains(&format!("{today} to {today}")), "{html}");

    let (_, month) = get_page(&db, "/admin/ui/stats?preset=month").await;
    let bar = filter_bar(&month);
    assert!(
        bar.contains(&format!(
            "name=\"from\" value=\"{}-01\"",
            current_month_label()
        )),
        "the month shortcut starts on the 1st: {bar}"
    );
    let table = table_rows(&month);
    assert!(
        table.contains(">$0.008<"),
        "both requests of the month are in the table: {table}"
    );
}

#[tokio::test]
async fn a_malformed_day_falls_back_to_the_current_month_behind_a_banner() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "now", None).await;
    // A request inside the current month, built from the same label the page
    // defaults to.
    let now = format!("{}-01 10:00:00", current_month_label());
    log_priced_request(&db, key_id, channel_id, &now).await;

    let (status, html) = get_page(&db, "/admin/ui/stats?from=January").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("is not a YYYY-MM-DD date"),
        "the banner must explain the expected shape: {html}"
    );
    assert!(
        html.contains("January"),
        "the banner must name the offending value: {html}"
    );
    assert!(html.contains(&prefix), "the current month still renders");
    assert!(filter_bar(&html).contains(&format!("value=\"{}-01\"", current_month_label())));
}

#[tokio::test]
async fn an_inverted_range_shows_a_banner_and_renders_the_current_month() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "inverted", None).await;
    log_priced_request(
        &db,
        key_id,
        channel_id,
        &format!("{} 10:00:00", today_label()),
    )
    .await;

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-06-09&to=2026-06-01").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("is after"), "{html}");
    assert!(
        html.contains("2026-06-09") && html.contains("2026-06-01"),
        "{html}"
    );
    let bar = filter_bar(&html);
    assert!(
        bar.contains(&format!(
            "name=\"from\" value=\"{}-01\"",
            current_month_label()
        )) && bar.contains(&format!("name=\"to\" value=\"{}\"", today_label())),
        "the default range is rendered instead: {bar}"
    );
    assert!(
        table_rows(&html).contains(">$0.004<"),
        "the current month's usage is still there: {html}"
    );
}

#[tokio::test]
async fn an_unknown_shortcut_shows_a_banner_and_no_shortcut_takes_effect() {
    let db = test_pool().await;
    let (status, html) = get_page(&db, "/admin/ui/stats?preset=yesterday").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("is not one of today, week, month"),
        "the banner lists the accepted shortcuts: {html}"
    );
    assert!(html.contains("yesterday"), "{html}");
    let bar = filter_bar(&html);
    assert!(
        bar.contains(&format!("name=\"to\" value=\"{}\"", today_label())),
        "the default range still renders: {bar}"
    );
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

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-06-01&to=2026-06-30").await;
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
async fn a_key_without_usage_in_the_range_is_omitted() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (used_id, used_prefix) = seed_key(&db, channel_id, "used", None).await;
    let (idle_id, idle_prefix) = seed_key(&db, channel_id, "idle", None).await;
    log_priced_request(&db, used_id, channel_id, "2026-07-01 10:00:00").await;
    assert!(idle_id > 0);

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-07-01&to=2026-07-31").await;
    assert_eq!(status, StatusCode::OK);
    let table = table_rows(&html);
    assert!(table.contains(&used_prefix));
    assert!(
        !table.contains(&idle_prefix),
        "a key without usage in the range has no row: {table}"
    );
    assert!(
        html.contains(&format!("<option value=\"{idle_id}\">")),
        "the idle key is still offered by the filter: {html}"
    );
}

#[tokio::test]
async fn a_limited_key_on_an_unlimitable_channel_carries_the_warning_badge() {
    let db = test_pool().await;
    let limited_id = seed_unlimitable_channel(&db).await;
    let (key_id, _) = seed_key(&db, limited_id, "warned", Some(50_000_000)).await;
    log_priced_request(&db, key_id, limited_id, "2026-08-01 10:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-08-01&to=2026-08-31").await;
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

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-09-01&to=2026-09-30").await;
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

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-10-01&to=2026-10-31").await;
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

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-04-01&to=2026-04-30").await;
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

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-03-01&to=2026-03-31").await;
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

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-08-01&to=2026-08-31").await;
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

// --- Filters and charts ---------------------------------------------------

#[tokio::test]
async fn the_key_filter_narrows_the_table_and_keeps_the_range() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (first_id, first_prefix) = seed_key(&db, channel_id, "kept", None).await;
    let (second_id, second_prefix) = seed_key(&db, channel_id, "dropped", None).await;
    log_priced_request(&db, first_id, channel_id, "2026-06-01 10:00:00").await;
    log_priced_request(&db, second_id, channel_id, "2026-06-02 10:00:00").await;

    let (status, html) = get_page(
        &db,
        &format!("/admin/ui/stats?from=2026-06-01&to=2026-06-30&key_id={first_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let table = table_rows(&html);
    assert!(table.contains(&first_prefix), "{table}");
    assert!(
        !table.contains(&second_prefix),
        "the other key is filtered out of the table: {table}"
    );
    assert!(
        table.contains(">$0.004<"),
        "the totals follow the filter, so one priced request is the grand total: {table}"
    );
    assert!(
        option_is_selected(&html, &first_id.to_string()),
        "the filtered key stays selected in the control: {html}"
    );
    assert!(
        filter_bar(&html).contains("name=\"from\" value=\"2026-06-01\""),
        "the range survives the filter: {html}"
    );
    assert!(
        select_markup(&html, "stats-key").contains(&format!("<option value=\"{second_id}\">")),
        "the dropped key is still offered, so the filter can be widened: {html}"
    );

    // Without the filter both keys are back.
    let (_, unfiltered) = get_page(&db, "/admin/ui/stats?from=2026-06-01&to=2026-06-30").await;
    assert!(table_rows(&unfiltered).contains(&second_prefix));
}

#[tokio::test]
async fn the_model_filter_narrows_the_table_and_the_charts() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "models", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-06-01 10:00:00").await;
    log_at(
        &db,
        key_id,
        channel_id,
        Some("gpt-5-mini"),
        Some(100),
        Some(50),
        None,
        "2026-06-03 10:00:00",
    )
    .await;

    let (status, html) = get_page(
        &db,
        "/admin/ui/stats?from=2026-06-01&to=2026-06-30&model=gpt-5-mini",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let table = table_rows(&html);
    assert!(table.contains("gpt-5-mini"), "{table}");
    assert!(
        !table.contains("gpt-4o"),
        "the other model's line is filtered out: {table}"
    );
    assert!(
        table.contains(">$0.00<"),
        "the surviving model is outside the price table and costs zero: {table}"
    );
    assert!(
        option_is_selected(&html, "gpt-5-mini"),
        "the filtered model stays selected: {html}"
    );
    // The charts plot the filtered day rows: the surviving request's day
    // keeps its bar and the filtered-out day leaves none.
    let charts = chart_panel(&html);
    assert!(charts.contains("Day 3:"), "{charts}");
    assert!(
        !charts.contains("Day 1:"),
        "a filtered-out day draws no bar: {charts}"
    );
    // The control still lists the whole range, so the filter can be widened,
    // and it does not fall back to its "All models" default.
    let models = select_markup(&html, "stats-model");
    assert!(models.contains("<option value=\"gpt-4o\">"), "{html}");
    assert!(
        !models.contains("<option value=\"\" selected>"),
        "the filter in force is not hidden behind All models: {html}"
    );
}

#[tokio::test]
async fn the_charts_plot_the_range_and_agree_with_the_table() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "plot", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-06-01 10:00:00").await;

    let (status, html) = get_page(&db, "/admin/ui/stats?from=2026-06-01&to=2026-06-30").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Usage over time"), "{html}");
    let charts = chart_panel(&html);
    assert!(charts.contains("aria-label=\"Tokens per day\""), "{charts}");
    assert!(charts.contains("aria-label=\"Cost per day\""), "{charts}");
    assert!(
        charts.contains("Day 1: 1,200 tokens (1,000 in, 200 out) in 1 requests"),
        "the token bar carries the day's figures: {charts}"
    );
    // 1000 prompt tokens with 400 of them cached and 200 completions cost
    // 4000 micro-USD, the same figure the table's total line carries.
    assert!(charts.contains("Day 1: $0.004"), "{charts}");
    assert!(table_rows(&html).contains(">$0.004<"), "{html}");

    // A filter that selects nothing says so, keeps the empty styling and
    // draws no chart at all.
    let (status, filtered_out) = get_page(
        &db,
        "/admin/ui/stats?from=2026-06-01&to=2026-06-30&model=nope",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        filtered_out
            .contains("No usage matches the current filters between 2026-06-01 and 2026-06-30"),
        "{filtered_out}"
    );
    assert!(!filtered_out.contains("<svg"), "{filtered_out}");
    assert!(
        option_is_selected(&filtered_out, "nope"),
        "the selection is visible even when it matches nothing: {filtered_out}"
    );
}

#[tokio::test]
async fn a_soft_deleted_key_stays_selectable_in_the_key_filter() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "gone", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-10-02 10:00:00").await;
    within(keys::delete(&Db::from(db.clone()), key_id))
        .await
        .unwrap();

    let (status, html) = get_page(
        &db,
        &format!("/admin/ui/stats?from=2026-10-01&to=2026-10-31&key_id={key_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        select_markup(&html, "stats-key").contains(&prefix),
        "the deleted key keeps its option: {html}"
    );
    assert!(
        option_is_selected(&html, &key_id.to_string()),
        "and it is the option the filter selects: {html}"
    );
    let table = table_rows(&html);
    assert!(
        table.contains("<span class=\"badge deleted\">deleted</span>"),
        "its usage still renders under the filter: {table}"
    );
}

#[tokio::test]
async fn an_unparsable_key_id_shows_a_banner_and_filters_nothing() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, prefix) = seed_key(&db, channel_id, "kept", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-06-01 10:00:00").await;

    let (status, html) = get_page(
        &db,
        "/admin/ui/stats?from=2026-06-01&to=2026-06-30&key_id=abc",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("is not a row id"), "{html}");
    assert!(html.contains("abc"), "the banner names the value: {html}");
    assert!(
        table_rows(&html).contains(&prefix),
        "the view stays unfiltered: {html}"
    );
}

// --- Status codes and classes ---------------------------------------------

#[tokio::test]
async fn the_requests_chart_stacks_every_exact_status_code_of_the_range() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "codes", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-06-01 10:00:00").await;
    log_with_status(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(500),
        Some(100),
        Some(10),
        "2026-06-01 11:00:00",
    )
    .await;

    let (status, html) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Requests per day"), "{html}");
    let charts = chart_panel(&html);
    assert!(
        charts.contains("aria-label=\"Requests per day\""),
        "{charts}"
    );
    assert!(
        charts.contains("Day 1: 2 requests (1 200, 1 500)"),
        "the bar splits the day by code: {charts}"
    );
    for code in ["200", "500"] {
        assert!(
            charts.contains(&format!(">{code}</text>")),
            "the legend names {code}: {charts}"
        );
    }
}

#[tokio::test]
async fn the_status_filter_narrows_the_whole_requests_page() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "narrowed", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-06-01 10:00:00").await;
    log_with_status(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        Some(500),
        Some(100),
        Some(10),
        "2026-06-01 11:00:00",
    )
    .await;

    let (status, html) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30&status_class=5xx",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let charts = chart_panel(&html);
    assert!(
        charts.contains("Day 1: 1 requests (1 500)"),
        "the requests chart follows the filter: {charts}"
    );
    assert!(
        option_is_selected(&html, "5xx"),
        "the filter in force is visible: {html}"
    );
    // The status code table of the same page follows the same filter, so the
    // page never shows two different selections.
    assert_eq!(
        status_code_rows(&html),
        vec![[
            "500".to_string(),
            "5xx".to_string(),
            "1".to_string(),
            "100.0%".to_string()
        ]],
        "{html}"
    );

    // Without the filter both requests of the day are back, chart and table
    // alike.
    let (_, unfiltered) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30",
    )
    .await;
    assert!(chart_panel(&unfiltered).contains("Day 1: 2 requests (1 200, 1 500)"));
    assert_eq!(status_code_rows(&unfiltered).len(), 2, "{unfiltered}");
}

#[tokio::test]
async fn the_tokens_page_has_no_status_filter_to_honour() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "linked", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-06-01 10:00:00").await;

    let (status, html) = get_page(
        &db,
        "/admin/ui/stats?from=2026-06-01&to=2026-06-30&status_class=errors",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !filter_bar(&html).contains("status_class"),
        "the bar of this page carries no status filter: {html}"
    );
    assert!(
        !html.contains("aria-label=\"Requests per day\"") && !html.contains("Status codes"),
        "the requests chart and the status code table live on the other page: {html}"
    );
    assert!(
        !html.contains("is not one of 2xx, 4xx, 5xx, other"),
        "a parameter this page does not read is not reported as a bad filter: {html}"
    );
    assert!(
        filter_bar(&html).contains("name=\"from\" value=\"2026-06-01\""),
        "the range survives the unused parameter: {html}"
    );
    // Nothing on this page is narrowed by it, so the request it names stays
    // in the table and in both charts.
    assert!(html.contains("aria-label=\"Tokens per day\""), "{html}");
    assert!(html.contains("aria-label=\"Cost per day\""), "{html}");
    let table = table_rows(&html);
    assert!(table.contains(">$0.004<"), "{table}");
}

#[tokio::test]
async fn a_request_that_never_reached_an_upstream_is_listed_without_a_code() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "failed", None).await;
    log_with_status(
        &db,
        key_id,
        channel_id,
        Some("gpt-4o"),
        None,
        None,
        None,
        "2026-06-02 10:00:00",
    )
    .await;

    let (status, html) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let charts = chart_panel(&html);
    assert!(
        charts.contains("Day 2: 1 requests (1 n/a)"),
        "the unlabelled request is its own segment: {charts}"
    );
    assert!(
        charts.contains(">n/a (other)</text>"),
        "the legend names the bucket those requests land in: {charts}"
    );
    assert_eq!(
        status_code_rows(&html),
        vec![[
            "n/a".to_string(),
            "other".to_string(),
            "1".to_string(),
            "100.0%".to_string()
        ]],
        "{html}"
    );

    let (_, other) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30&status_class=other",
    )
    .await;
    assert!(
        chart_panel(&other).contains("Day 2: 1 requests (1 n/a)"),
        "the other class is selectable: {other}"
    );

    let (_, success) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30&status_class=2xx",
    )
    .await;
    assert!(
        !success.contains("aria-label=\"Requests per day\""),
        "a class the request does not belong to draws no bar: {success}"
    );
    assert!(
        !success.contains("<table"),
        "and no status code table either: {success}"
    );
    assert!(
        success.contains("No requests match the current filters"),
        "the page says why it is empty: {success}"
    );
}

#[tokio::test]
async fn the_status_code_table_lists_every_code_of_the_period_with_its_share() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "shares", None).await;
    // Three 200s, one 404 and one request that never reached an upstream: the
    // shares are 60, 20 and 20 percent of the five requests of the period,
    // the busiest code first.
    for (status, at) in [
        (Some(200), "2026-06-01 10:00:00"),
        (Some(200), "2026-06-01 11:00:00"),
        (Some(200), "2026-06-02 10:00:00"),
        (Some(404), "2026-06-02 11:00:00"),
        (None, "2026-06-03 10:00:00"),
    ] {
        log_with_status(
            &db,
            key_id,
            channel_id,
            Some("gpt-4o"),
            status,
            Some(10),
            Some(1),
            at,
        )
        .await;
    }

    let (status, html) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Status codes"), "{html}");
    let rows = status_code_rows(&html);
    assert_eq!(
        rows,
        vec![
            [
                "200".to_string(),
                "2xx".to_string(),
                "3".to_string(),
                "60.0%".to_string()
            ],
            [
                "404".to_string(),
                "4xx".to_string(),
                "1".to_string(),
                "20.0%".to_string()
            ],
            [
                "n/a".to_string(),
                "other".to_string(),
                "1".to_string(),
                "20.0%".to_string()
            ],
        ],
        "{html}"
    );
    let summed: f64 = rows
        .iter()
        .map(|row| row[3].trim_end_matches('%').parse::<f64>().unwrap())
        .sum();
    assert!(
        (summed - 100.0).abs() < 0.2,
        "the shares cover the whole period: {summed}"
    );

    // The requests chart of the same view stacks the same codes.
    let charts = chart_panel(&html);
    assert!(charts.contains("Day 1: 2 requests (2 200)"), "{charts}");
    assert!(
        charts.contains("Day 2: 2 requests (1 200, 1 404)"),
        "{charts}"
    );
    assert!(charts.contains("Day 3: 1 requests (1 n/a)"), "{charts}");

    // A range without requests renders no status code panel at all.
    let (_, empty) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-04-01&to=2026-04-30",
    )
    .await;
    assert!(!empty.contains("Status codes"), "{empty}");
}

#[tokio::test]
async fn the_status_code_table_follows_the_key_model_and_status_filters() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (first_id, _) = seed_key(&db, channel_id, "first", None).await;
    let (second_id, _) = seed_key(&db, channel_id, "second", None).await;
    log_with_status(
        &db,
        first_id,
        channel_id,
        Some("gpt-4o"),
        Some(200),
        Some(10),
        Some(1),
        "2026-06-01 10:00:00",
    )
    .await;
    log_with_status(
        &db,
        first_id,
        channel_id,
        Some("gpt-5-mini"),
        Some(500),
        Some(10),
        Some(1),
        "2026-06-01 11:00:00",
    )
    .await;
    log_with_status(
        &db,
        second_id,
        channel_id,
        Some("gpt-4o"),
        Some(429),
        Some(10),
        Some(1),
        "2026-06-01 12:00:00",
    )
    .await;

    let (status, html) = get_page(
        &db,
        &format!("/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30&key_id={first_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = status_code_rows(&html);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][0], "200");
    assert_eq!(
        rows[0][3], "50.0%",
        "the share divides by the requests the filter selects: {rows:?}"
    );
    assert_eq!(rows[1][0], "500");
    assert_eq!(rows[1][1], "5xx");

    // The model filter narrows the codes and the total the shares divide by.
    let (_, model) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30&model=gpt-5-mini",
    )
    .await;
    assert_eq!(
        status_code_rows(&model),
        vec![[
            "500".to_string(),
            "5xx".to_string(),
            "1".to_string(),
            "100.0%".to_string()
        ]],
        "{model}"
    );

    // The class filter keeps the codes of that class alone.
    let (_, class) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30&status_class=4xx",
    )
    .await;
    assert_eq!(
        status_code_rows(&class),
        vec![[
            "429".to_string(),
            "4xx".to_string(),
            "1".to_string(),
            "100.0%".to_string()
        ]],
        "{class}"
    );
}

#[tokio::test]
async fn an_unknown_status_class_shows_a_banner_and_filters_nothing() {
    let db = test_pool().await;
    let channel_id = seed_priced_channel(&db).await;
    let (key_id, _) = seed_key(&db, channel_id, "bogus", None).await;
    log_priced_request(&db, key_id, channel_id, "2026-06-01 10:00:00").await;

    // `errors` is a bucket of the log listing, not a class of the chart.
    let (status, html) = get_page(
        &db,
        "/admin/ui/stats/requests?from=2026-06-01&to=2026-06-30&status_class=errors",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("is not one of 2xx, 4xx, 5xx, other"),
        "the banner lists the accepted values: {html}"
    );
    assert!(
        html.contains("errors"),
        "the banner names the value: {html}"
    );
    assert!(
        html.contains("aria-label=\"Requests per day\""),
        "the view stays unfiltered: {html}"
    );
    assert_eq!(
        status_code_rows(&html),
        vec![[
            "200".to_string(),
            "2xx".to_string(),
            "1".to_string(),
            "100.0%".to_string()
        ]],
        "{html}"
    );
    assert!(
        select_markup(&html, "stats-status").contains("<option value=\"\" selected>"),
        "no filter is in force: {html}"
    );
}
