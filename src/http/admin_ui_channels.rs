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

use crate::db::channels::{self, Channel, ChannelPatch, DeleteOutcome, NewChannel};
use crate::http::admin_auth::AdminAuth;
use crate::http::admin_channels::{
    mask_api_key, validate_allowed_models, validate_base_url, validate_channel_api_key,
    validate_channel_name, AdminError,
};
use crate::http::AppState;

/// One table row. The upstream key is masked before it leaves the server,
/// exactly like the JSON views.
pub struct ChannelRow {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key_masked: String,
    pub allowed_models: Option<Vec<String>>,
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
}

impl ChannelFormErrors {
    fn any(&self) -> bool {
        self.name.is_some()
            || self.base_url.is_some()
            || self.api_key.is_some()
            || self.allowed_models.is_some()
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
}

#[derive(Deserialize)]
pub struct ChannelEditForm {
    pub name: String,
    pub base_url: String,
    /// Empty or absent means "keep the current key" on the edit form.
    pub api_key: Option<String>,
    pub allowed_models: Option<String>,
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

fn row_from(channel: Channel) -> ChannelRow {
    ChannelRow {
        id: channel.id,
        api_key_masked: mask_api_key(&channel.api_key),
        name: channel.name,
        base_url: channel.base_url,
        allowed_models: channel.allowed_models,
        enabled: channel.enabled,
    }
}

async fn rows(state: &AppState) -> Result<Vec<ChannelRow>, sqlx::Error> {
    Ok(channels::list(&state.db)
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
pub async fn page(State(state): State<AppState>, _auth: AdminAuth) -> ChannelsPage {
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
async fn new_form(State(state): State<AppState>, _auth: AdminAuth) -> ChannelsPanel {
    panel(
        &state,
        Some(ChannelForm {
            action: create_action(),
            editing: false,
            name: String::new(),
            base_url: String::new(),
            allowed_models_text: String::new(),
            errors: ChannelFormErrors::default(),
        }),
        None,
        None,
    )
    .await
}

/// "Edit" action: opens the form prefilled with the channel's current
/// values; the upstream key field stays empty so the stored secret never
/// crosses the wire.
async fn edit_form(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
) -> ChannelsPanel {
    match channels::find(&state.db, id).await {
        Ok(Some(channel)) => {
            let allowed_models_text = channel
                .allowed_models
                .clone()
                .unwrap_or_default()
                .join("\n");
            panel(
                &state,
                Some(ChannelForm {
                    action: edit_action(id),
                    editing: true,
                    name: channel.name,
                    base_url: channel.base_url,
                    allowed_models_text,
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
    _auth: AdminAuth,
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
    let (errors, allowed) = collect_errors(
        &submitted.name,
        &submitted.base_url,
        Some(&submitted.api_key),
        &parsed_models,
    );
    if errors.any() {
        let form = ChannelForm {
            action: create_action(),
            editing: false,
            name: submitted.name,
            base_url: submitted.base_url,
            allowed_models_text: parsed_models.join("\n"),
            errors,
        };
        return panel(&state, Some(form), None, None).await;
    }
    match channels::create(
        &state.db,
        &NewChannel {
            name: submitted.name,
            base_url: submitted.base_url,
            api_key: submitted.api_key,
            allowed_models: allowed,
        },
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
    _auth: AdminAuth,
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
    let (errors, allowed) = collect_errors(
        &submitted.name,
        &submitted.base_url,
        new_api_key,
        &parsed_models,
    );
    if errors.any() {
        let form = ChannelForm {
            action: edit_action(id),
            editing: true,
            name: submitted.name,
            base_url: submitted.base_url,
            allowed_models_text: parsed_models.join("\n"),
            errors,
        };
        return panel(&state, Some(form), None, None).await;
    }
    // Invalidate every entry bound to the channel before and after: the
    // edit form can rotate base_url and the upstream api_key together.
    state.cache.invalidate_channel(id);
    match channels::update(
        &state.db,
        id,
        &ChannelPatch {
            name: Some(submitted.name),
            base_url: Some(submitted.base_url),
            api_key: new_api_key.map(str::to_string),
            allowed_models: Some(allowed),
            enabled: None,
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

/// Enable/disable toggle. The form carries the target state, so no read
/// of the current row is needed.
async fn toggle(
    State(state): State<AppState>,
    _auth: AdminAuth,
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
    _auth: AdminAuth,
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
}
