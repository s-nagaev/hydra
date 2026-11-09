//! Runtime sqlx queries for the `channels` table.
//!
//! One SQL body per query: fragments are written against the backend-aware
//! [`Sql`] builder, which picks the placeholder per backend.
//!
//! The price table of a channel lives in the same row, in the
//! `model_prices` column. Reads that need it go through [`PricedChannel`],
//! which carries the parsed table next to the row; the plain [`Channel`]
//! keeps serving the proxy and cache paths, which read the whitelist and
//! the upstream credentials only. Writes that set prices go through the
//! `*_with_prices` variants so a channel and its prices are written by one
//! statement.

use crate::prices::ModelPrices;

use super::timestamp;
use super::{Arg, Db, Fields, Row, Sql};

/// A stored channel row. `api_key` is the upstream secret and must never
/// cross the HTTP boundary unmasked.
#[derive(Clone)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub allowed_models: Option<Vec<String>>,
    pub enabled: bool,
    pub created_at: String,
}

/// A channel row together with its parsed price table. `model_prices` is
/// `None` when the channel has no prices stored.
#[derive(Clone)]
pub struct PricedChannel {
    pub channel: Channel,
    pub model_prices: Option<ModelPrices>,
}

impl PricedChannel {
    /// Eligible for monthly key limits: the channel is enabled, its
    /// whitelist resolves to a finite model set, and every model in it
    /// carries all four prices.
    pub fn limitable(&self) -> bool {
        self.channel.enabled
            && crate::prices::all_models_priced(
                self.channel.allowed_models.as_deref(),
                self.model_prices.as_ref(),
            )
    }
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

const SELECT_COLUMNS: &str =
    "id, name, base_url, api_key, allowed_models, enabled, created_at, model_prices";

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

/// Reads the `model_prices` column of an already-fetched row; a NULL column
/// means no prices, corrupt content is a decode failure like any other.
fn map_prices(row: &Row) -> sqlx::Result<Option<ModelPrices>> {
    let raw = row.opt_text("model_prices")?;
    raw.map(|text| {
        ModelPrices::from_stored_json(&text).map_err(|e| sqlx::Error::ColumnDecode {
            index: "model_prices".into(),
            source: Box::new(e),
        })
    })
    .transpose()
}

fn map_priced(row: &Row) -> sqlx::Result<PricedChannel> {
    Ok(PricedChannel {
        channel: map_channel(row)?,
        model_prices: map_prices(row)?,
    })
}

async fn insert(
    db: &Db,
    channel: &NewChannel,
    prices: Option<&ModelPrices>,
) -> sqlx::Result<PricedChannel> {
    let allowed_json = allowed_models_json(channel.allowed_models.as_ref());
    let mut sql = Sql::for_db(db);
    sql.push(
        "INSERT INTO channels (name, base_url, api_key, allowed_models, enabled, created_at, \
         model_prices) VALUES (",
    )
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
    .push(", ")
    .bind(Arg::Text(prices_json(prices)))
    .push(") RETURNING ")
    .push(SELECT_COLUMNS);
    let row = db.fetch_one(sql).await?;
    map_priced(&row)
}

pub async fn create(db: &Db, channel: &NewChannel) -> sqlx::Result<Channel> {
    Ok(create_with_prices(db, channel, None).await?.channel)
}

/// Creates a channel with its price table in one statement.
pub async fn create_with_prices(
    db: &Db,
    channel: &NewChannel,
    prices: Option<&ModelPrices>,
) -> sqlx::Result<PricedChannel> {
    insert(db, channel, prices).await
}

pub async fn list(db: &Db) -> sqlx::Result<Vec<Channel>> {
    Ok(list_priced(db)
        .await?
        .into_iter()
        .map(|priced| priced.channel)
        .collect())
}

/// Lists every channel with its price table, oldest first.
pub async fn list_priced(db: &Db) -> sqlx::Result<Vec<PricedChannel>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT ")
        .push(SELECT_COLUMNS)
        .push(" FROM channels ORDER BY id");
    let rows = db.fetch_all(sql).await?;
    rows.iter().map(map_priced).collect()
}

pub async fn find(db: &Db, id: i64) -> sqlx::Result<Option<Channel>> {
    Ok(find_priced(db, id).await?.map(|priced| priced.channel))
}

/// Fetches one channel with its price table.
pub async fn find_priced(db: &Db, id: i64) -> sqlx::Result<Option<PricedChannel>> {
    let mut sql = Sql::for_db(db);
    sql.push("SELECT ")
        .push(SELECT_COLUMNS)
        .push(" FROM channels WHERE id = ")
        .bind(Arg::Int(id));
    let row = db.fetch_optional(sql).await?;
    row.as_ref().map(map_priced).transpose()
}

pub async fn update(db: &Db, id: i64, patch: &ChannelPatch) -> sqlx::Result<Option<Channel>> {
    Ok(update_with_prices(db, id, patch, None)
        .await?
        .map(|priced| priced.channel))
}

/// Applies a patch and, when `prices` is given, the channel's price table in
/// the same statement. `None` leaves the column untouched, `Some(None)`
/// clears it and `Some(Some(table))` replaces it, mirroring the
/// `allowed_models` double option.
pub async fn update_with_prices(
    db: &Db,
    id: i64,
    patch: &ChannelPatch,
    prices: Option<Option<&ModelPrices>>,
) -> sqlx::Result<Option<PricedChannel>> {
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
    if let Some(prices) = prices {
        if assignments > 0 {
            sql.push(", ");
        }
        sql.push("model_prices = ")
            .bind(Arg::Text(prices_json(prices)));
        assignments += 1;
    }
    if assignments == 0 {
        return find_priced(db, id).await;
    }
    sql.push(" WHERE id = ")
        .bind(Arg::Int(id))
        .push(" RETURNING ")
        .push(SELECT_COLUMNS);

    let row = db.fetch_optional(sql).await?;
    row.as_ref().map(map_priced).transpose()
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

/// The stored shape of a price table: integer micro-USD JSON, or NULL when
/// the channel has no prices.
fn prices_json(prices: Option<&ModelPrices>) -> Option<String> {
    prices.map(ModelPrices::to_stored_json)
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::prices::ModelPrices;

    async fn test_db() -> Db {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        Db::from(pool)
    }

    fn new_channel(name: &str, allowed: Option<Vec<String>>) -> NewChannel {
        NewChannel {
            name: name.to_string(),
            base_url: "https://api.example.com".to_string(),
            api_key: "sk-upstream-secret".to_string(),
            allowed_models: allowed,
        }
    }

    fn prices(json: serde_json::Value) -> ModelPrices {
        ModelPrices::from_usd_json(&json).unwrap()
    }

    #[tokio::test]
    async fn price_table_round_trips_through_the_column() {
        let db = test_db().await;
        let table = prices(serde_json::json!({
            "gpt-4o": { "input": "2.5", "output": "10", "cached_input": "1.25", "cached_output": "5" }
        }));
        let created = create_with_prices(
            &db,
            &new_channel("priced", Some(vec!["gpt-4o".to_string()])),
            Some(&table),
        )
        .await
        .unwrap();
        assert_eq!(created.model_prices.as_ref(), Some(&table));
        assert!(created.limitable());

        let fetched = find_priced(&db, created.channel.id).await.unwrap().unwrap();
        assert_eq!(fetched.model_prices.as_ref(), Some(&table));

        // The plain readers still see the row, without the price table.
        let plain = find(&db, created.channel.id).await.unwrap().unwrap();
        assert_eq!(plain.name, "priced");
    }

    #[tokio::test]
    async fn channels_without_prices_read_back_as_none() {
        let db = test_db().await;
        let created = create(&db, &new_channel("plain", None)).await.unwrap();
        let fetched = find_priced(&db, created.id).await.unwrap().unwrap();
        assert!(fetched.model_prices.is_none());
        assert!(!fetched.limitable(), "no prices means not limitable");
    }

    #[tokio::test]
    async fn priced_update_replaces_and_clears_the_column() {
        let db = test_db().await;
        let first = prices(serde_json::json!({
            "a": { "input": "1", "output": "1", "cached_input": "1", "cached_output": "1" }
        }));
        let second = prices(serde_json::json!({
            "b": { "input": "2", "output": "2", "cached_input": "2", "cached_output": "2" }
        }));
        let created = create_with_prices(&db, &new_channel("priced", None), Some(&first))
            .await
            .unwrap();
        let id = created.channel.id;
        let empty_patch = ChannelPatch {
            name: None,
            base_url: None,
            api_key: None,
            allowed_models: None,
            enabled: None,
        };

        let replaced = update_with_prices(&db, id, &empty_patch, Some(Some(&second)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replaced.model_prices.as_ref(), Some(&second));

        // Unchanged keeps the stored table.
        let untouched = update_with_prices(&db, id, &empty_patch, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.model_prices.as_ref(), Some(&second));

        let cleared = update_with_prices(&db, id, &empty_patch, Some(None))
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.model_prices.is_none());
    }

    #[tokio::test]
    async fn update_without_assignments_still_reports_missing_channels() {
        let db = test_db().await;
        let empty_patch = ChannelPatch {
            name: None,
            base_url: None,
            api_key: None,
            allowed_models: None,
            enabled: None,
        };
        assert!(update_with_prices(&db, 999, &empty_patch, None)
            .await
            .unwrap()
            .is_none());
    }
}
