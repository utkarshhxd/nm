// SPDX-License-Identifier: AGPL-3.0-only

//! The worker's own connection pool, and the tick that drains the queue.
//!
//! **Invariant 7 is the first test here and the reason the file is named after a pool.**
//! `db::connect` sizes the API's at 20 and `/readyz` reads an exhausted pool as not ready, so a
//! worker drawing from that pool makes *sending* able to take the product down: sixteen messages
//! in flight is sixteen connections the API no longer has. The worker gets its own, sized by
//! `worker_pool_size`, and the test below saturates it and then asks the API for a connection.
//!
//! **Nothing here asserts an elapsed time as an equality.** The tick's budget is tested by
//! injecting a zero budget rather than by watching a clock, and the one test that waits —
//! `the_loop_drains_the_queue_and_returns_when_shutdown_resolves` — waits for a *condition* with
//! a generous ceiling, which is a liveness bound and not a timing assertion.

mod common;

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use common::smtp_sink::{AfterData, Script, Sink};
use nm::{
    config::Config,
    crypto::secrets::Secrets,
    db,
    public::token::{self, Purpose, Token},
    relay::{self, transport::Transports},
    worker::{self, BATCH, JOB_LEASE, Worker, confirmation::Ctx},
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// `https`, because `mail::build` refuses anything else as an unsubscribe URL. The boot check
/// this task adds is what makes an instance that cannot satisfy that say so before it claims a
/// row; these tests are about what happens after it does.
const PUBLIC_URL: &str = "https://mail.example.test";
const SENDING_DOMAIN: &str = "brand.test";
const ADDRESS: &str = "reader@dest.test";

/// Enough that a batch of sixteen is granted on the first take and the three that follow it on
/// the second. The exhausted-bucket test uses a relay of its own with a limit of one.
const RATE_LIMIT: i32 = 600;

// ---------------------------------------------------------------------------------------------
// Invariant 7: sending cannot take the API down.

/// The test database's URL, which is what `db::connect` and `db::connect_worker` take. A
/// `#[sqlx::test]` pool carries no URL of its own, so it is rebuilt from the one the dev
/// container sets and the temporary database's name.
fn database_url(pool: &PgPool) -> String {
    let name = pool
        .connect_options()
        .get_database()
        .expect("the test database's name")
        .to_owned();
    let mut url: url::Url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL, which the dev container sets")
        .parse()
        .expect("DATABASE_URL is a url");
    url.set_path(&name);
    url.to_string()
}

/// **The worker's pool is its own, and an exhausted one does not reach the API.** Saturating it
/// takes `worker_pool_size` connections and no more; the API's twenty are untouched, so
/// `/readyz` — which reads an exhausted pool as not ready — still answers `200`.
///
/// Give the worker `db::connect` instead and the first assertion goes: five held connections do
/// not saturate a pool of twenty, and nothing here would be able to tell the two pools apart.
#[sqlx::test]
async fn the_worker_pool_is_separate_from_the_api_pool(pool: PgPool) {
    let mut cfg = common::test_config(&database_url(&pool));
    cfg.worker_pool_size = 3;

    let api = db::connect(&cfg).await.expect("the API pool");
    let sending = db::connect_worker(&cfg).await.expect("the worker pool");

    // Held for the rest of the test: dropping one returns it to the pool.
    let mut held = Vec::new();
    for i in 0..cfg.worker_pool_size {
        held.push(
            sending.acquire().await.unwrap_or_else(|e| {
                panic!("worker connection {i} of {}: {e}", cfg.worker_pool_size)
            }),
        );
    }
    assert!(
        sending.try_acquire().is_none(),
        "the worker pool is not bounded by worker_pool_size: {} held connections left room for \
         another, so it is not a pool of its own",
        held.len()
    );

    assert!(
        api.try_acquire().is_some(),
        "the API pool had no connection left while the worker's was saturated"
    );
    common::TestApp::with_config(api.clone(), |_| {})
        .await
        .server
        .get("/readyz")
        .await
        .assert_status_ok();

    // These pools are this test's own, not `#[sqlx::test]`'s, so nothing else closes them and the
    // temporary database cannot be dropped while they hold connections open.
    drop(held);
    sending.close().await;
    api.close().await;
}

// ---------------------------------------------------------------------------------------------
// The tick.

/// A workspace that can send: one list, one contact, one relay pointing at the sink, and one
/// sending domain whose DNS checks all pass and whose signing is the relay's.
struct World {
    pool: PgPool,
    sink: Sink,
    ws: Uuid,
    list: Uuid,
    contact: Uuid,
    relay: Uuid,
    secrets: Arc<Secrets>,
    config: Arc<Config>,
}

impl World {
    async fn new(pool: PgPool, script: Script) -> World {
        let sink = Sink::start(script).await;
        let (ws, list, contact, relay) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        sqlx::query("insert into workspaces (id, slug, name) values ($1, 'acme', 'Acme')")
            .bind(ws)
            .execute(&pool)
            .await
            .expect("workspace");
        sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, 'News')")
            .bind(list)
            .bind(ws)
            .execute(&pool)
            .await
            .expect("list");
        sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
            .bind(contact)
            .bind(ws)
            .bind(ADDRESS)
            .execute(&pool)
            .await
            .expect("contact");
        sqlx::query(
            "insert into sending_domains (id, workspace_id, domain, signing, dkim_selector, checks)
             values ($1, $2, $3, 'relay', 'nm', $4)",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(SENDING_DOMAIN)
        .bind(json!({
            "spf": {"status": "pass"},
            "dkim": {"status": "pass"},
            "dmarc": {"status": "pass"},
            "return_path": {"status": "pass"},
        }))
        .execute(&pool)
        .await
        .expect("sending domain");

        let mut config = common::test_config("postgres://unused");
        config.public_url = PUBLIC_URL.parse().expect("the public URL");
        let secrets = Arc::new(Secrets::new(&config.secret_key));
        let world = World {
            pool,
            sink,
            ws,
            list,
            contact,
            relay,
            secrets,
            config: Arc::new(config),
        };
        world
            .add_relay(relay, "primary", 0, world.sink.addr.port(), RATE_LIMIT)
            .await;
        world
    }

    /// Another relay for this workspace. `port` is the sink's for a relay that can deliver and
    /// anything else for one that cannot; no test here needs a second one to answer.
    async fn add_relay(&self, id: Uuid, name: &str, order: i32, port: u16, limit: i32) {
        sqlx::query(
            "insert into relays (id, workspace_id, name, host, port, security,
                                 rate_limit_per_minute, failover_order)
             values ($1, $2, $3, $4, $5, 'starttls', $6, $7)",
        )
        .bind(id)
        .bind(self.ws)
        .bind(name)
        .bind(self.sink.addr.ip().to_string())
        .bind(i32::from(port))
        .bind(limit)
        .bind(order)
        .execute(&self.pool)
        .await
        .expect("relay");
    }

    /// A worker on this world, talking to the sink in the clear.
    fn worker(&self, concurrency: usize) -> Worker {
        Worker::new(
            Ctx {
                db: self.pool.clone(),
                secrets: Arc::clone(&self.secrets),
                config: Arc::clone(&self.config),
                transports: Arc::new(Transports::plaintext_for_tests()),
            },
            concurrency,
        )
    }

    /// `n` fresh, well-formed `confirmation_email` rows, as a signup form would queue them.
    async fn enqueue(&self, n: usize) {
        for _ in 0..n {
            let confirm = token::issue(
                &self.secrets,
                &Token {
                    workspace_id: self.ws,
                    contact_id: self.contact,
                    purpose: Purpose::Confirm,
                    list_id: Some(self.list),
                    issued_at: Utc::now().timestamp(),
                    nonce: Uuid::now_v7().to_string(),
                    kid: None,
                    ref_id: None,
                },
            );
            sqlx::query(
                "insert into jobs (id, workspace_id, kind, contact_id, list_id, payload)
                 values ($1, $2, 'confirmation_email', $3, $4, $5)",
            )
            .bind(Uuid::now_v7())
            .bind(self.ws)
            .bind(self.contact)
            .bind(self.list)
            .bind(json!({ "email": ADDRESS, "token": confirm }))
            .execute(&self.pool)
            .await
            .expect("job");
        }
    }

    async fn rows(&self) -> Vec<JobRow> {
        sqlx::query_as::<_, JobRow>(
            "select failed_at, completed_at, last_error, run_after from jobs order by id",
        )
        .fetch_all(&self.pool)
        .await
        .expect("the job rows")
    }

    async fn completed(&self) -> usize {
        self.rows()
            .await
            .iter()
            .filter(|r| r.completed_at.is_some())
            .count()
    }

    /// The bucket balance of `relay`, or `None` where no bucket row exists.
    async fn balance(&self, relay: Uuid) -> Option<i32> {
        sqlx::query_scalar("select tokens from relay_buckets where relay_id = $1")
            .bind(relay)
            .fetch_optional(&self.pool)
            .await
            .expect("read the balance")
    }
}

#[derive(sqlx::FromRow)]
struct JobRow {
    failed_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    run_after: DateTime<Utc>,
}

/// One tick takes `BATCH` rows and no more, and the next takes what is left. Deterministic: the
/// two ticks are driven directly, so nothing here depends on how long either one took.
#[sqlx::test]
async fn a_tick_sends_a_batch_and_the_next_takes_the_rest(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let queued = BATCH as usize + 3;
    w.enqueue(queued).await;

    assert_eq!(
        w.worker(4).tick().await.expect("the first tick"),
        BATCH as usize
    );
    assert_eq!(
        w.completed().await,
        BATCH as usize,
        "the first tick must complete exactly the batch it claimed"
    );

    assert_eq!(w.worker(4).tick().await.expect("the second tick"), 3);
    assert_eq!(w.completed().await, queued, "the queue must be drained");
    assert_eq!(
        w.sink.messages().len(),
        queued,
        "every completed row must be a message the relay actually read"
    );
}

/// **The budget is half the lease, and it is what stops a tick outliving its own claim.** Four
/// waves of a sixteen-row batch against a relay burning the full 60-second session timeout is 240
/// seconds, twice `JOB_LEASE`; a tick that runs that long has its rows re-claimed and its
/// messages sent twice.
///
/// A zero budget is the same rule at the other end of the range, and it needs no clock: every
/// claimed row is one the tick did not start. They must come back **deferred**, because a job
/// that was never attempted is always safe to retry.
#[sqlx::test]
async fn a_tick_that_is_out_of_budget_starts_nothing_and_defers_what_it_claimed(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    w.enqueue(2).await;

    let claimed = w
        .worker(4)
        .with_budget(Duration::ZERO)
        .tick()
        .await
        .expect("the tick");

    assert_eq!(
        claimed, 2,
        "the rows were claimed before the budget was read"
    );
    assert_eq!(
        w.sink.connections(),
        0,
        "a tick with no budget left dialled the relay"
    );
    for row in w.rows().await {
        assert!(row.completed_at.is_none(), "nothing was sent");
        assert!(
            row.failed_at.is_none(),
            "a job that was never attempted must stay retryable, not fail"
        );
        assert!(
            row.run_after > Utc::now(),
            "the row was not deferred: run_after is {}",
            row.run_after
        );
        let reason = row.last_error.expect("a reason an operator can read");
        assert!(
            reason.contains("budget"),
            "the reason does not name the budget: {reason}"
        );
    }
}

/// The other end of `with_budget`: what the product builds for itself is half the lease.
#[sqlx::test]
async fn a_worker_budgets_its_tick_at_half_the_lease(pool: PgPool) {
    let w = World::new(pool, Script::Silence).await;
    assert_eq!(
        w.worker(4).budget(),
        JOB_LEASE / 2,
        "a tick may not be allowed to outlive the lease it claimed under"
    );
}

/// **A throttle is not a failure.** An exhausted bucket means this relay may not be driven any
/// harder *this minute*, so the rows come back with a future `run_after` and no `failed_at`, and
/// nothing is dialled: the sink counts a connection the moment it accepts one, so a tick that
/// connected and then thought better of it is still caught.
#[sqlx::test]
async fn an_exhausted_bucket_sends_nothing_and_leaves_the_rows_claimable(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    // One token a minute, and none in the bucket: the refill is `floor(elapsed * 1 / 60)`, which
    // is zero for the next sixty seconds however slow this machine is.
    sqlx::query("update relays set rate_limit_per_minute = 1 where id = $1")
        .bind(w.relay)
        .execute(&w.pool)
        .await
        .expect("the limit");
    sqlx::query(
        "insert into relay_buckets (relay_id, workspace_id, tokens, refilled_at)
         values ($1, $2, 0, now())",
    )
    .bind(w.relay)
    .bind(w.ws)
    .execute(&w.pool)
    .await
    .expect("an empty bucket");
    w.enqueue(2).await;

    w.worker(4).tick().await.expect("the tick");

    assert_eq!(
        w.sink.connections(),
        0,
        "a throttled tick dialled the relay anyway"
    );
    for row in w.rows().await {
        assert!(row.completed_at.is_none());
        assert!(
            row.failed_at.is_none(),
            "a throttle destroyed a message: it must be a deferral"
        );
        assert!(row.run_after > Utc::now(), "the row was not deferred");
    }
    assert_eq!(
        w.balance(w.relay).await,
        Some(0),
        "a refusal spends nothing"
    );
}

/// **A relay slower than one batch a minute must still send, and asking for the whole batch is
/// how it would never send at all.** `take` is all or nothing: a batch of sixteen asked of a
/// relay that allows ten a minute is refused by the insert arm's `limit >= wanted`, every tick,
/// for ever — the rows are deferred until `max_attempts` runs out and the workspace's mail is
/// simply destroyed, slowly, with every part of the system reporting that it is behaving.
///
/// A relay rated at 100 an hour is an ordinary cheap plan, so this is not a corner. The tick asks
/// for what the relay could grant at most, sends that many, and defers the rest.
#[sqlx::test]
async fn a_relay_slower_than_a_batch_sends_what_it_can_rather_than_nothing(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query("update relays set rate_limit_per_minute = 2 where id = $1")
        .bind(w.relay)
        .execute(&w.pool)
        .await
        .expect("a relay that allows two a minute");
    w.enqueue(5).await;

    w.worker(4).tick().await.expect("the tick");

    assert_eq!(
        w.sink.messages().len(),
        2,
        "a relay allowing two a minute sent {} of a batch of five",
        w.sink.messages().len()
    );
    let rows = w.rows().await;
    assert_eq!(
        rows.iter().filter(|r| r.completed_at.is_some()).count(),
        2,
        "the two the bucket granted must be the two that completed"
    );
    for row in rows.iter().filter(|r| r.completed_at.is_none()) {
        assert!(row.failed_at.is_none(), "a rate limit destroyed a message");
        assert!(row.run_after > Utc::now(), "the row was not deferred");
    }
    assert_eq!(w.balance(w.relay).await, Some(0), "both tokens were spent");
}

/// **An operator deleting a relay mid-tick is a real path, and it is not this batch's fault.**
/// The relays are read, the operator deletes one, and the debit then raises `23503` against a
/// parent row that is gone. The tick reads that as *this relay is gone*, falls to the next in
/// failover order, and the claimed jobs are not failed over it.
///
/// Take the `23503` arm out and `debit` surfaces a database error, which the tick can only read
/// as "the bucket could not be debited" — the whole batch deferred over a relay the workspace
/// was no longer using.
#[sqlx::test]
async fn a_relay_deleted_mid_tick_falls_through_to_the_next(pool: PgPool) {
    let w = World::new(pool, Script::Silence).await;
    let standby = Uuid::now_v7();
    w.add_relay(standby, "standby", 1, w.sink.addr.port(), RATE_LIMIT)
        .await;
    let relays = relay::enabled_relays(&w.pool, w.ws).await.expect("relays");
    assert_eq!(
        relays.len(),
        2,
        "failover order: the primary, then the standby"
    );

    // Between the read and the debit, which is the window this is about.
    sqlx::query("delete from relays where id = $1")
        .bind(w.relay)
        .execute(&w.pool)
        .await
        .expect("the operator's delete");

    let granted = worker::debit(&w.pool, w.ws, &relays, 4)
        .await
        .expect("a deleted relay is not an error the batch pays for");

    assert_eq!(
        granted, 4,
        "the standby relay had the tokens for this batch"
    );
    assert_eq!(
        w.balance(standby).await,
        Some(RATE_LIMIT - 4),
        "the standby's bucket was not debited"
    );
    assert_eq!(
        w.balance(w.relay).await,
        None,
        "a bucket was written for a relay that does not exist"
    );
}

/// The loop: it keeps ticking until the shutdown future resolves, and then it **returns** rather
/// than being cancelled mid-send.
///
/// The wait is for a condition — every row completed — with a ceiling far above any plausible
/// runtime, so it is a liveness bound and not a timing assertion. The ceiling failing is a loop
/// that stopped polling, which is the defect this is for.
#[sqlx::test]
async fn the_loop_drains_the_queue_and_returns_when_shutdown_resolves(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let queued = BATCH as usize + 3;
    w.enqueue(queued).await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();

    let worker = w.worker(4);
    let running = worker.run_until(async {
        let _ = stopped.await;
    });
    let driving = async {
        loop {
            if w.completed().await == queued {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.send(()).expect("the loop is still running");
    };
    // A hang is the failure this bounds: without it a loop that never returns hangs the suite.
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(running, driving)
    })
    .await
    .expect("the loop drained the queue and returned within a minute");

    assert_eq!(w.completed().await, queued);
    assert_eq!(w.sink.messages().len(), queued);
}
