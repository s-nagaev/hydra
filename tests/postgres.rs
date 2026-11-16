//! PostgreSQL integration tests.
//!
//! Gated behind `HYDRA_TEST_PG_URL`: without the variable every test prints
//! a skip note and passes, so the suite stays green without a server. With
//! one (e.g. from `docker run -d --name hydra-pg-test -e
//! POSTGRES_PASSWORD=hydra -p 54329:5432 postgres:16-alpine`):
//!
//! ```text
//! HYDRA_TEST_PG_URL=postgres://postgres:hydra@127.0.0.1:54329/postgres \
//!     cargo test --test postgres
//! ```
//!
//! The tests drive the real query layer and the real router against the
//! migrated PG schema: channel and key CRUD, the soft-delete state machine,
//! one proxied chat call through the in-process mock upstream, the log
//! listing that reads the row back, the month-to-date usage aggregate with
//! its index, and the 429 monthly-limit gate end to end.

mod common;

use base64::Engine as _;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::{fixed_response_router, spawn_upstream, within, Recorder};
use hydra::config::DbTarget;
use hydra::db::channels::NewChannel;
use hydra::db::keys::NewKey;
use hydra::db::request_logs::{self, LogFilters, StatusBucket, StatusClass};
use hydra::db::{channels, keys, Db, Sql};
use hydra::http::{router, AppState};
use hydra::month::Month;
use hydra::prices::ModelPrices;

fn pg_url() -> Option<String> {
    std::env::var("HYDRA_TEST_PG_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

/// A fresh migrated pool per test: each `#[tokio::test]` runs its own tokio
/// runtime, and an sqlx pool must not outlive the runtime that created it.
/// Migrations are idempotent and the PG migrator serializes concurrent
/// runners on an advisory lock, so parallel tests coexist.
async fn pg_db() -> Option<Db> {
    let url = pg_url()?;
    Some(
        hydra::db::init_pool(&DbTarget::Postgres(url))
            .await
            .expect("postgres pool init (migrations included)"),
    )
}

fn app_state(db: &Db) -> AppState {
    AppState::new(
        db.clone(),
        "admin".to_string(),
        "secret".to_string(),
        false,
        hydra::config::TrustedProxies::default(),
        hydra::flood::FloodCap::default(),
    )
}

fn auth_header() -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("admin:secret")
    )
}

async fn admin_post(state: &AppState, uri: &str, body: Value) -> (axum::http::StatusCode, Value) {
    use axum::body::Body;
    use axum::http::Request;

    let response = within(
        router(state.clone()).oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", auth_header())
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

async fn post_chat(state: &AppState, key: &str, body: Value) -> axum::http::StatusCode {
    use axum::body::Body;
    use axum::http::Request;

    let response = within(
        router(state.clone()).oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    response.status()
}

async fn post_chat_json(
    state: &AppState,
    key: &str,
    body: Value,
) -> (axum::http::StatusCode, Value) {
    use axum::body::Body;
    use axum::http::Request;

    let response = within(
        router(state.clone()).oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// One request log row for `key_id` at an explicit instant: 1000 prompt
/// tokens with 400 of them cached and 200 completion tokens, which the
/// pinned formula prices at 4000 micro-USD on the 2.50/1.25/10.00 table.
async fn log_usage_at(db: &Db, key_id: i64, channel_id: i64, at: &str) {
    request_logs::insert_at(
        db,
        &request_logs::NewRequestLog {
            api_key_id: Some(key_id),
            channel_id: Some(channel_id),
            model: Some("gpt-4o".to_string()),
            status: Some(200),
            prompt_tokens: Some(1000),
            completion_tokens: Some(200),
            cached_input_tokens: Some(400),
            cached_output_tokens: None,
            cache_write_input_tokens: None,
            duration_ms: 1,
            request_body: None,
            response_body: None,
            error: None,
            client_ip: None,
        },
        at,
    )
    .await
    .unwrap();
}

/// One request log row with an explicit model, status and duration, for the
/// hourly aggregate tests: 1000 prompt tokens with 400 cached and 200
/// completion tokens, so an hour's sums are recognisable by eye. A `key_id`
/// of `None` is a rejection whose key never resolved.
async fn log_hourly_at(
    db: &Db,
    key_id: Option<i64>,
    channel_id: Option<i64>,
    model: &str,
    status: Option<i64>,
    duration_ms: i64,
    at: &str,
) {
    request_logs::insert_at(
        db,
        &request_logs::NewRequestLog {
            api_key_id: key_id,
            channel_id,
            model: Some(model.to_string()),
            status,
            prompt_tokens: Some(1000),
            completion_tokens: Some(200),
            cached_input_tokens: Some(400),
            cached_output_tokens: None,
            cache_write_input_tokens: None,
            duration_ms,
            request_body: None,
            response_body: None,
            error: None,
            client_ip: None,
        },
        at,
    )
    .await
    .unwrap();
}

/// Whether two averages agree to the precision the tests assert.
fn approx(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

/// The hourly per-code hand-over: hour, status and request count.
fn hour_codes(rows: &[request_logs::HourStatusCodeUsage]) -> Vec<(String, Option<i64>, i64)> {
    rows.iter()
        .map(|row| (row.hour.clone(), row.status, row.requests))
        .collect()
}

fn skip_note() {
    eprintln!("skip: HYDRA_TEST_PG_URL is not set");
}

/// Unique suffix per process so parallel test binaries never collide on the
/// UNIQUE columns.
fn suffix(tag: &str) -> String {
    format!("{}-{}", tag, std::process::id())
}

#[tokio::test]
async fn pg_channel_crud_round_trip() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("crud");

    let created = channels::create(
        &db,
        &NewChannel {
            name: format!("pg-{tag}"),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "sk-pg-secret".to_string(),
            allowed_models: Some(vec!["gpt-4o".to_string()]),
        },
    )
    .await
    .unwrap();
    assert!(created.enabled);
    assert_eq!(
        created.allowed_models.as_deref(),
        Some(&["gpt-4o".to_string()][..])
    );
    // The canonical TEXT timestamp shape, not a PG-native rendering.
    assert_eq!(created.created_at.len(), 19);
    assert_eq!(created.created_at.as_bytes()[10], b' ');

    let found = channels::find(&db, created.id).await.unwrap().unwrap();
    assert_eq!(found.name, format!("pg-{tag}"));

    let patched = channels::update(
        &db,
        created.id,
        &hydra::db::channels::ChannelPatch {
            name: None,
            base_url: None,
            api_key: None,
            allowed_models: None,
            enabled: Some(false),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!patched.enabled);

    let listed = channels::list(&db).await.unwrap();
    assert!(listed.iter().any(|c| c.id == created.id));

    // A bound key blocks the delete exactly as on sqlite.
    let key = keys::create(
        &db,
        &NewKey {
            key_hash: format!("hash-{tag}"),
            display_prefix: "hyd-pgcrud".to_string(),
            name: None,
            channel_id: created.id,
            monthly_limit_micro_usd: None,
        },
    )
    .await
    .unwrap();
    let blocked = channels::delete(&db, created.id).await.unwrap();
    assert!(matches!(blocked, channels::DeleteOutcome::HasBoundKeys));
    assert_eq!(channels::bound_key_count(&db, created.id).await.unwrap(), 1);
    keys::delete(&db, key.id).await.unwrap();
    // Soft-deleted keys keep blocking: the row stays in place for the
    // request_logs FK, so the channel is still un-deletable.
    let still_blocked = channels::delete(&db, created.id).await.unwrap();
    assert!(matches!(
        still_blocked,
        channels::DeleteOutcome::HasBoundKeys
    ));
}

#[tokio::test]
async fn pg_key_soft_delete_state_machine() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("keys");
    let hash = format!("hash-{tag}");

    let channel = channels::create(
        &db,
        &NewChannel {
            name: format!("pg-{tag}"),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "sk-pg-secret".to_string(),
            allowed_models: None,
        },
    )
    .await
    .unwrap();
    let key = keys::create(
        &db,
        &NewKey {
            key_hash: hash.clone(),
            display_prefix: "hyd-pgkeys".to_string(),
            name: Some("before".to_string()),
            channel_id: channel.id,
            monthly_limit_micro_usd: Some(25_000_000),
        },
    )
    .await
    .unwrap();

    let alive = keys::find_alive_by_hash(&db, &hash)
        .await
        .unwrap()
        .expect("alive before delete");
    assert_eq!(alive.channel_id, channel.id);
    assert_eq!(alive.monthly_limit_micro_usd, Some(25_000_000));

    let patched = keys::patch(
        &db,
        key.id,
        &hydra::db::keys::KeyPatch {
            name: Some(Some("renamed".to_string())),
            enabled: Some(false),
            monthly_limit_micro_usd: Some(Some(60_000_000)),
            channel_id: None,
        },
    )
    .await
    .unwrap();
    let keys::PatchOutcome::Updated(patched) = patched else {
        panic!("patch must update an alive key");
    };
    assert!(!patched.enabled);
    assert_eq!(patched.name.as_deref(), Some("renamed"));
    assert_eq!(patched.monthly_limit_micro_usd, Some(60_000_000));

    // An absent limit leaves the stored value alone; an explicit null
    // clears the column rather than writing a zero.
    let keys::PatchOutcome::Updated(untouched) = keys::patch(
        &db,
        key.id,
        &hydra::db::keys::KeyPatch {
            name: Some(Some("renamed-again".to_string())),
            enabled: None,
            monthly_limit_micro_usd: None,
            channel_id: None,
        },
    )
    .await
    .unwrap() else {
        panic!("patch must update an alive key");
    };
    assert_eq!(untouched.monthly_limit_micro_usd, Some(60_000_000));

    let keys::PatchOutcome::Updated(cleared) = keys::patch(
        &db,
        key.id,
        &hydra::db::keys::KeyPatch {
            name: None,
            enabled: None,
            monthly_limit_micro_usd: Some(None),
            channel_id: None,
        },
    )
    .await
    .unwrap() else {
        panic!("patch must update an alive key");
    };
    assert_eq!(cleared.monthly_limit_micro_usd, None);
    assert_eq!(
        keys::find(&db, key.id)
            .await
            .unwrap()
            .unwrap()
            .monthly_limit_micro_usd,
        None
    );

    let removed = keys::delete(&db, key.id).await.unwrap();
    assert!(matches!(removed, keys::DeleteOutcome::Deleted));
    let again = keys::delete(&db, key.id).await.unwrap();
    assert!(matches!(again, keys::DeleteOutcome::AlreadyDeleted));
    let lookup = keys::find_alive_by_hash(&db, &hash).await.unwrap();
    assert!(lookup.is_none(), "soft-deleted keys must not resolve");

    let patched_deleted = keys::patch(
        &db,
        key.id,
        &hydra::db::keys::KeyPatch {
            name: Some(Some("x".to_string())),
            enabled: None,
            monthly_limit_micro_usd: None,
            channel_id: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(patched_deleted, keys::PatchOutcome::Deleted));

    channels::delete(&db, channel.id).await.unwrap();
}

/// The monthly limit is a nullable integer column: it must store, read back
/// and clear through the same paths as sqlite, and a NULL must stay NULL
/// rather than turning into a zero limit.
#[tokio::test]
async fn pg_key_monthly_limit_round_trip() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("limit");
    let state = app_state(&db);

    let (status, created) = admin_post(
        &state,
        "/admin/channels",
        json!({
            "name": format!("pg-{tag}"),
            "base_url": "http://127.0.0.1:9",
            "api_key": "sk-pg-secret"
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    let channel_id = created["id"].as_i64().unwrap();

    // Issued with a limit: stored as micro-USD, echoed in both shapes.
    let (status, key) = admin_post(
        &state,
        "/admin/keys",
        json!({ "channel_id": channel_id, "name": "pg-limit", "monthly_limit_usd": "50.00" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{key}");
    assert_eq!(key["monthly_limit_micro_usd"], json!(50_000_000));
    assert_eq!(key["monthly_limit_usd"], json!("50.00"));
    let key_id = key["id"].as_i64().unwrap();

    let found = keys::find(&db, key_id).await.unwrap().unwrap();
    assert_eq!(found.monthly_limit_micro_usd, Some(50_000_000));

    // A patch replaces the stored limit, then clears it back to unlimited.
    let keys::PatchOutcome::Updated(replaced) = keys::patch(
        &db,
        key_id,
        &hydra::db::keys::KeyPatch {
            name: None,
            enabled: None,
            monthly_limit_micro_usd: Some(Some(7_500_000)),
            channel_id: None,
        },
    )
    .await
    .unwrap() else {
        panic!("patch must update an alive key");
    };
    assert_eq!(replaced.monthly_limit_micro_usd, Some(7_500_000));

    let keys::PatchOutcome::Updated(cleared) = keys::patch(
        &db,
        key_id,
        &hydra::db::keys::KeyPatch {
            name: None,
            enabled: None,
            monthly_limit_micro_usd: Some(None),
            channel_id: None,
        },
    )
    .await
    .unwrap() else {
        panic!("patch must update an alive key");
    };
    assert_eq!(cleared.monthly_limit_micro_usd, None);
    assert_eq!(
        keys::find(&db, key_id)
            .await
            .unwrap()
            .unwrap()
            .monthly_limit_micro_usd,
        None,
        "a cleared limit must read back as NULL"
    );

    // Issued without a limit: NULL, not zero.
    let (status, unlimited) = admin_post(
        &state,
        "/admin/keys",
        json!({ "channel_id": channel_id, "name": "pg-unlimited" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{unlimited}");
    assert_eq!(unlimited["monthly_limit_micro_usd"], Value::Null);
    assert_eq!(unlimited["monthly_limit_usd"], Value::Null);

    // A limit that cannot be represented is refused before anything lands.
    for refused in [json!("0.0000005"), json!(-1), json!("1000000001")] {
        let (status, error) = admin_post(
            &state,
            "/admin/keys",
            json!({ "channel_id": channel_id, "monthly_limit_usd": refused }),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::BAD_REQUEST,
            "{refused} must be refused: {error}"
        );
    }
}

#[tokio::test]
async fn pg_proxy_call_logs_and_lists() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("proxy");

    let upstream = spawn_upstream(fixed_response_router(
        Recorder::default(),
        axum::http::StatusCode::OK,
        "application/json",
        br#"{"id":"x","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":2,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":1}}}"#.to_vec(),
    ))
    .await;

    let state = app_state(&db);
    let (status, created) = admin_post(
        &state,
        "/admin/channels",
        json!({
            "name": format!("pg-{tag}"),
            "base_url": upstream,
            "api_key": "sk-upstream-pg"
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    let channel_id = created["id"].as_i64().unwrap();

    let (status, key) = admin_post(
        &state,
        "/admin/keys",
        json!({ "channel_id": channel_id, "name": "pg-client" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{key}");
    let plaintext = key["key"].as_str().unwrap().to_string();

    let status = post_chat(
        &state,
        &plaintext,
        json!({ "model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}] }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let filters = LogFilters {
        key_id: None,
        channel_id: Some(channel_id),
        model: None,
        status_class: None,
        from: None,
        to: None,
        search: None,
        page: 1,
        page_size: 10,
    };
    // The log insert is detached; poll briefly for it to land.
    let mut rows = Vec::new();
    for _ in 0..100 {
        let (items, total) = request_logs::list(&db, &filters).await.unwrap();
        if total >= 1 {
            rows = items;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(rows.len(), 1, "exactly one log row expected");
    assert_eq!(rows[0].status, Some(200));
    assert_eq!(rows[0].prompt_tokens, Some(2));
    assert_eq!(rows[0].completion_tokens, Some(3));
    assert_eq!(
        rows[0].cached_input_tokens,
        Some(1),
        "the cached prompt tokens must round-trip through the pg schema"
    );
    assert_eq!(
        rows[0].cached_output_tokens, None,
        "the upstream reported no cached completion tokens"
    );
    assert!(rows[0].error.is_none());
    assert_eq!(rows[0].created_at.len(), 19);
    assert_eq!(rows[0].created_at.as_bytes()[10], b' ');
}

/// A rejection row (unknown key 401) carries no key or channel reference:
/// the PG schema must accept the NULLs and read them back as such.
#[tokio::test]
async fn pg_rejection_row_with_null_ids_round_trips() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };

    request_logs::insert(
        &db,
        &request_logs::NewRequestLog {
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
            request_body: Some(r#"{"model":"gpt-4o"}"#.to_string()),
            response_body: None,
            error: Some("Invalid API key provided".to_string()),
            client_ip: Some("203.0.113.7".to_string()),
        },
    )
    .await
    .unwrap();

    let filters = LogFilters {
        key_id: None,
        channel_id: None,
        model: None,
        status_class: Some(StatusClass::Client4xx),
        from: None,
        to: None,
        search: None,
        page: 1,
        page_size: 10,
    };
    let (items, _) = request_logs::list(&db, &filters).await.unwrap();
    let row = items
        .iter()
        .find(|row| {
            row.status == Some(401) && row.error.as_deref() == Some("Invalid API key provided")
        })
        .expect("the rejection row must be listed");
    assert_eq!(row.api_key_id, None);
    assert_eq!(row.channel_id, None);
    assert_eq!(row.client_ip.as_deref(), Some("203.0.113.7"));
    assert_eq!(row.request_body.as_deref(), Some(r#"{"model":"gpt-4o"}"#));
}

/// The free-text search over the real PostgreSQL schema: `LOWER` over the
/// nullable text columns and the `LIKE <pattern> ESCAPE '\'` predicate must
/// parse and select exactly as the SQLite unit tests pin them, so a needle
/// finds the same rows on both backends. The needle carries a per-process
/// marker so rows other tests inserted cannot satisfy the assertion.
#[tokio::test]
async fn pg_search_matches_the_text_columns_case_insensitively() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let marker = suffix("needle");
    let needle = format!("invol-{marker}");
    for (request_body, response_body, error) in [
        (Some(format!("order {needle} confirmed")), None, None),
        (None, Some(format!("{{\"id\":\"{needle}\"}}")), None),
        (None, None, Some(format!("upstream refused {needle}"))),
        (Some("nothing of the sort".to_string()), None, None),
    ] {
        request_logs::insert(
            &db,
            &request_logs::NewRequestLog {
                api_key_id: None,
                channel_id: None,
                model: None,
                status: Some(200),
                prompt_tokens: None,
                completion_tokens: None,
                cached_input_tokens: None,
                cached_output_tokens: None,
                cache_write_input_tokens: None,
                duration_ms: 1,
                request_body,
                response_body,
                error,
                client_ip: None,
            },
        )
        .await
        .unwrap();
    }

    let mut filters = LogFilters {
        key_id: None,
        channel_id: None,
        model: None,
        status_class: None,
        from: None,
        to: None,
        search: Some(needle.to_uppercase()),
        page: 1,
        page_size: 50,
    };
    let (rows, total) = request_logs::list(&db, &filters).await.unwrap();
    assert_eq!(total, 3, "one row per searched column: {rows:?}");

    // A % and a _ in the needle are matched literally, not as wildcards:
    // the needle would otherwise select every row above, whose text carries
    // the marker.
    filters.search = Some(format!("%{marker}_"));
    let (rows, total) = request_logs::list(&db, &filters).await.unwrap();
    assert_eq!(total, 0, "no row holds the literal pattern: {rows:?}");

    let literal = format!("100%{marker}_done");
    request_logs::insert(
        &db,
        &request_logs::NewRequestLog {
            api_key_id: None,
            channel_id: None,
            model: None,
            status: Some(200),
            prompt_tokens: None,
            completion_tokens: None,
            cached_input_tokens: None,
            cached_output_tokens: None,
            cache_write_input_tokens: None,
            duration_ms: 1,
            request_body: Some(literal.clone()),
            response_body: None,
            error: None,
            client_ip: None,
        },
    )
    .await
    .unwrap();
    let (rows, total) = request_logs::list(&db, &filters).await.unwrap();
    assert_eq!(total, 1, "the literal characters must match: {rows:?}");
    assert_eq!(rows[0].request_body.as_deref(), Some(literal.as_str()));
}

/// The month-to-date aggregate over the real PostgreSQL schema: the range
/// predicate, the `BIGINT` casts PostgreSQL needs (`SUM` over `BIGINT`
/// returns `NUMERIC`) and the per-model grouping must behave exactly as the
/// SQLite unit tests pin them.
#[tokio::test]
async fn pg_month_to_date_usage_sums_the_month_window() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("usage");
    let channel = channels::create(
        &db,
        &NewChannel {
            name: format!("pg-{tag}"),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "sk-pg-secret".to_string(),
            allowed_models: None,
        },
    )
    .await
    .unwrap();
    let key = keys::create(
        &db,
        &NewKey {
            key_hash: format!("hash-{tag}"),
            display_prefix: "hyd-pgusage".to_string(),
            name: None,
            channel_id: channel.id,
            monthly_limit_micro_usd: None,
        },
    )
    .await
    .unwrap();

    // Both January edges, and one row on each side of the window.
    log_usage_at(&db, key.id, channel.id, "2026-01-05 10:00:00").await;
    log_usage_at(&db, key.id, channel.id, "2026-01-31 23:59:59").await;
    log_usage_at(&db, key.id, channel.id, "2025-12-31 23:59:59").await;
    log_usage_at(&db, key.id, channel.id, "2026-02-01 00:00:00").await;
    // A row inside the window without token counts: it counts as a request
    // and adds nothing to the sums.
    request_logs::insert_at(
        &db,
        &request_logs::NewRequestLog {
            api_key_id: Some(key.id),
            channel_id: Some(channel.id),
            model: Some("gpt-4o".to_string()),
            status: Some(200),
            prompt_tokens: None,
            completion_tokens: None,
            cached_input_tokens: None,
            cached_output_tokens: None,
            cache_write_input_tokens: None,
            duration_ms: 1,
            request_body: None,
            response_body: None,
            error: None,
            client_ip: None,
        },
        "2026-01-20 12:00:00",
    )
    .await
    .unwrap();
    // Another key's January spend must not leak into these totals.
    let other_key = keys::create(
        &db,
        &NewKey {
            key_hash: format!("hash-{tag}-other"),
            display_prefix: "hyd-pgusage2".to_string(),
            name: None,
            channel_id: channel.id,
            monthly_limit_micro_usd: None,
        },
    )
    .await
    .unwrap();
    log_usage_at(&db, other_key.id, channel.id, "2026-01-06 10:00:00").await;

    let january = Month::from_parts(2026, 1).unwrap();
    let usage = request_logs::month_to_date_usage(&db, key.id, &january)
        .await
        .unwrap();
    assert_eq!(usage.len(), 1, "one model, one group: {usage:?}");
    assert_eq!(usage[0].model.as_deref(), Some("gpt-4o"));
    assert_eq!(usage[0].prompt_tokens, 2000);
    assert_eq!(usage[0].completion_tokens, 400);
    assert_eq!(usage[0].cached_input_tokens, 800);
    assert_eq!(usage[0].cached_output_tokens, 0);
    assert_eq!(usage[0].requests, 3);

    // The month the aggregate is asked for decides the window, not the
    // current date.
    assert!(
        request_logs::month_to_date_usage(&db, key.id, &Month::from_parts(2025, 11).unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}

/// The `(api_key_id, created_at)` index the aggregate relies on must exist
/// after the migrations ran, so the range predicate can use it.
#[tokio::test]
async fn pg_request_logs_key_created_at_index_exists() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let mut sql = Sql::for_db(&db);
    sql.push(
        "SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'request_logs' AND indexname = \
         'idx_request_logs_api_key_id_created_at'",
    );
    assert_eq!(
        db.scalar(sql).await.unwrap(),
        1,
        "the per-key month-to-date index must be migrated in"
    );
}

/// The monthly-limit gate end to end over PostgreSQL: the aggregate feeds
/// the in-memory verdict, the verdict rejects the next request with the
/// exact OpenAI 429 body, and the audit row names the key that hit it.
#[tokio::test]
async fn pg_monthly_limit_gate_rejects_an_over_budget_key() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("limitgate");
    let upstream = spawn_upstream(fixed_response_router(
        Recorder::default(),
        axum::http::StatusCode::OK,
        "application/json",
        br#"{"id":"x","object":"chat.completion","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#.to_vec(),
    ))
    .await;
    let prices = ModelPrices::from_usd_json(&json!({
        "gpt-4o": {
            "input": "2.50",
            "output": "10.00",
            "cached_input": "1.25",
            "cached_output": "5.00"
        }
    }))
    .unwrap();
    let priced = channels::create_with_prices(
        &db,
        &NewChannel {
            name: format!("pg-{tag}"),
            base_url: upstream,
            api_key: "sk-pg-secret".to_string(),
            allowed_models: Some(vec!["gpt-4o".to_string()]),
        },
        Some(&prices),
    )
    .await
    .unwrap();
    assert!(priced.limitable());
    let generated = hydra::keys::generate();
    let key = keys::create(
        &db,
        &NewKey {
            key_hash: generated.key_hash,
            display_prefix: generated.display_prefix,
            name: None,
            channel_id: priced.channel.id,
            monthly_limit_micro_usd: Some(1),
        },
    )
    .await
    .unwrap();

    let state = app_state(&db);
    // One month-to-date request costs 4000 micro-USD, far past the limit.
    log_usage_at(
        &db,
        key.id,
        priced.channel.id,
        &hydra::db::timestamp::now_utc(),
    )
    .await;
    hydra::usage::recompute(&db, &state.limit_flags, key.id)
        .await
        .unwrap();

    let (status, body) = post_chat_json(
        &state,
        &generated.full,
        json!({ "model": "gpt-4o", "messages": [{ "role": "user", "content": "hi" }] }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["message"], "Monthly limit reached");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["code"], Value::Null);

    // The rejection is logged against the real key, so per-key reporting of
    // a limited key stays complete on this backend too.
    let filters = LogFilters {
        key_id: Some(key.id),
        channel_id: None,
        model: None,
        status_class: None,
        from: None,
        to: None,
        search: None,
        page: 1,
        page_size: 10,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (items, _) = request_logs::list(&db, &filters).await.unwrap();
        if let Some(row) = items.iter().find(|row| row.status == Some(429)) {
            assert_eq!(row.api_key_id, Some(key.id));
            assert_eq!(row.channel_id, Some(priced.channel.id));
            assert_eq!(row.model.as_deref(), Some("gpt-4o"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the 429 audit row never landed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// The admin stats page over PostgreSQL: the per-day aggregate of a date
/// range and the priced cost have to render through the real router on the PG
/// schema, where the `substr` bucket and the `BIGINT` casts behave as the
/// sqlite unit tests pin them.
#[tokio::test]
async fn pg_stats_page_renders_priced_range_rows() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    use axum::body::Body;
    use axum::http::Request;

    let tag = suffix("stats");
    let prices = ModelPrices::from_usd_json(&serde_json::json!({
        "gpt-4o": {
            "input": "2.50",
            "output": "10.00",
            "cached_input": "1.25",
            "cached_output": "5.00"
        }
    }))
    .unwrap();
    let priced = channels::create_with_prices(
        &db,
        &NewChannel {
            name: format!("pg-{tag}"),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "sk-pg-secret".to_string(),
            allowed_models: Some(vec!["gpt-4o".to_string()]),
        },
        Some(&prices),
    )
    .await
    .unwrap();
    assert!(priced.limitable(), "the seeded channel must be limitable");
    let key = keys::create(
        &db,
        &NewKey {
            key_hash: format!("hash-{tag}"),
            display_prefix: "hyd-pgstats".to_string(),
            name: None,
            channel_id: priced.channel.id,
            monthly_limit_micro_usd: Some(50_000_000),
        },
    )
    .await
    .unwrap();

    // One January request and one in February: only January belongs to the
    // rendered range.
    log_usage_at(&db, key.id, priced.channel.id, "2026-01-05 10:00:00").await;
    log_usage_at(&db, key.id, priced.channel.id, "2026-02-05 10:00:00").await;

    // Every other test of this binary shares this database, and the January
    // 2026 window is not this test's own: the month-to-date test logs rows on
    // 2026-01-05, 06, 20 and 31 under its own keys. Without a key in the URL
    // the pages count those rows too, so both URLs below name this test's
    // key, which leaves the assertions reading its rows alone.
    let response = within(
        router(app_state(&db)).oneshot(
            Request::builder()
                .uri(format!(
                    "/admin/ui/stats?from=2026-01-01&to=2026-01-31&key_id={}",
                    key.id
                ))
                .header("authorization", auth_header())
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(html.contains("hyd-pgstats"), "{html}");
    assert!(html.contains("gpt-4o"), "{html}");
    assert!(html.contains(">1,000<"), "one January request only: {html}");
    assert!(html.contains(">$0.004<"), "the priced January row: {html}");
    assert!(html.contains(">All keys<"), "the grand total line: {html}");
    assert!(
        html.contains("2026-01-01 to 2026-01-31")
            && html.contains("name=\"from\" value=\"2026-01-01\"")
            && html.contains("name=\"to\" value=\"2026-01-31\""),
        "the range the page rendered is the one the URL named: {html}"
    );

    // The Requests page reads the same range out of the status aggregates on
    // the PG schema, which count the January request alone once the key is
    // named.
    let response = within(
        router(app_state(&db)).oneshot(
            Request::builder()
                .uri(format!(
                    "/admin/ui/stats/requests?from=2026-01-01&to=2026-01-31&key_id={}",
                    key.id
                ))
                .header("authorization", auth_header())
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(html.contains("Requests per day"), "{html}");
    assert!(
        html.contains("Day 5: 1 requests (1 200)"),
        "one January request only: {html}"
    );
    // The whole table of the view, whitespace flattened: one line for code
    // 200 carrying the one request of the range. The February request of the
    // same key would make it two, so this line pins the range boundary too.
    let cells = html.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        cells.contains(
            "<td class=\"mono\">200</td> <td>2xx</td> <td class=\"mono\">1</td> \
             <td class=\"mono\">100.0%</td>"
        ),
        "the status code table counts the logged code alone: {html}"
    );
    assert!(html.contains("2026-01-01 to 2026-01-31"), "{html}");
}

/// The hourly aggregates over the real PostgreSQL schema: the
/// `substr(created_at, 1, 13)` bucket, its edges and the `BIGINT`/`DOUBLE
/// PRECISION` casts have to behave exactly as the sqlite unit tests pin
/// them. The rows sit in a March window no other test of this binary writes
/// to, and every query names this test's key, so the shared database cannot
/// leak into the figures.
#[tokio::test]
async fn pg_hourly_aggregates_bucket_by_hour_and_respect_their_filters() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("hourly");
    let channel = channels::create(
        &db,
        &NewChannel {
            name: format!("pg-{tag}"),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "sk-pg-secret".to_string(),
            allowed_models: None,
        },
    )
    .await
    .unwrap();
    let key = keys::create(
        &db,
        &NewKey {
            key_hash: format!("hash-{tag}"),
            display_prefix: "hyd-pghourly".to_string(),
            name: None,
            channel_id: channel.id,
            monthly_limit_micro_usd: None,
        },
    )
    .await
    .unwrap();

    // The instant before the range, both hours of it (the second row lands
    // exactly on the last instant of the 08 hour, the third exactly on the
    // first instant of the 09 hour), and the first instant after it.
    log_hourly_at(
        &db,
        Some(key.id),
        Some(channel.id),
        "gpt-4o",
        Some(200),
        999,
        "2026-03-04 07:59:59",
    )
    .await;
    log_hourly_at(
        &db,
        Some(key.id),
        Some(channel.id),
        "gpt-4o",
        Some(200),
        100,
        "2026-03-04 08:00:00",
    )
    .await;
    log_hourly_at(
        &db,
        Some(key.id),
        Some(channel.id),
        "gpt-4o",
        Some(200),
        300,
        "2026-03-04 08:59:59",
    )
    .await;
    log_hourly_at(
        &db,
        Some(key.id),
        Some(channel.id),
        "gpt-4o",
        Some(500),
        50,
        "2026-03-04 09:00:00",
    )
    .await;
    log_hourly_at(
        &db,
        Some(key.id),
        Some(channel.id),
        "claude",
        Some(200),
        150,
        "2026-03-04 09:30:00",
    )
    .await;
    log_hourly_at(
        &db,
        Some(key.id),
        Some(channel.id),
        "gpt-4o",
        Some(200),
        999,
        "2026-03-04 10:00:00",
    )
    .await;

    let range_start = "2026-03-04 08:00:00";
    let range_end = "2026-03-04 10:00:00";

    // Usage: one group per (hour, key, model), the two models of the 09 hour
    // apart, and the rows on both sides of the range out.
    let usage = request_logs::hour_usage(&db, range_start, range_end, Some(key.id), None, None)
        .await
        .unwrap();
    let buckets: Vec<(String, Option<String>, i64, i64)> = usage
        .iter()
        .map(|group| {
            (
                group.hour.clone(),
                group.model.clone(),
                group.requests,
                group.prompt_tokens,
            )
        })
        .collect();
    assert_eq!(
        buckets,
        vec![
            (
                "2026-03-04 08".to_string(),
                Some("gpt-4o".to_string()),
                2,
                2000
            ),
            (
                "2026-03-04 09".to_string(),
                Some("claude".to_string()),
                1,
                1000
            ),
            (
                "2026-03-04 09".to_string(),
                Some("gpt-4o".to_string()),
                1,
                1000
            ),
        ],
        "{usage:?}"
    );

    // The model filter narrows to the hours that model spans.
    let only_gpt = request_logs::hour_usage(
        &db,
        range_start,
        range_end,
        Some(key.id),
        Some("gpt-4o".to_string()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(only_gpt.len(), 2, "{only_gpt:?}");
    assert!(only_gpt
        .iter()
        .all(|group| group.model.as_deref() == Some("gpt-4o")));

    // Status: one group per (hour, exact code), the 5xx of the 09 hour on its
    // own line.
    let status =
        request_logs::hour_status_code_usage(&db, range_start, range_end, Some(key.id), None, None)
            .await
            .unwrap();
    assert_eq!(
        hour_codes(&status),
        vec![
            ("2026-03-04 08".to_string(), Some(200), 2),
            ("2026-03-04 09".to_string(), Some(200), 1),
            ("2026-03-04 09".to_string(), Some(500), 1),
        ],
        "{status:?}"
    );

    // The class filter narrows by the bucket the code falls into.
    let server = request_logs::hour_status_code_usage(
        &db,
        range_start,
        range_end,
        Some(key.id),
        None,
        Some(StatusBucket::Server5xx),
    )
    .await
    .unwrap();
    assert_eq!(
        hour_codes(&server),
        vec![("2026-03-04 09".to_string(), Some(500), 1)],
        "{server:?}"
    );

    // Latency: one bucket per hour, the mean over the durations of both rows
    // of the hour, the hour edges included and the range edges excluded.
    let latency = request_logs::hour_latency(&db, range_start, range_end, Some(key.id), None, None)
        .await
        .unwrap();
    assert_eq!(latency.len(), 2, "{latency:?}");
    assert_eq!(latency[0].hour, "2026-03-04 08");
    assert!(
        approx(latency[0].avg_duration_ms, 200.0),
        "the mean of 100 and 300: {latency:?}"
    );
    assert_eq!(latency[0].requests, 2);
    assert_eq!(latency[1].hour, "2026-03-04 09");
    assert!(
        approx(latency[1].avg_duration_ms, 100.0),
        "the mean of 50 and 150: {latency:?}"
    );
    assert_eq!(latency[1].requests, 2);
}

/// The hourly latency aggregate averages every timed request of a bucket,
/// including one whose key never resolved, while the usage aggregate of the
/// same rows drops it: the difference the sqlite unit test pins has to hold
/// on the PG schema too. The model name carries the process id, so no other
/// test of this binary can join the average.
#[tokio::test]
async fn pg_hourly_latency_keeps_the_requests_without_a_key() {
    let Some(db) = pg_db().await else {
        skip_note();
        return;
    };
    let tag = suffix("hourly-nokey");
    let model = format!("model-{tag}");
    let channel = channels::create(
        &db,
        &NewChannel {
            name: format!("pg-{tag}"),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "sk-pg-secret".to_string(),
            allowed_models: None,
        },
    )
    .await
    .unwrap();
    let key = keys::create(
        &db,
        &NewKey {
            key_hash: format!("hash-{tag}"),
            display_prefix: "hyd-pgnokey".to_string(),
            name: None,
            channel_id: channel.id,
            monthly_limit_micro_usd: None,
        },
    )
    .await
    .unwrap();

    log_hourly_at(
        &db,
        Some(key.id),
        Some(channel.id),
        &model,
        Some(200),
        10,
        "2026-03-05 10:00:00",
    )
    .await;
    // A rejection row whose key never resolved: it is still timed.
    log_hourly_at(
        &db,
        None,
        None,
        &model,
        Some(401),
        30,
        "2026-03-05 10:30:00",
    )
    .await;

    let range_start = "2026-03-05 10:00:00";
    let range_end = "2026-03-05 11:00:00";

    let latency =
        request_logs::hour_latency(&db, range_start, range_end, None, Some(model.clone()), None)
            .await
            .unwrap();
    assert_eq!(latency.len(), 1, "{latency:?}");
    assert_eq!(latency[0].hour, "2026-03-05 10");
    assert!(
        approx(latency[0].avg_duration_ms, 20.0),
        "the mean of 10 and 30 keeps the rejection: {latency:?}"
    );
    assert_eq!(latency[0].requests, 2);

    // The usage aggregate of the same rows leaves the keyless row out.
    let usage =
        request_logs::hour_usage(&db, range_start, range_end, None, Some(model.clone()), None)
            .await
            .unwrap();
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0].key_id, key.id);
    assert_eq!(usage[0].requests, 1);
}
