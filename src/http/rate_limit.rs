// SPDX-License-Identifier: AGPL-3.0-only

//! A per-workspace request limiter for the endpoints a `viewer` can make expensive (spec M1b
//! section 12). `POST /segments/preview` accepts an arbitrary rule document, costs up to 1.5
//! seconds of a pooled connection, and the rule builder calls it on a debounce as someone types;
//! the pool has 20 connections. Nothing outside sign-in was limited before this.
//!
//! Sign-in's limiter (`auth::rate_limit`) counts rows in `login_attempts`, because a lockout must
//! survive a restart and be seen by every instance — an attacker gets no reset by reconnecting.
//! This one is in memory on purpose: what it protects is **this process's** connection pool, so a
//! per-process counter is the thing that matches what is being defended, and it costs no database
//! round trip on the path whose whole point is to be cheap. It shares sign-in's refusal
//! (`ApiError::TooManyRequests`, and so its `Retry-After`) and adds the `RateLimit` headers
//! `spec.md` promises.
//!
//! A fixed window, not a sliding one: at the end of a window the counter starts again. The worst
//! case is twice the limit across a window boundary, which for a cap on a debounced read is not
//! worth a ring buffer per workspace.
//!
//! shortcut: the counter is per process, so N instances behind a load balancer allow N times the
//! limit; move it to a shared store when NM is run as more than one instance.
//!
//! shortcut: the **key** of the four authenticated limiters is `workspace_id`, so every
//! per-minute figure below bounds one workspace and the bound on the shared pool is
//! `workspaces × limit`. Read the comparisons against the pool's 20 as "one workspace cannot do
//! this on its own", never as a global ceiling — four workspaces starting five exports each inside
//! one minute would hold all 20 connections, because an export holds one for its whole stream and
//! `SEND_TIMEOUT` is 30 s per *chunk*, not a total. That keying is correct and stays: §12 says per
//! workspace, and it is what keeps one tenant from spending another's quota.
//!
//! **The public pages have no workspace until their token is parsed, so they key on the client IP**
//! ([`PUBLIC_PER_MINUTE`]) — and an IP is not a tenant: it is free to mint, which is why [`check`]
//! bounds its map rather than trusting the caller to be few. See THE BOUND on `MAX_KEYS`. Each
//! `RateLimiter` owns its own map, so a flood of IPs fills the public one and cannot touch the
//! four above.
//!
//! [`check`]: RateLimiter::check
//!
//! **Export is the one of the three that now has an instance-wide ceiling as well**, because that
//! day came and was measured: `contacts::export::MAX_CONCURRENT_EXPORTS`, taken in
//! `export_contacts` before the streaming connection is acquired. Preview and selection still have
//! none — they are bounded only by how long each holds a connection, 1.5 s and one statement,
//! where an export holds one for as long as a client takes to read. Do not tune these constants
//! against a ceiling they do not set.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, HeaderValue};
use uuid::Uuid;

/// How many previews one workspace may run per window. Enough for one person typing against a
/// 300 ms debounce with room for a colleague, and well under what one workspace would need to
/// hold the pool's 20 connections for the 1.5 seconds each preview may cost.
///
/// **`GET /segments/counts` spends from this same budget**, one unit per call for up to 25
/// segments — a deliberate decision, recorded in `m1b-decisions.md` as "Counts and previews share
/// one budget". So this number is not "previews a minute": it is previews plus segment-list page
/// loads, and lowering it takes both.
pub const PREVIEW_PER_MINUTE: u32 = 30;

/// How many selections one workspace may materialise per window. Harder than preview, because a
/// selection is the more expensive call of the two: measured at 4,887 ms and 158 MB for a 1M-row
/// match, against preview's 1.5 s of one connection and nothing written. Ten is more than the one
/// active selection the UI has at a time, and still bounds a workspace to under a minute of
/// connection time per window.
pub const SELECTION_PER_MINUTE: u32 = 10;

/// How many exports one workspace may start per window. An export holds a pooled connection for as
/// long as the client takes to read the book, so five a minute is more than a person with a
/// spreadsheet needs and well under the pool's 20. It bounds how many start; how long each may
/// last is `contacts::export::SEND_TIMEOUT`, because a count does nothing about a request that
/// never ends.
pub const EXPORT_PER_MINUTE: u32 = 5;

/// How many CSV uploads one workspace may start per window. Nothing counted them before: the
/// route streams up to [`MAX_IMPORT_BYTES`](crate::contacts::import::routes::MAX_IMPORT_BYTES) to
/// disk per call and leaves the file there until the import is run or swept, and a script could
/// ask for that as fast as it liked.
///
/// **Sixty, and the reason is the opposite of the other three.** Those are sized against the pool,
/// because a preview, a selection and an export each hold a connection for a long time. An upload
/// holds one only for two statements at the end; what it spends is disk and the sniff's CPU, and a
/// single upload's disk is already capped. So this number is not a pool budget — it is the line
/// between a client and a hammer, and it has to sit **above the fastest legitimate client there
/// is**, which is the e2e harness driving the real screen: `web/e2e/import.spec.ts` makes about 25
/// uploads into one workspace, serially, as fast as a browser will go, and
/// `web/e2e/worst-case-data.spec.ts` makes 8 per full `everyScreen` pass — four upload-bearing
/// screens in both themes — over the three passes that take the whole `SCREENS` list, so **24**
/// into its own workspace. (Its fourth call site passes a filtered list with no import screen in
/// it, and uploads nothing.) One a second is past what either can reach — each of those uploads
/// costs a navigation, a file chooser and a round trip — and past what a person at the mapping
/// screen could ever want, and it still stops the thousands a minute a loop would otherwise get.
/// **The margin over the harness is about 2.5×**, so a change to either file that batches uploads
/// more tightly is a change that has to be measured against this number.
///
/// This counts uploads, not runs. What bounds the runs is the advisory lock in `start_import`:
/// one import at a time per workspace, however many files are waiting.
pub const IMPORT_PER_MINUTE: u32 = 60;

/// How many requests one **IP address** may make to the five public pages per window
/// (`/c/`, `/u/`, `/f/`; spec M1d §7.2). One limiter and one budget for all five, because what it
/// protects is the pool they share.
///
/// **An IP is not a tenant, and that changes what this number is for.** The four above each bound
/// one paying workspace, so they can sit near what one workspace legitimately does. An IP is a
/// shared vantage point: a household, an office NAT, a mobile carrier's CGNAT and a mail
/// provider's link-scanning proxy all arrive as a single address, and the moment after a campaign
/// goes out is exactly when a crowd behind one of them clicks at once. So this number has to sit
/// above what a *crowd* behind one address does, not above what one person does — a person
/// confirming, unsubscribing and setting their preferences does well under ten in a sitting.
///
/// Sixty, one a second, is that line. It leaves a NAT'd office room to confirm a send and still
/// stops the thousands a minute a loop would get. It is **not** what bounds this limiter's memory,
/// because an IP costs an attacker nothing: that is `MAX_KEYS`, and the two are separate
/// defences. Raising this number does not weaken the bound, and lowering it does not strengthen it.
pub const PUBLIC_PER_MINUTE: u32 = 60;

const WINDOW: Duration = Duration::from_secs(60);

/// THE BOUND. How many windows one limiter's map may hold at once.
///
/// The map is keyed by whatever [`RateLimiter::check`] is given, and for the public pages that is
/// an IP address an unauthenticated caller chooses. One IPv6 /64 is 18 million million million
/// distinct keys for free, so "how many keys can there be" is not a question the key type answers
/// any more and the map needs a ceiling of its own. At 100,000 entries of a `Key` and a `Window`
/// this is single-digit megabytes per limiter, and it is far above the distinct addresses a real
/// instance sees in a minute, including the burst after a send.
///
/// **The eviction rule, in full:** finished windows are swept — and swept *only* — when a key that
/// is not already in the map arrives and the map is at the ceiling. If that sweep makes room, the
/// new key is stored as usual. If it does not, because every window is still live, the new key is
/// **refused and not stored**: the map never grows past the ceiling, and a request that arrives
/// while 100,000 other addresses are mid-window is answered `429` rather than admitted. Fail
/// closed is deliberate. The alternative — evicting a live window to make room — would let an
/// attacker clear their own counter on demand, which is the limiter undone; refusing costs an
/// unlucky visitor a minute during a flood, which is the limiter working.
///
/// The sweep moved here from "every check" in the same change that admitted IP keys. On every
/// check it was `O(keys)` under a `std::sync::Mutex` held on an async runtime worker, which an
/// unauthenticated caller could have made arbitrarily expensive by supplying arbitrarily many
/// keys. Here it costs `O(MAX_KEYS)` at most once per `MAX_KEYS` new keys — amortised `O(1)` —
/// and every other check is a single hash lookup.
const MAX_KEYS: usize = 100_000;

/// What a window is counted against. A workspace for the authenticated endpoints, an IP for the
/// public pages, and never confusable: a workspace id and an address that happen to collide in
/// their bytes are different keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Workspace(Uuid),
    Ip(IpAddr),
}

impl From<Uuid> for Key {
    fn from(id: Uuid) -> Key {
        Key::Workspace(id)
    }
}

impl From<IpAddr> for Key {
    fn from(ip: IpAddr) -> Key {
        Key::Ip(ip)
    }
}

pub struct RateLimiter {
    limit: u32,
    windows: Mutex<HashMap<Key, Window>>,
}

struct Window {
    ends: Instant,
    used: u32,
}

/// What one `check` decided: the headers to put on the response either way, and `over` carrying
/// the `Retry-After` seconds when this request must be refused.
pub struct Quota {
    pub headers: HeaderMap,
    pub over: Option<u64>,
}

impl RateLimiter {
    pub fn per_minute(limit: u32) -> RateLimiter {
        RateLimiter {
            limit,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one request against `key` — a `Uuid` or an `IpAddr`, either of which converts — and
    /// reports whether it may proceed. A refused request is counted too: the limit is on what
    /// arrives, not on what is served.
    pub fn check(&self, key: impl Into<Key>) -> Quota {
        self.check_at(key.into(), Instant::now())
    }

    /// `check` with the clock passed in, so a test can move time instead of sleeping a window.
    fn check_at(&self, key: Key, now: Instant) -> Quota {
        // A poisoned lock here would mean a panic while holding it, which the block below cannot
        // do; recovering beats turning every later preview into a 500.
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        let known = windows.contains_key(&key);
        // The sweep, and the only place it happens. Everything about why it is here and not on
        // every check, and what happens when it frees nothing, is on `MAX_KEYS`.
        if !known && windows.len() >= MAX_KEYS {
            windows.retain(|_, w| w.ends > now);
        }
        // `None`: the map is full of live windows, so this key is refused and never stored.
        let counted = if known || windows.len() < MAX_KEYS {
            let window = windows.entry(key).or_insert(Window {
                ends: now + WINDOW,
                used: 0,
            });
            // A finished window starts again here. It used to be the sweep that did this, as a
            // side effect of running on every check; now that the sweep is rare, each key's
            // expiry is its own business and cannot wait for a stranger to arrive.
            if window.ends <= now {
                *window = Window {
                    ends: now + WINDOW,
                    used: 0,
                };
            }
            // Unreachable at 4.29e9 requests in one window, but a debug build would panic on it.
            window.used = window.used.saturating_add(1);
            Some((window.used, window.ends.saturating_duration_since(now)))
        } else {
            None
        };
        drop(windows);
        // A refused-because-full key has no window of its own, so it is told to come back after a
        // whole one rather than after a window it is not in.
        let left = counted.map_or(WINDOW, |(_, left)| left);
        // Rounded up, so a reset is never reported as 0 seconds away while the window still holds.
        let reset = left.as_secs() + u64::from(left.subsec_nanos() > 0);
        let remaining = counted.map_or(0, |(used, _)| self.limit.saturating_sub(used));
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-limit", HeaderValue::from(self.limit));
        headers.insert("ratelimit-remaining", HeaderValue::from(remaining));
        headers.insert("ratelimit-reset", HeaderValue::from(reset));
        Quota {
            headers,
            over: counted
                .is_none_or(|(used, _)| used > self.limit)
                .then(|| reset.max(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

    #[test]
    fn the_counter_starts_again_in_the_next_window() {
        let limiter = RateLimiter::per_minute(1);
        let key = Key::Workspace(Uuid::now_v7());
        let start = Instant::now();
        assert!(limiter.check_at(key, start).over.is_none());
        assert!(limiter.check_at(key, start).over.is_some(), "the second");
        let next = start + WINDOW + Duration::from_secs(1);
        let after = limiter.check_at(key, next);
        assert!(after.over.is_none(), "the window has passed");
        assert_eq!(after.headers["ratelimit-remaining"], "0", "one of one used");
    }

    #[test]
    fn a_workspace_is_refused_past_its_limit_and_another_is_not() {
        let limiter = RateLimiter::per_minute(2);
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let first = limiter.check(a);
        assert!(first.over.is_none());
        assert_eq!(first.headers["ratelimit-remaining"], "1");
        assert!(limiter.check(a).over.is_none());
        let third = limiter.check(a);
        assert_eq!(third.over, Some(60), "the third is over the limit of two");
        assert_eq!(third.headers["ratelimit-remaining"], "0");
        assert_eq!(third.headers["ratelimit-limit"], "2");
        assert!(
            limiter.check(b).over.is_none(),
            "the limit is per workspace, not global"
        );
    }

    /// The public pages are reached without a session, so the key is an address the caller picks.
    /// A `/64` is more addresses than there are grains of sand; the map must not follow them.
    #[test]
    fn the_limiter_map_never_grows_past_its_cap_however_many_ips_arrive() {
        let limiter = RateLimiter::per_minute(PUBLIC_PER_MINUTE);
        let start = Instant::now();
        // Every window is still live at `start`, so the sweep frees nothing and the ceiling is
        // the only thing holding the map down.
        let arrivals = MAX_KEYS + MAX_KEYS / 5;
        for n in 0..arrivals {
            let ip = IpAddr::from(Ipv6Addr::from(0x2001_0db8_0000_0000_u128 << 64 | n as u128));
            limiter.check_at(Key::Ip(ip), start);
        }
        let held = limiter.windows.lock().unwrap().len();
        assert_eq!(
            held, MAX_KEYS,
            "{arrivals} addresses arrived; {held} are held"
        );

        // And the request that found it full was refused rather than admitted untracked.
        let last = IpAddr::from(Ipv6Addr::from(
            0x2001_0db8_0000_0000_u128 << 64 | (arrivals - 1) as u128,
        ));
        assert!(limiter.check_at(Key::Ip(last), start).over.is_some());

        // A window that has ended is swept when the next new key needs the room, so the map
        // recovers on its own: the cap is a ceiling, not a permanent lockout.
        let next = start + WINDOW + Duration::from_secs(1);
        let fresh = IpAddr::from(Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, 1));
        assert!(limiter.check_at(Key::Ip(fresh), next).over.is_none());
        assert_eq!(limiter.windows.lock().unwrap().len(), 1, "the sweep ran");
    }
}
