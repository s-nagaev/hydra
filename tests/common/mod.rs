//! Shared helpers for the integration test suite.
//!
//! The mock upstream is a real axum server bound to an ephemeral loopback
//! port and spawned in-process: no network-mocking libraries, no external
//! services. The router under test is driven through `tower::ServiceExt::oneshot`.
//! Every awaited step is wrapped in [`within`] so a deadlock fails the test
//! fast instead of hanging CI.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use tokio::net::TcpListener;

use hydra::db::channels::{self as channel_rows, NewChannel};
use hydra::db::keys::{self as key_rows, NewKey};
use hydra::http::{self, AppState};
use hydra::keys as keygen;

/// The upstream key seeded into every test channel; proxying must
/// substitute it for the client key.
pub const UPSTREAM_KEY: &str = "sk-upstream-secret";

/// Per-test cap on any single awaited step.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Wraps a future so a stuck await fails the test instead of hanging it.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("test step timed out")
}

/// A migrated in-memory SQLite pool. Single connection so every query sees
/// the same `:memory:` database.
pub async fn test_pool() -> SqlitePool {
    let pool = within(
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:"),
    )
    .await
    .unwrap();
    within(sqlx::migrate!().run(&pool)).await.unwrap();
    pool
}

/// Application state wired like production, with the test pool.
pub fn test_app(db: SqlitePool) -> AppState {
    AppState {
        db,
        admin_login: "admin".to_string(),
        admin_password: "secret".to_string(),
        client: http::upstream_client(),
    }
}

/// Spawns the mock upstream on an ephemeral port and returns its base URL
/// (no trailing slash).
pub async fn spawn_upstream(router: Router) -> String {
    let listener = within(TcpListener::bind("127.0.0.1:0")).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // The listener is held until serve takes it, so the port cannot be
        // stolen in between.
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// A base URL on a loopback port nothing is listening on: the listener is
/// bound, its address read, then dropped. Rebinding by another process in
/// the microseconds between drop and connect is negligible.
pub async fn unreachable_base_url() -> String {
    let listener = within(TcpListener::bind("127.0.0.1:0")).await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// Inserts a channel bound to `base_url` with the shared upstream key.
pub async fn seed_channel(
    db: &SqlitePool,
    base_url: &str,
    allowed_models: Option<Vec<String>>,
) -> i64 {
    let channel = within(channel_rows::create(
        db,
        &NewChannel {
            name: "test channel".to_string(),
            base_url: base_url.to_string(),
            api_key: UPSTREAM_KEY.to_string(),
            allowed_models,
        },
    ))
    .await
    .unwrap();
    channel.id
}

/// Inserts an enabled client key bound to the channel and returns the
/// plaintext (which is otherwise never stored).
pub async fn seed_key(db: &SqlitePool, channel_id: i64) -> String {
    let generated = keygen::generate();
    within(key_rows::create(
        db,
        &NewKey {
            key_hash: generated.key_hash,
            display_prefix: generated.display_prefix,
            name: None,
            channel_id,
        },
    ))
    .await
    .unwrap();
    generated.full
}

/// Disables a key without deleting it, mirroring the admin PATCH.
pub async fn disable_key(db: &SqlitePool, key_id: i64) {
    use hydra::db::keys::{KeyPatch, PatchOutcome};
    match within(key_rows::patch(
        db,
        key_id,
        &KeyPatch {
            name: None,
            enabled: Some(false),
        },
    ))
    .await
    .unwrap()
    {
        PatchOutcome::Updated(_) => {}
        PatchOutcome::NotFound => panic!("seeded key vanished before disable"),
        PatchOutcome::Deleted => panic!("seeded key was soft-deleted before disable"),
    }
}

/// A request the mock upstream received, with header names lowercased.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Thread-safe collector the mock handlers push into.
#[derive(Clone, Default)]
pub struct Recorder {
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Recorder {
    pub fn record(&self, recorded: Recorded) {
        self.requests.lock().unwrap().push(recorded);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// What the mock upstream replies with.
enum Reply {
    Fixed {
        status: StatusCode,
        content_type: &'static str,
        body: Bytes,
    },
    /// SSE: the chunks are sent with a delay between them so the passthrough
    /// is exercised over a real stream, not one pre-sized buffer.
    Sse { chunks: Vec<Bytes> },
}

#[derive(Clone)]
struct MockState {
    recorder: Recorder,
    reply: Arc<Reply>,
}

async fn capture_and_reply(
    State(state): State<MockState>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let received = to_bytes(body, usize::MAX).await.unwrap();
    state.recorder.record(Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value): (&HeaderName, &HeaderValue)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect(),
        body: received.to_vec(),
    });

    match &*state.reply {
        Reply::Fixed {
            status,
            content_type,
            body,
        } => Response::builder()
            .status(*status)
            .header("content-type", *content_type)
            .body(Body::from(body.clone()))
            .unwrap(),
        Reply::Sse { chunks } => {
            use tokio_stream::StreamExt as _;
            let stream = tokio_stream::iter(chunks.clone()).then(|chunk| async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok::<Bytes, std::convert::Infallible>(chunk)
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

/// Builds a mock upstream that records every request and answers with a
/// fixed status/body.
pub fn fixed_response_router(
    recorder: Recorder,
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> Router {
    Router::new()
        .route("/{*path}", any(capture_and_reply))
        .with_state(MockState {
            recorder,
            reply: Arc::new(Reply::Fixed {
                status,
                content_type,
                body: body.into(),
            }),
        })
}

/// Builds a mock upstream that records every request and answers with an
/// SSE stream of `chunks` (sent with delays so buffering would be visible
/// as an ordering hazard, and the collected bytes must still match).
pub fn sse_router(recorder: Recorder, chunks: Vec<Bytes>) -> Router {
    Router::new()
        .route("/{*path}", any(capture_and_reply))
        .with_state(MockState {
            recorder,
            reply: Arc::new(Reply::Sse { chunks }),
        })
}
