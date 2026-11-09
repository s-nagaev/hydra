//! Month-to-date usage evaluation behind the monthly key limits.
//!
//! The proxy auth path never aggregates: it reads one in-memory verdict per
//! key ([`crate::limit_flags`]), so enforcing a limit costs a hash lookup
//! instead of a database scan. The verdict is recomputed this way after
//! every logged response: the month-to-date totals of the key's models are
//! read from `request_logs`
//! ([`crate::db::request_logs::month_to_date_usage`]), priced with the bound
//! channel's table ([`crate::cost`]) and compared against the stored limit.
//!
//! The verdict folds in the channel's limitability, which is the same
//! property the admin UI badges on a key ([`crate::db::channels::PricedChannel::limitable`]):
//! a channel is limitable when it is enabled, its whitelist resolves to a
//! finite model set and every model in it carries all four prices. A key
//! whose channel is not limitable, or which has no limit at all, stores the
//! verdict `false`: the limit is kept but never enforced, which is the
//! documented behavior for a channel the router cannot price.
//!
//! Two accepted properties of an in-memory verdict, both documented in the
//! README. First, flags start empty, so the first request of a limited key
//! after a restart passes; the following logged responses repopulate the
//! verdict. Second, the recompute runs after the response it belongs to has
//! been logged, so a key that crosses its limit can overshoot by the
//! requests already in flight.
//!
//! The recompute is strictly a reader: it writes no `request_logs` row and
//! therefore cannot re-enter the logging path that spawned it. A failed
//! aggregate or lookup leaves the stored verdict untouched, so a database
//! hiccup can never unblock a key that was over budget.

use crate::cost::{self, TokenTotals};
use crate::db::request_logs::{self, ModelUsage};
use crate::db::{channels, keys, Db};
use crate::limit_flags::LimitFlags;
use crate::month::Month;
use crate::prices::ModelPrices;

/// Recomputes one key's monthly-limit verdict and stores it in `flags`.
///
/// The verdict is the key's month-to-date spend against the stored limit,
/// and it is only ever `true` for a limitable channel: see the module docs.
/// The key row is read including soft-deleted rows, because a deleted or
/// vanished key must drop its verdict rather than keep one alive.
pub async fn recompute(db: &Db, flags: &LimitFlags, key_id: i64) -> sqlx::Result<()> {
    let month = Month::current();
    let Some(key) = keys::find(db, key_id).await? else {
        flags.invalidate(key_id);
        return Ok(());
    };
    if key.deleted_at.is_some() {
        flags.invalidate(key_id);
        return Ok(());
    }
    let exceeded = match key.monthly_limit_micro_usd {
        None => false,
        Some(limit) => match channels::find_priced(db, key.channel_id).await? {
            Some(priced) if priced.limitable() => {
                let usage = request_logs::month_to_date_usage(db, key_id, &month).await?;
                month_cost(&usage, priced.model_prices.as_ref()) >= limit
            }
            _ => false,
        },
    };
    flags.set(key_id, month.bucket(), exceeded);
    Ok(())
}

/// Sums the cost of one key's month-to-date usage groups at the channel's
/// price table. A model the channel does not price costs nothing, exactly as
/// [`crate::cost::model_cost`] defines it; the sum saturates instead of
/// wrapping, matching the arithmetic in `cost`.
pub fn month_cost(usage: &[ModelUsage], prices: Option<&ModelPrices>) -> i64 {
    usage
        .iter()
        .map(|group| group_cost(group, prices))
        .fold(0i64, i64::saturating_add)
}

/// Cost of one aggregate group in micro-USD. This is the single place where
/// an aggregate group becomes a [`TokenTotals`], so the enforcement verdict
/// and the admin stats page price a group identically.
pub fn group_cost(group: &ModelUsage, prices: Option<&ModelPrices>) -> i64 {
    let totals = TokenTotals {
        prompt: group.prompt_tokens,
        completion: group.completion_tokens,
        cached_input: group.cached_input_tokens,
        cached_output: group.cached_output_tokens,
    };
    cost::model_cost(group.model.as_deref(), &totals, prices)
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::db::channels::{self, NewChannel};
    use crate::db::keys::{self, NewKey};
    use crate::db::request_logs::NewRequestLog;
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

    /// A channel priced for `gpt-4o` (2.50/10.00/1.25/5.00 USD per 1M
    /// tokens, cached_input included) with the model whitelisted, so it is
    /// limitable, plus a key bound to it with `limit`.
    async fn seed(db: &Db, tag: &str, limit: Option<i64>) -> i64 {
        let prices = ModelPrices::from_usd_json(&serde_json::json!({
            "gpt-4o": {
                "input": "2.50",
                "output": "10.00",
                "cached_input": "1.25",
                "cached_output": "5.00"
            }
        }))
        .unwrap();
        let channel = channels::create_with_prices(
            db,
            &NewChannel {
                name: format!("channel-{tag}"),
                base_url: "http://127.0.0.1:9".to_string(),
                api_key: "sk-upstream-secret".to_string(),
                allowed_models: Some(vec!["gpt-4o".to_string()]),
            },
            Some(&prices),
        )
        .await
        .unwrap();
        assert!(channel.limitable(), "the seeded channel must be limitable");
        keys::create(
            db,
            &NewKey {
                key_hash: format!("hash-{tag}"),
                display_prefix: "hyd-abcdefgh".to_string(),
                name: None,
                channel_id: channel.channel.id,
                monthly_limit_micro_usd: limit,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// One month-to-date request: 1000 prompt tokens with 400 of them
    /// cached, 200 completion tokens and no cached output, at
    /// 2.50/1.25/10.00 USD per 1M tokens, which costs 4000 micro-USD
    /// through the pinned formula.
    fn logged_request(key_id: i64, channel_id: i64) -> NewRequestLog {
        NewRequestLog {
            api_key_id: Some(key_id),
            channel_id: Some(channel_id),
            model: Some("gpt-4o".to_string()),
            status: Some(200),
            prompt_tokens: Some(1000),
            completion_tokens: Some(200),
            cached_input_tokens: Some(400),
            cached_output_tokens: None,
            duration_ms: 1,
            request_body: None,
            response_body: None,
            error: None,
            client_ip: None,
        }
    }

    /// Logs one month-to-date request for `key_id`.
    async fn log_request(db: &Db, key_id: i64, channel_id: i64) {
        request_logs::insert(db, &logged_request(key_id, channel_id))
            .await
            .unwrap();
    }

    fn bucket_now() -> crate::limit_flags::MonthBucket {
        Month::current().bucket()
    }

    #[tokio::test]
    async fn spend_below_the_limit_stores_a_false_verdict() {
        let db = test_db().await;
        let key_id = seed(&db, "below", Some(50_000_000)).await;
        let flags = LimitFlags::default();
        log_request(&db, key_id, 1).await;

        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(flags.get(key_id, bucket_now()), Some(false));
    }

    #[tokio::test]
    async fn spending_the_limit_trips_the_verdict_and_clearing_the_limit_relaxes_it() {
        let db = test_db().await;
        // One request costs 4000 micro-USD, so a 3999 micro-USD budget is
        // crossed by the first logged request.
        let key_id = seed(&db, "cross", Some(3_999)).await;
        let flags = LimitFlags::default();

        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(
            flags.get(key_id, bucket_now()),
            Some(false),
            "no usage yet means no overshoot"
        );

        log_request(&db, key_id, 1).await;
        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(flags.get(key_id, bucket_now()), Some(true));

        // Raising the limit above the spend relaxes the verdict.
        keys::patch(
            &db,
            key_id,
            &keys::KeyPatch {
                name: None,
                enabled: None,
                monthly_limit_micro_usd: Some(Some(50_000_000)),
            },
        )
        .await
        .unwrap();
        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(flags.get(key_id, bucket_now()), Some(false));
    }

    #[tokio::test]
    async fn spending_exactly_the_limit_trips_the_verdict() {
        let db = test_db().await;
        // One logged request costs exactly 4000 micro-USD, so a limit of
        // 4000 is the boundary case: the comparison is `>=`, equality is
        // already over budget and does not wait for a larger spend.
        let key_id = seed(&db, "exact", Some(4_000)).await;
        let flags = LimitFlags::default();
        log_request(&db, key_id, 1).await;

        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(
            flags.get(key_id, bucket_now()),
            Some(true),
            "a spend equal to the limit is already over budget"
        );

        // One micro-USD of extra headroom and the same spend stays under the
        // limit, pinning the other side of the edge.
        keys::patch(
            &db,
            key_id,
            &keys::KeyPatch {
                name: None,
                enabled: None,
                monthly_limit_micro_usd: Some(Some(4_001)),
            },
        )
        .await
        .unwrap();
        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(
            flags.get(key_id, bucket_now()),
            Some(false),
            "one micro-USD above the spend is not crossed"
        );
    }

    #[tokio::test]
    async fn a_key_without_a_limit_never_trips() {
        let db = test_db().await;
        let key_id = seed(&db, "unlimited", None).await;
        let flags = LimitFlags::default();
        log_request(&db, key_id, 1).await;

        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(flags.get(key_id, bucket_now()), Some(false));
    }

    #[tokio::test]
    async fn an_unlimitable_channel_is_stored_but_not_enforced() {
        let db = test_db().await;
        // No whitelist and no prices: the channel cannot be priced, so the
        // limit stays stored and the verdict stays false.
        let channel = channels::create(
            &db,
            &NewChannel {
                name: "unpriced".to_string(),
                base_url: "http://127.0.0.1:9".to_string(),
                api_key: "sk-upstream-secret".to_string(),
                allowed_models: None,
            },
        )
        .await
        .unwrap();
        let key_id = keys::create(
            &db,
            &NewKey {
                key_hash: "hash-unpriced".to_string(),
                display_prefix: "hyd-abcdefgh".to_string(),
                name: None,
                channel_id: channel.id,
                monthly_limit_micro_usd: Some(1),
            },
        )
        .await
        .unwrap()
        .id;
        let flags = LimitFlags::default();
        log_request(&db, key_id, channel.id).await;

        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(
            flags.get(key_id, bucket_now()),
            Some(false),
            "an unlimitable channel must not enforce the stored limit"
        );
    }

    #[tokio::test]
    async fn usage_from_another_month_does_not_trip_the_verdict() {
        let db = test_db().await;
        let key_id = seed(&db, "rollover", Some(1)).await;
        let flags = LimitFlags::default();
        // A row from January, far over budget for that month.
        request_logs::insert_at(&db, &logged_request(key_id, 1), "2026-01-15 10:00:00")
            .await
            .unwrap();

        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(
            flags.get(key_id, bucket_now()),
            Some(false),
            "the new month starts with an empty budget"
        );
    }

    #[tokio::test]
    async fn a_deleted_key_drops_its_verdict() {
        let db = test_db().await;
        let key_id = seed(&db, "deleted", Some(4_000)).await;
        let flags = LimitFlags::default();
        flags.set(key_id, bucket_now(), true);

        keys::delete(&db, key_id).await.unwrap();
        recompute(&db, &flags, key_id).await.unwrap();
        assert_eq!(flags.get(key_id, bucket_now()), None);
    }

    #[tokio::test]
    async fn month_cost_prices_every_group_and_ignores_unpriced_models() {
        let prices = ModelPrices::from_usd_json(&serde_json::json!({
            "priced": {
                "input": "1.00",
                "output": "2.00",
                "cached_input": "0.50",
                "cached_output": "1.00"
            }
        }))
        .unwrap();
        let usage = vec![
            ModelUsage {
                model: Some("priced".to_string()),
                prompt_tokens: 1000,
                completion_tokens: 1000,
                cached_input_tokens: 0,
                cached_output_tokens: 0,
                requests: 1,
            },
            ModelUsage {
                model: Some("unpriced".to_string()),
                prompt_tokens: 999_999,
                completion_tokens: 999_999,
                cached_input_tokens: 0,
                cached_output_tokens: 0,
                requests: 1,
            },
        ];
        // 1000 * 1.00 + 1000 * 2.00 USD per 1M tokens, in micro-USD.
        assert_eq!(month_cost(&usage, Some(&prices)), 3000);
        assert_eq!(month_cost(&usage, None), 0);
        assert_eq!(month_cost(&[], Some(&prices)), 0);
        // A single group prices on its own exactly as it prices inside the
        // sum, which is what the admin stats page relies on.
        assert_eq!(group_cost(&usage[0], Some(&prices)), 3000);
        assert_eq!(group_cost(&usage[1], Some(&prices)), 0);
        assert_eq!(group_cost(&usage[1], None), 0);
    }
}
