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
//! ([`PUBLIC_PER_MINUTE`]) — and an IP is not a tenant. A single address is free to mint by the
//! 2^64, so the key is the /64 it sits in (`From<IpAddr> for Key`, via
//! `auth::rate_limit::bucket_of`) and the map has a ceiling on top of that (`MAX_KEYS`). Both are
//! needed: bucketing makes a key cost an allocation, and the ceiling bounds the memory of someone
//! who buys allocations. Each `RateLimiter` owns its own map, so a flood fills the public one and
//! cannot touch the four above.
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
/// stops the thousands a minute a loop would get. It is **not** what bounds this limiter's memory:
/// that is `MAX_KEYS`, and the two are separate defences. Raising this number does not weaken the
/// bound, and lowering it does not strengthen it.
///
/// **The deployment that collapses every client into one key, and it is the documented one.**
/// `request_meta::client_ip` returns the peer address unless `trust_proxy_headers` is on, and that
/// flag defaults to `false`. NM is meant to run behind a reverse proxy (see
/// `http::security_headers`), so on a default configuration behind one, *every* public request
/// keys on the proxy's address: one window, sixty requests a minute for the whole internet, and
/// the 61st person to click their confirmation link after a send gets a `429`. `app::serve` warns
/// at startup for exactly this reason — and only when `public_url` is not localhost, so the
/// warning stays something a reader has not been trained to scroll past. Sign-in's per-IP limiter
/// survives the same deployment only
/// because 100 failures per address per 15 minutes is loose enough to absorb it; sixty a minute
/// across every subscriber is not. **An operator behind a proxy must set `trust_proxy_headers`,
/// and must only do so when the proxy is theirs** — the header is client-controlled otherwise.
///
/// **What the e2e harness spends against it, which is the other thing this number has to clear.**
/// `web/playwright.config.ts` runs `workers: 1` against one `nm serve`, so without help every
/// request in the whole run would key on `127.0.0.1` and share one window of sixty — and `check`
/// counts refusals, so once tripped it stays tripped for the minute. `web/e2e/support/server.ts`
/// therefore sets `NM_TRUST_PROXY_HEADERS`, and each spec that touches a public page claims an
/// `X-Forwarded-For` of its own, so the budget is per spec file rather than per run.
/// `web/e2e/public-form.spec.ts` is the only one today and spends **4** of the 60 on its address:
/// two page views and two submissions. **The margin over the harness is 15×.** A spec added under
/// `/c/`, `/u/` or `/f/` picks a fresh address from RFC 5737's range and counts its own cost
/// here; one that reuses another spec's address inherits its spending, and the failure looks
/// exactly like a product bug.
pub const PUBLIC_PER_MINUTE: u32 = 60;

const WINDOW: Duration = Duration::from_secs(60);

/// THE BOUND. How many windows one limiter's map may hold at once.
///
/// The map is keyed by whatever [`RateLimiter::check`] is given, and for the public pages that is
/// an IP address an unauthenticated caller chooses. Addresses are bucketed to a /64 by
/// `From<IpAddr> for Key`, so a key costs an attacker an allocation rather than a packet — but
/// allocations can be bought, so the map still needs a ceiling of its own. At 100,000 entries of a
/// `Key` and a `Window` this is single-digit megabytes per limiter, and it is far above the
/// distinct allocations a real instance sees in a minute, including the burst after a send.
///
/// **The eviction rule, in full:** finished windows are swept — and swept *only* — when a key that
/// is not already in the map arrives, the map is at the ceiling, and the last sweep was at least
/// `SWEEP_EVERY` ago. If the sweep makes room, the new key is stored as usual. If it does not,
/// because every window is still live, the new key is **refused and not stored**: the map never
/// grows past the ceiling.
///
/// **What that refusal costs, exactly.** Not "one unlucky visitor": while the map holds
/// `MAX_KEYS` live windows, *every* key not already in it is refused, and a first-time visitor is
/// by definition not in it. So for as long as someone can keep the map full, the public surface is
/// closed to everyone new. That is why the /64 bucketing above matters more than this ceiling
/// does — it is what makes the full-map state expensive to buy — and why `PUBLIC_PER_MINUTE` is
/// sized to be hard to reach honestly.
///
/// Failing closed is still the right direction and stays: the alternative, evicting a live window
/// to make room, lets an attacker clear their own counter on demand, which is the limiter undone.
/// What a refusal must never be is an unsubscribe that does not stick (spec M1d §4's second
/// anti-goal). `GET /c/{token}` may answer `429`; `POST /u/{token}` may not, and the task that
/// adds it decides how — not by changing this rule, which protects everyone, but by what that
/// route does with a `Quota` that is over.
///
/// The sweep moved here from "every check" in the same change that admitted IP keys, where it was
/// `O(keys)` on every request under a `std::sync::Mutex` held on an async runtime worker — an
/// unauthenticated caller could make it arbitrarily expensive by supplying arbitrarily many keys.
///
/// **Its cost now, stated so the next reader can check it:** a sweep is `O(MAX_KEYS)` and runs at
/// most once per `SWEEP_EVERY`, whatever arrives — about 10^5 entry visits a second in the worst
/// case, against the 10^5 *per request* it would be without the clock. It is deliberately **not**
/// "amortised `O(1)` per new key": when the map is full the sweep frees nothing and the key is
/// refused without being stored, so there is no state change to amortise against and every new key
/// would re-run the scan. The clock, not the arrival pattern, is what bounds this.
const MAX_KEYS: usize = 100_000;

/// The shortest interval between two sweeps of one limiter's map. A sweep can only free a window
/// that has ended, and windows last `WINDOW`, so sweeping more often than this frees a handful of
/// entries for a full scan.
///
/// **What waiting costs, before anyone raises this number.** A map full of windows that have
/// *ended* stays closed until the next sweep is allowed, and a new key arriving in that interval
/// is not queued — it takes the refusal path like any other key that finds the map full, which is
/// a `429` carrying `Retry-After: 60`, because a key with no window of its own is told to come
/// back after a whole one. So the exposure is not "a key is delayed by `SWEEP_EVERY`"; it is
/// "every new visitor for up to `SWEEP_EVERY` is refused for a minute". At one second that is
/// negligible and errs towards refusing rather than admitting. At sixty it would not be, and this
/// sentence is the one to read before changing the constant.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// What a window is counted against. A workspace for the authenticated endpoints, a [`Bucket`] of
/// addresses for the public pages, and never confusable: a workspace id and an address that happen
/// to collide in their bytes are different keys.
///
/// `Workspace` takes its `Uuid` bare because a workspace id is already the thing being limited and
/// a caller had to be authorised to hold one. An address is not: see [`Bucket`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Workspace(Uuid),
    Ip(Bucket),
}

/// An address reduced to the block it is counted against — **the only thing this module will key a
/// window on**, and the reason the bucketing cannot be skipped by a route that forgets.
///
/// The field is private and there is no constructor but [`From<IpAddr>`](Bucket::from), which
/// buckets. So `Key::Ip(some_raw_address)` does not compile outside this module: the guarantee is
/// the type's, not a convention in a doc comment or a test that has to be kept in step. That
/// matters because keying on raw addresses was the defect this module shipped once already, and
/// the next route to call `check` is one someone else writes.
///
/// `auth::rate_limit::bucket_of` does the masking and carries the reasoning — a /64 comes free
/// with any VPS, so a limiter that counts single addresses stops nobody who holds an allocation.
/// Sign-in has keyed through it since M0; this is the product's one answer, not a second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bucket(IpAddr);

impl From<IpAddr> for Bucket {
    fn from(ip: IpAddr) -> Bucket {
        Bucket(crate::auth::rate_limit::bucket_of(ip))
    }
}

impl From<Uuid> for Key {
    fn from(id: Uuid) -> Key {
        Key::Workspace(id)
    }
}

impl From<IpAddr> for Key {
    fn from(ip: IpAddr) -> Key {
        Key::Ip(ip.into())
    }
}

pub struct RateLimiter {
    limit: u32,
    windows: Mutex<Windows>,
}

struct Windows {
    map: HashMap<Key, Window>,
    /// When `map` was last swept. See `SWEEP_EVERY`.
    swept: Instant,
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
            windows: Mutex::new(Windows {
                map: HashMap::new(),
                swept: Instant::now(),
            }),
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
        let known = windows.map.contains_key(&key);
        // The sweep, and the only place it happens. The clock is what keeps it off the hot path:
        // when the map is full the sweep frees nothing and the key is refused without being
        // stored, so without this check every new key would re-run the whole scan under the lock.
        // Everything about the cost and the eviction rule is on `MAX_KEYS`.
        if !known
            && windows.map.len() >= MAX_KEYS
            && now.saturating_duration_since(windows.swept) >= SWEEP_EVERY
        {
            windows.map.retain(|_, w| w.ends > now);
            windows.swept = now;
        }
        // `None`: the map is full of live windows, so this key is refused and never stored.
        let counted = if known || windows.map.len() < MAX_KEYS {
            let window = windows.map.entry(key).or_insert(Window {
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

    /// The `n`th distinct /64 of the documentation prefix: `2001:db8:<n>::`.
    fn allocation(n: u64) -> IpAddr {
        IpAddr::from(Ipv6Addr::from(
            (0x2001_0db8_u128 << 96) | (u128::from(n) << 64),
        ))
    }

    /// An address *inside* `2001:db8:0::/64`, which an attacker gets 2^64 of for one allocation.
    fn inside_one_allocation(n: u64) -> IpAddr {
        IpAddr::from(Ipv6Addr::from((0x2001_0db8_u128 << 96) | u128::from(n)))
    }

    /// The property the /64 bucketing buys, and the one a raw-address key does not have: an
    /// allocation is one client however many of its addresses it sends from. Without this, sixty
    /// a minute is sixty per address and the public surface has no limit at all.
    #[test]
    fn every_address_in_one_ipv6_allocation_shares_one_window() {
        let limiter = RateLimiter::per_minute(PUBLIC_PER_MINUTE);
        let start = Instant::now();
        for n in 0..u64::from(PUBLIC_PER_MINUTE) {
            let quota = limiter.check_at(inside_one_allocation(n).into(), start);
            assert!(quota.over.is_none(), "address {n} is within the limit");
        }
        // A sixty-first address in the same /64 is the same client, and is refused.
        let over = limiter.check_at(inside_one_allocation(9_999).into(), start);
        assert!(over.over.is_some(), "a new address is not a new budget");
        assert_eq!(
            limiter.windows.lock().unwrap().map.len(),
            1,
            "one allocation, one window"
        );

        // A different allocation is a different client and is not charged for the first one.
        assert!(limiter.check_at(allocation(7).into(), start).over.is_none());
    }

    /// A dual-stack listener may report an IPv4 client as `::ffff:a.b.c.d`. Two budgets for one
    /// client would be a second way past the limit, and masking it to a /64 would be far worse:
    /// every IPv4 address on earth would share one `::` window.
    #[test]
    fn an_ipv4_mapped_address_is_the_same_key_as_the_address_it_carries() {
        let plain: IpAddr = "198.51.100.7".parse().unwrap();
        let mapped: IpAddr = "::ffff:198.51.100.7".parse().unwrap();
        let neighbour: IpAddr = "198.51.100.8".parse().unwrap();
        assert_eq!(Key::from(mapped), Key::from(plain));
        assert_ne!(Key::from(mapped), Key::from(neighbour));
    }

    /// The public pages are reached without a session, so the key is an address the caller picks.
    /// Bucketing makes a key cost an allocation rather than a packet, but allocations can be
    /// bought, so the map still must not follow them without end.
    #[test]
    fn the_limiter_map_never_grows_past_its_cap_however_many_ips_arrive() {
        let limiter = RateLimiter::per_minute(PUBLIC_PER_MINUTE);
        let start = Instant::now();
        // Distinct /64s, so each is genuinely its own key after bucketing — 120,000 addresses in
        // one allocation would be one key and would prove nothing about the ceiling.
        let arrivals = (MAX_KEYS + MAX_KEYS / 5) as u64;
        for n in 0..arrivals {
            limiter.check_at(allocation(n).into(), start);
        }
        let held = limiter.windows.lock().unwrap().map.len();
        assert_eq!(
            held, MAX_KEYS,
            "{arrivals} allocations arrived; {held} are held"
        );

        // And the request that found it full was refused rather than admitted untracked.
        assert!(
            limiter
                .check_at(allocation(arrivals - 1).into(), start)
                .over
                .is_some()
        );

        // A window that has ended is swept when the next new key needs the room, so the map
        // recovers on its own: the cap is a ceiling, not a permanent lockout.
        let next = start + WINDOW + Duration::from_secs(1);
        assert!(
            limiter
                .check_at(allocation(9_999_999).into(), next)
                .over
                .is_none()
        );
        assert_eq!(
            limiter.windows.lock().unwrap().map.len(),
            1,
            "the sweep ran"
        );
    }

    /// The fix for the scan that ran on every request once the map was full. A full map frees
    /// nothing and stores nothing, so there is no state change to amortise against: only the
    /// clock stops the next new key from re-running the whole `retain` under the lock.
    #[test]
    fn a_full_map_sweeps_at_most_once_per_interval_however_many_new_keys_arrive() {
        let limiter = RateLimiter::per_minute(PUBLIC_PER_MINUTE);
        let start = Instant::now();
        for n in 0..MAX_KEYS as u64 {
            limiter.check_at(allocation(n).into(), start);
        }
        // Full, and every window live. `swept` is the limiter's construction time, so the first
        // new key here is more than `SWEEP_EVERY` later only if we say so.
        let full = start + SWEEP_EVERY;
        limiter.check_at(allocation(10_000_000).into(), full);
        assert_eq!(limiter.windows.lock().unwrap().swept, full, "it swept once");

        // Every further new key within the interval leaves `swept` alone: no second scan.
        for n in 0..1_000u64 {
            limiter.check_at(
                allocation(20_000_000 + n).into(),
                full + Duration::from_millis(1),
            );
        }
        assert_eq!(
            limiter.windows.lock().unwrap().swept,
            full,
            "1,000 new keys inside one interval ran the scan again"
        );

        // Once the interval has passed, a new key may sweep again.
        let later = full + SWEEP_EVERY;
        limiter.check_at(allocation(30_000_000).into(), later);
        assert_eq!(limiter.windows.lock().unwrap().swept, later);
    }
}
