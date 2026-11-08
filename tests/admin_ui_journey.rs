//! The whole-UI journey: a channel is created through the admin UI form, a
//! client key is issued through the UI and shown once in the reveal modal,
//! a client call with that key is proxied to a mock upstream through the
//! real router, and the resulting log row is visible on the logs page with
//! the key filter applied. No row is ever seeded directly except by the UI
//! and the proxy itself.

mod common;

use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use sqlx::{Row, SqlitePool};
use tower::ServiceExt;

use common::{fixed_response_router, within, Recorder};

fn auth_header() -> String {
    format!("Basic {}", STANDARD.encode("admin:secret"))
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

/// Sends a request through the full router, as a browser form would.
async fn send(
    db: &SqlitePool,
    method: &str,
    uri: &str,
    body: Option<String>,
    authorization: Option<&str>,
) -> (StatusCode, String) {
    let app = common::test_app(db.clone());
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(auth) = authorization {
        builder = builder.header("Authorization", auth);
    }
    if body.is_some() {
        builder = builder.header("Content-Type", "application/x-www-form-urlencoded");
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

/// Extracts the full key from the reveal modal fragment; the same parsing
/// the browser performs when the admin copies the key.
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

/// Polls until one request_logs row exists and returns its status.
async fn wait_for_log(db: &SqlitePool) -> Option<i64> {
    let deadline = Instant::now() + common::TEST_TIMEOUT;
    loop {
        let row =
            within(sqlx::query("SELECT status FROM request_logs ORDER BY id").fetch_optional(db))
                .await
                .unwrap();
        if let Some(row) = row {
            return row.try_get::<Option<i64>, _>("status").unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "the proxied request was never logged"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn ui_created_channel_and_key_proxy_and_show_up_in_the_logs_page() {
    let db = common::test_pool().await;

    // The mock upstream answers any path with a fixed completion carrying
    // usage, so the log row can be verified down to the tokens.
    let recorder = Recorder::default();
    let completion = serde_json::json!({
        "id": "chatcmpl-ui-journey",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "journey ok" },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 },
    });
    let upstream = common::spawn_upstream(fixed_response_router(
        recorder.clone(),
        StatusCode::OK,
        "application/json",
        completion.to_string(),
    ))
    .await;

    // Step 1: create the channel through the UI form.
    let (status, body) = send(
        &db,
        "POST",
        "/admin/ui/channels",
        Some(form(&[
            ("name", "journey"),
            ("base_url", &upstream),
            ("api_key", "sk-upstream-secret-1234567890"),
            ("allowed_models", "gpt-4o"),
        ])),
        Some(&auth_header()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("journey"), "the new channel row must render");

    let channel_id = within(
        sqlx::query_scalar::<_, i64>("SELECT id FROM channels WHERE name = 'journey'")
            .fetch_one(&db),
    )
    .await
    .unwrap();

    // Step 2: issue a client key through the UI; the modal fragment carries
    // the full key exactly once.
    let (status, body) = send(
        &db,
        "POST",
        "/admin/ui/keys",
        Some(form(&[
            ("channel_id", &channel_id.to_string()),
            ("name", "journey-bot"),
        ])),
        Some(&auth_header()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let full_key = full_key_from(&body);
    assert!(full_key.starts_with("hyd-"));
    assert_eq!(body.matches(&full_key).count(), 1);

    // The stored page never shows the key again.
    let (status, page) = send(&db, "GET", "/admin/ui/keys", None, Some(&auth_header())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains(&full_key));
    assert!(page.contains(&full_key[..12]));

    // Step 3: a client calls the proxy with the revealed key.
    let app = common::test_app(db.clone());
    let chat = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let request = Request::post("/v1/chat/completions")
        .header("Authorization", format!("Bearer {full_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(chat.to_string()))
        .unwrap();
    let response = within(hydra::http::router(app).oneshot(request))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = within(to_bytes(response.into_body(), usize::MAX))
        .await
        .unwrap();
    let proxied: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(proxied["choices"][0]["message"]["content"], "journey ok");
    // The mock upstream saw the substituted upstream key, not the client's.
    let recorded = recorder.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].header("authorization").unwrap(),
        "Bearer sk-upstream-secret-1234567890"
    );

    // Step 4: the log row lands in the background and shows up on the logs
    // page, filtered by the channel the UI created.
    assert_eq!(wait_for_log(&db).await, Some(200));
    let (status, logs) = send(
        &db,
        "GET",
        &format!("/admin/ui/logs?channel_id={channel_id}&status_class=2xx"),
        None,
        Some(&auth_header()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(logs.contains("gpt-4o"));
    assert!(logs.contains("journey"));
    assert!(logs.contains("3 / 2"));
    assert!(logs.contains("badge status-2xx\">200"));
    assert!(!logs.contains("empty-state"));

    // The complementary filter is empty: the same request does not match
    // the errors-only view.
    let (status, logs) = send(
        &db,
        "GET",
        &format!("/admin/ui/logs?channel_id={channel_id}&status_class=errors"),
        None,
        Some(&auth_header()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(logs.contains("No requests match the current filters."));
}
