// SPDX-License-Identifier: AGPL-3.0-only

//! The work queue's claim, lease, completion and backoff primitives. Task 11's `run` is the tick
//! that calls them.
//!
//! **The claim protocol is not derived here. It is written down, and the authority is
//! `comment on table jobs`** (`migrations/0026_jobs_references_and_consent_guards.sql`), which
//! supersedes the query in `0023_forms_and_jobs.sql` — that one predates `completed_at` and omits
//! it, so a worker written from `0023` alone re-claims every completed row the moment its lease
//! expires, for ever. `0023` cannot be amended: its checksum has been taken. [`CLAIM`] below is
//! that comment's statement, and `tests/worker_claim.rs` reads it clause by clause.
//!
//! Three rules from the same comment that are not visible in a function signature:
//!
//! - **Nothing deletes a `jobs` row.** The hosted signup form's resend throttle has no state of
//!   its own and counts these rows; a worker that deleted on success would turn a public route
//!   into a mailer anybody could aim at any address, with nothing going red. A delete inside the
//!   retention window is refused by the `jobs_throttle_retention` trigger rather than failing
//!   quietly. Success is [`complete`]; giving up is [`fail`]. Neither removes anything.
//! - **References are columns, not payload.** `contact_id` and `list_id` are real columns with
//!   composite foreign keys carrying `workspace_id`, so [`Claimed`] carries them and nothing digs
//!   them out of the JSON.
//! - **The lease is imported, never re-declared.** [`JOB_LEASE`] lives in
//!   [`crate::relay::transport`], where a compile-time assertion (`SESSION_TIMEOUT < JOB_LEASE`)
//!   and a hang test share one value. A second constant meaning "the lease" is how the two drift,
//!   and a tick that outlives its lease has its rows re-claimed and its messages sent twice.
//!
//! `docs/adr/0005` places all four functions here in one lock class: they touch `jobs` rows alone,
//! and `jobs` is a parent of nothing.

pub mod confirmation;
pub mod deliver;
pub mod test_send;

use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use rand::{TryRng, rngs::SysRng};
use sqlx::{PgExecutor, PgPool};
use tokio::task::JoinSet;
use uuid::Uuid;

use self::confirmation::{Ctx, handle};
use crate::{
    campaign::{
        launch,
        send::{self, CampaignWorker, States},
    },
    config::Config,
    crypto::secrets::Secrets,
    db,
    relay::{self, Relay, bucket, transport::Transports},
};

pub use crate::relay::transport::JOB_LEASE;

/// Rows one tick claims. Enough that a single worker is not the bottleneck at the signup form's
/// own rate limits, small enough that a crash re-does little (spec §5.2).
pub const BATCH: i64 = 16;

/// How often a worker asks for work when the last tick found none.
///
/// **Task 11's tick must jitter this.** That is an obligation, not a description: nothing in this
/// module applies it. A fleet restarted together — which is what a deploy is — otherwise polls in
/// lockstep for ever, every worker hitting the same claim at the same instant and all but one of
/// them doing a round trip for nothing.
///
/// shortcut: polling puts a one-second floor on latency; upgrade to `LISTEN`/`NOTIFY` when a
/// tick's claim regularly comes back full (spec §5.2).
pub const POLL: Duration = Duration::from_secs(1);

/// First backoff, and the base of the doubling: `30 * 2^(attempts - 1)` seconds.
const BACKOFF_BASE_SECS: i64 = 30;

/// An hour. Past this the doubling stops, so a row that keeps failing is still retried hourly
/// rather than drifting out to a delay nobody would wait for.
const BACKOFF_CAP_SECS: i64 = 3_600;

/// The claim, exactly as `comment on table jobs` states it, plus the `kind` filter spec §5.3 adds.
///
/// `$1` kind, `$2` lease in seconds, `$3` batch size.
///
/// Load-bearing, none of it optional — the comment's own list, with its reasons:
///
/// - `for update skip locked`: without it two pollers queue behind one row instead of taking a
///   row each, and one worker's stall is every worker's stall.
/// - `run_after` advanced in **this** statement. It is the lease as well as the schedule; a claim
///   that leaves `run_after` in the past re-offers the row before the first worker has finished,
///   and the message goes twice.
/// - `completed_at is null` (`0025`): without it a completed row's lease expires and it is claimed
///   again, for ever.
/// - `attempts < max_attempts`: a worker that dies after incrementing `attempts` and before
///   writing `failed_at` otherwise leaves a row retried past its limit for ever.
///
/// `returning` names the columns [`Claimed`] carries rather than the comment's `*`; it is a subset
/// of it, and `tests/worker_claim.rs` reads this string for every clause above.
///
/// `#[doc(hidden)] pub` for that test, the way `relay::bucket::TAKE` is public for
/// `tests/lock_order.rs`.
#[doc(hidden)]
pub const CLAIM: &str = "update jobs
        set attempts = attempts + 1, run_after = now() + make_interval(secs => $2)
      where id in (select id from jobs
                    where kind = $1 and run_after <= now() and failed_at is null
                      and completed_at is null and attempts < max_attempts
                    order by run_after, id
                    for update skip locked
                    limit $3)
  returning id, workspace_id, payload, contact_id, list_id, attempts, max_attempts, created_at";

/// A job this worker holds the lease on. `attempts` is the post-increment count — this claim
/// included — so it is what [`defer`] measures against `max_attempts`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Claimed {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub payload: serde_json::Value,
    pub contact_id: Option<Uuid>,
    pub list_id: Option<Uuid>,
    pub attempts: i32,
    pub max_attempts: i32,
    /// When the row was enqueued, not when it was claimed. A `confirmation_email` older than the
    /// token's 14-day TTL carries a link that is already dead and must be failed, not sent.
    pub created_at: DateTime<Utc>,
}

/// Takes up to `batch` runnable rows of `kind` and holds them for `lease_secs`.
///
/// **One statement.** Pass the pool: nothing here needs a transaction, and a transaction holding
/// claimed rows open across the sends is how a tick ends up outliving its own lease.
pub async fn claim(
    db: impl PgExecutor<'_>,
    kind: &str,
    batch: i64,
    lease_secs: i64,
) -> sqlx::Result<Vec<Claimed>> {
    sqlx::query_as::<_, Claimed>(CLAIM)
        .bind(kind)
        .bind(lease_secs as f64)
        .bind(batch)
        .fetch_all(db)
        .await
}

/// Done. The row is **kept** and marked; see this module's note on the resend throttle.
pub async fn complete(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("update jobs set completed_at = now() where id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Gives up, with the reason left on the row. `failed_at` is what answers "why did this
/// confirmation never arrive?" months later, so it is never a bare flag.
pub async fn fail(db: impl PgExecutor<'_>, id: Uuid, reason: &str) -> sqlx::Result<()> {
    sqlx::query("update jobs set failed_at = now(), last_error = $2 where id = $1")
        .bind(id)
        .bind(reason)
        .execute(db)
        .await?;
    Ok(())
}

/// A transient outcome: reschedule with backoff, and leave `reason` on the row so an operator can
/// see what keeps going wrong before it finally gives up.
///
/// **Unless this was the last attempt, in which case it [`fail`]s instead.** Once `attempts`
/// reaches `max_attempts` the claim predicate stops offering the row, so nothing else would ever
/// write `failed_at`: deferring it would leave it sitting silently unclaimable, invisible to an
/// operator asking why a message never arrived.
pub async fn defer(db: impl PgExecutor<'_>, j: &Claimed, reason: &str) -> sqlx::Result<()> {
    if j.attempts >= j.max_attempts {
        return fail(db, j.id, reason).await;
    }
    sqlx::query(
        "update jobs set run_after = now() + make_interval(secs => $2), last_error = $3
          where id = $1",
    )
    .bind(j.id)
    .bind(backoff(j.attempts) as f64)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// Hands a claimed job back **unattempted**: the lease is given up and the claim's own increment
/// of `attempts` is taken back, because nothing was submitted.
///
/// **This is the one path that may undo that increment, and the reason it may is certainty.**
/// `attempts` answers "how many times have we tried to send this". [`claim`] increments it
/// because claiming is where the lease is taken, and for every other path that is right: a worker
/// that dies mid-send must burn an attempt, since nothing can know whether the message went. The
/// tick's budget is the one case where the code knows for certain that nothing was submitted —
/// [`confirmation::handle`] was never called — and counting that as an attempt turns a scheduling
/// decision of ours into mail loss, because the fifth such claim fails the row for good.
///
/// **So its only callers are the two places in [`Worker::tick`] that hold a job back before
/// `handle`, and that narrowness is the safety property.** A `release` reached from a path that
/// did submit something would give that message unbounded retries and send it again and again;
/// `tests/worker_pool.rs` holds both ends, that an unstarted job keeps its attempt and that one
/// which reached a relay spends it.
///
/// `wait` is how long the row stays out of reach, and it is not a backoff — nothing failed, so it
/// does not double and it costs no attempt. A job the tick simply ran out of budget for waits
/// zero: the next tick has a fresh budget and should take it at once. A job the bucket would not
/// pay for waits [`RATE_LIMITED_WAIT`], because zero there is a throttled workspace re-claiming
/// and re-releasing its whole [`BATCH`] every poll, for ever, for no benefit.
async fn release(
    db: impl PgExecutor<'_>,
    j: &Claimed,
    wait: Duration,
    reason: &str,
) -> sqlx::Result<()> {
    // `greatest` rather than a bare subtraction: `jobs_attempts_non_negative` is on the table,
    // and a release of a row claimed by something that did not increment must not violate it.
    sqlx::query(
        "update jobs set attempts = greatest(0, attempts - 1),
                         run_after = now() + make_interval(secs => $2), last_error = $3
          where id = $1",
    )
    .bind(j.id)
    .bind(wait.as_secs_f64())
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// `30 * 2^(attempts - 1)` seconds, capped at an hour, then jittered into the top half of that
/// range: `[base / 2, base]`.
///
/// Jitter rather than the bare doubling because a relay that refuses one message usually refuses
/// the whole batch, and an exact backoff sends that batch back at the relay in lockstep for ever.
/// The bottom half of the range is kept out of it so the nudge can never make the next attempt
/// sooner than half the delay that was asked for.
///
/// The shift is clamped before it is applied: `attempts` is an `int` the database will happily
/// carry to two billion, and `1 << 2_000_000_000` is not a number.
pub(crate) fn backoff(attempts: i32) -> i64 {
    let shift = u32::try_from(attempts.saturating_sub(1))
        .unwrap_or(0)
        .min(32);
    let base = (BACKOFF_BASE_SECS << shift).min(BACKOFF_CAP_SECS);
    let half = base / 2;
    half + jitter(half)
}

/// A whole number of seconds in `0..=span`.
///
/// The OS CSPRNG because it is the only generator this codebase draws on; the modulo bias of a
/// span under two thousand against 2^64 is unmeasurable, and this is a scheduling nudge, not a
/// secret.
fn jitter(span: i64) -> i64 {
    if span <= 0 {
        return 0;
    }
    let mut buf = [0u8; 8];
    SysRng.try_fill_bytes(&mut buf).expect("OS RNG unavailable");
    (u64::from_le_bytes(buf) % (span as u64 + 1)) as i64
}

/// The confirmation kind. `kind` is deliberately open (`0023`), so a second kind is a second
/// match arm here and not a migration: [`test_send::TEST_KIND`] is that second arm.
const KIND: &str = "confirmation_email";

/// What a job the tick never started carries as its reason. A **deferral**, not a failure: a job
/// that was never attempted is always safe to retry.
pub(crate) const BUDGET_SPENT: &str =
    "the tick's budget (half the job lease) was spent before this job was started";

/// What a job the bucket would not pay for carries. Also unattempted: the relay's limit is about
/// this minute, and the message is still worth sending in the next one.
pub(crate) const RATE_LIMITED: &str =
    "every relay of this workspace is at its rate limit for this minute";

/// How long a job the bucket would not pay for stays out of reach.
///
/// **Not a backoff — it is there to stop a spin.** A release with no wait puts the row back at
/// once, so a throttled workspace re-claims and re-releases its whole [`BATCH`] on every poll:
/// about thirty row updates a second, indefinitely, for no benefit, since a bucket worth
/// throttling cannot have refilled in a second. Five seconds is still far below the
/// fifteen-second floor on [`backoff`], so a row the bucket held back is offered again sooner
/// than one that actually failed — which is the order these two deserve.
pub(crate) const RATE_LIMITED_WAIT: Duration = Duration::from_secs(5);

/// What the bucket will let one workspace's share of a batch do this tick.
enum Allowance {
    /// The workspace has no relay to debit at all. Everything goes to the handler unpaid, which
    /// is where the refusal — and its reason — is already written.
    Unmetered,
    /// No relay would grant a token. Nothing is sent and every row is released.
    Throttled,
    /// `n` jobs may be sent, **through `relay` and no other**.
    Granted { relay: Uuid, n: usize },
}

/// The tick: one claimed batch at a time, against a pool of its own.
///
/// Built by [`run`] from a [`Config`], and by `tests/worker_pool.rs` from a [`Ctx`] whose
/// transport talks to an in-process sink.
pub struct Worker {
    /// Shared with every send in flight, which is why the tick holds it behind an `Arc` rather
    /// than building one per message: the DKIM key cache, the pooled SMTP clients and the pool
    /// itself are all in here, and a per-message copy would throw the connections away.
    ctx: Arc<Ctx>,
    concurrency: usize,
    budget: Duration,
}

impl Worker {
    /// `concurrency` is how many messages one tick sends at once; zero reads as one, since a
    /// worker that sends nothing is not a configuration anyone means.
    pub fn new(ctx: Ctx, concurrency: usize) -> Worker {
        Worker {
            ctx: Arc::new(ctx),
            concurrency: concurrency.max(1),
            budget: JOB_LEASE / 2,
        }
    }

    /// A budget other than half the lease, for the test that has to watch one run out.
    /// `a_worker_budgets_its_tick_at_half_the_lease` holds the other end.
    #[doc(hidden)]
    pub fn with_budget(self, budget: Duration) -> Worker {
        Worker { budget, ..self }
    }

    #[doc(hidden)]
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// Ticks until `shutdown` resolves, then returns.
    ///
    /// The signal is read **between** ticks and never inside one: a send cancelled mid-session is
    /// exactly the ambiguity [`crate::relay::transport::Outcome::Unknown`] exists for, and a
    /// shutdown is not worth a message that may have been delivered twice. The tick's budget is
    /// what bounds how long that makes a stop take.
    pub async fn run_until(&self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            match self.tick().await {
                Ok(claimed) => tracing::debug!(claimed, "worker tick"),
                // Keeping going is the point: a claim that failed is usually the database being
                // restarted, and a worker that exited would need an operator to notice.
                Err(e) => tracing::error!(error = %e, "a worker tick could not claim"),
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(poll_interval()) => {}
            }
        }
    }

    /// Claims up to [`BATCH`] rows, debits the bucket once for each workspace's share of them,
    /// and sends. Returns how many rows were claimed.
    pub async fn tick(&self) -> sqlx::Result<usize> {
        let mail = self.tick_mail(KIND).await?;
        let tests = self.tick_mail(test_send::TEST_KIND).await?;
        Ok(mail + tests + self.launch().await?)
    }

    /// Runs one campaign launch slice, if one is due. After the mail: a launch is up to
    /// [`launch::LAUNCH_SLICE`] of work, and a double opt-in email never waits behind a campaign.
    /// One job per tick, and a launch that hands off is claimed again by the next.
    async fn launch(&self) -> sqlx::Result<usize> {
        // A launch whose worker died on its last attempt is claimable by nothing; end it first. A
        // sweep that fails is logged and the claim goes ahead: it must not hold up every launch.
        if let Err(e) = launch::sweep(&self.ctx.db).await {
            tracing::error!(error = %e, "orphaned launches could not be swept");
        }
        let jobs = claim(
            &self.ctx.db,
            launch::LAUNCH_KIND,
            1,
            JOB_LEASE.as_secs() as i64,
        )
        .await?;
        for job in &jobs {
            launch::handle(&self.ctx, job, launch::LAUNCH_SLICE).await;
        }
        Ok(jobs.len())
    }

    /// One kind of mail: `confirmation_email`, or `campaign_test`.
    async fn tick_mail(&self, kind: &'static str) -> sqlx::Result<usize> {
        let started = Instant::now();
        let claimed = claim(&self.ctx.db, kind, BATCH, JOB_LEASE.as_secs() as i64).await?;
        let total = claimed.len();
        if total == 0 {
            return Ok(0);
        }
        // One batch may span workspaces, and a bucket belongs to a relay of one workspace. So the
        // debit is per workspace's share, which is what "once per claimed batch" means here.
        let mut by_workspace: HashMap<Uuid, Vec<Claimed>> = HashMap::new();
        for job in claimed {
            by_workspace.entry(job.workspace_id).or_default().push(job);
        }
        for (workspace_id, mut jobs) in by_workspace {
            // `paid_for` travels with every send of this workspace's share: the relay the debit
            // bought is the relay those messages go through. See `debit`.
            let (paid_for, allowed) = match self.allowance(workspace_id, &jobs).await {
                Ok(Allowance::Granted { relay, n }) => (Some(relay), n),
                Ok(Allowance::Unmetered) => (None, jobs.len()),
                Ok(Allowance::Throttled) => (None, 0),
                Err(reason) => {
                    self.defer_all(&jobs, &reason).await;
                    continue;
                }
            };
            // What the bucket would not pay for waits for the next tick, and is **released**
            // rather than deferred: it was claimed and never submitted, so it spends no attempt.
            // A throttle is not a failure, and five throttled claims must not destroy a message
            // that was only ever waiting its turn — which is reachable now that `debit` asks for
            // at most the relay's whole minute and therefore holds rows back routinely.
            let held_back = jobs.split_off(allowed);
            self.release_all(&held_back, RATE_LIMITED_WAIT, RATE_LIMITED)
                .await;
            self.send(kind, jobs, paid_for, started).await;
        }
        Ok(total)
    }

    /// Spends this workspace's share of the batch against the token bucket. `Err` is the reason
    /// to defer all of them.
    async fn allowance(&self, workspace_id: Uuid, jobs: &[Claimed]) -> Result<Allowance, String> {
        // The error goes to the log and a bounded reason goes on the row: `ApiError` has no
        // `Display`, and its `Debug` carries an `anyhow::Error` that formats a backtrace when one
        // was captured — build-machine paths, in a `last_error` an operator reads in a browser.
        // `confirmation::db_error` makes the same split for the same reason.
        let relays = match relay::enabled_relays(&self.ctx.db, workspace_id).await {
            Ok(relays) => relays,
            Err(e) => {
                tracing::warn!(error = ?e, "the relays could not be read");
                return Err("the relays could not be read".to_owned());
            }
        };
        // A workspace with no relay has nothing to debit, and the refusal is already written —
        // with its reason — in `confirmation::attempt`. Restating it here would be a second copy
        // of a rule that has one owner.
        if relays.is_empty() {
            return Ok(Allowance::Unmetered);
        }
        let n = i32::try_from(jobs.len()).unwrap_or(i32::MAX);
        match debit(&self.ctx.db, workspace_id, &relays, n).await {
            // No relay granting is not an error here: it is every job held back, which the
            // caller releases.
            Ok(None) => Ok(Allowance::Throttled),
            Ok(Some((relay, granted))) => Ok(Allowance::Granted {
                relay,
                n: usize::try_from(granted).unwrap_or(0).min(jobs.len()),
            }),
            // A database error, by contrast, is a failure to be recorded with backoff — and one
            // that must eventually give up, which is what spending an attempt buys.
            Err(e) => Err(format!("the relay bucket could not be debited: {e}")),
        }
    }

    /// Sends `jobs`, at most `concurrency` at a time, and **starts nothing once the tick's budget
    /// is spent**.
    ///
    /// THE ARITHMETIC THIS EXISTS FOR. [`BATCH`] is 16 and `worker_concurrency` defaults to 4, so
    /// a tick is four waves; a relay that burns the whole 60-second
    /// [`crate::relay::transport::SESSION_TIMEOUT`] on each wave takes 240 seconds, which is
    /// twice [`JOB_LEASE`]. A tick that outlives its own lease has its rows re-claimed by another
    /// worker and its messages sent twice — the double send this milestone's outcome table exists
    /// to prevent, arriving by arithmetic rather than by a hang. The budget is half the lease, so
    /// a send started at the last moment still finishes its session inside it.
    ///
    /// The deadline is read by each send as it starts rather than once per wave: a wave admitted
    /// at 59 seconds would run until 119, which is inside the lease only by accident.
    ///
    /// Every job that was not started is **deferred**, with the reason on the row. A job that was
    /// never attempted is always safe to retry; one that was started is governed by its own
    /// outcome.
    async fn send(
        &self,
        kind: &'static str,
        jobs: Vec<Claimed>,
        paid_for: Option<Uuid>,
        started: Instant,
    ) {
        let deadline = started + self.budget;
        let mut running: JoinSet<()> = JoinSet::new();
        for job in jobs {
            while running.len() >= self.concurrency {
                let _ = running.join_next().await;
            }
            let ctx = Arc::clone(&self.ctx);
            running.spawn(async move {
                if Instant::now() >= deadline {
                    // `release`, not `defer`: this job was claimed and never submitted, so it
                    // spends no attempt. See `release`'s own note on why that path is the only
                    // one allowed to undo the claim's increment.
                    // No wait: the next tick has a whole budget again, and this job is the
                    // first thing it should spend it on.
                    if let Err(e) = release(&ctx.db, &job, Duration::ZERO, BUDGET_SPENT).await {
                        tracing::error!(
                            job = %job.id,
                            error = %e,
                            "a job this tick had no budget for could not be deferred"
                        );
                    }
                    return;
                }
                if kind == test_send::TEST_KIND {
                    test_send::handle(&ctx, &job, paid_for).await;
                } else {
                    handle(&ctx, &job, paid_for).await;
                }
            });
        }
        while running.join_next().await.is_some() {}
    }

    /// Reschedules every one of `jobs` with backoff, spending an attempt, with the reason on the
    /// row. For something that went wrong: a `defer` that cannot be written is logged and nothing
    /// else, since the lease expires on its own and the row comes back.
    async fn defer_all(&self, jobs: &[Claimed], reason: &str) {
        for job in jobs {
            if let Err(e) = defer(&self.ctx.db, job, reason).await {
                tracing::error!(job = %job.id, error = %e, "a job could not be deferred");
            }
        }
    }

    /// Hands every one of `jobs` back unattempted. For jobs this tick **chose** not to start,
    /// where nothing went wrong and nothing was submitted; see [`release`].
    async fn release_all(&self, jobs: &[Claimed], wait: Duration, reason: &str) {
        for job in jobs {
            if let Err(e) = release(&self.ctx.db, job, wait, reason).await {
                tracing::error!(job = %job.id, error = %e, "a job could not be released");
            }
        }
    }
}

/// Takes up to `n` tokens from the first relay in failover order that will part with them, and
/// returns **which relay** it spent on and how many it got. `None` means this workspace sends
/// nothing this tick.
///
/// **Which relay is half the answer, not a convenience.** The sends this buys have to go through
/// the relay the tokens came off: `debit` spends on the first relay that *grants*, while
/// `confirmation::submit` sends through the first relay that is *reachable*, and with relay 1
/// exhausted and relay 2 funded those are different relays — a debit of relay 2 followed by a
/// send through relay 1 drives relay 1 past its limit without bound, which empties the bucket of
/// meaning in exactly the configuration it exists for. So the id travels with the batch, and
/// falling over to any other relay takes a token for that one first.
///
/// **It asks for at most the relay's whole minute, and that is not an optimisation.**
/// [`bucket::take`] is all or nothing, so a batch of sixteen asked of a relay rated at ten a
/// minute is refused by the insert arm's `limit >= wanted` — every tick, for ever. The rows are
/// deferred until `max_attempts` runs out and the workspace's mail is destroyed slowly, with
/// every part of the system reporting that it is behaving. A relay rated at 100 an hour is an
/// ordinary cheap plan, so this is reachable rather than theoretical.
///
/// **The pool, never a transaction**, and that is ADR 0005's ruling rather than a preference: its
/// tripwire fires on any transaction holding a `relay_buckets` row across more than one
/// statement, and a tick that wrapped this debit and its sends together would be exactly that —
/// at which point the ruling that `relays` is not a lock class stops describing the code. The
/// parameter is `&PgPool` and not `impl PgExecutor` so that it cannot be handed one by accident.
///
/// `#[doc(hidden)] pub` for `tests/worker_pool.rs`, the way [`CLAIM`] is.
#[doc(hidden)]
pub async fn debit(
    db: &PgPool,
    workspace_id: Uuid,
    relays: &[Relay],
    n: i32,
) -> sqlx::Result<Option<(Uuid, i32)>> {
    for relay in relays {
        let want = n.min(relay.rate_limit_per_minute);
        match bucket::take(
            db,
            workspace_id,
            relay.id,
            relay.rate_limit_per_minute,
            want,
        )
        .await
        {
            Ok(true) => return Ok(Some((relay.id, want))),
            Ok(false) => continue,
            Err(e) if deleted_relay(&e) => {
                tracing::info!(
                    relay = %relay.id,
                    "the relay was deleted while this tick was debiting it; trying the next"
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// A foreign key violation out of [`bucket::take`] means its insert arm pointed at a `relays` row
/// that is no longer there — **an operator deleted the relay while this tick was debiting it**.
/// It is a real and reachable path, and nothing else that statement can raise `23503` for:
/// `relay_buckets`' only foreign key is the composite one onto `relays`.
///
/// So it reads as *this relay is gone*: the tick falls to the next relay in failover order, and
/// the claimed jobs are not failed over a relay the workspace is no longer using.
fn deleted_relay(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23503"))
}

/// [`POLL`], **jittered** — which that constant documents as this tick's obligation rather than
/// as something it already does. Without it a fleet restarted together, which is what a deploy
/// is, wakes on one cadence for ever: every worker claiming against the same rows at the same
/// instant, and all but one of them doing a round trip for nothing.
///
/// Upwards only, into `POLL..=POLL * 1.5`. The one-second floor on claim latency is a property
/// worth keeping as written, and a jitter that could shorten the interval would chip at it.
pub(crate) fn poll_interval() -> Duration {
    let span = i64::try_from(POLL.as_millis() / 2).unwrap_or(0);
    POLL + Duration::from_millis(u64::try_from(jitter(span)).unwrap_or(0))
}

/// What `public_url`'s scheme means for a worker, answered once at boot instead of once per
/// message.
///
/// `Err` refuses to start, `Ok(Some(_))` starts and warns, `Ok(None)` says nothing.
///
/// WHY A WORKER CARES ABOUT A URL'S SCHEME. [`crate::mail::build`] refuses a non-`https`
/// unsubscribe URL, and `Config::public_url` defaults to `http://localhost:8080`. So `nm all` on
/// defaults fails **every** confirmation email. That refusal is right — an `http` unsubscribe
/// link in a real inbox is a defect, and spec section 5.5's guarantee rests on the link being
/// ours and safe to click — but what the operator sees is a worker rejecting everything, once per
/// message, with no visible cause. This is that failure, moved to boot and named.
///
/// THE SPLIT, for which `app::serve`'s `trust_proxy_headers` warning is the precedent on both
/// halves. The refusal fires only where somebody can act on it: an instance subscribers can
/// reach, whose every send would fail. On localhost it is a warning instead, because that is the
/// developer who has every reason to run without TLS and who needs the worker to start anyway —
/// and a refusal they hit on every run is one they route around, which is worse than none.
fn public_url_check(public_url: &url::Url) -> Result<Option<String>, String> {
    if public_url.scheme() == "https" {
        return Ok(None);
    }
    if crate::app::is_local_url(public_url) {
        return Ok(Some(format!(
            "NM_PUBLIC_URL is {public_url}, which is not https, so every confirmation email will \
             be refused: the unsubscribe link this product puts in one must be https. Sending \
             from this instance needs NM_PUBLIC_URL set to an https origin."
        )));
    }
    Err(format!(
        "NM_PUBLIC_URL is {public_url}, which is not https. Every confirmation email would be \
         refused, because the unsubscribe link in it must be https — this worker would claim rows \
         and fail all of them. Set NM_PUBLIC_URL to the https origin subscribers reach this \
         instance at."
    ))
}

/// The worker's two loops and the state refresher, on `ctx`'s pool and SMTP clients, until
/// `shutdown` resolves: the jobs loop (confirmation, test, launch) and the messages loop, side by
/// side, so a campaign tick never sits in front of a double opt-in email (spec M2c §4.7.8).
///
/// `#[doc(hidden)] pub` for `tests/campaign_send.rs`, which runs it against a sink.
#[doc(hidden)]
pub async fn run_loops(
    ctx: Ctx,
    concurrency: usize,
    campaign_concurrency: usize,
    shutdown: impl Future<Output = ()>,
) {
    let pool = ctx.db.clone();
    let messages_ctx = Ctx {
        db: ctx.db.clone(),
        secrets: Arc::clone(&ctx.secrets),
        config: Arc::clone(&ctx.config),
        transports: Arc::clone(&ctx.transports),
    };
    let states = Arc::new(States::default());
    let jobs = Worker::new(ctx, concurrency);
    let messages = CampaignWorker::new(
        Arc::new(messages_ctx),
        Arc::clone(&states),
        campaign_concurrency,
    );
    let (stop, stopped) = tokio::sync::watch::channel(());
    let until = || {
        let mut stopped = stopped.clone();
        async move {
            let _ = stopped.changed().await;
        }
    };
    tokio::join!(
        jobs.run_until(until()),
        messages.run_until(until()),
        send::refresh_until(&states, &pool, until()),
        async {
            shutdown.await;
            let _ = stop.send(());
        },
    );
}

/// Runs the worker until `shutdown` resolves. `nm worker` and `nm all` both call this; **`nm
/// serve` does not**, which is spec M2a section 5.1: scaling the API must not multiply the send
/// rate, and an API host's pool must not be drawn down by sending.
pub async fn run(cfg: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    match public_url_check(&cfg.public_url) {
        Ok(None) => {}
        Ok(Some(warning)) => tracing::warn!("{warning}"),
        Err(refusal) => anyhow::bail!(refusal),
    }
    let pool = db::connect_worker(&cfg).await?;
    // The same `auto_migrate` every other mode honours. An operator who set it does not expect
    // one mode to ignore it, and `nm worker` on its own is a whole deployment of this product.
    if cfg.auto_migrate {
        db::MIGRATOR.run(&pool).await?;
    }
    let (concurrency, campaign_concurrency) =
        (cfg.worker_concurrency, cfg.worker_campaign_concurrency);
    let ctx = Ctx {
        db: pool.clone(),
        secrets: Arc::new(Secrets::new(&cfg.secret_key)),
        config: Arc::new(cfg),
        transports: Arc::new(Transports::new()),
    };
    tracing::info!(
        concurrency,
        campaign_concurrency,
        batch = BATCH,
        "worker started"
    );
    run_loops(ctx, concurrency, campaign_concurrency, shutdown).await;
    pool.close().await;
    tracing::info!("worker stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The poll interval has to vary, and `POLL`'s own documentation says why.** A fleet
    /// restarted together — which is what a deploy is — otherwise wakes on one cadence for ever,
    /// every worker claiming against the same rows at the same instant and all but one of them
    /// doing a round trip for nothing.
    ///
    /// A bound on **variance**, like `backoff_does_not_return_the_same_delay_every_time`, so it
    /// adds no dependence on elapsed time. Fifty draws from the 501 values of the range: a fixed
    /// interval yields one distinct value.
    #[test]
    fn the_poll_interval_is_jittered() {
        let seen: std::collections::HashSet<Duration> = (0..50).map(|_| poll_interval()).collect();
        assert!(
            seen.len() > 1,
            "fifty polls came back identical, so a restarted fleet polls in lockstep: {seen:?}"
        );
        for interval in seen {
            assert!(
                (POLL..=POLL + POLL / 2).contains(&interval),
                "a poll of {interval:?} is outside POLL..=POLL * 1.5"
            );
        }
    }

    fn check(url: &str) -> Result<Option<String>, String> {
        public_url_check(&url.parse().expect("a valid url"))
    }

    /// An `http` public URL on an instance other people reach means every confirmation email is
    /// refused by `mail::build` — an unsubscribe link must be `https` — and the operator sees a
    /// worker rejecting everything with no visible cause. Refusing at boot names the variable.
    #[test]
    fn an_http_public_url_someone_can_reach_refuses_to_start() {
        for url in ["http://mail.example.com", "http://203.0.113.10:8080"] {
            let refusal = check(url).expect_err(url);
            assert!(
                refusal.contains("NM_PUBLIC_URL"),
                "the refusal does not name the variable to change: {refusal}"
            );
        }
    }

    /// On localhost it is a warning and not a refusal, because that is the developer who has
    /// every reason to run without TLS and needs the worker to start anyway.
    #[test]
    fn an_http_public_url_on_localhost_warns_instead() {
        let warning = check("http://localhost:8080")
            .expect("a developer's instance must still start")
            .expect("and must say why its confirmations will fail");
        assert!(
            warning.contains("NM_PUBLIC_URL"),
            "the warning does not name the variable: {warning}"
        );
    }

    /// And on an instance that can actually send, nothing at all. A warning printed on every
    /// correct run is one nobody reads by the end of the week — `app::serve`'s own reasoning
    /// about the `trust_proxy_headers` warning, applied to the same question.
    #[test]
    fn an_https_public_url_says_nothing() {
        assert_eq!(check("https://mail.example.com"), Ok(None));
    }

    /// The shape of the curve, without a database. Bounds, because of the jitter.
    #[test]
    fn backoff_doubles_from_thirty_seconds_and_stops_at_an_hour() {
        for (attempts, lo, hi) in [
            (1, 15, 30),
            (2, 30, 60),
            (5, 240, 480),
            // 30 * 2^7 is 3 840, over the cap already.
            (8, 1_800, 3_600),
            (20, 1_800, 3_600),
            (i32::MAX, 1_800, 3_600),
        ] {
            let b = backoff(attempts);
            assert!(
                (lo..=hi).contains(&b),
                "backoff({attempts}) must land in {lo}..={hi}, got {b}"
            );
        }
        // Zero cannot be claimed (`max_attempts > 0` is on the table and a claim increments before
        // it returns), but the shift must not panic if one ever reaches here.
        assert!((15..=30).contains(&backoff(0)));
    }

    /// **The jitter has to actually vary, and no bound above says so.** Every range asserted there
    /// has `base` as its exact upper end and `base / 2` at or below its lower end, so a `backoff`
    /// that returned a bare `base` — or a `jitter` that returned zero — passes all of them.
    ///
    /// What that would cost is not coverage, it is a thundering herd: a relay that refuses one
    /// message usually refuses the whole batch, and an exact backoff sends every job that failed
    /// in one tick back at that relay at the same instant. It is the pile-up the token bucket
    /// exists to prevent, arriving from the other side.
    ///
    /// This is a bound on **variance**, not on timing, so it adds no dependence on elapsed time.
    /// Fifty draws from the 241 values of attempt 5's range: a fixed implementation yields one
    /// distinct value, and the chance of fifty genuine draws colliding is `241^-49`.
    #[test]
    fn backoff_does_not_return_the_same_delay_every_time() {
        let seen: std::collections::HashSet<i64> = (0..50).map(|_| backoff(5)).collect();
        assert!(
            seen.len() > 1,
            "fifty backoffs over a 240-second range came back identical: {seen:?}"
        );
    }
}
