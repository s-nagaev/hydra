//! Admin UI logs pages: a read-only, server-rendered table over
//! `request_logs` with server-side pagination and filters, plus a
//! per-row drill-down page (`GET /admin/ui/logs/{id}`) that shows the
//! full stored picture of one request.
//!
//! The page reuses the exact query construction of the JSON endpoint
//! `GET /admin/logs` ([`build_filters`]), so the UI cannot drift from the
//! API semantics; the status-class filter lives on the JSON endpoint for
//! the same reason. Every piece of view state (page, page size, filters,
//! time preset) travels in the URL query string: the filter bar is a
//! standard GET form and pagination is plain links, so reloading, sharing
//! or going back restores the view with no JavaScript. There are no
//! mutating routes here.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::db::channels::{self, Channel};
use crate::db::keys::{self, ApiKey};
use crate::db::request_logs::{self, LogFilters, RequestLog};
use crate::http::admin_auth::AdminAuth;
use crate::http::admin_logs::{build_filters, LogsError, RawLogQuery, DEFAULT_PAGE_SIZE};
use crate::http::AppState;

/// Time presets: URL value, label, length in minutes. The URL keeps the
/// preset name and the `from` bound is computed at render time, so a
/// shared "last 24 hours" link stays relative to the moment it is opened.
const PRESETS: &[(&str, &str, i64)] = &[
    ("1h", "Last hour", 60),
    ("24h", "Last 24 hours", 24 * 60),
    ("7d", "Last 7 days", 7 * 24 * 60),
];
const PRESET_CUSTOM: &str = "custom";

/// Page sizes offered by the selector; the server clamps to 500 anyway.
const PAGE_SIZES: &[u64] = &[25, 50, 100, 250];

/// All view state as it arrives in the URL. Numeric fields stay strings so
/// an empty select value (`key_id=`) means "no filter" instead of failing
/// the whole page; they are parsed and validated in [`parse_ids`].
#[derive(Debug, Default, Deserialize)]
pub struct LogsQuery {
    page: Option<String>,
    page_size: Option<String>,
    key_id: Option<String>,
    channel_id: Option<String>,
    model: Option<String>,
    status_class: Option<String>,
    preset: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

/// One row of the rendered table; body content never reaches this view,
/// only the fact that bodies were captured. `detail_url` points at the
/// drill-down page and carries the whole list view state, so the back
/// link on that page restores the exact page and filters.
pub struct LogRow {
    pub id: i64,
    pub created_at: String,
    pub key_prefix: String,
    pub channel_name: String,
    pub model: Option<String>,
    pub status_text: String,
    pub status_css: String,
    pub duration_ms: i64,
    pub tokens: String,
    pub client_ip: Option<String>,
    pub error_full: Option<String>,
    pub error_shown: Option<String>,
    pub has_bodies: bool,
    pub detail_url: String,
}

/// One option of a filter select, with the preselection flag.
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// One entry of the pagination strip; `url: None` renders an ellipsis gap.
pub struct PageLink {
    pub label: String,
    pub url: Option<String>,
    pub current: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "logs.html")]
pub struct LogsPage {
    pub version: &'static str,
    pub section: &'static str,
    pub rows: Vec<LogRow>,
    pub total: i64,
    pub range_display: String,
    pub prev_url: Option<String>,
    pub next_url: Option<String>,
    pub page_links: Vec<PageLink>,
    pub key_options: Vec<SelectOption>,
    pub channel_options: Vec<SelectOption>,
    pub status_options: Vec<SelectOption>,
    pub preset_options: Vec<SelectOption>,
    pub page_size_options: Vec<SelectOption>,
    pub model_value: String,
    pub from_value: String,
    pub to_value: String,
    pub refresh_url: String,
    pub empty_message: String,
    pub error: Option<String>,
}

/// The drill-down page for one request log. Bodies arrive already
/// formatted (pretty JSON when parseable, raw text otherwise); Askama
/// escapes whatever is rendered, so no markup can become live HTML.
#[derive(Template, WebTemplate)]
#[template(path = "log_detail.html")]
pub struct LogDetailPage {
    pub version: &'static str,
    pub section: &'static str,
    pub id: i64,
    pub created_at: String,
    pub key_label: String,
    pub channel_label: String,
    pub model: Option<String>,
    pub status_text: String,
    pub status_css: String,
    pub tokens: String,
    pub duration_ms: i64,
    pub client_ip: Option<String>,
    pub log_error: Option<String>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub back_url: String,
    pub error: Option<String>,
}

fn internal_error(error: &sqlx::Error) -> String {
    tracing::error!(error = %error, "admin ui logs query failed");
    "internal server error".to_string()
}

fn trimmed(raw: Option<&String>) -> Option<&str> {
    raw.map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Numeric URL fields after trimming; `None` means "not in the URL".
struct ParsedIds {
    page: Option<u64>,
    page_size: Option<u64>,
    key_id: Option<i64>,
    channel_id: Option<i64>,
}

/// Parses the numeric URL fields; the message names the offending param.
fn parse_ids(query: &LogsQuery) -> Result<ParsedIds, String> {
    fn parse_u64(name: &str, raw: Option<&str>) -> Result<Option<u64>, String> {
        raw.map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("{name} must be a positive integer"))
        })
        .transpose()
    }
    fn parse_i64(name: &str, raw: Option<&str>) -> Result<Option<i64>, String> {
        raw.map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| format!("{name} must be a row id"))
        })
        .transpose()
    }
    Ok(ParsedIds {
        page: parse_u64("page", trimmed(query.page.as_ref()))?,
        page_size: parse_u64("page_size", trimmed(query.page_size.as_ref()))?,
        key_id: parse_i64("key_id", trimmed(query.key_id.as_ref()))?,
        channel_id: parse_i64("channel_id", trimmed(query.channel_id.as_ref()))?,
    })
}

/// Converts a `datetime-local` value (`YYYY-MM-DDTHH:MM[:SS]`) into the
/// RFC3339 form the shared filter builder parses. The input carries no
/// timezone, so it is read as UTC; the stored `created_at` is UTC too.
fn local_to_rfc3339(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    if text.len() == 16 {
        text.push_str(":00");
    }
    text.push('Z');
    text
}

/// Formats a UTC moment either as the stored timestamp text (a space
/// separator) or as a datetime-local input value (a `T` separator).
fn utc_text(moment: OffsetDateTime, separator: char) -> String {
    let utc = moment.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}{}{:02}:{:02}:{:02}",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        separator,
        utc.hour(),
        utc.minute(),
        utc.second()
    )
}

/// Resolves the preset and the custom bounds into the RFC3339 strings the
/// shared filter builder expects, plus the datetime-local values to echo
/// back into the inputs. An unknown preset is ignored (all time).
fn resolve_time_range(
    query: &LogsQuery,
) -> Result<(Option<String>, Option<String>, String, String), LogsError> {
    let preset_raw = query.preset.as_deref().unwrap_or("");
    if let Some((_, _, minutes)) = PRESETS.iter().find(|(value, _, _)| *value == preset_raw) {
        let from = OffsetDateTime::now_utc() - Duration::minutes(*minutes);
        return Ok((
            Some(format!("{}Z", utc_text(from, 'T'))),
            None,
            utc_text(from, 'T'),
            String::new(),
        ));
    }
    if preset_raw == PRESET_CUSTOM
        || trimmed(query.from.as_ref()).is_some()
        || trimmed(query.to.as_ref()).is_some()
    {
        let from_rfc = trimmed(query.from.as_ref()).map(local_to_rfc3339);
        let to_rfc = trimmed(query.to.as_ref()).map(local_to_rfc3339);
        let from_input = trimmed(query.from.as_ref())
            .map(str::to_string)
            .unwrap_or_default();
        let to_input = trimmed(query.to.as_ref())
            .map(str::to_string)
            .unwrap_or_default();
        return Ok((from_rfc, to_rfc, from_input, to_input));
    }
    Ok((None, None, String::new(), String::new()))
}

fn status_view(status: Option<i64>) -> (String, String) {
    match status {
        Some(value) => {
            let css = if (200..300).contains(&value) {
                "status-2xx"
            } else if (400..500).contains(&value) {
                "status-4xx"
            } else if (500..600).contains(&value) {
                "status-5xx"
            } else {
                "status-other"
            };
            (value.to_string(), css.to_string())
        }
        // The request never reached the upstream; the error column says why.
        None => ("err".to_string(), "status-err".to_string()),
    }
}

fn tokens_view(prompt: Option<i64>, completion: Option<i64>) -> String {
    match (prompt, completion) {
        (Some(prompt), Some(completion)) => format!("{prompt} / {completion}"),
        (Some(prompt), None) => format!("{prompt} / -"),
        (None, Some(completion)) => format!("- / {completion}"),
        (None, None) => "-".to_string(),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{cut}\u{2026}")
    }
}

/// The display label for the key of a log row. `None` means the request
/// was rejected before its key resolved (unknown key 401, lookup
/// failure): no key to name. An id missing from the live key list (the
/// key was since deleted) falls back to the bare row id.
fn key_label(key_names: &HashMap<i64, String>, api_key_id: Option<i64>) -> String {
    match api_key_id {
        Some(id) => key_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("#{id}")),
        None => "unknown key".to_string(),
    }
}

/// The display label for the channel of a log row, same conventions as
/// [`key_label`]; `channel_id` is NULL exactly when `api_key_id` is.
fn channel_label(channel_names: &HashMap<i64, String>, channel_id: Option<i64>) -> String {
    match channel_id {
        Some(id) => channel_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("#{id}")),
        None => "unknown channel".to_string(),
    }
}

/// Prepares a stored body for display: pretty-printed when it parses as
/// JSON, the stored text otherwise. Askama escapes the result, so hostile
/// markup stays inert on the page.
fn body_view(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| raw.to_string())
}

fn thousands(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Appends the whole list view state (filters plus the given page) to a
/// URL serializer. Shared by the pagination, refresh and detail links so
/// all of them carry the identical URL shape.
fn push_view_state(
    serializer: &mut url::form_urlencoded::Serializer<String>,
    query: &LogsQuery,
    page: u64,
) {
    let mut push = |name: &str, value: Option<&str>| {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            serializer.append_pair(name, value);
        }
    };
    push("page", Some(&page.to_string()));
    push("page_size", query.page_size.as_deref());
    push("key_id", query.key_id.as_deref());
    push("channel_id", query.channel_id.as_deref());
    push("model", query.model.as_deref());
    push("status_class", query.status_class.as_deref());
    push("preset", query.preset.as_deref());
    push("from", query.from.as_deref());
    push("to", query.to.as_deref());
}

/// Builds the URL for a pagination or refresh link, carrying the whole
/// filter state plus the given page. Empty values are dropped so shared
/// URLs stay clean.
fn view_url(query: &LogsQuery, page: u64) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    push_view_state(&mut serializer, query, page);
    format!("/admin/ui/logs?{}", serializer.finish())
}

/// Builds the drill-down URL for one row: the whole list view state (the
/// effective page included) rides along, so the back link on the detail
/// page restores the exact page and filters the user came from.
fn detail_url(query: &LogsQuery, page: u64, id: i64) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    push_view_state(&mut serializer, query, page);
    format!("/admin/ui/logs/{id}?{}", serializer.finish())
}

/// Everything the page needs from the database; the filters come back too,
/// carrying the effective page and page size after the shared clamping.
struct LogsData {
    rows: Vec<RequestLog>,
    total: i64,
    filters: LogFilters,
    key_list: Vec<ApiKey>,
    channel_list: Vec<Channel>,
}

/// Maps a validation failure to the banner message; the database variant
/// is logged inside the shared helpers already.
fn error_message(error: LogsError) -> String {
    match error {
        LogsError::BadRequest(message) => message,
        LogsError::NotFound => "request log not found".to_string(),
        LogsError::Database(e) => internal_error(&e),
    }
}

/// Loads one page of rows through the shared filter construction and the
/// key/channel lists for labels and selects, in one shot.
async fn load(state: &AppState, query: &LogsQuery) -> Result<LogsData, String> {
    let parsed = parse_ids(query)?;
    let (from, to, _, _) = resolve_time_range(query).map_err(error_message)?;
    let filters = build_filters(RawLogQuery {
        page: parsed.page,
        page_size: parsed.page_size,
        key_id: parsed.key_id,
        channel_id: parsed.channel_id,
        model: trimmed(query.model.as_ref()).map(str::to_string),
        status_class: query.status_class.clone(),
        from,
        to,
    })
    .map_err(error_message)?;

    let (rows, total) = request_logs::list(&state.db, &filters)
        .await
        .map_err(|e| internal_error(&e))?;
    let key_list = keys::list(&state.db)
        .await
        .map_err(|e| internal_error(&e))?;
    let channel_list = channels::list(&state.db)
        .await
        .map_err(|e| internal_error(&e))?;
    Ok(LogsData {
        rows,
        total,
        filters,
        key_list,
        channel_list,
    })
}

fn key_options(list: &[ApiKey], selected: Option<i64>) -> Vec<SelectOption> {
    let mut options = vec![SelectOption {
        value: String::new(),
        label: "All keys".to_string(),
        selected: selected.is_none(),
    }];
    options.extend(list.iter().map(|key| {
        let label = match &key.name {
            Some(name) => format!("{} {}", key.display_prefix, name),
            None => key.display_prefix.clone(),
        };
        SelectOption {
            value: key.id.to_string(),
            label,
            selected: Some(key.id) == selected,
        }
    }));
    options
}

fn channel_options(list: &[Channel], selected: Option<i64>) -> Vec<SelectOption> {
    let mut options = vec![SelectOption {
        value: String::new(),
        label: "All channels".to_string(),
        selected: selected.is_none(),
    }];
    options.extend(list.iter().map(|channel| SelectOption {
        value: channel.id.to_string(),
        label: channel.name.clone(),
        selected: Some(channel.id) == selected,
    }));
    options
}

fn status_options(selected: Option<&str>) -> Vec<SelectOption> {
    let options = [
        ("", "All statuses"),
        ("2xx", "2xx success"),
        ("4xx", "4xx client error"),
        ("5xx", "5xx server error"),
        ("errors", "All errors (incl. failures)"),
    ];
    options
        .into_iter()
        .map(|(value, label)| SelectOption {
            value: value.to_string(),
            label: label.to_string(),
            selected: Some(value) == selected.filter(|value| !value.is_empty()),
        })
        .collect()
}

fn preset_options(selected: &str) -> Vec<SelectOption> {
    let mut options = vec![SelectOption {
        value: String::new(),
        label: "All time".to_string(),
        selected: selected.is_empty(),
    }];
    options.extend(
        PRESETS
            .iter()
            .map(|(value, label, _)| SelectOption {
                value: (*value).to_string(),
                label: (*label).to_string(),
                selected: *value == selected,
            })
            .chain(std::iter::once(SelectOption {
                value: PRESET_CUSTOM.to_string(),
                label: "Custom range".to_string(),
                selected: selected == PRESET_CUSTOM,
            })),
    );
    options
}

fn page_size_options(selected: u64) -> Vec<SelectOption> {
    PAGE_SIZES
        .iter()
        .map(|size| SelectOption {
            value: size.to_string(),
            label: size.to_string(),
            selected: *size == selected,
        })
        .collect()
}

/// Pagination strip: a window of page numbers around the current one with
/// the first and last page always reachable, gaps rendered as ellipses.
fn pagination_links(current: u64, total_pages: u64, query: &LogsQuery) -> Vec<PageLink> {
    let window_start = current.saturating_sub(2).max(1);
    let window_end = current.saturating_add(2).min(total_pages).max(1);
    let mut links: Vec<PageLink> = Vec::new();
    let push_page = |links: &mut Vec<PageLink>, page: u64| {
        links.push(PageLink {
            label: page.to_string(),
            url: Some(view_url(query, page)),
            current: page == current,
        });
    };
    if window_start > 1 {
        push_page(&mut links, 1);
        if window_start > 2 {
            links.push(PageLink {
                label: "\u{2026}".to_string(),
                url: None,
                current: false,
            });
        }
    }
    for page in window_start..=window_end {
        push_page(&mut links, page);
    }
    if window_end < total_pages {
        if window_end + 1 < total_pages {
            links.push(PageLink {
                label: "\u{2026}".to_string(),
                url: None,
                current: false,
            });
        }
        push_page(&mut links, total_pages);
    }
    links
}

/// Renders the whole page from loaded data: rows, meta line, pagination
/// and the filter widgets reflecting the current URL.
fn build_page(query: &LogsQuery, data: LogsData, error: Option<String>) -> LogsPage {
    let parsed = match parse_ids(query) {
        Ok(parsed) => parsed,
        Err(message) => return fallback_page(&data.key_list, &data.channel_list, message),
    };
    let key_id = parsed.key_id;
    let channel_id = parsed.channel_id;
    let page = data.filters.page;
    let page_size = data.filters.page_size;
    let (_, _, from_value, to_value) =
        resolve_time_range(query).unwrap_or((None, None, String::new(), String::new()));
    let preset_raw = query.preset.as_deref().unwrap_or("");

    let key_names: HashMap<i64, String> = data
        .key_list
        .iter()
        .map(|key| {
            let label = match &key.name {
                Some(name) => format!("{} {}", key.display_prefix, name),
                None => key.display_prefix.clone(),
            };
            (key.id, label)
        })
        .collect();
    let channel_names: HashMap<i64, String> = data
        .channel_list
        .iter()
        .map(|channel| (channel.id, channel.name.clone()))
        .collect();

    let rows: Vec<LogRow> = data
        .rows
        .iter()
        .map(|log| {
            let (status_text, status_css) = status_view(log.status);
            let (error_full, error_shown) = match &log.error {
                Some(error) => (Some(error.clone()), Some(truncate(error, 80))),
                None => (None, None),
            };
            LogRow {
                id: log.id,
                created_at: log.created_at.clone(),
                key_prefix: key_label(&key_names, log.api_key_id),
                channel_name: channel_label(&channel_names, log.channel_id),
                model: log.model.clone(),
                status_text,
                status_css,
                duration_ms: log.duration_ms,
                tokens: tokens_view(log.prompt_tokens, log.completion_tokens),
                client_ip: log.client_ip.clone(),
                error_full,
                error_shown,
                has_bodies: log.request_body.is_some() || log.response_body.is_some(),
                detail_url: detail_url(query, page, log.id),
            }
        })
        .collect();

    let total = data.total;
    let total_pages = if total <= 0 {
        1
    } else {
        (total as u64).div_ceil(page_size)
    };
    let raw_start = page
        .saturating_sub(1)
        .saturating_mul(page_size)
        .saturating_add(1) as i64;
    let end = (page.saturating_mul(page_size) as i64).min(total);
    let range_display = if total == 0 || raw_start > total || raw_start > end {
        String::new()
    } else {
        format!(
            "{}\u{2013}{} of {}",
            thousands(raw_start),
            thousands(end),
            thousands(total)
        )
    };
    let is_filtered = [
        trimmed(query.key_id.as_ref()).is_some(),
        trimmed(query.channel_id.as_ref()).is_some(),
        trimmed(query.model.as_ref()).is_some(),
        trimmed(query.status_class.as_ref()).is_some(),
        !preset_raw.is_empty(),
        trimmed(query.from.as_ref()).is_some(),
        trimmed(query.to.as_ref()).is_some(),
    ]
    .into_iter()
    .any(|flag| flag);
    let empty_message = if total == 0 {
        if is_filtered {
            "No requests match the current filters.".to_string()
        } else {
            "No requests logged yet. Requests appear here as clients call \
             /v1/chat/completions; set HYDRA_LOG_BODIES=1 to also capture \
             request and response bodies."
                .to_string()
        }
    } else {
        "No rows on this page. Go back to an earlier page.".to_string()
    };

    let page_size_value = page_size.max(1);
    LogsPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "logs",
        rows,
        total,
        range_display,
        prev_url: (page > 1).then(|| view_url(query, page - 1)),
        next_url: (page < total_pages).then(|| view_url(query, page + 1)),
        page_links: pagination_links(page, total_pages, query),
        key_options: key_options(&data.key_list, key_id),
        channel_options: channel_options(&data.channel_list, channel_id),
        status_options: status_options(query.status_class.as_deref()),
        preset_options: preset_options(preset_raw),
        page_size_options: page_size_options(page_size_value),
        model_value: trimmed(query.model.as_ref())
            .map(str::to_string)
            .unwrap_or_default(),
        from_value,
        to_value,
        refresh_url: view_url(query, page),
        empty_message,
        error,
    }
}

/// Error path: keeps the filter widgets usable, shows the banner, no rows.
fn fallback_page(key_list: &[ApiKey], channel_list: &[Channel], message: String) -> LogsPage {
    LogsPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "logs",
        rows: Vec::new(),
        total: 0,
        range_display: String::new(),
        prev_url: None,
        next_url: None,
        page_links: Vec::new(),
        key_options: key_options(key_list, None),
        channel_options: channel_options(channel_list, None),
        status_options: status_options(None),
        preset_options: preset_options(""),
        page_size_options: page_size_options(DEFAULT_PAGE_SIZE),
        model_value: String::new(),
        from_value: String::new(),
        to_value: String::new(),
        refresh_url: "/admin/ui/logs".to_string(),
        empty_message: String::new(),
        error: Some(message),
    }
}

async fn render(state: &AppState, query: LogsQuery) -> LogsPage {
    match load(state, &query).await {
        Ok(data) => build_page(&query, data, None),
        Err(message) => {
            let key_list = keys::list(&state.db).await.unwrap_or_default();
            let channel_list = channels::list(&state.db).await.unwrap_or_default();
            fallback_page(&key_list, &channel_list, message)
        }
    }
}

/// GET /admin/ui/logs. Query deserialization cannot fail with all-string
/// fields, but the rejection path is kept for robustness.
pub async fn page(
    State(state): State<AppState>,
    _auth: AdminAuth,
    query: Result<Query<LogsQuery>, QueryRejection>,
) -> LogsPage {
    match query {
        Ok(Query(query)) => render(&state, query).await,
        Err(rejection) => {
            let key_list = keys::list(&state.db).await.unwrap_or_default();
            let channel_list = channels::list(&state.db).await.unwrap_or_default();
            fallback_page(&key_list, &channel_list, rejection.body_text())
        }
    }
}

/// Builds the drill-down page for one stored log row, resolving the
/// key/channel labels the same way the listing does.
async fn build_detail(state: &AppState, log: RequestLog, back_url: String) -> LogDetailPage {
    let key_list = keys::list(&state.db).await.unwrap_or_default();
    let channel_list = channels::list(&state.db).await.unwrap_or_default();
    let key_names: HashMap<i64, String> = key_list
        .iter()
        .map(|key| {
            let label = match &key.name {
                Some(name) => format!("{} {}", key.display_prefix, name),
                None => key.display_prefix.clone(),
            };
            (key.id, label)
        })
        .collect();
    let channel_names: HashMap<i64, String> = channel_list
        .iter()
        .map(|channel| (channel.id, channel.name.clone()))
        .collect();
    let (status_text, status_css) = status_view(log.status);
    LogDetailPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "logs",
        id: log.id,
        created_at: log.created_at,
        key_label: key_label(&key_names, log.api_key_id),
        channel_label: channel_label(&channel_names, log.channel_id),
        model: log.model,
        status_text,
        status_css,
        tokens: tokens_view(log.prompt_tokens, log.completion_tokens),
        duration_ms: log.duration_ms,
        client_ip: log.client_ip,
        log_error: log.error,
        request_body: log.request_body.as_deref().map(body_view),
        response_body: log.response_body.as_deref().map(body_view),
        back_url,
        error: None,
    }
}

/// A detail page that only carries the banner (row not found, database
/// failure, bad query): metadata stays on dashes, bodies on placeholders.
fn empty_detail(id: i64, back_url: String, message: String) -> LogDetailPage {
    LogDetailPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "logs",
        id,
        created_at: String::new(),
        key_label: String::new(),
        channel_label: String::new(),
        model: None,
        status_text: String::new(),
        status_css: String::new(),
        tokens: String::new(),
        duration_ms: 0,
        client_ip: None,
        log_error: None,
        request_body: None,
        response_body: None,
        back_url,
        error: Some(message),
    }
}

/// The effective page of a list view state, for the back link. An absent
/// or malformed `page` falls back to the first page.
fn back_page(query: &LogsQuery) -> u64 {
    trimmed(query.page.as_ref())
        .and_then(|page| page.parse::<u64>().ok())
        .unwrap_or(1)
}

/// GET /admin/ui/logs/{id}: the drill-down page for one request log. The
/// list view state rides in the query string so the back link restores
/// the exact page and filters the user came from. Read-only, like the
/// listing, so nothing sits under the CSRF guard. The query string only
/// shapes the back link: a malformed one never hides a stored row.
pub async fn detail(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
    query: Result<Query<LogsQuery>, QueryRejection>,
) -> (StatusCode, LogDetailPage) {
    let back_url = match query {
        Ok(Query(query)) => view_url(&query, back_page(&query)),
        Err(_) => "/admin/ui/logs".to_string(),
    };
    match request_logs::find(&state.db, id).await {
        Ok(Some(log)) => (StatusCode::OK, build_detail(&state, log, back_url).await),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            empty_detail(id, back_url, "request log not found".to_string()),
        ),
        Err(e) => {
            tracing::error!(error = %e, "admin ui log detail query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                empty_detail(id, back_url, "internal server error".to_string()),
            )
        }
    }
}

/// Read-only routes for the `/admin/ui` nest; no POSTs, so nothing new
/// sits under the CSRF guard.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/logs", get(page))
        .route("/logs/{id}", get(detail))
}
