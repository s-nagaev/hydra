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

/// Filters for the admin listing; every field is optional. `from` and `to`
/// are inclusive bounds already normalized to the `YYYY-MM-DD HH:MM:SS` UTC
/// text format the `created_at` column stores, so string comparison against
/// the column is correct and can use the time-ordered index.
pub struct LogFilters {
    pub key_id: Option<i64>,
    pub channel_id: Option<i64>,
    pub model: Option<String>,
    pub status_class: Option<StatusClass>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub page: u64,
    pub page_size: u64,
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
}
