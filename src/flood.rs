//! In-memory flood cap for anonymous 401 spam.
//!
//! Failed key authentications are the one proxy path that can never be
//! cached (only successful resolutions enter the resolution cache), so a
//! client spamming bad keys costs one or two `SELECT`s per request. This
//! module bounds that cost: after `HYDRA_FLOOD_LIMIT` failed authentications
//! from the same client IP inside a `HYDRA_FLOOD_WINDOW_SECS` fixed window,
//! further requests from that IP are answered with the same 401 an unknown
//! key gets, but the key lookup (cache miss, database read) never runs.
//!
//! Accounting rules, chosen and fixed here:
//!
//! - The budget counts recorded `InvalidKey` failures (missing, malformed
//!   or unknown key) plus in-flight reservations. Admission is one atomic
//!   step ([`FloodCap::admit`]) under a single lock acquisition: the check
//!   and the counter update cannot interleave, so a parallel burst can
//!   never all pass the gate against a stale counter. At most `limit`
//!   requests per window per IP are past the gate at any time, and
//!   therefore at most `limit` key lookups can run before the cap closes.
//! - An admitted request holds its slot until its authentication settles.
//!   An invalid key keeps the slot as a recorded failure. Every other
//!   outcome releases it ([`FloodCap::release`]): a successful
//!   authentication, a disabled key (403) and a storage failure leave the
//!   settled counter untouched, exactly as if they had never been admitted.
//!   The transient side effect is that reservations count toward the limit,
//!   so a legitimate burst of parallel requests from one IP can fill the
//!   budget while it is being processed; with the default limit of 60 this
//!   only affects clients holding more than 60 concurrent requests.
//! - A capped request changes no counter state: it neither extends the
//!   window nor moves the count. The window start and the failure count
//!   only move on admissions and releases, and the window expiry is the
//!   only reset, so a sustained flood gets exactly one extra log row per
//!   window rather than unbounded growth.
//! - Writing the capped-phase log row (at most one per window per IP,
//!   claimed by the chat handler via [`FloodCap::claim_log_slot`]) does not
//!   reset or otherwise touch the failure counter. The claim is bound to
//!   the window the capped decision was made in (the ticket carries the
//!   window start), so it is idempotent and can never consume or duplicate
//!   the next window's row: if the window expires between the cap decision
//!   and the claim, the row is skipped and the new window produces its own
//!   capped row when its own cap is reached.
//! - The response given to a capped request is byte-identical to the
//!   unknown-key 401, so the cap never becomes an oracle distinguishing
//!   "bad key" from "you have been flood-capped". Once an IP is capped,
//!   every request from that IP is rejected for the rest of the window,
//!   valid keys included.
//!
//! The state is a `HashMap` keyed by the resolved client IP, stored in
//! [`AppState`](crate::http::AppState) and shared through clones like the
//! resolution cache. Memory is bounded: expired entries are reset on sight
//! by the next admission, and once the map holds [`MAX_ENTRIES`] live
//! entries a full sweep of expired windows runs at most once per window,
//! with any remaining overflow handled by evicting an arbitrary live entry.
//! An adversary rotating across many source IPs can therefore force counter
//! resets for other IPs (an arbitrary victim loses its count), which is the
//! accepted price of a hard memory bound; the entry count itself never
//! grows past [`MAX_ENTRIES`].
//!
//! Known interaction with request-body handling (documented in the README):
//! the flood check runs inside the `ProxyAuth` extractor, but the chat body
//! is buffered afterwards by the `ChatBody` extractor regardless of the
//! auth outcome. A capped request with an over-limit body therefore still
//! fails with 413 before the 401 is decided, and every unauthenticated
//! request is buffered up to the 32 MiB body limit before it is rejected.
//!
//! The gate only sees requests that carry a resolvable client IP
//! (`ConnectInfo` present); in-process `oneshot` requests have no peer
//! address and are never counted or capped.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default number of failed authentications per window per IP.
pub const DEFAULT_LIMIT: u64 = 60;

/// Default flood-cap window, in seconds.
pub const DEFAULT_WINDOW_SECS: u64 = 60;

/// Hard bound on tracked IPs. At this many live entries new IPs evict an
/// arbitrary existing entry, so the map can never grow past this size
/// regardless of how many distinct source addresses appear.
const MAX_ENTRIES: usize = 65_536;

/// Per-IP flood state for one fixed window.
struct Entry {
    window_start: Instant,
    /// Recorded failed authentications plus in-flight reservations; the
    /// number the limit is checked against.
    failures: u64,
    /// Whether the one capped-phase log row of this window was already
    /// claimed (and written or at least reserved) by the handler.
    log_slot_claimed: bool,
}

impl Entry {
    /// A fresh window with an empty budget; the admitting request consumes
    /// the first slot through the shared decision below.
    fn fresh(now: Instant) -> Self {
        Entry {
            window_start: now,
            failures: 0,
            log_slot_claimed: false,
        }
    }

    fn expired(&self, now: Instant, window: Duration) -> bool {
        now.duration_since(self.window_start) >= window
    }
}

/// Proof of admission through the flood gate, binding the client IP to the
/// window the decision was made against. A capped request's log-row claim
/// can only ever act on that exact window, which is what makes the claim
/// idempotent under window-expiry races.
#[derive(Debug, Clone)]
pub struct FloodTicket {
    ip: IpAddr,
    window_start: Instant,
}

/// Outcome of the atomic admission check.
#[derive(Debug)]
pub enum Admission {
    /// The request may proceed to key resolution while holding one budget
    /// slot. The slot settles into a recorded failure when the request
    /// fails with an invalid key (simply keep the ticket), and is given
    /// back through [`FloodCap::release`] for every other outcome.
    Allowed(FloodTicket),
    /// The IP's budget is spent for the current window: the request must be
    /// rejected without any key lookup. The ticket identifies the window
    /// the decision was made against, for the one-row log claim.
    Capped(FloodTicket),
}

/// Process-wide per-IP flood limiter shared through
/// [`crate::http::AppState`]. Cloning shares the same entry map. A limit of
/// `0` (from `HYDRA_FLOOD_LIMIT=0`) disables the cap entirely: every failed
/// authentication is processed and logged as before.
#[derive(Clone)]
pub struct FloodCap {
    limit: u64,
    window: Duration,
    entries: Arc<Mutex<HashMap<IpAddr, Entry>>>,
    /// Rate-limits the O(n) expired-entry sweep: at most one full sweep per
    /// window, so a full map degrades to O(1) inserts between sweeps.
    last_sweep: Arc<Mutex<Instant>>,
}

impl Default for FloodCap {
    fn default() -> Self {
        Self::new(DEFAULT_LIMIT, DEFAULT_WINDOW_SECS)
    }
}

impl FloodCap {
    /// Builds the cap from parsed configuration values. `limit == 0`
    /// disables it; a zero window is rejected earlier by config validation.
    pub fn new(limit: u64, window_secs: u64) -> Self {
        FloodCap {
            limit,
            window: Duration::from_secs(window_secs.max(1)),
            entries: Arc::new(Mutex::new(HashMap::new())),
            last_sweep: Arc::new(Mutex::new(
                Instant::now() - Duration::from_secs(window_secs.max(1)),
            )),
        }
    }

    /// Whether the cap is active (`HYDRA_FLOOD_LIMIT` above zero).
    pub fn is_enabled(&self) -> bool {
        self.limit > 0
    }

    /// Admits one request from `ip` or caps it, atomically: the window
    /// check, the cap decision and the counter update happen in a single
    /// critical section, so a parallel burst sees every earlier admission
    /// and at most `limit` requests per window can be past this point at
    /// the same time. A capped admission changes no counter state. A no-op
    /// returning an inert [`Admission::Allowed`] ticket when disabled (the
    /// map is never touched; release and claim against it do nothing).
    pub fn admit(&self, ip: IpAddr) -> Admission {
        let now = Instant::now();
        if !self.is_enabled() {
            return Admission::Allowed(FloodTicket {
                ip,
                window_start: now,
            });
        }
        let mut entries = self.entries.lock().expect("flood map lock poisoned");
        // Expired entries are reset in place; a first-time IP gets an entry
        // (bounding memory first when the map is full). Both paths produce
        // an empty budget, and the decision below then consumes this
        // request's own slot.
        if let Some(entry) = entries.get_mut(&ip) {
            if entry.expired(now, self.window) {
                *entry = Entry::fresh(now);
            }
        } else {
            if entries.len() >= MAX_ENTRIES {
                self.sweep_and_make_room(&mut entries, now);
            }
            entries.insert(ip, Entry::fresh(now));
        }
        let entry = entries.get_mut(&ip).expect("entry exists after admit");
        let ticket = FloodTicket {
            ip,
            window_start: entry.window_start,
        };
        if entry.failures >= self.limit {
            return Admission::Capped(ticket);
        }
        entry.failures += 1;
        Admission::Allowed(ticket)
    }

    /// Settles an admitted request without turning it into a recorded
    /// failure: successful authentications, disabled keys (403) and storage
    /// errors give their budget slot back. A no-op for tickets whose window
    /// has expired or been replaced in the meantime.
    pub fn release(&self, ticket: &FloodTicket) {
        if !self.is_enabled() {
            return;
        }
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("flood map lock poisoned");
        if let Some(entry) = entries.get_mut(&ticket.ip) {
            if entry.window_start == ticket.window_start
                && !entry.expired(now, self.window)
                && entry.failures > 0
            {
                entry.failures -= 1;
            }
        }
    }

    /// Claims the single capped-phase log row for the window the capped
    /// decision was made in: `true` exactly once per (IP, window). Claims
    /// bound to a window that has since expired or been replaced return
    /// `false`, so a straggler capped request can never consume or
    /// duplicate the next window's row. A no-op returning `false` when
    /// disabled.
    pub fn claim_log_slot(&self, ticket: &FloodTicket) -> bool {
        if !self.is_enabled() {
            return false;
        }
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("flood map lock poisoned");
        match entries.get_mut(&ticket.ip) {
            Some(entry)
                if entry.window_start == ticket.window_start
                    && !entry.expired(now, self.window) =>
            {
                if entry.log_slot_claimed {
                    false
                } else {
                    entry.log_slot_claimed = true;
                    true
                }
            }
            _ => false,
        }
    }

    /// Makes room for a new entry in a full map: at most one full expired
    /// sweep per window, otherwise (or when the sweep freed nothing) evict
    /// an arbitrary live entry. The map never exceeds [`MAX_ENTRIES`] for
    /// longer than this call.
    fn sweep_and_make_room(&self, entries: &mut HashMap<IpAddr, Entry>, now: Instant) {
        let mut last_sweep = self.last_sweep.lock().expect("sweep lock poisoned");
        if now.duration_since(*last_sweep) >= self.window {
            entries.retain(|_, entry| !entry.expired(now, self.window));
            *last_sweep = now;
        }
        if entries.len() >= MAX_ENTRIES {
            // Drop an arbitrary live entry: iteration order is unspecified,
            // which is acceptable for a DoS bound (the victim's flood count
            // simply restarts).
            if let Some(victim) = entries.keys().next().copied() {
                entries.remove(&victim);
            }
        }
    }

    /// Number of tracked IPs; used by tests to observe eviction.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.lock().expect("flood map lock poisoned").len()
    }

    /// Whether no IP is currently tracked; used by tests.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries
            .lock()
            .expect("flood map lock poisoned")
            .is_empty()
    }

    /// The settled budget of `ip` (recorded failures plus in-flight
    /// reservations); used by tests.
    #[cfg(test)]
    pub fn failures(&self, ip: IpAddr) -> Option<u64> {
        self.entries
            .lock()
            .expect("flood map lock poisoned")
            .get(&ip)
            .map(|entry| entry.failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    /// Records an invalid-key failure for `ip` if admitted (the reservation
    /// is kept); returns whether the request was capped instead.
    fn fail(cap: &FloodCap, ip: IpAddr) -> bool {
        matches!(cap.admit(ip), Admission::Capped(_))
    }

    #[test]
    fn failures_below_the_limit_are_never_capped() {
        let cap = FloodCap::new(3, 60);
        let target = ip("198.51.100.5");
        for _ in 0..3 {
            assert!(!fail(&cap, target));
        }
        // The limit-th failure still processed normally; capping starts
        // with the request after it.
        assert!(fail(&cap, target));
        assert_eq!(cap.failures(target), Some(3));
    }

    #[test]
    fn capped_admissions_change_no_state() {
        let cap = FloodCap::new(1, 60);
        let target = ip("198.51.100.5");
        assert!(!fail(&cap, target));
        for _ in 0..5 {
            assert!(matches!(cap.admit(target), Admission::Capped(_)));
        }
        assert_eq!(
            cap.failures(target),
            Some(1),
            "capped admissions must not move the counter"
        );
        assert_eq!(cap.len(), 1, "capped admissions must not create entries");
    }

    #[test]
    fn released_slots_do_not_count_toward_the_cap() {
        let cap = FloodCap::new(2, 60);
        let target = ip("198.51.100.5");
        // Successful authentications: each holds a slot only until settled.
        for _ in 0..3 {
            let ticket = match cap.admit(target) {
                Admission::Allowed(ticket) => ticket,
                Admission::Capped(_) => panic!("valid traffic must not be capped"),
            };
            cap.release(&ticket);
        }
        assert_eq!(cap.failures(target), Some(0), "releases settle to zero");
        // Two invalid keys fill the budget for real.
        assert!(!fail(&cap, target));
        assert!(!fail(&cap, target));
        assert!(fail(&cap, target));
    }

    #[test]
    fn release_ignores_a_window_that_has_rolled() {
        let cap = FloodCap::new(5, 1);
        let target = ip("198.51.100.5");
        let stale = match cap.admit(target) {
            Admission::Allowed(ticket) => ticket,
            Admission::Capped(_) => panic!(),
        };
        std::thread::sleep(Duration::from_millis(1100));
        // The window rolled and another request was admitted meanwhile.
        assert!(!fail(&cap, target));
        let before = cap.failures(target);
        cap.release(&stale);
        assert_eq!(
            cap.failures(target),
            before,
            "a stale release must not touch the new window"
        );
    }

    #[test]
    fn different_ips_are_independent() {
        let cap = FloodCap::new(1, 60);
        assert!(!fail(&cap, ip("198.51.100.5")));
        assert!(fail(&cap, ip("198.51.100.5")));
        assert!(!fail(&cap, ip("203.0.113.7")));
        assert!(fail(&cap, ip("203.0.113.7")));
        assert_eq!(cap.len(), 2);
    }

    #[test]
    fn window_expiry_resets_the_counter() {
        let cap = FloodCap::new(1, 1);
        let target = ip("198.51.100.5");
        assert!(!fail(&cap, target));
        assert!(fail(&cap, target));

        std::thread::sleep(Duration::from_millis(1100));

        assert!(
            matches!(cap.admit(target), Admission::Allowed(_)),
            "expired window must admit again"
        );
        assert_eq!(
            cap.failures(target),
            Some(1),
            "fresh window counts from one in-flight reservation"
        );
    }

    #[test]
    fn log_slot_is_claimed_once_per_window() {
        let cap = FloodCap::new(1, 60);
        let target = ip("198.51.100.5");
        assert!(!fail(&cap, target));
        let capped = match cap.admit(target) {
            Admission::Capped(ticket) => ticket,
            Admission::Allowed(_) => panic!(),
        };
        assert!(cap.claim_log_slot(&capped));
        assert!(!cap.claim_log_slot(&capped), "the claim is idempotent");
        // The claim must not reset the counter or the window.
        assert_eq!(cap.failures(target), Some(1));
        assert!(fail(&cap, target));
    }

    #[test]
    fn a_claim_never_leaks_into_the_next_window() {
        let cap = FloodCap::new(1, 1);
        let target = ip("198.51.100.5");
        assert!(!fail(&cap, target));
        let capped = match cap.admit(target) {
            Admission::Capped(ticket) => ticket,
            Admission::Allowed(_) => panic!(),
        };
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            !cap.claim_log_slot(&capped),
            "an expired window's claim must be refused, not granted"
        );
        // Even after the new window's entry exists, the stale ticket can
        // neither claim the new row nor duplicate anything.
        assert!(matches!(cap.admit(target), Admission::Allowed(_)));
        assert!(!cap.claim_log_slot(&capped));
    }

    #[test]
    fn a_parallel_burst_cannot_all_admit() {
        let cap = Arc::new(FloodCap::new(2, 60));
        let target = ip("198.51.100.5");
        let handles: Vec<_> = (0..40)
            .map(|_| {
                let cap = Arc::clone(&cap);
                std::thread::spawn(move || matches!(cap.admit(target), Admission::Allowed(_)))
            })
            .collect();
        let allowed = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(
            allowed, 2,
            "only `limit` requests may pass the gate, no matter the burst size"
        );
        assert_eq!(cap.failures(target), Some(2));
    }

    #[test]
    fn limit_zero_disables_the_cap() {
        let cap = FloodCap::new(0, 60);
        assert!(!cap.is_enabled());
        let target = ip("198.51.100.5");
        for _ in 0..100 {
            assert!(matches!(cap.admit(target), Admission::Allowed(_)));
        }
        assert!(cap.is_empty(), "disabled cap must not track anything");
        let ticket = match cap.admit(target) {
            Admission::Allowed(ticket) => ticket,
            Admission::Capped(_) => panic!(),
        };
        assert!(!cap.claim_log_slot(&ticket));
        cap.release(&ticket);
        assert!(cap.is_empty());
    }

    #[test]
    fn entry_count_never_grows_past_the_bound() {
        let cap = FloodCap::new(10, 3600);
        // One past the bound: the last insert must evict instead of grow.
        for i in 0..=MAX_ENTRIES {
            fail(
                &cap,
                ip(&format!("10.{}.{}.{}", i >> 16, (i >> 8) & 0xff, i & 0xff)),
            );
        }
        assert!(
            cap.len() <= MAX_ENTRIES,
            "map must stay bounded, got {}",
            cap.len()
        );
        assert!(!cap.is_empty());
    }
}
