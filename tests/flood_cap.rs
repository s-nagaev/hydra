//! Integration tests for the per-IP flood cap on anonymous 401 spam:
//! failures below the limit are all logged normally, the capped phase
//! answers the same 401 without the database and contributes at most one
//! `request_logs` row per window per IP, a parallel burst cannot slip past
//! the atomic gate, IPs are independent, and the counter expires with its
//! window.

mod common;

use std::time::{Duration, Instant};

use futures_util::future::join_all;
use sqlx::{Row, SqlitePool};

use common::{test_app_with_flood, test_app_with_flood_and_trusted_proxies, test_pool, within};
use hydra::config::TrustedProxies;

const LOOPBACK_TRUSTED: &str = "127.0.0.1/32";
const BAD_KEY: &str = "hyd-not-a-real-key";

/// A flood row's interesting columns, read back through a runtime query.
struct FloodRow {
    status: Option<i64>,
    error: Option<String>,
    client_ip: Option<String>,
    api_key_id: Option<i64>,
}

async fn fetch_rows(db: &SqlitePool) -> Vec<FloodRow> {
    within(
        sqlx::query(
            "SELECT status, error, client_ip, api_key_id \
             FROM request_logs ORDER BY id",
        )
        .fetch_all(db),
    )
    .await
    .unwrap()
    .iter()
    .map(|row| FloodRow {
        status: row.try_get("status").unwrap(),
        error: row.try_get("error").unwrap(),
        client_ip: row.try_get("client_ip").unwrap(),
        api_key_id: row.try_get("api_key_id").unwrap(),
    })
    .collect()
}

/// The log inserts run detached; poll until `count` rows are visible.
async fn wait_for_rows(db: &SqlitePool, count: usize) -> Vec<FloodRow> {
    let deadline = Instant::now() + common::TEST_TIMEOUT;
    loop {
        let rows = fetch_rows(db).await;
        if rows.len() >= count {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} request_logs rows, got {}",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Sends one chat request with a bogus key through the served router (so
/// ConnectInfo is present, which the flood gate requires), with an optional
/// X-Forwarded-For header.
async fn send_bad_chat(base_url: &str, xff: Option<&str>) -> reqwest::StatusCode {
    let mut request = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .header("authorization", format!("Bearer {BAD_KEY}"))
        .header("content-type", "application/json")
        .body("{}");
    if let Some(xff) = xff {
        request = request.header("x-forwarded-for", xff);
    }
    let response = within(request.send()).await.expect("request completes");
    let status = response.status();
    let _ = within(async { response.bytes().await }).await;
    status
}

/// The same anonymous spam against the second proxied POST surface, which
/// runs the same extractor and therefore the same flood gate.
async fn send_bad_responses(base_url: &str) -> reqwest::StatusCode {
    let response = within(
        reqwest::Client::new()
            .post(format!("{base_url}/v1/responses"))
            .header("authorization", format!("Bearer {BAD_KEY}"))
            .header("content-type", "application/json")
            .body("{}")
            .send(),
    )
    .await
    .expect("request completes");
    let status = response.status();
    let _ = within(async { response.bytes().await }).await;
    status
}

/// The same anonymous spam against the third proxied POST surface, which
/// authenticates through its own extractor and therefore shares the same
/// flood gate; the key rides the Messages credential header here.
async fn send_bad_messages(base_url: &str) -> reqwest::StatusCode {
    let response = within(
        reqwest::Client::new()
            .post(format!("{base_url}/v1/messages"))
            .header("x-api-key", BAD_KEY)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body("{}")
            .send(),
    )
    .await
    .expect("request completes");
    let status = response.status();
    let _ = within(async { response.bytes().await }).await;
    status
}

#[tokio::test]
async fn failures_below_the_limit_are_all_logged_normally() {
    let db = test_pool().await;
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood(db.clone(), 3, 60)).await;

    for _ in 0..3 {
        assert_eq!(
            send_bad_chat(&base, None).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    let rows = wait_for_rows(&db, 3).await;
    assert_eq!(rows.len(), 3, "each below-limit failure gets its own row");
    for row in &rows {
        assert_eq!(row.status, Some(401));
        assert_eq!(row.error.as_deref(), Some("Invalid API key provided"));
        assert_eq!(row.client_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(row.api_key_id, None, "unknown keys resolve to no key row");
    }
}

#[tokio::test]
async fn capped_requests_answer_401_without_touching_the_database() {
    let db = test_pool().await;
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood(db.clone(), 1, 60)).await;

    assert_eq!(
        send_bad_chat(&base, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    wait_for_rows(&db, 1).await;

    // Pool swap per repo convention: with the pool closed, any database
    // access would fail, so the 401s below can only come from the in-memory
    // flood gate. (Their flood-phase log row is then lost, which the
    // one-row-per-window test covers with a live pool.)
    db.close().await;
    for _ in 0..3 {
        assert_eq!(
            send_bad_chat(&base, None).await,
            reqwest::StatusCode::UNAUTHORIZED,
            "capped requests must answer 401 without the database"
        );
    }
}

#[tokio::test]
async fn capped_phase_writes_at_most_one_row_per_window() {
    let db = test_pool().await;
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood(db.clone(), 1, 60)).await;

    // Failure 1: processed normally. Failures 2..4: capped phase.
    for _ in 0..4 {
        assert_eq!(
            send_bad_chat(&base, None).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    let rows = wait_for_rows(&db, 2).await;
    assert_eq!(rows.len(), 2, "one normal row plus exactly one capped row");
    assert_eq!(rows[0].error.as_deref(), Some("Invalid API key provided"));
    assert_eq!(
        rows[1].error.as_deref(),
        Some("flood cap: too many failed authentications from this address")
    );
    assert_eq!(rows[1].status, Some(401));
    assert_eq!(rows[1].api_key_id, None);

    // Further capped requests in the same window add no rows.
    assert_eq!(
        send_bad_chat(&base, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fetch_rows(&db).await.len(),
        2,
        "capped requests must not log more than one row per window"
    );
}

#[tokio::test]
async fn the_flood_cap_covers_the_responses_route() {
    let db = test_pool().await;
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood(db.clone(), 1, 60)).await;

    // Failure 1: processed normally. Failures 2..3: capped phase.
    for _ in 0..3 {
        assert_eq!(
            send_bad_responses(&base).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    let rows = wait_for_rows(&db, 2).await;
    assert_eq!(rows.len(), 2, "one normal row plus exactly one capped row");
    assert_eq!(rows[0].error.as_deref(), Some("Invalid API key provided"));
    assert_eq!(
        rows[1].error.as_deref(),
        Some("flood cap: too many failed authentications from this address"),
        "the capped phase contributes its single row on this surface too"
    );
    assert_eq!(rows[1].status, Some(401));
    assert_eq!(rows[1].api_key_id, None);
}

#[tokio::test]
async fn the_flood_cap_covers_the_messages_route() {
    let db = test_pool().await;
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood(db.clone(), 1, 60)).await;

    // Failure 1: processed normally. Failures 2..3: capped phase.
    for _ in 0..3 {
        assert_eq!(
            send_bad_messages(&base).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    let rows = wait_for_rows(&db, 2).await;
    assert_eq!(rows.len(), 2, "one normal row plus exactly one capped row");
    assert_eq!(rows[0].error.as_deref(), Some("Invalid API key provided"));
    assert_eq!(
        rows[1].error.as_deref(),
        Some("flood cap: too many failed authentications from this address"),
        "the capped phase contributes its single row on this surface too"
    );
    assert_eq!(rows[1].status, Some(401));
    assert_eq!(rows[1].api_key_id, None);
}

#[tokio::test]
async fn different_ips_are_capped_independently() {
    let db = test_pool().await;
    let trusted = TrustedProxies::parse(LOOPBACK_TRUSTED).unwrap();
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood_and_trusted_proxies(
        db.clone(),
        1,
        60,
        trusted,
    ))
    .await;

    // 198.51.100.5 exhausts its budget; 203.0.113.7 must be unaffected.
    assert_eq!(
        send_bad_chat(&base, Some("198.51.100.5")).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send_bad_chat(&base, Some("203.0.113.7")).await,
        reqwest::StatusCode::UNAUTHORIZED,
        "a different resolved IP must not be capped"
    );
    assert_eq!(
        send_bad_chat(&base, Some("198.51.100.5")).await,
        reqwest::StatusCode::UNAUTHORIZED
    );

    let rows = wait_for_rows(&db, 3).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].client_ip.as_deref(), Some("198.51.100.5"));
    assert_eq!(rows[1].client_ip.as_deref(), Some("203.0.113.7"));
    assert_eq!(
        rows[2].error.as_deref(),
        Some("flood cap: too many failed authentications from this address"),
        "the exhausted IP enters the capped phase"
    );
    assert_eq!(rows[2].client_ip.as_deref(), Some("198.51.100.5"));
}

#[tokio::test]
async fn counter_expires_after_the_window() {
    let db = test_pool().await;
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood(db.clone(), 1, 1)).await;

    assert_eq!(
        send_bad_chat(&base, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send_bad_chat(&base, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    let rows = wait_for_rows(&db, 2).await;
    assert_eq!(rows[0].error.as_deref(), Some("Invalid API key provided"));
    assert_eq!(
        rows[1].error.as_deref(),
        Some("flood cap: too many failed authentications from this address")
    );

    std::thread::sleep(Duration::from_millis(1100));

    // The window rolled: the IP is processed (and logged) normally again.
    assert_eq!(
        send_bad_chat(&base, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    let rows = wait_for_rows(&db, 3).await;
    assert_eq!(
        rows[2].error.as_deref(),
        Some("Invalid API key provided"),
        "after expiry the failure is processed normally again"
    );
    assert_eq!(rows[2].status, Some(401));
}

/// Regression test for the concurrent-burst bypass: admission through the
/// flood gate is one atomic step per request, so of `5 * limit` concurrent
/// unknown-key requests exactly `limit` may pass the gate (each performing
/// exactly one key lookup and logging one normal row), every other request
/// of the burst is capped with no database access, and the capped phase
/// still writes exactly one row per window.
#[tokio::test]
async fn a_parallel_burst_cannot_bypass_the_cap() {
    let db = test_pool().await;
    let base = common::spawn_hydra_with_connect_info(test_app_with_flood(db.clone(), 2, 60)).await;

    const BURST: usize = 40;
    let statuses = join_all((0..BURST).map(|_| send_bad_chat(&base, None))).await;
    for status in &statuses {
        assert_eq!(
            *status,
            reqwest::StatusCode::UNAUTHORIZED,
            "both admitted and capped burst requests answer the same 401"
        );
    }

    // Exactly `limit` normal rows (one per admitted request, one key lookup
    // each) plus exactly one capped row: the burst cannot produce one row
    // per request, and nothing in between.
    wait_for_rows(&db, 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = fetch_rows(&db).await;
    assert_eq!(
        rows.len(),
        3,
        "a burst of 40 unknown-key requests with limit 2 must log exactly 3 rows",
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.error.as_deref() == Some("Invalid API key provided"))
            .count(),
        2,
        "only the admitted requests reach the key lookup and log a normal row"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.error.as_deref()
                == Some("flood cap: too many failed authentications from this address"))
            .count(),
        1,
        "the capped phase of the window writes exactly one row"
    );
    for row in &rows {
        assert_eq!(row.status, Some(401));
        assert_eq!(row.client_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(row.api_key_id, None);
    }

    // The IP stays capped for the rest of the window without new rows.
    assert_eq!(
        send_bad_chat(&base, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fetch_rows(&db).await.len(),
        3,
        "post-burst capped requests must not add rows"
    );
}
