//! Background request logging for the chat proxy flow.
//!
//! Every authenticated `/v1/chat/completions` request gets exactly one
//! `request_logs` row, written by a detached task so the client flow never
//! waits on, or fails from, logging. Rejections that happen before the
//! handler runs (401/403 auth, 413 from the body extractor) cannot be
//! logged: they have no key row or no readable body to reference. `GET
//! /v1/models` is a lightweight discovery endpoint and is not logged.
//!
//! Duration is measured from handler start to response completion: for
//! non-streaming requests the moment the full upstream body has been read,
//! for streams the moment the last chunk was forwarded (or the response was
//! dropped). It is rounded up to whole milliseconds, so any completed
//! request logs at least 1.
//!
//! Body capture is opt-in via `HYDRA_LOG_BODIES`. When on, request bytes are
//! copied once (they are already buffered for the model peek) and response
//! bytes are teed: streaming chunks are refcount-cloned into an unbounded
//! channel whose send never blocks, so forwarding keeps its own backpressure
//! semantics. Both captures stop at [`BODY_CAPTURE_LIMIT`] (1 MiB); past the
//! cap bytes are simply not captured. Stream token counts are parsed from
//! the captured SSE buffer, so they need body logging on as well; with it
//! off, stream rows have NULL tokens and NULL bodies.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::Db;
use axum::body::Bytes;
use futures_util::{Stream, StreamExt};

use crate::db::request_logs::NewRequestLog;

/// Maximum number of response body bytes captured per request (1 MiB).
/// Bytes past the cap are not captured; nothing is appended to mark the
/// truncation.
pub const BODY_CAPTURE_LIMIT: usize = 1024 * 1024;

/// In-flight background log insert counter, so graceful shutdown can wait
/// briefly for the last rows to land before the process exits.
#[derive(Clone, Default)]
pub struct LogTracker {
    inflight: Arc<AtomicUsize>,
}

impl LogTracker {
    fn guard(&self) -> LogGuard {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        LogGuard {
            tracker: self.clone(),
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }
}

struct LogGuard {
    tracker: LogTracker,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self.tracker.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Inputs for one chat request's log context, gathered before the request
/// body is handed to the forwarder.
pub struct ChatLogInit<'a> {
    pub db: &'a Db,
    pub tracker: &'a LogTracker,
    pub log_bodies: bool,
    pub api_key_id: i64,
    pub channel_id: i64,
    pub model: Option<String>,
    pub start: Instant,
    pub request_body: &'a [u8],
}

/// Everything the detached log task needs about one chat request. Built in
/// the handler, finished exactly once by whichever path resolves the request.
pub struct ChatLogContext {
    db: Db,
    tracker: LogTracker,
    log_bodies: bool,
    api_key_id: i64,
    channel_id: i64,
    model: Option<String>,
    start: Instant,
    request_body: Option<String>,
}

impl ChatLogContext {
    pub fn new(init: ChatLogInit) -> Self {
        Self {
            db: init.db.clone(),
            tracker: init.tracker.clone(),
            log_bodies: init.log_bodies,
            api_key_id: init.api_key_id,
            channel_id: init.channel_id,
            model: init.model,
            start: init.start,
            request_body: init
                .log_bodies
                .then(|| capture_to_string(init.request_body)),
        }
    }

    /// Captured response body for a fully buffered (non-streaming) response,
    /// NULL unless body logging is on.
    pub fn capture_response_body(&self, bytes: &[u8]) -> Option<String> {
        self.log_bodies.then(|| capture_to_string(bytes))
    }

    /// Assembles the final row and dispatches the detached insert. Consumes
    /// the context so every request finishes its log exactly once.
    pub fn finish(
        self,
        status: Option<i64>,
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
        response_body: Option<String>,
        error: Option<String>,
    ) {
        spawn_insert(
            self.db,
            self.tracker,
            NewRequestLog {
                api_key_id: self.api_key_id,
                channel_id: self.channel_id,
                model: self.model,
                status,
                prompt_tokens,
                completion_tokens,
                duration_ms: elapsed_ms(self.start),
                request_body: self.request_body,
                response_body,
                error,
            },
        );
    }
}

/// Detached single-row insert. Failures are logged at WARN and dropped: the
/// client flow has already moved on.
fn spawn_insert(db: Db, tracker: LogTracker, record: NewRequestLog) {
    let guard = tracker.guard();
    tokio::spawn(async move {
        let _inflight = guard;
        if let Err(e) = crate::db::request_logs::insert(&db, &record).await {
            tracing::warn!(error = %e, "request log insert failed");
        }
    });
}

/// Waits until every in-flight log insert has finished, or the timeout
/// expires; the latter is warned about, never fatal (the last row may be
/// lost, which is the documented bound for abrupt exits).
pub async fn drain(tracker: &LogTracker, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while tracker.inflight() > 0 {
        if Instant::now() >= deadline {
            tracing::warn!(
                left = tracker.inflight(),
                "log inserts still in flight after drain timeout"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Tees a streaming upstream response into a detached capture task while
/// forwarding every chunk untouched. Chunk copies are refcount bumps and
/// the channel is unbounded, so the forwarding stream keeps its own
/// backpressure and timing. When the stream ends or is dropped, the capture
/// task parses usage from the buffered SSE bytes (only when body logging is
/// on) and finishes the log row.
pub fn tee_stream<S, E>(
    stream: S,
    status: i64,
    ctx: ChatLogContext,
) -> impl Stream<Item = Result<Bytes, E>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let capture = ctx.log_bodies;
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = rx.recv().await {
            if buffer.len() < BODY_CAPTURE_LIMIT {
                let room = BODY_CAPTURE_LIMIT - buffer.len();
                buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
        }
        // The channel closed: the response body finished or was dropped.
        let (usage, response_body) = if capture {
            (parse_sse_usage(&buffer), Some(capture_to_string(&buffer)))
        } else {
            (Usage::default(), None)
        };
        ctx.finish(
            Some(status),
            usage.prompt_tokens,
            usage.completion_tokens,
            response_body,
            None,
        );
    });
    stream.map(move |chunk| {
        if let Ok(bytes) = &chunk {
            // With bodies off this is an empty marker: the capture task
            // learns the stream ended without copying a single byte.
            let _ = tx.send(if capture { bytes.clone() } else { Bytes::new() });
        }
        chunk
    })
}

/// Token counts extracted from an OpenAI `usage` object.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}

/// Extracts usage from a buffered (non-streaming) response body; a missing
/// or malformed `usage` yields NULLs.
pub fn parse_usage(body: &[u8]) -> Usage {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok();
    usage_from_value(value.as_ref().and_then(|v| v.get("usage")))
}

/// Extracts usage from a captured SSE byte buffer by scanning `data:` lines
/// and keeping the last chunk that carries token counts (OpenAI sends a
/// final usage chunk when the client set `stream_options.include_usage`).
pub fn parse_sse_usage(buffer: &[u8]) -> Usage {
    let text = String::from_utf8_lossy(buffer);
    let mut last = Usage::default();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim_start();
        if data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let parsed = usage_from_value(value.get("usage"));
        if parsed != Usage::default() {
            last = parsed;
        }
    }
    last
}

fn usage_from_value(usage: Option<&serde_json::Value>) -> Usage {
    match usage {
        Some(usage) => Usage {
            prompt_tokens: usage
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_i64),
            completion_tokens: usage
                .get("completion_tokens")
                .and_then(serde_json::Value::as_i64),
        },
        None => Usage::default(),
    }
}

/// Copies bytes into a lossy string, stopping at the capture cap.
fn capture_to_string(bytes: &[u8]) -> String {
    let capped = &bytes[..bytes.len().min(BODY_CAPTURE_LIMIT)];
    String::from_utf8_lossy(capped).into_owned()
}

/// Elapsed whole milliseconds, rounded up so any completed request logs at
/// least 1.
fn elapsed_ms(start: Instant) -> i64 {
    let micros = start.elapsed().as_micros();
    let millis = micros.div_ceil(1000);
    millis.min(i64::MAX as u128).max(1) as i64
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::Row;

    use super::*;
    use crate::db::channels::{self, NewChannel};
    use crate::db::keys::{self, NewKey};

    #[test]
    fn parse_usage_extracts_token_counts() {
        let body = br#"{"id":"x","usage":{"prompt_tokens":12,"completion_tokens":34}}"#;
        assert_eq!(
            parse_usage(body),
            Usage {
                prompt_tokens: Some(12),
                completion_tokens: Some(34),
            }
        );
    }

    #[test]
    fn parse_usage_yields_nulls_without_usage_or_with_garbage() {
        assert_eq!(parse_usage(br#"{"id":"x"}"#), Usage::default());
        assert_eq!(parse_usage(b"not json"), Usage::default());
        // A usage object missing one field keeps the field it has.
        assert_eq!(
            parse_usage(br#"{"usage":{"prompt_tokens":5}}"#),
            Usage {
                prompt_tokens: Some(5),
                completion_tokens: None,
            }
        );
    }

    #[test]
    fn parse_sse_usage_keeps_the_last_usage_chunk() {
        let buffer = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":null}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(
            parse_sse_usage(buffer.as_bytes()),
            Usage {
                prompt_tokens: Some(7),
                completion_tokens: Some(9),
            }
        );
    }

    #[test]
    fn parse_sse_usage_yields_nulls_without_a_usage_chunk() {
        let buffer = "data: {\"choices\":[]}\n\ndata: [DONE]\n\n";
        assert_eq!(parse_sse_usage(buffer.as_bytes()), Usage::default());
        assert_eq!(parse_sse_usage(b""), Usage::default());
    }

    #[test]
    fn capture_to_string_truncates_at_the_cap() {
        let oversized = vec![b'a'; BODY_CAPTURE_LIMIT + 100];
        let captured = capture_to_string(&oversized);
        assert_eq!(captured.len(), BODY_CAPTURE_LIMIT);
        let exact = vec![b'b'; BODY_CAPTURE_LIMIT];
        assert_eq!(capture_to_string(&exact).len(), BODY_CAPTURE_LIMIT);
        assert_eq!(capture_to_string(b"short"), "short");
    }

    #[test]
    fn elapsed_ms_rounds_up_to_at_least_one() {
        assert!(elapsed_ms(Instant::now()) >= 1);
    }

    async fn seeded_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        let db = Db::from(pool.clone());
        let channel = channels::create(
            &db,
            &NewChannel {
                name: "test".to_string(),
                base_url: "http://127.0.0.1:9".to_string(),
                api_key: "secret".to_string(),
                allowed_models: None,
            },
        )
        .await
        .unwrap();
        keys::create(
            &db,
            &NewKey {
                key_hash: "hash".to_string(),
                display_prefix: "hyd-".to_string(),
                name: None,
                channel_id: channel.id,
            },
        )
        .await
        .unwrap();
        pool
    }

    async fn wait_until_drained(tracker: &LogTracker) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while tracker.inflight() > 0 {
            assert!(Instant::now() < deadline, "log task never drained");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn spawn_insert_writes_the_row_and_drains_to_zero() {
        let pool = seeded_pool().await;
        let tracker = LogTracker::default();
        spawn_insert(
            Db::from(pool.clone()),
            tracker.clone(),
            NewRequestLog {
                api_key_id: 1,
                channel_id: 1,
                model: Some("gpt-4o".to_string()),
                status: Some(200),
                prompt_tokens: Some(3),
                completion_tokens: Some(5),
                duration_ms: 7,
                request_body: None,
                response_body: None,
                error: None,
            },
        );
        wait_until_drained(&tracker).await;
        assert_eq!(tracker.inflight(), 0);

        let row = sqlx::query("SELECT model, status, prompt_tokens FROM request_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>, _>("model").unwrap(),
            Some("gpt-4o".to_string())
        );
        assert_eq!(row.try_get::<Option<i64>, _>("status").unwrap(), Some(200));
        assert_eq!(
            row.try_get::<Option<i64>, _>("prompt_tokens").unwrap(),
            Some(3)
        );
    }

    #[tokio::test]
    async fn failed_inserts_are_swallowed_not_propagated() {
        let pool = seeded_pool().await;
        let tracker = LogTracker::default();
        // No api_keys row with id 99: the FK rejects the insert, which must
        // surface as a WARN in the task, not as a panic here.
        spawn_insert(
            Db::from(pool.clone()),
            tracker.clone(),
            NewRequestLog {
                api_key_id: 99,
                channel_id: 1,
                model: None,
                status: None,
                prompt_tokens: None,
                completion_tokens: None,
                duration_ms: 1,
                request_body: None,
                response_body: None,
                error: None,
            },
        );
        wait_until_drained(&tracker).await;
        assert_eq!(tracker.inflight(), 0);
    }
}
