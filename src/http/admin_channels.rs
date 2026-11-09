//! Admin CRUD handlers for channels ("redirects").
//!
//! Error style: admin routes use plain JSON bodies `{"error": "..."}`, not
//! the OpenAI shape reserved for proxy routes. Status mapping: 400 invalid
//! input, 401 handled by `admin_auth`, 404 unknown channel, 409 channel has
//! bound API keys, 500 internal storage failure.
//!
//! Security invariant: the upstream `api_key` never appears in a response.
//! Handlers return `ChannelView`, which carries only a masked preview and a
//! `has_api_key` flag.
//!
//! Prices: the request shape carries decimal USD per 1M tokens (strings or
//! numbers, up to six decimal places), the stored and returned shape carries
//! integer micro-USD per 1M tokens. See [`crate::prices`] for the
//! conversion rules; nothing here rounds an amount.

use std::collections::BTreeMap;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use utoipa::ToSchema;

use crate::db::channels::{self, Channel, ChannelPatch, DeleteOutcome, NewChannel, PricedChannel};
use crate::http::admin_auth::AdminAuth;
use crate::http::docs::AdminErrorBody;
use crate::http::AppState;
use crate::prices::{ModelPrice, ModelPrices};

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("channel not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AdminError::NotFound => (StatusCode::NOT_FOUND, "channel not found".to_string()),
            AdminError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            AdminError::Conflict(message) => (StatusCode::CONFLICT, message),
            AdminError::Database(e) => {
                tracing::error!(error = %e, "admin channel query failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

/// Body for creating a channel.
#[derive(Deserialize, ToSchema)]
pub struct CreateChannelRequest {
    /// Human-readable channel label, shown in listings and logs.
    #[schema(example = "openai")]
    pub name: String,
    /// Absolute http(s) URL of the upstream; the API path is appended.
    #[schema(example = "https://api.openai.com")]
    pub base_url: String,
    /// Upstream API key, stored server-side and never returned in full.
    #[schema(example = "sk-upstream-secret-1234567890")]
    pub api_key: String,
    /// Exact-match model whitelist. Omitted or null allows every model the
    /// upstream serves; an empty array also means allow-all.
    #[serde(default)]
    #[schema(example = json!(["gpt-4o", "gpt-4o-mini"]))]
    pub allowed_models: Option<Vec<String>>,
    /// Prices for this channel, object keyed by model name. Each model needs
    /// all four dimensions: `input`, `output`, `cached_input` and
    /// `cached_output`, given in USD per 1M tokens as decimal strings (or
    /// numbers) with at most six decimal places. Amounts that cannot be
    /// stored exactly as a whole number of micro-USD are rejected with 400,
    /// never rounded. Omitted or null stores no prices, and an empty object
    /// does the same. When `allowed_models` is set, every priced model must
    /// be part of it. Responses return the same object with the amounts
    /// converted to integer micro-USD per 1M tokens.
    #[serde(default)]
    #[schema(
        value_type = Object,
        example = json!({
            "gpt-4o": {
                "input": "2.50",
                "output": "10.00",
                "cached_input": "1.25",
                "cached_output": "5.00"
            }
        })
    )]
    pub model_prices: Option<Value>,
}

/// Body for editing a channel; only the sent fields change.
#[derive(Deserialize, ToSchema)]
pub struct PatchChannelRequest {
    #[serde(default)]
    #[schema(example = "renamed")]
    pub name: Option<String>,
    #[serde(default)]
    #[schema(example = "https://api.openai.com/v1")]
    pub base_url: Option<String>,
    #[serde(default)]
    #[schema(example = "sk-new-upstream-secret-1234567890")]
    pub api_key: Option<String>,
    /// `null` (JSON null) clears the whitelist back to allow-all; the field
    /// being absent leaves it unchanged.
    #[serde(default, deserialize_with = "double_option")]
    pub allowed_models: Option<Option<Vec<String>>>,
    /// Same shape and rules as on create. Being absent leaves the stored
    /// prices untouched; `null` (JSON null) or an empty object clears them;
    /// an object replaces the whole table rather than merging into it. The
    /// resulting prices are validated against the whitelist the patch
    /// leaves in force, so shrinking `allowed_models` below the priced
    /// models is rejected with 400.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(
        value_type = Object,
        nullable = true,
        example = json!({
            "gpt-4o-mini": {
                "input": "0.15",
                "output": "0.60",
                "cached_input": "0.075",
                "cached_output": "0.30"
            }
        })
    )]
    pub model_prices: Option<Option<Value>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// One model's prices in the JSON API: integer micro-USD per 1M tokens, so
/// `2500000` means 2.50 USD per 1M tokens.
#[derive(Serialize, ToSchema)]
pub struct ModelPriceView {
    /// Prompt tokens billed at the full input rate.
    #[schema(example = 2_500_000)]
    pub input: u64,
    /// Completion tokens.
    #[schema(example = 10_000_000)]
    pub output: u64,
    /// Prompt tokens served from the upstream cache.
    #[schema(example = 1_250_000)]
    pub cached_input: u64,
    /// Cached prompt tokens written back to the upstream cache.
    #[schema(example = 5_000_000)]
    pub cached_output: u64,
}

impl From<&ModelPrice> for ModelPriceView {
    fn from(price: &ModelPrice) -> Self {
        Self {
            input: price.input,
            output: price.output,
            cached_input: price.cached_input,
            cached_output: price.cached_output,
        }
    }
}

/// Public view of a channel; the upstream key is masked.
#[derive(Serialize, ToSchema)]
pub struct ChannelView {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key_masked: String,
    pub has_api_key: bool,
    pub allowed_models: Option<Vec<String>>,
    /// Stored prices, integer micro-USD per 1M tokens; null when the
    /// channel has none.
    pub model_prices: Option<BTreeMap<String, ModelPriceView>>,
    pub enabled: bool,
    pub created_at: String,
}

impl From<PricedChannel> for ChannelView {
    fn from(priced: PricedChannel) -> Self {
        let Channel {
            id,
            name,
            base_url,
            api_key,
            allowed_models,
            enabled,
            created_at,
        } = priced.channel;
        Self {
            id,
            name,
            base_url,
            api_key_masked: mask_api_key(&api_key),
            has_api_key: true,
            allowed_models,
            model_prices: priced.model_prices.as_ref().map(price_views),
            enabled,
            created_at,
        }
    }
}

fn price_views(prices: &ModelPrices) -> BTreeMap<String, ModelPriceView> {
    prices
        .iter()
        .map(|(model, price)| (model.clone(), ModelPriceView::from(price)))
        .collect()
}

/// Converts the external USD shape of a request into the stored integer
/// shape; an empty object means the channel carries no prices, exactly like
/// an omitted field or an explicit null.
pub(crate) fn parse_model_prices(value: &Value) -> Result<Option<ModelPrices>, AdminError> {
    let prices = ModelPrices::from_usd_json(value)
        .map_err(|error| AdminError::BadRequest(error.to_string()))?;
    Ok(if prices.is_empty() {
        None
    } else {
        Some(prices)
    })
}

/// Rejects a price table that names models outside the channel's whitelist.
/// A channel without a whitelist serves every upstream model, so its prices
/// need no such check.
pub(crate) fn validate_model_prices(
    prices: Option<&ModelPrices>,
    allowed_models: Option<&[String]>,
) -> Result<(), AdminError> {
    match prices {
        Some(prices) => prices
            .validate_against_allowed(allowed_models)
            .map_err(|error| AdminError::BadRequest(error.to_string())),
        None => Ok(()),
    }
}

/// Masks an upstream key for display: first and last four characters around
/// an ellipsis, or full asterisks when the key is too short to halve safely.
/// Shared by the JSON views and the admin UI table.
pub(crate) fn mask_api_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 8 {
        return "*".repeat(len);
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(len - 4).collect();
    format!("{head}...{tail}")
}

/// Channel name must be a non-empty-after-trim label. Shared by the JSON
/// handlers and the admin UI form.
pub(crate) fn validate_channel_name(name: &str) -> Result<(), AdminError> {
    if name.trim().is_empty() {
        return Err(AdminError::BadRequest("name must not be empty".to_string()));
    }
    Ok(())
}

/// Upstream key must be non-empty after trim. Shared by the JSON handlers
/// and the admin UI form.
pub(crate) fn validate_channel_api_key(api_key: &str) -> Result<(), AdminError> {
    if api_key.trim().is_empty() {
        return Err(AdminError::BadRequest(
            "api_key must not be empty".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_base_url(raw: &str) -> Result<(), AdminError> {
    let parsed =
        Url::parse(raw).map_err(|_| AdminError::BadRequest(format!("invalid base_url: {raw}")))?;
    if parsed.cannot_be_a_base() || !matches!(parsed.scheme(), "http" | "https") {
        return Err(AdminError::BadRequest(format!(
            "base_url must be an absolute http(s) URL: {raw}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_allowed_models(
    models: Option<&[String]>,
) -> Result<Option<Vec<String>>, AdminError> {
    let Some(models) = models else {
        return Ok(None);
    };
    if models.iter().any(|m| m.trim().is_empty()) {
        return Err(AdminError::BadRequest(
            "allowed_models must not contain empty strings".to_string(),
        ));
    }
    // An empty whitelist means "allow all" per plan semantics.
    if models.is_empty() {
        Ok(None)
    } else {
        Ok(Some(models.to_vec()))
    }
}

fn validate_common(
    name: &str,
    base_url: &str,
    api_key: &str,
    allowed_models: Option<&[String]>,
) -> Result<Option<Vec<String>>, AdminError> {
    validate_channel_name(name)?;
    validate_channel_api_key(api_key)?;
    validate_base_url(base_url)?;
    validate_allowed_models(allowed_models)
}

#[utoipa::path(
    post,
    path = "/admin/channels",
    tag = "Admin",
    operation_id = "create_channel",
    summary = "Create a channel",
    description = "Registers an upstream provider that client keys can be \
        bound to. The upstream key is stored server-side; every response \
        returns only a masked preview, never the full secret. New channels \
        start enabled.",
    security(("basic_auth" = [])),
    request_body(
        content = CreateChannelRequest,
        description = "Channel definition. Omit `allowed_models` to allow \
            every model the upstream serves. `model_prices` carries USD per \
            1M tokens as decimal strings or numbers and is stored as integer \
            micro-USD; every priced model must be part of `allowed_models` \
            when that list is set.",
        examples(
            ("whitelist" = (
                summary = "Channel restricted to two models",
                value = json!({
                    "name": "openai",
                    "base_url": "https://api.openai.com",
                    "api_key": "sk-upstream-secret-1234567890",
                    "allowed_models": ["gpt-4o", "gpt-4o-mini"]
                })
            )),
            ("priced" = (
                summary = "Channel with prices for both whitelisted models",
                value = json!({
                    "name": "openai",
                    "base_url": "https://api.openai.com",
                    "api_key": "sk-upstream-secret-1234567890",
                    "allowed_models": ["gpt-4o", "gpt-4o-mini"],
                    "model_prices": {
                        "gpt-4o": {
                            "input": "2.50",
                            "output": "10.00",
                            "cached_input": "1.25",
                            "cached_output": "5.00"
                        },
                        "gpt-4o-mini": {
                            "input": "0.15",
                            "output": "0.60",
                            "cached_input": "0.075",
                            "cached_output": "0.30"
                        }
                    }
                })
            )),
            ("allow_all" = (
                summary = "Channel serving every upstream model",
                value = json!({
                    "name": "openai",
                    "base_url": "https://api.openai.com",
                    "api_key": "sk-upstream-secret-1234567890"
                })
            )),
            ("invalid" = (
                summary = "Rejected with 400: base_url is not a URL",
                value = json!({
                    "name": "broken",
                    "base_url": "not a url",
                    "api_key": "sk-upstream-secret-1234567890"
                })
            ))
        )
    ),
    responses(
        (status = 201, description = "Channel created", body = ChannelView, example = json!({
            "id": 1,
            "name": "openai",
            "base_url": "https://api.openai.com",
            "api_key_masked": "sk-u...7890",
            "has_api_key": true,
            "allowed_models": ["gpt-4o", "gpt-4o-mini"],
            "model_prices": {
                "gpt-4o": {
                    "input": 2500000,
                    "output": 10000000,
                    "cached_input": 1250000,
                    "cached_output": 5000000
                }
            },
            "enabled": true,
            "created_at": "2026-11-07 23:59:59"
        })),
        (status = 400, description = "Invalid input: empty name or api_key, \
            a non-absolute or non-http base_url, an empty string inside \
            allowed_models, or a price table that is malformed, finer than \
            1e-6 USD or names a model outside allowed_models", body = AdminErrorBody, example = json!({
            "error": "invalid base_url: not a url"
        })),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body")
    )
)]
pub async fn create(
    State(state): State<AppState>,
    _auth: AdminAuth,
    payload: Result<Json<CreateChannelRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ChannelView>), AdminError> {
    let Json(request) =
        payload.map_err(|rejection| AdminError::BadRequest(rejection.body_text()))?;
    let allowed_models = validate_common(
        &request.name,
        &request.base_url,
        &request.api_key,
        request.allowed_models.as_deref(),
    )?;
    let model_prices = match &request.model_prices {
        Some(value) => parse_model_prices(value)?,
        None => None,
    };
    validate_model_prices(model_prices.as_ref(), allowed_models.as_deref())?;
    let channel = channels::create_with_prices(
        &state.db,
        &NewChannel {
            name: request.name,
            base_url: request.base_url,
            api_key: request.api_key,
            allowed_models,
        },
        model_prices.as_ref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(channel.into())))
}

#[utoipa::path(
    get,
    path = "/admin/channels",
    tag = "Admin",
    operation_id = "list_channels",
    summary = "List channels",
    description = "Returns every channel, including disabled ones, with the \
        upstream key masked in each row.",
    security(("basic_auth" = [])),
    responses(
        (status = 200, description = "All channels, oldest first", body = Vec<ChannelView>, example = json!([
            {
                "id": 1,
                "name": "openai",
                "base_url": "https://api.openai.com",
                "api_key_masked": "sk-u...7890",
                "has_api_key": true,
                "allowed_models": null,
                "model_prices": null,
                "enabled": true,
                "created_at": "2026-11-07 23:59:59"
            }
        ])),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body")
    )
)]
pub async fn list(
    State(state): State<AppState>,
    _auth: AdminAuth,
) -> Result<Json<Vec<ChannelView>>, AdminError> {
    let channel_list = channels::list_priced(&state.db).await?;
    Ok(Json(channel_list.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    get,
    path = "/admin/channels/{id}",
    tag = "Admin",
    operation_id = "get_channel",
    summary = "Fetch one channel",
    description = "Returns the channel by id, with the upstream key masked.",
    security(("basic_auth" = [])),
    params(("id" = i64, Path, description = "Channel id", example = 1)),
    responses(
        (status = 200, description = "The channel", body = ChannelView, example = json!({
            "id": 1,
            "name": "openai",
            "base_url": "https://api.openai.com",
            "api_key_masked": "sk-u...7890",
            "has_api_key": true,
            "allowed_models": ["gpt-4o", "gpt-4o-mini"],
            "model_prices": {
                "gpt-4o": {
                    "input": 2500000,
                    "output": 10000000,
                    "cached_input": 1250000,
                    "cached_output": 5000000
                }
            },
            "enabled": true,
            "created_at": "2026-11-07 23:59:59"
        })),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body"),
        (status = 404, description = "No channel with this id", body = AdminErrorBody, example = json!({
            "error": "channel not found"
        }))
    )
)]
pub async fn get_one(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
) -> Result<Json<ChannelView>, AdminError> {
    let channel = channels::find_priced(&state.db, id)
        .await?
        .ok_or(AdminError::NotFound)?;
    Ok(Json(channel.into()))
}

#[utoipa::path(
    patch,
    path = "/admin/channels/{id}",
    tag = "Admin",
    operation_id = "patch_channel",
    summary = "Edit a channel",
    description = "Updates only the fields present in the body. Sending JSON \
        null as `allowed_models` clears the whitelist back to allow-all; \
        an empty array does the same. `model_prices` replaces the whole \
        table when present and is cleared by an explicit null or an empty \
        object. This is also how a channel is enabled or disabled.",
    security(("basic_auth" = [])),
    params(("id" = i64, Path, description = "Channel id", example = 1)),
    request_body(
        content = PatchChannelRequest,
        description = "Fields to change; everything absent stays as it is.",
        examples(
            ("rename_and_restrict" = (
                summary = "Rename and set a whitelist",
                value = json!({
                    "name": "renamed",
                    "allowed_models": ["gpt-4o"],
                    "enabled": false
                })
            )),
            ("set_prices" = (
                summary = "Replace the price table",
                value = json!({
                    "model_prices": {
                        "gpt-4o": {
                            "input": "2.50",
                            "output": "10.00",
                            "cached_input": "1.25",
                            "cached_output": "5.00"
                        }
                    }
                })
            )),
            ("clear_whitelist" = (
                summary = "Clear the whitelist with an explicit null",
                value = json!({ "allowed_models": null })
            )),
            ("clear_prices" = (
                summary = "Clear the price table with an explicit null",
                value = json!({ "model_prices": null })
            )),
            ("invalid" = (
                summary = "Rejected with 400: ftp scheme",
                value = json!({ "base_url": "ftp://api.example.com" })
            ))
        )
    ),
    responses(
        (status = 200, description = "The updated channel", body = ChannelView, example = json!({
            "id": 1,
            "name": "renamed",
            "base_url": "https://api.openai.com",
            "api_key_masked": "sk-u...7890",
            "has_api_key": true,
            "allowed_models": ["gpt-4o"],
            "model_prices": {
                "gpt-4o": {
                    "input": 2500000,
                    "output": 10000000,
                    "cached_input": 1250000,
                    "cached_output": 5000000
                }
            },
            "enabled": false,
            "created_at": "2026-11-07 23:59:59"
        })),
        (status = 400, description = "Invalid field value, same rules as on \
            create; a price table that the patched whitelist no longer \
            covers is rejected too", body = AdminErrorBody, example = json!({
            "error": "base_url must be an absolute http(s) URL: ftp://api.example.com"
        })),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body"),
        (status = 404, description = "No channel with this id", body = AdminErrorBody, example = json!({
            "error": "channel not found"
        }))
    )
)]
pub async fn patch(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
    payload: Result<Json<PatchChannelRequest>, JsonRejection>,
) -> Result<Json<ChannelView>, AdminError> {
    let Json(request) =
        payload.map_err(|rejection| AdminError::BadRequest(rejection.body_text()))?;
    if let Some(name) = &request.name {
        validate_channel_name(name)?;
    }
    if let Some(api_key) = &request.api_key {
        validate_channel_api_key(api_key)?;
    }
    if let Some(base_url) = &request.base_url {
        validate_base_url(base_url)?;
    }
    let allowed_models = match &request.allowed_models {
        None => None,
        Some(inner) => Some(validate_allowed_models(inner.as_deref())?),
    };
    // The price table is checked against the whitelist the patch leaves in
    // force, so a stored table cannot end up naming models the channel no
    // longer serves.
    let current = channels::find_priced(&state.db, id)
        .await?
        .ok_or(AdminError::NotFound)?;
    let prices_patch = match &request.model_prices {
        None => None,
        Some(None) => Some(None),
        Some(Some(value)) => Some(parse_model_prices(value)?),
    };
    let effective_allowed = match &allowed_models {
        Some(allowed) => allowed.clone(),
        None => current.channel.allowed_models.clone(),
    };
    let effective_prices = match &prices_patch {
        Some(prices) => prices.as_ref(),
        None => current.model_prices.as_ref(),
    };
    validate_model_prices(effective_prices, effective_allowed.as_deref())?;
    // Invalidate every cached entry bound to this channel before and after
    // the write, advancing the cache epoch so an in-flight request that read
    // the pre-write row loses its refill: a rotated base_url or upstream
    // api_key must take effect at once, not when the entries expire.
    state.cache.invalidate_channel(id);
    let channel = channels::update_with_prices(
        &state.db,
        id,
        &ChannelPatch {
            name: request.name,
            base_url: request.base_url,
            api_key: request.api_key,
            allowed_models,
            enabled: request.enabled,
        },
        prices_patch.as_ref().map(|prices| prices.as_ref()),
    )
    .await?
    .ok_or(AdminError::NotFound)?;
    state.cache.invalidate_channel(id);
    Ok(Json(channel.into()))
}

#[utoipa::path(
    delete,
    path = "/admin/channels/{id}",
    tag = "Admin",
    operation_id = "delete_channel",
    summary = "Delete a channel",
    description = "Removes the channel. A channel that still has client keys \
        bound to it refuses deletion with 409; disable it instead.",
    security(("basic_auth" = [])),
    params(("id" = i64, Path, description = "Channel id", example = 1)),
    responses(
        (status = 204, description = "Channel deleted, no body"),
        (status = 401, description = "Missing or wrong admin credentials; \
            the response carries a Basic challenge header and no body"),
        (status = 404, description = "No channel with this id", body = AdminErrorBody, example = json!({
            "error": "channel not found"
        })),
        (status = 409, description = "The channel still has bound keys", body = AdminErrorBody, example = json!({
            "error": "channel has bound API keys; disable it instead"
        }))
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
) -> Result<StatusCode, AdminError> {
    state.cache.invalidate_channel(id);
    match channels::delete(&state.db, id).await? {
        DeleteOutcome::Deleted => {
            state.cache.invalidate_channel(id);
            Ok(StatusCode::NO_CONTENT)
        }
        DeleteOutcome::NotFound => Err(AdminError::NotFound),
        DeleteOutcome::HasBoundKeys => Err(AdminError::Conflict(
            "channel has bound API keys; disable it instead".to_string(),
        )),
    }
}

// `null` in JSON maps to `Some(None)`, a missing field to the outer default.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde_json::{json, Value};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::http::{router, AppState};

    const UPSTREAM_KEY: &str = "sk-upstream-secret-1234567890";

    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        AppState {
            db: pool.into(),
            admin_login: "admin".to_string(),
            admin_password: "secret".to_string(),
            client: crate::http::upstream_client(),
            log_bodies: false,
            log_tracker: crate::logging::LogTracker::default(),
            cache: crate::cache::ResolutionCache::default(),
        }
    }

    fn auth_header() -> String {
        format!("Basic {}", STANDARD.encode("admin:secret"))
    }

    async fn send_json(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        send_raw(state, method, uri, Some(body.to_string().into_bytes())).await
    }

    async fn send_raw(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", auth_header())
            .header("Content-Type", "application/json");
        let request = match body {
            Some(bytes) => request.body(Body::from(bytes)),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let parsed = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, parsed)
    }

    fn create_body() -> Value {
        json!({
            "name": "primary",
            "base_url": "https://api.example.com",
            "api_key": UPSTREAM_KEY,
        })
    }

    #[tokio::test]
    async fn create_then_list_get_patch_returns_channel_state() {
        let state = test_state().await;
        let (status, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["name"], "primary");
        assert_eq!(created["enabled"], true);
        assert_eq!(created["allowed_models"], Value::Null);

        let (status, listed) = send_json(&state, "GET", "/admin/channels", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed.as_array().unwrap().len(), 1);

        let id = created["id"].as_i64().unwrap();
        let (status, fetched) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fetched["name"], "primary");

        let patch = json!({
            "name": "renamed",
            "allowed_models": ["gpt-4o", "gpt-4o-mini"],
            "enabled": false,
        });
        let (status, patched) =
            send_json(&state, "PATCH", &format!("/admin/channels/{id}"), patch).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(patched["name"], "renamed");
        assert_eq!(patched["allowed_models"], json!(["gpt-4o", "gpt-4o-mini"]));
        assert_eq!(patched["enabled"], false);

        let (_, reread) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert_eq!(reread["name"], "renamed");
        assert_eq!(reread["enabled"], false);
    }

    #[tokio::test]
    async fn patch_can_clear_allowed_models_with_explicit_null() {
        let state = test_state().await;
        let mut body = create_body();
        body["allowed_models"] = json!(["gpt-4o"]);
        let (_, created) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(created["allowed_models"], json!(["gpt-4o"]));

        let id = created["id"].as_i64().unwrap();
        let (status, patched) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({ "allowed_models": null }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(patched["allowed_models"], Value::Null);
    }

    #[tokio::test]
    async fn invalid_base_url_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["base_url"] = json!("not a url");
        let (status, error) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"].as_str().unwrap().contains("base_url"));
    }

    #[tokio::test]
    async fn non_http_scheme_base_url_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["base_url"] = json!("ftp://api.example.com");
        let (status, _) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn allowed_models_with_empty_string_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["allowed_models"] = json!(["gpt-4o", ""]);
        let (status, _) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn allowed_models_non_array_is_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["allowed_models"] = json!("gpt-4o");
        let (status, _) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_without_bound_keys_removes_channel() {
        let state = test_state().await;
        let (_, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        let id = created["id"].as_i64().unwrap();
        let (status, body) =
            send_raw(&state, "DELETE", &format!("/admin/channels/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);

        let (status, _) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_with_bound_key_returns_conflict() {
        let state = test_state().await;
        let (_, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        let channel_id = created["id"].as_i64().unwrap();
        sqlx::query(
            "INSERT INTO api_keys (key_hash, display_prefix, channel_id, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind("deadbeefhash")
        .bind("hyd-abcd")
        .bind(channel_id)
        .bind(crate::db::timestamp::now_utc())
        .execute(state.db.as_sqlite())
        .await
        .unwrap();

        let (status, error) = send_raw(
            &state,
            "DELETE",
            &format!("/admin/channels/{channel_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(error["error"].as_str().unwrap().contains("bound"));

        let (status, _) = send_json(
            &state,
            "GET",
            &format!("/admin/channels/{channel_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_channel_returns_not_found() {
        let state = test_state().await;
        let (status, _) = send_json(&state, "GET", "/admin/channels/999", Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send_json(
            &state,
            "PATCH",
            "/admin/channels/999",
            json!({ "enabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send_raw(&state, "DELETE", "/admin/channels/999", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_key_is_masked_in_every_response() {
        let state = test_state().await;
        let (status, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        assert_eq!(status, StatusCode::CREATED);
        let raw = created.to_string();
        assert!(!raw.contains(UPSTREAM_KEY));
        assert_eq!(created["has_api_key"], true);
        assert_eq!(created["api_key_masked"], json!("sk-u...7890"));

        let (_, listed) = send_json(&state, "GET", "/admin/channels", Value::Null).await;
        assert!(!listed.to_string().contains(UPSTREAM_KEY));

        let id = created["id"].as_i64().unwrap();
        let (_, patched) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({ "api_key": UPSTREAM_KEY }),
        )
        .await;
        assert!(!patched.to_string().contains(UPSTREAM_KEY));

        let (_, fetched) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert!(!fetched.to_string().contains(UPSTREAM_KEY));
    }

    /// A channel with prices stored as integer micro-USD, exactly as the
    /// API returns them.
    fn priced_body() -> Value {
        json!({
            "name": "primary",
            "base_url": "https://api.example.com",
            "api_key": UPSTREAM_KEY,
            "allowed_models": ["gpt-4o", "gpt-4o-mini"],
            "model_prices": {
                "gpt-4o": {
                    "input": "2.50",
                    "output": "10.00",
                    "cached_input": "1.25",
                    "cached_output": "5.00"
                }
            }
        })
    }

    #[tokio::test]
    async fn create_stores_prices_as_integer_micro_usd_and_returns_them() {
        let state = test_state().await;
        let (status, created) = send_json(&state, "POST", "/admin/channels", priced_body()).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(
            created["model_prices"],
            json!({
                "gpt-4o": {
                    "input": 2_500_000,
                    "output": 10_000_000,
                    "cached_input": 1_250_000,
                    "cached_output": 5_000_000
                }
            })
        );

        // The stored integers survive a round trip through the database.
        let id = created["id"].as_i64().unwrap();
        let (status, fetched) =
            send_json(&state, "GET", &format!("/admin/channels/{id}"), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fetched["model_prices"], created["model_prices"]);

        let (_, listed) = send_json(&state, "GET", "/admin/channels", Value::Null).await;
        assert_eq!(listed[0]["model_prices"], created["model_prices"]);
    }

    #[tokio::test]
    async fn create_without_prices_stores_null() {
        let state = test_state().await;
        let (status, created) = send_json(&state, "POST", "/admin/channels", create_body()).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["model_prices"], Value::Null);
    }

    #[tokio::test]
    async fn create_with_an_empty_price_object_stores_null() {
        let state = test_state().await;
        let mut body = create_body();
        body["model_prices"] = json!({});
        let (status, created) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["model_prices"], Value::Null);
    }

    #[tokio::test]
    async fn patch_replaces_prices_and_leaves_them_alone_when_absent() {
        let state = test_state().await;
        let (_, created) = send_json(&state, "POST", "/admin/channels", priced_body()).await;
        let id = created["id"].as_i64().unwrap();

        // Absent: the stored table stays as it is.
        let (status, patched) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({ "name": "renamed" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(patched["model_prices"], created["model_prices"]);

        // Present: the whole table is replaced, not merged.
        let (status, replaced) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({
                "model_prices": {
                    "gpt-4o-mini": {
                        "input": "0.15",
                        "output": "0.60",
                        "cached_input": "0.075",
                        "cached_output": "0.30"
                    }
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replaced}");
        assert_eq!(
            replaced["model_prices"],
            json!({
                "gpt-4o-mini": {
                    "input": 150_000,
                    "output": 600_000,
                    "cached_input": 75_000,
                    "cached_output": 300_000
                }
            })
        );

        // Explicit null clears the table.
        let (status, cleared) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({ "model_prices": null }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cleared["model_prices"], Value::Null);
        assert_eq!(cleared["allowed_models"], json!(["gpt-4o", "gpt-4o-mini"]));
    }

    #[tokio::test]
    async fn prices_finer_than_micro_usd_are_rejected_with_400() {
        let state = test_state().await;
        let mut body = create_body();
        body["model_prices"] = json!({
            "gpt-4o": {
                "input": "0.0000005",
                "output": "1",
                "cached_input": "1",
                "cached_output": "1"
            }
        });
        let (status, error) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            error["error"].as_str().unwrap().contains("stored exactly"),
            "unexpected message: {error}"
        );
    }

    #[tokio::test]
    async fn malformed_price_tables_are_rejected_with_400() {
        let state = test_state().await;
        for prices in [
            json!(-1),
            json!("2.50"),
            json!([{ "input": "1" }]),
            json!({ "gpt-4o": "2.50" }),
            json!({ "gpt-4o": { "input": "1", "output": "1", "cached_input": "1" } }),
            json!({ "gpt-4o": { "input": "1", "output": "1", "cached_input": "1", "cached_output": "1", "extra": "1" } }),
            json!({ "gpt-4o": { "input": true, "output": "1", "cached_input": "1", "cached_output": "1" } }),
        ] {
            let mut body = create_body();
            body["model_prices"] = prices.clone();
            let (status, error) = send_json(&state, "POST", "/admin/channels", body).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "prices {prices} must be rejected"
            );
            assert!(error["error"].is_string(), "{error}");
        }
    }

    #[tokio::test]
    async fn prices_outside_the_whitelist_are_rejected() {
        let state = test_state().await;
        let mut body = create_body();
        body["allowed_models"] = json!(["gpt-4o"]);
        body["model_prices"] = json!({
            "gpt-4o-mini": {
                "input": "1",
                "output": "1",
                "cached_input": "1",
                "cached_output": "1"
            }
        });
        let (status, error) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("outside allowed_models"),
            "unexpected message: {error}"
        );

        // Shrinking the whitelist below the stored prices is refused too, so
        // the stored table always stays inside the whitelist.
        let (_, created) = send_json(&state, "POST", "/admin/channels", priced_body()).await;
        let id = created["id"].as_i64().unwrap();
        let (status, error) = send_json(
            &state,
            "PATCH",
            &format!("/admin/channels/{id}"),
            json!({ "allowed_models": ["gpt-4o-mini"] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            error["error"].as_str().unwrap().contains("gpt-4o"),
            "unexpected message: {error}"
        );
        assert_eq!(created["model_prices"]["gpt-4o"]["input"], 2_500_000);
    }

    #[tokio::test]
    async fn prices_need_no_whitelist_when_the_channel_serves_every_model() {
        let state = test_state().await;
        let body = json!({
            "name": "open",
            "base_url": "https://api.example.com",
            "api_key": UPSTREAM_KEY,
            "model_prices": {
                "anything-goes": {
                    "input": "1",
                    "output": "1",
                    "cached_input": "1",
                    "cached_output": "1"
                }
            }
        });
        let (status, created) = send_json(&state, "POST", "/admin/channels", body).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["allowed_models"], Value::Null);
        assert_eq!(created["model_prices"]["anything-goes"]["input"], 1_000_000);
    }
}
