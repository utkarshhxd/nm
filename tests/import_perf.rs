// SPDX-License-Identifier: AGPL-3.0-only

//! The budget, measured: `spec.md:459` wants a 1M-row CSV imported in under two minutes.
//!
//! **`#[ignore]`, so it never runs in CI or in an ordinary `cargo test`.** It writes a ~100 MB
//! file and some four million rows; that is a measurement, not a gate, and a gate on a shared
//! runner's wall clock is a flake generator. Run it deliberately:
//!
//! ```text
//! bash scripts/dev.sh cargo test --test import_perf -- --ignored --nocapture
//! ```
//!
//! **What this measures is the two pipeline stages, not `POST /imports/{id}/start`.** The handler
//! also does the multipart upload, the authorisation, the advisory lock and the `COMMIT`, and a
//! single wall-clock figure over all of that cannot say which of the three phases below moved.
//! The stages are where the budget is spent and where an optimisation would land; the upload is a
//! network, and it is charged to the client's link rather than to this. The `COMMIT` is timed
//! separately here, because it is the only part of the handler's extra work that is on the
//! database and is what makes the import durable.
//!
//! **The third phase cannot be timed from outside, and this file does not pretend otherwise.**
//! `write_memberships_and_tags` is private and runs inside `upsert_from_staging`, so `upsert`
//! below is the whole statement set: the `fail on duplicate` pre-flight (not in this run), the
//! `insert … select` into `contacts`, the memberships-and-tags statement, and the erasure sweep.
//! To split it, the run turns on `log_min_duration_statement`, which makes Postgres time each of
//! those statements server-side and write the figure to its own log — see `m1c-decisions.md` for
//! the split that produced.
//!
//! **The third number is therefore read out of the container's log, not printed here.** That is
//! the one figure this file cannot print, so here is exactly how to get it, which is how every
//! memberships figure in `m1c-decisions.md` was taken:
//!
//! ```text
//! docker logs --since 20m nm-postgres-1 2>&1 | grep 'LOG:  duration' | grep -vE 'parse |bind |plan:'
//! ```
//!
//! Each run prints six lines in order: `create temporary table`, the `copy`, the error-budget
//! `select count(*) … import_errors`, the `insert … select` into `contacts`, the memberships and
//! tags statement, and the erasure sweep, then `COMMIT`. **Read the log before the next run**:
//! docker rotates it, and a run that logs plans as well as durations can rotate its own figures
//! away inside a minute.

mod common;

use std::{
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::{PW, create_user, create_workspace};
use nm::{
    config::SecretKey,
    contacts::import::{
        ColumnTarget, ContactField, Dedup, ImportJob,
        staging::{copy_into_staging, upsert_from_staging},
        upload_path,
    },
    crypto::secrets::Secrets,
};
use sqlx::{PgPool, postgres::PgConnection};
use uuid::Uuid;

/// The budget `spec.md:459` sets for the whole import.
const BUDGET: Duration = Duration::from_secs(120);

/// Rows in the generated file. Overridable so that a change to this file can be proved to run
/// against a few thousand rows in seconds; the number that gets recorded is always the default.
fn rows() -> usize {
    std::env::var("NM_PERF_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000)
}

/// Distinct tag names in the file. A fat file does not have a million distinct tags — it has a
/// handful applied to everyone — and a million would measure `tags` inserts rather than an import.
const TAG_POOL: usize = 50;

/// The generated file, removed when it goes out of scope however this test leaves. A gigabyte of
/// generated contacts abandoned in the system temp directory is the exact failure this milestone
/// has been fixing elsewhere.
struct TempCsv(PathBuf);

impl Drop for TempCsv {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A file of the shape an operator actually uploads: an address, two names, a timezone, two tags
/// out of a small pool, and the list they are being put on. Written with the blocking writer on
/// purpose — generating the fixture is not the thing being measured, and it is timed separately
/// so it can never be mistaken for part of the budget.
fn generate(path: &Path, rows: usize, list_name: &str) -> u64 {
    let file = std::fs::File::create(path).expect("create the csv");
    let mut w = BufWriter::with_capacity(1 << 20, file);
    writeln!(w, "email,first_name,last_name,timezone,tags,lists").unwrap();
    for i in 0..rows {
        writeln!(
            w,
            "user{i}@example.com,First{i},Last{i},UTC,cohort-{};vip,{list_name}",
            i % TAG_POOL
        )
        .unwrap();
    }
    w.flush().unwrap();
    std::fs::metadata(path).expect("stat the csv").len()
}

fn secs(d: Duration) -> String {
    format!("{:>8.3} s", d.as_secs_f64())
}

/// What the figures below were taken on. A number without its machine cannot be compared to the
/// next one, so the run prints its own.
async fn machine(conn: &mut PgConnection) -> String {
    let version: String = sqlx::query_scalar("select version()")
        .fetch_one(&mut *conn)
        .await
        .expect("server version");
    let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let total = mem
        .lines()
        .find(|l| l.starts_with("MemTotal:"))
        .unwrap_or("MemTotal: unknown");
    let cores = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    let shared_buffers: String =
        sqlx::query_scalar("select setting || ' * ' || unit from pg_settings where name = $1")
            .bind("shared_buffers")
            .fetch_one(&mut *conn)
            .await
            .unwrap_or_default();
    format!(
        "cores visible to the container: {cores}\n  {}\n  {version}\n  shared_buffers: \
         {shared_buffers}",
        total.trim()
    )
}

#[sqlx::test]
#[ignore = "writes a ~100 MB file and some four million rows; run it deliberately, not in CI"]
async fn a_million_rows_against_the_two_minute_budget(pool: PgPool) {
    let rows = rows();
    let ws = {
        let owner = create_user(&pool, "owner@example.com", PW, false).await;
        create_workspace(&pool, owner, "perf").await
    };
    let import = Uuid::now_v7();
    sqlx::query(
        "insert into imports (id, workspace_id, status, filename, delimiter, has_header) \
         values ($1, $2, 'running', 'perf.csv', 44, true)",
    )
    .bind(import)
    .bind(ws)
    .execute(&pool)
    .await
    .expect("insert import");
    let list = Uuid::now_v7();
    sqlx::query(
        "insert into lists (id, workspace_id, name, double_optin) values ($1, $2, $3, false)",
    )
    .bind(list)
    .bind(ws)
    .bind("News")
    .execute(&pool)
    .await
    .expect("insert list");

    // The path production would have put the upload at, so the file reaches `COPY` the way the
    // handler's does: off the filesystem, through `tokio::fs::File`.
    let csv = TempCsv(upload_path(import));
    let t = Instant::now();
    let bytes = generate(&csv.0, rows, "News");
    let generated = t.elapsed();

    let job = ImportJob {
        id: import,
        workspace_id: ws,
        path: csv.0.clone(),
        delimiter: b',',
        has_header: true,
        mapping: vec![
            ColumnTarget::Field(ContactField::Email),
            ColumnTarget::Field(ContactField::FirstName),
            ColumnTarget::Field(ContactField::LastName),
            ColumnTarget::Field(ContactField::Timezone),
            ColumnTarget::Field(ContactField::Tags),
            ColumnTarget::Field(ContactField::Lists),
        ],
        dedup: Dedup::UpdateExisting,
        list_id: Some(list),
        defs: vec![],
    };

    let mut tx = pool.begin().await.expect("begin");
    let machine = machine(&mut tx).await;
    // Makes Postgres time every statement of the upsert server-side and log it, which is the only
    // way the memberships statement can be told from the contacts one from out here.
    sqlx::query("set log_min_duration_statement = 0")
        .execute(&mut *tx)
        .await
        .expect("turn statement timing on");
    // OFF for a timing run, because `log_analyze` instruments every node and the figures above
    // would then be the instrumented ones. On, it logs the real plan of each statement — which is
    // how "does the upsert's plan want an index on the staging table?" gets answered from the plan
    // rather than from an opinion, and without a second copy of the SQL to go stale.
    if std::env::var("NM_PERF_EXPLAIN").is_ok() {
        for stmt in [
            "load 'auto_explain'",
            "set auto_explain.log_min_duration = 0",
            "set auto_explain.log_analyze = true",
            "set auto_explain.log_buffers = true",
            // THE ONE THAT MEASURES THE REFERENTIAL INTEGRITY TRIGGERS, per constraint, in their
            // own `Trigger for constraint …: time=… calls=…` lines. Without it their cost can only
            // be inferred by subtracting plan nodes from the statement — and that subtraction is
            // not sound, because a ModifyTable node's `actual time` already includes its children.
            "set auto_explain.log_triggers = true",
            // NOT `log_nested_statements`, which shows the same RI checks as some 600,000 extra
            // plans per run and rotates the four plans that were wanted straight out of the
            // container's log. `log_triggers` is the same answer at no cost.
        ] {
            sqlx::query(stmt)
                .execute(&mut *tx)
                .await
                .expect("turn plan logging on");
        }
    }

    let file = tokio::fs::File::open(&csv.0).await.expect("open the csv");
    let t = Instant::now();
    let staged = copy_into_staging(&mut tx, &Secrets::new(&SecretKey([7u8; 32])), &job, file)
        .await
        .expect("the COPY");
    let copy = t.elapsed();

    let t = Instant::now();
    let counts = upsert_from_staging(&mut tx, &job, staged.failed)
        .await
        .expect("the upsert");
    let upsert = t.elapsed();

    let t = Instant::now();
    tx.commit().await.expect("commit");
    let commit = t.elapsed();

    let pipeline = copy + upsert + commit;
    let verdict = match pipeline.checked_sub(BUDGET) {
        Some(over) => format!("MISSED by {}", secs(over)),
        None => format!("MET, with {} to spare", secs(BUDGET - pipeline)),
    };
    println!(
        "\n--- import of {rows} rows ({:.1} MiB) ---\n  \
         {machine}\n\n  \
         generate csv  {}  (not part of the budget: the fixture, not the import)\n  \
         COPY          {}  (parse, normalise, hash, render, stream into staging)\n  \
         upsert        {}  (contacts, THEN memberships and tags, THEN the erasure sweep)\n  \
         commit        {}\n  \
         ---\n  \
         pipeline      {}  against a {} budget: {verdict}\n  \
         counts        {counts:?}\n",
        bytes as f64 / (1024.0 * 1024.0),
        secs(generated),
        secs(copy),
        secs(upsert),
        secs(commit),
        secs(pipeline),
        secs(BUDGET),
    );

    // NOT an assertion on the budget — the number is the deliverable, and a wall clock is not a
    // pass/fail. This asserts the run did the work, because a timing taken off a pipeline that
    // imported nothing is worse than no timing at all.
    let rows = rows as i64;
    assert_eq!(staged.total_rows, rows, "every row reached staging");
    assert_eq!(counts.imported, rows, "every row became a contact");
    assert_eq!(counts.failed, 0, "nothing in the generated file may fail");
    let memberships: i64 =
        sqlx::query_scalar("select count(*) from list_memberships where workspace_id = $1")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .expect("count memberships");
    let tags: i64 = sqlx::query_scalar("select count(*) from contact_tags where workspace_id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .expect("count contact_tags");
    assert_eq!(memberships, rows, "every contact went on the list");
    assert_eq!(tags, rows * 2, "every contact got both of its tags");
}
