// SPDX-License-Identifier: AGPL-3.0-only

//! The message queue: claim, write-ahead, record, sweep and the per-batch eligibility check.
//! No SMTP here; `campaign::send` drives these in this order for every message:
//!
//! 1. [`claim`] takes a batch and holds it for the lease (`run_after`), spending an attempt.
//! 2. [`eligible`] drops whoever stopped being mailable since launch; the caller records them
//!    [`Verdict::Skipped`].
//! 3. [`mark_submitting`], one committed statement, **before** SMTP. `false` means another claim
//!    holds the row now: drop it without a word.
//! 4. [`record`] the outcome, in flushes.
//!
//! **Why the write-ahead.** A lease alone cannot tell "never sent" from "sent, then the worker died
//! before recording it". `submitting` can: a row in it is never claimed again, and when its lease
//! runs out [`sweep`] records it `unknown`, which is final. A second copy of a campaign email is
//! worse than a lost one, so a crash between the relay's 250 and the record costs a message that
//! was delivered being counted `unknown`, and never a second delivery.
//!
//! **The lease is the claim's.** [`mark_submitting`] does not extend `run_after`: the tick starts
//! no send after half the lease and a session ends within `SESSION_TIMEOUT <= JOB_LEASE / 2`, so a
//! live send finishes inside the lease it was claimed under, and only a dead one is swept.
//!
//! Lock order (ADR 0005): [`claim`], [`mark_submitting`] and [`release`] are one statement on
//! `messages` rows and write no key column. [`record`] and [`sweep`] take `messages`, then
//! `campaigns` by primary key in ascending id order, in one transaction.

use std::{collections::BTreeMap, time::Duration};

use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::worker::backoff;

/// Messages one tick claims, split evenly across the sending campaigns (spec §4.7.2).
pub const CAMPAIGN_BATCH: i64 = 500;

/// Claims before a message gives up. The same 5 as the `messages.max_attempts` column default.
pub const MESSAGE_MAX_ATTEMPTS: i32 = 5;

/// How long a deferred message (`455`, or no relay reachable) stays out of reach. It spent no
/// attempt, so this is a pause, not a backoff.
const DEFERRED_WAIT: f64 = 5.0;

/// What the sweep leaves on a message whose worker vanished after `submitting`.
pub const SWEPT: &str = "the sending worker stopped mid-send; not retried";

/// The claim (spec §4.7.1–2). `$1` campaign ids, `$2` per-campaign limit, `$3` lease seconds.
///
/// `0026`'s protocol, per campaign: `for update skip locked` (two workers take disjoint rows, and
/// neither waits on the other), `run_after` advanced in this statement (it is the lease),
/// `attempts < max_attempts`, and `status = 'queued'`, which is what keeps a `submitting` row
/// from ever being offered again. The lateral gives each campaign its own `limit`, so a 5M-row
/// campaign cannot starve a small one; each lateral walks `messages_runnable` for one campaign.
///
/// The join on `campaigns` makes "a paused or cancelled campaign is never claimed" a property of
/// the database rather than of the caller's state map, which can be a second stale. It takes no
/// lock on `campaigns`: the locking clause belongs to the lateral's `messages`.
///
/// `#[doc(hidden)] pub` for `tests/campaign_queue.rs`, which reads it clause by clause.
#[doc(hidden)]
pub const CLAIM: &str = "update messages m
        set attempts = m.attempts + 1, run_after = now() + make_interval(secs => $3)
      where m.id in (select x.id from unnest($1::uuid[]) as c(id)
                     join campaigns k on k.id = c.id and k.state = 'sending'
                     cross join lateral (select id from messages
                                          where campaign_id = c.id and status = 'queued'
                                            and run_after <= now() and attempts < max_attempts
                                          order by run_after, id
                                          for update skip locked limit $2) x)
  returning m.id, m.workspace_id, m.campaign_id, m.contact_id, m.variant, m.attempts,
            m.max_attempts";

/// A message this worker holds the lease on. `attempts` is the post-increment count, and it is
/// the fence [`mark_submitting`] checks.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Claimed {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub campaign_id: Uuid,
    pub contact_id: Uuid,
    pub variant: i16,
    pub attempts: i32,
    pub max_attempts: i32,
}

/// Takes up to `ceil(batch / campaigns.len())` due messages from each of `campaigns` and holds
/// them for `lease`. **The pool, one statement**: a transaction held across the sends is how a
/// tick outlives its lease.
pub async fn claim(
    db: &PgPool,
    campaigns: &[Uuid],
    batch: i64,
    lease: Duration,
) -> sqlx::Result<Vec<Claimed>> {
    if campaigns.is_empty() || batch <= 0 {
        return Ok(Vec::new());
    }
    let n = i64::try_from(campaigns.len()).unwrap_or(i64::MAX);
    let per_campaign = batch / n + i64::from(batch % n != 0);
    sqlx::query_as(CLAIM)
        .bind(campaigns)
        .bind(per_campaign)
        .bind(lease.as_secs_f64())
        .fetch_all(db)
        .await
}

/// The write-ahead: `queued -> submitting`, committed before SMTP begins. `false` means this
/// worker no longer holds the row (its lease ran out and another claim took it, or it was
/// recorded), and it must not send.
///
/// **The pool, not a transaction**, so it cannot be handed an uncommitted one by accident: a
/// `submitting` that never committed is a send the sweep cannot see.
pub async fn mark_submitting(db: &PgPool, m: &Claimed) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "update messages set status = 'submitting'
          where id = $1 and status = 'queued' and attempts = $2",
    )
    .bind(m.id)
    .bind(m.attempts)
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Who a claimed message goes to, read once per batch.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Recipient {
    pub message_id: Uuid,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub timezone: Option<String>,
    pub attributes: Value,
}

/// The per-batch eligibility check (spec §4.7.5): the subset of `ids` whose contact still exists
/// and is still mailable on one of the campaign's lists. Unsubscribed, unconfirmed, consent
/// withdrawn, suppressed by address, domain or list, or deleted: absent. The caller records every
/// absent id [`Verdict::Skipped`].
///
/// **It does not look at the campaign's state, on purpose.** `Skipped` is final, so a check that
/// dropped a paused campaign's messages would lose them on resume. Paused and cancelled campaigns
/// are kept out by [`CLAIM`] and, between claim and send, by the caller releasing.
pub async fn eligible(db: &PgPool, ids: &[Uuid]) -> sqlx::Result<Vec<Recipient>> {
    sqlx::query_as(
        "select m.id as message_id, k.email, k.first_name, k.last_name, k.timezone, k.attributes
           from messages m
           join campaigns c on c.id = m.campaign_id
           join contacts k on k.workspace_id = m.workspace_id and k.id = m.contact_id
          where m.id = any($1)
            and exists (select 1 from mailable_list_members l
                         where l.workspace_id = m.workspace_id and l.contact_id = m.contact_id
                           and l.list_id = any(c.list_ids))",
    )
    .bind(ids)
    .fetch_all(db)
    .await
}

/// What became of one message. The send loop maps the transport's `Outcome` onto it.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// The relay took it (2xx).
    Sent { relay: Uuid, response: String },
    /// A 4xx about this message: back to `queued` with backoff, the attempt spent; `failed` once
    /// the attempts are used up.
    Retry(String),
    /// The relay, not the message, could not take it (`455`, or never connected): back to
    /// `queued` with the attempt given back.
    Deferred(String),
    /// A 5xx, or the suppression notice. Final.
    Failed(String),
    /// The session broke after `DATA`: it may have been delivered. Final, never retried.
    Unknown(String),
    /// Not mailable at the batch's check. Final, and recorded from `queued`: it never reached
    /// `submitting`.
    Skipped(String),
}

/// Records a flush of outcomes: one statement over the messages, then each touched campaign's
/// counters and finish check, all in one transaction.
///
/// Every verdict is predicated on the state it leaves — `Skipped` from `queued`, every other from
/// `submitting` — so an outcome that arrives after the sweep wrote `unknown` changes nothing, and
/// counts nothing.
pub async fn record(db: &PgPool, outcomes: &[(Claimed, Verdict)]) -> sqlx::Result<()> {
    if outcomes.is_empty() {
        return Ok(());
    }
    let len = outcomes.len();
    let mut ids = Vec::with_capacity(len);
    let mut to = Vec::with_capacity(len);
    let mut from = Vec::with_capacity(len);
    let mut spent: Vec<i32> = Vec::with_capacity(len);
    let mut wait: Vec<Option<f64>> = Vec::with_capacity(len);
    let mut relay: Vec<Option<Uuid>> = Vec::with_capacity(len);
    let mut response: Vec<Option<&str>> = Vec::with_capacity(len);
    let mut error: Vec<Option<&str>> = Vec::with_capacity(len);
    for (m, v) in outcomes {
        let (status, was, give_back, after, by, said, why) = match v {
            Verdict::Sent {
                relay: by,
                response: r,
            } => (
                "sent",
                "submitting",
                0,
                None,
                Some(*by),
                Some(r.as_str()),
                None,
            ),
            Verdict::Retry(e) if m.attempts >= m.max_attempts => (
                "failed",
                "submitting",
                0,
                None,
                None,
                None,
                Some(e.as_str()),
            ),
            Verdict::Retry(e) => (
                "queued",
                "submitting",
                0,
                Some(backoff(m.attempts) as f64),
                None,
                None,
                Some(e.as_str()),
            ),
            Verdict::Deferred(e) => (
                "queued",
                "submitting",
                -1,
                Some(DEFERRED_WAIT),
                None,
                None,
                Some(e.as_str()),
            ),
            Verdict::Failed(e) => (
                "failed",
                "submitting",
                0,
                None,
                None,
                None,
                Some(e.as_str()),
            ),
            Verdict::Unknown(e) => (
                "unknown",
                "submitting",
                0,
                None,
                None,
                None,
                Some(e.as_str()),
            ),
            Verdict::Skipped(e) => ("skipped", "queued", 0, None, None, None, Some(e.as_str())),
        };
        ids.push(m.id);
        to.push(status);
        from.push(was);
        spent.push(give_back);
        wait.push(after);
        relay.push(by);
        response.push(said);
        error.push(why);
    }

    let mut tx = db.begin().await?;
    let moved: Vec<(Uuid, String)> = sqlx::query_as(
        "update messages m
            set status = u.status,
                attempts = greatest(0, m.attempts + u.spent),
                run_after = coalesce(now() + make_interval(secs => u.wait), m.run_after),
                relay_id = coalesce(u.relay_id, m.relay_id),
                response = coalesce(u.response, m.response),
                error = u.error,
                sent_at = case when u.status = 'sent' then now() else m.sent_at end
           from unnest($1::uuid[], $2::text[], $3::text[], $4::int4[], $5::float8[], $6::uuid[],
                       $7::text[], $8::text[])
                as u(id, status, from_status, spent, wait, relay_id, response, error)
          where m.id = u.id and m.status = u.from_status
      returning m.campaign_id, m.status",
    )
    .bind(&ids)
    .bind(&to)
    .bind(&from)
    .bind(&spent)
    .bind(&wait)
    .bind(&relay)
    .bind(&response)
    .bind(&error)
    .fetch_all(&mut *tx)
    .await?;
    count(&mut tx, moved).await?;
    tx.commit().await
}

/// Hands claimed messages back **unattempted**: the claim's increment is taken back and the row
/// waits `wait`. For messages the tick chose not to start (budget, throttle, pause).
///
/// **Only from `queued`.** A `submitting` row may already be at the relay; releasing it would
/// offer it to another claim and send it twice, so this statement cannot reach one.
pub async fn release(db: &PgPool, ids: &[Uuid], wait: Duration, reason: &str) -> sqlx::Result<()> {
    sqlx::query(
        "update messages set attempts = greatest(0, attempts - 1),
                             run_after = now() + make_interval(secs => $2), error = $3
          where id = any($1) and status = 'queued'",
    )
    .bind(ids)
    .bind(wait.as_secs_f64())
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// Records every `submitting` message whose lease has run out as `unknown`, counts them, and
/// finishes any campaign that was waiting on them. Returns how many it swept.
///
/// `skip locked`: a row a flush is recording right now is that flush's, and the sweep never waits
/// on one, so the two cannot deadlock. It is picked up next time if the flush did not move it.
pub async fn sweep(db: &PgPool) -> sqlx::Result<u64> {
    let mut tx = db.begin().await?;
    let swept: Vec<(Uuid, String)> = sqlx::query_as(
        "update messages set status = 'unknown', error = $1
          where id in (select id from messages
                        where status = 'submitting' and run_after <= now()
                        for update skip locked)
      returning campaign_id, status",
    )
    .bind(SWEPT)
    .fetch_all(&mut *tx)
    .await?;
    let n = swept.len() as u64;
    count(&mut tx, swept).await?;
    tx.commit().await?;
    Ok(n)
}

/// Moves a `sending` campaign to `sent` once every message is recorded. A campaign still
/// resolving, or an A/B test waiting for its winner, is not done.
pub async fn finish_if_done(db: &mut PgConnection, campaign_id: Uuid) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "update campaigns set state = 'sent', finished_at = now()
          where id = $1 and state = 'sending' and resolved
            and (ab_test_percent is null or ab_test_percent = 100 or phase = 2)
            and total = sent + failed + unknown + skipped",
    )
    .bind(campaign_id)
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Adds the final statuses in `moved` to their campaigns' counters, one statement per campaign
/// in ascending id order (so two flushes lock campaigns in the same order), then runs the finish
/// check. `queued` rows (a retry, a deferral) count nothing.
async fn count(db: &mut PgConnection, moved: Vec<(Uuid, String)>) -> sqlx::Result<()> {
    let mut by_campaign: BTreeMap<Uuid, [i64; 4]> = BTreeMap::new();
    for (campaign, status) in moved {
        let slot = match status.as_str() {
            "sent" => 0,
            "failed" => 1,
            "unknown" => 2,
            "skipped" => 3,
            _ => continue,
        };
        by_campaign.entry(campaign).or_default()[slot] += 1;
    }
    for (campaign, [sent, failed, unknown, skipped]) in by_campaign {
        sqlx::query(
            "update campaigns set sent = sent + $2, failed = failed + $3,
                                  unknown = unknown + $4, skipped = skipped + $5
              where id = $1",
        )
        .bind(campaign)
        .bind(sent)
        .bind(failed)
        .bind(unknown)
        .bind(skipped)
        .execute(&mut *db)
        .await?;
        finish_if_done(db, campaign).await?;
    }
    Ok(())
}
