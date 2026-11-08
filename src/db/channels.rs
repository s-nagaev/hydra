//! Runtime sqlx queries for the `channels` table.
//!
//! One SQL body per query: fragments are written against the backend-aware
//! [`Sql`] builder, which picks the placeholder per backend.

use super::timestamp;
use super::{Arg, Db, Fields, Row, Sql};

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

/// Count of keys bound to a channel, alive or soft-deleted; the number the
/// admin UI shows when a delete is refused. Mirrors the check `delete`
/// runs to make that decision.
pub async fn bound_key_count(db: &Db, id: i64) -> sqlx::Result<i64> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT COUNT(*) FROM api_keys WHERE channel_id = ")
        .bind(Arg::Int(id));
    db.scalar(sql).await
}

/// Result of a channel delete attempt.
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    HasBoundKeys,
}

const SELECT_COLUMNS: &str = "id, name, base_url, api_key, allowed_models, enabled, created_at";

fn map_channel(row: &Row) -> sqlx::Result<Channel> {
    let allowed_raw: Option<String> = row.opt_text("allowed_models")?;
    let allowed_models = allowed_raw
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|e| sqlx::Error::ColumnDecode {
                index: "allowed_models".into(),
                source: Box::new(e),
            })
        })
        .transpose()?;
    Ok(Channel {
        id: row.int("id")?,
        name: row.text("name")?,
        base_url: row.text("base_url")?,
        api_key: row.text("api_key")?,
        allowed_models,
        enabled: row.boolean("enabled")?,
        created_at: row.text("created_at")?,
    })
}

pub async fn create(db: &Db, channel: &NewChannel) -> sqlx::Result<Channel> {
    let allowed_json = allowed_models_json(channel.allowed_models.as_ref());
    let mut sql = Sql::for_db(db);
    sql.push("INSERT INTO channels (name, base_url, api_key, allowed_models, enabled, created_at) VALUES (")
        .bind(Arg::Text(Some(channel.name.clone())))
        .push(", ")
        .bind(Arg::Text(Some(channel.base_url.clone())))
        .push(", ")
        .bind(Arg::Text(Some(channel.api_key.clone())))
        .push(", ")
        .bind(Arg::Text(allowed_json))
        .push(", ")
        .bind(Arg::Bool(true))
        .push(", ")
        .bind(Arg::Text(Some(timestamp::now_utc())))
        .push(") RETURNING ")
        .push(SELECT_COLUMNS);
    let row = db.fetch_one(sql).await?;
    map_channel(&row)
}

pub async fn list(db: &Db) -> sqlx::Result<Vec<Channel>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT ")
        .push(SELECT_COLUMNS)
        .push(" FROM channels ORDER BY id");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_channel).collect()
}

pub async fn find(db: &Db, id: i64) -> sqlx::Result<Option<Channel>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT ")
        .push(SELECT_COLUMNS)
        .push(" FROM channels WHERE id = ")
        .bind(Arg::Int(id));
    let row = db.fetch_optional(sql).await?;
    row.as_ref().map(map_channel).transpose()
}

pub async fn update(db: &Db, id: i64, patch: &ChannelPatch) -> sqlx::Result<Option<Channel>> {
    let mut sql = Sql::for_db(db);
    sql.push("UPDATE channels SET ");
    let mut assignments = 0;
    if let Some(name) = &patch.name {
        sql.push("name = ").bind(Arg::Text(Some(name.clone())));
        assignments += 1;
    }
    if let Some(base_url) = &patch.base_url {
        if assignments > 0 {
            sql.push(", ");
        }
        sql.push("base_url = ")
            .bind(Arg::Text(Some(base_url.clone())));
        assignments += 1;
    }
    if let Some(api_key) = &patch.api_key {
        if assignments > 0 {
            sql.push(", ");
        }
        sql.push("api_key = ")
            .bind(Arg::Text(Some(api_key.clone())));
        assignments += 1;
    }
    if let Some(allowed_models) = &patch.allowed_models {
        if assignments > 0 {
            sql.push(", ");
        }
        sql.push("allowed_models = ")
            .bind(Arg::Text(allowed_models_json(allowed_models.as_ref())));
        assignments += 1;
    }
    if let Some(enabled) = patch.enabled {
        if assignments > 0 {
            sql.push(", ");
        }
        sql.push("enabled = ").bind(Arg::Bool(enabled));
        assignments += 1;
    }
    if assignments == 0 {
        return find(db, id).await;
    }
    sql.push(" WHERE id = ")
        .bind(Arg::Int(id))
        .push(" RETURNING ")
        .push(SELECT_COLUMNS);

    let row = db.fetch_optional(sql).await?;
    row.as_ref().map(map_channel).transpose()
}

pub async fn delete(db: &Db, id: i64) -> sqlx::Result<DeleteOutcome> {
    let mut exists = Sql::for_db(db);
    exists
        .push("SELECT CAST(1 AS BIGINT) FROM channels WHERE id = ")
        .bind(Arg::Int(id));
    if db.scalar_optional(exists).await?.is_none() {
        return Ok(DeleteOutcome::NotFound);
    }
    // Any bound key row, alive or soft-deleted, blocks the delete: the FK
    // from api_keys and the request_logs history both depend on it.
    let bound_keys = bound_key_count(db, id).await?;
    if bound_keys > 0 {
        return Ok(DeleteOutcome::HasBoundKeys);
    }
    let mut remove = Sql::for_db(db);
    remove
        .push("DELETE FROM channels WHERE id = ")
        .bind(Arg::Int(id));
    db.execute(remove).await?;
    Ok(DeleteOutcome::Deleted)
}

fn allowed_models_json(models: Option<&Vec<String>>) -> Option<String> {
    models.map(|m| serde_json::to_string(m).expect("Vec<String> serialization is infallible"))
}
