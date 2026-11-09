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

use hydra::db::channels::{self as channel_rows, ChannelPatch, NewChannel};
use hydra::db::keys::{self as key_rows, NewKey};
use hydra::http::AppState;
use hydra::keys as keygen;

/// The upstream key seeded into every test channel; proxying must
/// substitute it for the client key.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
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
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
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
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub fn test_app(db: SqlitePool) -> AppState {
    AppState::new(
        hydra::db::Db::from(db),
        "admin".to_string(),
        "secret".to_string(),
        false,
        hydra::config::TrustedProxies::default(),
        hydra::flood::FloodCap::default(),
    )
}

/// Application state with `HYDRA_LOG_BODIES` enabled, for tests that assert
/// captured bodies.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub fn test_app_logging(db: SqlitePool) -> AppState {
    AppState::new(
        hydra::db::Db::from(db),
        "admin".to_string(),
        "secret".to_string(),
        true,
        hydra::config::TrustedProxies::default(),
        hydra::flood::FloodCap::default(),
    )
}

/// Application state with the given trusted-proxy networks, for tests that
/// exercise X-Forwarded-For resolution behind a trusted proxy.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub fn test_app_with_trusted_proxies(
    db: SqlitePool,
    trusted_proxies: hydra::config::TrustedProxies,
) -> AppState {
    AppState::new(
        hydra::db::Db::from(db),
        "admin".to_string(),
        "secret".to_string(),
        false,
        trusted_proxies,
        hydra::flood::FloodCap::default(),
    )
}

/// Application state with an explicit flood cap, for tests that exercise
/// the per-IP failed-authentication limit (production defaults are too
/// large to reach in a test).
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub fn test_app_with_flood(db: SqlitePool, flood_limit: u64, flood_window_secs: u64) -> AppState {
    AppState::new(
        hydra::db::Db::from(db),
        "admin".to_string(),
        "secret".to_string(),
        false,
        hydra::config::TrustedProxies::default(),
        hydra::flood::FloodCap::new(flood_limit, flood_window_secs),
    )
}

/// Application state with both an explicit flood cap and trusted-proxy
/// networks, for flood tests that vary the resolved client IP through the
/// `X-Forwarded-For` header behind a trusted loopback proxy.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub fn test_app_with_flood_and_trusted_proxies(
    db: SqlitePool,
    flood_limit: u64,
    flood_window_secs: u64,
    trusted_proxies: hydra::config::TrustedProxies,
) -> AppState {
    AppState::new(
        hydra::db::Db::from(db),
        "admin".to_string(),
        "secret".to_string(),
        false,
        trusted_proxies,
        hydra::flood::FloodCap::new(flood_limit, flood_window_secs),
    )
}

/// Serves the router on an ephemeral loopback port with ConnectInfo, the
/// production wiring: handlers can then see the peer address, which the
/// `oneshot` path cannot provide. Returns the base URL (no trailing slash).
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub async fn spawn_hydra_with_connect_info(app: AppState) -> String {
    let listener = within(TcpListener::bind("127.0.0.1:0")).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            hydra::http::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

/// Spawns the mock upstream on an ephemeral port and returns its base URL
/// (no trailing slash).
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
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
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub async fn unreachable_base_url() -> String {
    let listener = within(TcpListener::bind("127.0.0.1:0")).await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// Inserts a channel bound to `base_url` with the shared upstream key.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub async fn seed_channel(
    db: &SqlitePool,
    base_url: &str,
    allowed_models: Option<Vec<String>>,
) -> i64 {
    let channel = within(channel_rows::create(
        &hydra::db::Db::from(db.clone()),
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
/// Flips a channel's enabled flag the way the admin PATCH does, for tests
/// that cut a channel off after keys were already bound to it.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub async fn set_channel_enabled(db: &SqlitePool, channel_id: i64, enabled: bool) {
    let patched = within(channel_rows::update(
        &hydra::db::Db::from(db.clone()),
        channel_id,
        &ChannelPatch {
            name: None,
            base_url: None,
            api_key: None,
            allowed_models: None,
            enabled: Some(enabled),
        },
    ))
    .await
    .unwrap();
    assert!(
        patched.is_some_and(|channel| channel.enabled == enabled),
        "channel {channel_id} must exist for the flag flip"
    );
}

/// Inserts an enabled client key bound to the channel and returns the
/// plaintext (which is otherwise never stored).
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub async fn seed_key(db: &SqlitePool, channel_id: i64) -> String {
    let generated = keygen::generate();
    within(key_rows::create(
        &hydra::db::Db::from(db.clone()),
        &NewKey {
            key_hash: generated.key_hash,
            display_prefix: generated.display_prefix,
            name: None,
            channel_id,
            monthly_limit_micro_usd: None,
        },
    ))
    .await
    .unwrap();
    generated.full
}

/// Disables a key without deleting it, mirroring the admin PATCH.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub async fn disable_key(db: &SqlitePool, key_id: i64) {
    use hydra::db::keys::{KeyPatch, PatchOutcome};
    match within(key_rows::patch(
        &hydra::db::Db::from(db.clone()),
        key_id,
        &KeyPatch {
            name: None,
            enabled: Some(false),
            monthly_limit_micro_usd: None,
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
// Shared module: each test binary compiles its own copy and uses a subset
// of the fields, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
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

// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
impl Recorder {
    pub fn record(&self, recorded: Recorded) {
        self.requests.lock().unwrap().push(recorded);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// What the mock upstream replies with.
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
enum Reply {
    Fixed {
        status: StatusCode,
        content_type: &'static str,
        body: Bytes,
    },
    /// SSE: the chunks are sent with a delay between them so the passthrough
    /// is exercised over a real stream, not one pre-sized buffer.
    #[allow(dead_code)]
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
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
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
// Shared module: each test binary compiles its own copy and uses a subset
// of the helpers, so per-binary dead code is expected, not a smell.
#[allow(dead_code)]
pub fn sse_router(recorder: Recorder, chunks: Vec<Bytes>) -> Router {
    Router::new()
        .route("/{*path}", any(capture_and_reply))
        .with_state(MockState {
            recorder,
            reply: Arc::new(Reply::Sse { chunks }),
        })
}
