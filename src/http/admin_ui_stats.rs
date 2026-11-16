//! Admin UI stats pages: the Tokens page, token usage and cost per client
//! key over a selected date range grouped by model, and the Requests page,
//! the request counts of the same range per day and per response status
//! code.
//!
//! Every figure either page shows comes out of the same aggregates of the
//! range: the day-level ones, [`crate::db::request_logs::day_usage`] for the
//! tokens and the cost and
//! [`crate::db::request_logs::day_status_code_usage`] with
//! [`crate::db::request_logs::status_code_totals`] for the requests, or,
//! when the range covers one or two days, their hourly twins
//! ([`crate::db::request_logs::hour_usage`],
//! [`crate::db::request_logs::hour_status_code_usage`] and
//! [`crate::db::request_logs::hour_latency`]), which show the shape of such a
//! day bucket by bucket instead of one bar per day. Both granularities carry
//! the same dimensions and the same filters, and the cost of a bucket comes
//! out of the same pinned cost formula the monthly-limit enforcement uses
//! ([`crate::usage::group_cost`]), so a table, a chart and the status code
//! table can never disagree about the same request: the table folds the rows
//! per key and model, the charts plot the rows themselves. No second
//! aggregate query and no second pricing path exists, so a range that runs
//! over a whole calendar month prices what the gate prices for that month.
//! Each page runs the queries its own content needs: the Tokens page never
//! asks for the status aggregates, the Requests page never asks for the usage
//! rows or the price tables.
//!
//! Cost comes from the price table of the channel the key is bound to,
//! exactly as enforcement prices it: a model the channel does not price
//! costs zero, which the footnote on the page states. A key carrying a
//! stored monthly limit on a channel that cannot be limited (disabled
//! channel, open whitelist, incomplete price list) shows the same
//! informational badge the keys panel shows.
//!
//! Rendering rules worth naming. A key with no usage in the selected range
//! is omitted entirely, because the table lists usage rather than keys;
//! soft-deleted keys keep their rows, marked, since they spent the tokens.
//! Rows whose upstream reported no usage add no tokens to the sums while
//! still counting as requests, and rows rejected before a key resolved
//! (`api_key_id` NULL) belong to no key and are counted nowhere.
//!
//! The range travels in the query string as `from` and `to`, both
//! `YYYY-MM-DD`, so every view is shareable and reloadable without
//! JavaScript. The admin's `to` day is inclusive, and the SQL predicate
//! compares with `<`, so the bounds handed to the aggregates are the first
//! instant of `from` and the first instant after `to` ([`DateRange::end`]
//! carries that conversion). An empty `from` is the first day of the current
//! month and an empty `to` is today, so a page without a range shows the
//! month to date; a malformed day or a `from` after `to` falls back to that
//! default range behind a banner instead of failing the page.
//!
//! Three shortcut buttons sit between the date fields and the filters,
//! Today, Current week and Current month, on both pages alike. Each is a
//! submit button carrying a `preset` value, resolved to concrete dates at
//! render time (the same pattern the logs page presets follow), so a shared
//! shortcut stays relative to the moment it is opened and the fields always
//! show the days a view really covers.
//!
//! The filters narrow the view, a key and a model on both pages and a status
//! class on the Requests page, read from the same query string and applied to
//! the rows in their own query, so every figure of a page reads the same
//! rows. The status class narrows the whole Requests page, its chart and its
//! status code table alike, because everything that page counts is a
//! request. The model list of the select stays range-scoped and unfiltered,
//! so the control always offers every model of the range.
//!
//! The Tokens page holds the usage table and, under it, two charts of the
//! same filtered range, tokens and cost per bucket, drawn server-side by
//! [`crate::http::stats_chart`] as plain SVG and injected with askama's
//! `|safe` filter. The cost of a bucket is the sum of
//! [`crate::usage::group_cost`] over that bucket's groups, priced from the
//! key's channel exactly like the table, so the bars and the table add up to
//! the same money. A view with no usage rows renders no chart at all and
//! keeps the table's empty state instead.
//!
//! The Requests page holds the requests chart, stacking one segment per
//! exact response status code of the range, under it the latency chart, one
//! bar per bucket of the mean duration of the requests the filters select,
//! and then the status code table: one line per exact code of the range with
//! its class, its request count and its share of the requests the filters
//! select, the busiest code first. Each panel is left out when its own query
//! returns no row, and the page then falls back to its empty state; the
//! latency aggregate is the one that keeps the requests that never reached an
//! upstream, so a range of plain rejections still has a mean to show.
//!
//! There is no mutating route here, so the CSRF guard has nothing to reject;
//! the module is merged into the `/admin/ui` nest like the other read-only
//! pages.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use time::{Date, Duration, OffsetDateTime};

use crate::db::channels::{self, PricedChannel};
use crate::db::keys::{self, ApiKey};
use crate::db::request_logs::{
    self, DayStatusCodeUsage, DayUsage, HourLatency, HourStatusCodeUsage, HourUsage, ModelUsage,
    StatusBucket, StatusCodeTotal,
};
use crate::db::Db;
use crate::http::admin_ui_logs::{thousands, SelectOption};
use crate::http::stats_chart::{self, Granularity};
use crate::http::ui_auth::UiAuth;
use crate::http::AppState;
use crate::limits;
use crate::prices::ModelPrices;
use crate::usage;

/// The shortcut buttons of the filter bar: the value of the `preset`
/// parameter and the label of the button that carries it. Each one resolves
/// to concrete days at render time, so a shortcut link stays relative to the
/// moment it is opened.
const PRESETS: &[(&str, &str)] = &[
    (PRESET_TODAY, "Today"),
    (PRESET_WEEK, "Current week"),
    (PRESET_MONTH, "Current month"),
];

/// The current day, as both bounds of the range.
const PRESET_TODAY: &str = "today";
/// The Monday through Sunday of the current ISO week.
const PRESET_WEEK: &str = "week";
/// The first through the last day of the current month.
const PRESET_MONTH: &str = "month";

/// The view state of the page: the date range, the shortcut that may stand
/// in for it and the three optional filters. Kept as strings so a malformed
/// or empty value becomes a banner over a usable view instead of a rejected
/// request.
#[derive(Debug, Default, Deserialize)]
pub struct StatsQuery {
    from: Option<String>,
    to: Option<String>,
    preset: Option<String>,
    key_id: Option<String>,
    model: Option<String>,
    status_class: Option<String>,
}

/// The inclusive UTC day range of one view: the days the `from` and `to`
/// parameters name, or the days a shortcut resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DateRange {
    /// The first day of the range, inclusive.
    from: Date,
    /// The last day of the range, inclusive for the admin. The SQL predicate
    /// compares with `<`, so it reads the bound [`DateRange::end`] builds:
    /// the first instant of the following day.
    to: Date,
}

impl DateRange {
    /// The first day of `day`'s month through `day` itself.
    fn month_to_date(day: Date) -> Self {
        DateRange {
            from: month_start(day),
            to: day,
        }
    }

    /// The inclusive lower bound in the canonical stored timestamp format.
    fn start(&self) -> String {
        format!("{} 00:00:00", day_text(self.from))
    }

    /// The exclusive upper bound in the canonical stored timestamp format.
    ///
    /// The admin's `to` day is inclusive and the SQL predicate compares with
    /// `<`, so the bound is the first instant of the day after it; a request
    /// of the `to` day, at any hour, stays inside. The last day of the
    /// supported calendar has no day after it, and the bound then names the
    /// end of that day, which no stored timestamp reaches.
    fn end(&self) -> String {
        match self.to.next_day() {
            Some(next) => format!("{} 00:00:00", day_text(next)),
            None => format!("{} 24:00:00", day_text(self.to)),
        }
    }

    /// How the range reads in the page's headings: `2026-11-01 to 2026-11-15`.
    fn label(&self) -> String {
        format!("{} to {}", day_text(self.from), day_text(self.to))
    }
}

/// One shortcut button of the filter bar: the `preset` value it submits and
/// the label it shows.
pub struct PresetButton {
    pub value: &'static str,
    pub label: &'static str,
}

/// The shortcut buttons, in the order the bar offers them.
fn preset_buttons() -> Vec<PresetButton> {
    PRESETS
        .iter()
        .map(|(value, label)| PresetButton { value, label })
        .collect()
}

/// The accepted `preset` values, in the order the buttons offer them, so the
/// banner and the controls cannot list different vocabularies.
fn preset_values() -> String {
    PRESETS
        .iter()
        .map(|(value, _)| *value)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The current UTC day, the day the default range ends on.
fn today_utc() -> Date {
    OffsetDateTime::now_utc().date()
}

/// The `YYYY-MM-DD` text of a day, the shape a `type="date"` input and the
/// `from` and `to` parameters carry.
fn day_text(day: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

/// Reads a `YYYY-MM-DD` day; `None` for anything else, including a day its
/// month does not have (`2026-02-30`), a year that is not four digits and a
/// month outside 1 through 12.
fn parse_day(raw: &str) -> Option<Date> {
    let (year, rest) = raw.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    if !digits(year, 4) || !digits(month, 2) || !digits(day, 2) {
        return None;
    }
    let month = time::Month::try_from(month.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(year.parse().ok()?, month, day.parse().ok()?).ok()
}

/// Exactly `width` ASCII digits.
fn digits(text: &str, width: usize) -> bool {
    text.len() == width && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The first day of the month a day falls in.
fn month_start(day: Date) -> Date {
    Date::from_calendar_date(day.year(), day.month(), 1).expect("the first of a month exists")
}

/// The last day of the month a day falls in, the length of that month in the
/// leap-year sense included.
fn month_end(day: Date) -> Date {
    let last = time::util::days_in_month(day.month(), day.year());
    Date::from_calendar_date(day.year(), day.month(), last).expect("the month has a last day")
}

/// The Monday of the ISO week a day falls in; the day itself when it is a
/// Monday.
fn week_start(day: Date) -> Date {
    let offset = Duration::days(i64::from(day.weekday().number_days_from_monday()));
    // Only a day in the first week of the supported calendar has no Monday
    // behind it, and no shortcut produces one.
    day.checked_sub(offset).unwrap_or(day)
}

/// The days one shortcut stands for, relative to `today`: the current day,
/// the week it falls in, or the month it falls in.
fn preset_range(name: &str, today: Date) -> Option<DateRange> {
    match name {
        PRESET_TODAY => Some(DateRange {
            from: today,
            to: today,
        }),
        // The week runs through its Sunday, which is later than today for
        // every day but Sunday itself; those days simply have no data yet.
        PRESET_WEEK => {
            let monday = week_start(today);
            let sunday = monday.checked_add(Duration::days(6)).unwrap_or(today);
            Some(DateRange {
                from: monday,
                to: sunday,
            })
        }
        // The whole month, the days that have not started included.
        PRESET_MONTH => Some(DateRange {
            from: month_start(today),
            to: month_end(today),
        }),
        _ => None,
    }
}

/// A parameter's value when it carries something other than whitespace.
fn non_blank(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|value| !value.is_empty())
}

/// The key, model and status class filters of one view, after parsing. Every
/// one is absent when its parameter is missing or blank; the key is also
/// absent when the value is not a row id and the class when it names no
/// bucket. The Tokens page reads the first two and the Requests page all
/// three.
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

/// The charts of the Tokens page, already rendered as SVG. Present only when
/// the filtered range has rows to plot.
pub struct TokenCharts {
    /// Tokens per day, input and output stacked.
    pub tokens_svg: String,
    /// Cost per day, priced from each key's channel.
    pub cost_svg: String,
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

/// The Tokens page: the usage table and the token and cost charts of one date
/// range, under the key and model filters of the URL.
#[derive(Template, WebTemplate)]
#[template(path = "tokens.html")]
pub struct TokensPage {
    pub version: &'static str,
    pub section: &'static str,
    /// The rendered range, `2026-11-01 to 2026-11-15`.
    pub range_label: String,
    /// The first day of the range, as the `from` input value.
    pub from_value: String,
    /// The last day of the range, as the `to` input value.
    pub to_value: String,
    /// The shortcut buttons of the filter bar.
    pub preset_buttons: Vec<PresetButton>,
    pub rows: Vec<StatsRow>,
    pub grand: DisplaySums,
    /// The Key select: `All keys` plus every stored key, soft-deleted ones
    /// included.
    pub key_options: Vec<SelectOption>,
    /// The Model select: `All models` plus every model logged in the range.
    pub model_options: Vec<SelectOption>,
    /// The bucket the charts and the headings of this view span, resolved
    /// from the range: a one or two day range plots hours, a longer one days.
    pub(crate) granularity: Granularity,
    /// Absent when the filtered range has nothing to plot.
    pub charts: Option<TokenCharts>,
    pub empty_message: String,
    pub error: Option<String>,
}

/// The Requests page: the requests chart and the status code table of one
/// date range, under the key, model and status class filters of the URL.
#[derive(Template, WebTemplate)]
#[template(path = "requests.html")]
pub struct RequestsPage {
    pub version: &'static str,
    pub section: &'static str,
    /// The rendered range, `2026-11-01 to 2026-11-15`.
    pub range_label: String,
    /// The first day of the range, as the `from` input value.
    pub from_value: String,
    /// The last day of the range, as the `to` input value.
    pub to_value: String,
    /// The shortcut buttons of the filter bar.
    pub preset_buttons: Vec<PresetButton>,
    /// The Key select: `All keys` plus every stored key, soft-deleted ones
    /// included.
    pub key_options: Vec<SelectOption>,
    /// The Model select: `All models` plus every model logged in the range.
    pub model_options: Vec<SelectOption>,
    /// The Status select: `All statuses` plus the four classes the requests
    /// chart stacks.
    pub status_options: Vec<SelectOption>,
    /// The status code table of the range, busiest code first; empty when
    /// the filters select no request, which is when the template leaves the
    /// whole panel out.
    pub status_codes: Vec<StatusCodeRow>,
    /// The bucket the charts and the headings of this view span, resolved
    /// from the range: a one or two day range plots hours, a longer one days.
    pub(crate) granularity: Granularity,
    /// Requests per bucket, one segment per exact status code. Absent when
    /// the filters select no request, because an empty plot says less than
    /// none.
    pub status_svg: Option<String>,
    /// The mean request duration per bucket, the latency chart of the range.
    /// Absent when no request of the view was timed, which is a view the
    /// filters select nothing from.
    pub latency_svg: Option<String>,
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

/// Everything the Tokens page needs out of the database.
struct TokensData {
    range: DateRange,
    /// Every stored key, soft-deleted ones included: they fill the key select
    /// and name the channel a row's cost comes from.
    keys: Vec<ApiKey>,
    channels: HashMap<i64, ChannelInfo>,
    /// Usage rows of the filtered range, one per day or one per hour
    /// whichever the range selected: the table folds them per key and model,
    /// the charts plot them as they are.
    usage: UsageRows,
    /// The distinct models logged in the range, for the Model select.
    models: Vec<String>,
}

/// The usage rows of one view, at the granularity its range selected: the
/// hour rows of a one or two day range, the day rows of a longer one.
///
/// The two shapes carry the same key, model and token dimensions, so the
/// table folds either one per key and model and the cost of a bucket is
/// priced the same way; only the charts, which place a bucket on an axis,
/// read the shape itself.
enum UsageRows {
    /// One row per (day, key, model), from
    /// [`crate::db::request_logs::day_usage`].
    Daily(Vec<DayUsage>),
    /// One row per (hour, key, model), from
    /// [`crate::db::request_logs::hour_usage`].
    Hourly(Vec<HourUsage>),
}

impl UsageRows {
    /// Whether the range has no usage row at all, which is when the page
    /// draws no chart and keeps its empty state.
    fn is_empty(&self) -> bool {
        match self {
            UsageRows::Daily(rows) => rows.is_empty(),
            UsageRows::Hourly(rows) => rows.is_empty(),
        }
    }

    /// The usage groups of one key, folded per model out of whichever shape
    /// the range loaded. The rows already carry the range and the key and
    /// model filters, so this fold narrows nothing.
    fn key_groups(&self, key_id: i64) -> Vec<ModelUsage> {
        match self {
            UsageRows::Daily(rows) => key_groups(key_id, rows),
            UsageRows::Hourly(rows) => key_groups(key_id, rows),
        }
    }
}

/// Everything the Requests page needs out of the database: the request
/// aggregates of the range and the lists the two selects offer.
struct RequestsData {
    range: DateRange,
    /// Every stored key, soft-deleted ones included: they fill the key
    /// select.
    keys: Vec<ApiKey>,
    /// Request counts per exact status code of the filtered range, one row
    /// per day or per hour, for the requests chart.
    requests: RequestRows,
    /// The mean duration of every hour of the filtered range, the only rows
    /// the latency chart reads: it folds them onto the daily grid itself
    /// when the range is a long one.
    latency: Vec<HourLatency>,
    /// Request totals per exact status code of the filtered range, for the
    /// status code table.
    status_code_totals: Vec<StatusCodeTotal>,
    /// The distinct models logged in the range, for the Model select.
    models: Vec<String>,
}

/// The per-code request counts of one view, at the granularity its range
/// selected: the twin of [`UsageRows`] for the requests chart.
enum RequestRows {
    /// One row per (day, exact status code), from
    /// [`crate::db::request_logs::day_status_code_usage`].
    Daily(Vec<DayStatusCodeUsage>),
    /// One row per (hour, exact status code), from
    /// [`crate::db::request_logs::hour_status_code_usage`].
    Hourly(Vec<HourStatusCodeUsage>),
}

impl RequestRows {
    /// Whether the range has no request row at all, which is when no
    /// requests chart is drawn.
    fn is_empty(&self) -> bool {
        match self {
            RequestRows::Daily(rows) => rows.is_empty(),
            RequestRows::Hourly(rows) => rows.is_empty(),
        }
    }
}

fn internal_error(error: &sqlx::Error) -> String {
    tracing::error!(error = %error, "admin ui stats query failed");
    "internal server error".to_string()
}

/// The bucket the figures of one view are grouped by: a range of one or two
/// days plots hours, a longer one days. The decision is made once per
/// request, out of the same resolved range the queries read, so the rows, the
/// charts and the headings of a page can never span different buckets.
fn granularity(range: &DateRange) -> Granularity {
    Granularity::from_range(range.from, range.to)
}

/// Resolves the range of one view out of the `preset`, `from` and `to`
/// parameters, with the banner text of a value the page cannot use.
///
/// A shortcut wins over the two date fields, because the button that carries
/// it has just computed them. Without one, an empty `from` is the first day
/// of the current month and an empty `to` is today, so a bare page shows the
/// month to date. A malformed day and a `from` after `to` both render that
/// default range behind a banner, the same lenient reading the month
/// parameter used to get.
fn resolve_range(query: &StatsQuery) -> (DateRange, Option<String>) {
    let today = today_utc();
    let default = DateRange::month_to_date(today);
    if let Some(name) = non_blank(query.preset.as_deref()) {
        return match preset_range(name, today) {
            Some(range) => (range, None),
            None => (
                default,
                Some(format!(
                    "preset \"{name}\" is not one of {}; showing {}",
                    preset_values(),
                    default.label()
                )),
            ),
        };
    }
    let (from, from_error) = match non_blank(query.from.as_deref()) {
        None => (default.from, None),
        Some(raw) => match parse_day(raw) {
            Some(day) => (day, None),
            None => (
                default.from,
                Some(format!("from \"{raw}\" is not a YYYY-MM-DD date")),
            ),
        },
    };
    let (to, to_error) = match non_blank(query.to.as_deref()) {
        None => (default.to, None),
        Some(raw) => match parse_day(raw) {
            Some(day) => (day, None),
            None => (
                default.to,
                Some(format!("to \"{raw}\" is not a YYYY-MM-DD date")),
            ),
        },
    };
    if let Some(error) = from_error.or(to_error) {
        return (
            default,
            Some(format!("{error}; showing {}", default.label())),
        );
    }
    if from > to {
        return (
            default,
            Some(format!(
                "from {} is after to {}; showing {}",
                day_text(from),
                day_text(to),
                default.label()
            )),
        );
    }
    (DateRange { from, to }, None)
}

/// Resolves the `key_id`, `model` and `status_class` query parameters into
/// the view's filters. A blank value means "no filter", an unparsable key id
/// or status class yields the banner text and no filter (the same lenient
/// reading the range parameters get), and the model name is trimmed so a
/// stray space cannot hide every row.
///
/// `with_status_class` is false on the Tokens page, which carries no status
/// filter: the parameter is then not read at all, so neither a class nor a
/// banner about one can come out of it.
fn resolve_filters(query: &StatsQuery, with_status_class: bool) -> (Filters, Option<String>) {
    let model = non_blank(query.model.as_deref()).map(str::to_string);
    let raw_key = non_blank(query.key_id.as_deref());
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
    let raw_class = non_blank(query.status_class.as_deref()).filter(|_| with_status_class);
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

/// Loads the key list, the channel price tables, the usage rows the Tokens
/// page reads and the model names of the range.
///
/// The usage query carries the range and the key and model filters, so those
/// rows answer the table and both charts alike, and its granularity follows
/// the range: hours for one or two days, days for anything longer. No status
/// aggregate is asked for, because nothing on this page counts requests by
/// their status. The model list stays range-scoped and unfiltered, so the
/// select always offers every model of the range rather than only the
/// surviving one.
async fn load_tokens(
    state: &AppState,
    range: &DateRange,
    filters: &Filters,
) -> Result<TokensData, String> {
    // The bounds the aggregates compare against: the first instant of the
    // range and the first instant after its last day.
    let start = range.start();
    let end = range.end();
    let keys = keys::list(&state.db)
        .await
        .map_err(|error| internal_error(&error))?;
    let channels = channel_info(&state.db)
        .await
        .map_err(|error| internal_error(&error))?;
    let usage = match granularity(range) {
        Granularity::Hourly => UsageRows::Hourly(
            request_logs::hour_usage(
                &state.db,
                &start,
                &end,
                filters.key_id,
                filters.model.clone(),
                filters.status_class,
            )
            .await
            .map_err(|error| internal_error(&error))?,
        ),
        Granularity::Daily => UsageRows::Daily(
            request_logs::day_usage(
                &state.db,
                &start,
                &end,
                filters.key_id,
                filters.model.clone(),
            )
            .await
            .map_err(|error| internal_error(&error))?,
        ),
    };
    let models = request_logs::distinct_models(&state.db, &start, &end)
        .await
        .map_err(|error| internal_error(&error))?;
    Ok(TokensData {
        range: *range,
        keys,
        channels,
        usage,
        models,
    })
}

/// Loads the key list, the per-code request rows, the latency rows, the
/// status code totals of the range and the model names of the range, which is
/// everything the Requests page reads.
///
/// The per-code query carries the range and the key, model and status class
/// filters, so the chart is built from the selection the table reports, and
/// its granularity follows the range exactly as the Tokens page's usage query
/// does. The latency aggregate is the hourly one for every range, because the
/// chart folds its hours onto the daily grid itself when the range is a long
/// one; it takes the same filters. The model list of the select stays
/// range-scoped and unfiltered on purpose, so the control always offers every
/// model of the range rather than only the surviving one. Neither cost nor
/// token data is asked for, because nothing on this page counts them.
async fn load_requests(
    state: &AppState,
    range: &DateRange,
    filters: &Filters,
) -> Result<RequestsData, String> {
    let start = range.start();
    let end = range.end();
    let keys = keys::list(&state.db)
        .await
        .map_err(|error| internal_error(&error))?;
    let requests = match granularity(range) {
        Granularity::Hourly => RequestRows::Hourly(
            request_logs::hour_status_code_usage(
                &state.db,
                &start,
                &end,
                filters.key_id,
                filters.model.clone(),
                filters.status_class,
            )
            .await
            .map_err(|error| internal_error(&error))?,
        ),
        Granularity::Daily => RequestRows::Daily(
            request_logs::day_status_code_usage(
                &state.db,
                &start,
                &end,
                filters.key_id,
                filters.model.clone(),
                filters.status_class,
            )
            .await
            .map_err(|error| internal_error(&error))?,
        ),
    };
    let latency = request_logs::hour_latency(
        &state.db,
        &start,
        &end,
        filters.key_id,
        filters.model.clone(),
        filters.status_class,
    )
    .await
    .map_err(|error| internal_error(&error))?;
    let status_code_totals = request_logs::status_code_totals(
        &state.db,
        &start,
        &end,
        filters.key_id,
        filters.model.clone(),
        filters.status_class,
    )
    .await
    .map_err(|error| internal_error(&error))?;
    let models = request_logs::distinct_models(&state.db, &start, &end)
        .await
        .map_err(|error| internal_error(&error))?;
    Ok(RequestsData {
        range: *range,
        keys,
        requests,
        latency,
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
fn key_options(keys: &[ApiKey], selected: Option<i64>) -> Vec<SelectOption> {
    let mut options = vec![SelectOption {
        value: String::new(),
        label: "All keys".to_string(),
        selected: selected.is_none(),
    }];
    options.extend(keys.iter().map(|key| SelectOption {
        value: key.id.to_string(),
        label: key_label(key),
        selected: Some(key.id) == selected,
    }));
    options
}

/// The Model select: `All models` plus every model logged in the range,
/// alphabetically. A selected model the range has no rows for is added to
/// the list, so narrowing the range behind a model filter never leaves the
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

/// The Tokens page with its filter widgets and empty table in place; the
/// rows, the grand totals and the charts are filled in by
/// [`build_tokens_page`].
fn tokens_shell(range: &DateRange, filters: &Filters, error: Option<String>) -> TokensPage {
    let from = day_text(range.from);
    let to = day_text(range.to);
    let empty_message = if error.is_some() {
        String::new()
    } else if filters.is_set() {
        format!("No usage matches the current filters between {from} and {to}.")
    } else {
        format!(
            "No requests logged between {from} and {to}. Usage appears here as \
             clients call /v1/chat/completions."
        )
    };
    TokensPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "tokens",
        range_label: range.label(),
        from_value: from,
        to_value: to,
        preset_buttons: preset_buttons(),
        rows: Vec::new(),
        grand: Sums::default().display(),
        key_options: key_options(&[], filters.key_id),
        model_options: model_options(&[], filters.model.as_deref()),
        granularity: granularity(range),
        charts: None,
        empty_message,
        error,
    }
}

/// The Requests page with its filter widgets in place; the chart and the
/// status code table are filled in by [`build_requests_page`].
fn requests_shell(range: &DateRange, filters: &Filters, error: Option<String>) -> RequestsPage {
    let from = day_text(range.from);
    let to = day_text(range.to);
    let empty_message = if error.is_some() {
        String::new()
    } else if filters.is_set() {
        format!("No requests match the current filters between {from} and {to}.")
    } else {
        format!(
            "No requests logged between {from} and {to}. The chart and the code \
             breakdown fill in as clients call the API."
        )
    };
    RequestsPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "requests",
        range_label: range.label(),
        from_value: from,
        to_value: to,
        preset_buttons: preset_buttons(),
        key_options: key_options(&[], filters.key_id),
        model_options: model_options(&[], filters.model.as_deref()),
        status_options: status_options(filters.status_class),
        status_codes: Vec::new(),
        granularity: granularity(range),
        status_svg: None,
        latency_svg: None,
        empty_message,
        error,
    }
}

/// One day group as the shape the pricing fold reads, so a day is priced by
/// the same [`usage::group_cost`] call the table's folded groups go through.
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

/// One hour group as the shape the pricing fold reads, the hourly twin of
/// [`day_group`]: an hour carries the same dimensions a day does, so an hour
/// is priced by the same call.
fn hour_group(hour: &HourUsage) -> ModelUsage {
    ModelUsage {
        model: hour.model.clone(),
        prompt_tokens: hour.prompt_tokens,
        completion_tokens: hour.completion_tokens,
        cached_input_tokens: hour.cached_input_tokens,
        cached_output_tokens: hour.cached_output_tokens,
        cache_write_input_tokens: hour.cache_write_input_tokens,
        requests: hour.requests,
    }
}

/// A usage row as the page reads it, whichever aggregate produced it: the
/// label of its bucket, the key that spent it and the counts the cost formula
/// prices. The day and the hour aggregates carry the same dimensions under
/// different names, so one fold serves both granularities.
trait UsageRow {
    /// The label of the row's bucket: its day or its hour.
    fn bucket(&self) -> &str;
    /// The key the row's requests were made with; never missing in a row an
    /// aggregate returns.
    fn key_id(&self) -> i64;
    /// The row's counts in the shape the pricing fold reads.
    fn usage(&self) -> ModelUsage;
}

impl UsageRow for DayUsage {
    fn bucket(&self) -> &str {
        &self.day
    }

    fn key_id(&self) -> i64 {
        self.key_id
    }

    fn usage(&self) -> ModelUsage {
        day_group(self)
    }
}

impl UsageRow for HourUsage {
    fn bucket(&self) -> &str {
        &self.hour
    }

    fn key_id(&self) -> i64 {
        self.key_id
    }

    fn usage(&self) -> ModelUsage {
        hour_group(self)
    }
}

/// The price table of every key of the page, keyed by the key id: a usage row
/// names the key and not the channel, so the key's bound channel is where its
/// prices come from.
fn key_channels(data: &TokensData) -> HashMap<i64, &ChannelInfo> {
    data.keys
        .iter()
        .filter_map(|key| {
            data.channels
                .get(&key.channel_id)
                .map(|info| (key.id, info))
        })
        .collect()
}

/// One entry per bucket row: the label of the bucket and the cost of the
/// row's groups, priced from the price table of the key that spent it. A key
/// whose channel is missing or prices nothing contributes zero, exactly as it
/// does in the table.
fn cost_entries<R: UsageRow>(
    rows: &[R],
    channels: &HashMap<i64, &ChannelInfo>,
) -> Vec<(String, i64)> {
    rows.iter()
        .map(|row| {
            let prices = channels
                .get(&row.key_id())
                .and_then(|info| info.prices.as_ref());
            (
                row.bucket().to_string(),
                usage::group_cost(&row.usage(), prices),
            )
        })
        .collect()
}

/// The charts of the Tokens page: the token bars and the cost bars of the
/// range, drawn at the granularity its usage rows carry, each built by
/// pricing every bucket group from the price table of the key that spent it.
/// A key whose channel is missing or prices nothing contributes zero, exactly
/// as it does in the table. `None` when the filtered range has no usage row
/// at all, which is when the page shows its empty state instead of an empty
/// plot.
fn tokens_charts(data: &TokensData, theme: stats_chart::ChartTheme) -> Option<TokenCharts> {
    if data.usage.is_empty() {
        return None;
    }
    // A bucket group names the key, not the channel, so the key's bound
    // channel is where its prices come from.
    let channels = key_channels(data);
    let from = data.range.from;
    let to = data.range.to;
    let (tokens_svg, cost_svg) = match &data.usage {
        UsageRows::Daily(rows) => {
            let costs = cost_entries(rows, &channels);
            (
                stats_chart::daily_token_bars(rows, theme),
                stats_chart::daily_cost_bars(&costs, theme),
            )
        }
        UsageRows::Hourly(rows) => {
            let costs = cost_entries(rows, &channels);
            (
                stats_chart::hourly_token_bars(rows, from, to, theme),
                stats_chart::hourly_cost_bars(&costs, from, to, theme),
            )
        }
    };
    Some(TokenCharts {
        tokens_svg,
        cost_svg,
    })
}

/// Renders the Tokens page: the model lines of every key that saw usage in
/// the range, each followed by the key's total line, plus the grand total and
/// the two charts.
///
/// The rows are folded out of the same usage rows the charts plot, day rows
/// or hour rows as the range selected, and those rows already carry the
/// range, key and model filters, so the table and the charts can never
/// disagree about a request. A key left with no group disappears whole,
/// exactly like a key with no usage in the range at all.
fn build_tokens_page(
    data: TokensData,
    filters: &Filters,
    error: Option<String>,
    theme: stats_chart::ChartTheme,
) -> TokensPage {
    let mut page = tokens_shell(&data.range, filters, error);
    let mut grand = Sums::default();
    let mut rows: Vec<StatsRow> = Vec::new();

    for key in &data.keys {
        let mut groups = data.usage.key_groups(key.id);
        if groups.is_empty() {
            continue;
        }
        let info = data.channels.get(&key.channel_id);
        let prices = info.and_then(|info| info.prices.as_ref());
        let limit_inactive =
            key.monthly_limit_micro_usd.is_some() && !info.is_some_and(|i| i.limitable);
        let key_label = key_label(key);
        let key_deleted = key.deleted_at.is_some();

        groups.sort_by(|left, right| sort_key(&left.model).cmp(&sort_key(&right.model)));

        // The key's total is the shared fold of the same per-group costs the
        // rows below show, so the two can never disagree.
        let mut key_sums = Sums::default();
        for group in &groups {
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
    page.charts = tokens_charts(&data, theme);
    page.grand = grand.display();
    page.rows = rows;
    page
}

/// Renders the Requests page: the requests chart stacked by exact status
/// code, the latency chart and the status code table of the same selection.
///
/// The three read the rows the queries already filtered, so the chart and the
/// table can never disagree about a request, and the model select keeps
/// offering every model logged in the range rather than only the surviving
/// one. A filter that selects no request draws no requests chart and leaves
/// the table out; a filter that selects no timed request in addition leaves
/// the latency chart out, and the page falls back to its empty state.
fn build_requests_page(
    data: RequestsData,
    filters: &Filters,
    error: Option<String>,
    theme: stats_chart::ChartTheme,
) -> RequestsPage {
    let mut page = requests_shell(&data.range, filters, error);
    let from = data.range.from;
    let to = data.range.to;
    page.key_options = key_options(&data.keys, filters.key_id);
    page.model_options = model_options(&data.models, filters.model.as_deref());
    page.status_options = status_options(filters.status_class);
    page.status_codes = status_code_rows(&data.status_code_totals);
    page.status_svg = (!data.requests.is_empty()).then(|| match &data.requests {
        RequestRows::Daily(rows) => stats_chart::daily_status_code_bars(rows, theme),
        RequestRows::Hourly(rows) => stats_chart::hourly_status_code_bars(rows, from, to, theme),
    });
    // The latency aggregate is hourly for every range; the chart folds its
    // hours onto the daily grid when the range is a long one.
    page.latency_svg = (!data.latency.is_empty())
        .then(|| stats_chart::latency_bars(&data.latency, from, to, theme));
    if page.status_svg.is_some() || page.latency_svg.is_some() || !page.status_codes.is_empty() {
        page.empty_message.clear();
    }
    page
}

/// The usage groups of one key: the bucket rows of that key folded per model,
/// which is the shape the table's rows and costs are built from. The rows
/// already carry the range and the key and model filters, so this fold
/// narrows nothing.
fn key_groups<R: UsageRow>(key_id: i64, rows: &[R]) -> Vec<ModelUsage> {
    let mut groups: Vec<ModelUsage> = Vec::new();
    for row in rows.iter().filter(|row| row.key_id() == key_id) {
        let usage = row.usage();
        match groups.iter_mut().find(|group| group.model == usage.model) {
            Some(group) => add_usage(group, &usage),
            None => groups.push(usage),
        }
    }
    groups
}

/// Adds one bucket group's counts onto the model group of the same model, so
/// a key's models sum every bucket of the range they saw.
fn add_usage(group: &mut ModelUsage, usage: &ModelUsage) {
    group.prompt_tokens = group.prompt_tokens.saturating_add(usage.prompt_tokens);
    group.completion_tokens = group
        .completion_tokens
        .saturating_add(usage.completion_tokens);
    group.cached_input_tokens = group
        .cached_input_tokens
        .saturating_add(usage.cached_input_tokens);
    group.cached_output_tokens = group
        .cached_output_tokens
        .saturating_add(usage.cached_output_tokens);
    group.cache_write_input_tokens = group
        .cache_write_input_tokens
        .saturating_add(usage.cache_write_input_tokens);
    group.requests = group.requests.saturating_add(usage.requests);
}

/// Reads the view state of one request out of its query string: the date
/// range, the filters and the banner text of a value the page cannot use.
///
/// Query deserialization cannot fail with string fields, but the rejection
/// path is kept for robustness, and a rejected query then renders the default
/// range and no filter behind the banner. `with_status_class` tells the
/// resolution which page is being rendered, and the Tokens page passes false
/// because it has no status filter to read a class from.
fn resolve_view(
    query: Result<Query<StatsQuery>, QueryRejection>,
    with_status_class: bool,
) -> (DateRange, Filters, Option<String>) {
    let (query, rejection) = match query {
        Ok(Query(query)) => (query, None),
        Err(rejection) => (StatsQuery::default(), Some(rejection.body_text())),
    };
    let (range, range_error) = resolve_range(&query);
    let (filters, filter_error) = resolve_filters(&query, with_status_class);
    (range, filters, rejection.or(range_error).or(filter_error))
}

/// GET /admin/ui/stats: the usage table and the token and cost charts for one
/// date range, under the key and model filters of the URL.
pub async fn tokens_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    _auth: UiAuth,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> TokensPage {
    let (range, filters, error) = resolve_view(query, false);
    let theme = stats_chart::theme_from_headers(&headers);
    match load_tokens(&state, &range, &filters).await {
        Ok(data) => build_tokens_page(data, &filters, error, theme),
        Err(message) => tokens_shell(&range, &filters, Some(message)),
    }
}

/// GET /admin/ui/stats/requests: the requests chart and the status code table
/// for one date range, under the key, model and status class filters of the
/// URL, which narrow the whole page.
pub async fn requests_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    _auth: UiAuth,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> RequestsPage {
    let (range, filters, error) = resolve_view(query, true);
    let theme = stats_chart::theme_from_headers(&headers);
    match load_requests(&state, &range, &filters).await {
        Ok(data) => build_requests_page(data, &filters, error, theme),
        Err(message) => requests_shell(&range, &filters, Some(message)),
    }
}

/// Read-only routes for the `/admin/ui` nest; nothing here mutates, so the
/// CSRF guard layered over the nest has nothing to reject.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats", get(tokens_page))
        .route("/stats/requests", get(requests_page))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A day of the calendar, for the range and shortcut tests.
    fn date(year: i32, month: u8, day: u8) -> Date {
        let month = time::Month::try_from(month).expect("a month of the calendar");
        Date::from_calendar_date(year, month, day).expect("a day of the calendar")
    }

    /// An inclusive day range.
    fn span(from: Date, to: Date) -> DateRange {
        DateRange { from, to }
    }

    /// The whole of November 2026, the range most table tests render.
    fn november() -> DateRange {
        span(date(2026, 11, 1), date(2026, 11, 30))
    }

    /// One day of November 2026, the shortest range of the hourly tests.
    fn one_day() -> DateRange {
        span(date(2026, 11, 15), date(2026, 11, 15))
    }

    /// One day-level row as the day aggregate hands it over, with one
    /// request and no cache counts unless a test sets them.
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

    /// The same row for an explicit key.
    fn day_row_for(key_id: i64, day: &str, model: &str, prompt: i64, completion: i64) -> DayUsage {
        DayUsage {
            key_id,
            ..day_row(day, model, prompt, completion)
        }
    }

    /// One hour-level row as the hourly aggregate hands it over, with one
    /// request and no cache counts unless a test sets them.
    fn hour_row(hour: &str, model: &str, prompt: i64, completion: i64) -> HourUsage {
        HourUsage {
            hour: hour.to_string(),
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

    /// Loaded data for one range: the day rows the table and the charts read,
    /// one stored key bound to the priced channel and nothing else.
    fn tokens_data(days: Vec<DayUsage>, channels: HashMap<i64, ChannelInfo>) -> TokensData {
        tokens_data_for(vec![stored_key(None)], days, channels)
    }

    /// The same with an explicit key list, for the tests that need a second
    /// key or a key carrying a limit.
    fn tokens_data_for(
        keys: Vec<ApiKey>,
        days: Vec<DayUsage>,
        channels: HashMap<i64, ChannelInfo>,
    ) -> TokensData {
        TokensData {
            range: november(),
            keys,
            channels,
            usage: UsageRows::Daily(days),
            models: Vec::new(),
        }
    }

    /// Loaded data for a one-day range, the shortest range that plots hours:
    /// the hour rows the table and the charts read, one stored key bound to
    /// the priced channel and nothing else.
    fn hourly_tokens_data(
        hours: Vec<HourUsage>,
        channels: HashMap<i64, ChannelInfo>,
    ) -> TokensData {
        TokensData {
            range: one_day(),
            keys: vec![stored_key(None)],
            channels,
            usage: UsageRows::Hourly(hours),
            models: Vec::new(),
        }
    }

    /// Loaded data for the Requests page: the request aggregates of the range
    /// and the key list the filter offers.
    fn requests_data(
        requests: RequestRows,
        status_code_totals: Vec<StatusCodeTotal>,
    ) -> RequestsData {
        RequestsData {
            range: november(),
            keys: vec![stored_key(None)],
            requests,
            latency: Vec::new(),
            status_code_totals,
            models: Vec::new(),
        }
    }

    /// The same for a one-day range, with the hour rows and the latency rows
    /// the two charts of that page read.
    fn hourly_requests_data(
        requests: Vec<HourStatusCodeUsage>,
        latency: Vec<HourLatency>,
        status_code_totals: Vec<StatusCodeTotal>,
    ) -> RequestsData {
        RequestsData {
            range: one_day(),
            keys: vec![stored_key(None)],
            requests: RequestRows::Hourly(requests),
            latency,
            status_code_totals,
            models: Vec::new(),
        }
    }

    #[test]
    fn a_missing_or_blank_range_resolves_to_the_month_to_date() {
        let today = today_utc();
        let (range, error) = resolve_range(&StatsQuery::default());
        assert!(error.is_none());
        assert_eq!(range.to, today, "an empty to is today");
        assert_eq!(range.from, month_start(today), "an empty from is the 1st");

        let blank = StatsQuery {
            from: Some("  ".to_string()),
            to: Some(String::new()),
            ..StatsQuery::default()
        };
        assert_eq!(resolve_range(&blank).0, range);
        assert!(resolve_range(&blank).1.is_none());
    }

    #[test]
    fn a_parseable_range_is_used_verbatim_and_a_bad_day_falls_back() {
        let query = StatsQuery {
            from: Some(" 2026-01-05 ".to_string()),
            to: Some("2026-02-09".to_string()),
            ..StatsQuery::default()
        };
        let (range, error) = resolve_range(&query);
        assert_eq!(range, span(date(2026, 1, 5), date(2026, 2, 9)));
        assert!(error.is_none());

        // One bad bound is enough to fall back to the default range, and the
        // banner names the parameter and the value.
        let query = StatsQuery {
            from: Some("January".to_string()),
            to: Some("2026-02-09".to_string()),
            ..StatsQuery::default()
        };
        let (range, error) = resolve_range(&query);
        assert_eq!(range, DateRange::month_to_date(today_utc()));
        let error = error.expect("a malformed day is reported");
        assert!(error.contains("from \"January\""), "{error}");
        assert!(error.contains("YYYY-MM-DD"), "{error}");

        let query = StatsQuery {
            to: Some("2026-13-01".to_string()),
            ..StatsQuery::default()
        };
        let (range, error) = resolve_range(&query);
        assert_eq!(range, DateRange::month_to_date(today_utc()));
        let error = error.expect("a malformed day is reported");
        assert!(error.contains("to \"2026-13-01\""), "{error}");
    }

    #[test]
    fn a_from_after_to_falls_back_to_the_default_range_behind_a_banner() {
        let query = StatsQuery {
            from: Some("2026-06-09".to_string()),
            to: Some("2026-06-01".to_string()),
            ..StatsQuery::default()
        };
        let (range, error) = resolve_range(&query);
        assert_eq!(
            range,
            DateRange::month_to_date(today_utc()),
            "an inverted range renders the default one"
        );
        let error = error.expect("an inverted range is reported");
        assert!(error.contains("2026-06-09"), "{error}");
        assert!(error.contains("2026-06-01"), "{error}");
        assert!(error.contains("is after"), "{error}");

        // The same day on both sides is a one-day range, not an inversion.
        let query = StatsQuery {
            from: Some("2026-06-09".to_string()),
            to: Some("2026-06-09".to_string()),
            ..StatsQuery::default()
        };
        let (range, error) = resolve_range(&query);
        assert_eq!(range, span(date(2026, 6, 9), date(2026, 6, 9)));
        assert!(error.is_none());
    }

    #[test]
    fn the_to_day_is_inclusive_so_the_bound_is_the_first_instant_after_it() {
        let one_day = span(date(2026, 11, 15), date(2026, 11, 15));
        assert_eq!(one_day.start(), "2026-11-15 00:00:00");
        assert_eq!(
            one_day.end(),
            "2026-11-16 00:00:00",
            "a request of the to day, at any hour, stays below the bound"
        );
        assert_eq!(one_day.label(), "2026-11-15 to 2026-11-15");

        // The bound steps over the end of a month and over the end of a year.
        assert_eq!(
            span(date(2026, 2, 28), date(2026, 2, 28)).end(),
            "2026-03-01 00:00:00"
        );
        assert_eq!(
            span(date(2026, 12, 30), date(2026, 12, 31)).end(),
            "2027-01-01 00:00:00"
        );

        // The last day of the supported calendar has no day after it.
        let last = span(date(9999, 12, 31), date(9999, 12, 31));
        assert_eq!(last.end(), "9999-12-31 24:00:00");
    }

    #[test]
    fn parse_day_reads_the_calendar_shape_and_nothing_else() {
        assert_eq!(parse_day("2026-11-15"), Some(date(2026, 11, 15)));
        assert_eq!(parse_day("2024-02-29"), Some(date(2024, 2, 29)));
        for raw in [
            "",
            "2026",
            "2026-11",
            "26-11-15",
            "2026-1-5",
            "2026-11-5",
            "2026-11-15x",
            "2026-13-01",
            "2026-00-10",
            "2026-11-00",
            "2026-11-31",
            "2023-02-29",
            "January",
        ] {
            assert_eq!(parse_day(raw), None, "{raw} must not parse");
        }

        // The parameters are trimmed before they are read, so whitespace
        // around a valid day is not an error on the page.
        let query = StatsQuery {
            from: Some(" 2026-11-15 ".to_string()),
            ..StatsQuery::default()
        };
        assert_eq!(resolve_range(&query).0.from, date(2026, 11, 15));
        assert!(resolve_range(&query).1.is_none());
    }

    #[test]
    fn the_shortcuts_resolve_to_the_current_day_week_and_month() {
        // 2026-11-15 is a Sunday, the last day of its ISO week.
        let sunday = date(2026, 11, 15);
        assert_eq!(
            preset_range(PRESET_TODAY, sunday),
            Some(span(sunday, sunday))
        );
        assert_eq!(
            preset_range(PRESET_WEEK, sunday),
            Some(span(date(2026, 11, 9), sunday)),
            "the week runs from its Monday through its Sunday"
        );
        assert_eq!(
            preset_range(PRESET_MONTH, sunday),
            Some(span(date(2026, 11, 1), date(2026, 11, 30)))
        );

        // A Monday is the first day of its own week, and a week that spans a
        // month boundary keeps its Monday.
        let monday = date(2026, 11, 9);
        assert_eq!(
            preset_range(PRESET_WEEK, monday),
            Some(span(monday, date(2026, 11, 15)))
        );
        assert_eq!(
            preset_range(PRESET_WEEK, date(2026, 12, 1)),
            Some(span(date(2026, 11, 30), date(2026, 12, 6)))
        );

        // The month shortcut always covers the whole month, the days that
        // have not started included, and it follows the length of February.
        assert_eq!(
            preset_range(PRESET_MONTH, date(2026, 2, 3)),
            Some(span(date(2026, 2, 1), date(2026, 2, 28)))
        );
        assert_eq!(
            preset_range(PRESET_MONTH, date(2024, 2, 3)),
            Some(span(date(2024, 2, 1), date(2024, 2, 29)))
        );

        assert_eq!(preset_range("yesterday", sunday), None);
    }

    #[test]
    fn a_shortcut_wins_over_the_date_fields() {
        let today = today_utc();
        let query = StatsQuery {
            preset: Some(PRESET_MONTH.to_string()),
            from: Some("2020-01-01".to_string()),
            to: Some("2020-01-02".to_string()),
            ..StatsQuery::default()
        };
        let (range, error) = resolve_range(&query);
        assert_eq!(range, span(month_start(today), month_end(today)));
        assert!(error.is_none());
    }

    #[test]
    fn an_unknown_shortcut_shows_a_banner_over_the_default_range() {
        let query = StatsQuery {
            preset: Some("yesterday".to_string()),
            ..StatsQuery::default()
        };
        let (range, error) = resolve_range(&query);
        assert_eq!(range, DateRange::month_to_date(today_utc()));
        let error = error.expect("an unknown shortcut is reported");
        assert!(error.contains("yesterday"), "{error}");
        assert!(
            error.contains("today, week, month"),
            "the banner lists the accepted shortcuts: {error}"
        );
    }

    #[test]
    fn every_shortcut_button_submits_a_shortcut_the_handler_reads() {
        let buttons = preset_buttons();
        let values: Vec<&str> = buttons.iter().map(|button| button.value).collect();
        assert_eq!(values, vec!["today", "week", "month"]);
        let labels: Vec<&str> = buttons.iter().map(|button| button.label).collect();
        assert_eq!(labels, vec!["Today", "Current week", "Current month"]);
        for button in &buttons {
            assert!(
                preset_range(button.value, date(2026, 11, 15)).is_some(),
                "{} must resolve",
                button.value
            );
        }
        assert_eq!(preset_values(), "today, week, month");
    }

    #[test]
    fn the_month_helpers_read_the_calendar() {
        assert_eq!(month_start(date(2026, 11, 15)), date(2026, 11, 1));
        assert_eq!(month_end(date(2026, 11, 15)), date(2026, 11, 30));
        assert_eq!(month_end(date(2026, 12, 5)), date(2026, 12, 31));
        assert_eq!(month_end(date(2026, 2, 10)), date(2026, 2, 28));
        assert_eq!(
            month_end(date(2024, 2, 10)),
            date(2024, 2, 29),
            "a leap February has one more day"
        );
        assert_eq!(day_text(date(2026, 11, 5)), "2026-11-05");
        assert_eq!(week_start(date(2026, 11, 15)), date(2026, 11, 9));
        assert_eq!(week_start(date(2026, 11, 9)), date(2026, 11, 9));
    }

    #[test]
    fn filters_read_blank_values_as_no_filter_and_report_a_bad_key_id() {
        let (filters, error) = resolve_filters(&StatsQuery::default(), true);
        assert_eq!(filters, Filters::default());
        assert!(error.is_none());

        let query = StatsQuery {
            key_id: Some("  ".to_string()),
            model: Some("   ".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query, true);
        assert_eq!(filters, Filters::default());
        assert!(!filters.is_set());
        assert!(error.is_none());

        let query = StatsQuery {
            key_id: Some(" 7 ".to_string()),
            model: Some(" gpt-4o ".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query, true);
        assert_eq!(filters.key_id, Some(7));
        assert_eq!(filters.model.as_deref(), Some("gpt-4o"));
        assert!(filters.is_set());
        assert!(error.is_none());

        let query = StatsQuery {
            key_id: Some("abc".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query, true);
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
        let (filters, error) = resolve_filters(&query, true);
        assert_eq!(filters.status_class, Some(StatusBucket::Server5xx));
        assert!(filters.is_set());
        assert!(error.is_none());

        let query = StatsQuery {
            status_class: Some("errors".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query, true);
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
        let (filters, error) = resolve_filters(&query, true);
        assert_eq!(filters, Filters::default());
        assert!(error.is_none());

        // The page without a status filter reads no class at all, so the
        // same value is neither a filter nor a banner there.
        let query = StatsQuery {
            status_class: Some("errors".to_string()),
            ..StatsQuery::default()
        };
        let (filters, error) = resolve_filters(&query, false);
        assert_eq!(filters, Filters::default());
        assert!(error.is_none(), "an unread parameter is not reported");
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
            day_group(&DayUsage {
                model: None,
                ..day_row("2026-11-02", "-", 1, 1)
            }),
            day_group(&day_row("2026-11-02", "gpt-4o", 1, 1)),
            day_group(&day_row("2026-11-02", "claude-3", 1, 1)),
        ];
        order.sort_by(|left, right| sort_key(&left.model).cmp(&sort_key(&right.model)));
        let names: Vec<Option<&str>> = order.iter().map(|group| group.model.as_deref()).collect();
        assert_eq!(names, vec![Some("claude-3"), Some("gpt-4o"), None]);
    }

    #[test]
    fn a_keys_groups_fold_only_its_own_day_rows() {
        let days = vec![
            day_row("2026-11-02", "gpt-4o", 1000, 200),
            day_row_for(2, "2026-11-02", "gpt-4o", 10, 1),
            day_row("2026-11-03", "gpt-4o", 500, 100),
            day_row("2026-11-03", "gpt-5-mini", 7, 1),
        ];
        let mut groups = key_groups(1, &days);
        groups.sort_by(|left, right| sort_key(&left.model).cmp(&sort_key(&right.model)));
        assert_eq!(groups.len(), 2, "one group per model of the key");
        assert_eq!(groups[0].model.as_deref(), Some("gpt-4o"));
        assert_eq!(groups[0].prompt_tokens, 1500, "the days of the model sum");
        assert_eq!(groups[0].completion_tokens, 300);
        assert_eq!(groups[0].requests, 2);
        assert_eq!(groups[1].model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(groups[1].prompt_tokens, 7);

        assert_eq!(
            key_groups(2, &days).len(),
            1,
            "the other key's rows stay out of this key's groups"
        );
        assert!(key_groups(9, &days).is_empty());
    }

    #[test]
    fn a_page_builds_one_row_per_model_plus_a_key_total() {
        let data = tokens_data(
            vec![
                day_row("2026-11-02", "gpt-4o", 1000, 200),
                day_row("2026-11-03", "gpt-5-mini", 500, 100),
            ],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert_eq!(page.rows.len(), 3, "two models plus the total line");
        assert_eq!(page.section, "tokens");
        assert_eq!(page.range_label, "2026-11-01 to 2026-11-30");
        assert_eq!(page.from_value, "2026-11-01");
        assert_eq!(page.to_value, "2026-11-30");
        assert!(
            page.charts.is_some(),
            "the day rows of the table draw the charts"
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
        assert_eq!(total.sums.requests, "2");
        assert_eq!(total.key_label, "hyd-4f9dEg2x");
        assert!(!total.limit_inactive);

        assert_eq!(page.grand.cost, "$0.0045");
        assert_eq!(page.grand.prompt_tokens, "1,500");
        assert_eq!(page.grand.cache_write_input_tokens, "0");
        assert_eq!(page.grand.requests, "2");
    }

    #[test]
    fn a_cache_write_group_shows_its_token_column_and_is_priced_at_the_write_rate() {
        // An Anthropic row: 1300 prompt tokens (1000 uncached, 250 read, 50
        // written) and 200 completions. At 2.50/1.25/3.125/10.00 USD per 1M
        // tokens the pinned formula charges 1000 * 2.50 + 250 * 1.25 +
        // 50 * 3.125 + 200 * 10.00 = 4968.75 micro-USD, floored to 4968.
        let mut day = day_row("2026-11-02", "claude", 1300, 200);
        day.cached_input_tokens = 250;
        day.cache_write_input_tokens = 50;
        let data = tokens_data(vec![day], HashMap::from([(7, channel_info_write_priced())]));
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
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
    fn a_second_key_gets_its_own_block_and_the_grand_total_sums_both() {
        let mut second = stored_key(None);
        second.id = 2;
        second.display_prefix = "hyd-2b2b2b2b".to_string();
        let data = tokens_data_for(
            vec![stored_key(None), second],
            vec![
                day_row("2026-11-02", "gpt-4o", 1000, 200),
                day_row_for(2, "2026-11-02", "gpt-4o", 10, 1),
            ],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert_eq!(page.rows.len(), 4, "one model line and one total per key");
        assert_eq!(page.rows[0].key_label, "hyd-4f9dEg2x");
        assert_eq!(page.rows[0].sums.cost, "$0.0045");
        assert_eq!(page.rows[1].key_label, "hyd-4f9dEg2x");
        assert!(page.rows[1].is_total);
        assert_eq!(page.rows[2].key_label, "hyd-2b2b2b2b");
        assert_eq!(
            page.rows[2].sums.cost, "$0.000035",
            "10 prompt tokens at 2.50 and 1 completion at 10.00 per 1M"
        );
        assert_eq!(page.rows[3].sums.prompt_tokens, "10");
        assert_eq!(page.grand.prompt_tokens, "1,010");
        assert_eq!(page.grand.cost, "$0.004535");
    }

    #[test]
    fn a_key_without_usage_in_the_range_is_omitted_entirely() {
        let data = tokens_data(Vec::new(), HashMap::from([(7, channel_info_limitable())]));
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert!(page.rows.is_empty());
        assert_eq!(page.grand.cost, "$0.00");
        assert!(page.empty_message.contains("2026-11-01"));
        assert!(page.empty_message.contains("2026-11-30"));
        assert!(
            !page.empty_message.contains("filters"),
            "an unfiltered empty range keeps the plain wording"
        );
        assert!(
            page.charts.is_none(),
            "a range with no day rows draws no chart"
        );
    }

    #[test]
    fn an_empty_view_under_filters_says_the_filters_are_the_reason() {
        let data = tokens_data(Vec::new(), HashMap::from([(7, channel_info_limitable())]));
        let page = build_tokens_page(
            data,
            &Filters {
                key_id: Some(9),
                ..Filters::default()
            },
            None,
            stats_chart::ChartTheme::Light,
        );
        assert!(page.rows.is_empty());
        assert!(page
            .empty_message
            .contains("No usage matches the current filters"));
    }

    #[test]
    fn a_limited_key_on_an_unlimitable_channel_gets_the_warning_badge() {
        let mut unlimitable = channel_info_limitable();
        unlimitable.limitable = false;
        let data = tokens_data_for(
            vec![stored_key(Some(50_000_000))],
            vec![day_row("2026-11-02", "gpt-4o", 1000, 200)],
            HashMap::from([(7, unlimitable)]),
        );
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        let total = page.rows.last().unwrap();
        assert!(total.is_total);
        assert!(total.limit_inactive, "the total line carries the warning");

        // Without a stored limit there is nothing to warn about, however
        // unpriced the channel is.
        let mut unlimitable = channel_info_limitable();
        unlimitable.limitable = false;
        let data = tokens_data_for(
            vec![stored_key(None)],
            vec![day_row("2026-11-02", "gpt-4o", 1000, 200)],
            HashMap::from([(7, unlimitable)]),
        );
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert!(!page.rows.last().unwrap().limit_inactive);
    }

    #[test]
    fn a_key_bound_to_a_channel_that_vanished_costs_nothing_and_warns() {
        let data = tokens_data_for(
            vec![stored_key(Some(50_000_000))],
            vec![day_row("2026-11-02", "gpt-4o", 1000, 200)],
            HashMap::new(),
        );
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        let total = page.rows.last().unwrap();
        assert_eq!(total.sums.cost, "$0.00");
        assert!(total.limit_inactive);
    }

    #[test]
    fn a_deleted_key_keeps_its_rows_and_is_marked() {
        let mut key = stored_key(None);
        key.deleted_at = Some("2026-11-02 00:00:00".to_string());
        let data = tokens_data_for(
            vec![key],
            vec![day_row("2026-11-02", "gpt-4o", 1000, 200)],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert!(page.rows.iter().all(|row| row.key_deleted));
        assert_eq!(page.rows.len(), 2);
    }

    #[test]
    fn the_key_select_lists_every_key_of_the_page_with_the_selected_one_marked() {
        let mut deleted = stored_key(None);
        deleted.id = 2;
        deleted.display_prefix = "hyd-9a9a9a9a".to_string();
        deleted.deleted_at = Some("2026-11-03 00:00:00".to_string());
        let keys = vec![stored_key(None), deleted];

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
    fn the_model_select_keeps_a_selected_model_the_range_has_no_rows_for() {
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
    fn the_charts_render_for_a_range_with_day_rows_and_price_a_day_like_the_table() {
        // 1000 prompt tokens with 400 of them cached and 200 completions,
        // which the pinned formula prices at 4000 micro-USD.
        let mut day = day_row("2026-11-02", "gpt-4o", 1000, 200);
        day.cached_input_tokens = 400;
        let mut data = tokens_data(vec![day], HashMap::from([(7, channel_info_limitable())]));
        data.models = vec!["gpt-4o".to_string()];

        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
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
        assert_eq!(page.rows[0].sums.cost, "$0.004");
    }

    #[test]
    fn an_hourly_range_folds_its_hours_into_the_model_groups_of_their_key() {
        let rows = UsageRows::Hourly(vec![
            hour_row("2026-11-15 10", "gpt-4o", 1000, 200),
            hour_row("2026-11-15 11", "gpt-4o", 500, 100),
            hour_row("2026-11-15 11", "gpt-5-mini", 7, 1),
        ]);
        assert!(!rows.is_empty());
        let mut groups = rows.key_groups(1);
        groups.sort_by(|left, right| sort_key(&left.model).cmp(&sort_key(&right.model)));
        assert_eq!(groups.len(), 2, "one group per model of the key");
        assert_eq!(groups[0].model.as_deref(), Some("gpt-4o"));
        assert_eq!(groups[0].prompt_tokens, 1500, "the hours of the model sum");
        assert_eq!(groups[0].completion_tokens, 300);
        assert_eq!(groups[0].requests, 2);
        assert_eq!(groups[1].model.as_deref(), Some("gpt-5-mini"));
        assert!(
            rows.key_groups(9).is_empty(),
            "another key has no hours here"
        );
        assert!(UsageRows::Hourly(Vec::new()).is_empty());
    }

    #[test]
    fn a_short_range_charts_its_hours_and_prices_an_hour_like_the_table() {
        // 1000 prompt tokens with 400 of them cached and 200 completions in
        // the ten o'clock hour, which the pinned formula prices at 4000
        // micro-USD.
        let mut hour = hour_row("2026-11-15 10", "gpt-4o", 1000, 200);
        hour.cached_input_tokens = 400;
        let data = hourly_tokens_data(vec![hour], HashMap::from([(7, channel_info_limitable())]));

        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert_eq!(
            page.granularity,
            Granularity::Hourly,
            "a one day range plots hours"
        );
        assert_eq!(page.from_value, "2026-11-15");
        assert_eq!(page.to_value, "2026-11-15");
        let plot = page.charts.expect("hour rows draw both charts");
        assert!(
            plot.tokens_svg.contains("aria-label=\"Tokens per hour\""),
            "{}",
            plot.tokens_svg
        );
        assert!(
            plot.tokens_svg
                .contains("10:00: 1,200 tokens (1,000 in, 200 out) in 1 requests"),
            "the token bar carries the hour's figures: {}",
            plot.tokens_svg
        );
        assert!(
            plot.cost_svg.contains("aria-label=\"Cost per hour\""),
            "{}",
            plot.cost_svg
        );
        assert!(
            plot.cost_svg.contains("10:00: $0.004"),
            "the cached share is priced at the cached rate in an hour too: {}",
            plot.cost_svg
        );
        assert_eq!(
            page.grand.cost, "$0.004",
            "the chart and the table price the same rows the same way"
        );
        assert_eq!(page.rows[0].sums.cost, "$0.004");
    }

    #[test]
    fn a_long_range_keeps_the_day_charts_of_the_same_page() {
        let data = tokens_data(
            vec![day_row("2026-11-02", "gpt-4o", 1000, 200)],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_tokens_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert_eq!(
            page.granularity,
            Granularity::Daily,
            "a month range keeps one bar per day"
        );
        let plot = page.charts.expect("day rows draw both charts");
        assert!(
            plot.tokens_svg.contains("aria-label=\"Tokens per day\""),
            "{}",
            plot.tokens_svg
        );
        assert!(
            plot.cost_svg.contains("aria-label=\"Cost per day\""),
            "{}",
            plot.cost_svg
        );
    }

    #[test]
    fn the_tokens_page_carries_no_status_content() {
        let data = tokens_data(
            vec![day_row("2026-11-02", "gpt-4o", 1000, 200)],
            HashMap::from([(7, channel_info_limitable())]),
        );
        let page = build_tokens_page(
            data,
            &Filters {
                status_class: Some(StatusBucket::Server5xx),
                ..Filters::default()
            },
            None,
            stats_chart::ChartTheme::Light,
        );
        let plot = page
            .charts
            .expect("the day rows draw the token and cost charts");
        assert!(plot.tokens_svg.contains("aria-label=\"Tokens per day\""));
        assert!(plot.cost_svg.contains("aria-label=\"Cost per day\""));
        assert_eq!(page.section, "tokens");
        assert!(
            !page.model_options.is_empty(),
            "the page keeps its own selects"
        );
    }

    #[test]
    fn the_requests_page_renders_the_chart_and_the_table_of_the_same_rows() {
        let data = requests_data(
            RequestRows::Daily(vec![DayStatusCodeUsage {
                day: "2026-11-02".to_string(),
                status: Some(500),
                status_class: StatusBucket::Server5xx,
                requests: 2,
            }]),
            vec![
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
            ],
        );
        let page = build_requests_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert_eq!(page.section, "requests");
        assert_eq!(page.range_label, "2026-11-01 to 2026-11-30");
        let svg = page
            .status_svg
            .as_ref()
            .expect("status rows draw the requests chart");
        assert!(svg.contains("aria-label=\"Requests per day\""), "{svg}");
        assert!(svg.contains("Day 2: 2 requests (2 500)"), "{svg}");
        // The table of the same view lists both codes, most requests first.
        assert_eq!(page.status_codes.len(), 2);
        assert_eq!(page.status_codes[0].code, "500");
        assert_eq!(page.status_codes[0].share, "66.7%");
        assert_eq!(page.status_codes[1].code, "n/a");
        assert_eq!(page.status_codes[1].class, "other");
        assert_eq!(page.status_codes[1].share, "33.3%");
        assert!(
            page.empty_message.is_empty(),
            "a page with content shows no empty state"
        );
        assert_eq!(page.status_options.len(), 5);
    }

    #[test]
    fn a_short_requests_range_charts_hours_and_the_latency_of_the_same_buckets() {
        let data = hourly_requests_data(
            vec![HourStatusCodeUsage {
                hour: "2026-11-15 10".to_string(),
                status: Some(500),
                status_class: StatusBucket::Server5xx,
                requests: 2,
            }],
            vec![HourLatency {
                hour: "2026-11-15 10".to_string(),
                avg_duration_ms: 12.0,
                requests: 2,
            }],
            vec![StatusCodeTotal {
                status: Some(500),
                status_class: StatusBucket::Server5xx,
                requests: 2,
            }],
        );
        let page = build_requests_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert_eq!(page.granularity, Granularity::Hourly);
        let svg = page
            .status_svg
            .as_ref()
            .expect("hour rows draw the requests chart");
        assert!(svg.contains("aria-label=\"Requests per hour\""), "{svg}");
        assert!(svg.contains("10:00: 2 requests (2 500)"), "{svg}");
        let latency = page
            .latency_svg
            .as_ref()
            .expect("latency rows draw the latency chart");
        assert!(
            latency.contains("aria-label=\"Latency per hour\""),
            "{latency}"
        );
        assert!(
            latency.contains("10:00: 12.0 ms (2 requests)"),
            "the latency bar carries the mean of its bucket: {latency}"
        );
        assert_eq!(page.status_codes.len(), 1);
        assert!(
            page.empty_message.is_empty(),
            "a page with content shows no empty state"
        );
    }

    #[test]
    fn a_long_requests_range_folds_the_latency_rows_onto_its_days() {
        let mut data = requests_data(
            RequestRows::Daily(vec![DayStatusCodeUsage {
                day: "2026-11-02".to_string(),
                status: Some(200),
                status_class: StatusBucket::Ok2xx,
                requests: 1,
            }]),
            vec![StatusCodeTotal {
                status: Some(200),
                status_class: StatusBucket::Ok2xx,
                requests: 1,
            }],
        );
        data.latency = vec![
            HourLatency {
                hour: "2026-11-02 10".to_string(),
                avg_duration_ms: 10.0,
                requests: 1,
            },
            HourLatency {
                hour: "2026-11-02 11".to_string(),
                avg_duration_ms: 20.0,
                requests: 3,
            },
        ];
        let page = build_requests_page(
            data,
            &Filters::default(),
            None,
            stats_chart::ChartTheme::Light,
        );
        assert_eq!(page.granularity, Granularity::Daily);
        let latency = page
            .latency_svg
            .as_ref()
            .expect("latency rows draw the latency chart");
        assert!(
            latency.contains("aria-label=\"Latency per day\""),
            "a month range plots latency per day: {latency}"
        );
        // The hours of the day weigh by the timed requests each covers:
        // (10 * 1 + 20 * 3) / 4 = 17.5 ms on the second day of the month.
        assert!(latency.contains("Day 2: 17.5 ms (4 requests)"), "{latency}");
    }

    #[test]
    fn a_requests_page_without_a_matching_request_drops_both_panels() {
        // A status filter that selects nothing leaves the status rows empty,
        // which drops the chart and the table, and the page says so.
        let page = build_requests_page(
            requests_data(RequestRows::Daily(Vec::new()), Vec::new()),
            &Filters {
                status_class: Some(StatusBucket::Client4xx),
                ..Filters::default()
            },
            None,
            stats_chart::ChartTheme::Light,
        );
        assert!(page.status_svg.is_none());
        assert!(page.status_codes.is_empty());
        assert!(
            page.empty_message
                .contains("No requests match the current filters"),
            "{}",
            page.empty_message
        );
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
    fn a_share_of_a_range_without_requests_reads_zero_instead_of_dividing() {
        assert_eq!(share_label(0, 0), "0.0%");
        assert_eq!(share_label(1, 3), "33.3%");
        assert_eq!(share_label(2, 3), "66.7%");
        assert_eq!(share_label(5, 5), "100.0%");
        assert_eq!(share_label(1_000, i64::MAX), "0.0%");
    }

    #[test]
    fn the_filter_widgets_show_the_range_and_the_shortcut_buttons() {
        let page = tokens_shell(&november(), &Filters::default(), None);
        assert_eq!(page.range_label, "2026-11-01 to 2026-11-30");
        assert_eq!(page.from_value, "2026-11-01");
        assert_eq!(page.to_value, "2026-11-30");
        let values: Vec<&str> = page
            .preset_buttons
            .iter()
            .map(|button| button.value)
            .collect();
        assert_eq!(values, vec!["today", "week", "month"]);
        assert!(page.empty_message.contains("2026-11-01"));
        assert!(!page.empty_message.contains("filters"));

        let filtered = tokens_shell(
            &november(),
            &Filters {
                key_id: Some(4),
                model: Some("gpt-4o".to_string()),
                status_class: Some(StatusBucket::Server5xx),
            },
            None,
        );
        assert!(filtered
            .empty_message
            .contains("No usage matches the current filters"));
    }

    #[test]
    fn the_requests_shell_offers_the_same_range_under_its_own_empty_state() {
        let page = requests_shell(&november(), &Filters::default(), None);
        assert_eq!(page.section, "requests");
        assert_eq!(page.range_label, "2026-11-01 to 2026-11-30");
        assert_eq!(page.from_value, "2026-11-01");
        assert_eq!(page.to_value, "2026-11-30");
        assert_eq!(page.preset_buttons.len(), 3);
        assert!(
            page.empty_message.contains("No requests logged"),
            "{}",
            page.empty_message
        );
        assert!(page.status_svg.is_none());
        assert!(page.status_codes.is_empty());

        let filtered = requests_shell(
            &november(),
            &Filters {
                status_class: Some(StatusBucket::Server5xx),
                ..Filters::default()
            },
            None,
        );
        assert!(filtered
            .empty_message
            .contains("No requests match the current filters"));
        assert!(
            filtered
                .status_options
                .iter()
                .any(|option| option.selected && option.value == "5xx"),
            "the class in force stays selected"
        );
    }

    #[test]
    fn the_failure_shell_keeps_the_filter_widgets_and_drops_the_empty_message() {
        let page = tokens_shell(
            &november(),
            &Filters::default(),
            Some("internal server error".into()),
        );
        assert!(page.rows.is_empty());
        assert_eq!(page.error.as_deref(), Some("internal server error"));
        assert!(page.empty_message.is_empty());
        assert!(page.charts.is_none());
        assert_eq!(page.range_label, "2026-11-01 to 2026-11-30");
        assert_eq!(
            page.key_options.len(),
            1,
            "the key select keeps its default"
        );
        assert!(page.key_options[0].selected);

        let page = requests_shell(
            &november(),
            &Filters::default(),
            Some("internal server error".into()),
        );
        assert_eq!(page.error.as_deref(), Some("internal server error"));
        assert!(page.empty_message.is_empty());
        assert!(page.status_svg.is_none());
        assert!(page.key_options[0].selected);
        assert!(page.status_options[0].selected);
        assert_eq!(page.status_options.len(), 5);
    }
}
