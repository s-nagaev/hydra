//! Admin UI handlers for the keys list and the per-key detail page.
//!
//! Two submission patterns live here. The list keeps the htmx flow: the "New
//! key" action and the issue form target the page's `#keys-panel` element
//! with an `outerHTML` swap, and the server answers with the freshly rendered
//! panel fragment. A row click leaves the list for the full detail page
//! (`GET /admin/ui/keys/{id}`), which carries no htmx: its edit, toggle and
//! delete forms post back to the same page in a plain browser round trip.
//! Successful mutations re-render the detail page with a success notice,
//! failed validations re-render its form with the submitted values and
//! per-field errors, and a soft delete re-renders the page read-only, since
//! the key still exists.
//!
//! Key issuance goes through the same shared `issue_key` path as the JSON
//! API, and the full plaintext key is embedded in exactly one place: the
//! one-time reveal modal inside the creation response fragment. There is no
//! route that can serve the key again; the list, the detail page and every
//! later render carry the display prefix only.
//!
//! The create form and the detail page's edit form share one template block:
//! both carry the channel `<select>`. On the detail page the select holds
//! every enabled channel plus the key's current binding, so a key bound to a
//! channel that was disabled afterwards keeps its binding and a save that
//! changes nothing else cannot silently move it. `POST /admin/ui/keys/{id}`
//! patches `channel_id` only when the submitted binding differs from the
//! stored one, and refuses an unknown or disabled channel with a form error.
//! All mutations go through the JSON API's `mutate_key`, so the resolution
//! cache is invalidated the same way and the UI cannot drift from the API.
//!
//! A soft-deleted key renders read-only: metadata, the deleted state and the
//! explanation that it cannot be edited, but no form of any kind.
//!
//! The limit column shows the stored amount and, for a key whose channel
//! cannot be limited (the channel is not enabled, serves every upstream
//! model, or has an incomplete price list), a badge saying the limit is
//! inactive. The badge is informational: the limit stays stored, and it
//! becomes effective once the channel's prices are complete. The badge used
//! to carry that explanation in a `title` tooltip, which the row overlay
//! makes unreachable; the detail page states it as visible text instead.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::db::channels;
use crate::db::keys::{self, ApiKey, DeleteOutcome, KeyPatch, PatchOutcome};
use crate::http::admin_keys::{issue_key, mutate_key, KeysError};
use crate::http::admin_ui_channels::ToggleForm;
use crate::http::ui_auth::UiAuth;
use crate::http::AppState;
use crate::limits;

/// The one wording that explains a soft-deleted key: the read-only detail
/// page shows it as a notice, a mutation refused on a deleted key as the
/// banner, matching the JSON API's 409 message.
const DELETED_MESSAGE: &str = "key is deleted and cannot be edited";

/// One table row; the full key material never reaches this view. The limit
/// arrives as a ready-to-render amount and the inactive state is computed
/// here, so the template stays logic-free.
pub struct KeyRow {
    pub id: i64,
    pub display_prefix: String,
    pub name: Option<String>,
    pub channel_name: String,
    /// The stored limit as `$50.00`, or `None` for an unlimited key.
    pub monthly_limit: Option<String>,
    /// The key has a limit but its channel cannot be limited yet.
    pub limit_inactive: bool,
    pub enabled: bool,
    pub deleted: bool,
    pub created_at: String,
}

/// An option of a channel select. `disabled` is set on the detail page for a
/// channel offered only because it is the key's current binding or the
/// rejected choice of a previous attempt; a channel that simply cannot take a
/// new key is never offered under a false name.
pub struct ChannelOption {
    pub id: i64,
    pub name: String,
    pub selected: bool,
    pub disabled: bool,
}

/// Validation messages, rendered under the matching input. The strings are
/// the shared limit parser's own error texts, or the shared channel
/// resolution's. The optional name has nothing to validate, so it carries no
/// error arm.
#[derive(Default)]
pub struct KeyFormErrors {
    pub channel_id: Option<String>,
    pub monthly_limit_usd: Option<String>,
}

impl KeyFormErrors {
    fn any(&self) -> bool {
        self.channel_id.is_some() || self.monthly_limit_usd.is_some()
    }
}

/// State of the issue/edit form as it is rendered: the POST target, the
/// channel select, the submitted or prefilled values, and current validation
/// errors.
pub struct KeyForm {
    pub action: String,
    pub channel_options: Vec<ChannelOption>,
    pub name: String,
    pub monthly_limit_usd: String,
    pub errors: KeyFormErrors,
}

/// The one-time reveal modal payload; `full_key` appears in the rendered
/// fragment exactly once, in the modal itself.
pub struct RevealModal {
    pub full_key: String,
    pub channel_name: String,
}

/// The panel fragment the create flow renders: the table plus the optional
/// issue form, its banner and the reveal modal.
#[derive(Template, WebTemplate)]
#[template(path = "partials/keys_panel.html")]
pub struct KeysPanel {
    pub rows: Vec<KeyRow>,
    pub form: Option<KeyForm>,
    pub error: Option<String>,
    pub reveal: Option<RevealModal>,
}

/// Full page wrapper around the same panel for direct navigation.
#[derive(Template, WebTemplate)]
#[template(path = "keys.html")]
pub struct KeysPage {
    pub version: &'static str,
    pub section: &'static str,
    pub rows: Vec<KeyRow>,
    pub form: Option<KeyForm>,
    pub error: Option<String>,
    pub reveal: Option<RevealModal>,
}

/// The metadata block of one key's detail page; every value is
/// display-ready, and the plaintext key is not part of it.
pub struct KeyDetail {
    pub display_prefix: String,
    pub name: Option<String>,
    pub channel_id: i64,
    pub channel_name: String,
    /// The stored limit as `$50.00`, or `None` for an unlimited key.
    pub monthly_limit: Option<String>,
    pub limit_inactive: bool,
    pub enabled: bool,
    pub deleted: bool,
    pub created_at: String,
}

/// The per-key detail page. A missing key renders the same page with a
/// not-found banner and no forms, a soft-deleted key the read-only state with
/// the deleted notice, both with 200. `notice` carries a successful-mutation
/// message or the deleted state, `error` a failure banner.
#[derive(Template, WebTemplate)]
#[template(path = "key_detail.html")]
pub struct KeyDetailPage {
    pub version: &'static str,
    pub section: &'static str,
    pub id: i64,
    pub key: Option<KeyDetail>,
    pub form: Option<KeyForm>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct KeyCreateForm {
    pub channel_id: i64,
    pub name: Option<String>,
    pub monthly_limit_usd: Option<String>,
}

#[derive(Deserialize)]
pub struct KeyEditForm {
    pub name: String,
    pub monthly_limit_usd: Option<String>,
    /// The channel select always submits a binding; an absent field leaves
    /// the stored one alone.
    pub channel_id: Option<i64>,
}

fn internal_error(error: &sqlx::Error) -> String {
    tracing::error!(error = %error, "admin ui key query failed");
    "internal server error".to_string()
}

/// Splits the shared key errors the way the UI reports them: a storage
/// failure is logged and reported opaquely, the validation and state messages
/// are the JSON API's own texts.
fn keys_error_message(error: KeysError) -> String {
    match error {
        KeysError::Database(error) => internal_error(&error),
        other => other.to_string(),
    }
}

fn create_action() -> String {
    "/admin/ui/keys".to_string()
}

fn edit_action(id: i64) -> String {
    format!("/admin/ui/keys/{id}")
}

/// `$50.00`, or `$0.000001` for a limit below one cent.
fn money_display(micro: i64) -> String {
    format!("${}", limits::format_usd(micro))
}

/// Parses the limit form field: an empty value means unlimited, which
/// clears a stored limit, and anything else must be an exact decimal USD
/// amount. An absent field is the same as an empty one.
fn parse_limit_field(raw: &str) -> Result<Option<i64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    limits::parse_usd_str(trimmed)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Name and limit-eligibility of one channel, as the detail page needs them.
/// A channel that cannot be read falls back to `#id`, the same placeholder
/// the list uses.
async fn channel_context(state: &AppState, channel_id: i64) -> (String, bool) {
    match channels::find_priced(&state.db, channel_id).await {
        Ok(Some(priced)) => {
            let limitable = priced.limitable();
            (priced.channel.name, limitable)
        }
        _ => (format!("#{channel_id}"), false),
    }
}

async fn rows(state: &AppState) -> Result<Vec<KeyRow>, sqlx::Error> {
    let key_list = keys::list(&state.db).await?;
    // The inactive-limit badge depends on the bound channel's limitable
    // state, so the channels are read with their price tables, exactly as
    // the channels panel computes it.
    let bound_channels: HashMap<i64, (String, bool)> = channels::list_priced(&state.db)
        .await?
        .into_iter()
        .map(|priced| {
            let limitable = priced.limitable();
            (priced.channel.id, (priced.channel.name, limitable))
        })
        .collect();
    Ok(key_list
        .into_iter()
        .map(|key| {
            let (channel_name, limitable) = bound_channels
                .get(&key.channel_id)
                .cloned()
                .unwrap_or_else(|| (format!("#{}", key.channel_id), false));
            key_row(key, channel_name, limitable)
        })
        .collect())
}

fn key_row(key: ApiKey, channel_name: String, channel_limitable: bool) -> KeyRow {
    let monthly_limit = key.monthly_limit_micro_usd.map(money_display);
    KeyRow {
        id: key.id,
        display_prefix: key.display_prefix,
        name: key.name,
        channel_name,
        limit_inactive: monthly_limit.is_some() && !channel_limitable,
        monthly_limit,
        enabled: key.enabled,
        deleted: key.deleted_at.is_some(),
        created_at: key.created_at,
    }
}

async fn panel(
    state: &AppState,
    form: Option<KeyForm>,
    error: Option<String>,
    reveal: Option<RevealModal>,
) -> KeysPanel {
    let rows = match rows(state).await {
        Ok(rows) => rows,
        Err(e) => {
            return KeysPanel {
                rows: Vec::new(),
                form,
                error: Some(internal_error(&e)),
                reveal,
            }
        }
    };
    KeysPanel {
        rows,
        form,
        error,
        reveal,
    }
}

/// Renders the full keys page for GET /admin/ui/keys.
pub async fn page(State(state): State<AppState>, _auth: UiAuth) -> KeysPage {
    let panel = panel(&state, None, None, None).await;
    KeysPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "keys",
        rows: panel.rows,
        form: panel.form,
        error: panel.error,
        reveal: None,
    }
}

/// Options for the issue form's channel select: enabled channels only, the
/// given id preselected when it is among them (used to preserve the admin's
/// choice when the form is re-rendered after an error).
async fn channel_options(
    state: &AppState,
    selected: Option<i64>,
) -> Result<Vec<ChannelOption>, sqlx::Error> {
    Ok(channels::list(&state.db)
        .await?
        .into_iter()
        .filter(|channel| channel.enabled)
        .map(|channel| ChannelOption {
            id: channel.id,
            name: channel.name,
            selected: Some(channel.id) == selected,
            disabled: false,
        })
        .collect())
}

/// Options for the detail page's channel select: every enabled channel, plus
/// `selected` and `current` even when they are disabled. `current` is the
/// stored binding, which must stay selectable so a save that only changes the
/// label or the limit does not move the key, and `selected` the admin's
/// rejected choice, so the form opens exactly as it was submitted.
async fn detail_channel_options(
    state: &AppState,
    selected: i64,
    current: i64,
) -> Result<Vec<ChannelOption>, sqlx::Error> {
    Ok(channels::list(&state.db)
        .await?
        .into_iter()
        .filter(|channel| channel.enabled || channel.id == selected || channel.id == current)
        .map(|channel| ChannelOption {
            disabled: !channel.enabled,
            selected: channel.id == selected,
            id: channel.id,
            name: channel.name,
        })
        .collect())
}

/// Re-renders the issue form with the submitted values, its per-field errors
/// and an optional banner.
async fn create_panel(
    state: &AppState,
    selected_channel: Option<i64>,
    name: String,
    monthly_limit_usd: String,
    errors: KeyFormErrors,
    banner: Option<String>,
) -> KeysPanel {
    let options = channel_options(state, selected_channel)
        .await
        .unwrap_or_default();
    panel(
        state,
        Some(KeyForm {
            action: create_action(),
            channel_options: options,
            name,
            monthly_limit_usd,
            errors,
        }),
        banner,
        None,
    )
    .await
}

/// "New key" action: opens the form. Without an enabled channel there is
/// nothing to bind a key to, so the banner says so instead.
async fn new_form(State(state): State<AppState>, _auth: UiAuth) -> KeysPanel {
    match channel_options(&state, None).await {
        Ok(options) if options.is_empty() => {
            panel(
                &state,
                None,
                Some("no enabled channels; enable a channel before issuing a key".to_string()),
                None,
            )
            .await
        }
        Ok(options) => {
            panel(
                &state,
                Some(KeyForm {
                    action: create_action(),
                    channel_options: options,
                    name: String::new(),
                    monthly_limit_usd: String::new(),
                    errors: KeyFormErrors::default(),
                }),
                None,
                None,
            )
            .await
        }
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Splits a stored key into the detail page's metadata block and its edit
/// form. A soft-deleted key has no form: the page is read-only.
async fn detail_view(state: &AppState, key: ApiKey) -> (KeyDetail, Option<KeyForm>) {
    let (channel_name, limitable) = channel_context(state, key.channel_id).await;
    let monthly_limit = key.monthly_limit_micro_usd.map(money_display);
    let deleted = key.deleted_at.is_some();
    let detail = KeyDetail {
        display_prefix: key.display_prefix,
        name: key.name.clone(),
        channel_id: key.channel_id,
        channel_name,
        limit_inactive: monthly_limit.is_some() && !limitable,
        monthly_limit,
        enabled: key.enabled,
        deleted,
        created_at: key.created_at,
    };
    if deleted {
        return (detail, None);
    }
    let options = detail_channel_options(state, key.channel_id, key.channel_id)
        .await
        .unwrap_or_default();
    let form = KeyForm {
        action: edit_action(key.id),
        channel_options: options,
        name: key.name.unwrap_or_default(),
        monthly_limit_usd: key
            .monthly_limit_micro_usd
            .map(limits::format_usd)
            .unwrap_or_default(),
        errors: KeyFormErrors::default(),
    };
    (detail, Some(form))
}

/// Renders the reference detail page for one id. Handlers add a success
/// notice or an error banner on top of it.
async fn detail_page(state: &AppState, id: i64) -> KeyDetailPage {
    match keys::find(&state.db, id).await {
        Ok(Some(key)) => {
            let deleted = key.deleted_at.is_some();
            let (detail, form) = detail_view(state, key).await;
            KeyDetailPage {
                version: env!("CARGO_PKG_VERSION"),
                section: "keys",
                id,
                key: Some(detail),
                form,
                // A deleted key explains why it cannot be edited; an alive one
                // carries no notice until a mutation adds one.
                notice: deleted.then(|| DELETED_MESSAGE.to_string()),
                error: None,
            }
        }
        Ok(None) => not_found_page(id),
        Err(e) => KeyDetailPage {
            error: Some(internal_error(&e)),
            ..not_found_page(id)
        },
    }
}

/// The detail page shape with no key loaded: not-found banner, no metadata,
/// no forms.
fn not_found_page(id: i64) -> KeyDetailPage {
    KeyDetailPage {
        version: env!("CARGO_PKG_VERSION"),
        section: "keys",
        id,
        key: None,
        form: None,
        notice: None,
        error: Some("key not found".to_string()),
    }
}

/// Re-renders the detail page as a failed mutation: the read-only deleted
/// notice (if any) is dropped in favour of the error banner, so the page
/// carries one message.
async fn error_page(state: &AppState, id: i64, message: String) -> KeyDetailPage {
    let mut page = detail_page(state, id).await;
    page.notice = None;
    page.error = Some(message);
    page
}

/// Adds a success notice to a re-rendered detail page. A concurrent delete
/// leaves the page read-only with the deleted notice, which the notice must
/// not overwrite.
fn with_notice(mut page: KeyDetailPage, message: &str) -> KeyDetailPage {
    if page.form.is_some() {
        page.notice = Some(message.to_string());
    }
    page
}

/// GET /admin/ui/keys/{id}: the drill-down page for one key, a full render
/// rather than a fragment, with no htmx on it.
async fn detail(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
) -> KeyDetailPage {
    detail_page(&state, id).await
}

/// Issue-key form POST. Success renders the panel with the one-time reveal
/// modal embedded: the full key is delivered in this response fragment and
/// nowhere else. Channel errors re-render the form with the banner message
/// from the shared issuance path.
async fn create(
    State(state): State<AppState>,
    _auth: UiAuth,
    payload: Result<axum::Form<KeyCreateForm>, FormRejection>,
) -> KeysPanel {
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
    // The JSON API treats an absent label as None; an empty or whitespace
    // input maps to the same thing.
    let name = submitted
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let raw_limit = submitted.monthly_limit_usd.unwrap_or_default();
    let mut errors = KeyFormErrors::default();
    let monthly_limit = match parse_limit_field(&raw_limit) {
        Ok(limit) => limit,
        Err(message) => {
            errors.monthly_limit_usd = Some(message);
            None
        }
    };
    if errors.any() {
        return create_panel(
            &state,
            Some(submitted.channel_id),
            name.unwrap_or_default(),
            raw_limit,
            errors,
            None,
        )
        .await;
    }
    match issue_key(&state, submitted.channel_id, name.clone(), monthly_limit).await {
        Ok((_stored, generated)) => {
            let channel_name = channel_context(&state, submitted.channel_id).await.0;
            let reveal = RevealModal {
                full_key: generated.full,
                channel_name,
            };
            panel(&state, None, None, Some(reveal)).await
        }
        Err(KeysError::NotFound(message)) | Err(KeysError::BadRequest(message)) => {
            create_panel(
                &state,
                Some(submitted.channel_id),
                name.unwrap_or_default(),
                raw_limit,
                KeyFormErrors::default(),
                Some(message),
            )
            .await
        }
        Err(KeysError::Database(e)) => panel(&state, None, Some(internal_error(&e)), None).await,
        Err(KeysError::Conflict(message)) => panel(&state, None, Some(message), None).await,
    }
}

/// Edit-key form POST. The form carries the full intended state: an empty
/// label clears the label, an empty limit field clears the limit back to
/// unlimited, and a submitted channel differing from the stored binding moves
/// the key. A channel that does not exist, or is disabled, is a form error
/// and nothing is written; the binding a key already has is never validated,
/// so a key bound to a channel that was disabled afterwards stays editable.
async fn update(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
    payload: Result<axum::Form<KeyEditForm>, FormRejection>,
) -> Response {
    let Ok(submitted) = payload else {
        return error_page(&state, id, "invalid form submission".to_string())
            .await
            .into_response();
    };
    let submitted = submitted.0;
    let stored = match keys::find(&state.db, id).await {
        Ok(Some(key)) => key,
        // A missing id answers the not-found banner, a deleted one its
        // read-only page; neither accepts an edit.
        Ok(None) => return detail_page(&state, id).await.into_response(),
        Err(e) => {
            return error_page(&state, id, internal_error(&e))
                .await
                .into_response()
        }
    };
    if stored.deleted_at.is_some() {
        return error_page(&state, id, DELETED_MESSAGE.to_string())
            .await
            .into_response();
    }
    let name = submitted.name.trim().to_string();
    let raw_limit = submitted.monthly_limit_usd.unwrap_or_default();
    let mut errors = KeyFormErrors::default();
    let limit_patch = match parse_limit_field(&raw_limit) {
        Ok(limit) => Some(limit),
        Err(message) => {
            errors.monthly_limit_usd = Some(message);
            None
        }
    };
    let selected_channel = submitted.channel_id.unwrap_or(stored.channel_id);
    let mut channel_patch = None;
    if selected_channel != stored.channel_id {
        match channels::find(&state.db, selected_channel).await {
            Ok(Some(channel)) if channel.enabled => channel_patch = Some(selected_channel),
            Ok(Some(_)) => errors.channel_id = Some("channel is disabled".to_string()),
            Ok(None) => errors.channel_id = Some("channel not found".to_string()),
            Err(e) => {
                return error_page(&state, id, internal_error(&e))
                    .await
                    .into_response()
            }
        }
    }
    if errors.any() {
        let mut page = detail_page(&state, id).await;
        let options = detail_channel_options(&state, selected_channel, stored.channel_id)
            .await
            .unwrap_or_default();
        page.form = Some(KeyForm {
            action: edit_action(id),
            channel_options: options,
            name,
            monthly_limit_usd: raw_limit,
            errors,
        });
        return page.into_response();
    }
    let name_patch = if name.is_empty() {
        Some(None)
    } else {
        Some(Some(name))
    };
    match mutate_key(
        &state,
        id,
        &KeyPatch {
            name: name_patch,
            enabled: None,
            monthly_limit_micro_usd: limit_patch,
            channel_id: channel_patch,
        },
    )
    .await
    {
        Ok(PatchOutcome::Updated(_)) => {
            with_notice(detail_page(&state, id).await, "key updated").into_response()
        }
        Ok(PatchOutcome::NotFound) => detail_page(&state, id).await.into_response(),
        Ok(PatchOutcome::Deleted) => error_page(&state, id, DELETED_MESSAGE.to_string())
            .await
            .into_response(),
        Err(e) => error_page(&state, id, keys_error_message(e))
            .await
            .into_response(),
    }
}

/// Enable/disable toggle. The form carries the target state, so no read of
/// the current row is needed; success re-renders the detail page.
async fn toggle(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
    payload: Result<axum::Form<ToggleForm>, FormRejection>,
) -> Response {
    let Ok(submitted) = payload else {
        return error_page(&state, id, "invalid form submission".to_string())
            .await
            .into_response();
    };
    match mutate_key(
        &state,
        id,
        &KeyPatch {
            name: None,
            enabled: Some(submitted.enabled),
            monthly_limit_micro_usd: None,
            channel_id: None,
        },
    )
    .await
    {
        Ok(PatchOutcome::Updated(_)) => with_notice(
            detail_page(&state, id).await,
            if submitted.enabled {
                "key enabled"
            } else {
                "key disabled"
            },
        )
        .into_response(),
        Ok(PatchOutcome::NotFound) => detail_page(&state, id).await.into_response(),
        Ok(PatchOutcome::Deleted) => error_page(&state, id, DELETED_MESSAGE.to_string())
            .await
            .into_response(),
        Err(e) => error_page(&state, id, keys_error_message(e))
            .await
            .into_response(),
    }
}

/// Soft delete: the key stops authenticating, the row and its request logs
/// stay. The key still exists, so the answer is its read-only detail page
/// rather than the channels' redirect to the list. The confirm dialog on the
/// button spells the outcome out.
async fn delete(State(state): State<AppState>, _auth: UiAuth, Path(id): Path<i64>) -> Response {
    // Before and after, like the resolution cache: a deleted key must neither
    // authenticate nor keep a monthly-limit verdict alive.
    state.cache.invalidate_key(id);
    state.limit_flags.invalidate(id);
    let page = match keys::delete(&state.db, id).await {
        Ok(DeleteOutcome::Deleted) => {
            state.cache.invalidate_key(id);
            state.limit_flags.invalidate(id);
            detail_page(&state, id).await
        }
        Ok(DeleteOutcome::NotFound) => detail_page(&state, id).await,
        Ok(DeleteOutcome::AlreadyDeleted) => {
            return error_page(&state, id, "key is already deleted".to_string())
                .await
                .into_response()
        }
        Err(e) => {
            return error_page(&state, id, internal_error(&e))
                .await
                .into_response()
        }
    };
    page.into_response()
}

/// Key routes for the `/admin/ui` nest; merged before the CSRF layer so
/// every POST sits inside the guarded nest. The detail GET shares the
/// `/keys/{id}` line with the edit POST; `/keys/form` keeps its own line and
/// wins by matchit's static priority.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/keys", get(page).post(create))
        .route("/keys/form", get(new_form))
        .route("/keys/{id}", get(detail).post(update))
        .route("/keys/{id}/toggle", post(toggle))
        .route("/keys/{id}/delete", post(delete))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_key(limit: Option<i64>, deleted: bool) -> ApiKey {
        ApiKey {
            id: 1,
            key_hash: "hash".to_string(),
            display_prefix: "hyd-4f9dEg2x".to_string(),
            name: Some("ci-bot".to_string()),
            channel_id: 7,
            monthly_limit_micro_usd: limit,
            enabled: true,
            deleted_at: deleted.then(|| "2026-11-09 10:00:00".to_string()),
            created_at: "2026-11-08 00:00:00".to_string(),
        }
    }

    #[test]
    fn an_empty_limit_field_means_unlimited_and_anything_else_is_parsed() {
        assert_eq!(parse_limit_field("").unwrap(), None);
        assert_eq!(parse_limit_field("   ").unwrap(), None);
        assert_eq!(parse_limit_field("50").unwrap(), Some(50_000_000));
        assert_eq!(parse_limit_field(" 0.50 ").unwrap(), Some(500_000));
        assert_eq!(parse_limit_field("0").unwrap(), Some(0));
    }

    #[test]
    fn a_bad_limit_field_is_reported_with_the_shared_message() {
        let error = parse_limit_field("abc").unwrap_err();
        assert!(
            error.contains("non-negative decimal USD amount"),
            "unexpected message: {error}"
        );
        let error = parse_limit_field("-1").unwrap_err();
        assert!(
            error.contains("non-negative decimal USD amount"),
            "unexpected message: {error}"
        );
        let error = parse_limit_field("0.0000005").unwrap_err();
        assert!(
            error.contains("finer than 1e-6 USD"),
            "unexpected message: {error}"
        );
        let error = parse_limit_field("1000000001").unwrap_err();
        assert!(error.contains("above the $1B maximum"), "{error}");
    }

    #[test]
    fn money_display_renders_dollars_from_micro_usd() {
        assert_eq!(money_display(50_000_000), "$50.00");
        assert_eq!(money_display(0), "$0.00");
        assert_eq!(money_display(1), "$0.000001");
    }

    #[test]
    fn row_shows_the_limit_and_only_warns_when_the_channel_cannot_limit() {
        let row = key_row(
            stored_key(Some(50_000_000), false),
            "upstream".to_string(),
            true,
        );
        assert_eq!(row.monthly_limit.as_deref(), Some("$50.00"));
        assert!(!row.limit_inactive);
        assert!(!row.deleted);

        let row = key_row(
            stored_key(Some(50_000_000), false),
            "upstream".to_string(),
            false,
        );
        assert_eq!(row.monthly_limit.as_deref(), Some("$50.00"));
        assert!(row.limit_inactive, "an unlimitable channel warns");

        // No limit: nothing to warn about, however unpriced the channel is.
        let row = key_row(stored_key(None, false), "upstream".to_string(), false);
        assert_eq!(row.monthly_limit, None);
        assert!(!row.limit_inactive);

        let row = key_row(stored_key(Some(1), true), "upstream".to_string(), true);
        assert_eq!(row.monthly_limit.as_deref(), Some("$0.000001"));
        assert!(row.deleted);
    }

    #[test]
    fn a_field_error_marks_the_form_invalid() {
        let errors = KeyFormErrors::default();
        assert!(!errors.any());
        let errors = KeyFormErrors {
            monthly_limit_usd: Some("bad".to_string()),
            ..KeyFormErrors::default()
        };
        assert!(errors.any());
        let errors = KeyFormErrors {
            channel_id: Some("channel not found".to_string()),
            ..KeyFormErrors::default()
        };
        assert!(errors.any());
    }
}
