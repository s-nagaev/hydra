//! Admin UI handlers for the client keys page.
//!
//! Same submission pattern as the channels page: every interaction targets
//! `#keys-panel` with an outerHTML swap and the server always answers with
//! the re-rendered panel fragment. Key issuance goes through the same
//! shared `issue_key` path as the JSON API, and the full plaintext key is
//! embedded in exactly one place: the one-time reveal modal inside the
//! creation response fragment. There is no route that can serve the key
//! again; the list and every later render carry the display prefix only.
//! The modal is non-dismissable by design (no backdrop close, no Esc
//! handling, Close disabled) until the admin checks "I have stored the
//! key"; the checkbox, clipboard copy and curl snippet are wired by the
//! small inline script shipped inside the modal fragment.
//!
//! The create form and the edit form share one template: on create the
//! channel is chosen from the enabled ones, on edit the stored binding is
//! shown read-only next to the two editable fields, the label and the
//! monthly limit. An empty label clears it, exactly like an explicit null
//! through the JSON API, and an empty limit field clears the limit back to
//! unlimited. Mutations go through the JSON API's `mutate_key`, so the
//! resolution cache is invalidated the same way and the UI cannot drift
//! from the API's behavior.
//!
//! The limit column shows the stored amount and, for a key whose channel
//! cannot be limited (the channel is not enabled, serves every upstream
//! model, or has an incomplete price list), a badge saying the limit is
//! inactive. The badge is informational: the limit stays stored, and it
//! becomes effective once the channel's prices are complete.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
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

/// An option of the channel select: enabled channels only.
pub struct ChannelOption {
    pub id: i64,
    pub name: String,
    pub selected: bool,
}

/// Validation messages, rendered under the matching input. The strings are
/// the shared limit parser's own error texts. The optional name has nothing
/// to validate, so it carries no error arm.
#[derive(Default)]
pub struct KeyFormErrors {
    pub monthly_limit_usd: Option<String>,
}

impl KeyFormErrors {
    fn any(&self) -> bool {
        self.monthly_limit_usd.is_some()
    }
}

/// State of the issue/edit form as it is rendered: the hx-post target,
/// whether an existing key is being edited, the channel select on create or
/// the read-only channel name on edit, the submitted or prefilled values,
/// and current validation errors.
pub struct KeyForm {
    pub action: String,
    pub editing: bool,
    pub channel_options: Vec<ChannelOption>,
    pub channel_name: String,
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

/// The panel fragment every handler on this page renders.
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
}

fn internal_error(error: &sqlx::Error) -> String {
    tracing::error!(error = %error, "admin ui key query failed");
    "internal server error".to_string()
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

async fn channel_name(state: &AppState, channel_id: i64) -> String {
    match channels::find(&state.db, channel_id).await {
        Ok(Some(channel)) => channel.name,
        _ => format!("#{channel_id}"),
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

/// Options for the channel select: enabled channels only, the given id
/// preselected when it is among them (used to preserve the admin's choice
/// when the form is re-rendered after an error).
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
        })
        .collect())
}

/// Re-renders the create form with the submitted values, its per-field
/// errors and an optional banner.
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
            editing: false,
            channel_options: options,
            channel_name: String::new(),
            name,
            monthly_limit_usd,
            errors,
        }),
        banner,
        None,
    )
    .await
}

/// Re-renders the edit form with the submitted values and its per-field
/// errors. The channel binding is not editable, so its name is read back
/// from the stored row for display.
async fn edit_panel(
    state: &AppState,
    id: i64,
    name: String,
    monthly_limit_usd: String,
    errors: KeyFormErrors,
) -> KeysPanel {
    let channel_name = match keys::find(&state.db, id).await {
        Ok(Some(key)) => channel_name(state, key.channel_id).await,
        _ => String::new(),
    };
    panel(
        state,
        Some(KeyForm {
            action: edit_action(id),
            editing: true,
            channel_options: Vec::new(),
            channel_name,
            name,
            monthly_limit_usd,
            errors,
        }),
        None,
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
                    editing: false,
                    channel_options: options,
                    channel_name: String::new(),
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

/// "Edit" action: opens the form prefilled with the key's current label and
/// limit; the channel binding is shown read-only. A soft-deleted key cannot
/// be edited, matching the JSON API's 409.
async fn edit_form(State(state): State<AppState>, _auth: UiAuth, Path(id): Path<i64>) -> KeysPanel {
    match keys::find(&state.db, id).await {
        Ok(Some(key)) if key.deleted_at.is_none() => {
            let monthly_limit_usd = key
                .monthly_limit_micro_usd
                .map(limits::format_usd)
                .unwrap_or_default();
            edit_panel(
                &state,
                id,
                key.name.unwrap_or_default(),
                monthly_limit_usd,
                KeyFormErrors::default(),
            )
            .await
        }
        Ok(Some(_)) => {
            panel(
                &state,
                None,
                Some("key is deleted and cannot be edited".to_string()),
                None,
            )
            .await
        }
        Ok(None) => panel(&state, None, Some("key not found".to_string()), None).await,
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
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
            let channel_name = channel_name(&state, submitted.channel_id).await;
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
/// unlimited, and a present value replaces it.
async fn update(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
    payload: Result<axum::Form<KeyEditForm>, FormRejection>,
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
    if errors.any() {
        return edit_panel(&state, id, name, raw_limit, errors).await;
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
        },
    )
    .await
    {
        Ok(PatchOutcome::Updated(_)) => panel(&state, None, None, None).await,
        Ok(PatchOutcome::NotFound) => {
            panel(&state, None, Some("key not found".to_string()), None).await
        }
        Ok(PatchOutcome::Deleted) => {
            panel(
                &state,
                None,
                Some("key is deleted and cannot be edited".to_string()),
                None,
            )
            .await
        }
        Err(KeysError::Database(e)) => panel(&state, None, Some(internal_error(&e)), None).await,
        Err(KeysError::NotFound(message))
        | Err(KeysError::BadRequest(message))
        | Err(KeysError::Conflict(message)) => panel(&state, None, Some(message), None).await,
    }
}

/// Enable/disable toggle for an alive key. Soft-deleted keys are immutable:
/// the banner carries the same message the JSON API returns.
async fn toggle(
    State(state): State<AppState>,
    _auth: UiAuth,
    Path(id): Path<i64>,
    payload: Result<axum::Form<ToggleForm>, FormRejection>,
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
    match mutate_key(
        &state,
        id,
        &KeyPatch {
            name: None,
            enabled: Some(submitted.enabled),
            monthly_limit_micro_usd: None,
        },
    )
    .await
    {
        Ok(PatchOutcome::Updated(_)) => panel(&state, None, None, None).await,
        Ok(PatchOutcome::NotFound) => {
            panel(&state, None, Some("key not found".to_string()), None).await
        }
        Ok(PatchOutcome::Deleted) => {
            panel(
                &state,
                None,
                Some("key is deleted and cannot be edited".to_string()),
                None,
            )
            .await
        }
        Err(KeysError::Database(e)) => panel(&state, None, Some(internal_error(&e)), None).await,
        Err(KeysError::NotFound(message))
        | Err(KeysError::BadRequest(message))
        | Err(KeysError::Conflict(message)) => panel(&state, None, Some(message), None).await,
    }
}

/// Soft delete: the key stops authenticating, the row and its request logs
/// stay. The confirm dialog on the button spells that out.
async fn delete(State(state): State<AppState>, _auth: UiAuth, Path(id): Path<i64>) -> KeysPanel {
    // Before and after, like the resolution cache: a deleted key must neither
    // authenticate nor keep a monthly-limit verdict alive.
    state.cache.invalidate_key(id);
    state.limit_flags.invalidate(id);
    match keys::delete(&state.db, id).await {
        Ok(DeleteOutcome::Deleted) => {
            state.cache.invalidate_key(id);
            state.limit_flags.invalidate(id);
            panel(&state, None, None, None).await
        }
        Ok(DeleteOutcome::NotFound) => {
            panel(&state, None, Some("key not found".to_string()), None).await
        }
        Ok(DeleteOutcome::AlreadyDeleted) => {
            panel(
                &state,
                None,
                Some("key is already deleted".to_string()),
                None,
            )
            .await
        }
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Key routes for the `/admin/ui` nest; merged before the CSRF layer so
/// every POST sits inside the guarded nest.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/keys", get(page).post(create))
        .route("/keys/form", get(new_form))
        .route("/keys/{id}/edit", get(edit_form))
        .route("/keys/{id}", post(update))
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
        };
        assert!(errors.any());
    }
}
