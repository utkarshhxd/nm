// SPDX-License-Identifier: AGPL-3.0-only

//! The send pipeline's throughput, measured (spec M2c §4.10, §9): 100,000 recipients on one list,
//! launched by the real launch job and sent by the real messages loop through an in-process SMTP
//! sink, personalised (three merge tags) and DKIM-signed by this product.
//!
//! **`#[ignore]`, so it never runs in CI or in an ordinary `cargo test`.** It is a measurement, and
//! a throughput assertion on a shared runner's clock is a flake generator. Run it deliberately,
//! in release mode:
//!
//! ```text
//! bash scripts/dev.sh cargo test --release --test campaign_bench -- --ignored --nocapture
//! ```
//!
//! The 1,500 messages/s of spec §4.10 is asserted **only** when `NM_BENCH_REFERENCE=1`, which is
//! what the run on the 8 vCPU reference box sets; anywhere else the number is printed and recorded,
//! not gated. `NM_BENCH_CONTACTS` shrinks the list for a smoke run, `NM_BENCH_WORKSPACE` sets the
//! size of the large workspace in the second test (a million by default), and
//! `NM_BENCH_SYNC_COMMIT=off` runs the database with `synchronous_commit` off to show how much of
//! the figure is the disk: every write-ahead mark is a commit that waits for a WAL flush.
//!
//! **What is and is not in the number.** The runtime is multi-threaded with eight workers, as the
//! product's is; Postgres runs on the same machine, so it shares the cores with the worker and the
//! sink, which makes the figure a floor for a deployment with a database of its own. The window is
//! the messages loop only, from the first tick to the tick that claims nothing, and so includes
//! every sweep, claim, eligibility check, write-ahead, render, signature, SMTP session and flush in
//! between. Launch is timed on its own. The sink answers `250` at once, so a real relay's latency
//! is not in it: that is OCI's 300/s (§13.7), which this cannot prove either way.
//!
//! The throughput measure the project's `messages.id` as it is: `gen_random_uuid()`, a v4. PG16
//! has no `uuidv7()`; whether that scatter costs anything is what the number says.

mod common;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use common::{
    PW, TestApp, add_member, create_user, create_workspace,
    smtp_sink::{AfterData, Script, Sink},
};
use nm::{
    campaign::{
        launch::{self, LAUNCH_KIND, LAUNCH_SLICE},
        send::{CampaignWorker, States},
    },
    config::Config,
    crypto::secrets::Secrets,
    db,
    relay::transport::{JOB_LEASE, Transports},
    worker::{self, confirmation::Ctx},
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use uuid::Uuid;

const KEY_PEM: &str = include_str!("fixtures/dkim/signing-2048-pkcs8.pem");
const TARGET_PER_SECOND: f64 = 1500.0;

fn env_count(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The worker's configuration under test: pool 16, 64 sends at once, an https public URL.
fn config(url: &str) -> Config {
    let mut c = common::test_config(url);
    c.public_url = "https://mail.example.test".parse().expect("the public URL");
    c.worker_pool_size = 16;
    c.worker_campaign_concurrency = 64;
    c
}

/// A database made for this run and dropped after it, on the server `DATABASE_URL` names.
struct Scratch {
    admin: PgConnection,
    name: String,
    url: String,
}

impl Scratch {
    async fn create() -> Scratch {
        let base =
            std::env::var("DATABASE_URL").expect("DATABASE_URL, which the dev container sets");
        let mut admin = PgConnection::connect(&base).await.expect("the server");
        let name = format!("nm_bench_{}", Uuid::now_v7().simple());
        let sql = format!("create database {name}");
        sqlx::query(AssertSqlSafe(sql))
            .execute(&mut admin)
            .await
            .expect("create the benchmark database");
        // `NM_BENCH_SYNC_COMMIT=off` shows how much of the figure is the disk: every write-ahead
        // mark is a commit that waits for a WAL flush. It is a diagnostic, never a setting.
        if let Ok(setting) = std::env::var("NM_BENCH_SYNC_COMMIT") {
            let sql = format!("alter database {name} set synchronous_commit = {setting}");
            sqlx::query(AssertSqlSafe(sql))
                .execute(&mut admin)
                .await
                .expect("set synchronous_commit");
        }
        let mut url: url::Url = base.parse().expect("DATABASE_URL is a url");
        url.set_path(&name);
        Scratch {
            admin,
            name,
            url: url.to_string(),
        }
    }

    async fn drop(mut self) {
        let sql = format!("drop database {} with (force)", self.name);
        let _ = sqlx::query(AssertSqlSafe(sql))
            .execute(&mut self.admin)
            .await;
    }
}

/// A workspace that can send a campaign, with a relay pointing at the sink.
struct Fx {
    /// The worker's pool (`worker_pool_size`), which the launch and the messages loop share.
    pool: PgPool,
    app: TestApp,
    ws: Uuid,
    domain: Uuid,
    template: Uuid,
    list: Uuid,
    ctx: Ctx,
    sink: Sink,
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn fx(url: &str) -> Fx {
    let cfg = config(url);
    let api = db::connect(&cfg).await.expect("the API pool");
    db::MIGRATOR.run(&api).await.expect("migrate");
    let pool = db::connect_worker(&cfg).await.expect("the worker pool");

    let owner = create_user(&api, "owner@example.com", PW, false).await;
    let ws = create_workspace(&api, owner, "acme").await;
    let user = create_user(&api, "admin@example.com", PW, false).await;
    add_member(&api, ws, user, Role::Admin).await;
    let mut app = TestApp::with_config(api.clone(), |c| *c = config(url)).await;
    app.login("admin@example.com", PW).await;

    app.put(&format!("/api/v1/workspaces/{ws}/brand-kit"))
        .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
                      "footer": {"address": "1 Main St, Springfield"}}))
        .await
        .assert_status_ok();
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/sending-domains"))
        .json(&json!({"domain": "brand.test", "signing": "relay"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let domain = id_of(&r);
    // App-signed: this product signs every message, which is the expensive half of the send.
    let der = STANDARD
        .decode(
            KEY_PEM
                .lines()
                .filter(|l| !l.starts_with("-----"))
                .collect::<String>(),
        )
        .expect("the fixture key");
    let secrets = Arc::new(Secrets::new(&cfg.secret_key));
    sqlx::query(
        "update sending_domains set signing = 'app', dkim_private_key_encrypted = $2,
                checks = $3 where id = $1",
    )
    .bind(domain)
    .bind(secrets.encrypt(&der))
    .bind(
        json!({"spf": {"status": "pass"}, "dkim": {"status": "pass"},
                 "dmarc": {"status": "pass"}, "return_path": {"status": "pass"}}),
    )
    .execute(&api)
    .await
    .expect("the signing key");
    // Three merge tags, which is the personalisation a real campaign does.
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/templates"))
        .json(
            &json!({"name": "welcome", "kind": "blocks", "tree": {"blocks": [
            {"type": "Text", "runs": [
                {"type": "Tag", "source": "{{ first_name }}"},
                {"type": "Text", "text": " ", "marks": []},
                {"type": "Tag", "source": "{{ last_name }}"},
                {"type": "Text", "text": ", we wrote to ", "marks": []},
                {"type": "Tag", "source": "{{ email }}"}]}]}}),
        )
        .await;
    r.assert_status(StatusCode::CREATED);
    let template = id_of(&r);
    let list = new_list(&app, ws, "news").await;

    let sink = Sink::start_discarding(Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query(
        "insert into relays (id, workspace_id, name, host, port, security,
                             rate_limit_per_minute, failover_order)
         values ($1, $2, 'sink', '127.0.0.1', $3, 'starttls', 1000000, 0)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(i32::from(sink.addr.port()))
    .execute(&api)
    .await
    .expect("relay");

    let ctx = Ctx {
        db: pool.clone(),
        secrets,
        config: Arc::new(cfg),
        transports: Arc::new(Transports::plaintext_for_tests()),
    };
    Fx {
        pool,
        app,
        ws,
        domain,
        template,
        list,
        ctx,
        sink,
    }
}

async fn new_list(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/lists"))
        .json(&json!({"name": name, "double_optin": false}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

impl Fx {
    /// `n` contacts, on no list.
    async fn contacts(&self, n: i64) {
        sqlx::query(
            "insert into contacts (id, workspace_id, email, first_name, last_name)
             select gen_random_uuid(), $1, 'c' || g || '@dest.test', 'First' || g, 'Last' || g
               from generate_series(1, $2::int) g",
        )
        .bind(self.ws)
        .bind(n)
        .execute(&self.pool)
        .await
        .expect("contacts");
    }

    /// `n` contacts of the workspace, picked at random, subscribed to `list`.
    async fn subscribe(&self, list: Uuid, n: i64) {
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status)
             select $1, id, $2, 'subscribed'
               from (select id from contacts where workspace_id = $1 order by random() limit $3) s",
        )
        .bind(self.ws)
        .bind(list)
        .bind(n)
        .execute(&self.pool)
        .await
        .expect("memberships");
        sqlx::query("analyze contacts, list_memberships")
            .execute(&self.pool)
            .await
            .expect("analyze");
    }

    /// A campaign on `list`, scheduled to send now: written directly, as the tests do.
    async fn scheduled(&self, list: Uuid) -> Uuid {
        let r = self
            .app
            .post(&format!("/api/v1/workspaces/{}/campaigns", self.ws))
            .json(&json!({
                "name": "bench", "list_ids": [list], "sending_domain_id": self.domain,
                "from_local_part": "news", "from_name": "Acme", "reply_to": "hello@brand.test",
                "variants": [{"subject": "Hi {{ first_name }}", "template_id": self.template,
                              "from_name": null}],
            }))
            .await;
        r.assert_status(StatusCode::CREATED);
        let id = id_of(&r);
        let job = Uuid::now_v7();
        sqlx::query("update campaigns set state = 'scheduled', launch_job_id = $2 where id = $1")
            .bind(id)
            .bind(job)
            .execute(&self.pool)
            .await
            .expect("schedule");
        sqlx::query(
            "insert into jobs (id, workspace_id, kind, campaign_id, payload, run_after)
             values ($1, $2, 'campaign_launch', $3, '{}', now())",
        )
        .bind(job)
        .bind(self.ws)
        .bind(id)
        .execute(&self.pool)
        .await
        .expect("launch job");
        id
    }

    /// Runs the launch until it is done, as the jobs loop would, one claim at a time. Returns the
    /// seconds it took and how many claims (slices) it needed.
    async fn launch(&self) -> (f64, usize) {
        let started = Instant::now();
        let mut claims = 0;
        while let Some(job) = worker::claim(&self.pool, LAUNCH_KIND, 1, 120)
            .await
            .expect("claim")
            .pop()
        {
            launch::handle(&self.ctx, &job, LAUNCH_SLICE).await;
            claims += 1;
        }
        (started.elapsed().as_secs_f64(), claims)
    }

    async fn counts(&self, campaign: Uuid) -> (String, i64, i64) {
        sqlx::query_as("select state, total, sent from campaigns where id = $1")
            .bind(campaign)
            .fetch_one(&self.pool)
            .await
            .expect("campaign")
    }
}

/// Where this ran, for the record: the figure means nothing without it.
fn machine() -> String {
    format!(
        "{} logical CPUs visible, tokio multi-thread runtime with 8 workers",
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    )
}

/// **The §9 rate.** 100,000 contacts on one list, a `blocks` template with three merge tags, an
/// app-signed domain, a sink relay rated at a million a minute, one worker with
/// `worker_campaign_concurrency = 64` and `worker_pool_size = 16`.
///
/// Prints the launch time and the sustained rate. Asserts the rate only under
/// `NM_BENCH_REFERENCE=1`; asserts always that every message was sent exactly once, which is what
/// makes the rate a rate of something.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measurement, not a gate: see the module docs for the command"]
async fn sends_at_the_section_nine_rate_to_a_sink() {
    let contacts = env_count("NM_BENCH_CONTACTS", 100_000);
    let scratch = Scratch::create().await;
    let f = fx(&scratch.url).await;
    f.contacts(contacts).await;
    f.subscribe(f.list, contacts).await;
    let campaign = f.scheduled(f.list).await;

    let (launch_secs, claims) = f.launch().await;
    let (state, total, _) = f.counts(campaign).await;
    assert_eq!((state.as_str(), total), ("sending", contacts), "the launch");

    let states = Arc::new(States::default());
    let worker = CampaignWorker::new(
        Arc::new(Ctx {
            db: f.pool.clone(),
            secrets: Arc::clone(&f.ctx.secrets),
            config: Arc::clone(&f.ctx.config),
            transports: Arc::clone(&f.ctx.transports),
        }),
        Arc::clone(&states),
        f.ctx.config.worker_campaign_concurrency,
    );
    let started = Instant::now();
    let mut ticks = 0;
    loop {
        states.refresh(&f.pool).await.expect("refresh");
        ticks += 1;
        if worker.tick().await.expect("a tick") == 0 {
            break;
        }
        assert!(ticks < 10_000, "a send that never ends");
    }
    let send_secs = started.elapsed().as_secs_f64();

    let (state, _, sent) = f.counts(campaign).await;
    let delivered = f.sink.delivered();
    let per_second = contacts as f64 / send_secs;
    println!("machine:  {}", machine());
    println!(
        "launch:   {contacts} recipients in {launch_secs:.2} s ({claims} claim(s), slice {LAUNCH_SLICE:?})"
    );
    println!(
        "send:     {contacts} messages in {send_secs:.2} s over {ticks} ticks = {per_second:.0} messages/s"
    );
    println!("sink saw: {delivered} messages; campaign state {state}, sent {sent}");

    drop(worker);
    f.pool.close().await;
    scratch.drop().await;

    assert_eq!(
        (state.as_str(), sent, delivered),
        ("sent", contacts, usize::try_from(contacts).unwrap()),
        "every contact is sent exactly once, and the sink saw as many"
    );
    if std::env::var("NM_BENCH_REFERENCE").as_deref() == Ok("1") {
        assert!(
            per_second >= TARGET_PER_SECOND,
            "{per_second:.0} messages/s is under the {TARGET_PER_SECOND} spec §4.10 asks for"
        );
    }
}

/// **A small list in a large workspace** (R5 M2). A launch batch is driven from `contacts`, so it
/// reads the whole workspace to find a handful of members, and its one statement could outlast the
/// 120 s claim lease on a big enough workspace, letting a second worker claim the same job. This
/// builds `NM_BENCH_WORKSPACE` contacts (a million), puts a thousand of them on a second list, and
/// times the launch of a campaign on that list. It prints whether the statement outlasts the lease
/// and asserts nothing about time.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measurement, not a gate: see the module docs for the command"]
async fn a_small_list_in_a_large_workspace_launches_inside_the_lease() {
    let workspace = env_count("NM_BENCH_WORKSPACE", 1_000_000);
    let members = 1_000;
    let scratch = Scratch::create().await;
    let f = fx(&scratch.url).await;
    f.contacts(workspace).await;
    let small = new_list(&f.app, f.ws, "small").await;
    f.subscribe(small, members).await;
    let campaign = f.scheduled(small).await;

    let (secs, claims) = f.launch().await;
    let (state, total, _) = f.counts(campaign).await;
    println!("machine:  {}", machine());
    println!(
        "launch:   {members} members of a {workspace}-contact workspace in {secs:.2} s \
         ({claims} claim(s)); the claim lease is {} s: {}",
        JOB_LEASE.as_secs(),
        if Duration::from_secs_f64(secs) > JOB_LEASE {
            "OUTLASTED"
        } else {
            "not outlasted"
        }
    );
    f.pool.close().await;
    scratch.drop().await;
    assert_eq!(
        (state.as_str(), total),
        ("sending", members),
        "every member queued, none sent"
    );
}
