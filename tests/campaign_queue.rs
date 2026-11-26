// SPDX-License-Identifier: AGPL-3.0-only

//! The campaign message queue (`nm::campaign::queue`): claim, write-ahead, record, sweep and the
//! per-batch eligibility check. Spec M2c §4.7 and Review Focus 1 and 4.
//!
//! Time is asserted as bounds read against the database's clock, never as equalities, as in
//! `tests/worker_claim.rs`.

use std::time::Duration;

use nm::{
    campaign::queue::{
        CLAIM, Claimed, EXHAUSTED, MESSAGE_MAX_ATTEMPTS, SUBMIT_LEASE, SWEPT, Verdict, claim,
        eligible, finish_if_done, mark_submitting, record, release, sweep,
    },
    config::SecretKey,
    crypto::{secrets::Secrets, suppression_hmac},
};
use sqlx::{Connection, PgConnection, PgPool};
use tokio::time::timeout;
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(120);

async fn workspace(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, $2, $2)")
        .bind(id)
        .bind(id.simple().to_string())
        .execute(pool)
        .await
        .expect("workspace");
    id
}

/// A `sending` campaign, fully resolved, whose `total` the caller states.
async fn campaign(pool: &PgPool, ws: Uuid, lists: &[Uuid], total: i64) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into campaigns (id, workspace_id, name, state, list_ids, resolved, total)
         values ($1, $2, 'c', 'sending', $3, true, $4)",
    )
    .bind(id)
    .bind(ws)
    .bind(lists)
    .bind(total)
    .execute(pool)
    .await
    .expect("campaign");
    id
}

/// `n` queued messages to made-up contacts (`messages` has no foreign key to `contacts`), due
/// `age` ago.
async fn queue(pool: &PgPool, ws: Uuid, campaign: Uuid, n: i32, age: &str) {
    sqlx::query(
        "insert into messages (id, workspace_id, campaign_id, contact_id, run_after)
         select gen_random_uuid(), $1, $2, gen_random_uuid(), now() - $4::interval
           from generate_series(1, $3)",
    )
    .bind(ws)
    .bind(campaign)
    .bind(n)
    .bind(age)
    .execute(pool)
    .await
    .expect("queue");
}

async fn message(pool: &PgPool, ws: Uuid, campaign: Uuid, contact: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into messages (id, workspace_id, campaign_id, contact_id) values ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(ws)
    .bind(campaign)
    .bind(contact)
    .execute(pool)
    .await
    .expect("message");
    id
}

/// What the passage of the lease does, without sleeping: `run_after` is the lease.
async fn expire(pool: &PgPool, id: Uuid) {
    sqlx::query("update messages set run_after = now() - interval '1 second' where id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("expire");
}

/// `(status, attempts, error)`.
async fn row(pool: &PgPool, id: Uuid) -> (String, i32, Option<String>) {
    sqlx::query_as("select status, attempts, error from messages where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("message row")
}

async fn seconds_until_due(pool: &PgPool, id: Uuid) -> f64 {
    sqlx::query_scalar(
        "select extract(epoch from (run_after - now()))::float8 from messages where id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("run_after")
}

/// `(state, sent, failed, bounced, unknown, skipped)`.
async fn counters(pool: &PgPool, c: Uuid) -> (String, i64, i64, i64, i64, i64) {
    sqlx::query_as(
        "select state, sent, failed, bounced, unknown, skipped from campaigns where id = $1",
    )
    .bind(c)
    .fetch_one(pool)
    .await
    .expect("campaign row")
}

async fn claim_one(pool: &PgPool, c: Uuid) -> Claimed {
    let mut got = claim(pool, &[c], 10, LEASE).await.expect("claim");
    assert_eq!(got.len(), 1, "exactly one message is due");
    got.remove(0)
}

/// What a bounce's suppression is keyed with.
fn secrets() -> Secrets {
    Secrets::new(&SecretKey([7u8; 32]))
}

/// The claim is `0026`'s protocol per campaign, read clause by clause. Every clause except the
/// ordering also has a behavioural test below that goes red without it:
///
/// | clause | test |
/// |---|---|
/// | `for update skip locked` | `two_concurrent_claims_are_disjoint` |
/// | `status = 'queued'` | `a_submitting_row_is_never_claimed` |
/// | `attempts < max_attempts` | `a_message_claimed_to_its_limit_without_an_outcome_is_failed_by_the_sweep` |
/// | `run_after <= now()` | `a_stale_lease_holder_cannot_mark_submitting` |
/// | `attempts + 1` | `a_stale_lease_holder_cannot_mark_submitting` |
/// | `claim_token = gen_random_uuid()` | `a_replayed_release_or_flush_from_an_earlier_claim_touches_nothing` |
/// | `cross join lateral` | `a_batch_is_shared_fairly_across_campaigns` |
/// | `k.state = 'sending'` | `a_paused_or_cancelled_campaign_is_never_claimed` |
/// | `count(*) over () as due` | `sending_campaigns_with_nothing_due_take_no_share_of_the_batch` |
#[test]
fn the_claim_is_the_protocol() {
    let flat = CLAIM.split_whitespace().collect::<Vec<_>>().join(" ");
    for clause in [
        "count(*) over () as due",
        "for update skip locked",
        "status = 'queued'",
        "attempts < max_attempts",
        "run_after <= now()",
        "set attempts = m.attempts + 1, run_after = now() + make_interval(secs => $3), \
         claim_token = gen_random_uuid()",
        "cross join lateral",
        "order by run_after, id",
        "k.state = 'sending'",
    ] {
        assert!(flat.contains(clause), "CLAIM lost `{clause}`: {flat}");
    }
}

/// **Invariant 1, with the interleaving forced.** Session X runs the claim and holds its rows,
/// uncommitted; only then does Y claim. Y must take the other rows at once: without `skip
/// locked` it waits behind X (the timeout fires), and without the locking clause at all it picks
/// X's rows from its snapshot and then waits on them.
#[sqlx::test]
async fn two_concurrent_claims_are_disjoint(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 20).await;
    queue(&pool, ws, c, 20, "1 minute").await;

    let mut x = PgConnection::connect_with(&pool.connect_options())
        .await
        .expect("X connects outside the pool");
    let mut tx = x.begin().await.expect("begin");
    let first: Vec<Claimed> = sqlx::query_as(CLAIM)
        .bind(vec![c])
        .bind(10_i64)
        .bind(LEASE.as_secs_f64())
        .fetch_all(&mut *tx)
        .await
        .expect("X claims");
    assert_eq!(first.len(), 10);

    let second = timeout(Duration::from_secs(5), claim(&pool, &[c], 10, LEASE))
        .await
        .expect("Y must step over X's rows, not wait for them")
        .expect("Y claims");
    tx.commit().await.expect("X commits");

    assert_eq!(second.len(), 10, "Y takes the ten X does not hold");
    let mut all: Vec<Uuid> = first.iter().chain(&second).map(|m| m.id).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 20, "no message is held by both workers");
}

#[sqlx::test]
async fn only_the_named_campaigns_are_claimed(pool: PgPool) {
    let ws = workspace(&pool).await;
    let named = campaign(&pool, ws, &[], 3).await;
    let other = campaign(&pool, ws, &[], 3).await;
    queue(&pool, ws, named, 3, "1 minute").await;
    queue(&pool, ws, other, 3, "1 hour").await;

    let got = claim(&pool, &[named], 500, LEASE).await.expect("claim");
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|m| m.campaign_id == named));
}

/// The claim names campaigns from a state map that may be a second stale; the database's state
/// is what decides. A paused or cancelled campaign's messages are not claimed, so none spends an
/// attempt on a pause.
#[sqlx::test]
async fn a_paused_or_cancelled_campaign_is_never_claimed(pool: PgPool) {
    let ws = workspace(&pool).await;
    let paused = campaign(&pool, ws, &[], 2).await;
    let cancelled = campaign(&pool, ws, &[], 2).await;
    let sending = campaign(&pool, ws, &[], 2).await;
    for c in [paused, cancelled, sending] {
        queue(&pool, ws, c, 2, "1 minute").await;
    }
    for (c, state) in [(paused, "paused"), (cancelled, "cancelled")] {
        sqlx::query("update campaigns set state = $2 where id = $1")
            .bind(c)
            .bind(state)
            .execute(&pool)
            .await
            .expect("set state");
    }

    let got = claim(&pool, &[paused, cancelled, sending], 500, LEASE)
        .await
        .expect("claim");
    assert_eq!(got.len(), 2, "only the sending campaign's messages");
    assert!(got.iter().all(|m| m.campaign_id == sending));
    let spent: i64 = sqlx::query_scalar("select count(*) from messages where attempts > 0")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        spent, 2,
        "no attempt was spent on a paused or cancelled message"
    );
}

/// Spec §4.7.2: 500 split evenly. The big campaign's rows are all older, so a single
/// `order by run_after limit 500` across both would take 500 from it and none from the other.
#[sqlx::test]
async fn a_batch_is_shared_fairly_across_campaigns(pool: PgPool) {
    let ws = workspace(&pool).await;
    let old = campaign(&pool, ws, &[], 1_000).await;
    let new = campaign(&pool, ws, &[], 1_000).await;
    queue(&pool, ws, old, 1_000, "1 hour").await;
    queue(&pool, ws, new, 1_000, "1 minute").await;

    let got = claim(&pool, &[old, new], 500, LEASE).await.expect("claim");
    let from_old = got.iter().filter(|m| m.campaign_id == old).count();
    let from_new = got.iter().filter(|m| m.campaign_id == new).count();
    assert_eq!((from_old, from_new), (250, 250));
}

/// The split is over the campaigns with a message due, not over every one that is `sending`. An
/// A/B test waiting for its winner has nothing queued, and a recipient-local send has rows not due
/// for hours; both stay `sending`, in any workspace. Split over them too, the live campaign below
/// would get a fifth of the batch, and a hundred forgotten A/B tests would leave every live send a
/// handful of messages a tick.
#[sqlx::test]
async fn sending_campaigns_with_nothing_due_take_no_share_of_the_batch(pool: PgPool) {
    let ws = workspace(&pool).await;
    let live = campaign(&pool, ws, &[], 1_000).await;
    queue(&pool, ws, live, 1_000, "1 minute").await;
    let elsewhere = workspace(&pool).await;
    let mut named = vec![live];
    for i in 0..4 {
        let idle = campaign(&pool, elsewhere, &[], 10).await;
        if i % 2 == 1 {
            // Recipient-local: queued, due in an hour.
            queue(&pool, elsewhere, idle, 10, "-1 hour").await;
        }
        named.push(idle);
    }

    let got = claim(&pool, &named, 500, LEASE).await.expect("claim");
    assert_eq!(got.len(), 500, "the live campaign takes the whole batch");
    assert!(got.iter().all(|m| m.campaign_id == live));
}

/// Review Focus 4. A worker whose lease ran out while another claim took the row must not mark
/// it: the claim's `attempts` is the fence.
#[sqlx::test]
async fn a_stale_lease_holder_cannot_mark_submitting(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;

    let stale = claim_one(&pool, c).await;
    let early = claim(&pool, &[c], 10, LEASE).await.expect("claim");
    assert!(early.is_empty(), "a row was re-claimed inside its lease");
    expire(&pool, stale.id).await;
    let current = claim_one(&pool, c).await;
    assert_eq!((stale.attempts, current.attempts), (1, 2));

    assert!(
        !mark_submitting(&pool, &stale).await.expect("mark"),
        "the stale holder marked a row another worker holds"
    );
    assert!(mark_submitting(&pool, &current).await.expect("mark"));
    assert_eq!(row(&pool, current.id).await.0, "submitting");
}

/// The write-ahead's other half: a `submitting` row is never offered again, even with its lease
/// gone. Only the sweep moves it, and only to `unknown`.
#[sqlx::test]
async fn a_submitting_row_is_never_claimed(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;

    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));
    expire(&pool, m.id).await;

    let again = claim(&pool, &[c], 10, LEASE).await.expect("claim");
    assert!(
        again.is_empty(),
        "a submitting row was claimed again: {again:?}"
    );
}

#[sqlx::test]
async fn an_expired_submitting_row_is_swept_to_unknown_and_counted(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 2).await;
    queue(&pool, ws, c, 2, "1 minute").await;
    let held = claim(&pool, &[c], 10, LEASE).await.expect("claim");
    for m in &held {
        assert!(mark_submitting(&pool, m).await.expect("mark"));
    }
    let (dead, live) = (&held[0], &held[1]);
    expire(&pool, dead.id).await;

    assert_eq!(
        sweep(&pool).await.expect("sweep"),
        1,
        "only the expired one"
    );
    assert_eq!(
        row(&pool, dead.id).await,
        ("unknown".into(), 1, Some(SWEPT.into()))
    );
    assert_eq!(
        row(&pool, live.id).await.0,
        "submitting",
        "a live lease is not swept"
    );
    assert_eq!(counters(&pool, c).await, ("sending".into(), 0, 0, 0, 1, 0));

    // The last outstanding message swept finishes the campaign; nothing else would.
    expire(&pool, live.id).await;
    assert_eq!(sweep(&pool).await.expect("sweep"), 1);
    assert_eq!(counters(&pool, c).await, ("sent".into(), 0, 0, 0, 2, 0));
}

/// The worker that the sweep gave up on comes back and reports success. `unknown` stands, and
/// nothing is counted twice.
#[sqlx::test]
async fn a_late_outcome_after_the_sweep_does_not_overwrite_unknown(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));
    expire(&pool, m.id).await;
    assert_eq!(sweep(&pool).await.expect("sweep"), 1);

    let sent = Verdict::Sent {
        relay: Uuid::now_v7(),
        response: "250 ok".into(),
    };
    record(&pool, &secrets(), &[(m.clone(), sent)])
        .await
        .expect("record");

    assert_eq!(row(&pool, m.id).await.0, "unknown");
    let (_, sent, _, _, unknown, _) = counters(&pool, c).await;
    assert_eq!((sent, unknown), (0, 1));
}

/// Spec §4.7.4: a 4xx spends the attempt and backs off (M2a's curve); the fifth fails it.
#[sqlx::test]
async fn a_retry_spends_an_attempt_and_the_fifth_fails(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;

    for attempt in 1..=5 {
        let m = claim_one(&pool, c).await;
        assert_eq!(m.attempts, attempt);
        assert!(mark_submitting(&pool, &m).await.expect("mark"));
        record(
            &pool,
            &secrets(),
            &[(m.clone(), Verdict::Retry("451 later".into()))],
        )
        .await
        .expect("record");
        let (status, attempts, error) = row(&pool, m.id).await;
        assert_eq!(attempts, attempt, "a retry keeps the attempt it spent");
        assert_eq!(error.as_deref(), Some("451 later"));
        if attempt < 5 {
            assert_eq!(status, "queued");
            let base = 30.0 * f64::from(1 << (attempt - 1));
            let wait = seconds_until_due(&pool, m.id).await;
            assert!(
                wait > base / 2.0 - 10.0 && wait <= base,
                "attempt {attempt} backs off {wait}s, outside [{}, {base}]",
                base / 2.0
            );
            expire(&pool, m.id).await;
        } else {
            assert_eq!(status, "failed");
        }
    }
    assert_eq!(counters(&pool, c).await, ("sent".into(), 0, 1, 0, 0, 0));
}

/// Invariant 6's second half, and `release`: neither spends an attempt. And `release` cannot reach
/// a `submitting` row, which would offer a message that may be at the relay to another claim.
#[sqlx::test]
async fn a_deferral_and_a_release_spend_no_attempt(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 3).await;
    queue(&pool, ws, c, 3, "1 minute").await;
    let held = claim(&pool, &[c], 10, LEASE).await.expect("claim");
    let (deferred, released, in_flight) = (&held[0], &held[1], &held[2]);
    assert!(mark_submitting(&pool, deferred).await.expect("mark"));
    assert!(mark_submitting(&pool, in_flight).await.expect("mark"));

    record(
        &pool,
        &secrets(),
        &[(deferred.clone(), Verdict::Deferred("455 busy".into()))],
    )
    .await
    .expect("record");
    assert_eq!(
        row(&pool, deferred.id).await,
        ("queued".into(), 0, Some("455 busy".into()))
    );
    let wait = seconds_until_due(&pool, deferred.id).await;
    assert!((3.0..=5.0).contains(&wait), "deferred for {wait}s, not ~5");

    release(
        &pool,
        &[released.clone(), in_flight.clone()],
        Duration::from_secs(30),
        "budget",
    )
    .await
    .expect("release");
    assert_eq!(
        row(&pool, released.id).await,
        ("queued".into(), 0, Some("budget".into()))
    );
    let wait = seconds_until_due(&pool, released.id).await;
    assert!(
        (20.0..=30.0).contains(&wait),
        "released for {wait}s, not ~30"
    );
    assert_eq!(
        row(&pool, in_flight.id).await,
        ("submitting".into(), 1, None),
        "release reached a submitting row"
    );
}

#[sqlx::test]
async fn recording_moves_the_counters_and_finishes_the_campaign(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 4).await;
    queue(&pool, ws, c, 4, "1 minute").await;
    let held = claim(&pool, &[c], 10, LEASE).await.expect("claim");
    let [s, f, u, k] = [&held[0], &held[1], &held[2], &held[3]];
    for m in [s, f, u] {
        assert!(mark_submitting(&pool, m).await.expect("mark"));
    }
    let relay = Uuid::now_v7();

    record(
        &pool,
        &secrets(),
        &[
            (
                s.clone(),
                Verdict::Sent {
                    relay,
                    response: "250 queued".into(),
                },
            ),
            (f.clone(), Verdict::Failed("550 no".into())),
        ],
    )
    .await
    .expect("first flush");
    assert_eq!(counters(&pool, c).await, ("sending".into(), 1, 1, 0, 0, 0));

    record(
        &pool,
        &secrets(),
        &[
            (u.clone(), Verdict::Unknown("broke after DATA".into())),
            (
                k.clone(),
                Verdict::Skipped("not mailable since launch".into()),
            ),
        ],
    )
    .await
    .expect("second flush");
    assert_eq!(counters(&pool, c).await, ("sent".into(), 1, 1, 0, 1, 1));

    let (relay_id, response, sent_at, finished): (Option<Uuid>, Option<String>, bool, bool) =
        sqlx::query_as(
            "select m.relay_id, m.response, m.sent_at is not null, c.finished_at is not null
               from messages m join campaigns c on c.id = m.campaign_id where m.id = $1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .expect("read");
    assert_eq!(
        (relay_id, response.as_deref(), sent_at, finished),
        (Some(relay), Some("250 queued"), true, true)
    );
    for (m, status) in [(f, "failed"), (u, "unknown"), (k, "skipped")] {
        assert_eq!(row(&pool, m.id).await.0, status);
    }
}

/// An A/B test below 100% waits in phase 1 for an admin to pick the winner, with every test
/// message recorded. It is not finished.
#[sqlx::test]
async fn an_awaiting_winner_campaign_does_not_finish(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    sqlx::query("update campaigns set ab_test_percent = 20, phase = 1 where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .expect("ab");
    queue(&pool, ws, c, 1, "1 minute").await;
    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));
    let sent = Verdict::Sent {
        relay: Uuid::now_v7(),
        response: "250".into(),
    };
    record(&pool, &secrets(), &[(m, sent)])
        .await
        .expect("record");
    assert_eq!(counters(&pool, c).await, ("sending".into(), 1, 0, 0, 0, 0));

    // Control: the same campaign in phase 2 is done.
    sqlx::query("update campaigns set phase = 2 where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .expect("phase 2");
    let mut conn = pool.acquire().await.expect("conn");
    assert!(finish_if_done(&mut conn, c).await.expect("finish"));
}

async fn list(pool: &PgPool, ws: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, $3)")
        .bind(id)
        .bind(ws)
        .bind(id.simple().to_string())
        .execute(pool)
        .await
        .expect("list");
    id
}

async fn contact(pool: &PgPool, ws: Uuid, email: &str, member_of: &[(Uuid, &str)]) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into contacts (id, workspace_id, email, first_name, timezone, attributes)
         values ($1, $2, $3, 'Ada', 'Europe/London', '{\"plan\": \"pro\"}')",
    )
    .bind(id)
    .bind(ws)
    .bind(email)
    .execute(pool)
    .await
    .expect("contact");
    for (list, status) in member_of {
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status)
             values ($1, $2, $3, $4)",
        )
        .bind(ws)
        .bind(id)
        .bind(list)
        .bind(status)
        .execute(pool)
        .await
        .expect("membership");
    }
    id
}

/// Review Focus 1, at the queue's level: the check is per batch, against the state *now*, not at
/// launch. The campaign mails the union of two lists; a member of a third list it does not name
/// is not its recipient.
#[sqlx::test]
async fn eligible_drops_a_member_who_unsubscribed_or_was_suppressed_after_launch(pool: PgPool) {
    let ws = workspace(&pool).await;
    let (l1, l2, other) = (
        list(&pool, ws).await,
        list(&pool, ws).await,
        list(&pool, ws).await,
    );
    let c = campaign(&pool, ws, &[l1, l2], 5).await;
    let on_l1 = contact(&pool, ws, "a@ok.test", &[(l1, "subscribed")]).await;
    let on_l2 = contact(&pool, ws, "b@ok.test", &[(l2, "subscribed")]).await;
    let unsub = contact(
        &pool,
        ws,
        "u@ok.test",
        &[(l1, "subscribed"), (other, "subscribed")],
    )
    .await;
    let suppressed = contact(&pool, ws, "s@blocked.test", &[(l1, "subscribed")]).await;
    let pending = contact(&pool, ws, "p@ok.test", &[(l1, "pending")]).await;
    let mut ids = Vec::new();
    for k in [on_l1, on_l2, unsub, suppressed, pending] {
        ids.push(message(&pool, ws, c, k).await);
    }

    // After launch: one unsubscribes from the campaign's list, one's domain is suppressed.
    sqlx::query("update list_memberships set status = 'unsubscribed' where contact_id = $1 and list_id = $2")
        .bind(unsub)
        .bind(l1)
        .execute(&pool)
        .await
        .expect("unsubscribe");
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, domain, reason)
         values ($1, $2, 'domain', 'blocked.test', 'manual')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .execute(&pool)
    .await
    .expect("suppress");

    let mut got = eligible(&pool, &ids).await.expect("eligible");
    got.sort_by_key(|r| r.email.clone());
    let emails: Vec<&str> = got.iter().map(|r| r.email.as_str()).collect();
    assert_eq!(emails, ["a@ok.test", "b@ok.test"]);
    let a = &got[0];
    assert_eq!(a.message_id, ids[0]);
    assert_eq!(
        (
            a.first_name.as_str(),
            a.last_name.as_str(),
            a.timezone.as_deref()
        ),
        ("Ada", "", Some("Europe/London"))
    );
    assert_eq!(a.attributes, serde_json::json!({"plan": "pro"}));
}

/// Spec §4.2.5: `messages` has no foreign key to `contacts`, so a deleted contact's message is
/// still there; the check is what skips it.
#[sqlx::test]
async fn eligible_drops_a_deleted_contact(pool: PgPool) {
    let ws = workspace(&pool).await;
    let l = list(&pool, ws).await;
    let c = campaign(&pool, ws, &[l], 2).await;
    let kept = contact(&pool, ws, "k@ok.test", &[(l, "subscribed")]).await;
    let gone = contact(&pool, ws, "g@ok.test", &[(l, "subscribed")]).await;
    let ids = [
        message(&pool, ws, c, kept).await,
        message(&pool, ws, c, gone).await,
    ];
    sqlx::query("delete from contacts where id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("delete contact");

    let got = eligible(&pool, &ids).await.expect("eligible");
    let ids_back: Vec<Uuid> = got.iter().map(|r| r.message_id).collect();
    assert_eq!(ids_back, [ids[0]]);
}

/// `(status, attempts, run_after, claim_token, error)`, to show a statement touched nothing.
async fn snapshot(pool: &PgPool, id: Uuid) -> (String, i32, String, Option<Uuid>, Option<String>) {
    sqlx::query_as(
        "select status, attempts, run_after::text, claim_token, error from messages where id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("message row")
}

/// The holder dies before `submitting`, at every attempt. The claim stops offering the row at
/// `max_attempts`, and the sweep fails it so the campaign can finish. A row at the limit whose
/// lease is still live is left alone, and its holder, back late, can no longer mark it.
#[sqlx::test]
async fn a_message_claimed_to_its_limit_without_an_outcome_is_failed_by_the_sweep(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    for _ in 1..MESSAGE_MAX_ATTEMPTS {
        let m = claim_one(&pool, c).await;
        expire(&pool, m.id).await;
    }
    let last = claim_one(&pool, c).await;
    assert_eq!(last.attempts, MESSAGE_MAX_ATTEMPTS);

    assert_eq!(
        sweep(&pool).await.expect("sweep"),
        0,
        "the last lease is live"
    );
    expire(&pool, last.id).await;
    let again = claim(&pool, &[c], 10, LEASE).await.expect("claim");
    assert!(
        again.is_empty(),
        "a row at max_attempts was claimed: {again:?}"
    );

    assert_eq!(sweep(&pool).await.expect("sweep"), 1);
    assert_eq!(
        row(&pool, last.id).await,
        (
            "failed".into(),
            MESSAGE_MAX_ATTEMPTS,
            Some(EXHAUSTED.into())
        )
    );
    assert_eq!(counters(&pool, c).await, ("sent".into(), 0, 1, 0, 0, 0));
    assert!(
        !mark_submitting(&pool, &last).await.expect("mark"),
        "the late holder marked a failed row"
    );
}

/// Spec §4.2.3: launch resolves in batches while the first ones send, so every message present can
/// be recorded before the last batch is inserted. A campaign that finished then would never be
/// claimed again, and its later batches would never be sent.
#[sqlx::test]
async fn a_campaign_still_resolving_does_not_finish(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    sqlx::query("update campaigns set resolved = false where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .expect("resolving");
    queue(&pool, ws, c, 1, "1 minute").await;
    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));
    let sent = Verdict::Sent {
        relay: Uuid::now_v7(),
        response: "250".into(),
    };
    record(&pool, &secrets(), &[(m, sent)])
        .await
        .expect("record");
    assert_eq!(counters(&pool, c).await, ("sending".into(), 1, 0, 0, 0, 0));

    // Control: once resolution is done, the same campaign finishes.
    sqlx::query("update campaigns set resolved = true where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .expect("resolved");
    let mut conn = pool.acquire().await.expect("conn");
    assert!(finish_if_done(&mut conn, c).await.expect("finish"));
}

/// The caller records whatever `eligible` leaves out as `Skipped`, which is final. A campaign
/// paused between the claim and this check must keep its mailable recipients, or resume finds
/// them skipped and their mail is lost.
#[sqlx::test]
async fn eligible_ignores_campaign_state(pool: PgPool) {
    let ws = workspace(&pool).await;
    let l = list(&pool, ws).await;
    let c = campaign(&pool, ws, &[l], 1).await;
    let k = contact(&pool, ws, "k@ok.test", &[(l, "subscribed")]).await;
    let id = message(&pool, ws, c, k).await;
    for state in ["paused", "cancelled"] {
        sqlx::query("update campaigns set state = $2 where id = $1")
            .bind(c)
            .bind(state)
            .execute(&pool)
            .await
            .expect("set state");
        let got = eligible(&pool, &[id]).await.expect("eligible");
        assert_eq!(
            got.len(),
            1,
            "a {state} campaign's mailable recipient was dropped; it would be skipped for good"
        );
    }
}

/// The fencing token. A release gives the attempt back, so `attempts` recurs across claims: both
/// claims here carry `attempts = 1`. Whatever the first claim does after the second took the row
/// (a replayed release, a late mark, a replayed flush) must touch nothing. Without the token, the
/// replayed `Retry` moves the second claim's `submitting` row back to `queued` while it is at the
/// relay, and the next claim sends it again.
#[sqlx::test]
async fn a_replayed_release_or_flush_from_an_earlier_claim_touches_nothing(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    let first = claim_one(&pool, c).await;
    release(
        &pool,
        std::slice::from_ref(&first),
        Duration::ZERO,
        "budget",
    )
    .await
    .expect("release");
    let second = claim_one(&pool, c).await;
    assert_eq!(first.attempts, second.attempts, "the attempts value recurs");
    assert_ne!(
        first.claim_token, second.claim_token,
        "a re-claim mints a new token"
    );

    let held = snapshot(&pool, second.id).await;
    release(
        &pool,
        std::slice::from_ref(&first),
        Duration::ZERO,
        "replayed",
    )
    .await
    .expect("release");
    assert_eq!(
        snapshot(&pool, second.id).await,
        held,
        "a replayed release touched the second claim's row"
    );
    assert!(
        !mark_submitting(&pool, &first).await.expect("mark"),
        "the first claim marked the second claim's row"
    );

    assert!(mark_submitting(&pool, &second).await.expect("mark"));
    let held = snapshot(&pool, second.id).await;
    for replayed in [
        Verdict::Retry("451 replayed".into()),
        Verdict::Deferred("455 replayed".into()),
    ] {
        record(&pool, &secrets(), &[(first.clone(), replayed)])
            .await
            .expect("record");
    }
    assert_eq!(
        snapshot(&pool, second.id).await,
        held,
        "a replayed flush moved a row that is at the relay"
    );
}

/// **The replay test above only tried `Retry` and `Deferred`**, and the other verdicts do not move
/// a row to the same place. `Sent`, `Failed`, `Bounced`, `Unknown` and a `Retry` on its last
/// attempt are final and each counts; `Skipped` reads from `queued`, so it needs the second claim
/// not yet marked. A replay of any of them from the first claim must touch neither the second
/// claim's row nor the campaign's counters: a `bounced` counted twice leaves `total` short of the
/// sum, and the campaign never leaves `sending` (spec M2d §4.5.3). Red-first: drop the token from
/// `record`'s `where`, or count a bounce from the flush instead of from the rows it moved.
#[sqlx::test]
async fn a_replayed_flush_of_any_verdict_from_an_earlier_claim_touches_nothing(pool: PgPool) {
    let ws = workspace(&pool).await;
    // Two claims of one row, the first released: `attempts` recurs, the token does not.
    let two_claims = |c: Uuid| {
        let pool = pool.clone();
        async move {
            queue(&pool, ws, c, 1, "1 minute").await;
            let first = claim_one(&pool, c).await;
            release(
                &pool,
                std::slice::from_ref(&first),
                Duration::ZERO,
                "budget",
            )
            .await
            .expect("release");
            (first, claim_one(&pool, c).await)
        }
    };

    // `submitting` under the second claim: every verdict that reads from `submitting`.
    let c = campaign(&pool, ws, &[], 1).await;
    let (first, second) = two_claims(c).await;
    assert!(mark_submitting(&pool, &second).await.expect("mark"));
    let at_the_relay = snapshot(&pool, second.id).await;
    let held = counters(&pool, c).await;
    // The first claim spent its last attempt, so a `Retry` from it reads as final.
    let spent = Claimed {
        attempts: first.max_attempts,
        ..first.clone()
    };
    for (who, replayed) in [
        (
            &first,
            Verdict::Sent {
                relay: Uuid::now_v7(),
                response: "250 replayed".into(),
            },
        ),
        (&first, Verdict::Failed("550 replayed".into())),
        (&first, bounced("replayed@example.test")),
        (
            &first,
            Verdict::Unknown("broken after DATA, replayed".into()),
        ),
        (&spent, Verdict::Retry("451 replayed".into())),
    ] {
        record(&pool, &secrets(), &[(who.clone(), replayed.clone())])
            .await
            .expect("record");
        assert_eq!(
            snapshot(&pool, second.id).await,
            at_the_relay,
            "a replayed {replayed:?} moved a row that is at the relay"
        );
        assert_eq!(
            counters(&pool, c).await,
            held,
            "a replayed {replayed:?} was counted"
        );
    }

    // `Skipped` reads from `queued`: a row the second claim holds but has not marked.
    let c = campaign(&pool, ws, &[], 1).await;
    let (first, second) = two_claims(c).await;
    let queued = snapshot(&pool, second.id).await;
    let held = counters(&pool, c).await;
    record(
        &pool,
        &secrets(),
        &[(first, Verdict::Skipped("replayed".into()))],
    )
    .await
    .expect("record");
    assert_eq!(
        snapshot(&pool, second.id).await,
        queued,
        "a replayed Skipped moved a row another claim holds"
    );
    assert_eq!(
        counters(&pool, c).await,
        held,
        "a replayed Skipped was counted"
    );
}

/// The mark restarts the lease at `SUBMIT_LEASE`, counted from the mark, so a send started as its
/// claim lease runs out is not swept while the session, render and signing are still running.
#[sqlx::test]
async fn a_mark_leases_the_whole_send_from_the_mark(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    let m = claim(&pool, &[c], 10, Duration::from_secs(1))
        .await
        .expect("claim")
        .remove(0);
    assert!(mark_submitting(&pool, &m).await.expect("mark"));

    let wait = seconds_until_due(&pool, m.id).await;
    let lease = SUBMIT_LEASE.as_secs_f64();
    assert!(
        wait > lease - 10.0 && wait <= lease,
        "the mark left {wait}s of lease, not ~{lease}s"
    );
}

// ---------------------------------------------------------------------------------------------
// Spec M2d §4.5: the send-time hard bounce.

fn bounced(email: &str) -> Verdict {
    Verdict::Bounced {
        reason: "permanent error (550): 5.1.1 <recipient>: no such user".into(),
        email: email.into(),
    }
}

/// `(scope, email, email_hmac, reason, source, note)` of every suppression in `ws`.
type SuppressionRow = (String, String, Vec<u8>, String, String, Option<String>);

async fn suppressions_in(pool: &PgPool, ws: Uuid) -> Vec<SuppressionRow> {
    sqlx::query_as(
        "select scope, email, email_hmac, reason, source, note from suppressions
          where workspace_id = $1 order by id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .expect("suppressions")
}

/// Spec M2d §4.5.2: the message is `bounced`, counted as `bounced` and not `failed`, and its
/// address is suppressed in the workspace, normalised and keyed by its HMAC. A replay of the same
/// flush counts nothing and writes no second row.
#[sqlx::test]
async fn a_bounce_is_recorded_counted_and_suppressed_once(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 2).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));
    let flush = [(m.clone(), bounced("Dead@Example.test"))];

    record(&pool, &secrets(), &flush).await.expect("record");
    record(&pool, &secrets(), &flush).await.expect("a replay");

    assert_eq!(row(&pool, m.id).await.0, "bounced");
    assert_eq!(counters(&pool, c).await, ("sending".into(), 0, 0, 1, 0, 0));
    assert_eq!(
        suppressions_in(&pool, ws).await,
        [(
            "address".to_owned(),
            "dead@example.test".to_owned(),
            suppression_hmac(&secrets(), "dead@example.test").to_vec(),
            "bounced".to_owned(),
            "send".to_owned(),
            Some(format!("hard bounce 5.1.1 at send, campaign {c}")),
        )]
    );
}

/// Spec M2d §4.5.3: the finish check counts `bounced`.
#[sqlx::test]
async fn a_campaign_whose_last_message_bounced_finishes(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));

    record(&pool, &secrets(), &[(m, bounced("dead@example.test"))])
        .await
        .expect("record");

    assert_eq!(counters(&pool, c).await.0, "sent");
}

/// issues/27: an address already blocked keeps the row it has, and the flush still commits.
#[sqlx::test]
async fn an_address_already_blocked_keeps_its_row(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason, source)
         values ($1, $2, 'address', $3, $4, 'unsubscribed_all', 'one_click')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind("dead@example.test")
    .bind(suppression_hmac(&secrets(), "dead@example.test").as_slice())
    .execute(&pool)
    .await
    .expect("an opt-out");
    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));

    record(
        &pool,
        &secrets(),
        &[(m.clone(), bounced("dead@example.test"))],
    )
    .await
    .expect("the flush commits");

    assert_eq!(row(&pool, m.id).await.0, "bounced");
    let reasons: Vec<String> = suppressions_in(&pool, ws)
        .await
        .into_iter()
        .map(|r| r.3)
        .collect();
    assert_eq!(reasons, ["unsubscribed_all"]);
}

/// Invariant 5: the outcome and the suppression commit together. A flush that fails after its
/// suppression was written leaves neither the `bounced` status nor the suppression behind, and its
/// retry writes both. The failure is a trigger refusing the counters, the last write a flush makes.
#[sqlx::test]
async fn a_bounce_and_its_suppression_commit_together(pool: PgPool) {
    let ws = workspace(&pool).await;
    let c = campaign(&pool, ws, &[], 1).await;
    queue(&pool, ws, c, 1, "1 minute").await;
    let m = claim_one(&pool, c).await;
    assert!(mark_submitting(&pool, &m).await.expect("mark"));
    sqlx::raw_sql(
        "create function refuse_counters() returns trigger language plpgsql
           as $$ begin raise exception 'the counters refused'; end $$;
         create trigger refuse_counters before update on campaigns
           for each row execute function refuse_counters();",
    )
    .execute(&pool)
    .await
    .expect("the trigger");
    let flush = [(m.clone(), bounced("dead@example.test"))];

    record(&pool, &secrets(), &flush)
        .await
        .expect_err("the counters refused");
    assert_eq!(row(&pool, m.id).await.0, "submitting");
    assert!(
        suppressions_in(&pool, ws).await.is_empty(),
        "the suppression outlived its flush"
    );

    sqlx::raw_sql("drop trigger refuse_counters on campaigns")
        .execute(&pool)
        .await
        .expect("drop the trigger");
    record(&pool, &secrets(), &flush).await.expect("the retry");
    assert_eq!(row(&pool, m.id).await.0, "bounced");
    assert_eq!(suppressions_in(&pool, ws).await.len(), 1);
}
