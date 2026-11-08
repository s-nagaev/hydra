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
//! one proxied chat call through the in-process mock upstream, and the log
//! listing that reads the row back.

mod common;

use base64::Engine as _;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::{fixed_response_router, spawn_upstream, within, Recorder};
use hydra::config::DbTarget;
use hydra::db::channels::NewChannel;
use hydra::db::keys::NewKey;
use hydra::db::request_logs::{self, LogFilters, StatusClass};
use hydra::db::{channels, keys, Db};
use hydra::http::{router, AppState};

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
    AppState {
        db: db.clone(),
        admin_login: "admin".to_string(),
        admin_password: "secret".to_string(),
        client: hydra::http::upstream_client(),
        log_bodies: false,
        log_tracker: hydra::logging::LogTracker::default(),
        cache: hydra::cache::ResolutionCache::default(),
        trusted_proxies: hydra::config::TrustedProxies::default(),
        flood: hydra::flood::FloodCap::default(),
    }
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
        },
    )
    .await
    .unwrap();

    let alive = keys::find_alive_by_hash(&db, &hash)
        .await
        .unwrap()
        .expect("alive before delete");
    assert_eq!(alive.channel_id, channel.id);

    let patched = keys::patch(
        &db,
        key.id,
        &hydra::db::keys::KeyPatch {
            name: Some(Some("renamed".to_string())),
            enabled: Some(false),
        },
    )
    .await
    .unwrap();
    let keys::PatchOutcome::Updated(patched) = patched else {
        panic!("patch must update an alive key");
    };
    assert!(!patched.enabled);
    assert_eq!(patched.name.as_deref(), Some("renamed"));

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
        },
    )
    .await
    .unwrap();
    assert!(matches!(patched_deleted, keys::PatchOutcome::Deleted));

    channels::delete(&db, channel.id).await.unwrap();
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
        br#"{"id":"x","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#.to_vec(),
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
