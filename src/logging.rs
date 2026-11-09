//! Background request logging for the chat proxy flow.
//!
//! Every `/v1/chat/completions` request gets exactly one `request_logs`
//! row, written by a detached task so the client flow never waits on, or
//! fails from, logging: accepted calls and rejected ones alike. A
//! rejected call stores its failure in `error` and its HTTP status where
//! one was produced: auth-layer rejections (401 unknown key, 403 disabled
//! key) resolve to no key row, so their rows carry NULL `api_key_id` and
//! `channel_id`; handler rejections (unknown model, gone or disabled
//! channel) keep the caller's key id. The one exception is the 413
//! body-limit rejection: it happens inside the body extractor, before any
//! handler code runs, and there is no readable body to reference, so it
//! stays unloggable. `GET /v1/models` is a lightweight discovery endpoint
//! and is not logged.
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
//! semantics. That capture stops at [`BODY_CAPTURE_LIMIT`] (1 MiB); past the
//! cap bytes are simply not captured.
//!
//! Token usage is captured independently of body logging. A streaming
//! response gets a rolling tail window of its last [`USAGE_TAIL_WINDOW`]
//! bytes, so the closing usage chunk is parsed even when the stream runs far
//! past the 1 MiB body cap; a buffered response has its usage read straight
//! from the body. Stream rows therefore carry tokens with body logging off,
//! only the bodies stay NULL.
//!
//! A logged row that names a key is followed, in the same detached flow, by
//! a month-to-date recompute of that key's monthly-limit verdict
//! ([`crate::usage::recompute`]). The recompute only reads: it writes no log
//! row and therefore cannot re-enter this path, and a row without a key
//! (a rejection whose key never resolved) skips it.

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

/// Size of the rolling tail window kept for stream usage parsing (64 KiB).
/// The usage chunk closes an SSE stream, so a window anchored at the end of
/// the stream captures it whatever the stream length; the window is much
/// smaller than [`BODY_CAPTURE_LIMIT`] because only the closing chunks
/// matter, not the content.
pub const USAGE_TAIL_WINDOW: usize = 64 * 1024;

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
    /// NULL for rows about requests whose key never resolved (unknown key
    /// 401, disabled key 403, lookup failure).
    pub api_key_id: Option<i64>,
    /// NULL exactly when `api_key_id` is NULL.
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub start: Instant,
    pub request_body: &'a [u8],
    /// Resolved client IP, absent when no peer address was available.
    pub client_ip: Option<String>,
    /// The monthly-limit verdict store, so the detached insert can hand the
    /// finished row to the recompute that follows it.
    pub flags: &'a crate::limit_flags::LimitFlags,
}

/// Everything the detached log task needs about one chat request. Built in
/// the handler, finished exactly once by whichever path resolves the request.
pub struct ChatLogContext {
    db: Db,
    tracker: LogTracker,
    log_bodies: bool,
    api_key_id: Option<i64>,
    channel_id: Option<i64>,
    model: Option<String>,
    start: Instant,
    request_body: Option<String>,
    client_ip: Option<String>,
    flags: crate::limit_flags::LimitFlags,
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
            client_ip: init.client_ip,
            flags: init.flags.clone(),
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
        usage: Usage,
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
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                cached_output_tokens: usage.cached_output_tokens,
                duration_ms: elapsed_ms(self.start),
                request_body: self.request_body,
                response_body,
                error,
                client_ip: self.client_ip,
            },
            self.flags,
        );
    }
}

/// Detached single-row insert, followed by the monthly-limit recompute of
/// the row's key. Failures are logged at WARN and dropped: the client flow
/// has already moved on.
fn spawn_insert(
    db: Db,
    tracker: LogTracker,
    record: NewRequestLog,
    flags: crate::limit_flags::LimitFlags,
) {
    let guard = tracker.guard();
    tokio::spawn(async move {
        let _inflight = guard;
        match crate::db::request_logs::insert(&db, &record).await {
            // Only a row that landed is worth aggregating; a row without a
            // key (a rejection whose key never resolved) belongs to nobody.
            Ok(()) => {
                if let Some(key_id) = record.api_key_id {
                    spawn_limit_recompute(db, flags, key_id);
                }
            }
            Err(e) => tracing::warn!(error = %e, "request log insert failed"),
        }
    });
}

/// Detached month-to-date aggregate for one key, spawned after the row that
/// triggered it was inserted, so the fresh row is part of the sum. The
/// recompute writes no log row, so it cannot re-enter this path. A failure
/// leaves the stored verdict untouched (see [`crate::usage`]) and is
/// reported through tracing, there being no client left to tell.
fn spawn_limit_recompute(db: Db, flags: crate::limit_flags::LimitFlags, key_id: i64) {
    tokio::spawn(async move {
        if let Err(e) = crate::usage::recompute(&db, &flags, key_id).await {
            tracing::warn!(error = %e, key_id, "monthly limit recompute failed");
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
/// task parses usage from the tail of the SSE bytes, stores the captured
/// body when body logging is on, and finishes the log row.
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
        let mut tail: Vec<u8> = Vec::new();
        while let Some(chunk) = rx.recv().await {
            if capture && buffer.len() < BODY_CAPTURE_LIMIT {
                let room = BODY_CAPTURE_LIMIT - buffer.len();
                buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            push_tail(&mut tail, &chunk);
        }
        // The channel closed: the response body finished or was dropped.
        let usage = parse_sse_usage(&tail);
        let response_body = capture.then(|| capture_to_string(&buffer));
        ctx.finish(Some(status), usage, response_body, None);
    });
    stream.map(move |chunk| {
        if let Ok(bytes) = &chunk {
            // Every chunk reaches the capture task whatever body logging
            // says: usage parsing needs the end of the stream, and a Bytes
            // clone is a refcount bump, not a copy.
            let _ = tx.send(bytes.clone());
        }
        chunk
    })
}

/// Keeps `window` holding at most the last [`USAGE_TAIL_WINDOW`] bytes seen:
/// the usage chunk closes the stream, so the cap must drop bytes off the
/// front, never off the back.
fn push_tail(window: &mut Vec<u8>, chunk: &[u8]) {
    if chunk.len() >= USAGE_TAIL_WINDOW {
        window.clear();
        window.extend_from_slice(&chunk[chunk.len() - USAGE_TAIL_WINDOW..]);
        return;
    }
    window.extend_from_slice(chunk);
    if window.len() > USAGE_TAIL_WINDOW {
        let excess = window.len() - USAGE_TAIL_WINDOW;
        window.drain(..excess);
    }
}

/// Token counts extracted from an OpenAI `usage` object, cached ones
/// included.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// Prompt tokens the upstream served from its cache (OpenAI
    /// `prompt_tokens_details.cached_tokens`, DeepSeek
    /// `prompt_cache_hit_tokens`).
    pub cached_input_tokens: Option<i64>,
    /// Completion tokens the upstream served from its cache (OpenAI
    /// `completion_tokens_details.cached_tokens`). A vendor that reports no
    /// cached output leaves this NULL.
    pub cached_output_tokens: Option<i64>,
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
/// The caller passes the tail window of the stream, which holds that chunk.
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

/// Reads one `usage` object field by field; every field is independent, and
/// an absent one stays NULL instead of becoming a fabricated zero.
fn usage_from_value(usage: Option<&serde_json::Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(serde_json::Value::as_i64),
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(serde_json::Value::as_i64),
        cached_input_tokens: cached_input_tokens(usage),
        cached_output_tokens: details_cached_tokens(usage, "completion_tokens_details"),
    }
}

/// Cached prompt tokens: OpenAI's `prompt_tokens_details.cached_tokens`
/// wins when both shapes are present, which is what DeepSeek's
/// OpenAI-compatible API emits (it repeats the same number in its own
/// `prompt_cache_hit_tokens` field).
fn cached_input_tokens(usage: &serde_json::Value) -> Option<i64> {
    details_cached_tokens(usage, "prompt_tokens_details").or_else(|| {
        usage
            .get("prompt_cache_hit_tokens")
            .and_then(serde_json::Value::as_i64)
    })
}

/// The `cached_tokens` value inside a `<name>_tokens_details` object.
fn details_cached_tokens(usage: &serde_json::Value, details_key: &str) -> Option<i64> {
    usage
        .get(details_key)
        .and_then(|details| details.get("cached_tokens"))
        .and_then(serde_json::Value::as_i64)
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
                ..Usage::default()
            }
        );
    }

    #[test]
    fn parse_usage_extracts_openai_cached_token_details() {
        let body = br#"{"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":64},"completion_tokens_details":{"cached_tokens":8}}}"#;
        assert_eq!(
            parse_usage(body),
            Usage {
                prompt_tokens: Some(100),
                completion_tokens: Some(20),
                cached_input_tokens: Some(64),
                cached_output_tokens: Some(8),
            }
        );
    }

    #[test]
    fn parse_usage_extracts_deepseek_prompt_cache_hit_tokens() {
        let body = br#"{"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_cache_hit_tokens":64,"prompt_cache_miss_tokens":36}}"#;
        assert_eq!(
            parse_usage(body),
            Usage {
                prompt_tokens: Some(100),
                completion_tokens: Some(20),
                cached_input_tokens: Some(64),
                cached_output_tokens: None,
            }
        );
    }

    #[test]
    fn parse_usage_prefers_prompt_tokens_details_over_the_deepseek_field() {
        // DeepSeek's OpenAI-compatible shape carries the same number in
        // both places; the OpenAI location is the one that wins.
        let body = br#"{"usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":7},"prompt_cache_hit_tokens":7}}"#;
        assert_eq!(parse_usage(body).cached_input_tokens, Some(7));
    }

    #[test]
    fn parse_usage_leaves_cached_tokens_null_when_absent() {
        // Legacy completions shape: no details objects at all.
        let legacy = br#"{"usage":{"prompt_tokens":9,"completion_tokens":1,"total_tokens":10}}"#;
        let usage = parse_usage(legacy);
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(usage.cached_output_tokens, None);
        // Details objects that carry other keys only: still nothing to
        // read, and never a fabricated zero.
        let other_details = br#"{"usage":{"prompt_tokens":9,"completion_tokens":1,"prompt_tokens_details":{"audio_tokens":3},"completion_tokens_details":{"reasoning_tokens":5}}}"#;
        let usage = parse_usage(other_details);
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(usage.cached_output_tokens, None);
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
                ..Usage::default()
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
                ..Usage::default()
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
    fn push_tail_keeps_the_last_bytes_of_a_long_stream() {
        let mut window: Vec<u8> = Vec::new();
        // A first chunk larger than the window keeps only its own tail.
        push_tail(&mut window, &vec![b'a'; USAGE_TAIL_WINDOW + 10]);
        assert_eq!(window.len(), USAGE_TAIL_WINDOW);
        assert!(window.iter().all(|byte| *byte == b'a'));
        // Later small chunks keep the window at the cap and push the front
        // out, so the newest bytes survive.
        push_tail(&mut window, b"tail-marker");
        assert_eq!(window.len(), USAGE_TAIL_WINDOW);
        assert!(window.ends_with(b"tail-marker"));
    }

    #[test]
    fn push_tail_of_a_short_stream_is_the_whole_stream() {
        let mut window: Vec<u8> = Vec::new();
        push_tail(&mut window, b"data: [DONE]\n\n");
        push_tail(&mut window, b"");
        assert_eq!(window, b"data: [DONE]\n\n");
    }

    #[test]
    fn parse_sse_usage_reads_a_usage_chunk_that_follows_the_body_cap() {
        // The tail window is what makes this work in production: the
        // window, not the body buffer, carries the closing usage chunk,
        // and it starts mid-event because the stream ran far past the cap.
        let filler = b"data: {\"choices\":[{\"delta\":{\"content\":\"chunk\"}}]}\n\n";
        let mut window: Vec<u8> = Vec::new();
        push_tail(
            &mut window,
            &filler.repeat(BODY_CAPTURE_LIMIT * 2 / filler.len()),
        );
        push_tail(
            &mut window,
            br#"data: {"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":22,"prompt_tokens_details":{"cached_tokens":4}}}"#,
        );
        push_tail(&mut window, b"\n\ndata: [DONE]\n\n");
        assert_eq!(
            parse_sse_usage(&window),
            Usage {
                prompt_tokens: Some(11),
                completion_tokens: Some(22),
                cached_input_tokens: Some(4),
                cached_output_tokens: None,
            }
        );
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
                monthly_limit_micro_usd: None,
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
                api_key_id: Some(1),
                channel_id: Some(1),
                model: Some("gpt-4o".to_string()),
                status: Some(200),
                prompt_tokens: Some(3),
                completion_tokens: Some(5),
                cached_input_tokens: Some(2),
                cached_output_tokens: None,
                duration_ms: 7,
                request_body: None,
                response_body: None,
                error: None,
                client_ip: Some("198.51.100.5".to_string()),
            },
            crate::limit_flags::LimitFlags::default(),
        );
        wait_until_drained(&tracker).await;
        assert_eq!(tracker.inflight(), 0);

        let row = sqlx::query(
            "SELECT model, status, prompt_tokens, cached_input_tokens, \
             cached_output_tokens, client_ip FROM request_logs",
        )
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
        assert_eq!(
            row.try_get::<Option<i64>, _>("cached_input_tokens")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("cached_output_tokens")
                .unwrap(),
            None,
            "an upstream that reports no cached output leaves the column null"
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("client_ip").unwrap(),
            Some("198.51.100.5".to_string())
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
                api_key_id: Some(99),
                channel_id: Some(1),
                model: None,
                status: None,
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
            crate::limit_flags::LimitFlags::default(),
        );
        wait_until_drained(&tracker).await;
        assert_eq!(tracker.inflight(), 0);
    }
}
