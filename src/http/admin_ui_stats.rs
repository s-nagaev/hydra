//! Admin UI stats page: month-to-date token usage and cost per client key,
//! grouped by model, for a month selected in the URL.
//!
//! The page is a reader of the same two building blocks the monthly-limit
//! enforcement uses: the `request_logs` month-to-date aggregate
//! ([`crate::db::request_logs::month_to_date_usage`]) and the pinned cost
//! formula ([`crate::usage::group_cost`], with
//! [`crate::usage::month_cost`] for a key's total). No second aggregate
//! query and no second pricing path exists, so the numbers an admin reads
//! here are the numbers the gate compares against the stored limit.
//!
//! The aggregate is per key, so the page runs one aggregate per listed key.
//! That is deliberate at the current scale (one admin, a handful of keys)
//! and keeps the SQL in exactly one place; it is the price of not writing a
//! second, all-keys aggregate query.
//!
//! Cost comes from the price table of the channel the key is bound to,
//! exactly as enforcement prices it: a model the channel does not price
//! costs zero, which the footnote on the page states. A key carrying a
//! stored monthly limit on a channel that cannot be limited (disabled
//! channel, open whitelist, incomplete price list) shows the same
//! informational badge the keys panel shows.
//!
//! Rendering rules worth naming. A key with no usage in the selected month
//! is omitted entirely, because the table lists usage rather than keys;
//! soft-deleted keys keep their rows, marked, since they spent the tokens.
//! Rows whose upstream reported no usage add no tokens to the sums while
//! still counting as requests, and rows rejected before a key resolved
//! (`api_key_id` NULL) belong to no key and are counted nowhere. The
//! selected month travels in the query string as `YYYY-MM`, so every view is
//! shareable and reloadable without JavaScript, and a malformed value falls
//! back to the current month behind a banner instead of failing the page.
//!
//! There is no mutating route here, so the CSRF guard has nothing to reject;
//! the module is merged into the `/admin/ui` nest like the other read-only
//! pages.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::db::channels::{self, PricedChannel};
use crate::db::keys::{self, ApiKey};
use crate::db::request_logs::{self, ModelUsage};
use crate::db::Db;
use crate::http::admin_auth::AdminAuth;
use crate::http::admin_ui_logs::thousands;
use crate::http::AppState;
use crate::limits;
use crate::month::Month;
use crate::prices::ModelPrices;
use crate::usage;

/// The month query parameter. Kept as a string so a malformed value becomes
/// a banner over the current month instead of a rejected request.
#[derive(Debug, Default, Deserialize)]
pub struct StatsQuery {
    month: Option<String>,
}

/// One table line: either a key's usage of one model, or that key's total
/// across its models. `model_label` is `-` for the group whose log rows
/// carried no model name.
pub struct StatsRow {
    pub key_label: String,
    pub key_deleted: bool,
    pub model_label: String,
    pub is_total: bool,
    /// The key has a stored limit but its channel cannot be limited yet.
    pub limit_inactive: bool,
    pub sums: DisplaySums,
}

/// The integer sums of one block of usage, formatted for the table cells.
pub struct DisplaySums {
    pub prompt_tokens: String,
    pub completion_tokens: String,
    pub cached_input_tokens: String,
    pub cached_output_tokens: String,
    pub requests: String,
    pub cost: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "stats.html")]
pub struct StatsPage {
    pub version: &'static str,
    pub section: &'static str,
    /// The rendered month, `YYYY-MM`.
    pub month_label: String,
    /// The same month as the `month` input value.
    pub month_value: String,
    pub rows: Vec<StatsRow>,
    pub grand: DisplaySums,
    pub prev_url: String,
    pub next_url: String,
    pub current_url: String,
    pub is_current_month: bool,
    pub empty_message: String,
    pub error: Option<String>,
}

/// Running sums of a block of usage, in the stored integer units. Every
/// addition saturates like the cost arithmetic, so no total can wrap.
#[derive(Default, Clone, Copy)]
struct Sums {
    prompt: i64,
    completion: i64,
    cached_input: i64,
    cached_output: i64,
    requests: i64,
    cost: i64,
}

impl Sums {
    /// Adds one aggregate group's token counts and request count.
    fn add_group(&mut self, group: &ModelUsage) {
        self.add_counts(
            group.prompt_tokens,
            group.completion_tokens,
            group.cached_input_tokens,
            group.cached_output_tokens,
            group.requests,
        );
    }

    /// Adds another block of sums; the grand total is the sum of the key
    /// totals.
    fn add_sums(&mut self, other: &Sums) {
        self.add_counts(
            other.prompt,
            other.completion,
            other.cached_input,
            other.cached_output,
            other.requests,
        );
    }

    /// Adds one amount of micro-USD.
    fn add_cost(&mut self, micro: i64) {
        self.cost = self.cost.saturating_add(micro);
    }

    fn add_counts(
        &mut self,
        prompt: i64,
        completion: i64,
        cached_input: i64,
        cached_output: i64,
        requests: i64,
    ) {
        self.prompt = self.prompt.saturating_add(prompt);
        self.completion = self.completion.saturating_add(completion);
        self.cached_input = self.cached_input.saturating_add(cached_input);
        self.cached_output = self.cached_output.saturating_add(cached_output);
        self.requests = self.requests.saturating_add(requests);
    }

    fn display(&self) -> DisplaySums {
        DisplaySums {
            prompt_tokens: thousands(self.prompt),
            completion_tokens: thousands(self.completion),
            cached_input_tokens: thousands(self.cached_input),
            cached_output_tokens: thousands(self.cached_output),
            requests: thousands(self.requests),
            cost: cost_display(self.cost),
        }
    }
}

/// What the page needs from a bound channel: its price table for the cost
/// column and its limitability for the inactive-limit warning.
struct ChannelInfo {
    prices: Option<ModelPrices>,
    limitable: bool,
}

/// One key together with its usage groups for the selected month.
struct KeyUsage {
    key: ApiKey,
    groups: Vec<ModelUsage>,
}

/// Everything the page needs out of the database.
struct StatsData {
    month: Month,
    keys: Vec<KeyUsage>,
    channels: HashMap<i64, ChannelInfo>,
}

fn internal_error(error: &sqlx::Error) -> String {
    tracing::error!(error = %error, "admin ui stats query failed");
    "internal server error".to_string()
}

/// The `YYYY-MM` label of a month, read from the same seven-byte bucket the
/// limit flags key on.
fn month_label(month: &Month) -> String {
    String::from_utf8_lossy(&month.bucket()).to_string()
}

/// The link to one month's view of the page.
fn month_url(month: &Month) -> String {
    format!("/admin/ui/stats?month={}", month_label(month))
}

/// Resolves the `month` query parameter into the month to render: the
/// requested one, or the current month when the parameter is absent or
/// malformed. A malformed value also yields the banner text, so the page
/// still shows usable numbers.
fn resolve_month(query: &StatsQuery) -> (Month, Option<String>) {
    let current = Month::current();
    match query.month.as_deref().map(str::trim) {
        None | Some("") => (current, None),
        Some(raw) => match Month::parse(raw) {
            Some(month) => (month, None),
            None => (
                current,
                Some(format!(
                    "month \"{raw}\" is not a YYYY-MM month; showing {}",
                    month_label(&current)
                )),
            ),
        },
    }
}

/// `$0.004`, or `$0.000001` for an amount below one cent. A cost is never
/// negative (the cost arithmetic clamps and saturates), and the shared
/// formatter renders a negative value as `0.00` anyway.
fn cost_display(micro: i64) -> String {
    format!("${}", limits::format_usd(micro))
}

/// Orders the model lines of one key: named models alphabetically, then the
/// group whose log rows carried no model at all.
fn sort_key(model: &Option<String>) -> (u8, &str) {
    match model {
        Some(name) => (0, name.as_str()),
        None => (1, ""),
    }
}

/// The label of a key: its display prefix plus its optional name, the same
/// shape the keys panel and the log labels use.
fn key_label(key: &ApiKey) -> String {
    match &key.name {
        Some(name) => format!("{} {}", key.display_prefix, name),
        None => key.display_prefix.clone(),
    }
}

/// Reads every bound channel with its price table once, so the per-key cost
/// lookup is a hash map hit.
async fn channel_info(db: &Db) -> sqlx::Result<HashMap<i64, ChannelInfo>> {
    Ok(channels::list_priced(db)
        .await?
        .into_iter()
        .map(|priced: PricedChannel| {
            let limitable = priced.limitable();
            (
                priced.channel.id,
                ChannelInfo {
                    prices: priced.model_prices,
                    limitable,
                },
            )
        })
        .collect())
}

/// Loads the key list, the channel price tables and one month-to-date
/// aggregate per key.
async fn load(state: &AppState, month: &Month) -> Result<StatsData, String> {
    let key_list = keys::list(&state.db)
        .await
        .map_err(|error| internal_error(&error))?;
    let channels = channel_info(&state.db)
        .await
        .map_err(|error| internal_error(&error))?;
    let mut keys = Vec::with_capacity(key_list.len());
    for key in key_list {
        let groups = request_logs::month_to_date_usage(&state.db, key.id, month)
            .await
            .map_err(|error| internal_error(&error))?;
        keys.push(KeyUsage { key, groups });
    }
    Ok(StatsData {
        month: *month,
        keys,
        channels,
    })
}

/// The page with its navigation and empty table in place; the rows and the
/// grand totals are filled in by [`build_page`].
fn page_shell(month: &Month, error: Option<String>) -> StatsPage {
    let label = month_label(month);
    let current = Month::current();
    let empty_message = if error.is_some() {
        String::new()
    } else {
        format!(
            "No requests logged in {label}. Usage appears here as clients call \
             /v1/chat/completions."
        )
    };
    StatsPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "stats",
        month_value: label.clone(),
        month_label: label,
        rows: Vec::new(),
        grand: Sums::default().display(),
        prev_url: month_url(&month.previous()),
        next_url: month_url(&month.next()),
        current_url: month_url(&current),
        is_current_month: *month == current,
        empty_message,
        error,
    }
}

/// Renders the table: the model lines of every key that saw usage in the
/// month, each followed by the key's total line, plus the grand total.
fn build_page(data: StatsData, error: Option<String>) -> StatsPage {
    let mut page = page_shell(&data.month, error);
    let mut grand = Sums::default();
    let mut rows: Vec<StatsRow> = Vec::new();

    for key_usage in &data.keys {
        if key_usage.groups.is_empty() {
            continue;
        }
        let info = data.channels.get(&key_usage.key.channel_id);
        let prices = info.and_then(|info| info.prices.as_ref());
        let limit_inactive =
            key_usage.key.monthly_limit_micro_usd.is_some() && !info.is_some_and(|i| i.limitable);
        let key_label = key_label(&key_usage.key);
        let key_deleted = key_usage.key.deleted_at.is_some();

        let mut order: Vec<&ModelUsage> = key_usage.groups.iter().collect();
        order.sort_by(|left, right| sort_key(&left.model).cmp(&sort_key(&right.model)));

        let mut key_sums = Sums::default();
        for group in order {
            let cost = usage::group_cost(group, prices);
            key_sums.add_group(group);
            rows.push(StatsRow {
                key_label: key_label.clone(),
                key_deleted,
                model_label: group.model.clone().unwrap_or_else(|| "-".to_string()),
                is_total: false,
                limit_inactive: false,
                sums: DisplaySums {
                    prompt_tokens: thousands(group.prompt_tokens),
                    completion_tokens: thousands(group.completion_tokens),
                    cached_input_tokens: thousands(group.cached_input_tokens),
                    cached_output_tokens: thousands(group.cached_output_tokens),
                    requests: thousands(group.requests),
                    cost: cost_display(cost),
                },
            });
        }

        // The key's total is the shared fold of the same per-group costs the
        // rows above show, so the two can never disagree.
        key_sums.add_cost(usage::month_cost(&key_usage.groups, prices));
        rows.push(StatsRow {
            key_label,
            key_deleted,
            model_label: "total".to_string(),
            is_total: true,
            limit_inactive,
            sums: key_sums.display(),
        });

        grand.add_cost(key_sums.cost);
        grand.add_sums(&key_sums);
    }

    page.grand = grand.display();
    page.rows = rows;
    page
}

/// GET /admin/ui/stats: the usage table for one month. Query deserialization
/// cannot fail with a single string field, but the rejection path is kept for
/// robustness.
pub async fn page(
    State(state): State<AppState>,
    _auth: AdminAuth,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> StatsPage {
    let (query, rejection) = match query {
        Ok(Query(query)) => (query, None),
        Err(rejection) => (StatsQuery::default(), Some(rejection.body_text())),
    };
    let (month, month_error) = resolve_month(&query);
    let error = rejection.or(month_error);
    match load(&state, &month).await {
        Ok(data) => build_page(data, error),
        Err(message) => page_shell(&month, Some(message)),
    }
}

/// Read-only route for the `/admin/ui` nest; nothing here mutates, so the
/// CSRF guard layered over the nest has nothing to reject.
pub fn router() -> Router<AppState> {
    Router::new().route("/stats", get(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(model: &str, prompt: i64, completion: i64, requests: i64) -> ModelUsage {
        ModelUsage {
            model: Some(model.to_string()),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_input_tokens: 0,
            cached_output_tokens: 0,
            requests,
        }
    }

    /// A channel priced for `gpt-4o` at 2.50/10.00/1.25/5.00 USD per 1M
    /// tokens, whitelisted, so it is limitable.
    fn channel_info_limitable() -> ChannelInfo {
        let prices = ModelPrices::from_usd_json(&serde_json::json!({
            "gpt-4o": {
                "input": "2.50",
                "output": "10.00",
                "cached_input": "1.25",
                "cached_output": "5.00"
            }
        }))
        .unwrap();
        ChannelInfo {
            prices: Some(prices),
            limitable: true,
        }
    }

    fn stored_key(limit: Option<i64>) -> ApiKey {
        ApiKey {
            id: 1,
            key_hash: "hash".to_string(),
            display_prefix: "hyd-4f9dEg2x".to_string(),
            name: None,
            channel_id: 7,
            monthly_limit_micro_usd: limit,
            enabled: true,
            deleted_at: None,
            created_at: "2026-11-01 00:00:00".to_string(),
        }
    }

    #[test]
    fn a_missing_or_blank_month_resolves_to_the_current_month() {
        let current = Month::current();
        assert_eq!(resolve_month(&StatsQuery::default()).0, current);
        let query = StatsQuery {
            month: Some("   ".to_string()),
        };
        assert_eq!(resolve_month(&query).0, current);
        assert!(resolve_month(&StatsQuery::default()).1.is_none());
    }

    #[test]
    fn a_parseable_month_is_used_verbatim_and_a_bad_one_falls_back() {
        let query = StatsQuery {
            month: Some("2026-01".to_string()),
        };
        let (month, error) = resolve_month(&query);
        assert_eq!(month, Month::from_parts(2026, 1).unwrap());
        assert!(error.is_none());

        let query = StatsQuery {
            month: Some("January".to_string()),
        };
        let (month, error) = resolve_month(&query);
        assert_eq!(month, Month::current());
        let error = error.expect("a malformed month must be reported");
        assert!(error.contains("January"), "{error}");
        assert!(error.contains("YYYY-MM"), "{error}");
    }

    #[test]
    fn month_links_carry_the_adjacent_labels() {
        let january = Month::from_parts(2026, 1).unwrap();
        assert_eq!(month_url(&january), "/admin/ui/stats?month=2026-01");
        assert_eq!(
            month_url(&january.previous()),
            "/admin/ui/stats?month=2025-12"
        );
        assert_eq!(month_url(&january.next()), "/admin/ui/stats?month=2026-02");
    }

    #[test]
    fn model_lines_sort_by_name_with_the_unnamed_group_last() {
        let mut order = [
            ModelUsage {
                model: None,
                ..usage("-", 1, 1, 1)
            },
            usage("gpt-4o", 1, 1, 1),
            usage("claude-3", 1, 1, 1),
        ];
        order.sort_by(|left, right| sort_key(&left.model).cmp(&sort_key(&right.model)));
        let names: Vec<Option<&str>> = order.iter().map(|group| group.model.as_deref()).collect();
        assert_eq!(names, vec![Some("claude-3"), Some("gpt-4o"), None]);
    }

    #[test]
    fn a_page_builds_one_row_per_model_plus_a_key_total() {
        let data = StatsData {
            month: Month::from_parts(2026, 11).unwrap(),
            keys: vec![KeyUsage {
                key: stored_key(None),
                groups: vec![
                    usage("gpt-4o", 1000, 200, 2),
                    usage("gpt-5-mini", 500, 100, 1),
                ],
            }],
            channels: HashMap::from([(7, channel_info_limitable())]),
        };
        let page = build_page(data, None);
        assert_eq!(page.rows.len(), 3, "two models plus the total line");
        assert_eq!(page.section, "stats");
        assert_eq!(page.month_label, "2026-11");
        assert_eq!(page.month_value, "2026-11");
        assert_eq!(
            page.is_current_month,
            Month::from_parts(2026, 11).unwrap() == Month::current()
        );
        assert!(page.prev_url.ends_with("month=2026-10"));
        assert!(page.next_url.ends_with("month=2026-12"));

        // gpt-4o is priced (4500 micro-USD for 1000 prompt and 200
        // completion tokens with no cache report), the other model is not in
        // the channel's table and costs zero; the key total is the priced
        // row.
        let rows = &page.rows;
        assert_eq!(rows[0].model_label, "gpt-4o");
        assert_eq!(rows[0].sums.cost, "$0.0045");
        assert_eq!(rows[1].model_label, "gpt-5-mini");
        assert_eq!(rows[1].sums.cost, "$0.00");
        assert!(!rows[1].is_total);
        let total = &rows[2];
        assert!(total.is_total);
        assert_eq!(total.model_label, "total");
        assert_eq!(total.sums.cost, "$0.0045");
        assert_eq!(total.sums.prompt_tokens, "1,500");
        assert_eq!(total.sums.completion_tokens, "300");
        assert_eq!(total.sums.requests, "3");
        assert_eq!(total.key_label, "hyd-4f9dEg2x");
        assert!(!total.limit_inactive);

        assert_eq!(page.grand.cost, "$0.0045");
        assert_eq!(page.grand.prompt_tokens, "1,500");
        assert_eq!(page.grand.requests, "3");
    }

    #[test]
    fn a_key_without_usage_in_the_month_is_omitted_entirely() {
        let data = StatsData {
            month: Month::from_parts(2026, 11).unwrap(),
            keys: vec![KeyUsage {
                key: stored_key(None),
                groups: Vec::new(),
            }],
            channels: HashMap::from([(7, channel_info_limitable())]),
        };
        let page = build_page(data, None);
        assert!(page.rows.is_empty());
        assert_eq!(page.grand.cost, "$0.00");
        assert!(page.empty_message.contains("2026-11"));
    }

    #[test]
    fn a_limited_key_on_an_unlimitable_channel_gets_the_warning_badge() {
        let mut unlimitable = channel_info_limitable();
        unlimitable.limitable = false;
        let data = StatsData {
            month: Month::from_parts(2026, 11).unwrap(),
            keys: vec![KeyUsage {
                key: stored_key(Some(50_000_000)),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            channels: HashMap::from([(7, unlimitable)]),
        };
        let page = build_page(data, None);
        let total = page.rows.last().unwrap();
        assert!(total.is_total);
        assert!(total.limit_inactive, "the total line carries the warning");

        // Without a stored limit there is nothing to warn about, however
        // unpriced the channel is.
        let mut unlimitable = channel_info_limitable();
        unlimitable.limitable = false;
        let data = StatsData {
            month: Month::from_parts(2026, 11).unwrap(),
            keys: vec![KeyUsage {
                key: stored_key(None),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            channels: HashMap::from([(7, unlimitable)]),
        };
        let page = build_page(data, None);
        assert!(!page.rows.last().unwrap().limit_inactive);
    }

    #[test]
    fn a_key_bound_to_a_channel_that_vanished_costs_nothing_and_warns() {
        let data = StatsData {
            month: Month::from_parts(2026, 11).unwrap(),
            keys: vec![KeyUsage {
                key: stored_key(Some(50_000_000)),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            channels: HashMap::new(),
        };
        let page = build_page(data, None);
        let total = page.rows.last().unwrap();
        assert_eq!(total.sums.cost, "$0.00");
        assert!(total.limit_inactive);
    }

    #[test]
    fn a_deleted_key_keeps_its_rows_and_is_marked() {
        let mut key = stored_key(None);
        key.deleted_at = Some("2026-11-02 00:00:00".to_string());
        let data = StatsData {
            month: Month::from_parts(2026, 11).unwrap(),
            keys: vec![KeyUsage {
                key,
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            channels: HashMap::from([(7, channel_info_limitable())]),
        };
        let page = build_page(data, None);
        assert!(page.rows.iter().all(|row| row.key_deleted));
        assert_eq!(page.rows.len(), 2);
    }

    #[test]
    fn the_failure_shell_keeps_the_navigation_and_drops_the_empty_message() {
        let month = Month::from_parts(2026, 11).unwrap();
        let page = page_shell(&month, Some("internal server error".to_string()));
        assert!(page.rows.is_empty());
        assert_eq!(page.error.as_deref(), Some("internal server error"));
        assert!(page.empty_message.is_empty());
        assert_eq!(page.prev_url, "/admin/ui/stats?month=2026-10");
        assert_eq!(page.next_url, "/admin/ui/stats?month=2026-12");
    }

    #[test]
    fn the_current_month_is_flagged_and_linked() {
        let page = page_shell(&Month::current(), None);
        assert!(page.is_current_month);
        assert_eq!(page.current_url, month_url(&Month::current()));
        assert!(!page.empty_message.is_empty());
    }
}
