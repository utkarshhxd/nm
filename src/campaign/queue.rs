// SPDX-License-Identifier: AGPL-3.0-only

//! The message queue: claim, write-ahead, record, sweep and the per-batch eligibility check.
//! No SMTP here; `campaign::send` drives these in this order for every message:
//!
//! 1. [`claim`] takes a batch and holds it for the lease (`run_after`), spending an attempt.
//! 2. [`eligible`] drops whoever stopped being mailable since launch; the caller records them
//!    [`Verdict::Skipped`].
//! 3. [`mark_submitting`], one committed statement, **before** SMTP. `false` means another claim
//!    holds the row now: drop it without a word.
//! 4. [`record`] the outcome, in flushes, **once**. A flush that errors may be retried; the token
//!    makes a replay touch nothing that a later claim holds.
//!
//! **Why the write-ahead.** A lease alone cannot tell "never sent" from "sent, then the worker died
//! before recording it". `submitting` can: a row in it is never claimed again, and when its lease
//! runs out [`sweep`] records it `unknown`, which is final. A second copy of a campaign email is
//! worse than a lost one, so a crash between the relay's 250 and the record costs a message that
//! was delivered being counted `unknown`, and never a second delivery.
//!
//! **The claim token.** Every claim mints `claim_token`, and [`mark_submitting`], [`record`] and
//! [`release`] act only on the row their own claim holds. `attempts` cannot serve: a release and a
//! deferral give an attempt back, so one value recurs across claims. Without the token a replayed
//! `Retry` could move another claim's `submitting` row back to `queued` while it is at the relay.
//!
//! **Two leases.** The claim's covers a row from claim to mark. [`mark_submitting`] restarts it at
//! [`SUBMIT_LEASE`], counted from the mark: the whole `SESSION_TIMEOUT` plus [`SEND_OVERHEAD`] for
//! the render, build, DKIM signing and flush that run outside the session's timeout. A send that
//! overruns even that is swept `unknown` while it may still be delivering. That is a miscount,
//! never a second copy, because `unknown` is final.
//!
//! Lock order (ADR 0005): [`claim`], [`mark_submitting`] and [`release`] are one statement on
//! `messages` rows and write no key column. [`record`] and [`sweep`] take `messages`, then
//! `campaigns` by primary key in ascending id order, in one transaction. A [`record`] carrying a
//! bounce takes each bounced address's lock and its `suppressions` row first.

use std::{collections::BTreeMap, time::Duration};

use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    crypto::secrets::Secrets,
    relay::transport::{SESSION_TIMEOUT, hard_bounce_code},
    suppressions,
    worker::backoff,
};

/// Messages one tick claims, split evenly across the sending campaigns that have one due (spec
/// §4.7.2).
pub const CAMPAIGN_BATCH: i64 = 500;

/// Claims before a message gives up. The same 5 as the `messages.max_attempts` column default.
pub const MESSAGE_MAX_ATTEMPTS: i32 = 5;

/// How long a deferred message (`455`, or no relay reachable) stays out of reach. It spent no
/// attempt, so this is a pause, not a backoff.
const DEFERRED_WAIT: f64 = 5.0;

/// What a send may spend once it is `submitting`, outside the SMTP session: render, build, DKIM
/// signing and the outcome flush. `SESSION_TIMEOUT` bounds the session alone.
pub const SEND_OVERHEAD: Duration = Duration::from_secs(60);

/// The lease [`mark_submitting`] sets, counted from the mark. Past it the sweep records the
/// message `unknown`.
pub const SUBMIT_LEASE: Duration =
    Duration::from_secs(SESSION_TIMEOUT.as_secs() + SEND_OVERHEAD.as_secs());

/// What the sweep leaves on a message whose worker vanished after `submitting`.
pub const SWEPT: &str = "the sending worker stopped mid-send; not retried";

/// What the sweep leaves on a `queued` message that every attempt claimed and none submitted.
pub const EXHAUSTED: &str = "claimed max_attempts times without an outcome; never submitted";

/// The claim (spec §4.7.1–2). `$1` campaign ids, `$2` the batch, `$3` lease seconds.
///
/// `0026`'s protocol, per campaign: `for update skip locked` (two workers take disjoint rows, and
/// neither waits on the other), `run_after` advanced in this statement (it is the lease),
/// `attempts < max_attempts`, and `status = 'queued'`, which is what keeps a `submitting` row
/// from ever being offered again. The lateral gives each campaign its own `limit`, so a 5M-row
/// campaign cannot starve a small one; each lateral walks `messages_runnable` for one campaign.
/// Each claimed row gets a fresh `claim_token` (`0034`).
///
/// **The batch is split over the campaigns with a message due (`due`), not over every one named.**
/// An A/B test waiting for its winner and a recipient-local send between its recipients' mornings
/// stay `sending` with nothing due, in any workspace; an even split over them would leave a live
/// campaign a sliver of every batch. The `exists` probe is a plain read on `messages_runnable`.
///
/// The join on `campaigns` makes "a paused or cancelled campaign is never claimed" a property of
/// the database rather than of the caller's state map, which can be a second stale. It takes no
/// lock on `campaigns`: the locking clause belongs to the lateral's `messages`.
///
/// `#[doc(hidden)] pub` for `tests/campaign_queue.rs`, which reads it clause by clause.
#[doc(hidden)]
pub const CLAIM: &str = "update messages m
        set attempts = m.attempts + 1, run_after = now() + make_interval(secs => $3),
            claim_token = gen_random_uuid()
      where m.id in (select x.id
                       from (select c.id, count(*) over () as due
                               from unnest($1::uuid[]) as c(id)
                               join campaigns k on k.id = c.id and k.state = 'sending'
                              where exists (select from messages d
                                             where d.campaign_id = c.id and d.status = 'queued'
                                               and d.run_after <= now()
                                               and d.attempts < d.max_attempts)) c
                      cross join lateral (select id from messages
                                           where campaign_id = c.id and status = 'queued'
                                             and run_after <= now() and attempts < max_attempts
                                           order by run_after, id
                                           for update skip locked
                                           limit ($2 + c.due - 1) / c.due) x)
  returning m.id, m.workspace_id, m.campaign_id, m.contact_id, m.variant, m.attempts,
            m.max_attempts, m.claim_token";

/// A message this worker holds the lease on. `attempts` is the post-increment count.
/// `claim_token` is this claim's identity: [`mark_submitting`], [`record`] and [`release`] touch
/// the row only while it still carries it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Claimed {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub campaign_id: Uuid,
    pub contact_id: Uuid,
    pub variant: i16,
    pub attempts: i32,
    pub max_attempts: i32,
    pub claim_token: Uuid,
}

/// Takes up to `ceil(batch / due)` due messages from each of the `due` campaigns in `campaigns`
/// that have one, and holds them for `lease`. **The pool, one statement**: a transaction held
/// across the sends is how a tick outlives its lease.
pub async fn claim(
    db: &PgPool,
    campaigns: &[Uuid],
    batch: i64,
    lease: Duration,
) -> sqlx::Result<Vec<Claimed>> {
    if campaigns.is_empty() || batch <= 0 {
        return Ok(Vec::new());
    }
    sqlx::query_as(CLAIM)
        .bind(campaigns)
        .bind(batch)
        .bind(lease.as_secs_f64())
        .fetch_all(db)
        .await
}

/// The write-ahead: `queued -> submitting`, committed before SMTP begins, with the lease restarted
/// at [`SUBMIT_LEASE`]. `false` means this claim no longer holds the row (another claim took it,
/// or it was released, recorded or swept), and it must not send.
///
/// **The pool, not a transaction**, so it cannot be handed an uncommitted one by accident: a
/// `submitting` that never committed is a send the sweep cannot see.
pub async fn mark_submitting(db: &PgPool, m: &Claimed) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "update messages set status = 'submitting', run_after = now() + make_interval(secs => $4)
          where id = $1 and status = 'queued' and attempts = $2 and claim_token = $3",
    )
    .bind(m.id)
    .bind(m.attempts)
    .bind(m.claim_token)
    .bind(SUBMIT_LEASE.as_secs_f64())
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
    // No `c.state` filter: the caller skips whatever is missing, for good, so filtering a paused
    // campaign here would lose its mail. `eligible_ignores_campaign_state` holds this.
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
    /// A 5xx naming the recipient's mailbox or domain. Final, recorded from `submitting`, and the
    /// address `email` is suppressed in the same transaction.
    Bounced { reason: String, email: String },
    /// The session broke after `DATA`: it may have been delivered. Final, never retried.
    Unknown(String),
    /// Not mailable at the batch's check. Final, and recorded from `queued`: it never reached
    /// `submitting`.
    Skipped(String),
}

/// Records a flush of outcomes: the suppression of every bounced address, one statement over the
/// messages, then each touched campaign's counters and finish check, all in one transaction.
///
/// Every verdict is predicated on the state it leaves — `Skipped` from `queued`, every other from
/// `submitting` — and on the claim's token, so an outcome that arrives after the sweep wrote
/// `unknown`, or a replayed flush after another claim took the row, changes nothing and counts
/// nothing. Recording clears the token: one claim, one outcome.
///
/// **A bounce suppresses its address even when the message moved nothing** (spec M2d §4.5.2): the
/// relay's answer is a fact about the address, whatever the sweep did to the row meanwhile.
/// The relay text stored here is already redacted: every verdict carrying one comes from
/// `worker::deliver::submit`.
pub async fn record(
    db: &PgPool,
    secrets: &Secrets,
    outcomes: &[(Claimed, Verdict)],
) -> sqlx::Result<()> {
    if outcomes.is_empty() {
        return Ok(());
    }
    let bounces: Vec<(Uuid, &str, String)> = outcomes
        .iter()
        .filter_map(|(m, v)| match v {
            Verdict::Bounced { reason, email } => Some((
                m.workspace_id,
                email.as_str(),
                format!(
                    "hard bounce {} at send, campaign {}",
                    hard_bounce_code(reason).unwrap_or("unknown"),
                    m.campaign_id
                ),
            )),
            _ => None,
        })
        .collect();
    let len = outcomes.len();
    let mut ids = Vec::with_capacity(len);
    let mut tokens = Vec::with_capacity(len);
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
            Verdict::Bounced { reason: e, .. } => (
                "bounced",
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
        tokens.push(m.claim_token);
        to.push(status);
        from.push(was);
        spent.push(give_back);
        wait.push(after);
        relay.push(by);
        response.push(said);
        error.push(why);
    }

    let mut tx = db.begin().await?;
    suppressions::record_bounces(&mut tx, secrets, &bounces).await?;
    let moved: Vec<(Uuid, String)> = sqlx::query_as(
        "update messages m
            set status = u.status,
                attempts = greatest(0, m.attempts + u.spent),
                run_after = coalesce(now() + make_interval(secs => u.wait), m.run_after),
                relay_id = coalesce(u.relay_id, m.relay_id),
                response = coalesce(u.response, m.response),
                error = u.error,
                sent_at = case when u.status = 'sent' then now() else m.sent_at end,
                claim_token = null
           from unnest($1::uuid[], $2::text[], $3::text[], $4::int4[], $5::float8[], $6::uuid[],
                       $7::text[], $8::text[], $9::uuid[])
                as u(id, status, from_status, spent, wait, relay_id, response, error, token)
          where m.id = u.id and m.status = u.from_status and m.claim_token = u.token
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
    .bind(&tokens)
    .fetch_all(&mut *tx)
    .await?;
    count(&mut tx, moved).await?;
    tx.commit().await
}

/// Hands claimed messages back **unattempted**: the claim's increment is taken back and the row
/// waits `wait`. For messages the tick chose not to start (budget, throttle, pause).
///
/// **Only from `queued`, and only rows this claim still holds.** A `submitting` row may already be
/// at the relay; releasing it would offer it to another claim and send it twice. A row another
/// claim has taken since is that claim's, and a stale release would cut its lease short.
pub async fn release(
    db: &PgPool,
    held: &[Claimed],
    wait: Duration,
    reason: &str,
) -> sqlx::Result<()> {
    let ids: Vec<Uuid> = held.iter().map(|m| m.id).collect();
    let tokens: Vec<Uuid> = held.iter().map(|m| m.claim_token).collect();
    sqlx::query(
        "update messages m set attempts = greatest(0, m.attempts - 1),
                               run_after = now() + make_interval(secs => $3), error = $4,
                               claim_token = null
           from unnest($1::uuid[], $2::uuid[]) as u(id, token)
          where m.id = u.id and m.status = 'queued' and m.claim_token = u.token",
    )
    .bind(&ids)
    .bind(&tokens)
    .bind(wait.as_secs_f64())
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// Records every `submitting` message whose lease has run out as `unknown`, and fails every
/// `queued` message claimed `max_attempts` times with no outcome. Counts both and finishes any
/// campaign that was waiting on them. Returns how many it moved.
///
/// **Failing the second kind is safe.** Only a claim raises `attempts`; a release and a deferral
/// give it back, and a retry at the limit is already `failed`. So such a row's last claim never
/// reached `submitting`, and no relay ever accepted it. Without this, nothing claims it again and
/// its campaign never finishes. A last holder still alive past its lease loses the race on
/// `status = 'queued'` and the token: its mark returns `false`.
///
/// `skip locked`: a row a flush is recording right now is that flush's, and the sweep never waits
/// on one, so the two cannot deadlock. It is picked up next time if the flush did not move it.
pub async fn sweep(db: &PgPool) -> sqlx::Result<u64> {
    let mut tx = db.begin().await?;
    let mut swept: Vec<(Uuid, String)> = sqlx::query_as(
        "update messages set status = 'unknown', error = $1, claim_token = null
          where id in (select id from messages
                        where status = 'submitting' and run_after <= now()
                        for update skip locked)
      returning campaign_id, status",
    )
    .bind(SWEPT)
    .fetch_all(&mut *tx)
    .await?;
    let exhausted: Vec<(Uuid, String)> = sqlx::query_as(
        "update messages set status = 'failed', error = $1, claim_token = null
          where id in (select id from messages
                        where status = 'queued' and attempts >= max_attempts
                          and run_after <= now()
                        for update skip locked)
      returning campaign_id, status",
    )
    .bind(EXHAUSTED)
    .fetch_all(&mut *tx)
    .await?;
    swept.extend(exhausted);
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
            and total = sent + failed + bounced + unknown + skipped",
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
    let mut by_campaign: BTreeMap<Uuid, [i64; 5]> = BTreeMap::new();
    for (campaign, status) in moved {
        let slot = match status.as_str() {
            "sent" => 0,
            "failed" => 1,
            "unknown" => 2,
            "skipped" => 3,
            "bounced" => 4,
            _ => continue,
        };
        by_campaign.entry(campaign).or_default()[slot] += 1;
    }
    for (campaign, [sent, failed, unknown, skipped, bounced]) in by_campaign {
        sqlx::query(
            "update campaigns set sent = sent + $2, failed = failed + $3,
                                  unknown = unknown + $4, skipped = skipped + $5,
                                  bounced = bounced + $6
              where id = $1",
        )
        .bind(campaign)
        .bind(sent)
        .bind(failed)
        .bind(unknown)
        .bind(skipped)
        .bind(bounced)
        .execute(&mut *db)
        .await?;
        finish_if_done(db, campaign).await?;
    }
    Ok(())
}
