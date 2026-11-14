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

/// Sends a POST and returns its status together with the `Location` header,
/// for the routes that answer with a redirect instead of a page.
async fn post_redirect(db: &SqlitePool, uri: &str, body: &str) -> (StatusCode, Option<String>) {
    let app = test_app(db.clone());
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Authorization", auth_header())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = within(hydra::http::router(app).oneshot(request))
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .map(|value| value.to_str().unwrap().to_string());
    (status, location)
}

/// Creates a channel through the admin JSON API and returns its id.
async fn create_channel(db: &SqlitePool) -> i64 {
    create_channel_named(db, "primary").await
}

/// Creates a channel with the given name through the admin JSON API and
/// returns its id, for tests that need more than one bindable channel.
async fn create_channel_named(db: &SqlitePool, name: &str) -> i64 {
    let app = test_app(db.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/admin/channels")
        .header("Authorization", auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": name,
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

async fn key_channel(db: &SqlitePool, key_id: i64) -> i64 {
    within(
        sqlx::query_scalar::<_, i64>("SELECT channel_id FROM api_keys WHERE id = ?")
            .bind(key_id)
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
    assert!(
        body.contains("href=\"/admin/ui/channels/1\""),
        "the row must link to the detail page: {body}"
    );
    assert!(
        !body.contains("/toggle") && !body.contains("actions-col"),
        "the list carries no row actions any more: {body}"
    );
}

#[tokio::test]
async fn channels_list_empty_state_spans_the_seven_columns() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/channels").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No channels yet"), "{body}");
    assert!(
        body.contains("colspan=\"7\"") && !body.contains("colspan=\"8\""),
        "the empty state spans the whole row once the actions column is gone: {body}"
    );
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
    // The refusal renders on the detail page, whose banner carries a plain
    // form that turns the channel off.
    assert!(
        body.contains(&format!(
            "action=\"/admin/ui/channels/{channel_id}/toggle\""
        )),
        "{body}"
    );
    assert!(body.contains("value=\"false\""), "{body}");
    assert!(
        body.contains(&format!("action=\"/admin/ui/channels/{channel_id}\"")),
        "the detail page form stays available: {body}"
    );
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
async fn channel_delete_clean_path_redirects_to_the_list() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, location) =
        post_redirect(&db, &format!("/admin/ui/channels/{channel_id}/delete"), "").await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "a hard delete sends the browser back to the list"
    );
    assert_eq!(location.as_deref(), Some("/admin/ui/channels"));
    let count: i64 = within(sqlx::query_scalar("SELECT COUNT(*) FROM channels").fetch_one(&db))
        .await
        .unwrap();
    assert_eq!(count, 0);
    // The list renders the empty state again.
    let (status, body) = get_page(&db, "/admin/ui/channels").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No channels yet"), "{body}");
}

// --- Channel detail page --------------------------------------------------

#[tokio::test]
async fn channel_detail_page_renders_metadata_the_edit_form_and_the_actions() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;

    let (status, body) = get_page(&db, &format!("/admin/ui/channels/{channel_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Metadata, with the upstream key masked as everywhere else.
    assert!(body.contains("primary"), "{body}");
    assert!(body.contains("https://api.example.com"), "{body}");
    assert!(body.contains("sk-u...7890"), "{body}");
    assert!(!body.contains("sk-upstream-secret-1234567890"), "{body}");

    // The edit form posts back to the same page.
    assert!(
        body.contains(&format!("action=\"/admin/ui/channels/{channel_id}\"")),
        "{body}"
    );
    assert!(body.contains("Save channel"), "{body}");

    // Enable/disable and delete live here, with the confirm on the delete.
    assert!(
        body.contains(&format!(
            "action=\"/admin/ui/channels/{channel_id}/toggle\""
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "action=\"/admin/ui/channels/{channel_id}/delete\""
        )),
        "{body}"
    );
    assert!(
        body.contains("data-confirm=\"Delete this channel?\""),
        "{body}"
    );

    // A plain link back to the list.
    assert!(body.contains("href=\"/admin/ui/channels\""), "{body}");

    // The detail page carries no htmx: plain forms and a plain link only.
    assert!(!body.contains("hx-"), "{body}");
}

#[tokio::test]
async fn channel_detail_page_for_an_unknown_id_shows_the_not_found_banner() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/channels/999").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a missing channel is a banner page, not a rejection"
    );
    assert!(body.contains("channel not found"), "{body}");
    assert!(
        !body.contains("Save channel"),
        "no form for a channel that is not there: {body}"
    );
    assert!(!body.contains("/999/toggle"), "{body}");
}

#[tokio::test]
async fn channel_detail_edit_post_rejects_invalid_values_and_keeps_them_in_the_form() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/channels/{channel_id}"),
        &form(&[
            ("name", "broken"),
            ("base_url", "not a url"),
            ("api_key", ""),
            ("allowed_models", ""),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("invalid base_url: not a url"), "{body}");
    assert!(body.contains("field-error"), "{body}");
    // The rejected submission stays in the form for correction.
    assert!(body.contains("value=\"not a url\""), "{body}");
    // Nothing was stored: the created name survives.
    let name: String = within(
        sqlx::query_scalar("SELECT name FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert_eq!(name, "primary");
}

#[tokio::test]
async fn channel_detail_edit_post_success_rerenders_the_page_with_a_notice() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/channels/{channel_id}"),
        &form(&[
            ("name", "renamed"),
            ("base_url", "https://api.example.com/v1"),
            ("api_key", ""),
            ("allowed_models", "gpt-4o"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("channel updated"), "{body}");
    assert!(body.contains("value=\"renamed\""), "{body}");
    assert!(body.contains("https://api.example.com/v1"), "{body}");
    // The saved values are the stored ones.
    let base_url: String = within(
        sqlx::query_scalar("SELECT base_url FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert_eq!(base_url, "https://api.example.com/v1");
}

#[tokio::test]
async fn channel_detail_toggle_renders_the_new_state_on_the_detail_page() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/channels/{channel_id}/toggle"),
        &form(&[("enabled", "false")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("channel disabled"), "{body}");
    assert!(body.contains("badge off"), "{body}");
    assert!(!channel_enabled(&db, channel_id).await);
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
async fn keys_list_empty_state_spans_the_six_columns() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/keys").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No keys yet"), "{body}");
    assert!(
        body.contains("colspan=\"6\"") && !body.contains("colspan=\"7\""),
        "the empty state spans the whole row once the actions column is gone: {body}"
    );
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
    assert!(
        body.contains("key disabled"),
        "the toggle answers on the key's detail page: {body}"
    );

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}/toggle"),
        &form(&[("enabled", "true")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge ok"));
    assert!(body.contains("key enabled"), "{body}");
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
async fn key_delete_renders_the_read_only_detail_page_and_keeps_the_row() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    common::seed_key(&db, channel_id).await;
    let key_id = key_id_for_channel(&db, channel_id).await;

    // A soft delete answers with the key's own page, not a redirect: the row
    // still exists.
    let (status, body) = post_form(&db, &format!("/admin/ui/keys/{key_id}/delete"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge deleted"));
    assert!(
        body.contains("key is deleted and cannot be edited"),
        "the deleted state explains itself: {body}"
    );
    let deleted_at: Option<String> = within(
        sqlx::query_scalar("SELECT deleted_at FROM api_keys WHERE id = ?")
            .bind(key_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert!(deleted_at.is_some());

    // The read-only page carries no mutation form of any kind.
    assert!(
        !body.contains("Save key") && !body.contains("/toggle") && !body.contains("/delete"),
        "a deleted key offers no edit, toggle or delete form: {body}"
    );

    // A second delete and a toggle both refuse with the shared message.
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
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[("name", "x"), ("monthly_limit_usd", "1")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("key is deleted and cannot be edited"));
    assert_eq!(key_name(&db, key_id).await, None);
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

// --- Keys page: monthly limit ---------------------------------------------

async fn key_limit(db: &SqlitePool, key_id: i64) -> Option<i64> {
    within(
        sqlx::query_scalar("SELECT monthly_limit_micro_usd FROM api_keys WHERE id = ?")
            .bind(key_id)
            .fetch_one(db),
    )
    .await
    .unwrap()
}

async fn key_name(db: &SqlitePool, key_id: i64) -> Option<String> {
    within(
        sqlx::query_scalar("SELECT name FROM api_keys WHERE id = ?")
            .bind(key_id)
            .fetch_one(db),
    )
    .await
    .unwrap()
}

/// Issues a key through the UI form with the given limit field text.
async fn issue_key(
    db: &SqlitePool,
    channel_id: i64,
    name: &str,
    limit: &str,
) -> (StatusCode, String) {
    post_form(
        db,
        "/admin/ui/keys",
        &form(&[
            ("channel_id", &channel_id.to_string()),
            ("name", name),
            ("monthly_limit_usd", limit),
        ]),
    )
    .await
}

#[tokio::test]
async fn key_create_form_post_stores_the_limit_and_shows_it_with_the_inactive_badge() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = issue_key(&db, channel_id, "ci-bot", "50.00").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("reveal-key"),
        "the key must still be revealed"
    );

    let key_id = key_id_for_channel(&db, channel_id).await;
    assert_eq!(key_limit(&db, key_id).await, Some(50_000_000));
    assert_eq!(key_name(&db, key_id).await.as_deref(), Some("ci-bot"));
    assert!(body.contains("$50.00"), "{body}");
    // A channel without prices cannot limit, so the stored limit is flagged
    // as inactive in the row.
    assert!(body.contains("limit inactive"), "{body}");
    // The row opens the detail page; the row actions are gone, and so is the
    // tooltip that explained the inactive limit (the detail page states it).
    assert!(
        body.contains(&format!("href=\"/admin/ui/keys/{key_id}\"")),
        "the row must link to the detail page: {body}"
    );
    assert!(
        !body.contains("actions-col") && !body.contains("/toggle") && !body.contains("/edit"),
        "the list carries no row actions any more: {body}"
    );
    assert!(
        !body.contains("title=\"the limit is stored but not enforced"),
        "the inactive-limit tooltip is unreachable under the row overlay: {body}"
    );
}

#[tokio::test]
async fn key_without_a_limit_renders_a_dash_and_no_inactive_badge() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, body) = issue_key(&db, channel_id, "ci-bot", "").await;
    assert_eq!(status, StatusCode::OK);
    let key_id = key_id_for_channel(&db, channel_id).await;
    assert_eq!(key_limit(&db, key_id).await, None);
    // The unpriced channel would warn about an inactive limit, but there is
    // no limit to be inactive.
    assert!(!body.contains("limit inactive"), "{body}");
    assert!(
        body.contains("<span class=\"text-muted\">-</span>"),
        "{body}"
    );
}

#[tokio::test]
async fn the_inactive_limit_badge_clears_once_the_channel_prices_are_complete() {
    let db = test_pool().await;
    let (status, _) = post_form(
        &db,
        "/admin/ui/channels",
        &form(&[
            ("name", "priced"),
            ("base_url", "https://api.example.com"),
            ("api_key", "sk-upstream-secret-1234567890"),
            ("allowed_models", "gpt-4o"),
            ("model_prices", "gpt-4o: 1, 1, 1, 1"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id: i64 = within(
        sqlx::query_scalar("SELECT id FROM channels ORDER BY id DESC LIMIT 1").fetch_one(&db),
    )
    .await
    .unwrap();

    let (status, body) = issue_key(&db, channel_id, "ci-bot", "50.00").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("$50.00"), "{body}");
    assert!(
        !body.contains("limit inactive"),
        "a fully priced channel makes the limit effective: {body}"
    );
}

#[tokio::test]
async fn key_create_form_rejects_a_bad_limit_without_issuing_a_key() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    for refused in ["abc", "-1", "0.0000005", "1000000001"] {
        let (status, body) = issue_key(&db, channel_id, "ci-bot", refused).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("field-error"), "{refused}: {body}");
        assert!(
            !body.contains("reveal-key"),
            "{refused}: no key may be issued"
        );
        // The submitted values come back so the admin can correct them.
        assert!(
            body.contains(&format!("value=\"{refused}\"")),
            "{refused}: {body}"
        );
    }
    let count: i64 = within(sqlx::query_scalar("SELECT COUNT(*) FROM api_keys").fetch_one(&db))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// --- Key detail page ------------------------------------------------------

/// The key id of the single seeded key.
async fn only_key_id(db: &SqlitePool) -> i64 {
    within(
        sqlx::query_scalar::<_, i64>("SELECT id FROM api_keys ORDER BY id LIMIT 1").fetch_one(db),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn key_detail_page_renders_metadata_the_edit_form_and_the_actions() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, _) = issue_key(&db, channel_id, "ci-bot", "50.00").await;
    assert_eq!(status, StatusCode::OK);
    let key_id = key_id_for_channel(&db, channel_id).await;

    let (status, body) = get_page(&db, &format!("/admin/ui/keys/{key_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Metadata: the display prefix, the label, the stored limit, the creation
    // stamp.
    let prefix: String = within(
        sqlx::query_scalar("SELECT display_prefix FROM api_keys WHERE id = ?")
            .bind(key_id)
            .fetch_one(&db),
    )
    .await
    .unwrap();
    assert!(body.contains(&prefix), "{body}");
    assert!(body.contains("ci-bot"), "{body}");
    assert!(body.contains("$50.00"), "{body}");
    assert!(body.contains("2026-"), "{body}");

    // The bound channel is visible text and a link to its own page.
    assert!(
        body.contains(&format!(
            "href=\"/admin/ui/channels/{channel_id}\">primary</a>"
        )),
        "{body}"
    );

    // The edit form posts back to this page, with the bound channel selected
    // in the select.
    assert!(
        body.contains(&format!("action=\"/admin/ui/keys/{key_id}\"")),
        "{body}"
    );
    assert!(body.contains("id=\"key-channel\""), "{body}");
    assert!(
        body.contains(&format!(
            "<option value=\"{channel_id}\" selected>primary</option>"
        )),
        "{body}"
    );
    assert!(body.contains("value=\"ci-bot\""), "{body}");
    assert!(body.contains("value=\"50.00\""), "{body}");
    assert!(body.contains("Save key"), "{body}");

    // Enable/disable and delete live here, with the confirm on the delete.
    assert!(
        body.contains(&format!("action=\"/admin/ui/keys/{key_id}/toggle\"")),
        "{body}"
    );
    assert!(
        body.contains(&format!("action=\"/admin/ui/keys/{key_id}/delete\"")),
        "{body}"
    );
    assert!(
        body.contains(
            "data-confirm=\"Delete this key? Request logs referencing it are preserved.\""
        ),
        "{body}"
    );
    assert!(body.contains("href=\"/admin/ui/keys\""), "{body}");

    // No htmx on the page, and the plaintext key exists nowhere: the reveal
    // modal stays on the creation response only.
    assert!(!body.contains("hx-"), "{body}");
    assert!(!body.contains("reveal-key"), "{body}");
}

#[tokio::test]
async fn key_detail_page_states_why_an_inactive_limit_is_not_enforced() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, _) = issue_key(&db, channel_id, "ci-bot", "50.00").await;
    assert_eq!(status, StatusCode::OK);
    let key_id = key_id_for_channel(&db, channel_id).await;

    let (status, body) = get_page(&db, &format!("/admin/ui/keys/{key_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("limit inactive"), "{body}");
    assert!(
        body.contains("The limit is stored but not enforced"),
        "the explanation the row tooltip used to carry is visible text here: {body}"
    );

    // The same page clears it back to unlimited: nothing left to explain.
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[("name", "ci-bot"), ("monthly_limit_usd", "")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("unlimited"), "{body}");
    assert!(!body.contains("limit inactive"), "{body}");
}

#[tokio::test]
async fn key_detail_page_for_an_unknown_id_shows_the_not_found_banner() {
    let db = test_pool().await;
    let (status, body) = get_page(&db, "/admin/ui/keys/999").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a missing key is a banner page, not a rejection"
    );
    assert!(body.contains("key not found"), "{body}");
    assert!(!body.contains("Save key"), "{body}");
    assert!(!body.contains("key-channel"), "{body}");
    assert!(!body.contains("/999/toggle"), "{body}");
}

#[tokio::test]
async fn key_detail_page_for_a_deleted_key_is_read_only() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    common::seed_key(&db, channel_id).await;
    let key_id = only_key_id(&db).await;
    let (status, _) = post_form(&db, &format!("/admin/ui/keys/{key_id}/delete"), "").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get_page(&db, &format!("/admin/ui/keys/{key_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("badge deleted"), "{body}");
    assert!(
        body.contains("key is deleted and cannot be edited"),
        "{body}"
    );
    assert!(
        !body.contains("Save key") && !body.contains("/toggle") && !body.contains("/delete"),
        "a deleted key offers no edit, toggle or delete form: {body}"
    );

    // The edit route is gone: the detail page replaced it.
    let (status, _) = get_page(&db, &format!("/admin/ui/keys/{key_id}/edit")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the old edit route was removed"
    );
}

#[tokio::test]
async fn key_detail_edit_post_renames_and_replaces_the_limit() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, _) = issue_key(&db, channel_id, "ci-bot", "50.00").await;
    assert_eq!(status, StatusCode::OK);
    let key_id = key_id_for_channel(&db, channel_id).await;

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[("name", "renamed"), ("monthly_limit_usd", "7.5")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(key_name(&db, key_id).await.as_deref(), Some("renamed"));
    assert_eq!(key_limit(&db, key_id).await, Some(7_500_000));
    assert!(body.contains("key updated"), "{body}");
    assert!(body.contains("value=\"renamed\""), "{body}");
    assert!(body.contains("$7.50"), "{body}");
    // The detail page re-renders with its form, so the next edit needs no
    // navigation back.
    assert!(body.contains("Save key"), "{body}");
}

#[tokio::test]
async fn key_detail_edit_post_clears_the_limit_and_the_label_with_empty_fields() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, _) = issue_key(&db, channel_id, "ci-bot", "50.00").await;
    assert_eq!(status, StatusCode::OK);
    let key_id = key_id_for_channel(&db, channel_id).await;

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[("name", ""), ("monthly_limit_usd", "")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(key_name(&db, key_id).await, None);
    assert_eq!(key_limit(&db, key_id).await, None);
    assert!(body.contains("key updated"), "{body}");
    assert!(!body.contains("$50.00"), "{body}");
}

#[tokio::test]
async fn key_detail_edit_post_rejects_a_bad_limit_and_keeps_the_stored_one() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    let (status, _) = issue_key(&db, channel_id, "ci-bot", "50.00").await;
    assert_eq!(status, StatusCode::OK);
    let key_id = key_id_for_channel(&db, channel_id).await;

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[("name", "renamed"), ("monthly_limit_usd", "0.0000005")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("finer than 1e-6 USD"), "{body}");
    assert!(body.contains("field-error"), "{body}");
    // Rejected means nothing changed: neither the limit nor the label.
    assert_eq!(key_limit(&db, key_id).await, Some(50_000_000));
    assert_eq!(key_name(&db, key_id).await.as_deref(), Some("ci-bot"));
    // The rejected submission stays in the form for correction.
    assert!(body.contains("value=\"renamed\""), "{body}");
}

#[tokio::test]
async fn key_detail_channel_select_moves_the_key_to_another_channel() {
    let db = test_pool().await;
    let first = create_channel_named(&db, "primary").await;
    let second = create_channel_named(&db, "secondary").await;
    common::seed_key(&db, first).await;
    let key_id = only_key_id(&db).await;
    assert_eq!(key_channel(&db, key_id).await, first);

    // Both enabled channels are offered, the bound one selected.
    let (status, body) = get_page(&db, &format!("/admin/ui/keys/{key_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!(
            "<option value=\"{first}\" selected>primary</option>"
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!("<option value=\"{second}\">secondary</option>")),
        "{body}"
    );

    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[
            ("name", "moved"),
            ("monthly_limit_usd", ""),
            ("channel_id", &second.to_string()),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("key updated"), "{body}");
    assert_eq!(key_channel(&db, key_id).await, second, "the binding moved");
    assert_eq!(key_name(&db, key_id).await.as_deref(), Some("moved"));
    // The page shows the new channel as its link text, and the select marks
    // it.
    assert!(
        body.contains(&format!(
            "href=\"/admin/ui/channels/{second}\">secondary</a>"
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "<option value=\"{second}\" selected>secondary</option>"
        )),
        "{body}"
    );
}

#[tokio::test]
async fn key_detail_channel_select_refuses_an_unknown_or_disabled_channel() {
    let db = test_pool().await;
    let first = create_channel_named(&db, "primary").await;
    let second = create_channel_named(&db, "secondary").await;
    common::seed_key(&db, first).await;
    let key_id = only_key_id(&db).await;

    // An unknown id is a form error: no panic, nothing written.
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[
            ("name", "renamed"),
            ("monthly_limit_usd", "5"),
            ("channel_id", "999"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("channel not found"), "{body}");
    assert!(body.contains("field-error"), "{body}");
    assert!(
        body.contains("value=\"renamed\""),
        "the rejected submission stays in the form: {body}"
    );
    assert_eq!(
        key_channel(&db, key_id).await,
        first,
        "the key is unchanged"
    );
    assert_eq!(key_name(&db, key_id).await, None);
    assert_eq!(key_limit(&db, key_id).await, None);

    // A disabled channel is refused the same way, and the rejected choice
    // stays visible in the select.
    common::set_channel_enabled(&db, second, false).await;
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[
            ("name", ""),
            ("monthly_limit_usd", ""),
            ("channel_id", &second.to_string()),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("channel is disabled"), "{body}");
    assert!(
        body.contains(&format!(
            "<option value=\"{second}\" selected>secondary (disabled)</option>"
        )),
        "{body}"
    );
    assert_eq!(key_channel(&db, key_id).await, first);
}

#[tokio::test]
async fn a_key_bound_to_a_disabled_channel_keeps_its_binding_and_stays_editable() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    common::seed_key(&db, channel_id).await;
    let key_id = only_key_id(&db).await;
    common::set_channel_enabled(&db, channel_id, false).await;

    // The stored binding is offered, marked, and selected: the admin can read
    // it and cannot lose it by accident.
    let (status, body) = get_page(&db, &format!("/admin/ui/keys/{key_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!(
            "<option value=\"{channel_id}\" selected>primary (disabled)</option>"
        )),
        "{body}"
    );

    // A save that changes only the label and the limit leaves the binding
    // alone, so a key bound to a channel that was disabled afterwards is
    // still editable.
    let (status, body) = post_form(
        &db,
        &format!("/admin/ui/keys/{key_id}"),
        &form(&[
            ("name", "ci-bot"),
            ("monthly_limit_usd", "5"),
            ("channel_id", &channel_id.to_string()),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("key updated"), "{body}");
    assert_eq!(key_channel(&db, key_id).await, channel_id);
    assert_eq!(key_name(&db, key_id).await.as_deref(), Some("ci-bot"));
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
        (
            "/admin/ui/keys/1".to_string(),
            form(&[("name", "edited"), ("monthly_limit_usd", "5")]),
        ),
        ("/admin/ui/keys/1/delete".to_string(), String::new()),
    ]
}

#[tokio::test]
async fn cross_site_posts_to_every_mutating_ui_route_return_403() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;

    let routes = mutating_ui_routes(channel_id);
    assert_eq!(routes.len(), 8, "the route list must cover every POST");
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
async fn ui_posts_without_credentials_redirect_to_the_login_page() {
    let db = test_pool().await;
    for (uri, body) in [
        (
            "/admin/ui/channels",
            form(&[
                ("name", "x"),
                ("base_url", "https://api.example.com"),
                ("api_key", "sk-anything"),
                ("allowed_models", ""),
            ]),
        ),
        ("/admin/ui/keys", form(&[("channel_id", "1"), ("name", "")])),
    ] {
        let (status, _) = send(&db, "POST", uri, Some(body), None, None).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "unauthenticated POST {uri} must be sent to the login page, not challenged"
        );
    }
}

// --- Access control matrix --------------------------------------------------

/// Every GET route on the admin UI with the status it answers
/// unauthenticated and with credentials. Pages without a session go to the
/// login page; the login page itself and the static assets the login page
/// needs are public; `/admin/ui` redirects once authenticated. Built with a
/// real channel id and a real key id so the detail routes resolve existing
/// rows.
fn ui_get_routes(channel_id: i64, key_id: i64) -> Vec<(String, StatusCode, StatusCode)> {
    vec![
        (
            "/admin/ui/login".to_string(),
            StatusCode::OK,
            StatusCode::OK,
        ),
        (
            "/admin/ui".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::SEE_OTHER,
        ),
        (
            "/admin/ui/channels".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        (
            "/admin/ui/channels/form".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        (
            format!("/admin/ui/channels/{channel_id}"),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        // A missing channel answers the not-found banner with 200, the
        // convention the removed channel edit route set.
        (
            "/admin/ui/channels/999".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        (
            "/admin/ui/keys".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        (
            "/admin/ui/keys/form".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        (
            format!("/admin/ui/keys/{key_id}"),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        // A missing key answers the not-found banner with 200, the convention
        // the removed key edit route set.
        (
            "/admin/ui/keys/999".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        (
            "/admin/ui/logs".to_string(),
            StatusCode::SEE_OTHER,
            StatusCode::OK,
        ),
        (
            "/admin/ui/static/htmx.min.js".to_string(),
            StatusCode::OK,
            StatusCode::OK,
        ),
        (
            "/admin/ui/static/admin.css".to_string(),
            StatusCode::OK,
            StatusCode::OK,
        ),
    ]
}

#[tokio::test]
async fn every_ui_get_route_redirects_without_a_session_and_serves_with_credentials() {
    let db = test_pool().await;
    let channel_id = create_channel(&db).await;
    common::seed_key(&db, channel_id).await;
    let key_id = key_id_for_channel(&db, channel_id).await;

    for (uri, unauthenticated, authenticated) in ui_get_routes(channel_id, key_id) {
        let (status, _) = send(&db, "GET", &uri, None, None, None).await;
        assert_eq!(status, unauthenticated, "unauthenticated GET {uri}");

        let (status, _) = send(&db, "GET", &uri, None, None, Some(&auth_header())).await;
        assert_eq!(status, authenticated, "authenticated GET {uri}");
    }
}
