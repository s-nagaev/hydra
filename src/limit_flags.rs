//! In-memory "this key is over its monthly limit" flags.
//!
//! The store is a fast-path cache over the authoritative figure, which stays
//! the month-to-date aggregate over `request_logs`: the request path must
//! not pay a database round trip to answer "is this key out of budget", so
//! the verdict is computed in the background ([`crate::usage`]) and
//! read from memory here.
//!
//! The shape follows [`crate::flood`]: a process-wide
//! `Arc<Mutex<HashMap<..>>>` shared through clones of
//! [`AppState`](crate::http::AppState), with admin handlers invalidating
//! entries explicitly. Every entry is labelled with the month it was
//! evaluated in, so a read in a different month reports "not evaluated"
//! rather than carrying last month's verdict into the new budget period:
//! month rollover needs no timer and no sweep, only the comparison in
//! [`LimitFlags::get`].
//!
//! Two deliberate fail-open choices, matching the accepted staleness of the
//! whole feature:
//!
//! - A key with no entry (never evaluated, invalidated by an admin change,
//!   evicted, or evaluated in an earlier month) passes the gate. The cost of
//!   a stale verdict is one request, never a request lost to a cache miss.
//! - Entries are bounded by [`MAX_ENTRIES`]; once the map is full a new key
//!   evicts an arbitrary existing entry (the same trade
//!   [`crate::flood`] makes). The victim loses its cached verdict and falls
//!   back to re-evaluation, so the map size never exceeds the bound however
//!   many keys exist.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::month;

/// The `YYYY-MM` label an entry was evaluated in, as
/// [`crate::month::Month::bucket`] renders it.
pub type MonthBucket = [u8; month::LABEL_LEN];

/// Hard bound on tracked keys. At this many entries a new key evicts an
/// arbitrary existing one, so the map can never grow past this size.
pub const MAX_ENTRIES: usize = 100_000;

/// One key's verdict for one month.
struct Entry {
    month: MonthBucket,
    exceeded: bool,
}

/// Process-wide monthly-limit flags shared through
/// [`crate::http::AppState`]. Cloning shares the same entry map.
#[derive(Clone, Default)]
pub struct LimitFlags {
    entries: Arc<Mutex<HashMap<i64, Entry>>>,
}

impl LimitFlags {
    /// The stored verdict for `key_id`, or `None` when the key was not
    /// evaluated in `month` (never, invalidated, evicted, or evaluated in
    /// another month). `None` means "not known to be over budget", which the
    /// gate lets through.
    pub fn get(&self, key_id: i64, month: MonthBucket) -> Option<bool> {
        let entries = self.entries.lock().expect("limit flag map lock poisoned");
        entries
            .get(&key_id)
            .filter(|entry| entry.month == month)
            .map(|entry| entry.exceeded)
    }

    /// Stores the verdict of a fresh evaluation, replacing any earlier one
    /// for this key regardless of the month it was made in.
    pub fn set(&self, key_id: i64, month: MonthBucket, exceeded: bool) {
        let mut entries = self.entries.lock().expect("limit flag map lock poisoned");
        if !entries.contains_key(&key_id) && entries.len() >= MAX_ENTRIES {
            // Iteration order is unspecified, which is acceptable for a hard
            // memory bound: the victim is re-evaluated after its next logged
            // request.
            if let Some(victim) = entries.keys().next().copied() {
                entries.remove(&victim);
            }
        }
        entries.insert(key_id, Entry { month, exceeded });
    }

    /// Drops one key's verdict, so the next evaluation decides again. Called
    /// on every admin change to a key, its limit or its existence.
    pub fn invalidate(&self, key_id: i64) {
        self.entries
            .lock()
            .expect("limit flag map lock poisoned")
            .remove(&key_id);
    }

    /// Drops every verdict. Test-only: nothing on the request path or in the
    /// admin handlers needs a wholesale reset, so the method stays out of the
    /// crate's public surface.
    #[cfg(test)]
    pub fn invalidate_all(&self) {
        self.entries
            .lock()
            .expect("limit flag map lock poisoned")
            .clear();
    }

    /// Number of tracked keys; used by tests to observe eviction.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("limit flag map lock poisoned")
            .len()
    }

    /// Whether no key is tracked; used by tests.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries
            .lock()
            .expect("limit flag map lock poisoned")
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::month::Month;

    fn bucket(year: i32, number: u8) -> MonthBucket {
        Month::from_parts(year, number).unwrap().bucket()
    }

    #[test]
    fn an_unknown_key_has_no_verdict() {
        let flags = LimitFlags::default();
        assert_eq!(flags.get(7, bucket(2026, 11)), None);
        assert_eq!(flags.len(), 0);
    }

    #[test]
    fn a_stored_verdict_reads_back_for_its_own_month() {
        let flags = LimitFlags::default();
        flags.set(7, bucket(2026, 11), true);
        assert_eq!(flags.get(7, bucket(2026, 11)), Some(true));
        flags.set(7, bucket(2026, 11), false);
        assert_eq!(flags.get(7, bucket(2026, 11)), Some(false));
        // Another key is unaffected.
        assert_eq!(flags.get(8, bucket(2026, 11)), None);
    }

    #[test]
    fn a_verdict_does_not_survive_the_month_rollover() {
        let flags = LimitFlags::default();
        flags.set(7, bucket(2026, 11), true);
        assert_eq!(
            flags.get(7, bucket(2026, 12)),
            None,
            "the new month starts with an empty budget"
        );
        // The stale entry is replaced, not merged, by the new month's verdict.
        flags.set(7, bucket(2026, 12), false);
        assert_eq!(flags.get(7, bucket(2026, 12)), Some(false));
        assert_eq!(flags.get(7, bucket(2026, 11)), None);
        assert_eq!(flags.len(), 1, "one entry per key, not per month");
    }

    #[test]
    fn invalidation_drops_one_key_or_all_of_them() {
        let flags = LimitFlags::default();
        flags.set(7, bucket(2026, 11), true);
        flags.set(8, bucket(2026, 11), true);
        flags.invalidate(7);
        assert_eq!(flags.get(7, bucket(2026, 11)), None);
        assert_eq!(flags.get(8, bucket(2026, 11)), Some(true));
        flags.invalidate_all();
        assert_eq!(flags.get(8, bucket(2026, 11)), None);
        assert_eq!(flags.len(), 0);
    }

    #[test]
    fn the_entry_count_never_grows_past_the_bound() {
        let flags = LimitFlags::default();
        let month = bucket(2026, 11);
        for key_id in 0..=(MAX_ENTRIES as i64) {
            flags.set(key_id, month, true);
        }
        assert!(
            flags.len() <= MAX_ENTRIES,
            "map must stay bounded, got {}",
            flags.len()
        );
        assert_eq!(flags.len(), MAX_ENTRIES, "the map keeps the entries it can");
    }
}
