//! Runtime sqlx queries for the `channels` table.

use sqlx::{Row, SqlitePool};

/// A stored channel row. `api_key` is the upstream secret and must never
/// cross the HTTP boundary unmasked.
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub allowed_models: Option<Vec<String>>,
    pub enabled: bool,
    pub created_at: String,
}

/// A new channel to insert. `allowed_models` `None` stores NULL (all models
/// allowed).
pub struct NewChannel {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub allowed_models: Option<Vec<String>>,
}

/// Partial update for a channel. `None` fields are left unchanged.
/// `allowed_models` is a double option: the outer `None` leaves the column
/// untouched, `Some(None)` clears it back to NULL (all models allowed),
/// `Some(Some(list))` sets the whitelist.
pub struct ChannelPatch {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub allowed_models: Option<Option<Vec<String>>>,
    pub enabled: Option<bool>,
}

/// Result of a channel delete attempt.
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    HasBoundKeys,
}

const SELECT_COLUMNS: &str = "id, name, base_url, api_key, allowed_models, enabled, created_at";

fn map_channel(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<Channel> {
    let allowed_raw: Option<String> = row.try_get("allowed_models")?;
    let allowed_models = allowed_raw
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|e| sqlx::Error::ColumnDecode {
                index: "allowed_models".into(),
                source: Box::new(e),
            })
        })
        .transpose()?;
    Ok(Channel {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        base_url: row.try_get("base_url")?,
        api_key: row.try_get("api_key")?,
        allowed_models,
        enabled: row.try_get("enabled")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn create(pool: &SqlitePool, channel: &NewChannel) -> sqlx::Result<Channel> {
    let allowed_json = allowed_models_json(channel.allowed_models.as_ref());
    let row = sqlx::query(&format!(
        "INSERT INTO channels (name, base_url, api_key, allowed_models, enabled, created_at) \
         VALUES (?, ?, ?, ?, 1, datetime('now')) \
         RETURNING {SELECT_COLUMNS}"
    ))
    .bind(&channel.name)
    .bind(&channel.base_url)
    .bind(&channel.api_key)
    .bind(allowed_json)
    .fetch_one(pool)
    .await?;
    map_channel(&row)
}

pub async fn list(pool: &SqlitePool) -> sqlx::Result<Vec<Channel>> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM channels ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(map_channel).collect()
}

pub async fn find(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Channel>> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM channels WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_channel).transpose()
}

pub async fn update(
    pool: &SqlitePool,
    id: i64,
    patch: &ChannelPatch,
) -> sqlx::Result<Option<Channel>> {
    let mut assignments: Vec<&str> = Vec::new();
    if patch.name.is_some() {
        assignments.push("name = ?");
    }
    if patch.base_url.is_some() {
        assignments.push("base_url = ?");
    }
    if patch.api_key.is_some() {
        assignments.push("api_key = ?");
    }
    if patch.allowed_models.is_some() {
        assignments.push("allowed_models = ?");
    }
    if patch.enabled.is_some() {
        assignments.push("enabled = ?");
    }
    if assignments.is_empty() {
        return find(pool, id).await;
    }

    let sql = format!(
        "UPDATE channels SET {} WHERE id = ? RETURNING {SELECT_COLUMNS}",
        assignments.join(", ")
    );
    let mut query = sqlx::query(&sql);
    if let Some(name) = &patch.name {
        query = query.bind(name);
    }
    if let Some(base_url) = &patch.base_url {
        query = query.bind(base_url);
    }
    if let Some(api_key) = &patch.api_key {
        query = query.bind(api_key);
    }
    if let Some(allowed_models) = &patch.allowed_models {
        query = query.bind(allowed_models_json(allowed_models.as_ref()));
    }
    if let Some(enabled) = patch.enabled {
        query = query.bind(enabled);
    }
    query = query.bind(id);

    let row = query.fetch_optional(pool).await?;
    row.as_ref().map(map_channel).transpose()
}

pub async fn delete(pool: &SqlitePool, id: i64) -> sqlx::Result<DeleteOutcome> {
    let exists = sqlx::query("SELECT 1 AS one FROM channels WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Ok(DeleteOutcome::NotFound);
    }
    // Any bound key row, alive or soft-deleted, blocks the delete: the FK
    // from api_keys and the request_logs history both depend on it.
    let bound_keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE channel_id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    if bound_keys > 0 {
        return Ok(DeleteOutcome::HasBoundKeys);
    }
    sqlx::query("DELETE FROM channels WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(DeleteOutcome::Deleted)
}

fn allowed_models_json(models: Option<&Vec<String>>) -> Option<String> {
    models.map(|m| serde_json::to_string(m).expect("Vec<String> serialization is infallible"))
}
