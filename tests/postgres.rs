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
use hydra::db::request_logs::{self, LogFilters, StatusClass};
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

/// The admin stats page over PostgreSQL: the per-key month-to-date aggregate
/// and the priced cost have to render through the real router on the PG
/// schema, where the `substr` bucket and the `BIGINT` casts behave as the
/// sqlite unit tests pin them.
#[tokio::test]
async fn pg_stats_page_renders_priced_month_rows() {
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
    // rendered window.
    log_usage_at(&db, key.id, priced.channel.id, "2026-01-05 10:00:00").await;
    log_usage_at(&db, key.id, priced.channel.id, "2026-02-05 10:00:00").await;

    let response = within(
        router(app_state(&db)).oneshot(
            Request::builder()
                .uri("/admin/ui/stats?month=2026-01")
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
        html.contains("/admin/ui/stats?month=2025-12")
            && html.contains("/admin/ui/stats?month=2026-02"),
        "the month navigation renders: {html}"
    );
}
