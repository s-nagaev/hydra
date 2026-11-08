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
use crate::http::admin_auth::AdminAuth;
use crate::http::admin_keys::{issue_key, KeysError};
use crate::http::admin_ui_channels::ToggleForm;
use crate::http::AppState;

/// One table row; the full key material never reaches this view.
pub struct KeyRow {
    pub id: i64,
    pub display_prefix: String,
    pub name: Option<String>,
    pub channel_name: String,
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

/// State of the issue-key form.
pub struct KeyForm {
    pub channel_options: Vec<ChannelOption>,
    pub name: String,
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
}

fn internal_error(error: &sqlx::Error) -> String {
    tracing::error!(error = %error, "admin ui key query failed");
    "internal server error".to_string()
}

async fn rows(state: &AppState) -> Result<Vec<KeyRow>, sqlx::Error> {
    let key_list = keys::list(&state.db).await?;
    let channel_names: HashMap<i64, String> = channels::list(&state.db)
        .await?
        .into_iter()
        .map(|channel| (channel.id, channel.name))
        .collect();
    Ok(key_list
        .into_iter()
        .map(|key| {
            let channel_name = channel_names
                .get(&key.channel_id)
                .cloned()
                .unwrap_or_else(|| format!("#{}", key.channel_id));
            key_row(key, channel_name)
        })
        .collect())
}

fn key_row(key: ApiKey, channel_name: String) -> KeyRow {
    KeyRow {
        id: key.id,
        display_prefix: key.display_prefix,
        name: key.name,
        channel_name,
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
pub async fn page(State(state): State<AppState>, _auth: AdminAuth) -> KeysPage {
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

/// "New key" action: opens the form. Without an enabled channel there is
/// nothing to bind a key to, so the banner says so instead.
async fn new_form(State(state): State<AppState>, _auth: AdminAuth) -> KeysPanel {
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
                    channel_options: options,
                    name: String::new(),
                }),
                None,
                None,
            )
            .await
        }
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Issue-key form POST. Success renders the panel with the one-time reveal
/// modal embedded: the full key is delivered in this response fragment and
/// nowhere else. Channel errors re-render the form with the banner message
/// from the shared issuance path.
async fn create(
    State(state): State<AppState>,
    _auth: AdminAuth,
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
    match issue_key(&state, submitted.channel_id, name.clone()).await {
        Ok((_stored, generated)) => {
            let channel_name = match channels::find(&state.db, submitted.channel_id).await {
                Ok(Some(channel)) => channel.name,
                _ => String::new(),
            };
            let reveal = RevealModal {
                full_key: generated.full,
                channel_name,
            };
            panel(&state, None, None, Some(reveal)).await
        }
        Err(KeysError::NotFound(message)) | Err(KeysError::BadRequest(message)) => {
            let options = channel_options(&state, Some(submitted.channel_id))
                .await
                .unwrap_or_default();
            let form = KeyForm {
                channel_options: options,
                name: name.unwrap_or_default(),
            };
            panel(&state, Some(form), Some(message), None).await
        }
        Err(KeysError::Database(e)) => panel(&state, None, Some(internal_error(&e)), None).await,
        Err(KeysError::Conflict(message)) => panel(&state, None, Some(message), None).await,
    }
}

/// Enable/disable toggle for an alive key. Soft-deleted keys are immutable:
/// the banner carries the same message the JSON API returns.
async fn toggle(
    State(state): State<AppState>,
    _auth: AdminAuth,
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
    match keys::patch(
        &state.db,
        id,
        &KeyPatch {
            name: None,
            enabled: Some(submitted.enabled),
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
        Err(e) => panel(&state, None, Some(internal_error(&e)), None).await,
    }
}

/// Soft delete: the key stops authenticating, the row and its request logs
/// stay. The confirm dialog on the button spells that out.
async fn delete(State(state): State<AppState>, _auth: AdminAuth, Path(id): Path<i64>) -> KeysPanel {
    match keys::delete(&state.db, id).await {
        Ok(DeleteOutcome::Deleted) => panel(&state, None, None, None).await,
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
        .route("/keys/{id}/toggle", post(toggle))
        .route("/keys/{id}/delete", post(delete))
}
