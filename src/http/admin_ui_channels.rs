//! Admin UI handlers for the channels page.
//!
//! Submission pattern: one uniform htmx flow for every interaction. Each
//! request (open form, save, enable/disable, delete) targets the page's
//! `#channels-panel` element with an `outerHTML` swap, and the server always
//! answers with the freshly rendered panel fragment. Successful mutations
//! re-render the table, failed validations re-render the form with the
//! submitted values and per-field errors, and refused actions render the
//! error banner. Validation reuses the JSON API's helpers and every
//! mutation goes through the same db layer, so the UI cannot drift from
//! the API's behavior.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::db::channels::{self, Channel, ChannelPatch, DeleteOutcome, NewChannel, PricedChannel};
use crate::http::admin_channels::{
    mask_api_key, validate_allowed_models, validate_base_url, validate_channel_api_key,
    validate_channel_name, validate_model_prices, AdminError,
};
use crate::http::ui_auth::UiAuth;
use crate::http::AppState;
use crate::prices::{self, ModelPrice, ModelPrices, UsdAmountError, DIMENSIONS};

/// One table row. The upstream key is masked before it leaves the server,
/// exactly like the JSON views. Prices arrive as ready-to-render lines and
/// the limitable state is computed here, so the template stays logic-free.
pub struct ChannelRow {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key_masked: String,
    pub allowed_models: Option<Vec<String>>,
    pub prices_display: Vec<String>,
    pub limitable: bool,
    pub enabled: bool,
}

/// Per-field validation messages, rendered under the matching input. The
/// strings are the JSON API's own error texts.
#[derive(Default)]
pub struct ChannelFormErrors {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub allowed_models: Option<String>,
    pub model_prices: Option<String>,
}

impl ChannelFormErrors {
    fn any(&self) -> bool {
        self.name.is_some()
            || self.base_url.is_some()
            || self.api_key.is_some()
            || self.allowed_models.is_some()
            || self.model_prices.is_some()
    }
}

/// State of the create/edit form as it is rendered: the hx-post target,
/// submitted or prefilled values, and current validation errors. The
/// upstream key is never echoed back into the form.
pub struct ChannelForm {
    pub action: String,
    pub editing: bool,
    pub name: String,
    pub base_url: String,
    pub allowed_models_text: String,
    pub model_prices_text: String,
    pub errors: ChannelFormErrors,
}

/// The panel fragment: every handler on this page renders it. On the
/// delete-refused path `disable_instead` carries the channel id so the
/// banner can offer the disable action.
#[derive(Template, WebTemplate)]
#[template(path = "partials/channels_panel.html")]
pub struct ChannelsPanel {
    pub rows: Vec<ChannelRow>,
    pub form: Option<ChannelForm>,
    pub error: Option<String>,
    pub disable_instead: Option<i64>,
}

/// Full page wrapper around the same panel for direct navigation and
/// initial render.
#[derive(Template, WebTemplate)]
#[template(path = "channels.html")]
pub struct ChannelsPage {
    pub version: &'static str,
    pub section: &'static str,
    pub rows: Vec<ChannelRow>,
    pub form: Option<ChannelForm>,
    pub error: Option<String>,
    pub disable_instead: Option<i64>,
}

#[derive(Deserialize)]
pub struct ChannelCreateForm {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub allowed_models: Option<String>,
    pub model_prices: Option<String>,
}

#[derive(Deserialize)]
pub struct ChannelEditForm {
    pub name: String,
    pub base_url: String,
    /// Empty or absent means "keep the current key" on the edit form.
    pub api_key: Option<String>,
    pub allowed_models: Option<String>,
    pub model_prices: Option<String>,
}

#[derive(Deserialize)]
pub struct ToggleForm {
    pub enabled: bool,
}

/// Splits the allowed-models textarea into trimmed, non-empty lines; an
/// empty textarea yields an empty list, which means "allow all" through
/// the shared `validate_allowed_models` semantics.
fn parse_allowed_models_text(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parses the prices textarea: one model per line, in the form
/// `model: input, output, cached_input, cached_output[, cache_write_input]`,
/// each amount USD per 1M tokens with at most six decimal places. The fifth,
/// cache-write amount is optional and reads as zero when it is left out, so
/// a line written before the dimension existed keeps working. Blank lines
/// are skipped and an empty textarea yields an empty table, which clears the
/// stored prices exactly like an explicit null in the JSON API. Every error
/// names the line it came from.
fn parse_model_prices_text(raw: Option<&str>) -> Result<ModelPrices, String> {
    let required = DIMENSIONS.len() - 1;
    let expected =
        "expected \"model: input, output, cached_input, cached_output[, cache_write_input]\"";
    let mut entries: Vec<(String, ModelPrice)> = Vec::new();
    for (index, raw_line) in raw.unwrap_or_default().lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let number = index + 1;
        let Some((model, amounts)) = line.split_once(':') else {
            return Err(format!("line {number}: {expected}"));
        };
        let model = model.trim();
        if model.is_empty() {
            return Err(format!("line {number}: the model name is empty"));
        }
        let amounts: Vec<&str> = amounts.split(',').map(str::trim).collect();
        if amounts.len() < required || amounts.len() > DIMENSIONS.len() {
            return Err(format!(
                "line {number}: {model} needs {required} or {} amounts, got {}",
                DIMENSIONS.len(),
                amounts.len()
            ));
        }
        if entries.iter().any(|(existing, _)| existing == model) {
            return Err(format!("line {number}: {model} is listed twice"));
        }
        // An omitted write amount leaves the slot at zero, which is what
        // "no write price set" means.
        let mut micro = [0u64; DIMENSIONS.len()];
        for (slot, (dimension, amount)) in micro.iter_mut().zip(DIMENSIONS.iter().zip(amounts)) {
            *slot = match prices::parse_usd(amount) {
                Ok(micro) => micro,
                Err(UsdAmountError::NotExact) => {
                    return Err(format!(
                        "line {number}: {model} {dimension} \"{amount}\" is finer than \
                         0.000001 USD"
                    ))
                }
                Err(_) => {
                    return Err(format!(
                        "line {number}: {model} {dimension} \"{amount}\" must be a \
                         non-negative decimal USD amount"
                    ))
                }
            };
        }
        entries.push((
            model.to_string(),
            ModelPrice {
                input: micro[0],
                output: micro[1],
                cached_input: micro[2],
                cached_output: micro[3],
                cache_write_input: micro[4],
            },
        ));
    }
    ModelPrices::from_entries(entries).map_err(|error| error.to_string())
}

/// Renders a stored table back into the textarea format, so the edit form
/// opens with exactly what the parser accepts. Every dimension is written
/// out, the write one included, so an entry that carries no write price is
/// visible as the zero it is.
fn prices_text(prices: &ModelPrices) -> String {
    prices
        .iter()
        .map(|(model, price)| {
            format!(
                "{model}: {}, {}, {}, {}, {}",
                prices::format_usd(price.input),
                prices::format_usd(price.output),
                prices::format_usd(price.cached_input),
                prices::format_usd(price.cached_output),
                prices::format_usd(price.cache_write_input)
            )
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// Renders a stored table for the channels table: one compact line per
/// model, `gpt-4o: 2.50/10.00/1.25/5.00/3.13 $/1M`, the last amount being
/// the cache-write rate.
fn prices_display(prices: &ModelPrices) -> Vec<String> {
    prices
        .iter()
        .map(|(model, price)| {
            format!(
                "{model}: {}/{}/{}/{}/{} $/1M",
                prices::format_usd(price.input),
                prices::format_usd(price.output),
                prices::format_usd(price.cached_input),
                prices::format_usd(price.cached_output),
                prices::format_usd(price.cache_write_input)
            )
        })
        .collect()
}

fn create_action() -> String {
    "/admin/ui/channels".to_string()
}

fn edit_action(id: i64) -> String {
    format!("/admin/ui/channels/{id}")
}

/// Maps a storage failure to the same opaque message the JSON API uses;
/// the details go to the log only.
fn internal_error(error: &sqlx::Error) -> String {
    tracing::error!(error = %error, "admin ui channel query failed");
    "internal server error".to_string()
}

fn row_from(priced: PricedChannel) -> ChannelRow {
    let limitable = priced.limitable();
    let PricedChannel {
        channel,
        model_prices,
    } = priced;
    let Channel {
        id,
        name,
        base_url,
        api_key,
        allowed_models,
        enabled,
        created_at: _,
    } = channel;
    ChannelRow {
        id,
        api_key_masked: mask_api_key(&api_key),
        name,
        base_url,
        allowed_models,
        prices_display: model_prices
            .as_ref()
            .map(prices_display)
            .unwrap_or_default(),
        limitable,
        enabled,
    }
}

async fn rows(state: &AppState) -> Result<Vec<ChannelRow>, sqlx::Error> {
    Ok(channels::list_priced(&state.db)
        .await?
        .into_iter()
        .map(row_from)
        .collect())
}

/// Renders the panel fragment with the current table state plus whatever
/// form state or error the handler passes in.
async fn panel(
    state: &AppState,
    form: Option<ChannelForm>,
    error: Option<String>,
    disable_instead: Option<i64>,
) -> ChannelsPanel {
    let rows = match rows(state).await {
        Ok(rows) => rows,
        Err(e) => {
            return ChannelsPanel {
                rows: Vec::new(),
                form,
                error: Some(internal_error(&e)),
                disable_instead: None,
            }
        }
    };
    ChannelsPanel {
        rows,
        form,
        error,
        disable_instead,
    }
}

/// Renders the full channels page for GET /admin/ui/channels.
pub async fn page(State(state): State<AppState>, _auth: UiAuth) -> ChannelsPage {
    let panel = panel(&state, None, None, None).await;
    ChannelsPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "channels",
        rows: panel.rows,
        form: panel.form,
        error: panel.error,
        disable_instead: panel.disable_instead,
    }
}

/// "New channel" action: opens the empty create form.
async fn new_form(State(state): State<AppState>, _auth: UiAuth) -> ChannelsPanel {
    panel(
        &state,
        Some(ChannelForm {
            action: create_action(),
            editing: false,
            name: String::new(),
            base_url: String::new(),
            allowed_models_text: String::new(),
            model_prices_text: String::new(),
            errors: ChannelFormErrors::default(),
        }),
        None,
        None,
    )
    .await
}

/// "Edit" action: opens the form prefilled with the channel's current
/// values; the upstream key field stays empty so the stored secret never
/// crosses the wire. The prices textarea carries the stored table in the
/// line format the parser reads back.
async fn edit_form(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
) -> ChannelsPanel {
    match channels::find_priced(&state.db, id).await {
        Ok(Some(priced)) => {
            let allowed_models_text = priced
                .channel
                .allowed_models
                .clone()
                .unwrap_or_default()
                .join("\n");
            let model_prices_text = priced
                .model_prices
                .as_ref()
                .map(prices_text)
                .unwrap_or_default();
            panel(
                &state,
                Some(ChannelForm {
                    action: edit_action(id),
                    editing: true,
                    name: priced.channel.name,
                    base_url: priced.channel.base_url,
                    allowed_models_text,
                    model_prices_text,
                    errors: ChannelFormErrors::default(),
                }),
                None,
                None,
            )
            .await
        }
        Ok(None) => panel(&state, None, Some("channel not found".to_string()), None).await,
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

fn field_error(error: AdminError) -> String {
    error.to_string()
}

/// Parses the prices textarea and checks the result against the whitelist
/// the form leaves in force. An empty textarea clears the prices, matching
/// the JSON API's explicit null; the error text is what the form renders.
fn parse_and_validate_prices(
    raw: Option<&str>,
    allowed_models: Option<&[String]>,
) -> (Option<ModelPrices>, Option<String>) {
    let parsed = match parse_model_prices_text(raw) {
        Ok(parsed) => parsed,
        Err(message) => return (None, Some(message)),
    };
    if let Err(error) = validate_model_prices(Some(&parsed), allowed_models) {
        return (None, Some(field_error(error)));
    }
    let parsed = if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    };
    (parsed, None)
}

/// Runs the shared per-field validators over a submitted form and fills the
/// error slots; the messages are the JSON API's own.
fn collect_errors(
    name: &str,
    base_url: &str,
    api_key: Option<&str>,
    parsed_models: &[String],
) -> (ChannelFormErrors, Option<Vec<String>>) {
    let mut errors = ChannelFormErrors::default();
    if let Err(e) = validate_channel_name(name) {
        errors.name = Some(field_error(e));
    }
    if let Err(e) = validate_base_url(base_url) {
        errors.base_url = Some(field_error(e));
    }
    if let Some(api_key) = api_key {
        if let Err(e) = validate_channel_api_key(api_key) {
            errors.api_key = Some(field_error(e));
        }
    }
    let allowed = match validate_allowed_models(Some(parsed_models)) {
        Ok(allowed) => allowed,
        Err(e) => {
            errors.allowed_models = Some(field_error(e));
            None
        }
    };
    (errors, allowed)
}

/// Create-channel form POST. Validation failures re-render the form with
/// the submitted values and per-field errors; success re-renders the table.
async fn create(
    State(state): State<AppState>,
    _auth: UiAuth,
    payload: Result<axum::Form<ChannelCreateForm>, FormRejection>,
) -> ChannelsPanel {
    let Ok(submitted) = payload else {
        return panel(
            &state,
            None,
            Some("invalid form submission".to_string()),
            None,
        )
        .await;
    };
    let submitted = submitted.0;
    let parsed_models = parse_allowed_models_text(submitted.allowed_models.as_deref());
    let (mut errors, allowed) = collect_errors(
        &submitted.name,
        &submitted.base_url,
        Some(&submitted.api_key),
        &parsed_models,
    );
    let (prices, prices_error) =
        parse_and_validate_prices(submitted.model_prices.as_deref(), allowed.as_deref());
    if let Some(message) = prices_error {
        errors.model_prices = Some(message);
    }
    if errors.any() {
        let form = ChannelForm {
            action: create_action(),
            editing: false,
            name: submitted.name,
            base_url: submitted.base_url,
            allowed_models_text: parsed_models.join("\n"),
            model_prices_text: submitted.model_prices.unwrap_or_default(),
            errors,
        };
        return panel(&state, Some(form), None, None).await;
    }
    match channels::create_with_prices(
        &state.db,
        &NewChannel {
            name: submitted.name,
            base_url: submitted.base_url,
            api_key: submitted.api_key,
            allowed_models: allowed,
        },
        prices.as_ref(),
    )
    .await
    {
        Ok(_) => panel(&state, None, None, None).await,
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Edit-channel form POST. An empty key field keeps the stored key; the
/// allowed-models textarea always carries the full intended list, empty
/// meaning allow-all (matching the JSON API's `Some(Some([]))` semantics).
async fn update(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
    payload: Result<axum::Form<ChannelEditForm>, FormRejection>,
) -> ChannelsPanel {
    let Ok(submitted) = payload else {
        return panel(
            &state,
            None,
            Some("invalid form submission".to_string()),
            None,
        )
        .await;
    };
    let submitted = submitted.0;
    // Empty or absent key input means "keep the current key".
    let new_api_key = submitted
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty());
    let parsed_models = parse_allowed_models_text(submitted.allowed_models.as_deref());
    let (mut errors, allowed) = collect_errors(
        &submitted.name,
        &submitted.base_url,
        new_api_key,
        &parsed_models,
    );
    let (prices, prices_error) =
        parse_and_validate_prices(submitted.model_prices.as_deref(), allowed.as_deref());
    if let Some(message) = prices_error {
        errors.model_prices = Some(message);
    }
    if errors.any() {
        let form = ChannelForm {
            action: edit_action(id),
            editing: true,
            name: submitted.name,
            base_url: submitted.base_url,
            allowed_models_text: parsed_models.join("\n"),
            model_prices_text: submitted.model_prices.unwrap_or_default(),
            errors,
        };
        return panel(&state, Some(form), None, None).await;
    }
    // Invalidate every entry bound to the channel before and after: the
    // edit form can rotate base_url and the upstream api_key together.
    state.cache.invalidate_channel(id);
    match channels::update_with_prices(
        &state.db,
        id,
        &ChannelPatch {
            name: Some(submitted.name),
            base_url: Some(submitted.base_url),
            api_key: new_api_key.map(str::to_string),
            allowed_models: Some(allowed),
            enabled: None,
        },
        Some(prices.as_ref()),
    )
    .await
    {
        Ok(Some(_)) => {
            state.cache.invalidate_channel(id);
            panel(&state, None, None, None).await
        }
        Ok(None) => panel(&state, None, Some("channel not found".to_string()), None).await,
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Enable/disable toggle. The form carries the target state, so no read
/// of the current row is needed.
async fn toggle(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
    payload: Result<axum::Form<ToggleForm>, FormRejection>,
) -> ChannelsPanel {
    let Ok(submitted) = payload else {
        return panel(
            &state,
            None,
            Some("invalid form submission".to_string()),
            None,
        )
        .await;
    };
    state.cache.invalidate_channel(id);
    match channels::update(
        &state.db,
        id,
        &ChannelPatch {
            name: None,
            base_url: None,
            api_key: None,
            allowed_models: None,
            enabled: Some(submitted.enabled),
        },
    )
    .await
    {
        Ok(Some(_)) => {
            state.cache.invalidate_channel(id);
            panel(&state, None, None, None).await
        }
        Ok(None) => panel(&state, None, Some("channel not found".to_string()), None).await,
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Delete with the 409 flow: when keys are still bound the panel comes
/// back with their count in the banner and a "disable instead" action,
/// because disabling preserves the keys and their request logs.
async fn delete(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
) -> ChannelsPanel {
    state.cache.invalidate_channel(id);
    match channels::delete(&state.db, id).await {
        Ok(DeleteOutcome::Deleted) => {
            state.cache.invalidate_channel(id);
            panel(&state, None, None, None).await
        }
        Ok(DeleteOutcome::NotFound) => {
            panel(&state, None, Some("channel not found".to_string()), None).await
        }
        Ok(DeleteOutcome::HasBoundKeys) => match channels::bound_key_count(&state.db, id).await {
            Ok(count) => {
                let error = format!(
                    "cannot delete this channel: {count} client keys are still bound to it. \
                     Disabling preserves those keys and their request logs."
                );
                panel(&state, None, Some(error), Some(id)).await
            }
            Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
        },
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Channel routes for the `/admin/ui` nest. They are merged into the UI
/// router before the CSRF layer is applied, so every POST here sits
/// inside the guarded nest.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/channels", get(page).post(create))
        .route("/channels/form", get(new_form))
        .route("/channels/{id}/edit", get(edit_form))
        .route("/channels/{id}", post(update))
        .route("/channels/{id}/toggle", post(toggle))
        .route("/channels/{id}/delete", post(delete))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::channels::Channel;

    #[test]
    fn parse_allowed_models_text_drops_blank_lines_and_trims() {
        let parsed = parse_allowed_models_text(Some("gpt-4o\n  \n gpt-4o-mini \n"));
        assert_eq!(
            parsed,
            vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]
        );
        assert!(parse_allowed_models_text(None).is_empty());
        assert!(parse_allowed_models_text(Some("\n  \n")).is_empty());
    }

    #[test]
    fn parse_model_prices_text_reads_lines_into_micro_usd() {
        let prices = parse_model_prices_text(Some(
            "gpt-4o: 2.50, 10.00, 1.25, 5.00\n\ngpt-4o-mini: 0.15,0.6,0.075,0.30\n",
        ))
        .unwrap();
        let full = prices.get("gpt-4o").unwrap();
        assert_eq!(full.input, 2_500_000);
        assert_eq!(full.output, 10_000_000);
        assert_eq!(full.cached_input, 1_250_000);
        assert_eq!(full.cached_output, 5_000_000);
        assert_eq!(
            full.cache_write_input, 0,
            "a four-amount line names no write price"
        );
        let mini = prices.get("gpt-4o-mini").unwrap();
        assert_eq!(mini.input, 150_000);
        assert_eq!(mini.output, 600_000);
        assert_eq!(mini.cached_input, 75_000);
        assert_eq!(mini.cached_output, 300_000);
        assert_eq!(prices.len(), 2);
    }

    #[test]
    fn parse_model_prices_text_reads_the_optional_cache_write_amount() {
        let prices = parse_model_prices_text(Some("claude: 15, 75, 1.50, 0, 18.75\n")).unwrap();
        let entry = prices.get("claude").unwrap();
        assert_eq!(entry.input, 15_000_000);
        assert_eq!(entry.output, 75_000_000);
        assert_eq!(entry.cached_input, 1_500_000);
        assert_eq!(entry.cached_output, 0);
        assert_eq!(entry.cache_write_input, 18_750_000);
    }

    #[test]
    fn parse_model_prices_text_treats_an_empty_textarea_as_no_prices() {
        assert!(parse_model_prices_text(None).unwrap().is_empty());
        assert!(parse_model_prices_text(Some("\n   \n")).unwrap().is_empty());
    }

    #[test]
    fn parse_model_prices_text_reports_the_offending_line() {
        let error = parse_model_prices_text(Some("gpt-4o: 2.50, 10.00, 1.25, 5.00\nno colon\n"))
            .unwrap_err();
        assert_eq!(
            error,
            "line 2: expected \"model: input, output, cached_input, cached_output[, \
             cache_write_input]\""
        );

        let error = parse_model_prices_text(Some("gpt-4o: 2.50, 10.00, 1.25")).unwrap_err();
        assert_eq!(error, "line 1: gpt-4o needs 4 or 5 amounts, got 3");

        // A sixth amount is one too many: an unknown dimension is never
        // silently dropped.
        let error = parse_model_prices_text(Some("gpt-4o: 1, 1, 1, 1, 1, 1")).unwrap_err();
        assert_eq!(error, "line 1: gpt-4o needs 4 or 5 amounts, got 6");

        let error = parse_model_prices_text(Some(": 1, 1, 1, 1")).unwrap_err();
        assert_eq!(error, "line 1: the model name is empty");

        let error =
            parse_model_prices_text(Some("gpt-4o: 1, 1, 1, 1\ngpt-4o: 2, 2, 2, 2")).unwrap_err();
        assert_eq!(error, "line 2: gpt-4o is listed twice");
    }

    #[test]
    fn parse_model_prices_text_rejects_bad_amounts() {
        let error = parse_model_prices_text(Some("gpt-4o: abc, 1, 1, 1")).unwrap_err();
        assert!(
            error.contains("must be a non-negative decimal USD amount"),
            "{error}"
        );

        let error = parse_model_prices_text(Some("gpt-4o: -1, 1, 1, 1")).unwrap_err();
        assert!(
            error.contains("must be a non-negative decimal USD amount"),
            "{error}"
        );

        let error = parse_model_prices_text(Some("gpt-4o: 0.0000005, 1, 1, 1")).unwrap_err();
        assert!(error.contains("finer than 0.000001 USD"), "{error}");
    }

    #[test]
    fn prices_text_round_trips_through_the_parser() {
        let prices = parse_model_prices_text(Some("gpt-4o: 2.50, 10, 1.25, 5, 3.13")).unwrap();
        let text = prices_text(&prices);
        assert_eq!(text, "gpt-4o: 2.50, 10.00, 1.25, 5.00, 3.13");
        assert_eq!(parse_model_prices_text(Some(&text)).unwrap(), prices);

        // A four-amount line opens as five: the dimension the entry does not
        // price is written out as the zero it reads as.
        let without_writes = parse_model_prices_text(Some("gpt-4o: 2.50, 10, 1.25, 5")).unwrap();
        assert_eq!(
            prices_text(&without_writes),
            "gpt-4o: 2.50, 10.00, 1.25, 5.00, 0.00"
        );
        assert_eq!(
            parse_model_prices_text(Some(&prices_text(&without_writes))).unwrap(),
            without_writes
        );
    }

    #[test]
    fn prices_display_is_compact_and_complete() {
        let prices = parse_model_prices_text(Some("gpt-4o: 2.50, 10, 1.25, 5, 3.125")).unwrap();
        assert_eq!(
            prices_display(&prices),
            vec!["gpt-4o: 2.50/10.00/1.25/5.00/3.125 $/1M".to_string()]
        );
        let four_only = parse_model_prices_text(Some("gpt-4o: 2.50, 10, 1.25, 5")).unwrap();
        assert_eq!(
            prices_display(&four_only),
            vec!["gpt-4o: 2.50/10.00/1.25/5.00/0.00 $/1M".to_string()]
        );
    }

    fn priced_channel(
        enabled: bool,
        allowed: Option<Vec<String>>,
        prices: Option<&str>,
    ) -> ChannelRow {
        let channel = Channel {
            id: 1,
            name: "upstream".to_string(),
            base_url: "https://api.example.com".to_string(),
            api_key: "sk-upstream-secret-1234567890".to_string(),
            allowed_models: allowed,
            enabled,
            created_at: "2026-11-08 00:00:00".to_string(),
        };
        row_from(PricedChannel {
            channel,
            model_prices: prices.map(|text| parse_model_prices_text(Some(text)).unwrap()),
        })
    }

    #[test]
    fn limitable_needs_an_enabled_channel_with_every_allowed_model_priced() {
        let complete = Some("gpt-4o: 1, 1, 1, 1");
        assert!(priced_channel(true, Some(vec!["gpt-4o".to_string()]), complete).limitable);
        assert!(!priced_channel(false, Some(vec!["gpt-4o".to_string()]), complete).limitable);
        assert!(!priced_channel(true, None, complete).limitable);
        assert!(!priced_channel(true, Some(vec!["gpt-4o".to_string()]), None).limitable);
        let partial = Some("gpt-4o: 1, 1, 1, 1");
        assert!(
            !priced_channel(
                true,
                Some(vec!["gpt-4o".to_string(), "other".to_string()]),
                partial
            )
            .limitable
        );
    }

    #[test]
    fn row_renders_prices_and_limitable_state_from_the_stored_table() {
        let row = priced_channel(
            true,
            Some(vec!["gpt-4o".to_string()]),
            Some("gpt-4o: 2.50, 10, 1.25, 5, 3.13"),
        );
        assert_eq!(
            row.prices_display,
            vec!["gpt-4o: 2.50/10.00/1.25/5.00/3.13 $/1M".to_string()]
        );
        assert!(row.limitable);

        let unpriced = priced_channel(true, Some(vec!["gpt-4o".to_string()]), None);
        assert!(unpriced.prices_display.is_empty());
        assert!(!unpriced.limitable);
    }
}
