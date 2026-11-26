// SPDX-License-Identifier: AGPL-3.0-only

//! The launch (spec M2c §4.2, §4.3, §4.9): freeze a campaign's content and decide, once, who is
//! mailed. A slip here mails someone who may not be mailed, so the rules are short and each has a
//! test that goes red without it.
//!
//! **Who is mailed.** The contacts that are `mailable_list_members` of one of the campaign's lists
//! (that view is the one definition of "mailable": subscribed, confirmed where the list asks for
//! it, and not suppressed), narrowed by the campaign's segment when it has one. A contact on two
//! lists is one recipient, because the batch selects from `contacts`, not from memberships, and
//! `(campaign_id, contact_id)` is unique. Nothing here re-states what mailable means.
//!
//! **A launch is a chain of jobs.** One `campaign_launch` job freezes the content (`scheduled ->
//! sending`), then resolves the audience in batches of [`LAUNCH_BATCH`] contacts by ascending id,
//! one transaction each, until [`LAUNCH_SLICE`] is spent. It then hands off: the cursor, the next
//! job and this job's completion commit together, so a crash resumes from the cursor and a batch
//! that runs twice inserts nothing twice (`on conflict do nothing`). Every write is fenced on
//! `campaigns.launch_job_id = this job`, so a job superseded by an unschedule, a cancel or a
//! winner pick changes nothing.
//!
//! **Fallbacks, stated.** A recipient-local message is due at the wall-clock time in the contact's
//! own zone; a contact with no zone, or one `chrono-tz` does not know, is due at the CAMPAIGN's
//! zone's instant. A test group is `hashtextextended(contact_id, seed) % 100 < percent`.
//!
//! Lock order (ADR 0005): a batch inserts `messages` (the foreign key takes `for key share` on the
//! campaign at the end of that statement), then updates `campaigns`; the hand-off then writes
//! `jobs`. `mrml` runs only inside [`prepare`], before any transaction opens.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    campaign::{
        self, Campaign, Mode, Refusal, State, prepare,
        queue::finish_if_done,
        schedule::{resolve, zone_instants},
    },
    error::ApiError,
    segments::{self, compile::push_rules, rules::Group},
    worker::{self, confirmation::Ctx},
};

pub const LAUNCH_KIND: &str = "campaign_launch";

/// Contacts one batch looks at, mailable or not sampled in. A batch that looks at fewer is the last.
pub const LAUNCH_BATCH: i64 = 10_000;

/// How long one claim of the launch job resolves before it hands off.
pub const LAUNCH_SLICE: Duration = Duration::from_secs(5);

/// What a job's `last_error` says when the database failed. The cause goes to the log.
const DATABASE: &str = "the launch could not reach the database";

/// Everything a batch needs, read once per claim (the segment's rules are loaded here, so a
/// segment edited mid-slice does not change the slice).
pub struct Resolution {
    pub workspace: Uuid,
    pub campaign: Uuid,
    /// The launch job this resolution is fenced on.
    pub job: Uuid,
    pub list_ids: Vec<Uuid>,
    pub segment: Option<Group>,
    pub phase: i16,
    /// The share that gets the test, when this phase samples.
    pub percent: Option<i16>,
    pub variants: i64,
    pub winner: Option<i16>,
    /// The campaign id's first eight bytes: the sampling seed. The variant hash uses `seed + 1`.
    pub seed: i64,
    /// A recipient-local campaign's own zone's instant: the fallback for a contact without one.
    pub default_at: Option<DateTime<Utc>>,
    /// Every zone a contact may hold and its instant; `None` means everyone is due now.
    pub zones: Option<(Vec<String>, Vec<DateTime<Utc>>)>,
}

/// One batch's outcome. `scanned` counts the contacts looked at (mailable and in the segment),
/// before sampling and before the conflict skip; fewer than [`LAUNCH_BATCH`] means the last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub scanned: u64,
    pub inserted: u64,
    pub last: Option<Uuid>,
    /// The campaign no longer belongs to this job (unscheduled, cancelled, superseded). The batch
    /// was rolled back and nothing was written.
    pub stale: bool,
}

fn missing(field: String, what: &'static str) -> Refusal {
    Refusal::Missing { field, what }
}

fn internal(e: impl std::fmt::Display) -> Refusal {
    Refusal::Internal(e.to_string())
}

impl Resolution {
    /// Reads the audience as it is now. A list or segment that is gone is a refusal that names it
    /// (`campaigns.list_ids` carries no foreign key; this is where a deletion is noticed).
    pub async fn load(db: &PgPool, c: &Campaign, job: Uuid) -> Result<Resolution, Refusal> {
        let found: Vec<Uuid> =
            sqlx::query_scalar("select id from lists where workspace_id = $1 and id = any($2)")
                .bind(c.workspace_id)
                .bind(&c.list_ids)
                .fetch_all(db)
                .await
                .map_err(internal)?;
        if let Some(gone) = c.list_ids.iter().find(|id| !found.contains(id)) {
            return Err(missing(format!("list {gone}"), "to still exist"));
        }
        let segment = match c.segment_id {
            None => None,
            Some(id) => {
                let mut conn = db.acquire().await.map_err(internal)?;
                match segments::load_rules(&mut conn, c.workspace_id, id).await {
                    Ok(group) => Some(group),
                    Err(ApiError::Internal(e)) => return Err(internal(e)),
                    Err(_) => {
                        return Err(missing(
                            format!("segment {id}"),
                            "to still exist and be readable",
                        ));
                    }
                }
            }
        };
        let (zones, default_at) = match (c.schedule_mode, c.schedule_local, &c.schedule_zone) {
            (Mode::RecipientLocal, Some(local), Some(zone)) => {
                let tz = zone.parse().map_err(|_| {
                    missing(
                        format!("schedule zone {zone}"),
                        "to be a zone this build knows",
                    )
                })?;
                (Some(zone_instants(local)), Some(resolve(local, tz)))
            }
            _ => (None, None),
        };
        Ok(Resolution {
            workspace: c.workspace_id,
            campaign: c.id,
            job,
            list_ids: c.list_ids.clone(),
            segment,
            phase: c.phase,
            percent: c.ab_test_percent,
            variants: i64::try_from(c.variants.0.len()).unwrap_or(1),
            winner: c.winner,
            seed: i64::from_be_bytes(c.id.as_bytes()[..8].try_into().expect("8 bytes")),
            default_at,
            zones,
        })
    }
}

/// The batch's statement: pick the next contacts, insert their messages, and report what happened.
fn batch_sql(r: &Resolution, after: Option<Uuid>) -> QueryBuilder<Postgres> {
    // `contacts` is not aliased: the segment compiler's fragments name `contacts.` columns.
    let mut q = QueryBuilder::new(
        "with picked as (select contacts.id, contacts.timezone from contacts where \
         contacts.workspace_id = ",
    );
    q.push_bind(r.workspace).push(" and (");
    q.push_bind(after)
        .push("::uuid is null or contacts.id > ")
        .push_bind(after)
        .push(
            ") and exists (select 1 from mailable_list_members l \
             where l.workspace_id = contacts.workspace_id and l.contact_id = contacts.id \
             and l.list_id = any(",
        )
        .push_bind(&r.list_ids)
        .push("))");
    if let Some(group) = &r.segment {
        q.push(" and ");
        push_rules(&mut q, group);
    }
    q.push(" order by contacts.id limit ")
        .push_bind(LAUNCH_BATCH)
        .push(
            "), ins as (insert into messages \
             (id, workspace_id, campaign_id, contact_id, variant, run_after) \
             select gen_random_uuid(), ",
        )
        .push_bind(r.workspace)
        .push(", ")
        .push_bind(r.campaign)
        .push(", p.id, ");
    // Which variant. Phase 2 is the winner; a test splits evenly by a second hash; one variant is 0.
    match (r.phase, r.winner) {
        (2, Some(winner)) => {
            q.push_bind(winner).push("::smallint");
        }
        _ if r.variants > 1 => {
            q.push("((hashtextextended(p.id::text, ")
                .push_bind(r.seed.wrapping_add(1))
                .push(") & 9223372036854775807) % ")
                .push_bind(r.variants)
                .push(")::smallint");
        }
        _ => {
            q.push("0::smallint");
        }
    }
    q.push(", ");
    match &r.zones {
        Some((names, instants)) => {
            q.push("coalesce(z.due, ").push_bind(r.default_at).push(")");
            q.push(" from picked p left join unnest(")
                .push_bind(names.clone())
                .push("::text[], ")
                .push_bind(instants.clone())
                .push("::timestamptz[]) as z(zone, due) on z.zone = p.timezone");
        }
        None => {
            q.push("now() from picked p");
        }
    }
    q.push(" where true");
    // Phase 1 of a test below 100%: only the sampled group is queued now.
    if let (1, Some(percent), true) = (r.phase, r.percent, r.variants > 1)
        && percent < 100
    {
        q.push(" and (hashtextextended(p.id::text, ")
            .push_bind(r.seed)
            .push(") & 9223372036854775807) % 100 < ")
            .push_bind(percent);
    }
    q.push(
        " on conflict (campaign_id, contact_id) do nothing returning 1) \
         select (select count(*) from picked) as scanned, (select count(*) from ins) as inserted, \
         (select id from picked order by id desc limit 1) as last",
    );
    q
}

/// One batch inside the caller's transaction: the insert, then the campaign's cursor and total.
/// `stale` when the campaign no longer belongs to this job; the caller rolls back.
async fn batch_in(
    tx: &mut PgConnection,
    r: &Resolution,
    after: Option<Uuid>,
) -> sqlx::Result<Batch> {
    let (scanned, inserted, last): (i64, i64, Option<Uuid>) = batch_sql(r, after)
        .build_query_as()
        .fetch_one(&mut *tx)
        .await?;
    let fenced = sqlx::query(
        "update campaigns set resolve_cursor = greatest(resolve_cursor, $4), total = total + $5, \
                updated_at = now()
          where id = $1 and workspace_id = $2 and launch_job_id = $3
            and state in ('sending', 'paused')",
    )
    .bind(r.campaign)
    .bind(r.workspace)
    .bind(r.job)
    .bind(last)
    .bind(inserted)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    Ok(Batch {
        scanned: scanned.unsigned_abs(),
        inserted: inserted.unsigned_abs(),
        last,
        stale: fenced == 0,
    })
}

/// One batch in its own transaction. Run twice from the same cursor, the second inserts nothing
/// and the campaign's `total` does not move.
pub async fn resolve_batch(
    db: &PgPool,
    r: &Resolution,
    after: Option<Uuid>,
) -> sqlx::Result<Batch> {
    let mut tx = db.begin().await?;
    let batch = batch_in(&mut tx, r, after).await?;
    if batch.stale {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(batch)
}

/// Runs one claim of a `campaign_launch` job and records the result on the job itself: completed
/// when it launched, handed off, or had nothing to do; deferred (spending an attempt) when the
/// database failed, because that is not the campaign's fault.
pub async fn handle(ctx: &Ctx, job: &worker::Claimed, slice: Duration) {
    if let Err(reason) = run(ctx, job, slice).await
        && let Err(e) = worker::defer(&ctx.db, job, &reason).await
    {
        tracing::error!(job = %job.id, error = %e, "a launch job could not be deferred");
    }
}

/// `Err` is a transient reason to defer the job with.
async fn run(ctx: &Ctx, job: &worker::Claimed, slice: Duration) -> Result<(), String> {
    let db = &ctx.db;
    let started = Instant::now();
    let fail = |e: sqlx::Error| {
        tracing::warn!(job = %job.id, error = %e, "a launch step failed on the database");
        DATABASE.to_owned()
    };
    let campaign_id: Option<Uuid> =
        sqlx::query_scalar("select campaign_id from jobs where id = $1 and workspace_id = $2")
            .bind(job.id)
            .bind(job.workspace_id)
            .fetch_one(db)
            .await
            .map_err(fail)?;
    let nothing = || async { worker::complete(db, job.id).await.map_err(fail) };
    let Some(campaign_id) = campaign_id else {
        return nothing().await;
    };
    let c = match campaign::load(db, job.workspace_id, campaign_id).await {
        Ok(c) => c,
        Err(ApiError::NotFound) => return nothing().await,
        Err(e) => {
            tracing::warn!(job = %job.id, error = ?e, "a launch could not read its campaign");
            return Err(DATABASE.to_owned());
        }
    };
    // Superseded (unscheduled, cancelled, a winner pick) or not in a state that launches: no-op.
    if c.launch_job_id != Some(job.id)
        || !matches!(c.state, State::Scheduled | State::Sending | State::Paused)
    {
        return nothing().await;
    }

    let resolution = match Resolution::load(db, &c, job.id).await {
        Ok(r) => r,
        Err(refusal) => return stop(db, job, &c, refusal).await,
    };
    // Freeze: `prepare` runs `mrml` on the blocking pool, so no transaction is open here.
    let frozen = if c.state == State::Scheduled {
        match prepare(db, &ctx.config.public_url, &c).await {
            Ok(f) => Some(f),
            Err(refusal) => return stop(db, job, &c, refusal).await,
        }
    } else {
        None
    };
    if let Some(frozen) = frozen {
        let frozen = serde_json::to_value(&frozen).map_err(|e| e.to_string())?;
        let mut tx = db.begin().await.map_err(fail)?;
        let moved = sqlx::query(
            "update campaigns set state = 'sending', frozen = $3, frozen_at = now(), \
                    launched_at = now(), updated_at = now()
              where id = $1 and workspace_id = $2 and launch_job_id = $4 and state = 'scheduled'",
        )
        .bind(c.id)
        .bind(c.workspace_id)
        .bind(frozen)
        .bind(job.id)
        .execute(&mut *tx)
        .await
        .map_err(fail)?
        .rows_affected();
        if moved == 0 {
            // Unscheduled or cancelled while the template compiled.
            worker::complete(&mut *tx, job.id).await.map_err(fail)?;
        }
        tx.commit().await.map_err(fail)?;
        if moved == 0 {
            return Ok(());
        }
    }

    let mut cursor = c.resolve_cursor;
    loop {
        let mut tx = db.begin().await.map_err(fail)?;
        let batch = batch_in(&mut tx, &resolution, cursor).await.map_err(fail)?;
        if batch.stale {
            tx.rollback().await.map_err(fail)?;
            return nothing().await;
        }
        cursor = batch.last.or(cursor);
        let done = i64::try_from(batch.scanned).is_ok_and(|n| n < LAUNCH_BATCH);
        if done {
            sqlx::query(
                "update campaigns set resolved = true, updated_at = now() where id = $1 \
                 and launch_job_id = $2",
            )
            .bind(c.id)
            .bind(job.id)
            .execute(&mut *tx)
            .await
            .map_err(fail)?;
            finish_if_done(&mut tx, c.id).await.map_err(fail)?;
            worker::complete(&mut *tx, job.id).await.map_err(fail)?;
        } else if started.elapsed() >= slice {
            // Hand off: the next job, the campaign's pointer to it and this job's completion are
            // one commit. `campaigns` before `jobs` (ADR 0005).
            let next = Uuid::now_v7();
            sqlx::query("update campaigns set launch_job_id = $2 where id = $1")
                .bind(c.id)
                .bind(next)
                .execute(&mut *tx)
                .await
                .map_err(fail)?;
            sqlx::query(
                "insert into jobs (id, workspace_id, kind, campaign_id, payload, run_after) \
                 values ($1, $2, 'campaign_launch', $3, '{}', now())",
            )
            .bind(next)
            .bind(c.workspace_id)
            .bind(c.id)
            .execute(&mut *tx)
            .await
            .map_err(fail)?;
            worker::complete(&mut *tx, job.id).await.map_err(fail)?;
        }
        tx.commit().await.map_err(fail)?;
        if done || started.elapsed() >= slice {
            return Ok(());
        }
    }
}

/// A refusal: a database error is the job's to retry, anything else ends the launch.
async fn stop(db: &PgPool, job: &worker::Claimed, c: &Campaign, r: Refusal) -> Result<(), String> {
    if let Refusal::Internal(e) = &r {
        tracing::warn!(job = %job.id, error = %e, "a launch step failed on the database");
        return Err(DATABASE.to_owned());
    }
    refuse(db, job, c, &r.to_string()).await.map_err(|e| {
        tracing::warn!(job = %job.id, error = %e, "a launch step failed on the database");
        DATABASE.to_owned()
    })
}

/// A launch that may not go ahead. Nothing sent yet (`scheduled`) means `failed`; something may
/// have been (a later slice, or phase 2) means a system pause the operator decides on. Either way
/// the job is finished: its reason is on the campaign.
async fn refuse(
    db: &PgPool,
    job: &worker::Claimed,
    c: &Campaign,
    reason: &str,
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    if c.state == State::Scheduled {
        sqlx::query(
            "update campaigns set state = 'failed', failure_reason = $3, finished_at = now(), \
                    updated_at = now()
              where id = $1 and launch_job_id = $2 and state = 'scheduled'",
        )
    } else {
        sqlx::query(
            "update campaigns set state = 'paused', pause_reason = $3, updated_at = now()
              where id = $1 and launch_job_id = $2 and state = 'sending'",
        )
    }
    .bind(c.id)
    .bind(job.id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    worker::complete(&mut *tx, job.id).await?;
    tx.commit().await
}
