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
//! shortcut: the **key** is `workspace_id`, so every per-minute figure below bounds one workspace
//! and the bound on the shared pool is `workspaces × limit`. Read the comparisons against the
//! pool's 20 as "one workspace cannot do this on its own", never as a global ceiling — four
//! workspaces starting five exports each inside one minute would hold all 20 connections, because
//! an export holds one for its whole stream and `SEND_TIMEOUT` is 30 s per *chunk*, not a total.
//! The keying is correct and stays: §12 says per workspace, and it is what keeps one tenant from
//! spending another's quota.
//!
//! **Export is the one of the three that now has an instance-wide ceiling as well**, because that
//! day came and was measured: `contacts::export::MAX_CONCURRENT_EXPORTS`, taken in
//! `export_contacts` before the streaming connection is acquired. Preview and selection still have
//! none — they are bounded only by how long each holds a connection, 1.5 s and one statement,
//! where an export holds one for as long as a client takes to read. Do not tune these constants
//! against a ceiling they do not set.

use std::{
    collections::HashMap,
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
/// `web/e2e/worst-case-data.spec.ts` makes 8 more per `everyScreen` pass. One a second is past
/// what either can reach and past what a person at the mapping screen could ever want, and it
/// still stops the thousands a minute a loop would otherwise get.
///
/// This counts uploads, not runs. What bounds the runs is the advisory lock in `start_import`:
/// one import at a time per workspace, however many files are waiting.
pub const IMPORT_PER_MINUTE: u32 = 60;

const WINDOW: Duration = Duration::from_secs(60);

pub struct RateLimiter {
    limit: u32,
    windows: Mutex<HashMap<Uuid, Window>>,
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

    /// Counts one request against `key` and reports whether it may proceed. A refused request is
    /// counted too: the limit is on what arrives, not on what is served.
    pub fn check(&self, key: Uuid) -> Quota {
        self.check_at(key, Instant::now())
    }

    /// `check` with the clock passed in, so a test can move time instead of sleeping a window.
    fn check_at(&self, key: Uuid, now: Instant) -> Quota {
        // A poisoned lock here would mean a panic while holding it, which the block below cannot
        // do; recovering beats turning every later preview into a 500.
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        // Finished windows are dropped on every check, so the map holds one small entry per
        // workspace that previewed in the last minute rather than one per workspace ever seen.
        //
        // shortcut: this is O(active workspaces) per request, under a `std::sync::Mutex` held on
        // an async runtime worker. It is bounded and safe today because `check` is reached only
        // after `authorize` succeeds — verified at all four call sites — so an unauthenticated
        // caller cannot grow the map at all, and a self-hosted instance has few workspaces. The
        // two things that would make it hurt are many workspaces and 30 previews a minute each,
        // which are the same growth this file is sized against: sweep lazily (every Nth check, or
        // only past a threshold) if it ever shows up in a profile.
        windows.retain(|_, w| w.ends > now);
        let window = windows.entry(key).or_insert(Window {
            ends: now + WINDOW,
            used: 0,
        });
        // Unreachable at 4.29e9 requests in one window, but a debug build would panic on it.
        window.used = window.used.saturating_add(1);
        let left = window.ends.saturating_duration_since(now);
        // Rounded up, so a reset is never reported as 0 seconds away while the window still holds.
        let reset = left.as_secs() + u64::from(left.subsec_nanos() > 0);
        let remaining = self.limit.saturating_sub(window.used);
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-limit", HeaderValue::from(self.limit));
        headers.insert("ratelimit-remaining", HeaderValue::from(remaining));
        headers.insert("ratelimit-reset", HeaderValue::from(reset));
        Quota {
            headers,
            over: (window.used > self.limit).then(|| reset.max(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_starts_again_in_the_next_window() {
        let limiter = RateLimiter::per_minute(1);
        let key = Uuid::now_v7();
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
}
