//! Runtime sqlx queries for the `request_logs` table.
//!
//! Rows are written only by the background logger in `crate::logging`; the
//! insert is fire-and-forget from the client flow's point of view.
//!
//! One SQL body per query: fragments are written against the backend-aware
//! [`Sql`] builder, which picks the placeholder per backend.

use super::timestamp;
use super::{Arg, Db, Fields, Row, Sql};

/// A new request log row. `status` is the upstream HTTP status, or NULL when
/// the request never reached the upstream; `created_at` is stamped in the
/// canonical UTC text format at insert time.
pub struct NewRequestLog {
    /// The key that made the request. NULL for rows about requests whose
    /// key never resolved: an unknown key 401 or a key lookup failure has
    /// no key row to reference.
    pub api_key_id: Option<i64>,
    /// The channel the request routed through. NULL exactly when
    /// `api_key_id` is NULL: both come from the same key resolution.
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub status: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// Prompt tokens the upstream served from its cache (OpenAI
    /// `prompt_tokens_details.cached_tokens`, DeepSeek
    /// `prompt_cache_hit_tokens`). NULL when the upstream reported no
    /// cached tokens, never a fabricated zero.
    pub cached_input_tokens: Option<i64>,
    /// Completion tokens the upstream served from its cache (OpenAI
    /// `completion_tokens_details.cached_tokens`); NULL when the vendor
    /// reports none.
    pub cached_output_tokens: Option<i64>,
    /// Prompt tokens the upstream wrote into its cache (Anthropic
    /// `cache_creation_input_tokens`), already counted inside
    /// `prompt_tokens`. NULL when the upstream reported no cache writes: the
    /// Chat and Responses surfaces never produce them, which is not the same
    /// as a reported zero.
    pub cache_write_input_tokens: Option<i64>,
    pub duration_ms: i64,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub error: Option<String>,
    pub client_ip: Option<String>,
}

/// Inserts one log row. The caller runs this on a detached task; failures
/// are reported through tracing, never to the proxied client.
pub async fn insert(db: &Db, log: &NewRequestLog) -> sqlx::Result<()> {
    insert_at(db, log, &timestamp::now_utc()).await
}

/// Inserts one log row stamped with an explicit `created_at`, in the
/// canonical `YYYY-MM-DD HH:MM:SS` UTC format. Production always goes
/// through [`insert`], which stamps the current instant; this entry point
/// exists for callers that must place a row at a chosen time.
pub async fn insert_at(db: &Db, log: &NewRequestLog, created_at: &str) -> sqlx::Result<()> {
    let mut sql = Sql::for_db(db);
    sql.push("INSERT INTO request_logs (api_key_id, channel_id, model, status, prompt_tokens, completion_tokens, cached_input_tokens, cached_output_tokens, cache_write_input_tokens, duration_ms, request_body, response_body, error, client_ip, created_at) VALUES (")
        .bind(Arg::OptInt(log.api_key_id))
        .push(", ")
        .bind(Arg::OptInt(log.channel_id))
        .push(", ")
        .bind(Arg::Text(log.model.clone()))
        .push(", ")
        .bind(Arg::OptInt(log.status))
        .push(", ")
        .bind(Arg::OptInt(log.prompt_tokens))
        .push(", ")
        .bind(Arg::OptInt(log.completion_tokens))
        .push(", ")
        .bind(Arg::OptInt(log.cached_input_tokens))
        .push(", ")
        .bind(Arg::OptInt(log.cached_output_tokens))
        .push(", ")
        .bind(Arg::OptInt(log.cache_write_input_tokens))
        .push(", ")
        .bind(Arg::Int(log.duration_ms))
        .push(", ")
        .bind(Arg::Text(log.request_body.clone()))
        .push(", ")
        .bind(Arg::Text(log.response_body.clone()))
        .push(", ")
        .bind(Arg::Text(log.error.clone()))
        .push(", ")
        .bind(Arg::Text(log.client_ip.clone()))
        .push(", ")
        .bind(Arg::Text(Some(created_at.to_string())))
        .push(")");
    db.execute(sql).await?;
    Ok(())
}

/// One stored request log row, as returned by the admin listing.
#[derive(Debug)]
pub struct RequestLog {
    pub id: i64,
    /// NULL when the request was rejected before its key resolved (unknown
    /// key 401, lookup failure); otherwise the key row id.
    pub api_key_id: Option<i64>,
    /// NULL exactly when `api_key_id` is NULL.
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub status: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// Cached prompt tokens the upstream reported; NULL when it reported
    /// none or the row predates the column.
    pub cached_input_tokens: Option<i64>,
    /// Cached completion tokens the upstream reported; NULL when it
    /// reported none or the row predates the column.
    pub cached_output_tokens: Option<i64>,
    /// Prompt tokens the upstream wrote into its cache, reported only by
    /// Anthropic; NULL when it reported none or the row predates the column.
    pub cache_write_input_tokens: Option<i64>,
    pub duration_ms: i64,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub error: Option<String>,
    pub client_ip: Option<String>,
    pub created_at: String,
}

/// Coarse bucket over the upstream HTTP status, used by the `status_class`
/// filter of the admin listing. A row whose request never reached the
/// upstream (null status) counts as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Ok2xx,
    Client4xx,
    Server5xx,
    Errors,
}

impl StatusClass {
    /// Parses the URL value; `None` means the value is not a known bucket.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "2xx" => Some(Self::Ok2xx),
            "4xx" => Some(Self::Client4xx),
            "5xx" => Some(Self::Server5xx),
            "errors" => Some(Self::Errors),
            _ => None,
        }
    }
}

/// One response status class of the daily status chart, and the value the
/// `status_class` query parameter of the stats page carries.
///
/// The buckets differ from [`StatusClass`], which serves the log listing: the
/// listing folds 4xx and 5xx together into `errors`, while the chart shows
/// each class on its own and keeps everything outside the three named ranges
/// (1xx, 3xx, and a NULL status, which is a request that never reached an
/// upstream) in [`StatusBucket::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusBucket {
    Ok2xx,
    Client4xx,
    Server5xx,
    Other,
}

impl StatusBucket {
    /// Every class, in the order the status chart stacks them bottom first.
    pub const ALL: [StatusBucket; 4] = [
        StatusBucket::Ok2xx,
        StatusBucket::Client4xx,
        StatusBucket::Server5xx,
        StatusBucket::Other,
    ];

    /// The label of the class: the value the bucket expression below emits
    /// for its rows and the URL value of the filter, so one string names a
    /// class in the query, in the filter and in the legend.
    pub fn label(self) -> &'static str {
        match self {
            StatusBucket::Ok2xx => "2xx",
            StatusBucket::Client4xx => "4xx",
            StatusBucket::Server5xx => "5xx",
            StatusBucket::Other => "other",
        }
    }

    /// Parses a class label; `None` means the value names no class.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.label() == raw)
    }
}

/// The SQL expression that buckets the nullable INTEGER `status` column into
/// the four [`StatusBucket`] labels.
///
/// One expression serves both the label of the grouping and the `GROUP BY`
/// itself, so a row is counted under exactly the class it is grouped by. The
/// three named ranges are the classes of the chart, and everything else, a
/// NULL status included, falls into `other`: `BETWEEN` is false for a NULL
/// operand, so a request that never reached an upstream lands there instead
/// of vanishing from the chart.
const STATUS_BUCKET_SQL: &str = "CASE WHEN status BETWEEN 200 AND 299 THEN '2xx' \
     WHEN status BETWEEN 400 AND 499 THEN '4xx' \
     WHEN status BETWEEN 500 AND 599 THEN '5xx' \
     ELSE 'other' END";

/// Filters for the admin listing; every field is optional. `from` and `to`
/// are inclusive bounds already normalized to the `YYYY-MM-DD HH:MM:SS` UTC
/// text format the `created_at` column stores, so string comparison against
/// the column is correct and can use the time-ordered index. `search` is a
/// free-text needle matched against the stored bodies and the error text.
pub struct LogFilters {
    pub key_id: Option<i64>,
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub status_class: Option<StatusClass>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub search: Option<String>,
    pub page: u64,
    pub page_size: u64,
}

/// The escape character of the search `LIKE` predicate, so a `%` or a `_` in
/// the needle matches itself instead of acting as a wildcard.
const LIKE_ESCAPE: char = '\\';

/// The text columns the search needle is matched against: the two captured
/// bodies and the recorded failure. A row matches when any of them holds the
/// needle.
const SEARCH_COLUMNS: [&str; 3] = ["request_body", "response_body", "error"];

/// The `LIKE` pattern of the search: the needle lowercased and wrapped in
/// `%`, with the LIKE metacharacters (`%`, `_`) and the escape character
/// itself backslash-escaped so the user's text is matched literally.
///
/// The pattern is a bind value, never SQL text, and the predicate compares
/// `LOWER(column)` against it on both backends, so a needle finds the same
/// rows on SQLite and on PostgreSQL. Full-text indexing is deliberately
/// avoided: a tokenizer would miss the substrings users actually search for
/// (an order id inside a JSON body) and would force a different query per
/// backend.
fn like_pattern(needle: &str) -> String {
    let mut pattern = String::with_capacity(needle.len() + 2);
    pattern.push('%');
    for ch in needle.to_lowercase().chars() {
        if matches!(ch, '%' | '_') || ch == LIKE_ESCAPE {
            pattern.push(LIKE_ESCAPE);
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// Adds one `LIKE <needle> ESCAPE '\'` probe per searched column, `OR`-ed
/// together: a row matches when any of the three holds the needle, and a
/// NULL column matches nothing.
fn push_search(sql: &mut Sql, needle: &str) {
    let pattern = like_pattern(needle);
    sql.push(" AND (");
    for (index, column) in SEARCH_COLUMNS.iter().enumerate() {
        if index > 0 {
            sql.push(" OR ");
        }
        sql.push("LOWER(COALESCE(")
            .push(column)
            .push(", '')) LIKE ")
            .bind(Arg::Text(Some(pattern.clone())))
            .push(" ESCAPE '\\'");
    }
    sql.push(")");
}

/// Adds the filter conditions to a SQL body. Values always travel as bind
/// parameters; user input never lands in the SQL text itself.
fn push_conditions(sql: &mut Sql, filters: &LogFilters) {
    sql.push("1=1");
    if let Some(key_id) = filters.key_id {
        sql.push(" AND api_key_id = ").bind(Arg::Int(key_id));
    }
    if let Some(channel_id) = filters.channel_id {
        sql.push(" AND channel_id = ").bind(Arg::Int(channel_id));
    }
    if let Some(model) = &filters.model {
        sql.push(" AND model = ")
            .bind(Arg::Text(Some(model.clone())));
    }
    if let Some(status_class) = filters.status_class {
        match status_class {
            StatusClass::Ok2xx => {
                sql.push(" AND status >= ").bind(Arg::Int(200));
                sql.push(" AND status < ").bind(Arg::Int(300));
            }
            StatusClass::Client4xx => {
                sql.push(" AND status >= ").bind(Arg::Int(400));
                sql.push(" AND status < ").bind(Arg::Int(500));
            }
            StatusClass::Server5xx => {
                sql.push(" AND status >= ").bind(Arg::Int(500));
                sql.push(" AND status < ").bind(Arg::Int(600));
            }
            // Failed transports have a null status and still count.
            StatusClass::Errors => {
                sql.push(" AND (status IS NULL OR status >= ")
                    .bind(Arg::Int(400));
                sql.push(")");
            }
        }
    }
    if let Some(from) = &filters.from {
        sql.push(" AND created_at >= ")
            .bind(Arg::Text(Some(from.clone())));
    }
    if let Some(to) = &filters.to {
        sql.push(" AND created_at <= ")
            .bind(Arg::Text(Some(to.clone())));
    }
    if let Some(needle) = &filters.search {
        push_search(sql, needle);
    }
}

fn map_row(row: &Row) -> sqlx::Result<RequestLog> {
    Ok(RequestLog {
        id: row.int("id")?,
        api_key_id: row.opt_int("api_key_id")?,
        channel_id: row.opt_int("channel_id")?,
        model: row.opt_text("model")?,
        status: row.opt_int("status")?,
        prompt_tokens: row.opt_int("prompt_tokens")?,
        completion_tokens: row.opt_int("completion_tokens")?,
        cached_input_tokens: row.opt_int("cached_input_tokens")?,
        cached_output_tokens: row.opt_int("cached_output_tokens")?,
        cache_write_input_tokens: row.opt_int("cache_write_input_tokens")?,
        duration_ms: row.int("duration_ms")?,
        request_body: row.opt_text("request_body")?,
        response_body: row.opt_text("response_body")?,
        error: row.opt_text("error")?,
        client_ip: row.opt_text("client_ip")?,
        created_at: row.text("created_at")?,
    })
}

/// Column list shared by the listing and the per-row lookup so both stay in
/// sync with `map_row`.
const SELECT_COLUMNS: &str = "id, api_key_id, channel_id, model, status, prompt_tokens, \
     completion_tokens, cached_input_tokens, cached_output_tokens, cache_write_input_tokens, \
     duration_ms, request_body, response_body, error, client_ip, created_at";

/// Fetches one log row by id; `None` when no row carries this id. Backs the
/// admin detail endpoint, which needs the stored bodies alongside the
/// metadata the listing shows.
pub async fn find(db: &Db, id: i64) -> sqlx::Result<Option<RequestLog>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT ")
        .push(SELECT_COLUMNS)
        .push(" FROM request_logs WHERE id = ")
        .bind(Arg::Int(id));
    let row = db.fetch_optional(sql).await?;
    row.as_ref().map(map_row).transpose()
}

/// One count query plus one page query, both sharing the same dynamically
/// built WHERE clause. Rows are ordered by id DESC (newest first).
pub async fn list(db: &Db, filters: &LogFilters) -> sqlx::Result<(Vec<RequestLog>, i64)> {
    let mut count = Sql::for_db(db);
    count.push("SELECT COUNT(*) FROM request_logs WHERE ");
    push_conditions(&mut count, filters);
    let total = db.scalar(count).await?;

    let mut page = Sql::for_db(db);
    page.push("SELECT ")
        .push(SELECT_COLUMNS)
        .push(" FROM request_logs WHERE ");
    push_conditions(&mut page, filters);
    page.push(" ORDER BY id DESC LIMIT ")
        .bind(Arg::Int(filters.page_size as i64))
        .push(" OFFSET ")
        .bind(Arg::Int(
            filters
                .page
                .saturating_sub(1)
                .saturating_mul(filters.page_size)
                .min(i64::MAX as u64) as i64,
        ));
    let rows = db.fetch_all(page).await?;
    let items = rows.iter().map(map_row).collect::<sqlx::Result<Vec<_>>>()?;
    Ok((items, total))
}

/// One model's month-to-date token totals for one key, as the monthly-limit
/// evaluation reads them.
#[derive(Debug, PartialEq, Eq)]
pub struct ModelUsage {
    /// The logged model name; NULL for rows that carry no model.
    pub model: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_input_tokens: i64,
    pub cached_output_tokens: i64,
    /// Prompt tokens the key's requests wrote into the upstream cache, the
    /// dimension the write price applies to.
    pub cache_write_input_tokens: i64,
    /// Number of logged requests in the group, whatever their status.
    pub requests: i64,
}

/// Month-to-date usage of one key, grouped by model.
///
/// The month window is a range predicate on `created_at` in the WHERE
/// clause, which is what the `(api_key_id, created_at)` index serves;
/// `substr(created_at, 1, 7)` appears only as the label of the grouping,
/// never as a filter that would defeat that index. Rows without an
/// `api_key_id` (rejections whose key never resolved) are excluded, and a
/// NULL token count contributes nothing, so a group whose rows all lack
/// tokens still counts its requests and sums to zero. The sums are cast to
/// `BIGINT` because PostgreSQL's `SUM` over `BIGINT` returns `NUMERIC`
/// while SQLite's stays an integer, and both must read back as `i64`.
pub async fn month_to_date_usage(
    db: &Db,
    key_id: i64,
    month: &crate::month::Month,
) -> sqlx::Result<Vec<ModelUsage>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT model, substr(created_at, 1, 7) AS month_bucket, ")
        .push("CAST(COALESCE(SUM(prompt_tokens), 0) AS BIGINT) AS prompt_tokens, ")
        .push("CAST(COALESCE(SUM(completion_tokens), 0) AS BIGINT) AS completion_tokens, ")
        .push("CAST(COALESCE(SUM(cached_input_tokens), 0) AS BIGINT) AS cached_input_tokens, ")
        .push("CAST(COALESCE(SUM(cached_output_tokens), 0) AS BIGINT) AS cached_output_tokens, ")
        .push(
            "CAST(COALESCE(SUM(cache_write_input_tokens), 0) AS BIGINT) AS cache_write_input_tokens, ",
        )
        .push("COUNT(*) AS request_count FROM request_logs ")
        .push("WHERE api_key_id IS NOT NULL AND api_key_id = ")
        .bind(Arg::Int(key_id))
        .push(" AND created_at >= ")
        .bind(Arg::Text(Some(month.start())))
        .push(" AND created_at < ")
        .bind(Arg::Text(Some(month.end())))
        .push(" GROUP BY model, substr(created_at, 1, 7)");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_usage).collect()
}

fn map_usage(row: &Row) -> sqlx::Result<ModelUsage> {
    Ok(ModelUsage {
        model: row.opt_text("model")?,
        prompt_tokens: row.int("prompt_tokens")?,
        completion_tokens: row.int("completion_tokens")?,
        cached_input_tokens: row.int("cached_input_tokens")?,
        cached_output_tokens: row.int("cached_output_tokens")?,
        cache_write_input_tokens: row.int("cache_write_input_tokens")?,
        requests: row.int("request_count")?,
    })
}

/// Day-level usage of one (day, key, model) group, as the charts page reads
/// it.
///
/// The token fields repeat the dimensions of [`ModelUsage`], cache
/// dimensions included, so a day group can be priced by the same
/// [`crate::usage::group_cost`] fold the table and the monthly-limit gate
/// use: pricing a day from a row that dropped the cache counts would charge
/// cached tokens at the full input rate and disagree with the table on the
/// same page.
#[derive(Debug, PartialEq, Eq)]
pub struct DayUsage {
    /// The `YYYY-MM-DD` label of the row's day, the label of the grouping.
    pub day: String,
    /// The key the requests were made with; never NULL in a returned row.
    pub key_id: i64,
    /// The logged model name; NULL for rows that carry no model.
    pub model: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Prompt tokens the day's rows served from the upstream cache.
    pub cached_input_tokens: i64,
    /// Completion tokens the day's rows served from the upstream cache.
    pub cached_output_tokens: i64,
    /// Prompt tokens the day's rows wrote into the upstream cache, the
    /// dimension the write price applies to.
    pub cache_write_input_tokens: i64,
    /// Number of logged requests in the group, whatever their status.
    pub requests: i64,
}

/// Day-level token and request totals inside a date range, one group per
/// day, key and model.
///
/// The window is a half-open range on `created_at` in the WHERE clause
/// (`>= start` and `< end`), which is what the `(api_key_id, created_at)`
/// index serves; `substr(created_at, 1, 10)` appears only as the label of
/// the grouping,
/// never as a filter that would defeat that index. Every token dimension the
/// pricing path reads is summed, the cache dimensions included, so a caller
/// that prices a day gets the same figure the month aggregate prices. Rows
/// without an `api_key_id` (rejections whose key never resolved) are
/// excluded. A `model` filter naturally drops the NULL-model rows, while
/// without it those rows form their own group. A NULL token count contributes
/// nothing, so a group whose rows all lack tokens still counts its requests
/// and sums to zero. The sums are cast to `BIGINT` because PostgreSQL's `SUM`
/// over `BIGINT` returns `NUMERIC` while SQLite's stays an integer, and both
/// must read back as `i64`.
pub async fn day_usage(
    db: &Db,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
) -> sqlx::Result<Vec<DayUsage>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT substr(created_at, 1, 10) AS day, api_key_id, model, ")
        .push("CAST(COALESCE(SUM(prompt_tokens), 0) AS BIGINT) AS prompt_tokens, ")
        .push("CAST(COALESCE(SUM(completion_tokens), 0) AS BIGINT) AS completion_tokens, ")
        .push("CAST(COALESCE(SUM(cached_input_tokens), 0) AS BIGINT) AS cached_input_tokens, ")
        .push("CAST(COALESCE(SUM(cached_output_tokens), 0) AS BIGINT) AS cached_output_tokens, ")
        .push(
            "CAST(COALESCE(SUM(cache_write_input_tokens), 0) AS BIGINT) AS cache_write_input_tokens, ",
        )
        .push("COUNT(*) AS request_count FROM request_logs ")
        .push("WHERE api_key_id IS NOT NULL AND created_at >= ")
        .bind(Arg::Text(Some(range_start.to_string())))
        .push(" AND created_at < ")
        .bind(Arg::Text(Some(range_end.to_string())));
    if let Some(key_id) = key_id {
        sql.push(" AND api_key_id = ").bind(Arg::Int(key_id));
    }
    if let Some(model) = model {
        sql.push(" AND model = ").bind(Arg::Text(Some(model)));
    }
    sql.push(" GROUP BY substr(created_at, 1, 10), api_key_id, model")
        .push(" ORDER BY day, api_key_id, model");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_day_usage).collect()
}

fn map_day_usage(row: &Row) -> sqlx::Result<DayUsage> {
    Ok(DayUsage {
        day: row.text("day")?,
        key_id: row.int("api_key_id")?,
        model: row.opt_text("model")?,
        prompt_tokens: row.int("prompt_tokens")?,
        completion_tokens: row.int("completion_tokens")?,
        cached_input_tokens: row.int("cached_input_tokens")?,
        cached_output_tokens: row.int("cached_output_tokens")?,
        cache_write_input_tokens: row.int("cache_write_input_tokens")?,
        requests: row.int("request_count")?,
    })
}

/// One (hour, key, model) group of the hourly usage aggregate, the token
/// dimensions a bucket is priced from.
///
/// The token fields repeat the dimensions of [`DayUsage`], cache dimensions
/// included, so an hour group is priced by the same
/// [`crate::usage::group_cost`] fold the table and the monthly-limit gate
/// use: pricing an hour from a row that dropped the cache counts would
/// charge cached tokens at the full input rate and disagree with the day
/// chart of the same page.
#[derive(Debug, PartialEq, Eq)]
pub struct HourUsage {
    /// The `YYYY-MM-DD HH` label of the row's hour, the label of the
    /// grouping.
    pub hour: String,
    /// The key the requests were made with; never NULL in a returned row.
    pub key_id: i64,
    /// The logged model name; NULL for rows that carry no model.
    pub model: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Prompt tokens the hour's rows served from the upstream cache.
    pub cached_input_tokens: i64,
    /// Completion tokens the hour's rows served from the upstream cache.
    pub cached_output_tokens: i64,
    /// Prompt tokens the hour's rows wrote into the upstream cache, the
    /// dimension the write price applies to.
    pub cache_write_input_tokens: i64,
    /// Number of logged requests in the group, whatever their status.
    pub requests: i64,
}

/// Hourly token and request totals inside a date range, one group per hour,
/// key and model: the hourly twin of [`day_usage`].
///
/// The window is the same half-open range on `created_at` (`>= start` and
/// `< end`), which is what the `(api_key_id, created_at)` index serves, and
/// `substr(created_at, 1, 13)` appears only as the label of the grouping,
/// never as a filter that would defeat that index. Every token dimension the
/// pricing path reads is summed, the cache dimensions included, so a caller
/// that prices an hour gets the same figure the day aggregate prices. Rows
/// without an `api_key_id` (rejections whose key never resolved) are
/// excluded, exactly as in [`day_usage`]. A `model` filter naturally drops
/// the NULL-model rows, while without it those rows form their own group. A
/// NULL token count contributes nothing, so a group whose rows all lack
/// tokens still counts its requests and sums to zero. The sums are cast to
/// `BIGINT` because PostgreSQL's `SUM` over `BIGINT` returns `NUMERIC` while
/// SQLite's stays an integer, and both must read back as `i64`.
pub async fn hour_usage(
    db: &Db,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) -> sqlx::Result<Vec<HourUsage>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT substr(created_at, 1, 13) AS hour, api_key_id, model, ")
        .push("CAST(COALESCE(SUM(prompt_tokens), 0) AS BIGINT) AS prompt_tokens, ")
        .push("CAST(COALESCE(SUM(completion_tokens), 0) AS BIGINT) AS completion_tokens, ")
        .push("CAST(COALESCE(SUM(cached_input_tokens), 0) AS BIGINT) AS cached_input_tokens, ")
        .push("CAST(COALESCE(SUM(cached_output_tokens), 0) AS BIGINT) AS cached_output_tokens, ")
        .push(
            "CAST(COALESCE(SUM(cache_write_input_tokens), 0) AS BIGINT) AS cache_write_input_tokens, ",
        )
        .push("COUNT(*) AS request_count FROM request_logs ");
    push_status_range(
        &mut sql,
        range_start,
        range_end,
        key_id,
        model,
        status_class,
    );
    sql.push(" GROUP BY substr(created_at, 1, 13), api_key_id, model")
        .push(" ORDER BY hour, api_key_id, model");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_hour_usage).collect()
}

fn map_hour_usage(row: &Row) -> sqlx::Result<HourUsage> {
    Ok(HourUsage {
        hour: row.text("hour")?,
        key_id: row.int("api_key_id")?,
        model: row.opt_text("model")?,
        prompt_tokens: row.int("prompt_tokens")?,
        completion_tokens: row.int("completion_tokens")?,
        cached_input_tokens: row.int("cached_input_tokens")?,
        cached_output_tokens: row.int("cached_output_tokens")?,
        cache_write_input_tokens: row.int("cache_write_input_tokens")?,
        requests: row.int("request_count")?,
    })
}

/// Request count of one (day, status class) group, as the status chart of
/// the stats page reads it.
#[derive(Debug, PartialEq, Eq)]
pub struct DayStatusUsage {
    /// The `YYYY-MM-DD` label of the row's day, the label of the grouping.
    pub day: String,
    /// The status class of the group, always one of the four buckets.
    pub status_class: StatusBucket,
    /// Number of logged requests in the group, whatever their token columns
    /// hold.
    pub requests: i64,
}

/// Request counts per day and status class inside a date range.
///
/// The window is a half-open range on `created_at` in the WHERE clause
/// (`>= start` and `< end`), which is what the `(api_key_id, created_at)`
/// index serves; `substr(created_at, 1, 10)` appears only as the label of
/// the grouping,
/// never as a filter that would defeat that index. Rows without an
/// `api_key_id` (rejections whose key never resolved) are excluded, exactly
/// as in [`day_usage`], so the status chart counts the same requests the
/// token chart plots. The class comes from one bucket expression in the
/// SELECT list and in the GROUP BY; a `status_class` filter
/// compares the same expression, so filtering and grouping can never
/// disagree. Sums are counted with `COUNT(*)`, which counts a row whatever
/// its token columns hold and returns an integer on both backends.
pub async fn day_status_usage(
    db: &Db,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) -> sqlx::Result<Vec<DayStatusUsage>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT substr(created_at, 1, 10) AS day, ")
        .push(STATUS_BUCKET_SQL)
        .push(" AS status_class, COUNT(*) AS request_count FROM request_logs ");
    push_status_range(
        &mut sql,
        range_start,
        range_end,
        key_id,
        model,
        status_class,
    );
    sql.push(" GROUP BY substr(created_at, 1, 10), ")
        .push(STATUS_BUCKET_SQL)
        .push(" ORDER BY day, status_class");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_day_status_usage).collect()
}

/// Maps one grouped row; a label the bucket expression does not know counts
/// as `other` rather than dropping the requests behind it.
fn map_day_status_usage(row: &Row) -> sqlx::Result<DayStatusUsage> {
    let label = row.text("status_class")?;
    Ok(DayStatusUsage {
        day: row.text("day")?,
        status_class: StatusBucket::parse(&label).unwrap_or(StatusBucket::Other),
        requests: row.int("request_count")?,
    })
}

/// Adds the date range and the optional key, model and status class
/// predicates the status and hourly token aggregates share to a SQL body.
///
/// The window is a half-open range on `created_at` (`>= start` and `< end`),
/// which is what the `(api_key_id, created_at)` index serves, and the rows
/// without an `api_key_id` (rejections whose key never resolved) are
/// excluded, so every aggregate that sums usage counts the same requests the
/// token aggregates count. The class
/// predicate compares the bucket expression the queries group by, so
/// filtering and grouping can never disagree.
fn push_status_range(
    sql: &mut Sql,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) {
    sql.push("WHERE api_key_id IS NOT NULL AND ");
    push_range_predicates(sql, range_start, range_end, key_id, model, status_class);
}

/// Adds the range and filter predicates of [`push_status_range`] without the
/// `api_key_id` guard, for the hourly latency aggregate: a bucket averages
/// every timed request it holds, whatever key the request carried.
fn push_latency_range(
    sql: &mut Sql,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) {
    sql.push("WHERE ");
    push_range_predicates(sql, range_start, range_end, key_id, model, status_class);
}

/// The half-open `created_at` window and the optional key, model and status
/// class predicates the hourly aggregates share, appended after the caller
/// has opened the WHERE clause.
fn push_range_predicates(
    sql: &mut Sql,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) {
    sql.push("created_at >= ")
        .bind(Arg::Text(Some(range_start.to_string())))
        .push(" AND created_at < ")
        .bind(Arg::Text(Some(range_end.to_string())));
    if let Some(key_id) = key_id {
        sql.push(" AND api_key_id = ").bind(Arg::Int(key_id));
    }
    if let Some(model) = model {
        sql.push(" AND model = ").bind(Arg::Text(Some(model)));
    }
    if let Some(status_class) = status_class {
        sql.push(" AND ")
            .push(STATUS_BUCKET_SQL)
            .push(" = ")
            .bind(Arg::Text(Some(status_class.label().to_string())));
    }
}

/// Request count of one (day, exact status code) group, as the per-code
/// detail of the requests chart of the stats page reads it.
#[derive(Debug, PartialEq, Eq)]
pub struct DayStatusCodeUsage {
    /// The `YYYY-MM-DD` label of the row's day, the label of the grouping.
    pub day: String,
    /// The exact upstream HTTP status of the group. NULL for a request that
    /// never reached an upstream, which is a group of its own rather than a
    /// fabricated code.
    pub status: Option<i64>,
    /// The class the status falls into, from the same bucket expression the
    /// `status_class` filter compares against.
    pub status_class: StatusBucket,
    /// Number of logged requests in the group.
    pub requests: i64,
}

/// Request counts per day and exact status code inside a date range, the
/// per-code detail of the requests chart.
///
/// Same window, filters and exclusions as [`day_status_usage`], which this
/// query refines: where that one folds the codes of one day into their four
/// classes, this one keeps every code apart, and the class travels along as
/// the bucket the code falls into. `substr(created_at, 1, 10)` appears only
/// as the label of the grouping and in the GROUP BY, never as a filter, so
/// the index still serves the window. The class comes from the shared
/// expression in the SELECT list and in the GROUP BY, which is also what the
/// `status_class` predicate compares. Rows are ordered by day, then by code,
/// with the requests that carry no status last.
pub async fn day_status_code_usage(
    db: &Db,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) -> sqlx::Result<Vec<DayStatusCodeUsage>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT substr(created_at, 1, 10) AS day, status AS status_code, ")
        .push(STATUS_BUCKET_SQL)
        .push(" AS status_class, COUNT(*) AS request_count FROM request_logs ");
    push_status_range(
        &mut sql,
        range_start,
        range_end,
        key_id,
        model,
        status_class,
    );
    sql.push(" GROUP BY substr(created_at, 1, 10), status, ")
        .push(STATUS_BUCKET_SQL)
        .push(" ORDER BY day, status IS NULL, status");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_day_status_code_usage).collect()
}

/// Maps one grouped per-code row; a label the bucket expression does not know
/// counts as `other` rather than dropping the requests behind it.
fn map_day_status_code_usage(row: &Row) -> sqlx::Result<DayStatusCodeUsage> {
    let label = row.text("status_class")?;
    Ok(DayStatusCodeUsage {
        day: row.text("day")?,
        status: row.opt_int("status_code")?,
        status_class: StatusBucket::parse(&label).unwrap_or(StatusBucket::Other),
        requests: row.int("request_count")?,
    })
}

/// Request count of one (hour, exact status code) group, the per-code detail
/// of the hourly requests chart.
#[derive(Debug, PartialEq, Eq)]
pub struct HourStatusCodeUsage {
    /// The `YYYY-MM-DD HH` label of the row's hour, the label of the
    /// grouping.
    pub hour: String,
    /// The exact upstream HTTP status of the group. NULL for a request that
    /// never reached an upstream, which is a group of its own rather than a
    /// fabricated code.
    pub status: Option<i64>,
    /// The class the status falls into, from the same bucket expression the
    /// `status_class` filter compares against.
    pub status_class: StatusBucket,
    /// Number of logged requests in the group.
    pub requests: i64,
}

/// Request counts per hour and exact status code inside a date range, the
/// per-code detail of the requests chart at hourly granularity: the hourly
/// twin of [`day_status_code_usage`].
///
/// Same window, filters and exclusions as [`day_status_code_usage`]:
/// `substr(created_at, 1, 13)` appears only as the label of the grouping and
/// in the GROUP BY, never as a filter, so the index still serves the window.
/// The class comes from the shared expression in the SELECT list and in the
/// GROUP BY, which is also what the `status_class` predicate compares. Rows
/// are ordered by hour, then by code, with the requests that carry no status
/// last.
pub async fn hour_status_code_usage(
    db: &Db,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) -> sqlx::Result<Vec<HourStatusCodeUsage>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT substr(created_at, 1, 13) AS hour, status AS status_code, ")
        .push(STATUS_BUCKET_SQL)
        .push(" AS status_class, COUNT(*) AS request_count FROM request_logs ");
    push_status_range(
        &mut sql,
        range_start,
        range_end,
        key_id,
        model,
        status_class,
    );
    sql.push(" GROUP BY substr(created_at, 1, 13), status, ")
        .push(STATUS_BUCKET_SQL)
        .push(" ORDER BY hour, status IS NULL, status");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_hour_status_code_usage).collect()
}

/// Maps one grouped hourly per-code row; a label the bucket expression does
/// not know counts as `other` rather than dropping the requests behind it.
fn map_hour_status_code_usage(row: &Row) -> sqlx::Result<HourStatusCodeUsage> {
    let label = row.text("status_class")?;
    Ok(HourStatusCodeUsage {
        hour: row.text("hour")?,
        status: row.opt_int("status_code")?,
        status_class: StatusBucket::parse(&label).unwrap_or(StatusBucket::Other),
        requests: row.int("request_count")?,
    })
}

/// Mean request duration of one hour bucket, as the latency chart reads it.
#[derive(Debug, PartialEq)]
pub struct HourLatency {
    /// The `YYYY-MM-DD HH` label of the hour, the label of the grouping.
    pub hour: String,
    /// The mean `duration_ms` over every row of the bucket, in
    /// milliseconds. `duration_ms` is NOT NULL on every request log row, so
    /// a bucket holds at least one timed row and the mean is never NULL.
    pub avg_duration_ms: f64,
    /// Number of timed requests the mean covers.
    pub requests: i64,
}

/// Mean request duration per hour bucket inside a date range.
///
/// The window and the optional key, model and status class predicates are
/// the ones the other hourly aggregates take, but this one keeps the rows
/// whose `api_key_id` never resolved: an hour averages every timed request
/// it holds, whatever key the request carried, so a rejection is not
/// silently dropped from the picture. `duration_ms` is NOT NULL on every
/// row, so no row is excluded for lack of a duration and a bucket always has
/// a mean. The bucket label is `substr(created_at, 1, 13)`, `YYYY-MM-DD HH`,
/// formed without a filter of its own so the index still serves the window.
/// The average is cast to `DOUBLE PRECISION`, which SQLite reads as `REAL`,
/// so both backends read it back as `f64`.
pub async fn hour_latency(
    db: &Db,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) -> sqlx::Result<Vec<HourLatency>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT substr(created_at, 1, 13) AS hour, ")
        .push("CAST(AVG(duration_ms) AS DOUBLE PRECISION) AS avg_duration_ms, ")
        .push("COUNT(*) AS request_count FROM request_logs ");
    push_latency_range(
        &mut sql,
        range_start,
        range_end,
        key_id,
        model,
        status_class,
    );
    sql.push(" GROUP BY substr(created_at, 1, 13) ORDER BY hour");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_hour_latency).collect()
}

fn map_hour_latency(row: &Row) -> sqlx::Result<HourLatency> {
    Ok(HourLatency {
        hour: row.text("hour")?,
        avg_duration_ms: row.float("avg_duration_ms")?,
        requests: row.int("request_count")?,
    })
}

/// Request count of one exact status code over the whole month window, as the
/// status code table of the stats page reads it.
#[derive(Debug, PartialEq, Eq)]
pub struct StatusCodeTotal {
    /// The exact upstream HTTP status; NULL for the requests of the period
    /// that never reached an upstream, which are one group of their own.
    pub status: Option<i64>,
    /// The class the status falls into, from the same bucket expression the
    /// `status_class` filter compares against.
    pub status_class: StatusBucket,
    /// Number of logged requests the period logged under this code.
    pub requests: i64,
}

/// Request counts per exact status code over a date range, without the day
/// dimension, for the status code table.
///
/// Same window, filters and exclusions as [`day_status_code_usage`]; the
/// order is by request count, largest first, with the exact code as the
/// tie-breaker and the requests that carry no status last, so the table reads
/// the busiest code first and never depends on the order the backend happens
/// to return NULLs in.
pub async fn status_code_totals(
    db: &Db,
    range_start: &str,
    range_end: &str,
    key_id: Option<i64>,
    model: Option<String>,
    status_class: Option<StatusBucket>,
) -> sqlx::Result<Vec<StatusCodeTotal>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT status AS status_code, ")
        .push(STATUS_BUCKET_SQL)
        .push(" AS status_class, COUNT(*) AS request_count FROM request_logs ");
    push_status_range(
        &mut sql,
        range_start,
        range_end,
        key_id,
        model,
        status_class,
    );
    sql.push(" GROUP BY status, ")
        .push(STATUS_BUCKET_SQL)
        .push(" ORDER BY request_count DESC, status IS NULL, status");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_status_code_total).collect()
}

/// Maps one grouped per-code row of the table aggregate.
fn map_status_code_total(row: &Row) -> sqlx::Result<StatusCodeTotal> {
    let label = row.text("status_class")?;
    Ok(StatusCodeTotal {
        status: row.opt_int("status_code")?,
        status_class: StatusBucket::parse(&label).unwrap_or(StatusBucket::Other),
        requests: row.int("request_count")?,
    })
}

/// The distinct model names logged in a date range, sorted, for the model
/// filter of the charts page.
///
/// The window is a half-open range on `created_at` (`>= start` and `< end`),
/// so the same index serves it, and rows without a model contribute nothing
/// to a list of names.
pub async fn distinct_models(
    db: &Db,
    range_start: &str,
    range_end: &str,
) -> sqlx::Result<Vec<String>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT DISTINCT model FROM request_logs ")
        .push("WHERE model IS NOT NULL AND created_at >= ")
        .bind(Arg::Text(Some(range_start.to_string())))
        .push(" AND created_at < ")
        .bind(Arg::Text(Some(range_end.to_string())))
        .push(" ORDER BY model");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(|row| row.text("model")).collect()
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::db::channels::{self, NewChannel};
    use crate::db::keys::{self, NewKey};

    async fn test_db() -> Db {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        Db::from(pool)
    }

    /// A channel plus a key bound to it; returns the key id and channel id.
    async fn seed(db: &Db, tag: &str) -> (i64, i64) {
        let channel = channels::create(
            db,
            &NewChannel {
                name: format!("channel-{tag}"),
                base_url: "http://127.0.0.1:9".to_string(),
                api_key: "sk-upstream-secret".to_string(),
                allowed_models: None,
            },
        )
        .await
        .unwrap();
        let key = keys::create(
            db,
            &NewKey {
                key_hash: format!("hash-{tag}"),
                display_prefix: "hyd-abcdefgh".to_string(),
                name: None,
                channel_id: channel.id,
                monthly_limit_micro_usd: None,
            },
        )
        .await
        .unwrap();
        (key.id, channel.id)
    }

    fn row(
        model: Option<&str>,
        prompt: Option<i64>,
        completion: Option<i64>,
        cached_input: Option<i64>,
        cached_output: Option<i64>,
    ) -> NewRequestLog {
        NewRequestLog {
            api_key_id: None,
            channel_id: None,
            model: model.map(str::to_string),
            status: Some(200),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_input_tokens: cached_input,
            cached_output_tokens: cached_output,
            cache_write_input_tokens: None,
            duration_ms: 1,
            request_body: None,
            response_body: None,
            error: None,
            client_ip: None,
        }
    }

    /// The same row with a cache-write count, which only the Anthropic
    /// surface reports.
    fn row_with_cache_writes(
        model: Option<&str>,
        prompt: Option<i64>,
        completion: Option<i64>,
        cached_input: Option<i64>,
        cache_write_input: Option<i64>,
    ) -> NewRequestLog {
        NewRequestLog {
            cache_write_input_tokens: cache_write_input,
            ..row(model, prompt, completion, cached_input, None)
        }
    }

    /// A row with an explicit upstream status; `None` is a request that
    /// never reached an upstream.
    fn row_with_status(status: Option<i64>, model: Option<&str>) -> NewRequestLog {
        NewRequestLog {
            status,
            ..row(model, Some(10), Some(1), None, None)
        }
    }

    /// The same row with an explicit duration, for the hourly latency tests.
    fn row_with_duration(
        status: Option<i64>,
        model: Option<&str>,
        duration_ms: i64,
    ) -> NewRequestLog {
        NewRequestLog {
            duration_ms,
            ..row_with_status(status, model)
        }
    }

    /// Whether two averages agree to the precision the tests assert.
    fn approx(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    /// Inserts a row for `key_id` at an explicit timestamp.
    async fn put(db: &Db, key_id: i64, channel_id: i64, usage: NewRequestLog, at: &str) {
        insert_at(
            db,
            &NewRequestLog {
                api_key_id: Some(key_id),
                channel_id: Some(channel_id),
                ..usage
            },
            at,
        )
        .await
        .unwrap();
    }

    fn november() -> crate::month::Month {
        crate::month::Month::from_parts(2026, 11).unwrap()
    }

    #[tokio::test]
    async fn month_to_date_usage_takes_only_the_rows_inside_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "range").await;

        // Last October instant, both November edges, first December instant.
        put(
            &db,
            key_id,
            channel_id,
            row(Some("gpt-4o"), Some(10), Some(1), Some(4), None),
            "2026-10-31 23:59:59",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("gpt-4o"), Some(100), Some(20), Some(40), Some(5)),
            "2026-11-01 00:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("gpt-4o"), Some(50), Some(10), None, None),
            "2026-11-30 23:59:59",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("gpt-4o"), Some(1000), Some(1000), None, None),
            "2026-12-01 00:00:00",
        )
        .await;

        let usage = month_to_date_usage(&db, key_id, &november()).await.unwrap();
        assert_eq!(usage.len(), 1, "one model, one group: {usage:?}");
        assert_eq!(usage[0].model.as_deref(), Some("gpt-4o"));
        assert_eq!(usage[0].prompt_tokens, 150);
        assert_eq!(usage[0].completion_tokens, 30);
        assert_eq!(usage[0].cached_input_tokens, 40);
        assert_eq!(usage[0].cached_output_tokens, 5);
        assert_eq!(usage[0].cache_write_input_tokens, 0);
        assert_eq!(usage[0].requests, 2);
    }

    #[tokio::test]
    async fn month_to_date_usage_groups_by_model_and_skips_other_keys() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "first").await;
        let (second_key, second_channel) = seed(&db, "second").await;

        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(7), Some(1), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(3), Some(2), None, None),
            "2026-11-08 11:00:00",
        )
        .await;
        put(
            &db,
            first_key,
            first_channel,
            row(Some("beta"), Some(9), Some(4), None, None),
            "2026-11-08 12:00:00",
        )
        .await;
        // Another key's spend must not leak into this key's totals.
        put(
            &db,
            second_key,
            second_channel,
            row(Some("alpha"), Some(1000), Some(1000), None, None),
            "2026-11-08 13:00:00",
        )
        .await;
        // A rejection row without a key belongs to nobody.
        insert_at(
            &db,
            &row(Some("alpha"), Some(1000), Some(1000), None, None),
            "2026-11-08 14:00:00",
        )
        .await
        .unwrap();

        let mut usage = month_to_date_usage(&db, first_key, &november())
            .await
            .unwrap();
        usage.sort_by(|a, b| a.model.cmp(&b.model));
        assert_eq!(usage.len(), 2, "{usage:?}");
        assert_eq!(usage[0].model.as_deref(), Some("alpha"));
        assert_eq!(usage[0].prompt_tokens, 10);
        assert_eq!(usage[0].completion_tokens, 3);
        assert_eq!(usage[0].requests, 2);
        assert_eq!(usage[1].model.as_deref(), Some("beta"));
        assert_eq!(usage[1].prompt_tokens, 9);
        assert_eq!(usage[1].requests, 1);
    }

    #[tokio::test]
    async fn a_group_without_token_counts_counts_requests_and_sums_to_zero() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "nulls").await;
        put(
            &db,
            key_id,
            channel_id,
            row(None, None, None, None, None),
            "2026-11-08 10:00:00",
        )
        .await;

        let usage = month_to_date_usage(&db, key_id, &november()).await.unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].model, None);
        assert_eq!(usage[0].prompt_tokens, 0);
        assert_eq!(usage[0].completion_tokens, 0);
        assert_eq!(usage[0].cached_input_tokens, 0);
        assert_eq!(usage[0].cached_output_tokens, 0);
        assert_eq!(usage[0].cache_write_input_tokens, 0);
        assert_eq!(usage[0].requests, 1);
    }

    #[tokio::test]
    async fn the_per_key_month_index_is_migrated_in() {
        let db = test_db().await;
        let mut sql = Sql::for_db(&db);
        sql.push(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = \
             'idx_request_logs_api_key_id_created_at'",
        );
        assert_eq!(
            db.scalar(sql).await.unwrap(),
            1,
            "the per-key month-to-date index must be migrated in"
        );
    }

    #[tokio::test]
    async fn the_cache_write_column_is_migrated_in() {
        let db = test_db().await;
        let mut sql = Sql::for_db(&db);
        sql.push(
            "SELECT COUNT(*) FROM pragma_table_info('request_logs') \
             WHERE name = 'cache_write_input_tokens'",
        );
        assert_eq!(
            db.scalar(sql).await.unwrap(),
            1,
            "the cache-write token column must be migrated in"
        );
    }

    #[tokio::test]
    async fn the_cache_write_column_round_trips_and_sums_into_the_month_aggregate() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "cache-write").await;
        // An Anthropic row: 1300 prompt tokens made of 1000 uncached, 250
        // read and 50 written, 20 completions.
        put(
            &db,
            key_id,
            channel_id,
            row_with_cache_writes(Some("claude"), Some(1300), Some(20), Some(250), Some(50)),
            "2026-11-08 10:00:00",
        )
        .await;
        // A row the upstream reported no write count for adds nothing to the
        // sum but keeps the group alive.
        put(
            &db,
            key_id,
            channel_id,
            row_with_cache_writes(Some("claude"), Some(100), Some(5), None, None),
            "2026-11-08 11:00:00",
        )
        .await;

        let id: i64 = {
            let mut sql = Sql::for_db(&db);
            sql.push("SELECT MIN(id) FROM request_logs");
            db.scalar(sql).await.unwrap()
        };
        let stored = find(&db, id).await.unwrap().expect("the row exists");
        assert_eq!(stored.cache_write_input_tokens, Some(50));
        assert_eq!(stored.cached_input_tokens, Some(250));
        assert_eq!(stored.prompt_tokens, Some(1300));

        let usage = month_to_date_usage(&db, key_id, &november()).await.unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].prompt_tokens, 1400);
        assert_eq!(usage[0].cached_input_tokens, 250);
        assert_eq!(usage[0].cached_output_tokens, 0);
        assert_eq!(
            usage[0].cache_write_input_tokens, 50,
            "the write count is summed per model like the other dimensions"
        );
        assert_eq!(usage[0].requests, 2);
    }

    #[tokio::test]
    async fn a_key_without_rows_in_the_month_has_no_usage() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "empty").await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("gpt-4o"), Some(1), Some(1), None, None),
            "2026-10-05 10:00:00",
        )
        .await;
        assert!(month_to_date_usage(&db, key_id, &november())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn day_usage_groups_by_day_key_and_model() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "days-first").await;
        let (second_key, second_channel) = seed(&db, "days-second").await;

        // One day, two models for the first key.
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(7), Some(1), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(3), Some(2), None, None),
            "2026-11-08 11:00:00",
        )
        .await;
        put(
            &db,
            first_key,
            first_channel,
            row(Some("beta"), Some(9), Some(4), None, None),
            "2026-11-08 12:00:00",
        )
        .await;
        // A second day for the first key.
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(100), Some(10), None, None),
            "2026-11-09 00:00:01",
        )
        .await;
        // Another key on the same day is a separate group.
        put(
            &db,
            second_key,
            second_channel,
            row(Some("alpha"), Some(1000), Some(0), None, None),
            "2026-11-08 13:00:00",
        )
        .await;

        let usage = day_usage(&db, &november().start(), &november().end(), None, None)
            .await
            .unwrap();
        assert_eq!(usage.len(), 4, "{usage:?}");
        // Ordered by day, then key, then model.
        assert_eq!(usage[0].day, "2026-11-08");
        assert_eq!(usage[0].key_id, first_key);
        assert_eq!(usage[0].model.as_deref(), Some("alpha"));
        assert_eq!(usage[0].prompt_tokens, 10);
        assert_eq!(usage[0].completion_tokens, 3);
        assert_eq!(usage[0].requests, 2);
        assert_eq!(usage[1].model.as_deref(), Some("beta"));
        assert_eq!(usage[1].prompt_tokens, 9);
        assert_eq!(usage[1].requests, 1);
        assert_eq!(usage[2].day, "2026-11-08");
        assert_eq!(usage[2].key_id, second_key);
        assert_eq!(usage[2].prompt_tokens, 1000);
        assert_eq!(usage[3].day, "2026-11-09");
        assert_eq!(usage[3].key_id, first_key);
        assert_eq!(usage[3].prompt_tokens, 100);
    }

    #[tokio::test]
    async fn day_usage_takes_only_rows_inside_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "day-range").await;

        // Last October instant, both November edges, first December instant.
        for (at, tokens) in [
            ("2026-10-31 23:59:59", 1000),
            ("2026-11-01 00:00:00", 10),
            ("2026-11-30 23:59:59", 20),
            ("2026-12-01 00:00:00", 1000),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row(Some("alpha"), Some(tokens), Some(0), None, None),
                at,
            )
            .await;
        }

        let usage = day_usage(&db, &november().start(), &november().end(), None, None)
            .await
            .unwrap();
        assert_eq!(usage.len(), 2, "{usage:?}");
        assert_eq!(usage[0].day, "2026-11-01");
        assert_eq!(usage[0].prompt_tokens, 10);
        assert_eq!(usage[1].day, "2026-11-30");
        assert_eq!(usage[1].prompt_tokens, 20);
    }

    #[tokio::test]
    async fn day_usage_excludes_rows_without_a_key() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "day-no-key").await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("alpha"), Some(5), Some(1), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        // A rejection row whose key never resolved belongs to nobody.
        insert_at(
            &db,
            &row(Some("alpha"), Some(1000), Some(1000), None, None),
            "2026-11-08 11:00:00",
        )
        .await
        .unwrap();

        let usage = day_usage(&db, &november().start(), &november().end(), None, None)
            .await
            .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].key_id, key_id);
        assert_eq!(usage[0].prompt_tokens, 5);
    }

    #[tokio::test]
    async fn day_usage_sums_missing_token_counts_to_zero() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "day-zero-sums").await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("alpha"), None, None, None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("alpha"), None, None, None, None),
            "2026-11-08 11:00:00",
        )
        .await;

        let usage = day_usage(&db, &november().start(), &november().end(), None, None)
            .await
            .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].prompt_tokens, 0);
        assert_eq!(usage[0].completion_tokens, 0);
        assert_eq!(usage[0].requests, 2);
    }

    #[tokio::test]
    async fn day_usage_sums_the_cache_dimensions_a_day_was_priced_from() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "day-cache").await;
        // An Anthropic row: 1300 prompt tokens (1000 uncached, 250 read, 50
        // written), 200 completions, 10 of them cached.
        put(
            &db,
            key_id,
            channel_id,
            NewRequestLog {
                cached_output_tokens: Some(10),
                ..row_with_cache_writes(Some("claude"), Some(1300), Some(200), Some(250), Some(50))
            },
            "2026-11-08 10:00:00",
        )
        .await;
        // A row the upstream reported no cache counts for adds nothing to the
        // sums, and a NULL count never becomes a fabricated zero per row.
        put(
            &db,
            key_id,
            channel_id,
            NewRequestLog {
                cached_output_tokens: Some(4),
                ..row_with_cache_writes(Some("claude"), Some(100), Some(5), None, None)
            },
            "2026-11-08 11:00:00",
        )
        .await;

        let usage = day_usage(&db, &november().start(), &november().end(), None, None)
            .await
            .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].prompt_tokens, 1400);
        assert_eq!(usage[0].completion_tokens, 205);
        assert_eq!(usage[0].cached_input_tokens, 250);
        assert_eq!(usage[0].cached_output_tokens, 14);
        assert_eq!(
            usage[0].cache_write_input_tokens, 50,
            "the write count is summed per day like the other dimensions"
        );
        assert_eq!(usage[0].requests, 2);
    }

    #[tokio::test]
    async fn day_usage_model_filter_drops_rows_without_a_model() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "day-model-filter").await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("alpha"), Some(5), Some(1), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row(None, Some(50), Some(50), None, None),
            "2026-11-08 11:00:00",
        )
        .await;

        // Without a filter the NULL-model row forms its own group.
        let all = day_usage(&db, &november().start(), &november().end(), None, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "{all:?}");
        let named = all
            .iter()
            .find(|group| group.model.as_deref() == Some("alpha"))
            .expect("named group");
        assert_eq!(named.prompt_tokens, 5);
        let unnamed = all
            .iter()
            .find(|group| group.model.is_none())
            .expect("NULL-model group");
        assert_eq!(unnamed.prompt_tokens, 50);

        // A model filter naturally drops the NULL-model group.
        let filtered = day_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            Some("alpha".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(filtered.len(), 1, "{filtered:?}");
        assert_eq!(filtered[0].prompt_tokens, 5);
    }

    #[tokio::test]
    async fn day_usage_key_filter_keeps_only_that_key() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "day-key-first").await;
        let (second_key, second_channel) = seed(&db, "day-key-second").await;
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(5), Some(1), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            second_key,
            second_channel,
            row(Some("alpha"), Some(50), Some(10), None, None),
            "2026-11-08 11:00:00",
        )
        .await;

        let usage = day_usage(
            &db,
            &november().start(),
            &november().end(),
            Some(first_key),
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].key_id, first_key);
        assert_eq!(usage[0].prompt_tokens, 5);
    }

    #[tokio::test]
    async fn day_usage_is_empty_without_rows_in_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "day-empty").await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("alpha"), Some(1), Some(1), None, None),
            "2026-10-05 10:00:00",
        )
        .await;

        assert!(
            day_usage(&db, &november().start(), &november().end(), None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn distinct_models_lists_each_month_model_once() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "distinct-models").await;
        for (model, at) in [
            (Some("beta"), "2026-11-02 10:00:00"),
            (None, "2026-11-03 10:00:00"),
            (Some("alpha"), "2026-11-04 10:00:00"),
            (Some("beta"), "2026-11-05 10:00:00"),
            (Some("gamma"), "2026-10-31 23:59:59"),
            (Some("delta"), "2026-12-01 00:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row(model, Some(1), Some(1), None, None),
                at,
            )
            .await;
        }

        let models = distinct_models(&db, &november().start(), &november().end())
            .await
            .unwrap();
        assert_eq!(models, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn distinct_models_is_empty_without_rows_in_the_month() {
        let db = test_db().await;

        assert!(distinct_models(&db, &november().start(), &november().end())
            .await
            .unwrap()
            .is_empty());
    }

    /// The bucket expression is the only place the class labels are written
    /// in SQL, while the filter, the chart and the legend read them from
    /// [`StatusBucket::label`]. The two must name the same strings.
    #[test]
    fn the_bucket_expression_names_every_class_label() {
        for class in StatusBucket::ALL {
            assert!(
                STATUS_BUCKET_SQL.contains(&format!("'{}'", class.label())),
                "{} is unnamed in {STATUS_BUCKET_SQL}",
                class.label()
            );
        }
        assert!(
            STATUS_BUCKET_SQL.ends_with("ELSE 'other' END"),
            "every class outside the named ranges falls into other: {STATUS_BUCKET_SQL}"
        );
    }

    #[tokio::test]
    async fn day_status_usage_groups_each_day_by_status_class() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "status-groups").await;

        for (status, at) in [
            (Some(200), "2026-11-08 10:00:00"),
            (Some(204), "2026-11-08 11:00:00"),
            (Some(404), "2026-11-08 12:00:00"),
            (Some(500), "2026-11-09 10:00:00"),
            (Some(503), "2026-11-09 11:00:00"),
            (Some(503), "2026-11-09 12:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                at,
            )
            .await;
        }

        let usage = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 3, "{usage:?}");
        // Ordered by day, then by the class label.
        assert_eq!(usage[0].day, "2026-11-08");
        assert_eq!(usage[0].status_class, StatusBucket::Ok2xx);
        assert_eq!(usage[0].requests, 2);
        assert_eq!(usage[1].day, "2026-11-08");
        assert_eq!(usage[1].status_class, StatusBucket::Client4xx);
        assert_eq!(usage[1].requests, 1);
        assert_eq!(usage[2].day, "2026-11-09");
        assert_eq!(usage[2].status_class, StatusBucket::Server5xx);
        assert_eq!(usage[2].requests, 3);
    }

    #[tokio::test]
    async fn day_status_usage_buckets_every_status_into_one_of_four_classes() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "status-buckets").await;

        // A NULL status is a request that never reached an upstream, so it
        // belongs with the statuses outside the three named ranges.
        for (index, status) in [
            None,
            Some(100),
            Some(199),
            Some(200),
            Some(299),
            Some(302),
            Some(400),
            Some(499),
            Some(500),
            Some(599),
            Some(600),
            Some(0),
        ]
        .into_iter()
        .enumerate()
        {
            let at = format!("2026-11-08 {:02}:00:00", index + 1);
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                &at,
            )
            .await;
        }

        let usage = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let counts: Vec<(StatusBucket, i64)> = usage
            .iter()
            .map(|group| (group.status_class, group.requests))
            .collect();
        assert_eq!(
            counts,
            vec![
                (StatusBucket::Ok2xx, 2),
                (StatusBucket::Client4xx, 2),
                (StatusBucket::Server5xx, 2),
                (StatusBucket::Other, 6),
            ],
            "{usage:?}"
        );
    }

    #[tokio::test]
    async fn day_status_usage_takes_only_rows_inside_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "status-range").await;

        // Last October instant, both November edges, first December instant.
        for at in [
            "2026-10-31 23:59:59",
            "2026-11-01 00:00:00",
            "2026-11-30 23:59:59",
            "2026-12-01 00:00:00",
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(Some(200), Some("alpha")),
                at,
            )
            .await;
        }

        let usage = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 2, "{usage:?}");
        assert_eq!(usage[0].day, "2026-11-01");
        assert_eq!(usage[0].requests, 1);
        assert_eq!(usage[1].day, "2026-11-30");
        assert_eq!(usage[1].requests, 1);
    }

    #[tokio::test]
    async fn day_status_usage_excludes_rows_without_a_key() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "status-no-key").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(200), Some("alpha")),
            "2026-11-08 10:00:00",
        )
        .await;
        // A rejection row whose key never resolved belongs to nobody, even
        // though it carries a status.
        insert_at(
            &db,
            &row_with_status(Some(401), Some("alpha")),
            "2026-11-08 11:00:00",
        )
        .await
        .unwrap();

        let usage = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].status_class, StatusBucket::Ok2xx);
        assert_eq!(usage[0].requests, 1);
    }

    #[tokio::test]
    async fn day_status_usage_key_filter_keeps_only_that_key() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "status-key-first").await;
        let (second_key, second_channel) = seed(&db, "status-key-second").await;
        put(
            &db,
            first_key,
            first_channel,
            row_with_status(Some(200), Some("alpha")),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            second_key,
            second_channel,
            row_with_status(Some(200), Some("alpha")),
            "2026-11-08 11:00:00",
        )
        .await;
        put(
            &db,
            second_key,
            second_channel,
            row_with_status(Some(500), Some("alpha")),
            "2026-11-08 12:00:00",
        )
        .await;

        let usage = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            Some(second_key),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 2, "{usage:?}");
        assert_eq!(usage[0].status_class, StatusBucket::Ok2xx);
        assert_eq!(usage[0].requests, 1);
        assert_eq!(usage[1].status_class, StatusBucket::Server5xx);
        assert_eq!(usage[1].requests, 1);
    }

    #[tokio::test]
    async fn day_status_usage_model_filter_drops_rows_without_a_model() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "status-model-filter").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(200), Some("alpha")),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(500), None),
            "2026-11-08 11:00:00",
        )
        .await;

        let usage = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            Some("alpha".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].status_class, StatusBucket::Ok2xx);
        assert_eq!(usage[0].requests, 1);
    }

    #[tokio::test]
    async fn day_status_usage_class_filter_keeps_only_that_class() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "status-class-filter").await;
        for status in [Some(200), Some(200), Some(404), Some(500), None] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                "2026-11-08 10:00:00",
            )
            .await;
        }

        let group = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Ok2xx),
        )
        .await
        .unwrap();
        assert_eq!(group.len(), 1, "{group:?}");
        assert_eq!(group[0].status_class, StatusBucket::Ok2xx);
        assert_eq!(group[0].requests, 2);

        let others = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Other),
        )
        .await
        .unwrap();
        assert_eq!(others.len(), 1, "{others:?}");
        assert_eq!(others[0].status_class, StatusBucket::Other);
        assert_eq!(
            others[0].requests, 1,
            "the request that never reached an upstream counts as other"
        );

        let none = day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Client4xx),
        )
        .await
        .unwrap();
        assert_eq!(none.len(), 1, "{none:?}");
        assert_eq!(none[0].requests, 1);
    }

    #[tokio::test]
    async fn day_status_usage_is_empty_without_rows_in_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "status-empty").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(200), Some("alpha")),
            "2026-10-05 10:00:00",
        )
        .await;

        assert!(day_status_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None
        )
        .await
        .unwrap()
        .is_empty());
    }

    /// The per-code aggregate hand-over used by the tests below: day, status
    /// and request count, without the class the query also carries.
    fn codes(usage: &[DayStatusCodeUsage]) -> Vec<(String, Option<i64>, i64)> {
        usage
            .iter()
            .map(|row| (row.day.clone(), row.status, row.requests))
            .collect()
    }

    #[tokio::test]
    async fn day_status_code_usage_keeps_every_code_of_a_day_apart() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "code-groups").await;

        for (status, at) in [
            (Some(200), "2026-11-08 10:00:00"),
            (Some(200), "2026-11-08 11:00:00"),
            (Some(204), "2026-11-08 12:00:00"),
            (Some(404), "2026-11-08 13:00:00"),
            (Some(200), "2026-11-09 10:00:00"),
            (Some(503), "2026-11-09 11:00:00"),
            (Some(503), "2026-11-09 12:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                at,
            )
            .await;
        }

        let usage = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&usage),
            vec![
                ("2026-11-08".to_string(), Some(200), 2),
                ("2026-11-08".to_string(), Some(204), 1),
                ("2026-11-08".to_string(), Some(404), 1),
                ("2026-11-09".to_string(), Some(200), 1),
                ("2026-11-09".to_string(), Some(503), 2),
            ],
            "{usage:?}"
        );
        // Every row carries the class its code falls into, the same bucket
        // the filter compares.
        assert_eq!(usage[0].status_class, StatusBucket::Ok2xx);
        assert_eq!(usage[2].status_class, StatusBucket::Client4xx);
        assert_eq!(usage[4].status_class, StatusBucket::Server5xx);
    }

    #[tokio::test]
    async fn day_status_code_usage_gives_the_requests_without_a_status_their_own_group() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "code-null").await;

        // A NULL status is a request that never reached an upstream: it is
        // not a code, so it stays a group of its own under the other class,
        // ordered after the codes.
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(None, Some("alpha")),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(None, Some("alpha")),
            "2026-11-08 11:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(302), Some("alpha")),
            "2026-11-08 12:00:00",
        )
        .await;

        let usage = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&usage),
            vec![
                ("2026-11-08".to_string(), Some(302), 1),
                ("2026-11-08".to_string(), None, 2),
            ],
            "the unlabelled requests stay apart from every code, and last: {usage:?}"
        );
        assert_eq!(usage[0].status_class, StatusBucket::Other);
        assert_eq!(usage[1].status_class, StatusBucket::Other);
    }

    #[tokio::test]
    async fn day_status_code_usage_takes_only_rows_inside_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "code-range").await;

        // Last October instant, both November edges, first December instant.
        for at in [
            "2026-10-31 23:59:59",
            "2026-11-01 00:00:00",
            "2026-11-30 23:59:59",
            "2026-12-01 00:00:00",
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(Some(200), Some("alpha")),
                at,
            )
            .await;
        }

        let usage = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&usage),
            vec![
                ("2026-11-01".to_string(), Some(200), 1),
                ("2026-11-30".to_string(), Some(200), 1),
            ],
            "{usage:?}"
        );
    }

    #[tokio::test]
    async fn day_status_code_usage_excludes_rows_without_a_key() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "code-no-key").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(200), Some("alpha")),
            "2026-11-08 10:00:00",
        )
        .await;
        // A rejection row whose key never resolved belongs to nobody, even
        // though it carries a status.
        insert_at(
            &db,
            &row_with_status(Some(401), Some("alpha")),
            "2026-11-08 11:00:00",
        )
        .await
        .unwrap();

        let usage = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&usage),
            vec![("2026-11-08".to_string(), Some(200), 1)],
            "{usage:?}"
        );
    }

    #[tokio::test]
    async fn day_status_code_usage_filters_by_key_model_and_class() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "code-key-first").await;
        let (second_key, second_channel) = seed(&db, "code-key-second").await;
        for (key_id, channel_id, status, model, at) in [
            (
                first_key,
                first_channel,
                Some(200),
                Some("alpha"),
                "10:00:00",
            ),
            (first_key, first_channel, Some(200), None, "11:00:00"),
            (
                first_key,
                first_channel,
                Some(404),
                Some("alpha"),
                "12:00:00",
            ),
            (
                second_key,
                second_channel,
                Some(200),
                Some("alpha"),
                "13:00:00",
            ),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, model),
                &format!("2026-11-08 {at}"),
            )
            .await;
        }

        let by_key = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            Some(first_key),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&by_key),
            vec![
                ("2026-11-08".to_string(), Some(200), 2),
                ("2026-11-08".to_string(), Some(404), 1),
            ],
            "the other key's rows stay out: {by_key:?}"
        );

        // A model filter drops the rows that carry no model at all.
        let by_model = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            Some("alpha".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&by_model),
            vec![
                ("2026-11-08".to_string(), Some(200), 2),
                ("2026-11-08".to_string(), Some(404), 1),
            ],
            "{by_model:?}"
        );

        // The class filter narrows by the bucket each code falls into.
        let by_class = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Client4xx),
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&by_class),
            vec![("2026-11-08".to_string(), Some(404), 1)],
            "{by_class:?}"
        );

        let other = day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Other),
        )
        .await
        .unwrap();
        assert!(
            other.is_empty(),
            "no request of the day landed outside: {other:?}"
        );
    }

    #[tokio::test]
    async fn day_status_code_usage_is_empty_without_rows_in_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "code-empty").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(200), Some("alpha")),
            "2026-10-05 10:00:00",
        )
        .await;

        assert!(day_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None
        )
        .await
        .unwrap()
        .is_empty());
    }

    /// The per-code total hand-over: status and request count.
    fn totals(rows: &[StatusCodeTotal]) -> Vec<(Option<i64>, i64)> {
        rows.iter().map(|row| (row.status, row.requests)).collect()
    }

    #[tokio::test]
    async fn status_code_totals_order_the_busiest_code_first() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "totals-order").await;

        for (status, at) in [
            (Some(500), "10:00:00"),
            (Some(500), "11:00:00"),
            (Some(500), "12:00:00"),
            (Some(404), "13:00:00"),
            (Some(200), "14:00:00"),
            (None, "15:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                &format!("2026-11-08 {at}"),
            )
            .await;
        }

        let rows = status_code_totals(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            totals(&rows),
            vec![(Some(500), 3), (Some(200), 1), (Some(404), 1), (None, 1)],
            "largest first, ties by code, the unlabelled requests last: {rows:?}"
        );
        assert_eq!(rows[0].status_class, StatusBucket::Server5xx);
        assert_eq!(rows[3].status_class, StatusBucket::Other);
    }

    #[tokio::test]
    async fn status_code_totals_follow_the_filters_and_stay_inside_the_month() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "totals-key-first").await;
        let (second_key, second_channel) = seed(&db, "totals-key-second").await;
        for (key_id, channel_id, status, model, at) in [
            (
                first_key,
                first_channel,
                Some(200),
                Some("alpha"),
                "2026-11-08 10:00:00",
            ),
            (
                first_key,
                first_channel,
                Some(200),
                Some("beta"),
                "2026-11-08 11:00:00",
            ),
            (
                first_key,
                first_channel,
                Some(500),
                Some("alpha"),
                "2026-11-08 12:00:00",
            ),
            (
                second_key,
                second_channel,
                Some(200),
                Some("alpha"),
                "2026-11-08 13:00:00",
            ),
            (
                first_key,
                first_channel,
                Some(200),
                Some("alpha"),
                "2026-10-31 23:59:59",
            ),
        ] {
            put(&db, key_id, channel_id, row_with_status(status, model), at).await;
        }

        let all = status_code_totals(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            totals(&all),
            vec![(Some(200), 3), (Some(500), 1)],
            "the October row stays out: {all:?}"
        );

        let by_key_model = status_code_totals(
            &db,
            &november().start(),
            &november().end(),
            Some(first_key),
            Some("alpha".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            totals(&by_key_model),
            vec![(Some(200), 1), (Some(500), 1)],
            "{by_key_model:?}"
        );

        let by_class = status_code_totals(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Server5xx),
        )
        .await
        .unwrap();
        assert_eq!(totals(&by_class), vec![(Some(500), 1)], "{by_class:?}");
    }

    #[tokio::test]
    async fn status_code_totals_are_empty_without_rows_in_the_month() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "totals-empty").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_status(Some(200), Some("alpha")),
            "2026-10-05 10:00:00",
        )
        .await;

        assert!(status_code_totals(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None
        )
        .await
        .unwrap()
        .is_empty());
    }

    /// The aggregates take a half-open range: a row of the day the caller
    /// named as the end bound is already outside. The stats page turns the
    /// admin's inclusive `to` day into that bound, so the `to` day itself
    /// stays inside.
    #[tokio::test]
    async fn day_usage_reads_an_arbitrary_half_open_range() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "day-arbitrary").await;

        // The instant before the range, both of its edges, and the first
        // instant outside it.
        for (at, tokens) in [
            ("2026-11-04 23:59:59", 1000),
            ("2026-11-05 00:00:00", 10),
            ("2026-11-07 23:59:59", 20),
            ("2026-11-08 00:00:00", 1000),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row(Some("alpha"), Some(tokens), Some(0), None, None),
                at,
            )
            .await;
        }

        // The days 2026-11-05 through 2026-11-07 inclusive.
        let usage = day_usage(
            &db,
            "2026-11-05 00:00:00",
            "2026-11-08 00:00:00",
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 2, "{usage:?}");
        assert_eq!(usage[0].day, "2026-11-05");
        assert_eq!(usage[0].prompt_tokens, 10);
        assert_eq!(usage[1].day, "2026-11-07");
        assert_eq!(usage[1].prompt_tokens, 20);

        // Bounds that are equal select nothing rather than the whole table.
        assert!(day_usage(
            &db,
            "2026-11-05 00:00:00",
            "2026-11-05 00:00:00",
            None,
            None
        )
        .await
        .unwrap()
        .is_empty());
    }

    #[tokio::test]
    async fn day_status_code_usage_reads_an_arbitrary_half_open_range() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "code-arbitrary").await;

        for (status, at) in [
            (Some(200), "2026-11-04 23:59:59"),
            (Some(200), "2026-11-05 10:00:00"),
            (Some(500), "2026-11-07 10:00:00"),
            (Some(500), "2026-11-08 00:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                at,
            )
            .await;
        }

        let usage = day_status_code_usage(
            &db,
            "2026-11-05 00:00:00",
            "2026-11-08 00:00:00",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            codes(&usage),
            vec![
                ("2026-11-05".to_string(), Some(200), 1),
                ("2026-11-07".to_string(), Some(500), 1),
            ],
            "{usage:?}"
        );
    }

    #[tokio::test]
    async fn status_code_totals_read_an_arbitrary_half_open_range() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "totals-arbitrary").await;

        for (status, at) in [
            (Some(200), "2026-11-04 23:59:59"),
            (Some(200), "2026-11-05 10:00:00"),
            (Some(404), "2026-11-06 10:00:00"),
            (Some(200), "2026-11-07 23:59:59"),
            (Some(200), "2026-11-08 00:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                at,
            )
            .await;
        }

        let rows = status_code_totals(
            &db,
            "2026-11-05 00:00:00",
            "2026-11-08 00:00:00",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            totals(&rows),
            vec![(Some(200), 2), (Some(404), 1)],
            "the rows on both sides of the range stay out: {rows:?}"
        );
    }

    #[tokio::test]
    async fn distinct_models_lists_the_models_of_the_range() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "distinct-range").await;

        for (model, at) in [
            (Some("outside"), "2026-11-04 23:59:59"),
            (Some("inside"), "2026-11-05 10:00:00"),
            (Some("also-inside"), "2026-11-07 10:00:00"),
            (Some("outside"), "2026-11-08 00:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row(model, Some(1), Some(1), None, None),
                at,
            )
            .await;
        }

        let models = distinct_models(&db, "2026-11-05 00:00:00", "2026-11-08 00:00:00")
            .await
            .unwrap();
        assert_eq!(
            models,
            vec!["also-inside".to_string(), "inside".to_string()]
        );
    }

    #[tokio::test]
    async fn hour_usage_groups_by_hour_key_and_model() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "hours-first").await;
        let (second_key, second_channel) = seed(&db, "hours-second").await;

        // Two models of one hour for the first key.
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(7), Some(1), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(3), Some(2), None, None),
            "2026-11-08 10:59:59",
        )
        .await;
        put(
            &db,
            first_key,
            first_channel,
            row(Some("beta"), Some(9), Some(4), None, None),
            "2026-11-08 10:30:00",
        )
        .await;
        // The same model in the next hour is a group of its own.
        put(
            &db,
            first_key,
            first_channel,
            row(Some("alpha"), Some(100), Some(10), None, None),
            "2026-11-08 11:00:00",
        )
        .await;
        // Another key in the same hour is a separate group.
        put(
            &db,
            second_key,
            second_channel,
            row(Some("alpha"), Some(1000), Some(0), None, None),
            "2026-11-08 10:15:00",
        )
        .await;

        let usage = hour_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 4, "{usage:?}");
        // Ordered by hour, then key, then model.
        assert_eq!(usage[0].hour, "2026-11-08 10");
        assert_eq!(usage[0].key_id, first_key);
        assert_eq!(usage[0].model.as_deref(), Some("alpha"));
        assert_eq!(usage[0].prompt_tokens, 10);
        assert_eq!(
            usage[0].requests, 2,
            "both edges of the hour share a bucket"
        );
        assert_eq!(usage[1].model.as_deref(), Some("beta"));
        assert_eq!(usage[1].prompt_tokens, 9);
        assert_eq!(usage[2].hour, "2026-11-08 10");
        assert_eq!(usage[2].key_id, second_key);
        assert_eq!(usage[2].prompt_tokens, 1000);
        assert_eq!(usage[3].hour, "2026-11-08 11");
        assert_eq!(usage[3].key_id, first_key);
        assert_eq!(usage[3].prompt_tokens, 100);
    }

    #[tokio::test]
    async fn hour_usage_buckets_rows_by_their_hour_and_honours_the_range() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-range").await;

        for (at, tokens) in [
            ("2026-11-08 09:59:59", 1000), // the instant before the range
            ("2026-11-08 10:00:00", 10),   // the first instant of the range
            ("2026-11-08 10:59:59", 20),   // the last instant of the same hour
            ("2026-11-08 11:00:00", 40),   // the next hour
            ("2026-11-08 12:59:59", 80),   // the last instant inside the range
            ("2026-11-08 13:00:00", 1000), // the first instant outside it
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row(Some("alpha"), Some(tokens), Some(0), None, None),
                at,
            )
            .await;
        }

        let usage = hour_usage(
            &db,
            "2026-11-08 10:00:00",
            "2026-11-08 13:00:00",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let hours: Vec<&str> = usage.iter().map(|group| group.hour.as_str()).collect();
        assert_eq!(
            hours,
            vec!["2026-11-08 10", "2026-11-08 11", "2026-11-08 12"],
            "{usage:?}"
        );
        assert_eq!(
            usage[0].prompt_tokens, 30,
            "both edges of the hour fall into the same bucket"
        );
        assert_eq!(usage[1].prompt_tokens, 40);
        assert_eq!(usage[2].prompt_tokens, 80);
    }

    #[tokio::test]
    async fn hour_usage_excludes_rows_without_a_key() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-no-key").await;
        put(
            &db,
            key_id,
            channel_id,
            row(Some("alpha"), Some(5), Some(1), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        // A rejection row whose key never resolved belongs to nobody.
        insert_at(
            &db,
            &row(Some("alpha"), Some(1000), Some(1000), None, None),
            "2026-11-08 10:30:00",
        )
        .await
        .unwrap();

        let usage = hour_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].key_id, key_id);
        assert_eq!(usage[0].prompt_tokens, 5);
    }

    #[tokio::test]
    async fn hour_usage_model_and_status_filters_narrow_the_groups() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "hour-filter-first").await;
        let (second_key, second_channel) = seed(&db, "hour-filter-second").await;
        for (key_id, channel_id, status, model) in [
            (first_key, first_channel, Some(200), Some("alpha")),
            (first_key, first_channel, Some(500), Some("alpha")),
            (first_key, first_channel, Some(200), None),
            (second_key, second_channel, Some(200), Some("alpha")),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, model),
                "2026-11-08 10:00:00",
            )
            .await;
        }

        let by_key = hour_usage(
            &db,
            &november().start(),
            &november().end(),
            Some(first_key),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            by_key.len(),
            2,
            "one group per model of the key: {by_key:?}"
        );
        let named = by_key
            .iter()
            .find(|group| group.model.as_deref() == Some("alpha"))
            .expect("the named group");
        assert_eq!(named.requests, 2);
        let unnamed = by_key
            .iter()
            .find(|group| group.model.is_none())
            .expect("the NULL-model group");
        assert_eq!(unnamed.requests, 1);

        // A model filter drops the rows that carry no model at all.
        let by_model = hour_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            Some("alpha".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(by_model.len(), 2, "{by_model:?}");
        assert!(by_model
            .iter()
            .all(|group| group.model.as_deref() == Some("alpha")));

        // The class filter keeps the rows of the class the bucket
        // expression names.
        let by_class = hour_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Ok2xx),
        )
        .await
        .unwrap();
        assert_eq!(
            by_class.len(),
            3,
            "the two 2xx rows and the 500 row: {by_class:?}"
        );
        assert!(by_class.iter().all(|group| group.requests == 1));
    }

    #[tokio::test]
    async fn hour_usage_sums_the_cache_dimensions() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-cache").await;
        // An Anthropic row: 1300 prompt tokens (1000 uncached, 250 read, 50
        // written), 200 completions, 10 of them cached.
        put(
            &db,
            key_id,
            channel_id,
            NewRequestLog {
                cached_output_tokens: Some(10),
                ..row_with_cache_writes(Some("claude"), Some(1300), Some(200), Some(250), Some(50))
            },
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            NewRequestLog {
                cached_output_tokens: Some(4),
                ..row_with_cache_writes(Some("claude"), Some(100), Some(5), None, None)
            },
            "2026-11-08 10:45:00",
        )
        .await;

        let usage = hour_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(usage[0].hour, "2026-11-08 10");
        assert_eq!(usage[0].prompt_tokens, 1400);
        assert_eq!(usage[0].completion_tokens, 205);
        assert_eq!(usage[0].cached_input_tokens, 250);
        assert_eq!(usage[0].cached_output_tokens, 14);
        assert_eq!(
            usage[0].cache_write_input_tokens, 50,
            "the write count is summed per hour like the other dimensions"
        );
        assert_eq!(usage[0].requests, 2);
    }

    /// The hourly per-code hand-over: hour, status and request count.
    fn hour_codes(usage: &[HourStatusCodeUsage]) -> Vec<(String, Option<i64>, i64)> {
        usage
            .iter()
            .map(|row| (row.hour.clone(), row.status, row.requests))
            .collect()
    }

    #[tokio::test]
    async fn hour_status_code_usage_keeps_every_code_of_an_hour_apart() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-code-groups").await;

        for (status, at) in [
            (Some(200), "2026-11-08 10:00:00"),
            (Some(200), "2026-11-08 10:59:59"),
            (Some(204), "2026-11-08 10:30:00"),
            (Some(200), "2026-11-08 11:00:00"),
            (Some(503), "2026-11-08 11:30:00"),
            (Some(503), "2026-11-08 11:59:59"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, Some("alpha")),
                at,
            )
            .await;
        }

        let usage = hour_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            hour_codes(&usage),
            vec![
                ("2026-11-08 10".to_string(), Some(200), 2),
                ("2026-11-08 10".to_string(), Some(204), 1),
                ("2026-11-08 11".to_string(), Some(200), 1),
                ("2026-11-08 11".to_string(), Some(503), 2),
            ],
            "{usage:?}"
        );
        assert_eq!(usage[0].status_class, StatusBucket::Ok2xx);
        assert_eq!(usage[3].status_class, StatusBucket::Server5xx);
    }

    #[tokio::test]
    async fn hour_status_code_usage_follows_the_key_model_and_class_filters() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-code-filters").await;

        for (status, model, at) in [
            (Some(200), Some("alpha"), "10:00:00"),
            (Some(200), None, "10:30:00"),
            (Some(404), Some("alpha"), "10:45:00"),
            (None, Some("alpha"), "11:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(status, model),
                &format!("2026-11-08 {at}"),
            )
            .await;
        }

        // A model filter drops the rows that carry no model at all, and the
        // requests without a status keep a group of their own, ordered last.
        let by_model = hour_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            Some("alpha".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            hour_codes(&by_model),
            vec![
                ("2026-11-08 10".to_string(), Some(200), 1),
                ("2026-11-08 10".to_string(), Some(404), 1),
                ("2026-11-08 11".to_string(), None, 1),
            ],
            "{by_model:?}"
        );

        // The class filter narrows by the bucket each code falls into.
        let by_class = hour_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Client4xx),
        )
        .await
        .unwrap();
        assert_eq!(
            hour_codes(&by_class),
            vec![("2026-11-08 10".to_string(), Some(404), 1)],
            "{by_class:?}"
        );

        let other = hour_status_code_usage(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Other),
        )
        .await
        .unwrap();
        assert_eq!(
            hour_codes(&other),
            vec![("2026-11-08 11".to_string(), None, 1)],
            "the request that never reached an upstream counts as other: {other:?}"
        );
    }

    #[tokio::test]
    async fn hour_status_code_usage_takes_only_rows_inside_the_range() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-code-range").await;

        for at in [
            "2026-11-08 09:59:59",
            "2026-11-08 10:00:00",
            "2026-11-08 12:59:59",
            "2026-11-08 13:00:00",
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_status(Some(200), Some("alpha")),
                at,
            )
            .await;
        }

        let usage = hour_status_code_usage(
            &db,
            "2026-11-08 10:00:00",
            "2026-11-08 13:00:00",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            hour_codes(&usage),
            vec![
                ("2026-11-08 10".to_string(), Some(200), 1),
                ("2026-11-08 12".to_string(), Some(200), 1),
            ],
            "the rows on both sides of the range stay out: {usage:?}"
        );
    }

    #[tokio::test]
    async fn hour_latency_averages_every_timed_row_of_the_hour() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-latency").await;

        for (at, duration) in [
            ("2026-11-08 10:00:00", 20),
            ("2026-11-08 10:59:59", 10),
            ("2026-11-08 11:00:00", 5),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_duration(Some(200), Some("alpha"), duration),
                at,
            )
            .await;
        }
        // A rejection row whose key never resolved is still a timed request,
        // and the latency average keeps it: the user waited for it too.
        insert_at(
            &db,
            &row_with_duration(Some(401), Some("alpha"), 15),
            "2026-11-08 11:30:00",
        )
        .await
        .unwrap();

        let latency = hour_latency(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(latency.len(), 2, "{latency:?}");
        assert_eq!(latency[0].hour, "2026-11-08 10");
        assert!(
            approx(latency[0].avg_duration_ms, 15.0),
            "the two rows of the hour average: {latency:?}"
        );
        assert_eq!(latency[0].requests, 2);
        assert_eq!(latency[1].hour, "2026-11-08 11");
        assert!(
            approx(latency[1].avg_duration_ms, 10.0),
            "the request without a key stays in the average: {latency:?}"
        );
        assert_eq!(latency[1].requests, 2);
    }

    #[tokio::test]
    async fn hour_latency_follows_the_key_model_and_status_filters() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "hour-lat-key-first").await;
        let (second_key, second_channel) = seed(&db, "hour-lat-key-second").await;
        for (key_id, channel_id, status, model, duration, at) in [
            (
                first_key,
                first_channel,
                Some(200),
                Some("alpha"),
                10,
                "10:00:00",
            ),
            (
                first_key,
                first_channel,
                Some(500),
                Some("alpha"),
                30,
                "10:10:00",
            ),
            (first_key, first_channel, Some(200), None, 50, "10:20:00"),
            (
                second_key,
                second_channel,
                Some(200),
                Some("alpha"),
                90,
                "10:30:00",
            ),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_duration(status, model, duration),
                &format!("2026-11-08 {at}"),
            )
            .await;
        }

        let by_key = hour_latency(
            &db,
            &november().start(),
            &november().end(),
            Some(first_key),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(by_key.len(), 1, "{by_key:?}");
        assert!(approx(by_key[0].avg_duration_ms, 30.0), "{by_key:?}");
        assert_eq!(by_key[0].requests, 3);

        // A model filter drops the rows that carry no model.
        let by_model = hour_latency(
            &db,
            &november().start(),
            &november().end(),
            None,
            Some("alpha".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(by_model.len(), 1, "{by_model:?}");
        assert!(
            approx(by_model[0].avg_duration_ms, 130.0 / 3.0),
            "{by_model:?}"
        );
        assert_eq!(by_model[0].requests, 3);

        let by_class = hour_latency(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            Some(StatusBucket::Ok2xx),
        )
        .await
        .unwrap();
        assert_eq!(by_class.len(), 1, "{by_class:?}");
        assert!(approx(by_class[0].avg_duration_ms, 50.0), "{by_class:?}");
        assert_eq!(by_class[0].requests, 3);
    }

    #[tokio::test]
    async fn hour_latency_is_empty_without_rows_in_the_range() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "hour-lat-empty").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_duration(Some(200), Some("alpha"), 10),
            "2026-10-05 10:00:00",
        )
        .await;

        assert!(hour_latency(
            &db,
            &november().start(),
            &november().end(),
            None,
            None,
            None
        )
        .await
        .unwrap()
        .is_empty());
    }

    /// The same row with the captured bodies and the recorded error, for the
    /// free-text search tests.
    fn row_with_bodies(
        model: &str,
        request_body: Option<&str>,
        response_body: Option<&str>,
        error: Option<&str>,
    ) -> NewRequestLog {
        NewRequestLog {
            request_body: request_body.map(str::to_string),
            response_body: response_body.map(str::to_string),
            error: error.map(str::to_string),
            ..row(Some(model), Some(1), Some(1), None, None)
        }
    }

    /// The listing filters with one free-text needle and nothing else.
    fn search_filters(needle: Option<&str>) -> LogFilters {
        LogFilters {
            key_id: None,
            channel_id: None,
            model: None,
            status_class: None,
            from: None,
            to: None,
            search: needle.map(str::to_string),
            page: 1,
            page_size: 50,
        }
    }

    /// The models of the rows the search returns, sorted, so an assertion
    /// names the matching rows without depending on the listing order. The
    /// page and the total the query returns must agree.
    async fn searched_models(db: &Db, needle: &str) -> Vec<String> {
        let (rows, total) = list(db, &search_filters(Some(needle))).await.unwrap();
        assert_eq!(
            total as usize,
            rows.len(),
            "the count and the page must select the same rows"
        );
        let mut models: Vec<String> = rows
            .iter()
            .map(|log| log.model.clone().unwrap_or_default())
            .collect();
        models.sort();
        models
    }

    #[tokio::test]
    async fn search_matches_a_needle_in_any_of_the_three_text_columns() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "search-columns").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("in-request", Some("order-42 confirmed"), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("in-response", None, Some(r#"{"id":"order-42"}"#), None),
            "2026-11-08 11:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("in-error", None, None, Some("upstream refused order-42")),
            "2026-11-08 12:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("elsewhere", Some("nothing of the sort"), None, None),
            "2026-11-08 13:00:00",
        )
        .await;

        assert_eq!(
            searched_models(&db, "order-42").await,
            vec![
                "in-error".to_string(),
                "in-request".to_string(),
                "in-response".to_string()
            ],
            "a row matches when any of the three text columns holds the needle"
        );
    }

    #[tokio::test]
    async fn search_is_case_insensitive() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "search-case").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("alpha", Some("Order-42 confirmed"), None, None),
            "2026-11-08 10:00:00",
        )
        .await;

        for needle in ["order-42", "ORDER-42", "oRdEr-42"] {
            assert_eq!(
                searched_models(&db, needle).await,
                vec!["alpha".to_string()],
                "the needle {needle} must find the stored text"
            );
        }
    }

    #[tokio::test]
    async fn search_returns_nothing_when_no_row_holds_the_needle() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "search-absent").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("alpha", Some("order-42"), Some("done"), Some("none")),
            "2026-11-08 10:00:00",
        )
        .await;

        assert!(searched_models(&db, "order-99").await.is_empty());
    }

    #[tokio::test]
    async fn search_matches_percent_and_underscore_literally() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "search-wildcards").await;
        for (model, body, at) in [
            ("hundred-percent", "100% done", "10:00:00"),
            ("hundred-other", "100x done", "11:00:00"),
            ("underscore", "a_b", "12:00:00"),
            ("any-char", "axb", "13:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_bodies(model, Some(body), None, None),
                &format!("2026-11-08 {at}"),
            )
            .await;
        }

        assert_eq!(
            searched_models(&db, "100%").await,
            vec!["hundred-percent".to_string()],
            "a % in the needle matches a literal percent sign"
        );
        assert_eq!(
            searched_models(&db, "%").await,
            vec!["hundred-percent".to_string()],
            "a lone % is a literal percent sign, not a wildcard over every row"
        );
        assert_eq!(
            searched_models(&db, "a_b").await,
            vec!["underscore".to_string()],
            "a _ in the needle matches a literal underscore, not any character"
        );
    }

    #[tokio::test]
    async fn search_ignores_rows_whose_text_columns_are_null() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "search-nulls").await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("all-null", None, None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            key_id,
            channel_id,
            row_with_bodies("matching", Some("needle-here"), None, None),
            "2026-11-08 11:00:00",
        )
        .await;

        assert_eq!(
            searched_models(&db, "needle-here").await,
            vec!["matching".to_string()],
            "a row whose text columns are all NULL matches nothing"
        );
    }

    #[tokio::test]
    async fn a_listing_without_a_needle_returns_every_row() {
        let db = test_db().await;
        let (key_id, channel_id) = seed(&db, "search-absent-filter").await;
        for (model, at) in [
            ("alpha", "2026-11-08 10:00:00"),
            ("beta", "2026-11-08 11:00:00"),
            ("gamma", "2026-11-08 12:00:00"),
        ] {
            put(
                &db,
                key_id,
                channel_id,
                row_with_bodies(model, Some("body"), None, None),
                at,
            )
            .await;
        }

        let (rows, total) = list(&db, &search_filters(None)).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 3, "an absent needle filters nothing");
    }

    #[tokio::test]
    async fn search_combines_with_the_other_filters() {
        let db = test_db().await;
        let (first_key, first_channel) = seed(&db, "search-and-first").await;
        let (second_key, second_channel) = seed(&db, "search-and-second").await;
        put(
            &db,
            first_key,
            first_channel,
            row_with_bodies("first-match", Some("needle"), None, None),
            "2026-11-08 10:00:00",
        )
        .await;
        put(
            &db,
            first_key,
            first_channel,
            row_with_bodies("first-other", Some("nothing"), None, None),
            "2026-11-08 11:00:00",
        )
        .await;
        put(
            &db,
            second_key,
            second_channel,
            row_with_bodies("second-match", Some("needle"), None, None),
            "2026-11-08 12:00:00",
        )
        .await;

        let mut filters = search_filters(Some("needle"));
        filters.key_id = Some(first_key);
        let (rows, total) = list(&db, &filters).await.unwrap();
        assert_eq!(total, 1, "the key filter still applies: {rows:?}");
        assert_eq!(rows[0].model.as_deref(), Some("first-match"));
    }
}
