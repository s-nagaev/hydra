//! In-memory cache for the key→channel proxy resolution.
//!
//! The proxy auth path otherwise runs two SELECTs per call
//! (`find_alive_by_hash` plus the channel lookup); a fresh cache hit answers
//! both from memory and touches the database not at all. Only successful
//! resolutions of an alive key bound to an enabled channel are cached: every
//! rejection (unknown key, disabled key, gone or disabled channel) stays
//! uncached and keeps consulting the database.
//!
//! Security flips are instant because admin mutations invalidate explicitly,
//! before and after the write: key PATCH/DELETE drops the entries of that key,
//! channel PATCH/DELETE drops every entry bound to the channel — a rotated
//! upstream `base_url` or `api_key` must never keep serving from a stale
//! entry. The TTL is the safety net for out-of-band edits (direct database
//! writes that bypass the admin API), not the primary invalidation mechanism:
//! such drift is visible for at most `HYDRA_CACHE_TTL_SECS` seconds. `0` turns
//! the cache off entirely; the default is 3600.
//!
//! The cache sits above [`crate::db::Db`], so it behaves identically over the
//! SQLite and PostgreSQL backends.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::db::channels::Channel;
use crate::db::keys::ApiKey;

/// Default lifetime of a cache entry, in seconds.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// One cached resolution: the alive key row and its enabled channel.
#[derive(Clone)]
pub struct CachedResolution {
    pub key: ApiKey,
    pub channel: Channel,
}

/// Keyed by the SHA-256 hash of the presented bearer token (the same value
/// the auth lookup matches on); plaintext keys never enter the cache.
struct Entry {
    resolution: CachedResolution,
    inserted_at: Instant,
}

/// Process-wide resolution cache shared through [`crate::http::AppState`].
/// Cloning shares the same entry map: every clone sees the same cache, so
/// invalidations apply across handler and test clones alike.
#[derive(Clone)]
pub struct ResolutionCache {
    /// `None` means the cache is off (`HYDRA_CACHE_TTL_SECS=0`).
    ttl: Option<Duration>,
    entries: Arc<RwLock<HashMap<String, Entry>>>,
}

impl Default for ResolutionCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL_SECS)
    }
}

impl ResolutionCache {
    /// Builds a cache with the given entry lifetime; a TTL of 0 disables it.
    pub fn new(ttl_secs: u64) -> Self {
        ResolutionCache {
            ttl: (ttl_secs > 0).then(|| Duration::from_secs(ttl_secs)),
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns the cached resolution when a fresh entry exists; an expired
    /// entry is dropped on sight.
    pub fn get(&self, key_hash: &str) -> Option<CachedResolution> {
        let mut entries = self.entries.write().expect("cache lock poisoned");
        let ttl = self.ttl?;
        let entry = entries.get(key_hash)?;
        if entry.inserted_at.elapsed() >= ttl {
            entries.remove(key_hash);
            return None;
        }
        Some(entry.resolution.clone())
    }

    /// Caches a successful resolution. A no-op when the cache is off.
    pub fn insert(&self, key_hash: String, key: ApiKey, channel: Channel) {
        if self.ttl.is_none() {
            return;
        }
        self.entries.write().expect("cache lock poisoned").insert(
            key_hash,
            Entry {
                resolution: CachedResolution { key, channel },
                inserted_at: Instant::now(),
            },
        );
    }

    /// Drops every entry of the given key row (admin key PATCH/DELETE).
    pub fn invalidate_key(&self, key_id: i64) {
        self.entries
            .write()
            .expect("cache lock poisoned")
            .retain(|_, entry| entry.resolution.key.id != key_id);
    }

    /// Drops every entry bound to the given channel (admin channel
    /// PATCH/DELETE), so rotated upstream credentials take effect at once.
    pub fn invalidate_channel(&self, channel_id: i64) {
        self.entries
            .write()
            .expect("cache lock poisoned")
            .retain(|_, entry| entry.resolution.key.channel_id != channel_id);
    }

    /// Number of live entries; used by tests to observe invalidation.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.read().expect("cache lock poisoned").len()
    }

    /// Whether the cache holds no entries; used by tests.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.read().expect("cache lock poisoned").is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_key(id: i64, channel_id: i64) -> ApiKey {
        ApiKey {
            id,
            key_hash: format!("hash-{id}"),
            display_prefix: "hyd-abcd".to_string(),
            name: None,
            channel_id,
            enabled: true,
            deleted_at: None,
            created_at: "2026-11-08 00:00:00".to_string(),
        }
    }

    fn sample_channel(id: i64) -> Channel {
        Channel {
            id,
            name: "upstream".to_string(),
            base_url: "https://api.example.com".to_string(),
            api_key: "sk-upstream-secret-1234567890".to_string(),
            allowed_models: None,
            enabled: true,
            created_at: "2026-11-08 00:00:00".to_string(),
        }
    }

    #[test]
    fn fresh_entry_roundtrips_and_isolation_between_hashes_holds() {
        let cache = ResolutionCache::new(60);
        assert!(cache.is_empty());

        cache.insert("hash-1".to_string(), sample_key(1, 10), sample_channel(10));
        cache.insert("hash-2".to_string(), sample_key(2, 10), sample_channel(10));

        let hit = cache.get("hash-1").expect("fresh entry must hit");
        assert_eq!(hit.key.id, 1);
        assert_eq!(hit.channel.id, 10);
        assert_eq!(cache.get("hash-2").expect("second entry").key.id, 2);
        assert!(cache.get("hash-unknown").is_none());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn ttl_zero_disables_the_cache_entirely() {
        let cache = ResolutionCache::new(0);
        cache.insert("hash-1".to_string(), sample_key(1, 10), sample_channel(10));
        assert!(cache.get("hash-1").is_none(), "TTL 0 must never serve hits");
        assert!(cache.is_empty(), "insert must be a no-op when disabled");
    }

    #[test]
    fn invalidate_key_drops_only_that_keys_entries() {
        let cache = ResolutionCache::new(60);
        cache.insert("hash-1".to_string(), sample_key(1, 10), sample_channel(10));
        cache.insert("hash-2".to_string(), sample_key(2, 10), sample_channel(10));

        cache.invalidate_key(1);

        assert!(cache.get("hash-1").is_none());
        assert!(cache.get("hash-2").is_some(), "other keys must survive");
    }

    #[test]
    fn invalidate_channel_drops_every_entry_bound_to_it() {
        let cache = ResolutionCache::new(60);
        cache.insert("hash-1".to_string(), sample_key(1, 10), sample_channel(10));
        cache.insert("hash-2".to_string(), sample_key(2, 10), sample_channel(10));
        cache.insert("hash-3".to_string(), sample_key(3, 20), sample_channel(20));

        cache.invalidate_channel(10);

        assert!(cache.get("hash-1").is_none());
        assert!(cache.get("hash-2").is_none());
        assert!(cache.get("hash-3").is_some(), "other channels must survive");
    }

    #[test]
    fn expired_entry_is_evicted_and_never_served() {
        let cache = ResolutionCache::new(1);
        cache.insert("hash-1".to_string(), sample_key(1, 10), sample_channel(10));
        assert!(cache.get("hash-1").is_some());

        std::thread::sleep(Duration::from_millis(1100));

        assert!(cache.get("hash-1").is_none(), "expired entry must miss");
        assert!(cache.is_empty(), "expired entry must be evicted on sight");
    }
}
