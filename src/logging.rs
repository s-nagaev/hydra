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
//! Token usage is captured independently of body logging, read in the shape
//! the request's own surface speaks ([`UsageFormat`]). Chat and Responses
//! report usage in the event that closes the stream, so a streaming response
//! gets a rolling tail window of its last [`USAGE_TAIL_WINDOW`] bytes and
//! that closing event is parsed from it even when the stream runs far past
//! the 1 MiB body cap. Anthropic reports its input counters in the event
//! that OPENS the stream, which no window anchored at the end can hold, so
//! it is read event by event as the bytes arrive ([`AnthropicStreamUsage`]).
//! A buffered response has its usage read straight from the body. Stream
//! rows therefore carry tokens with body logging off, only the bodies stay
//! NULL.
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

/// Longest single SSE line the Anthropic stream reader buffers. Usage lines
/// run to a few hundred bytes; a line past the cap is dropped up to its
/// newline, so a malformed or hostile stream cannot grow the buffer without
/// bound.
const USAGE_EVENT_LINE_LIMIT: usize = 64 * 1024;

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
    /// Which surface the request came through, so any response body is read
    /// in the shape its upstream speaks. The chat handler passes
    /// [`UsageFormat::Chat`]; the rejection paths, whose rows never carry
    /// usage because no upstream answered, pass it as an inert placeholder.
    pub usage_format: UsageFormat,
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
    usage_format: UsageFormat,
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
            usage_format: init.usage_format,
            start: init.start,
            request_body: init
                .log_bodies
                .then(|| capture_to_string(init.request_body)),
            client_ip: init.client_ip,
            flags: init.flags.clone(),
        }
    }

    /// The usage shape this request's response carries.
    pub fn usage_format(&self) -> UsageFormat {
        self.usage_format
    }

    /// Captured response body for a fully buffered (non-streaming) response,
    /// NULL unless body logging is on.
    pub fn capture_response_body(&self, bytes: &[u8]) -> Option<String> {
        self.log_bodies.then(|| capture_to_string(bytes))
    }

    /// Token counts of a fully buffered (non-streaming) response body, read
    /// in the shape this request's surface speaks.
    pub fn buffered_usage(&self, bytes: &[u8]) -> UsageExtraction {
        parse_usage_for(bytes, self.usage_format)
    }

    /// Assembles the final row and dispatches the detached insert. Consumes
    /// the context so every request finishes its log exactly once.
    pub fn finish(
        self,
        status: Option<i64>,
        usage: UsageExtraction,
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
                cache_write_input_tokens: usage.cache_write_input_tokens,
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
/// task extracts usage in the context's own format (a tail window for chat
/// and Responses, event by event for Anthropic), stores the captured body
/// when body logging is on, and finishes the log row.
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
    let format = ctx.usage_format();
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let mut usage = StreamUsage::new(format);
        while let Some(chunk) = rx.recv().await {
            if capture && buffer.len() < BODY_CAPTURE_LIMIT {
                let room = BODY_CAPTURE_LIMIT - buffer.len();
                buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            usage.observe(&chunk);
        }
        // The channel closed: the response body finished or was dropped.
        let usage = usage.finish();
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

/// Which API surface a request came through, and therefore which usage shape
/// its upstream answers with. A route hands it to [`ChatLogInit`] when it
/// builds the log context; every extraction below reads it from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageFormat {
    /// OpenAI Chat Completions (`/v1/chat/completions`).
    Chat,
    /// OpenAI Responses (`/v1/responses`).
    Responses,
    /// Anthropic Messages (`/v1/messages`).
    Anthropic,
}

/// Token counts extracted from one upstream response, in Hydra's own
/// vocabulary: the five dimensions the `request_logs` row stores, one per
/// price dimension. A dimension the upstream did not report stays NULL,
/// never a fabricated zero.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UsageExtraction {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cached_output_tokens: Option<i64>,
    /// Prompt tokens the upstream wrote into its cache (Anthropic
    /// `cache_creation_input_tokens`), already counted inside
    /// `prompt_tokens` and priced at its own per-model write rate.
    pub cache_write_input_tokens: Option<i64>,
}

impl From<Usage> for UsageExtraction {
    fn from(usage: Usage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cached_output_tokens: usage.cached_output_tokens,
            cache_write_input_tokens: None,
        }
    }
}

/// Extracts usage from a buffered (non-streaming) response body in the shape
/// the given surface speaks. A missing or malformed `usage` yields NULLs for
/// every format, exactly as [`parse_usage`] does for chat.
pub fn parse_usage_for(body: &[u8], format: UsageFormat) -> UsageExtraction {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok();
    let usage = value.as_ref().and_then(|value| value.get("usage"));
    match format {
        UsageFormat::Chat => usage_from_value(usage).into(),
        UsageFormat::Responses => responses_usage(usage),
        UsageFormat::Anthropic => anthropic_usage(usage),
    }
}

/// Usage of an OpenAI Responses object: `input_tokens` already includes the
/// cached ones, `output_tokens_details.reasoning_tokens` is a subset of
/// `output_tokens` with no column of its own, and a Responses object has no
/// cached-output concept at all, so those dimensions stay NULL.
fn responses_usage(usage: Option<&serde_json::Value>) -> UsageExtraction {
    let Some(usage) = usage else {
        return UsageExtraction::default();
    };
    UsageExtraction {
        prompt_tokens: usage_int(usage, "input_tokens"),
        completion_tokens: usage_int(usage, "output_tokens"),
        cached_input_tokens: details_cached_tokens(usage, "input_tokens_details"),
        cached_output_tokens: None,
        cache_write_input_tokens: None,
    }
}

/// Usage of an Anthropic message object.
fn anthropic_usage(usage: Option<&serde_json::Value>) -> UsageExtraction {
    let Some(usage) = usage else {
        return UsageExtraction::default();
    };
    anthropic_usage_parts(
        usage_int(usage, "input_tokens"),
        usage_int(usage, "cache_read_input_tokens"),
        usage_int(usage, "cache_creation_input_tokens"),
        usage_int(usage, "output_tokens"),
    )
}

/// Assembles one Anthropic reading from counters already read out of the
/// payload, shared by the buffered and the streaming paths so both normalize
/// the input side identically.
fn anthropic_usage_parts(
    input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) -> UsageExtraction {
    UsageExtraction {
        prompt_tokens: anthropic_prompt_tokens(
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        ),
        completion_tokens: output_tokens,
        cached_input_tokens: cache_read_input_tokens,
        cached_output_tokens: None,
        cache_write_input_tokens: cache_creation_input_tokens,
    }
}

/// Anthropic reports a prompt additively: `input_tokens` holds only the
/// uncached part, `cache_read_input_tokens` the cache hits and
/// `cache_creation_input_tokens` the cache writes, so the stored total is the
/// sum of whichever of the three the upstream reported. A report with none of
/// them leaves the total NULL rather than zero, and cache writes are never
/// dropped from the sum: they are prompt tokens the upstream really read.
pub fn anthropic_prompt_tokens(
    input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
) -> Option<i64> {
    [
        input_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
    ]
    .into_iter()
    .flatten()
    .reduce(|sum, part| sum + part)
}

/// Extracts usage from a captured tail of an OpenAI Responses SSE stream.
/// Usage rides only the terminal events, `response.completed` when the
/// generation finished and `response.incomplete` when a limit cut it short,
/// one level down at `data.response.usage`; both are read, and the last one
/// seen wins. The caller passes the tail window of the stream, which holds
/// the terminal event because nothing follows it.
pub fn parse_responses_sse_usage(buffer: &[u8]) -> UsageExtraction {
    let text = String::from_utf8_lossy(buffer);
    let mut last = UsageExtraction::default();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim_start()) else {
            continue;
        };
        if !matches!(
            value.get("type").and_then(serde_json::Value::as_str),
            Some("response.completed" | "response.incomplete")
        ) {
            continue;
        }
        let parsed = responses_usage(
            value
                .get("response")
                .and_then(|response| response.get("usage")),
        );
        if parsed != UsageExtraction::default() {
            last = parsed;
        }
    }
    last
}

/// Usage evidence gathered from a streaming response while its chunks fly
/// past. Chat and Responses are read once at the end from a rolling tail of
/// raw bytes, which suffices because both report usage in the event that
/// closes the stream; Anthropic is read as the bytes arrive, because its
/// input counters ride the stream's first event and no window can hold both
/// ends of a long stream.
enum StreamUsage {
    Chat { window: Vec<u8> },
    Responses { window: Vec<u8> },
    Anthropic(AnthropicStreamUsage),
}

impl StreamUsage {
    fn new(format: UsageFormat) -> Self {
        match format {
            UsageFormat::Chat => StreamUsage::Chat { window: Vec::new() },
            UsageFormat::Responses => StreamUsage::Responses { window: Vec::new() },
            UsageFormat::Anthropic => StreamUsage::Anthropic(AnthropicStreamUsage::default()),
        }
    }

    fn observe(&mut self, chunk: &[u8]) {
        match self {
            StreamUsage::Chat { window } | StreamUsage::Responses { window } => {
                push_tail(window, chunk);
            }
            StreamUsage::Anthropic(reader) => reader.observe(chunk),
        }
    }

    fn finish(self) -> UsageExtraction {
        match self {
            StreamUsage::Chat { window } => parse_sse_usage(&window).into(),
            StreamUsage::Responses { window } => parse_responses_sse_usage(&window),
            StreamUsage::Anthropic(reader) => reader.finish(),
        }
    }
}

/// Incremental event reader for an Anthropic Messages SSE stream.
///
/// The tail window the chat path uses cannot serve this stream: the input
/// counters ride `message_start`, which opens the stream, while the
/// authoritative `output_tokens` closes it in `message_delta`, so on a stream
/// longer than the window the window would evict the input counters and the
/// request would be logged with a NULL prompt, silently unbilled. The reader
/// therefore walks the bytes as they arrive and keeps only the incomplete
/// trailing line plus the numbers it has already seen, whatever the stream
/// length. A stream whose `message_start` never arrives (malformed, or cut
/// before its first event) yields NULL input counters instead of guesses; the
/// repetitions some vendors put on the closing event are not an opening
/// report, and inventing counts from them would bill a guess.
#[derive(Default)]
struct AnthropicStreamUsage {
    /// The line being received: SSE frames are line delimited and a chunk
    /// boundary can fall anywhere, so a partial line waits here.
    partial: Vec<u8>,
    /// Set while an over-long line is being dropped, up to its newline.
    skipping: bool,
    input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

impl AnthropicStreamUsage {
    fn observe(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while !rest.is_empty() {
            let Some(newline) = rest.iter().position(|byte| *byte == b'\n') else {
                self.push_partial(rest);
                return;
            };
            if !self.skipping {
                self.partial.extend_from_slice(&rest[..newline]);
                let line = std::mem::take(&mut self.partial);
                self.event(&line);
            }
            self.skipping = false;
            rest = &rest[newline + 1..];
        }
    }

    /// Appends an unterminated line, dropping the whole line once it passes
    /// the cap: a stream that never sends a newline must not grow the buffer,
    /// and no usage object is that large.
    fn push_partial(&mut self, bytes: &[u8]) {
        if self.skipping {
            return;
        }
        if self.partial.len() + bytes.len() > USAGE_EVENT_LINE_LIMIT {
            self.partial.clear();
            self.skipping = true;
            return;
        }
        self.partial.extend_from_slice(bytes);
    }

    /// Reads one complete SSE line. Input counters are taken only from
    /// `message_start`, and only the first time they appear; `output_tokens`
    /// is taken from every usage-bearing event, so the provisional count of
    /// `message_start` gives way to the `message_delta` that closes the
    /// stream.
    fn event(&mut self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        let Some(data) = text.trim_end_matches('\r').strip_prefix("data:") else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim_start()) else {
            return;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                let usage = value
                    .get("message")
                    .and_then(|message| message.get("usage"));
                self.first_inputs(usage);
                self.latest_output(usage);
            }
            Some("message_delta") => self.latest_output(value.get("usage")),
            _ => {}
        }
    }

    /// Keeps the counters the stream opened with; a repeated `message_start`
    /// never overwrites a value already read.
    fn first_inputs(&mut self, usage: Option<&serde_json::Value>) {
        let Some(usage) = usage else {
            return;
        };
        if self.input_tokens.is_none() {
            self.input_tokens = usage_int(usage, "input_tokens");
        }
        if self.cache_read_input_tokens.is_none() {
            self.cache_read_input_tokens = usage_int(usage, "cache_read_input_tokens");
        }
        if self.cache_creation_input_tokens.is_none() {
            self.cache_creation_input_tokens = usage_int(usage, "cache_creation_input_tokens");
        }
    }

    fn latest_output(&mut self, usage: Option<&serde_json::Value>) {
        let Some(usage) = usage else {
            return;
        };
        if let Some(output) = usage_int(usage, "output_tokens") {
            self.output_tokens = Some(output);
        }
    }

    fn finish(self) -> UsageExtraction {
        anthropic_usage_parts(
            self.input_tokens,
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
            self.output_tokens,
        )
    }
}

/// One integer counter of a usage object; an absent or non-integer field
/// stays NULL.
fn usage_int(usage: &serde_json::Value, key: &str) -> Option<i64> {
    usage.get(key).and_then(serde_json::Value::as_i64)
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

    /// Waits for `expected` rows to land. A stream's capture task, its insert
    /// and the recompute that follows are all detached, so a drained tracker
    /// is not yet a written row.
    async fn wait_for_rows(pool: &SqlitePool, expected: i64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
                .fetch_one(pool)
                .await
                .unwrap();
            if count >= expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected} log rows, found {count}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
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
                cache_write_input_tokens: Some(1),
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
             cached_output_tokens, cache_write_input_tokens, client_ip FROM request_logs",
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
            row.try_get::<Option<i64>, _>("cache_write_input_tokens")
                .unwrap(),
            Some(1),
            "the write count travels from the row builder to the column"
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
                cache_write_input_tokens: None,
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

    #[test]
    fn anthropic_prompt_tokens_adds_the_three_input_counters() {
        assert_eq!(
            anthropic_prompt_tokens(Some(100), Some(20), Some(5)),
            Some(125)
        );
    }

    #[test]
    fn anthropic_prompt_tokens_without_cache_is_the_plain_input_count() {
        assert_eq!(anthropic_prompt_tokens(Some(100), None, None), Some(100));
        // A reported zero is still a reported value, and adding it must not
        // turn the total into a NULL.
        assert_eq!(
            anthropic_prompt_tokens(Some(100), Some(0), Some(0)),
            Some(100)
        );
        assert_eq!(anthropic_prompt_tokens(Some(100), Some(0), None), Some(100));
    }

    #[test]
    fn anthropic_prompt_tokens_sums_whatever_the_upstream_reported() {
        // Partial reports still total up: an upstream that skips
        // `input_tokens` but reports cache hits has real prompt tokens.
        assert_eq!(anthropic_prompt_tokens(None, Some(7), None), Some(7));
        assert_eq!(anthropic_prompt_tokens(None, None, Some(3)), Some(3));
    }

    #[test]
    fn anthropic_prompt_tokens_without_any_component_is_null() {
        assert_eq!(anthropic_prompt_tokens(None, None, None), None);
    }

    #[test]
    fn parse_usage_for_chat_matches_the_chat_parser() {
        let body = br#"{"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":64},"completion_tokens_details":{"cached_tokens":8}}}"#;
        assert_eq!(
            parse_usage_for(body, UsageFormat::Chat),
            parse_usage(body).into()
        );
    }

    #[test]
    fn parse_usage_for_responses_reads_the_responses_shape() {
        // `reasoning_tokens` is a subset of `output_tokens` and has no
        // column, so it must not be added anywhere.
        let body = br#"{"usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2}}}"#;
        assert_eq!(
            parse_usage_for(body, UsageFormat::Responses),
            UsageExtraction {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                cached_input_tokens: Some(4),
                cached_output_tokens: None,
                cache_write_input_tokens: None,
            }
        );
    }

    #[test]
    fn parse_usage_for_responses_without_details_leaves_cached_input_null() {
        let body = br#"{"usage":{"input_tokens":10,"output_tokens":5}}"#;
        assert_eq!(
            parse_usage_for(body, UsageFormat::Responses),
            UsageExtraction {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                ..UsageExtraction::default()
            }
        );
    }

    #[test]
    fn parse_usage_for_anthropic_normalizes_the_prompt_additively() {
        let body = br#"{"usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}"#;
        assert_eq!(
            parse_usage_for(body, UsageFormat::Anthropic),
            UsageExtraction {
                prompt_tokens: Some(16),
                completion_tokens: Some(7),
                cached_input_tokens: Some(4),
                cached_output_tokens: None,
                cache_write_input_tokens: Some(2),
            }
        );
    }

    #[test]
    fn parse_usage_for_anthropic_without_cache_reports_the_plain_prompt() {
        let body = br#"{"usage":{"input_tokens":10,"output_tokens":7}}"#;
        let usage = parse_usage_for(body, UsageFormat::Anthropic);
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(usage.cache_write_input_tokens, None);
    }

    #[test]
    fn parse_usage_for_yields_nulls_for_every_format_without_usage() {
        for format in [
            UsageFormat::Chat,
            UsageFormat::Responses,
            UsageFormat::Anthropic,
        ] {
            assert_eq!(
                parse_usage_for(br#"{"id":"x"}"#, format),
                UsageExtraction::default()
            );
            // A usage object that reports only an output count must not have
            // a prompt total invented for it.
            let usage = parse_usage_for(br#"{"usage":{"output_tokens":7}}"#, format);
            assert_eq!(usage.prompt_tokens, None, "{format:?} invented a prompt");
            assert_eq!(usage.cached_input_tokens, None);
            assert_eq!(usage.cache_write_input_tokens, None);
        }
    }

    #[test]
    fn responses_stream_usage_is_read_from_the_terminal_event() {
        let chunks = [
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":4},\"output_tokens_details\":{\"reasoning_tokens\":2}}}}\n\n",
        ];
        assert_eq!(
            observe_stream_chunks(UsageFormat::Responses, &chunks),
            UsageExtraction {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                cached_input_tokens: Some(4),
                cached_output_tokens: None,
                cache_write_input_tokens: None,
            }
        );
    }

    #[test]
    fn responses_stream_usage_is_read_from_a_terminal_event_far_into_the_stream() {
        // The terminal event carries the only usage of the stream and sits
        // after more bytes than the tail window holds, so the window must
        // keep the END of the stream, not the beginning.
        let filler = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"chunk\"}\n\n";
        let long = filler.repeat(USAGE_TAIL_WINDOW / filler.len() + 2);
        let terminal = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":200}}}\n\n";
        let usage = observe_stream_chunks(UsageFormat::Responses, &[&long, terminal]);
        assert_eq!(usage.prompt_tokens, Some(1000));
        assert_eq!(usage.completion_tokens, Some(200));
    }

    #[test]
    fn responses_stream_usage_is_read_from_response_incomplete() {
        // A generation cut short by a limit reports its usage on the
        // `response.incomplete` terminal event instead.
        let chunks = [
            "event: response.in_progress\ndata: {\"type\":\"response.in_progress\"}\n\n",
            "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":4}}}\n\n",
        ];
        let usage = observe_stream_chunks(UsageFormat::Responses, &chunks);
        assert_eq!(usage.prompt_tokens, Some(9));
        assert_eq!(usage.completion_tokens, Some(4));
    }

    #[test]
    fn anthropic_stream_usage_survives_a_stream_longer_than_the_tail_window() {
        // The opening event carries the input counters and the closing one
        // the authoritative output count, with more bytes between them than
        // any window holds. The window is proven to lose the opening event,
        // which is exactly why the reader walks the stream instead.
        let opening = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1000,\"cache_read_input_tokens\":250,\"cache_creation_input_tokens\":50,\"output_tokens\":1}}}\n\n";
        let filler = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"chunk\"}}\n\n";
        let long = filler.repeat(USAGE_TAIL_WINDOW / filler.len() + 2);
        let closing = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n";

        let mut window: Vec<u8> = Vec::new();
        for chunk in [opening, long.as_str(), closing] {
            push_tail(&mut window, chunk.as_bytes());
        }
        assert!(
            !String::from_utf8_lossy(&window).contains("message_start"),
            "premise: the tail window evicts the opening event"
        );

        let usage = observe_stream_chunks(UsageFormat::Anthropic, &[opening, &long, closing]);
        assert_eq!(
            usage,
            UsageExtraction {
                prompt_tokens: Some(1300),
                completion_tokens: Some(42),
                cached_input_tokens: Some(250),
                cached_output_tokens: None,
                cache_write_input_tokens: Some(50),
            }
        );
    }

    #[test]
    fn anthropic_stream_usage_keeps_the_opening_inputs_and_the_latest_output() {
        // A chunk boundary can fall inside a line; the reader must rejoin
        // it. The repeated input fields of `message_delta` must not
        // overwrite the opening report, and its `output_tokens` must.
        let chunks = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":30,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":999,\"output_tokens\":40}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_del",
            "ta\",\"usage\":{\"output_tokens\":77}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
        let usage = observe_stream_chunks(UsageFormat::Anthropic, &chunks);
        assert_eq!(usage.prompt_tokens, Some(45), "the opening report wins");
        assert_eq!(usage.cached_input_tokens, Some(10));
        assert_eq!(usage.cache_write_input_tokens, Some(5));
        assert_eq!(usage.completion_tokens, Some(77), "the latest count wins");
    }

    #[test]
    fn anthropic_stream_usage_without_message_start_yields_null_inputs() {
        // Malformed stream: the opening event never arrives, so the input
        // counters stay NULL rather than being guessed from the repetitions
        // some vendors put on the closing event. The output count, which is
        // authoritative wherever it appears, is still read.
        let chunks = [
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":6}}\n\n",
        ];
        let usage = observe_stream_chunks(UsageFormat::Anthropic, &chunks);
        assert_eq!(usage.prompt_tokens, None);
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(usage.cache_write_input_tokens, None);
        assert_eq!(usage.completion_tokens, Some(6));
    }

    #[test]
    fn anthropic_stream_usage_drops_an_over_long_line_and_keeps_reading() {
        // A line past the cap is dropped up to its newline instead of
        // growing the buffer; the events around it are still read.
        let mut oversized =
            String::from("data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"");
        oversized.push_str(&"a".repeat(USAGE_EVENT_LINE_LIMIT));
        oversized.push_str("\"}}\n\n");
        let chunks = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":8,\"output_tokens\":1}}}\n\n",
            oversized.as_str(),
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":21}}\n\n",
        ];
        let usage = observe_stream_chunks(UsageFormat::Anthropic, &chunks);
        assert_eq!(usage.prompt_tokens, Some(8));
        assert_eq!(usage.completion_tokens, Some(21));
    }

    #[test]
    fn stream_usage_without_usage_yields_nulls_for_every_format() {
        let chunks = [
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
            "data: [DONE]\n\n",
        ];
        for format in [
            UsageFormat::Chat,
            UsageFormat::Responses,
            UsageFormat::Anthropic,
        ] {
            assert_eq!(
                observe_stream_chunks(format, &chunks),
                UsageExtraction::default(),
                "{format:?} must log NULLs when the upstream reported nothing"
            );
        }
    }

    #[tokio::test]
    async fn a_buffered_anthropic_response_persists_the_cache_write_count() {
        let pool = seeded_pool().await;
        let tracker = LogTracker::default();
        let db = Db::from(pool.clone());
        let flags = crate::limit_flags::LimitFlags::default();
        let ctx = ChatLogContext::new(ChatLogInit {
            db: &db,
            tracker: &tracker,
            log_bodies: false,
            api_key_id: Some(1),
            channel_id: Some(1),
            model: Some("claude".to_string()),
            usage_format: UsageFormat::Anthropic,
            start: Instant::now(),
            request_body: b"{}",
            client_ip: None,
            flags: &flags,
        });
        // The same counters the streaming test reads from `message_start`
        // and `message_delta`, here in one buffered body.
        let body = br#"{"usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}"#;
        let usage = ctx.buffered_usage(body);
        assert_eq!(usage.cache_write_input_tokens, Some(2));
        ctx.finish(Some(200), usage, None, None);
        wait_until_drained(&tracker).await;
        wait_for_rows(&pool, 1).await;

        let row = sqlx::query(
            "SELECT prompt_tokens, completion_tokens, cached_input_tokens, \
             cache_write_input_tokens FROM request_logs",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.try_get::<Option<i64>, _>("cache_write_input_tokens")
                .unwrap(),
            Some(2),
            "the buffered Anthropic path persists the cache-write count"
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("prompt_tokens").unwrap(),
            Some(16)
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("cached_input_tokens")
                .unwrap(),
            Some(4)
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("completion_tokens").unwrap(),
            Some(7)
        );
    }

    /// Feeds raw SSE chunks through the same streaming capture the tee uses
    /// and returns the reading it would have logged.
    fn observe_stream_chunks(format: UsageFormat, chunks: &[&str]) -> UsageExtraction {
        let mut capture = StreamUsage::new(format);
        for chunk in chunks {
            capture.observe(chunk.as_bytes());
        }
        capture.finish()
    }

    /// Drives one SSE body through the real tee with the given format, so the
    /// resulting row lands in the seeded pool.
    async fn tee_stream_once(
        pool: &SqlitePool,
        tracker: &LogTracker,
        format: UsageFormat,
        body: &str,
    ) {
        let db = Db::from(pool.clone());
        let flags = crate::limit_flags::LimitFlags::default();
        let ctx = ChatLogContext::new(ChatLogInit {
            db: &db,
            tracker,
            log_bodies: false,
            api_key_id: Some(1),
            channel_id: Some(1),
            model: Some("mock-model".to_string()),
            usage_format: format,
            start: Instant::now(),
            request_body: b"{}",
            client_ip: None,
            flags: &flags,
        });
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![Ok(Bytes::from(body.to_string()))];
        let stream = tee_stream(futures_util::stream::iter(chunks), 200, ctx);
        let forwarded: Vec<_> = futures_util::StreamExt::collect(stream).await;
        assert_eq!(forwarded.len(), 1, "the tee forwards every chunk");
        wait_until_drained(tracker).await;
    }

    #[tokio::test]
    async fn tee_stream_logs_usage_in_the_context_format() {
        let pool = seeded_pool().await;
        let tracker = LogTracker::default();
        tee_stream_once(
            &pool,
            &tracker,
            UsageFormat::Responses,
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
             event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n",
        )
        .await;
        tee_stream_once(
            &pool,
            &tracker,
            UsageFormat::Anthropic,
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":30,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":5,\"output_tokens\":1}}}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":40}}\n\n",
        )
        .await;
        wait_for_rows(&pool, 2).await;

        let rows = sqlx::query(
            "SELECT prompt_tokens, completion_tokens, cached_input_tokens, \
             cached_output_tokens, cache_write_input_tokens FROM request_logs ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].try_get::<Option<i64>, _>("prompt_tokens").unwrap(),
            Some(10),
            "the Responses row reads input_tokens"
        );
        assert_eq!(
            rows[0]
                .try_get::<Option<i64>, _>("completion_tokens")
                .unwrap(),
            Some(5)
        );
        assert_eq!(
            rows[0]
                .try_get::<Option<i64>, _>("cached_input_tokens")
                .unwrap(),
            Some(4)
        );
        assert_eq!(
            rows[1].try_get::<Option<i64>, _>("prompt_tokens").unwrap(),
            Some(45),
            "the Anthropic row sums input, cache reads and cache writes"
        );
        assert_eq!(
            rows[1]
                .try_get::<Option<i64>, _>("completion_tokens")
                .unwrap(),
            Some(40)
        );
        assert_eq!(
            rows[1]
                .try_get::<Option<i64>, _>("cached_input_tokens")
                .unwrap(),
            Some(10)
        );
        for row in &rows {
            assert_eq!(
                row.try_get::<Option<i64>, _>("cached_output_tokens")
                    .unwrap(),
                None
            );
        }
        assert_eq!(
            rows[1]
                .try_get::<Option<i64>, _>("cache_write_input_tokens")
                .unwrap(),
            Some(5),
            "the Anthropic row persists the cache-write count it extracted"
        );
        assert_eq!(
            rows[0]
                .try_get::<Option<i64>, _>("cache_write_input_tokens")
                .unwrap(),
            None,
            "the Responses surface reports no cache writes, so the column stays null"
        );
    }
}
