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
//! Three filters narrow the view, a key, a model and a status class, read
//! from the same query string and combined with the month. The key and the
//! model apply to the table and to every chart, which is why the table is
//! narrowed by filtering the per-key aggregates the page already loaded
//! rather than by a second query, while the day rows arrive pre-filtered from
//! their own aggregate. The status class applies to the requests chart and to
//! the status code table, which are both request-based: the usage table, the
//! token bars and the cost bars are token-based and keep their meaning under
//! it. The month links carry whatever filters are set, so stepping through
//! months keeps the slice of usage the admin is looking at.
//!
//! Under the table sit three charts of the same filtered month, tokens per
//! day, cost per day and requests per day, drawn server-side by
//! [`crate::http::stats_chart`] as plain SVG and injected with askama's
//! `|safe` filter. The cost of a day is the sum of [`crate::usage::group_cost`]
//! over that day's groups, priced from the key's channel exactly like the
//! table, so the bars and the table add up to the same money. The requests
//! chart stacks the exact response status codes and is left out when the
//! status filter selects no row at all; a view with no day rows renders no
//! chart at all and keeps the table's empty state instead.
//!
//! Under the charts sits the status code table of the same filters: one line
//! per exact code of the period with its class, its request count and its
//! share of the requests the filters select, the busiest code first. Like the
//! requests chart it is left out entirely when the filters select no request.
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
use crate::db::request_logs::{
    self, DayStatusCodeUsage, DayUsage, ModelUsage, StatusBucket, StatusCodeTotal,
};
use crate::db::Db;
use crate::http::admin_ui_logs::{thousands, SelectOption};
use crate::http::stats_chart;
use crate::http::ui_auth::UiAuth;
use crate::http::AppState;
use crate::limits;
use crate::month::Month;
use crate::prices::ModelPrices;
use crate::usage;

/// The view state of the page: the month and the three optional filters.
/// Kept as strings so a malformed or empty value becomes a banner over an
/// unfiltered view instead of a rejected request.
#[derive(Debug, Default, Deserialize)]
pub struct StatsQuery {
    month: Option<String>,
    key_id: Option<String>,
    model: Option<String>,
    status_class: Option<String>,
}

/// The key, model and status class filters of one view, after parsing. Every
/// one is absent when its parameter is missing or blank; the key is also
/// absent when the value is not a row id and the class when it names no
/// bucket.
#[derive(Debug, Default, PartialEq, Eq)]
struct Filters {
    key_id: Option<i64>,
    /// The trimmed model name; `None` when the parameter is absent or blank.
    model: Option<String>,
    /// The status class the requests chart and the status code table are
    /// narrowed to; `None` when the parameter is absent or blank.
    status_class: Option<StatusBucket>,
}

impl Filters {
    /// Whether the view narrows anything at all, which decides between the
    /// "no requests logged" and the "no usage matches the filters" empty
    /// text.
    fn is_set(&self) -> bool {
        self.key_id.is_some() || self.model.is_some() || self.status_class.is_some()
    }
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
    pub cache_write_input_tokens: String,
    pub requests: String,
    pub cost: String,
}

/// The charts of the view, already rendered as SVG. Present only when the
/// filtered month has rows to plot.
pub struct Charts {
    /// Tokens per day, input and output stacked.
    pub tokens_svg: String,
    /// Cost per day, priced from each key's channel.
    pub cost_svg: String,
    /// Requests per day, one segment per exact status code. Absent when the
    /// status filter selects no row, because an empty plot says less than
    /// none.
    pub status_svg: Option<String>,
}

/// One line of the status code table: one exact code of the filtered period
/// with its class, its request count and its share of the period.
#[derive(Debug)]
pub struct StatusCodeRow {
    /// The exact code, or `n/a` for the requests that never reached an
    /// upstream and so carry none.
    pub code: String,
    /// The class the code falls into, named as the status filter names it.
    pub class: String,
    pub requests: String,
    /// The share of the period's requests this code carries, as a
    /// percentage.
    pub share: String,
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
    /// The Key select: `All keys` plus every stored key, soft-deleted ones
    /// included.
    pub key_options: Vec<SelectOption>,
    /// The Model select: `All models` plus every model logged in the month.
    pub model_options: Vec<SelectOption>,
    /// The Status select: `All statuses` plus the four classes the requests
    /// chart stacks.
    pub status_options: Vec<SelectOption>,
    /// The status code table of the period, busiest code first; empty when
    /// the filters select no request, which is when the template leaves the
    /// whole panel out.
    pub status_codes: Vec<StatusCodeRow>,
    /// Absent when the filtered month has nothing to plot.
    pub charts: Option<Charts>,
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
    cache_write_input: i64,
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
            group.cache_write_input_tokens,
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
            other.cache_write_input,
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
        cache_write_input: i64,
        requests: i64,
    ) {
        self.prompt = self.prompt.saturating_add(prompt);
        self.completion = self.completion.saturating_add(completion);
        self.cached_input = self.cached_input.saturating_add(cached_input);
        self.cached_output = self.cached_output.saturating_add(cached_output);
        self.cache_write_input = self.cache_write_input.saturating_add(cache_write_input);
        self.requests = self.requests.saturating_add(requests);
    }

    fn display(&self) -> DisplaySums {
        DisplaySums {
            prompt_tokens: thousands(self.prompt),
            completion_tokens: thousands(self.completion),
            cached_input_tokens: thousands(self.cached_input),
            cached_output_tokens: thousands(self.cached_output),
            cache_write_input_tokens: thousands(self.cache_write_input),
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
    /// Day-level rows of the filtered month, for the token and cost charts.
    days: Vec<DayUsage>,
    /// Day-level request counts per exact status code of the filtered month,
    /// for the requests chart.
    status_code_days: Vec<DayStatusCodeUsage>,
    /// Request totals per exact status code of the filtered month, for the
    /// status code table.
    status_code_totals: Vec<StatusCodeTotal>,
    /// The distinct models logged in the month, for the Model select.
    models: Vec<String>,
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

/// The link to one view of the page: the month always travels, a filter only
/// when it carries a value, so a shared URL stays as short as it can.
fn view_url(month: &Month, filters: &Filters) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("month", &month_label(month));
    if let Some(key_id) = filters.key_id {
        serializer.append_pair("key_id", &key_id.to_string());
    }
    if let Some(model) = &filters.model {
        serializer.append_pair("model", model);
    }
    if let Some(status_class) = filters.status_class {
        serializer.append_pair("status_class", status_class.label());
    }
    format!("/admin/ui/stats?{}", serializer.finish())
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

/// Resolves the `key_id`, `model` and `status_class` query parameters into
/// the view's filters. A blank value means "no filter", an unparsable key id
/// or status class yields the banner text and no filter (the same lenient
/// reading the month parameter gets), and the model name is trimmed so a
/// stray space cannot hide every row.
fn resolve_filters(query: &StatsQuery) -> (Filters, Option<String>) {
    let model = query
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let raw_key = query
        .key_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (key_id, key_error) = match raw_key {
        None => (None, None),
        Some(value) => match value.parse::<i64>() {
            Ok(id) => (Some(id), None),
            Err(_) => (
                None,
                Some(format!(
                    "key_id \"{value}\" is not a row id; showing every key"
                )),
            ),
        },
    };
    let raw_class = query
        .status_class
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (status_class, class_error) = match raw_class {
        None => (None, None),
        Some(value) => match StatusBucket::parse(value) {
            Some(class) => (Some(class), None),
            None => (
                None,
                Some(format!(
                    "status_class \"{value}\" is not one of {}; showing every status",
                    class_values()
                )),
            ),
        },
    };
    let filters = Filters {
        key_id,
        model,
        status_class,
    };
    (filters, key_error.or(class_error))
}

/// The accepted `status_class` values, in the order the select offers them,
/// so the banner and the control cannot list different vocabularies.
fn class_values() -> String {
    StatusBucket::ALL
        .iter()
        .map(|class| class.label())
        .collect::<Vec<_>>()
        .join(", ")
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

/// Loads the key list, the channel price tables, one month-to-date aggregate
/// per key, the day rows the token and cost charts plot, the day rows per
/// exact status code, the status code totals of the period and the model
/// names of the month.
///
/// The month aggregate is per key and is narrowed in memory by [`build_page`];
/// the day rows are narrowed by their own queries, which already carry the
/// key and model predicates, and the status queries carry the status class
/// filter on top. The model list of the select stays month-scoped and
/// unfiltered on purpose, so the control always offers every model of the
/// month rather than only the surviving one.
async fn load(state: &AppState, month: &Month, filters: &Filters) -> Result<StatsData, String> {
    let key_list = keys::list(&state.db)
        .await
        .map_err(|error| internal_error(&error))?;
    let channels = channel_info(&state.db)
        .await
        .map_err(|error| internal_error(&error))?;
    let days = request_logs::day_usage(
        &state.db,
        &month.start(),
        &month.end(),
        filters.key_id,
        filters.model.clone(),
    )
    .await
    .map_err(|error| internal_error(&error))?;
    let status_days = request_logs::day_status_code_usage(
        &state.db,
        &month.start(),
        &month.end(),
        filters.key_id,
        filters.model.clone(),
        filters.status_class,
    )
    .await
    .map_err(|error| internal_error(&error))?;
    let status_code_totals = request_logs::status_code_totals(
        &state.db,
        &month.start(),
        &month.end(),
        filters.key_id,
        filters.model.clone(),
        filters.status_class,
    )
    .await
    .map_err(|error| internal_error(&error))?;
    let models = request_logs::distinct_models(&state.db, &month.start(), &month.end())
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
        days,
        status_code_days: status_days,
        status_code_totals,
        models,
    })
}

/// The status code table of one view: one line per exact code the filters
/// select, most requests first (the order the SQL aggregate returns), each
/// with its class and its share of the period's requests.
///
/// The total the shares divide by is the sum of the same rows, so the column
/// always adds up to a hundred percent of what the current filters select,
/// whatever they are.
fn status_code_rows(totals: &[StatusCodeTotal]) -> Vec<StatusCodeRow> {
    let total = totals
        .iter()
        .fold(0_i64, |sum, row| sum.saturating_add(row.requests));
    totals
        .iter()
        .map(|row| StatusCodeRow {
            code: match row.status {
                Some(code) => code.to_string(),
                None => "n/a".to_string(),
            },
            class: row.status_class.label().to_string(),
            requests: thousands(row.requests),
            share: share_label(row.requests, total),
        })
        .collect()
}

/// One code's share of the requests a view selected, as a percentage with one
/// decimal. A table without a single request (which the template does not
/// render) reads `0.0%` rather than dividing by zero.
fn share_label(requests: i64, total: i64) -> String {
    if total <= 0 {
        return "0.0%".to_string();
    }
    format!("{:.1}%", requests as f64 * 100.0 / total as f64)
}

/// The Key select: `All keys` plus every key the page loaded, soft-deleted
/// ones included (they hold usage rows), labeled like the keys panel. A
/// selected id the list does not carry leaves no option selected, and the
/// browser then shows the first one, `All keys`; the alternative, a
/// hand-typed id no key ever had, filters the view to nothing anyway.
fn key_options(keys: &[KeyUsage], selected: Option<i64>) -> Vec<SelectOption> {
    let mut options = vec![SelectOption {
        value: String::new(),
        label: "All keys".to_string(),
        selected: selected.is_none(),
    }];
    options.extend(keys.iter().map(|usage| SelectOption {
        value: usage.key.id.to_string(),
        label: key_label(&usage.key),
        selected: Some(usage.key.id) == selected,
    }));
    options
}

/// The Model select: `All models` plus every model logged in the month,
/// alphabetically. A selected model the month has no rows for is added to
/// the list, so switching months behind a model filter never leaves the
/// control claiming "All models" while the table is filtered to one.
fn model_options(models: &[String], selected: Option<&str>) -> Vec<SelectOption> {
    let mut names = models.to_vec();
    if let Some(selected) = selected {
        if !names.iter().any(|name| name == selected) {
            names.push(selected.to_string());
        }
    }
    names.sort();
    names.dedup();
    let mut options = vec![SelectOption {
        value: String::new(),
        label: "All models".to_string(),
        selected: selected.is_none(),
    }];
    options.extend(names.into_iter().map(|name| SelectOption {
        value: name.clone(),
        label: name.clone(),
        selected: Some(name.as_str()) == selected,
    }));
    options
}

/// The Status select: `All statuses` plus the four classes the requests
/// chart stacks, in the order they stack. The wording repeats the log
/// listing's, so one class reads the same way on both pages.
fn status_options(selected: Option<StatusBucket>) -> Vec<SelectOption> {
    let mut options = vec![SelectOption {
        value: String::new(),
        label: "All statuses".to_string(),
        selected: selected.is_none(),
    }];
    options.extend(StatusBucket::ALL.iter().map(|class| SelectOption {
        value: class.label().to_string(),
        label: class_label(*class).to_string(),
        selected: selected == Some(*class),
    }));
    options
}

/// The wording of one status class in the select, which explains what the
/// class covers rather than repeating its label.
fn class_label(class: StatusBucket) -> &'static str {
    match class {
        StatusBucket::Ok2xx => "2xx success",
        StatusBucket::Client4xx => "4xx client error",
        StatusBucket::Server5xx => "5xx server error",
        StatusBucket::Other => "Other (1xx, 3xx, failures)",
    }
}

/// The page with its navigation, filter widgets and empty table in place;
/// the rows, the grand totals and the charts are filled in by [`build_page`].
fn page_shell(month: &Month, filters: &Filters, error: Option<String>) -> StatsPage {
    let label = month_label(month);
    let current = Month::current();
    let empty_message = if error.is_some() {
        String::new()
    } else if filters.is_set() {
        format!("No usage matches the current filters in {label}.")
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
        key_options: key_options(&[], filters.key_id),
        model_options: model_options(&[], filters.model.as_deref()),
        status_options: status_options(filters.status_class),
        status_codes: Vec::new(),
        charts: None,
        prev_url: view_url(&month.previous(), filters),
        next_url: view_url(&month.next(), filters),
        current_url: view_url(&current, filters),
        is_current_month: *month == current,
        empty_message,
        error,
    }
}

/// One day group as the shape the pricing fold reads, so a day is priced by
/// the same [`usage::group_cost`] call the table's month groups go through.
fn day_group(day: &DayUsage) -> ModelUsage {
    ModelUsage {
        model: day.model.clone(),
        prompt_tokens: day.prompt_tokens,
        completion_tokens: day.completion_tokens,
        cached_input_tokens: day.cached_input_tokens,
        cached_output_tokens: day.cached_output_tokens,
        cache_write_input_tokens: day.cache_write_input_tokens,
        requests: day.requests,
    }
}

/// The charts of one view: the daily token bars, the daily cost bars built by
/// pricing every day group from the price table of the key that spent it, and
/// the daily requests bars stacked by exact status code. A key whose channel
/// is missing or prices nothing contributes zero, exactly as it does in the
/// table. `None` when the filtered month has no day rows at all, which is
/// when the page shows its empty state instead of an empty plot; the requests
/// chart is dropped on its own when the status filter leaves it no row.
fn charts(data: &StatsData) -> Option<Charts> {
    if data.days.is_empty() {
        return None;
    }
    // A day group names the key, not the channel, so the key's bound channel
    // is where its prices come from.
    let channels: HashMap<i64, &ChannelInfo> = data
        .keys
        .iter()
        .filter_map(|usage| {
            data.channels
                .get(&usage.key.channel_id)
                .map(|info| (usage.key.id, info))
        })
        .collect();
    let costs: Vec<(String, i64)> = data
        .days
        .iter()
        .map(|day| {
            let prices = channels
                .get(&day.key_id)
                .and_then(|info| info.prices.as_ref());
            (day.day.clone(), usage::group_cost(&day_group(day), prices))
        })
        .collect();
    let status_svg = (!data.status_code_days.is_empty())
        .then(|| stats_chart::daily_status_code_bars(&data.status_code_days));
    Some(Charts {
        tokens_svg: stats_chart::daily_token_bars(&data.days),
        cost_svg: stats_chart::daily_cost_bars(&costs),
        status_svg,
    })
}

/// Renders the table: the model lines of every key that saw usage in the
/// month, each followed by the key's total line, plus the grand total.
///
/// The filters narrow this in memory. A key filter drops every other key's
/// blocks, a model filter drops the groups of other models, and a block left
/// with no group disappears whole, so a key with no usage under the current
/// filters is omitted exactly like a key with no usage in the month.
fn build_page(data: StatsData, filters: &Filters, error: Option<String>) -> StatsPage {
    let mut page = page_shell(&data.month, filters, error);
    let mut grand = Sums::default();
    let mut rows: Vec<StatsRow> = Vec::new();

    for key_usage in &data.keys {
        if filters
            .key_id
            .is_some_and(|key_id| key_id != key_usage.key.id)
        {
            continue;
        }
        let mut order: Vec<&ModelUsage> = key_usage
            .groups
            .iter()
            .filter(|group| match filters.model.as_deref() {
                Some(model) => group.model.as_deref() == Some(model),
                None => true,
            })
            .collect();
        if order.is_empty() {
            continue;
        }
        let info = data.channels.get(&key_usage.key.channel_id);
        let prices = info.and_then(|info| info.prices.as_ref());
        let limit_inactive =
            key_usage.key.monthly_limit_micro_usd.is_some() && !info.is_some_and(|i| i.limitable);
        let key_label = key_label(&key_usage.key);
        let key_deleted = key_usage.key.deleted_at.is_some();

        order.sort_by(|left, right| sort_key(&left.model).cmp(&sort_key(&right.model)));

        // The key's total is the shared fold of the same per-group costs the
        // rows below show, so the two can never disagree; only the surviving
        // groups of the filter take part in it.
        let mut key_sums = Sums::default();
        for group in order {
            let cost = usage::group_cost(group, prices);
            key_sums.add_group(group);
            key_sums.add_cost(cost);
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
                    cache_write_input_tokens: thousands(group.cache_write_input_tokens),
                    requests: thousands(group.requests),
                    cost: cost_display(cost),
                },
            });
        }

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

    page.key_options = key_options(&data.keys, filters.key_id);
    page.model_options = model_options(&data.models, filters.model.as_deref());
    page.status_options = status_options(filters.status_class);
    page.status_codes = status_code_rows(&data.status_code_totals);
    page.charts = charts(&data);
    page.grand = grand.display();
    page.rows = rows;
    page
}

/// GET /admin/ui/stats: the usage table and the charts for one month, under
/// the key and model filters of the URL. Query deserialization cannot fail
/// with string fields, but the rejection path is kept for robustness.
pub async fn page(
    State(state): State<AppState>,
    _auth: UiAuth,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> StatsPage {
    let (query, rejection) = match query {
        Ok(Query(query)) => (query, None),
        Err(rejection) => (StatsQuery::default(), Some(rejection.body_text())),
    };
    let (month, month_error) = resolve_month(&query);
    let (filters, filter_error) = resolve_filters(&query);
    let error = rejection.or(month_error).or(filter_error);
    match load(&state, &month, &filters).await {
        Ok(data) => build_page(data, &filters, error),
        Err(message) => page_shell(&month, &filters, Some(message)),
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
            cache_write_input_tokens: 0,
            requests,
        }
    }

    /// One day-level row as the day aggregate hands it over. The cache
    /// dimensions stay at zero unless a test sets them.
    fn day_row(day: &str, model: &str, prompt: i64, completion: i64) -> DayUsage {
        DayUsage {
            day: day.to_string(),
            key_id: 1,
            model: Some(model.to_string()),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_input_tokens: 0,
            cached_output_tokens: 0,
            cache_write_input_tokens: 0,
            requests: 1,
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

    /// The same channel with the cache-write dimension priced too.
    fn channel_info_write_priced() -> ChannelInfo {
        let prices = ModelPrices::from_usd_json(&serde_json::json!({
            "claude": {
                "input": "2.50",
                "output": "10.00",
                "cached_input": "1.25",
                "cached_output": "5.00",
                "cache_write_input": "3.125"
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

    /// Loaded data for one month, without day rows or model names: the table
    /// tests do not need the charts.
    fn stats_data(
        month: Month,
        keys: Vec<KeyUsage>,
        channels: HashMap<i64, ChannelInfo>,
    ) -> StatsData {
        StatsData {
            month,
            keys,
            channels,
            days: Vec::new(),
            status_code_days: Vec::new(),
            status_code_totals: Vec::new(),
            models: Vec::new(),
        }
    }

    fn november() -> Month {
        Month::from_parts(2026, 11).unwrap()
    }

    #[test]
    fn a_missing_or_blank_month_resolves_to_the_current_month() {
        let current = Month::current();
        assert_eq!(resolve_month(&StatsQuery::default()).0, current);
        let query = StatsQuery {
            month: Some("   ".to_string()),
            ..StatsQuery::default()
        };
        assert_eq!(resolve_month(&query).0, current);
        assert!(resolve_month(&StatsQuery::default()).1.is_none());
    }

    #[test]
    fn a_parseable_month_is_used_verbatim_and_a_bad_one_falls_back() {
        let query = StatsQuery {
            month: Some("2026-01".to_string()),
            ..StatsQuery::default()
        };
        let (month, error) = resolve_month(&query);
        assert_eq!(month, Month::from_parts(2026, 1).unwrap());
        assert!(error.is_none());

        let query = StatsQuery {
            month: Some("January".to_string()),
            ..StatsQuery::default()
        };
        let (month, error) = resolve_month(&query);
        assert_eq!(month, Month::current());
        let error = error.expect("a malformed month must be reported");
        assert!(error.contains("January"), "{error}");
        assert!(error.contains("YYYY-MM"), "{error}");
    }

    #[test]
    fn view_links_carry_the_month_and_only_the_filters_that_are_set() {
        let january = Month::from_parts(2026, 1).unwrap();
        assert_eq!(
            view_url(&january, &Filters::default()),
            "/admin/ui/stats?month=2026-01"
        );
        assert_eq!(
            view_url(&january.previous(), &Filters::default()),
            "/admin/ui/stats?month=2025-12"
        );
        assert_eq!(
            view_url(&january.next(), &Filters::default()),
            "/admin/ui/stats?month=2026-02"
        );

        let both = Filters {
            key_id: Some(3),
            model: Some("gpt-4o".to_string()),
            ..Filters::default()
        };
        assert_eq!(
            view_url(&january, &both),
            "/admin/ui/stats?month=2026-01&key_id=3&model=gpt-4o"
        );
        // Whitespace inside a model name survives as a URL-encoded value
        // rather than splitting the query string.
        let spaced = Filters {
            model: Some("claude 3.5".to_string()),
            ..Filters::default()
        };
        assert_eq!(
            view_url(&january, &spaced),
            "/admin/ui/stats?month=2026-01&model=claude+3.5"
        );
        // The status class travels by its label, the same string the SQL
        // bucket expression emits.
        let classed = Filters {
            status_class: Some(StatusBucket::Client4xx),
            ..Filters::default()
        };
        assert_eq!(
            view_url(&january, &classed),
            "/admin/ui/stats?month=2026-01&status_class=4xx"
        );
    }

    #[test]
    fn filters_read_blank_values_as_no_filter_and_report_a_bad_key_id() {
        let (filters, error) = resolve_filters(&StatsQuery::default());
        assert_eq!(filters, Filters::default());
        assert!(error.is_none());

        let query = StatsQuery {
            key_id: Some("  ".to_string()),
            model: Some("   ".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query);
        assert_eq!(filters, Filters::default());
        assert!(!filters.is_set());
        assert!(error.is_none());

        let query = StatsQuery {
            key_id: Some(" 7 ".to_string()),
            model: Some(" gpt-4o ".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query);
        assert_eq!(filters.key_id, Some(7));
        assert_eq!(filters.model.as_deref(), Some("gpt-4o"));
        assert!(filters.is_set());
        assert!(error.is_none());

        let query = StatsQuery {
            key_id: Some("abc".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query);
        assert_eq!(filters.key_id, None, "an unparsable id filters nothing");
        let error = error.expect("the offending value is reported");
        assert!(error.contains("abc"), "{error}");
        assert!(error.contains("row id"), "{error}");
    }

    #[test]
    fn the_status_class_filter_parses_its_label_and_reports_an_unknown_one() {
        let query = StatsQuery {
            status_class: Some("  5xx  ".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query);
        assert_eq!(filters.status_class, Some(StatusBucket::Server5xx));
        assert!(filters.is_set());
        assert!(error.is_none());

        let query = StatsQuery {
            status_class: Some("errors".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query);
        assert_eq!(
            filters.status_class, None,
            "the log listing's errors bucket is not a class of the chart"
        );
        assert!(!filters.is_set());
        let error = error.expect("the offending value is reported");
        assert!(error.contains("errors"), "{error}");
        assert!(error.contains("2xx, 4xx, 5xx, other"), "{error}");

        let query = StatsQuery {
            status_class: Some("   ".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query);
        assert_eq!(filters, Filters::default());
        assert!(error.is_none());
    }

    #[test]
    fn the_status_select_offers_every_class_in_stacking_order() {
        let options = status_options(None);
        let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "All statuses",
                "2xx success",
                "4xx client error",
                "5xx server error",
                "Other (1xx, 3xx, failures)",
            ]
        );
        let values: Vec<&str> = options.iter().map(|option| option.value.as_str()).collect();
        assert_eq!(values, vec!["", "2xx", "4xx", "5xx", "other"]);
        assert!(options[0].selected);

        let options = status_options(Some(StatusBucket::Other));
        assert_eq!(
            options.iter().filter(|option| option.selected).count(),
            1,
            "exactly one option is selected"
        );
        assert!(options[4].selected);
        assert!(!options[0].selected);
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
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: vec![
                    usage("gpt-4o", 1000, 200, 2),
                    usage("gpt-5-mini", 500, 100, 1),
                ],
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_page(data, &Filters::default(), None);
        assert_eq!(page.rows.len(), 3, "two models plus the total line");
        assert_eq!(page.section, "stats");
        assert_eq!(page.month_label, "2026-11");
        assert_eq!(page.month_value, "2026-11");
        assert_eq!(
            page.is_current_month,
            Month::from_parts(2026, 11).unwrap() == Month::current(),
            "the flag follows the month the page renders"
        );
        assert!(page.prev_url.ends_with("month=2026-10"));
        assert!(page.next_url.ends_with("month=2026-12"));
        assert!(
            page.charts.is_none(),
            "a month with no day rows draws no chart"
        );

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
        assert_eq!(
            total.sums.cache_write_input_tokens, "0",
            "a request that writes no cache shows the column at zero"
        );
        assert_eq!(total.sums.requests, "3");
        assert_eq!(total.key_label, "hyd-4f9dEg2x");
        assert!(!total.limit_inactive);

        assert_eq!(page.grand.cost, "$0.0045");
        assert_eq!(page.grand.prompt_tokens, "1,500");
        assert_eq!(page.grand.cache_write_input_tokens, "0");
        assert_eq!(page.grand.requests, "3");
    }

    #[test]
    fn a_cache_write_group_shows_its_token_column_and_is_priced_at_the_write_rate() {
        // An Anthropic row: 1300 prompt tokens (1000 uncached, 250 read, 50
        // written) and 200 completions. At 2.50/1.25/3.125/10.00 USD per 1M
        // tokens the pinned formula charges 1000 * 2.50 + 250 * 1.25 +
        // 50 * 3.125 + 200 * 10.00 = 4968.75 micro-USD, floored to 4968.
        let mut group = usage("claude", 1300, 200, 1);
        group.cached_input_tokens = 250;
        group.cache_write_input_tokens = 50;
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: vec![group],
            }],
            HashMap::from([(7, channel_info_write_priced())]),
        );
        let page = build_page(data, &Filters::default(), None);
        let row = &page.rows[0];
        assert_eq!(row.model_label, "claude");
        assert_eq!(row.sums.cache_write_input_tokens, "50");
        assert_eq!(row.sums.cached_input_tokens, "250");
        assert_eq!(row.sums.prompt_tokens, "1,300");
        assert_eq!(row.sums.cost, "$0.004968");

        // The key total and the grand total carry the same figure and the
        // same token column.
        let total = &page.rows[1];
        assert!(total.is_total);
        assert_eq!(total.sums.cache_write_input_tokens, "50");
        assert_eq!(total.sums.cost, "$0.004968");
        assert_eq!(page.grand.cache_write_input_tokens, "50");
        assert_eq!(page.grand.cost, "$0.004968");
    }

    #[test]
    fn a_key_without_usage_in_the_month_is_omitted_entirely() {
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: Vec::new(),
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_page(data, &Filters::default(), None);
        assert!(page.rows.is_empty());
        assert_eq!(page.grand.cost, "$0.00");
        assert!(page.empty_message.contains("2026-11"));
        assert!(
            !page.empty_message.contains("filters"),
            "an unfiltered empty month keeps the plain wording"
        );
    }

    #[test]
    fn a_limited_key_on_an_unlimitable_channel_gets_the_warning_badge() {
        let mut unlimitable = channel_info_limitable();
        unlimitable.limitable = false;
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(Some(50_000_000)),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            HashMap::from([(7, unlimitable)]),
        );
        let page = build_page(data, &Filters::default(), None);
        let total = page.rows.last().unwrap();
        assert!(total.is_total);
        assert!(total.limit_inactive, "the total line carries the warning");

        // Without a stored limit there is nothing to warn about, however
        // unpriced the channel is.
        let mut unlimitable = channel_info_limitable();
        unlimitable.limitable = false;
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            HashMap::from([(7, unlimitable)]),
        );
        let page = build_page(data, &Filters::default(), None);
        assert!(!page.rows.last().unwrap().limit_inactive);
    }

    #[test]
    fn a_key_bound_to_a_channel_that_vanished_costs_nothing_and_warns() {
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(Some(50_000_000)),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            HashMap::new(),
        );
        let page = build_page(data, &Filters::default(), None);
        let total = page.rows.last().unwrap();
        assert_eq!(total.sums.cost, "$0.00");
        assert!(total.limit_inactive);
    }

    #[test]
    fn a_deleted_key_keeps_its_rows_and_is_marked() {
        let mut key = stored_key(None);
        key.deleted_at = Some("2026-11-02 00:00:00".to_string());
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key,
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_page(data, &Filters::default(), None);
        assert!(page.rows.iter().all(|row| row.key_deleted));
        assert_eq!(page.rows.len(), 2);
    }

    #[test]
    fn a_key_filter_keeps_only_that_keys_blocks_and_totals() {
        let mut second = stored_key(None);
        second.id = 2;
        second.display_prefix = "hyd-2b2b2b2b".to_string();
        let keys = || {
            vec![
                KeyUsage {
                    key: stored_key(None),
                    groups: vec![usage("gpt-4o", 1000, 200, 1)],
                },
                KeyUsage {
                    key: second.clone(),
                    groups: vec![usage("gpt-4o", 10, 1, 1)],
                },
            ]
        };
        let data = stats_data(
            november(),
            keys(),
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_page(
            data,
            &Filters {
                key_id: Some(2),
                ..Filters::default()
            },
            None,
        );
        assert_eq!(page.rows.len(), 2, "one model line and one total");
        assert_eq!(page.rows[0].key_label, "hyd-2b2b2b2b");
        assert_eq!(page.rows[1].key_label, "hyd-2b2b2b2b");
        assert_eq!(
            page.grand.prompt_tokens, "10",
            "the totals follow the filter"
        );
        assert_eq!(
            page.grand.cost, "$0.000035",
            "10 prompt tokens at 2.50 and 1 completion at 10.00 per 1M"
        );

        // A key id no row carries selects nothing, and the empty state says
        // the filters are the reason.
        let data = stats_data(
            november(),
            keys(),
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_page(
            data,
            &Filters {
                key_id: Some(9),
                ..Filters::default()
            },
            None,
        );
        assert!(page.rows.is_empty());
        assert_eq!(page.grand.cost, "$0.00");
        assert!(page
            .empty_message
            .contains("No usage matches the current filters"));
    }

    #[test]
    fn a_model_filter_keeps_one_model_line_per_key() {
        let groups = || {
            vec![
                usage("gpt-4o", 1000, 200, 1),
                usage("gpt-5-mini", 100, 50, 1),
            ]
        };
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: groups(),
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_page(
            data,
            &Filters {
                model: Some("gpt-5-mini".to_string()),
                ..Filters::default()
            },
            None,
        );
        assert_eq!(
            page.rows.len(),
            2,
            "the surviving model line plus the key total"
        );
        assert_eq!(page.rows[0].model_label, "gpt-5-mini");
        assert_eq!(
            page.rows[0].sums.cost, "$0.00",
            "an unpriced model stays zero"
        );
        assert_eq!(
            page.rows[1].sums.prompt_tokens, "100",
            "the total is filtered"
        );
        assert_eq!(page.grand.prompt_tokens, "100");

        // A model nothing logged empties the table and changes the wording.
        let data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: groups(),
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_page(
            data,
            &Filters {
                model: Some("nope".to_string()),
                ..Filters::default()
            },
            None,
        );
        assert!(page.rows.is_empty());
        assert!(page
            .empty_message
            .contains("No usage matches the current filters"));
    }

    #[test]
    fn the_key_select_lists_every_key_of_the_page_with_the_selected_one_marked() {
        let mut deleted = stored_key(None);
        deleted.id = 2;
        deleted.display_prefix = "hyd-9a9a9a9a".to_string();
        deleted.deleted_at = Some("2026-11-03 00:00:00".to_string());
        let keys = vec![
            KeyUsage {
                key: stored_key(None),
                groups: Vec::new(),
            },
            KeyUsage {
                key: deleted,
                groups: Vec::new(),
            },
        ];

        let options = key_options(&keys, Some(2));
        let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
        assert_eq!(labels, vec!["All keys", "hyd-4f9dEg2x", "hyd-9a9a9a9a"]);
        assert!(!options[0].selected);
        assert!(!options[1].selected);
        assert!(
            options[2].selected,
            "a soft-deleted key stays selectable and keeps its usage rows"
        );

        let options = key_options(&keys, None);
        assert!(options[0].selected);
        assert_eq!(options.len(), 3);
    }

    #[test]
    fn the_model_select_keeps_a_selected_model_the_month_has_no_rows_for() {
        let models = vec!["gpt-4o".to_string(), "claude-3".to_string()];
        let options = model_options(&models, Some("gpt-5-mini"));
        let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["All models", "claude-3", "gpt-4o", "gpt-5-mini"],
            "the list stays sorted and keeps the filter that is in force"
        );
        assert!(options[3].selected);
        assert!(!options[0].selected);

        let options = model_options(&models, None);
        assert_eq!(options.len(), 3);
        assert!(
            options[0].selected,
            "no filter means All models is selected"
        );

        let options = model_options(&models, Some("gpt-4o"));
        assert!(options[2].selected);
        assert_eq!(
            options.iter().filter(|option| option.selected).count(),
            1,
            "exactly one option is ever selected"
        );
    }

    #[test]
    fn the_charts_render_for_a_month_with_day_rows_and_price_a_day_like_the_table() {
        // The same usage as a month group and as one day group: 1000 prompt
        // tokens with 400 of them cached and 200 completions, which the
        // pinned formula prices at 4000 micro-USD.
        let mut group = usage("gpt-4o", 1000, 200, 1);
        group.cached_input_tokens = 400;
        let mut day = day_row("2026-11-02", "gpt-4o", 1000, 200);
        day.cached_input_tokens = 400;
        let mut data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: vec![group],
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        data.days = vec![day];
        data.models = vec!["gpt-4o".to_string()];

        let page = build_page(data, &Filters::default(), None);
        let plot = page.charts.expect("day rows draw both charts");
        assert!(plot.tokens_svg.starts_with("<svg "), "{}", plot.tokens_svg);
        assert!(plot.tokens_svg.contains("aria-label=\"Tokens per day\""));
        assert_eq!(
            plot.tokens_svg
                .matches("Day 2: 1,200 tokens (1,000 in, 200 out) in 1 requests")
                .count(),
            1,
            "the token bar carries the day's figures"
        );
        assert!(plot.cost_svg.contains("aria-label=\"Cost per day\""));
        assert!(
            plot.cost_svg.contains("Day 2: $0.004"),
            "the cached share is priced at the cached rate, not at the input rate: {}",
            plot.cost_svg
        );
        assert_eq!(
            page.grand.cost, "$0.004",
            "the chart and the table price the same rows the same way"
        );
    }

    #[test]
    fn the_charts_stay_absent_when_the_filtered_month_has_no_day_rows() {
        let mut data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        data.models = vec!["gpt-4o".to_string()];
        let page = build_page(data, &Filters::default(), None);
        assert!(page.charts.is_none());
        assert_eq!(page.rows.len(), 2, "the table still renders");
    }

    #[test]
    fn the_requests_chart_renders_with_status_rows_and_is_dropped_without_them() {
        let day = day_row("2026-11-02", "gpt-4o", 1000, 200);
        let mut data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        data.days = vec![day];
        data.status_code_days = vec![DayStatusCodeUsage {
            day: "2026-11-02".to_string(),
            status: Some(500),
            status_class: StatusBucket::Server5xx,
            requests: 2,
        }];
        data.status_code_totals = vec![
            StatusCodeTotal {
                status: Some(500),
                status_class: StatusBucket::Server5xx,
                requests: 2,
            },
            StatusCodeTotal {
                status: None,
                status_class: StatusBucket::Other,
                requests: 1,
            },
        ];
        let page = build_page(data, &Filters::default(), None);
        let status = page
            .charts
            .as_ref()
            .and_then(|plot| plot.status_svg.as_ref())
            .expect("status rows draw the requests chart");
        assert!(
            status.contains("aria-label=\"Requests per day\""),
            "{status}"
        );
        assert!(status.contains("Day 2: 2 requests (2 500)"), "{status}");
        // The table of the same view lists both codes, most requests first.
        assert_eq!(page.status_codes.len(), 2);
        assert_eq!(page.status_codes[0].code, "500");
        assert_eq!(page.status_codes[0].share, "66.7%");
        assert_eq!(page.status_codes[1].code, "n/a");
        assert_eq!(page.status_codes[1].class, "other");
        assert_eq!(page.status_codes[1].share, "33.3%");

        // A status filter that selects nothing drops the chart alone and
        // leaves the token and cost bars of the same view in place, with no
        // status code table either.
        let day = day_row("2026-11-02", "gpt-4o", 1000, 200);
        let mut data = stats_data(
            november(),
            vec![KeyUsage {
                key: stored_key(None),
                groups: vec![usage("gpt-4o", 1000, 200, 1)],
            }],
            HashMap::from([(7, channel_info_limitable())]),
        );
        data.days = vec![day];
        data.status_code_days = Vec::new();
        let page = build_page(
            data,
            &Filters {
                status_class: Some(StatusBucket::Client4xx),
                ..Filters::default()
            },
            None,
        );
        assert!(page.status_codes.is_empty());
        let plot = page
            .charts
            .expect("the token and cost rows still draw their charts");
        assert!(plot.status_svg.is_none());
        assert!(plot.tokens_svg.contains("aria-label=\"Tokens per day\""));
        assert!(plot.cost_svg.contains("aria-label=\"Cost per day\""));
    }

    #[test]
    fn the_status_code_table_labels_each_code_with_its_class_and_share() {
        let totals = vec![
            StatusCodeTotal {
                status: Some(500),
                status_class: StatusBucket::Server5xx,
                requests: 3,
            },
            StatusCodeTotal {
                status: Some(200),
                status_class: StatusBucket::Ok2xx,
                requests: 1,
            },
            StatusCodeTotal {
                status: None,
                status_class: StatusBucket::Other,
                requests: 1,
            },
        ];
        let rows = status_code_rows(&totals);
        assert_eq!(rows.len(), 3, "one line per code: {rows:?}");
        assert_eq!(rows[0].code, "500");
        assert_eq!(rows[0].class, "5xx");
        assert_eq!(rows[0].requests, "3");
        assert_eq!(rows[0].share, "60.0%");
        assert_eq!(rows[1].code, "200");
        assert_eq!(rows[1].class, "2xx");
        assert_eq!(rows[1].share, "20.0%");
        assert_eq!(rows[2].code, "n/a");
        assert_eq!(rows[2].class, "other");
        assert_eq!(rows[2].share, "20.0%");
        assert!(status_code_rows(&[]).is_empty());
    }

    #[test]
    fn a_share_of_a_period_without_requests_reads_zero_instead_of_dividing() {
        assert_eq!(share_label(0, 0), "0.0%");
        assert_eq!(share_label(1, 3), "33.3%");
        assert_eq!(share_label(2, 3), "66.7%");
        assert_eq!(share_label(5, 5), "100.0%");
        assert_eq!(share_label(1_000, i64::MAX), "0.0%");
    }

    #[test]
    fn the_failure_shell_keeps_the_navigation_and_drops_the_empty_message() {
        let month = november();
        let page = page_shell(
            &month,
            &Filters::default(),
            Some("internal server error".into()),
        );
        assert!(page.rows.is_empty());
        assert_eq!(page.error.as_deref(), Some("internal server error"));
        assert!(page.empty_message.is_empty());
        assert!(page.charts.is_none());
        assert_eq!(page.prev_url, "/admin/ui/stats?month=2026-10");
        assert_eq!(page.next_url, "/admin/ui/stats?month=2026-12");
        assert_eq!(
            page.key_options.len(),
            1,
            "the key select keeps its default"
        );
        assert!(page.key_options[0].selected);
        assert!(page.status_options[0].selected);
        assert_eq!(page.status_options.len(), 5);
    }

    #[test]
    fn the_current_month_is_flagged_and_linked() {
        let page = page_shell(&Month::current(), &Filters::default(), None);
        assert!(page.is_current_month);
        assert_eq!(
            page.current_url,
            view_url(&Month::current(), &Filters::default())
        );
        assert!(!page.empty_message.is_empty());
    }

    #[test]
    fn the_navigation_links_of_a_filtered_view_keep_the_filters() {
        let month = november();
        let filters = Filters {
            key_id: Some(4),
            model: Some("gpt-4o".to_string()),
            status_class: Some(StatusBucket::Server5xx),
        };
        let page = page_shell(&month, &filters, None);
        assert_eq!(
            page.prev_url,
            "/admin/ui/stats?month=2026-10&key_id=4&model=gpt-4o&status_class=5xx"
        );
        assert_eq!(
            page.next_url,
            "/admin/ui/stats?month=2026-12&key_id=4&model=gpt-4o&status_class=5xx"
        );
        assert!(page
            .empty_message
            .contains("No usage matches the current filters"));
    }
}
