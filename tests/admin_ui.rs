//! Integration tests for the admin UI channels and keys pages: form flows
//! through the real router, validation rendering, the delete-refusal flow,
//! the one-time key reveal fragment, CSRF rejection and auth gates. Form
//! POSTs carry the same-origin fetch metadata a browser form would send;
//! cross-site variants pin the guard's 403.

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

fn form(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

type Send = (StatusCode, String);

/// Sends a request through the full router with admin credentials and an
/// optional extra header (used for Sec-Fetch-Site metadata).
async fn send(
    db: &SqlitePool,
    method: &str,
    uri: &str,
    body: Option<String>,
    extra_header: Option<(&str, &str)>,
    auth: Option<&str>,
) -> Send {
    let app = test_app(db.clone());
    let content_type = body.as_ref().map(|_| "application/x-www-form-urlencoded");
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        builder = builder.header("Authorization", auth);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("Content-Type", content_type);
    }
    if let Some((name, value)) = extra_header {
        builder = builder.header(name, value);
    }
    let request = match body {
        Some(body) => builder.body(Body::from(body)),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let response = within(hydra::http::router(app).oneshot(request))
        .await
        .unwrap();
    let status = response.status();
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn post_form(db: &SqlitePool, uri: &str, body: &str) -> Send {
    send(
        db,
        "POST",
        uri,
        Some(body.to_string()),
        None,
        Some(&auth_header()),
    )
    .await
}

async fn get_page(db: &SqlitePool, uri: &str) -> Send {
    send(db, "GET", uri, None, None, Some(&auth_header())).await
}

/// Creates a channel through the admin JSON API and returns its id.
async fn create_channel(db: &SqlitePool) -> i64 {
    let app = test_app(db.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/admin/channels")
        .header("Authorization", auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": "primary",
                "base_url": "https://api.example.com",
                "api_key": "sk-upstream-secret-1234567890",
            })
            .to_string(),
        ))
        .unwrap();
    let response = within(hydra::http::router(app).oneshot(request))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    parsed["id"].as_i64().unwrap()
}

async fn channel_enabled(db: &SqlitePool, channel_id: i64) -> bool {
    within(
        sqlx::query_scalar::<_, i64>("SELECT enabled FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(db),
    )
    .await
    .unwrap()
        == 1
}

async fn key_id_for_channel(db: &SqlitePool, channel_id: i64) -> i64 {
    within(
        sqlx::query_scalar::<_, i64>("SELECT id FROM api_keys WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(db),
    )
    .await
    .unwrap()
}

// --- Channels page -------------------------------------------------------

#[tokio::test]
async fn channel_create_form_post_renders_new_row_with_masked_key() {
    let db = test_pool().await;
    let (status, body) = post_form(
        &db,
        "/admin/ui/channels",
        &form(&[
            ("name", "primary"),
            ("base_url", "https://api.example.com"),
            ("api_key", "sk-upstream-secret-1234567890"),
            ("allowed_models", "gpt-4o"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("primary"));
    assert!(body.contains("https://api.example.com"));
    assert!(body.contains("sk-u...7890"));
    assert!(!body.contains("sk-upstream-secret-1234567890"));
    assert!(body.contains("gpt-4o"));
    assert!(body.contains("/admin/ui/channels/1/toggle"));
}

#[tokio::test]
async fn channel_create_with_invalid_base_url_renders_field_error_not_a_crash() {
    let db = test_pool().await;
    let (status, body) = post_form(
        &db,
        "/admin/ui/channels",
        &form(&[
            ("name", "broken"),
            ("base_url", "not a url"),
            ("api_key", "sk-upstream-secret-1234567890"),
            ("allowed_models", ""),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("invalid base_url: not a url"));
    assert!(body.contains("field-error"));
    // The submitted values are preserved in the re-rendered form.
    assert!(body.contains("value=\"not a url\""));
    // Nothing was stored.
    let count: i64 = within(sqlx::query_scalar("SELECT COUNT(*) FROM channels").fetch_one(&db))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn channel_edit_form_post_renames_and_keeps_key_when_field_is_empty() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/channels/{channel_id}"),
        &form(&[
            ("name", "renamed"),
            ("base_url", "https://api.example.com/v1"),
            ("api_key", ""),
            ("allowed_models", "gpt-4o\ngpt-4o-mini"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("renamed"));

    let (name, api_key, allowed): (String, String, Option<String>) = within(
        sqlx::query_as("SELECT name, api_key, allowed_models FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert_eq!(name, "renamed");
    // An empty key field means "keep the current key".
    assert_eq!(api_key, "sk-upstream-secret-1234567890");
    let allowed: Vec<String> =
        serde_json::from_str(allowed.as_deref().expect("whitelist must be set")).unwrap();
    assert_eq!(
        allowed,
        vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]
    );
}

#[tokio::test]
async fn channel_toggle_flips_enabled_state_both_ways() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/channels/{channel_id}/toggle"),
        &form(&[("enabled", "false")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge off"));
    assert!(!channel_enabled(&db, channel_id).await);

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/channels/{channel_id}/toggle"),
        &form(&[("enabled", "true")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge ok"));
    assert!(channel_enabled(&db, channel_id).await);
}

#[tokio::test]
async fn channel_delete_with_bound_keys_shows_count_and_disable_instead_flow() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    common::seed_key(&db, channel_id).await;

    let (status, body) =
        post_form(&db, &format!("/admin/ui/channels/{channel_id}/delete"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("1 client keys are still bound"));
    assert!(body.contains("Disable instead"));
    // The channel survives the refused delete.
    assert!(channel_enabled(&db, channel_id).await);

    // The offered disable-instead action flips the channel off.
    let (status, _) = post_form(
        &db,
        &format!("/admin/ui/channels/{channel_id}/toggle"),
        &form(&[("enabled", "false")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!channel_enabled(&db, channel_id).await);
}

#[tokio::test]
async fn channel_delete_clean_path_removes_the_row() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) =
        post_form(&db, &format!("/admin/ui/channels/{channel_id}/delete"), "").await;
    assert_eq!(status, StatusCode::OK);
    // The row is gone (its base_url only ever renders inside a row).
    assert!(!body.contains("api.example.com"));
    assert!(body.contains("No channels yet"));
    let count: i64 = within(sqlx::query_scalar("SELECT COUNT(*) FROM channels").fetch_one(&db))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// --- Keys page ------------------------------------------------------------

/// Extracts the full key from the reveal modal fragment.
fn full_key_from(body: &str) -> String {
    let after = body
        .split("reveal-key\">")
        .nth(1)
        .expect("reveal modal must be present");
    after
        .split("</div>")
        .next()
        .expect("key value must be closed")
        .to_string()
}

#[tokio::test]
async fn key_create_form_post_returns_full_key_exactly_once_in_the_modal() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = post_form(
        &db,
        "/admin/ui/keys",
        &form(&[("channel_id", &channel_id.to_string()), ("name", "ci-bot")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let full_key = full_key_from(&body);
    assert!(full_key.starts_with("hyd-"));
    assert!(full_key.len() > "hyd-".len() + 10);
    // Exactly one occurrence: the modal. The refreshed list carries the
    // display prefix, which is a prefix of the full key, not a match.
    assert_eq!(body.matches(&full_key).count(), 1);
    // The modal's non-dismissable UX is all present.
    assert!(body.contains("only time the full key is shown"));
    assert!(body.contains("I have stored the key somewhere safe"));
    assert!(body.contains("reveal-close"));
    assert!(body.contains("disabled>Close"));
    assert!(body.contains("reveal-curl"));
    assert!(body.contains("ci-bot"));

    // The stored hash matches the revealed plaintext; the plaintext is
    // nowhere in the database.
    let hash = hydra::keys::sha256_hex(full_key.as_bytes());
    let stored: i64 = within(
        sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE key_hash = ?")
            .bind(&hash)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert_eq!(stored, 1);

    // After the reveal, the page shows the prefix only.
    let (status, page) = get_page(&db, "/admin/ui/keys").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains(&full_key));
    assert!(page.contains(&full_key[..12]));
}

#[tokio::test]
async fn key_toggle_flips_enabled_state_both_ways() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let key_id = {
        common::seed_key(&db, channel_id).await;
        key_id_for_channel(&db, channel_id).await
    };

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}/toggle"),
        &form(&[("enabled", "false")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge off"));

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}/toggle"),
        &form(&[("enabled", "true")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge ok"));
    let enabled: i64 = within(
        sqlx::query_scalar::<_, i64>("SELECT enabled FROM api_keys WHERE id = ?")
            .bind(key_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert_eq!(enabled, 1);
}

#[tokio::test]
async fn key_delete_renders_deleted_badge_and_second_delete_shows_banner() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    common::seed_key(&db, channel_id).await;
    let key_id = key_id_for_channel(&db, channel_id).await;

    let (status, body) = post_form(&db, &format!("/admin/ui/keys/{key_id}/delete"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge deleted"));
    let deleted_at: Option<String> = within(
        sqlx::query_scalar("SELECT deleted_at FROM api_keys WHERE id = ?")
            .bind(key_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert!(deleted_at.is_some());

    // A deleted key shows no actions and stays immutable.
    let (status, body) = post_form(&db, &format!("/admin/ui/keys/{key_id}/delete"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("key is already deleted"));
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}/toggle"),
        &form(&[("enabled", "true")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("key is deleted and cannot be edited"));
}

#[tokio::test]
async fn key_create_for_unknown_or_disabled_channel_renders_error_banner() {
    let db = test_pool().await;
    let (status, body) = post_form(
        &db,
        "/admin/ui/keys",
        &form(&[("channel_id", "999"), ("name", "")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("channel not found"));
    assert!(!body.contains("reveal-key"));

    let channel_id = create_channel(&db).await;
    common::set_channel_enabled(&db, channel_id, false).await;
    let (status, body) = post_form(
        &db,
        "/admin/ui/keys",
        &form(&[("channel_id", &channel_id.to_string()), ("name", "")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("channel is disabled"));
    assert!(!body.contains("reveal-key"));
    // No key row was created by either attempt.
    let count: i64 = within(sqlx::query_scalar("SELECT COUNT(*) FROM api_keys").fetch_one(&db))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// --- CSRF and auth gates --------------------------------------------------

/// Every mutating admin UI route with a body its handler accepts, built for
/// a freshly created channel. Single source of truth for the systematic
/// CSRF test; the routers in `admin_ui_channels` and `admin_ui_keys` must
/// stay in sync with this list, and every entry here sits inside the
/// CSRF-guarded `/admin/ui` nest.
fn mutating_ui_routes(channel_id: i64) -> Vec<(String, String)> {
    vec![
        (
            "/admin/ui/channels".to_string(),
            form(&[
                ("name", "created"),
                ("base_url", "https://api.example.com"),
                ("api_key", "sk-upstream-secret-1234567890"),
                ("allowed_models", ""),
            ]),
        ),
        (
            format!("/admin/ui/channels/{channel_id}"),
            form(&[
                ("name", "edited"),
                ("base_url", "https://api.example.com/v1"),
                ("api_key", ""),
                ("allowed_models", "gpt-4o"),
            ]),
        ),
        (
            format!("/admin/ui/channels/{channel_id}/toggle"),
            form(&[("enabled", "false")]),
        ),
        (
            format!("/admin/ui/channels/{channel_id}/delete"),
            String::new(),
        ),
        (
            "/admin/ui/keys".to_string(),
            form(&[("channel_id", &channel_id.to_string()), ("name", "")]),
        ),
        (
            "/admin/ui/keys/1/toggle".to_string(),
            form(&[("enabled", "false")]),
        ),
        ("/admin/ui/keys/1/delete".to_string(), String::new()),
    ]
}

#[tokio::test]
async fn cross_site_posts_to_every_mutating_ui_route_return_403() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;

    let routes = mutating_ui_routes(channel_id);
    assert_eq!(routes.len(), 7, "the route list must cover every POST");
    for (uri, body) in &routes {
        let (status, _) = send(
            &db,
            "POST",
            uri,
            Some(body.clone()),
            Some(("Sec-Fetch-Site", "cross-site")),
            Some(&auth_header()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "cross-site POST to {uri}");
    }

    // Nothing mutated behind the 403s: the channel survives enabled and no
    // key was issued by the cross-site key-creation attempt.
    let count: i64 = within(sqlx::query_scalar("SELECT COUNT(*) FROM channels").fetch_one(&db))
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(channel_enabled(&db, channel_id).await);
    let key_count: i64 = within(sqlx::query_scalar("SELECT COUNT(*) FROM api_keys").fetch_one(&db))
        .await
        .unwrap();
    assert_eq!(key_count, 0);
}

#[tokio::test]
async fn ui_posts_without_credentials_return_401() {
    let db = test_pool().await;
    let (status, _) = send(
        &db,
        "POST",
        "/admin/ui/channels",
        Some(form(&[
            ("name", "x"),
            ("base_url", "https://api.example.com"),
            ("api_key", "sk-anything"),
            ("allowed_models", ""),
        ])),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send(
        &db,
        "POST",
        "/admin/ui/keys",
        Some(form(&[("channel_id", "1"), ("name", "")])),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- Access control matrix --------------------------------------------------

/// Every GET route on the admin UI; `/admin/ui` redirects when
/// authenticated, everything else answers 200. Built with a real channel id
/// so the edit-form route resolves an existing row.
fn ui_get_routes(channel_id: i64) -> Vec<(String, StatusCode)> {
    vec![
        ("/admin/ui".to_string(), StatusCode::SEE_OTHER),
        ("/admin/ui/channels".to_string(), StatusCode::OK),
        ("/admin/ui/channels/form".to_string(), StatusCode::OK),
        (
            format!("/admin/ui/channels/{channel_id}/edit"),
            StatusCode::OK,
        ),
        ("/admin/ui/keys".to_string(), StatusCode::OK),
        ("/admin/ui/keys/form".to_string(), StatusCode::OK),
        ("/admin/ui/logs".to_string(), StatusCode::OK),
        ("/admin/ui/static/htmx.min.js".to_string(), StatusCode::OK),
        ("/admin/ui/static/admin.css".to_string(), StatusCode::OK),
    ]
}

#[tokio::test]
async fn every_ui_get_route_is_401_without_credentials_and_serves_with_them() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;

    for (uri, expected) in ui_get_routes(channel_id) {
        let (status, _) = send(&db, "GET", &uri, None, None, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "unauthenticated GET {uri}"
        );

        let (status, _) = send(&db, "GET", &uri, None, None, Some(&auth_header())).await;
        assert_eq!(status, expected, "authenticated GET {uri}");
    }
}
