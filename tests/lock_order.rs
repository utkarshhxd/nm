// SPDX-License-Identifier: AGPL-3.0-only

//! The one lock order, `docs/adr/0005-one-lock-order-for-every-path.md`, held against real
//! handlers.
//!
//! **Every test here is a deadlock test, and that is deliberate.** M1d shipped four ABBA cycles in
//! three review rounds, and the full suite — green through all of them — could not see one,
//! because a deadlock needs two transactions interleaved at a point no single request passes
//! through twice. So each test wedges a third transaction open at exactly that point, drives both
//! real routes concurrently, and asserts that neither fails. Revert the ordering fix and Postgres
//! aborts one side with `40P01`, which reaches the route as a `500`.
//!
//! A possession probe is not enough and this file exists because of that: asserting that a call
//! holds a row when it returns says nothing about whether it took that row before or after the one
//! it must not take first. Only the cycle can tell those apart.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use chrono::Utc;
use common::{PW, TestApp, create_user, create_workspace};
use nm::public::token::{Purpose, Token, issue};
use sqlx::PgPool;
use uuid::Uuid;

/// The body RFC 8058 §3.1 tells a mail client to POST.
const ONE_CLICK: &str = "List-Unsubscribe=One-Click";

/// **At most two of these run at once, and that is not tidiness.** Each test holds four
/// connections at its critical moment — the wedge, the probe, and two routes blocked mid-request.
/// A route that cannot get a connection does not block, it fails, and the test then reports that
/// the interleaving never formed. Which is the right answer, loudly: before `settle` existed those
/// runs went green instead.
///
/// **Take it as the FIRST line of every test, before anything else happens.** Seven tests took it
/// after their fixture and three never took it at all, so ten of them did their setup unthrottled
/// against the two that held it. Where the connections they compete for actually come from is
/// `own_pool` below, which is the other half of this.
static GATE: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(2));

/// A pool on this test's own database, **outside the budget `#[sqlx::test]` draws on**. Every test
/// here takes one and uses it in place of the pool it was handed.
///
/// The gate above cannot fit these tests into that budget, and this is the arithmetic that says
/// so. `#[sqlx::test]` hands each test a pool of 5 whose permits are *stolen* from one
/// process-wide master pool of **20**, and `DecrementSizeGuard` returns a stolen permit to the
/// **child's** semaphore, never the master's: the master gets them back only when the child pool
/// is dropped, at the end of the test. Creating that pool opens one connection eagerly, so on a
/// sixteen-core box the sixteen tests libtest has in flight have spent sixteen of the twenty
/// before any of them reaches a wedge — running or parked on the gate, it makes no difference —
/// and the four left over do not cover two tests needing four apiece.
///
/// A route that cannot get a connection waits out sqlx's 30-second `acquire_timeout`, answers
/// `500`, and so never blocks; `settle` then reports, truthfully, an interleaving that never
/// formed. That was the whole of this file's intermittent red, and it is why raising Postgres'
/// `max_connections` and raising `settle`'s budget both changed nothing — neither was the resource
/// running out. (`--test-threads=2` also makes it pass, for the same reason: fewer live tests,
/// fewer permits spent. This way the file does not depend on how it is invoked.)
///
/// Drawn on its own the ceiling is the server's `max_connections`, which `docker-compose.dev.yml`
/// already sizes for this file. Six is the four a test holds at its critical moment — wedge,
/// probe, two routes blocked mid-request — plus slack for the sign-in some of them do between the
/// two `settle`s.
async fn own_pool(pool: &PgPool) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("a pool on this test's own database")
}

/// How many sessions in this test's database are waiting on a lock right now.
///
/// It runs on a connection of its own, taken before anything is spawned. Asking the pool each time
/// does not work: by the time two routes and a wedge are in flight the pool is empty, and the
/// probe fails to acquire rather than returning an answer.
async fn blocked(probe: &mut sqlx::PgConnection) -> i64 {
    sqlx::query_scalar(
        "select count(*) from pg_stat_activity           where datname = current_database() and wait_event_type = 'Lock'",
    )
    .fetch_one(&mut *probe)
    .await
    .expect("read pg_stat_activity")
}

/// Waits until `n` sessions are blocked on a lock, and **fails if they never are**.
///
/// This replaces a fixed sleep, and the direction of its failure is the whole point. A sleep that
/// is too short fails OPEN: the second actor wins outright, the first finds nothing left to do,
/// and a test that asserts only an HTTP status goes green having interleaved nothing. That is not
/// hypothetical — three of these tests passed in a full run and failed every time in isolation,
/// because fourteen concurrent `sqlx::test` databases made 400 ms too short. Waiting on the
/// condition instead is both deterministic and loud: if the interleaving does not form, this
/// panics saying so.
/// **The budget is 30 seconds and it has been raised twice; raise it again rather than deleting a
/// test.** It was 400 ms, which was too short once fourteen concurrent `sqlx::test` databases
/// existed; then 6 s, which was enough while the machine was CPU-capped to two cores. Uncapped on
/// sixteen, `cargo test` runs every integration binary at once and the whole box saturates, so the
/// blocked session takes longer to appear than it ever did on a slower machine — a result that
/// reads backwards until you see why.
///
/// This costs nothing when it works: the loop returns the moment the condition holds, so the
/// budget is only ever spent on a run that was going to fail anyway. The failure it exists to
/// prevent is the opposite trade, and far worse — a budget too small fails OPEN in effect, because
/// the honest panic it produces is indistinguishable from flakiness and gets re-run until green.
async fn settle(probe: &mut sqlx::PgConnection, n: i64) {
    for _ in 0..1200 {
        if blocked(probe).await >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "only {} of {n} sessions ever blocked: the interleaving never formed, so this test          would have proved nothing",
        blocked(probe).await
    );
}

struct Fx {
    app: TestApp,
    admin: TestApp,
    workspace: Uuid,
    contact: Uuid,
}

/// A workspace with one contact subscribed to every list in `lists`, an anonymous client for the
/// public routes and a signed-in owner for the authenticated ones.
async fn fixture(pool: &PgPool, lists: &[&str]) -> (Fx, Vec<Uuid>) {
    let owner_email = "owner@example.com";
    let owner = create_user(pool, owner_email, PW, false).await;
    let workspace = create_workspace(pool, owner, "acme").await;
    let contact = Uuid::now_v7();
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
        .bind(contact)
        .bind(workspace)
        .bind("ada@example.com")
        .execute(pool)
        .await
        .expect("contact");
    let mut ids = vec![];
    for name in lists {
        let id = Uuid::now_v7();
        sqlx::query(
            "insert into lists (id, workspace_id, name, double_optin) values ($1, $2, $3, false)",
        )
        .bind(id)
        .bind(workspace)
        .bind(name)
        .execute(pool)
        .await
        .expect("list");
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status, \
             subscribed_at) values ($1, $2, $3, 'subscribed', now())",
        )
        .bind(workspace)
        .bind(contact)
        .bind(id)
        .execute(pool)
        .await
        .expect("membership");
        ids.push(id);
    }
    let mut admin = TestApp::new(pool.clone()).await;
    admin.login(owner_email, PW).await;
    (
        Fx {
            app: TestApp::new(pool.clone()).await,
            admin,
            workspace,
            contact,
        },
        ids,
    )
}

impl Fx {
    /// An unsubscribe link for the fixture's contact, covering the whole workspace.
    fn unsub(&self) -> String {
        issue(
            &self.app.state.secrets,
            &Token {
                workspace_id: self.workspace,
                contact_id: self.contact,
                purpose: Purpose::Unsubscribe,
                list_id: None,
                issued_at: Utc::now().timestamp(),
                nonce: "n1".to_owned(),
                kid: None,
                ref_id: None,
            },
        )
    }

    fn delete_list_path(&self, list: Uuid) -> String {
        format!("/api/v1/workspaces/{}/lists/{list}", self.workspace)
    }

    fn erase_path(&self) -> String {
        format!(
            "/api/v1/workspaces/{}/contacts/{}/erase",
            self.workspace, self.contact
        )
    }
}

/// **`one_click` against `delete_list`.** Each `consent_log` row the sweep produces takes
/// `for key share` on its list through `consent_log`'s foreign key, so before the sweep took its
/// lists itself this route ran `list_memberships` → `lists` while a deletion runs `lists` →
/// `list_memberships`. Reproduced on Postgres 16.15 with the victim on the RFC 8058 route, which
/// `apply` is written so that it may not fail.
///
/// **The wedge is the `contacts` row**, and it is the same wedge all three tests use. Every
/// `consent_log` insert takes `for key share` on its contact through
/// `consent_log_workspace_id_contact_id_fkey`, so holding that row stops the route **after** its
/// membership sweep and **before** the consent rows the sweep produced — which is the one point a
/// single request passes through where it holds `list_memberships` and has not yet asked for
/// `lists`. Nothing here depends on which order the sweep returned its lists in.
#[sqlx::test]
async fn a_one_click_and_a_list_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let link = fx.unsub();
    let one_click = tokio::spawn({
        let app = TestApp::new(pool.clone()).await;
        async move { app.post(&format!("/u/{link}")).text(ONE_CLICK).await }
    });
    settle(&mut probe, 1).await;

    let path = fx.delete_list_path(first);
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&path).await }
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (clicked, deleted) = (
        one_click.await.expect("the one-click"),
        deleting.await.expect("the deletion"),
    );
    clicked.assert_status_ok();
    deleted.assert_status(StatusCode::NO_CONTENT);
}

/// **`erase_contact` against `delete_list`.** The erasure writes its tombstone before it deletes
/// the contact — that order is what makes a concurrent create of the address impossible — and
/// since `0027` that delete of the address's rows also takes its `list`-scope refusals. So the
/// erasure runs `suppressions` → `list_memberships`, while a deletion cascaded into
/// `list_memberships` first and reached `suppressions` last, because `0027`'s foreign key is the
/// newest on `lists`. Reproduced on Postgres 16.15; victim either a GDPR erasure or an operator's
/// list deletion.
///
/// The precondition is ordinary: the erased contact holds a per-list refusal on the list being
/// deleted, which every preference-centre untick creates. The wedge is the `contacts` row, held so
/// the erasure stops between its tombstone and its cascade.
#[sqlx::test]
async fn erasing_a_contact_and_deleting_a_list_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let mut conn = pool.acquire().await.unwrap();
    nm::suppressions::unsubscribe_list(
        &mut conn,
        &fx.app.state.secrets,
        fx.workspace,
        first,
        "ada@example.com",
    )
    .await
    .expect("the contact refuses this list");
    drop(conn);

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let erase_path = fx.erase_path();
    let path = fx.delete_list_path(first);
    let erasing = tokio::spawn({
        let admin = fx.admin;
        async move { admin.post(&erase_path).await }
    });
    settle(&mut probe, 1).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&path).await });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (erased, deleted) = (
        erasing.await.expect("the erasure"),
        deleting.await.expect("the deletion"),
    );
    erased.assert_status(StatusCode::NO_CONTENT);
    deleted.assert_status(StatusCode::NO_CONTENT);
}

/// **The preference centre's re-tick against a rename.** `lift_unsubscribed_list` takes the list
/// row before the refusal it lifts; the membership insert that follows takes the same row through
/// its own foreign key, and a rename runs `lists` → `suppressions` through its trigger.
///
/// This replaces a probe that asserted the lift *held* the list row when it returned. It did
/// discriminate the mutation it was written against, but it passes just as well if the lift takes
/// the refusal first and the list row second — which is the deadlock, not the fix. Possession at
/// return is not order.
#[sqlx::test]
async fn a_re_tick_and_a_rename_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let mut conn = pool.acquire().await.unwrap();
    nm::suppressions::unsubscribe_list(
        &mut conn,
        &fx.app.state.secrets,
        fx.workspace,
        first,
        "ada@example.com",
    )
    .await
    .expect("refuse");
    sqlx::query("update list_memberships set status = 'unsubscribed' where list_id = $1")
        .bind(first)
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    // The same `contacts` wedge: the membership insert a re-tick writes takes that row through its
    // own foreign key, which stops the save after it has lifted the refusal and before it has the
    // list.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let link = fx.unsub();
    let saving = tokio::spawn({
        let app = TestApp::new(pool.clone()).await;
        let body = format!("list={first}");
        async move { app.post(&format!("/u/{link}")).text(&body).await }
    });
    settle(&mut probe, 1).await;

    let renaming = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("update lists set name = 'renamed' where id = $1")
                .bind(first)
                .execute(&pool)
                .await
        }
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let saved = saving.await.expect("the save");
    renaming
        .await
        .expect("the rename task")
        .expect("the rename");
    saved.assert_status_ok();
}

/// **The operator's per-list unsubscribe against a list deletion**, which is the same shape as the
/// one-click above and stands for every path that updates an existing membership and then records
/// it: `contacts::selection::unsubscribe`, `public::confirm` and `lists::add_member` all take
/// their list through `lists::lock_lists` or a `for key share` on the read they already do, for
/// this reason. One of them is held here rather than four, because the mechanism is one.
#[sqlx::test]
async fn an_operator_unsubscribe_and_a_list_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let unsub_path = format!(
        "/api/v1/workspaces/{}/lists/{first}/members/{}/unsubscribe",
        fx.workspace, fx.contact
    );
    let delete_path = fx.delete_list_path(first);
    let unsubscribing = tokio::spawn({
        let admin = fx.admin;
        async move { admin.post(&unsub_path).await }
    });
    settle(&mut probe, 1).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&delete_path).await });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (unsubscribed, deleted) = (
        unsubscribing.await.expect("the unsubscribe"),
        deleting.await.expect("the deletion"),
    );
    unsubscribed.assert_status(StatusCode::NO_CONTENT);
    deleted.assert_status(StatusCode::NO_CONTENT);
}

// ---- The other paths that take position 2 through the same mechanism -------------------------
//
// `an_operator_unsubscribe_…` above was written to stand for this class. Standing for a class is
// not holding it, so each of these is held on its own — and writing them out turned up a
// distinction nobody had made: **two of the six fail as deadlocks and four as foreign key
// violations**, because a cascade can only block on a membership row it can SEE. A path that
// updates an existing row holds something the deletion needs, and the cycle closes; a path that
// inserts a new one holds nothing visible, so the deletion commits out from under it and the
// foreign key check that follows finds no list. Both are reached by the same missing lock and both
// red here, but only the first kind is a deadlock, and a reviewer looking only for `40P01` would
// have called the other four safe.

/// `POST /f/…` against a list deleted while the submission is in flight.
///
/// The wedge here is the **address advisory lock** rather than the contact row: this route takes
/// that lock first on every path, and holding it stops the submission after it has read the form's
/// lists and before it writes anything. Spec §8.9 is what makes the failure interesting — this
/// route may not answer differently — so the mutation does not merely lose a membership, it
/// answers an anonymous submitter `500` where every other outcome is the same success page.
#[sqlx::test]
async fn a_form_submission_and_a_list_deletion_do_not_collide(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let form = Uuid::now_v7();
    sqlx::query(
        "insert into forms (id, workspace_id, slug, name, fields, headline, button_label) \
         values ($1, $2, 'join', 'Signup', $3::jsonb, 'Join', 'Join')",
    )
    .bind(form)
    .bind(fx.workspace)
    .bind(
        serde_json::json!([{"target": {"kind": "email"}, "required": true, "label": "Email"}])
            .to_string(),
    )
    .execute(&pool)
    .await
    .expect("form");
    sqlx::query("insert into form_lists (workspace_id, form_id, list_id) values ($1, $2, $3)")
        .bind(fx.workspace)
        .bind(form)
        .bind(first)
        .execute(&pool)
        .await
        .expect("form list");

    // The route's own first lock, derived the way `suppressions::lock_address` derives it.
    let hmac = nm::crypto::suppression_hmac(
        &fx.app.state.secrets,
        &nm::users::normalize_email("new@ex.test").unwrap(),
    );
    let key = i64::from_be_bytes(hmac[..8].try_into().unwrap());
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(key)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the address lock");

    let submitting = tokio::spawn({
        let app = TestApp::new(pool.clone()).await;
        async move {
            app.post("/f/acme/join")
                .content_type("application/x-www-form-urlencoded")
                .text("email=new@ex.test")
                .await
        }
    });
    settle(&mut probe, 1).await;

    let path = fx.delete_list_path(first);
    fx.admin
        .delete(&path)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    held.commit().await.expect("release the wedge");

    let answered = submitting.await.expect("the submission");
    answered.assert_status_ok();
    let memberships: i64 = sqlx::query_scalar("select count(*) from list_memberships")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(memberships, 0, "the list it named is gone; nothing is owed");
}

/// `GET /c/{token}` against a list deletion. A confirmation promotes an EXISTING membership, so
/// the deletion's cascade can see the row and wait for it: this one is a true cycle.
#[sqlx::test]
async fn a_confirmation_and_a_list_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    sqlx::query("update list_memberships set status = 'pending', subscribed_at = null")
        .execute(&pool)
        .await
        .unwrap();
    let link = issue(
        &fx.app.state.secrets,
        &Token {
            workspace_id: fx.workspace,
            contact_id: fx.contact,
            purpose: Purpose::Confirm,
            list_id: Some(first),
            issued_at: Utc::now().timestamp(),
            nonce: "c1".to_owned(),
            kid: None,
            ref_id: None,
        },
    );

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let confirming = tokio::spawn({
        let app = TestApp::new(pool.clone()).await;
        async move { app.get(&format!("/c/{link}")).await }
    });
    settle(&mut probe, 1).await;

    let path = fx.delete_list_path(first);
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&path).await }
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (confirmed, deleted) = (
        confirming.await.expect("the confirmation"),
        deleting.await.expect("the deletion"),
    );
    confirmed.assert_status_ok();
    deleted.assert_status(StatusCode::NO_CONTENT);
}

/// An operator adding a member against a list deletion. A new membership row is invisible to the
/// cascade, so this is the foreign key kind: the deletion commits out from under it.
#[sqlx::test]
async fn adding_a_member_and_a_list_deletion_do_not_collide(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first", "fresh"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let fresh = lists[1];
    sqlx::query("delete from list_memberships where list_id = $1")
        .bind(fresh)
        .execute(&pool)
        .await
        .unwrap();

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let add_path = format!("/api/v1/workspaces/{}/lists/{fresh}/members", fx.workspace);
    let delete_path = fx.delete_list_path(fresh);
    let contact = fx.contact;
    let adding = tokio::spawn({
        let admin = fx.admin;
        async move {
            admin
                .post(&add_path)
                .json(&serde_json::json!({"contact_id": contact}))
                .await
        }
    });
    settle(&mut probe, 1).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&delete_path).await });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (added, deleted) = (
        adding.await.expect("the add"),
        deleting.await.expect("the deletion"),
    );
    assert!(
        added.status_code().is_success(),
        "adding a member answered {}",
        added.status_code()
    );
    deleted.assert_status(StatusCode::NO_CONTENT);
}

/// Bulk subscribe against a list deletion: the foreign key kind, as the single-member add above.
#[sqlx::test]
async fn a_bulk_subscribe_and_a_list_deletion_do_not_collide(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first", "fresh"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let fresh = lists[1];
    sqlx::query("delete from list_memberships where list_id = $1")
        .bind(fresh)
        .execute(&pool)
        .await
        .unwrap();

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let bulk_path = format!(
        "/api/v1/workspaces/{}/contacts/bulk/subscribe",
        fx.workspace
    );
    let delete_path = fx.delete_list_path(fresh);
    let contact = fx.contact;
    let subscribing = tokio::spawn({
        let admin = fx.admin;
        async move {
            admin
                .post(&bulk_path)
                .json(&serde_json::json!({"ids": [contact], "list_id": fresh}))
                .await
        }
    });
    settle(&mut probe, 1).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&delete_path).await });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (subscribed, deleted) = (
        subscribing.await.expect("the bulk subscribe"),
        deleting.await.expect("the deletion"),
    );
    subscribed.assert_status_ok();
    deleted.assert_status(StatusCode::NO_CONTENT);
}

/// Bulk unsubscribe against a list deletion. It updates existing membership rows and records each
/// one in the same statement, so the cascade can see what it holds: the deadlock kind.
#[sqlx::test]
async fn a_bulk_unsubscribe_and_a_list_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from contacts where id = $1 for update")
        .bind(fx.contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the contact row");

    let bulk_path = format!(
        "/api/v1/workspaces/{}/contacts/bulk/unsubscribe",
        fx.workspace
    );
    let delete_path = fx.delete_list_path(first);
    let contact = fx.contact;
    let unsubscribing = tokio::spawn({
        let admin = fx.admin;
        async move {
            admin
                .post(&bulk_path)
                .json(&serde_json::json!({"ids": [contact], "list_id": first}))
                .await
        }
    });
    settle(&mut probe, 1).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&delete_path).await });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (unsubscribed, deleted) = (
        unsubscribing.await.expect("the bulk unsubscribe"),
        deleting.await.expect("the deletion"),
    );
    unsubscribed.assert_status_ok();
    deleted.assert_status(StatusCode::NO_CONTENT);
}

// ---- `delete_list`'s own order, and the `contacts` class -------------------------------------
//
// Every test above pairs some path AGAINST `delete_list`, which means `delete_list`'s own order
// was never the thing under test — and it was wrong. Every test above also wedges the `contacts`
// row with a transaction that never asks for anything back, which cannot cycle; a transaction that
// DELETES a contact does ask, for the very membership rows the wedged route is holding. Both gaps
// are closed below.

/// Nothing in either of these two is blocked outright, so neither can be asserted with a single
/// status: a list deleted out from under a rename is an honest `404`, and a save whose lists all
/// vanished is an honest `200`. What may not happen is a `5xx` — Postgres aborting one side with
/// `40P01` surfaces as `ApiError::Internal`.
fn no_server_error(label: &str, r: &axum_test::TestResponse) {
    assert!(
        !r.status_code().is_server_error(),
        "{label} answered {} — a deadlock reaches a caller as a 500",
        r.status_code()
    );
}

/// Locks the one `list`-scope refusal in the workspace, so that both sides of the pair below queue
/// on it and are released together. Postgres grants in arrival order, which is what makes the
/// interleaving deterministic rather than a coin flip.
async fn wedge_the_refusal(pool: &PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut held = pool.begin().await.unwrap();
    let locked: Vec<Uuid> = sqlx::query_scalar(
        "select id from suppressions where scope = 'list' and lifted_at is null for update",
    )
    .fetch_all(&mut *held)
    .await
    .expect("wedge the refusal");
    assert_eq!(locked.len(), 1, "the premise: one refusal to queue on");
    held
}

/// **`delete_list` against a rename of the same list.** `delete_list` takes the `suppressions`
/// rows for the list before it takes the list itself, which is position 3 before position 2 — the
/// one path in the ADR's own table that violated the order the ADR derives. A rename goes `lists`
/// then `suppressions` through its trigger, so the two invert.
#[sqlx::test]
async fn a_list_deletion_and_a_rename_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let mut conn = pool.acquire().await.unwrap();
    nm::suppressions::unsubscribe_list(
        &mut conn,
        &fx.app.state.secrets,
        fx.workspace,
        first,
        "ada@example.com",
    )
    .await
    .expect("a refusal of this list");
    drop(conn);

    let held = wedge_the_refusal(&pool).await;

    let delete_path = fx.delete_list_path(first);
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&delete_path).await }
    });
    settle(&mut probe, 1).await;

    let rename_path = format!("/api/v1/workspaces/{}/lists/{first}", fx.workspace);
    let mut renamer = TestApp::new(pool.clone()).await;
    renamer.login("owner@example.com", PW).await;
    let renaming = tokio::spawn(async move {
        renamer
            .patch(&rename_path)
            .json(&serde_json::json!({"name": "renamed"}))
            .await
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (deleted, renamed) = (
        deleting.await.expect("the deletion"),
        renaming.await.expect("the rename"),
    );
    no_server_error("the deletion", &deleted);
    no_server_error("the rename", &renamed);
}

/// **`delete_list` against the preference centre's re-tick**, which is the same inversion reached
/// from the other side: `save` takes the list through `lock_lists` and then *updates* the existing
/// refusal, so it holds position 2 and wants position 3 while `delete_list` holds 3 and wants 2.
/// Postgres picks the victim, so half the time this is a `500` on `POST /u/{token}`.
#[sqlx::test]
async fn a_list_deletion_and_a_re_tick_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let mut conn = pool.acquire().await.unwrap();
    nm::suppressions::unsubscribe_list(
        &mut conn,
        &fx.app.state.secrets,
        fx.workspace,
        first,
        "ada@example.com",
    )
    .await
    .expect("a refusal of this list");
    sqlx::query("update list_memberships set status = 'unsubscribed'")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let held = wedge_the_refusal(&pool).await;

    let delete_path = fx.delete_list_path(first);
    let link = fx.unsub();
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&delete_path).await }
    });
    settle(&mut probe, 1).await;

    let saving = tokio::spawn({
        let app = TestApp::new(pool.clone()).await;
        let body = format!("list={first}");
        async move { app.post(&format!("/u/{link}")).text(&body).await }
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (deleted, saved) = (
        deleting.await.expect("the deletion"),
        saving.await.expect("the save"),
    );
    no_server_error("the deletion", &deleted);
    no_server_error("the preference centre", &saved);
}

/// Locks the contact's membership rows, so that the writer and the contact deletion below both
/// queue on them in arrival order. The writer arrives first and is granted first, which is what
/// leaves it holding a membership row while the deletion holds the contact.
async fn wedge_the_memberships(
    pool: &PgPool,
    contact: Uuid,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from list_memberships where contact_id = $1 for update")
        .bind(contact)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the membership rows");
    held
}

/// **The `contacts` class, which the order did not name.** Every `list_memberships` and
/// `consent_log` row carries a foreign key to `contacts`, so every writer took that row
/// `for key share` **last**, at the end of its statement — while every contact-deleting path takes
/// it **first**, exclusively, and then cascades into the membership rows. An inversion on a whole
/// class rather than a pair.
///
/// It is the same split the tests above found for `delete_list`: a path that UPDATES a membership
/// row deadlocks, a path that inserts one does not. Three are held here, one per route that
/// updates: the operator's, and the two unauthenticated ones.
#[sqlx::test]
async fn an_operator_unsubscribe_and_a_contact_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let held = wedge_the_memberships(&pool, fx.contact).await;

    let unsub_path = format!(
        "/api/v1/workspaces/{}/lists/{first}/members/{}/unsubscribe",
        fx.workspace, fx.contact
    );
    let delete_path = format!(
        "/api/v1/workspaces/{}/contacts/{}",
        fx.workspace, fx.contact
    );
    let unsubscribing = tokio::spawn({
        let admin = fx.admin;
        async move { admin.post(&unsub_path).await }
    });
    settle(&mut probe, 1).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&delete_path).await });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (unsubscribed, deleted) = (
        unsubscribing.await.expect("the unsubscribe"),
        deleting.await.expect("the deletion"),
    );
    no_server_error("the operator unsubscribe", &unsubscribed);
    no_server_error("the contact deletion", &deleted);
}

/// `GET /c/{token}` against a contact deletion. Unauthenticated, and it promotes an existing
/// membership row, so it is the deadlocking kind.
#[sqlx::test]
async fn a_confirmation_and_a_contact_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    sqlx::query("update list_memberships set status = 'pending', subscribed_at = null")
        .execute(&pool)
        .await
        .unwrap();
    let link = issue(
        &fx.app.state.secrets,
        &Token {
            workspace_id: fx.workspace,
            contact_id: fx.contact,
            purpose: Purpose::Confirm,
            list_id: Some(first),
            issued_at: Utc::now().timestamp(),
            nonce: "c2".to_owned(),
            kid: None,
            ref_id: None,
        },
    );
    let held = wedge_the_memberships(&pool, fx.contact).await;

    let delete_path = format!(
        "/api/v1/workspaces/{}/contacts/{}",
        fx.workspace, fx.contact
    );
    let confirming = tokio::spawn({
        let app = TestApp::new(pool.clone()).await;
        async move { app.get(&format!("/c/{link}")).await }
    });
    settle(&mut probe, 1).await;

    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&delete_path).await }
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (confirmed, deleted) = (
        confirming.await.expect("the confirmation"),
        deleting.await.expect("the deletion"),
    );
    no_server_error("the confirmation", &confirmed);
    no_server_error("the contact deletion", &deleted);
}

/// `POST /u/{token}` with the RFC 8058 body, against a contact deletion. This is the route whose
/// own comment says it may not fail, and the advisory lock does not help: `delete_contact` never
/// takes it.
#[sqlx::test]
async fn a_one_click_and_a_contact_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let _ = lists;
    let held = wedge_the_memberships(&pool, fx.contact).await;

    let link = fx.unsub();
    let delete_path = format!(
        "/api/v1/workspaces/{}/contacts/{}",
        fx.workspace, fx.contact
    );
    let one_click = tokio::spawn({
        let app = TestApp::new(pool.clone()).await;
        async move { app.post(&format!("/u/{link}")).text(ONE_CLICK).await }
    });
    settle(&mut probe, 1).await;

    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&delete_path).await }
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (clicked, deleted) = (
        one_click.await.expect("the one-click"),
        deleting.await.expect("the deletion"),
    );
    no_server_error("the one-click", &clicked);
    no_server_error("the contact deletion", &deleted);
}

/// **`add_member` answers `404` when the list is gone, and nothing pinned that.** Its error
/// mapping reads *"the list exists in this workspace, so the only foreign key left to fail is the
/// contact's"* and sends every foreign key violation to `NotFound` — which is only true because
/// the `for key share` above it is on a `fetch_one`, so a vanished list raises `RowNotFound` first.
/// The concurrency test beside this one asserts `is_success()` and so says nothing about the
/// answer when the list really has gone. No wedge and no race: the list is simply deleted first.
#[sqlx::test]
async fn adding_a_member_to_a_deleted_list_is_not_found(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let first = lists[0];
    fx.admin
        .delete(&fx.delete_list_path(first))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let r = fx
        .admin
        .post(&format!(
            "/api/v1/workspaces/{}/lists/{first}/members",
            fx.workspace
        ))
        .json(&serde_json::json!({"contact_id": fx.contact}))
        .await;
    r.assert_status(StatusCode::NOT_FOUND);
}

/// Bulk subscribe to a list that is gone answers `404`. Pinned beside `add_member`'s `404` above
/// and for the same reason — nothing held it — but note what it does **not** hold.
///
/// `selection::subscribe` now narrows itself to what `lists::lock_lists` returned, because it is
/// the one caller that INSERTS and discarding the answer meant naming a deleted list in the insert
/// and reporting its foreign key violation as a `500`. This test passes with or without that
/// narrowing, because `bulk_subscribe_contacts` calls `lists::get` first and answers `404` from
/// there. The narrowing is reachable only when a deletion commits between that read and the
/// insert — two statements of one transaction apart — which no wedge available here can hold open.
/// So it is defence that matches `lock_lists`'s stated contract, and it is unpinned; said out loud
/// rather than dressed up by this test.
#[sqlx::test]
async fn a_bulk_subscribe_to_a_deleted_list_is_not_found(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first", "fresh"]).await;
    let fresh = lists[1];
    fx.admin
        .delete(&fx.delete_list_path(fresh))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let r = fx
        .admin
        .post(&format!(
            "/api/v1/workspaces/{}/contacts/bulk/subscribe",
            fx.workspace
        ))
        .json(&serde_json::json!({"ids": [fx.contact], "list_id": fresh}))
        .await;
    r.assert_status(StatusCode::NOT_FOUND);
}

/// **`update_form` against `delete_list`: `form_lists` is the fifth class.** ADR 0005 said
/// `form_lists` was "only ever reached through a `lists` cascade or after the `lists` and
/// `contacts` rows are already held", and `forms::routes::set_lists` is the writer that makes that
/// false — it deletes every committed link and re-inserts, and nothing in `src/forms/` takes a
/// `lists` row. So it holds a `form_lists` row the deletion's cascade needs, and then asks for the
/// list through its own insert's foreign key.
///
/// Any PATCH naming `list_ids` reaches it, including one that keeps the same lists, which is the
/// common case. `40P01` is neither a unique nor a foreign key violation, so it misses both of this
/// module's mappings and falls through to `ApiError::Internal`: a `500` on an Editor route.
#[sqlx::test]
async fn a_form_update_and_a_list_deletion_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let form = Uuid::now_v7();
    sqlx::query(
        "insert into forms (id, workspace_id, slug, name, fields, headline, button_label) \
         values ($1, $2, 'join', 'Signup', $3::jsonb, 'Join', 'Join')",
    )
    .bind(form)
    .bind(fx.workspace)
    .bind(
        serde_json::json!([{"target": {"kind": "email"}, "required": true, "label": "Email"}])
            .to_string(),
    )
    .execute(&pool)
    .await
    .expect("form");
    sqlx::query("insert into form_lists (workspace_id, form_id, list_id) values ($1, $2, $3)")
        .bind(fx.workspace)
        .bind(form)
        .bind(first)
        .execute(&pool)
        .await
        .expect("the committed link the cascade has to wait for");

    // The list, held exclusively, so the deletion and the form update queue on it in arrival
    // order: the deletion first, which is what leaves it holding the list while the update is
    // already holding the link row.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from lists where id = $1 for update")
        .bind(first)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the list");

    let delete_path = fx.delete_list_path(first);
    let form_path = format!("/api/v1/workspaces/{}/forms/{form}", fx.workspace);
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&delete_path).await }
    });
    settle(&mut probe, 1).await;

    let mut editor = TestApp::new(pool.clone()).await;
    editor.login("owner@example.com", PW).await;
    let updating = tokio::spawn(async move {
        editor
            .patch(&form_path)
            .json(&serde_json::json!({"list_ids": [first]}))
            .await
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (deleted, updated) = (
        deleting.await.expect("the deletion"),
        updating.await.expect("the form update"),
    );
    no_server_error("the list deletion", &deleted);
    no_server_error("the form update", &updated);
}

/// **Bulk tag against `delete_tag`.** Not a cycle — a tag deletion cascades into `contact_tags`
/// and this insert writes rows it cannot see, so there was never a cycle, only a race. The route's
/// `tags::get` guard is an unlocked read, so a tag deleted between it and the insert fired
/// `contact_tags_workspace_id_tag_id_fkey` and reached the operator as a `500`, while the
/// single-row sibling `tags::apply` answers `404` for the identical cause.
///
/// `selection::tag` now takes the tag `for key share` first, so inside its transaction the tag
/// cannot vanish: the violation is unreachable rather than translated, and a tag already gone is
/// the `404` the guard always meant.
#[sqlx::test]
async fn a_bulk_tag_and_a_tag_deletion_do_not_collide(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, _lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let tag = Uuid::now_v7();
    sqlx::query("insert into tags (id, workspace_id, name) values ($1, $2, 'vip')")
        .bind(tag)
        .bind(fx.workspace)
        .execute(&pool)
        .await
        .expect("tag");

    // The deletion queues first, so it wins the row and commits before the bulk action looks.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from tags where id = $1 for update")
        .bind(tag)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the tag");

    let tag_path = format!("/api/v1/workspaces/{}/tags/{tag}", fx.workspace);
    let bulk_path = format!("/api/v1/workspaces/{}/contacts/bulk/tag", fx.workspace);
    let contact = fx.contact;
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&tag_path).await }
    });
    settle(&mut probe, 1).await;

    let mut tagger = TestApp::new(pool.clone()).await;
    tagger.login("owner@example.com", PW).await;
    let tagging = tokio::spawn(async move {
        tagger
            .post(&bulk_path)
            .json(&serde_json::json!({"ids": [contact], "tag_id": tag}))
            .await
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (deleted, tagged) = (
        deleting.await.expect("the tag deletion"),
        tagging.await.expect("the bulk tag"),
    );
    no_server_error("the tag deletion", &deleted);
    no_server_error("the bulk tag", &tagged);
    assert_eq!(
        tagged.status_code(),
        StatusCode::NOT_FOUND,
        "a tag that is gone is a 404, the same answer `tags::apply` gives"
    );
}

/// **`POST /imports/{id}/start` against `delete_list`.** Also a race rather than a cycle:
/// `imports.list_id` is null until the `update` fires, so a list deletion has no committed row of
/// this import's to block on — which is why `imports` is not a lock class.
///
/// The guard above that update was an unlocked read, so a list deleted in between fired
/// `imports_workspace_id_list_id_fkey` and the operator read `internal error` — exactly what the
/// guard exists to prevent, through a window two statements wide. Holding the row closes it.
#[sqlx::test]
async fn starting_an_import_and_a_list_deletion_do_not_collide(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (fx, lists) = fixture(&pool, &["first"]).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let first = lists[0];
    let import = Uuid::now_v7();
    sqlx::query(
        "insert into imports (id, workspace_id, status, filename, delimiter, has_header) \
         values ($1, $2, 'pending', 'a.csv', 44, true)",
    )
    .bind(import)
    .bind(fx.workspace)
    .execute(&pool)
    .await
    .expect("import");

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from lists where id = $1 for update")
        .bind(first)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the list");

    let delete_path = fx.delete_list_path(first);
    let start_path = format!("/api/v1/workspaces/{}/imports/{import}/start", fx.workspace);
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&delete_path).await }
    });
    settle(&mut probe, 1).await;

    let mut starter = TestApp::new(pool.clone()).await;
    starter.login("owner@example.com", PW).await;
    let starting = tokio::spawn(async move {
        starter
            .post(&start_path)
            .json(&serde_json::json!({
                "list_id": first,
                "mapping": [{"kind": "field", "value": "email"}],
            }))
            .await
    });
    settle(&mut probe, 2).await;

    held.commit().await.expect("release the wedge");
    let (deleted, started) = (
        deleting.await.expect("the list deletion"),
        starting.await.expect("the import start"),
    );
    no_server_error("the list deletion", &deleted);
    no_server_error("the import start", &started);
}

// ---- M2a: the new foreign-key parents, re-enumerated ------------------------------------------
//
// Migration `0028` added `relays`, `sending_domains` and `relay_buckets`, so ADR 0005's
// enumeration was re-run against `pg_constraint` rather than re-read. It found **one** new parent,
// `relays`, whose only child is `relay_buckets`; `sending_domains` is referenced by nothing and so
// is not a parent at all. Both are ruled "not a class", and the five tests below are what that
// ruling rests on — a ruling with no evidence is the thing that section of the ADR exists to stop.
//
// Three of the five are a relay deletion against a bucket debit, because **one arrival order is
// not an answer**: the two orders produce different outcomes (`23503` and no error at all) by
// different mechanisms, and a reader given only one of them draws the wrong conclusion about why.

/// The shape Task 8's `bucket::take` will have: one `insert … on conflict do update` that refills
/// by elapsed time, caps at the relay's per-minute limit and subtracts `n`.
///
/// One thing about it is load-bearing here and the whole ruling rests on it. The **insert** arm
/// writes the foreign-key columns, so it takes `for key share` on the `relays` row at the end of
/// the statement. The **update** arm writes `tokens` and `refilled_at` only — neither is part of
/// the foreign key — so the referential trigger does not fire and it takes nothing on the parent.
/// **This is now a copy of the shipped statement, not a reconstruction of a plan's description of
/// it**, and the difference mattered. While `take` did not yet exist, this constant was written
/// from the plan's wording; Task 8 then wrote the real thing and found two departures, one of them
/// a bug this constant still had:
///
/// * the insert arm is a `select … where $3 >= $4` rather than `values`, so a first take larger
///   than the limit proposes no row and returns `false`, instead of inserting a negative balance
///   and raising `23514` against `relay_buckets_tokens_non_negative`;
/// * the refill is wrapped in `greatest(0, …)`. `now()` is **transaction start time**, so a taker
///   that began later but committed first leaves `refilled_at` *ahead* of this transaction's
///   `now()`. The elapsed interval is then slightly negative, `floor` turns that into `-1`, and
///   the update arm **removes** a token instead of adding none. Measured: the concurrency test
///   granted 45 of 50 before the clamp.
///
/// Both arms' lock behaviour is unchanged — the insert arm still writes the foreign-key columns and
/// so still takes `for key share` on `relays`, the update arm still writes only `tokens` and
/// `refilled_at` and so still takes nothing on the parent — which is why ADR 0005's ruling survives
/// the change. **Keep this in step with `src/relay/bucket.rs::TAKE`**: the moment the two drift,
/// the lock-order evidence is about a statement nothing runs.
const DEBIT: &str = "insert into relay_buckets (relay_id, workspace_id, tokens, refilled_at)
     select $1, $2, $3::int - $4::int, now() where $3::int >= $4::int
     on conflict (relay_id) do update
        set tokens = least($3::int, relay_buckets.tokens
              + greatest(0, floor(extract(epoch from now() - relay_buckets.refilled_at) * $3::int / 60)))::int
              - $4::int,
            refilled_at = now()
      where least($3::int, relay_buckets.tokens
              + greatest(0, floor(extract(epoch from now() - relay_buckets.refilled_at) * $3::int / 60)))::int
              >= $4::int
     returning tokens";

/// The lock-order evidence is about `DEBIT`; what runs is `bucket::take`. If they drift, the
/// evidence is about a statement nothing executes, and nobody would notice until a production
/// deadlock. This is the whole enforcement of "keep this in step".
#[test]
fn debit_is_the_statement_take_runs() {
    assert_eq!(DEBIT, nm::relay::bucket::TAKE);
}

fn sqlstate(err: &sqlx::Error) -> String {
    err.as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()))
        .unwrap_or_else(|| format!("not a database error: {err}"))
}

/// A workspace and one relay. `bucket` seeds the `relay_buckets` row, which is what tells the two
/// arms of `DEBIT` apart: absent is a relay's first tick, present is every tick after it.
async fn relay_fixture(pool: &PgPool, bucket: bool) -> (Uuid, Uuid) {
    let ws = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, 'acme', 'acme')")
        .bind(ws)
        .execute(pool)
        .await
        .expect("workspace");
    let relay = Uuid::now_v7();
    sqlx::query(
        "insert into relays (id, workspace_id, name, host, port, security, rate_limit_per_minute)
         values ($1, $2, 'primary', 'smtp.example.com', 587, 'starttls', 60)",
    )
    .bind(relay)
    .bind(ws)
    .execute(pool)
    .await
    .expect("relay");
    if bucket {
        // Ten, and the reason is a bug this seeding has already had. `DEBIT`'s update arm answers
        // `least(60, seeded + elapsed_seconds) - 10`, because the relay's limit of 60 a minute is
        // exactly one token a second. Seeded at 40 it answers 30 only while the test is
        // instantaneous, drifts up by one a second, and **reaches the insert arm's 50 after twenty
        // seconds** — at which point the assertion that tells the two arms apart silently stops
        // telling them apart. It passed only because the machine was CPU-capped at the time.
        //
        // Seeded at 10 the update arm answers `elapsed` — nought to a handful — against the insert
        // arm's unchanging 50, so the two are forty apart and stay that way for the best part of a
        // minute. The assertions below are written as bounds rather than equalities for the same
        // reason: the quantity genuinely is time-dependent, and a test that pretends otherwise is
        // a test that passes on a fast machine and fails on a slow one.
        sqlx::query(
            "insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 10)",
        )
        .bind(relay)
        .bind(ws)
        .execute(pool)
        .await
        .expect("bucket");
    }
    (ws, relay)
}

/// **The enumeration is closed, and this is what closes it.** `issues/38` records the method ADR
/// 0005's own history argues for — a new parent, **or a new cascade**, means re-running the
/// enumeration rather than merely obeying the order — and nothing enforced it, which is how
/// `form_lists` got through once already.
///
/// It pins the whole edge, `(parent, child, on delete action)`, and not the set of parents. The
/// set of parents alone would be enforcement of one half of the method and silence on the other:
/// a second child on `relays`, or a `cascade` quietly becoming a `set null`, moves nothing in the
/// set of parents and moves everything in the ruling. ADR 0005's rulings are written per child and
/// per action, so this is the granularity they are written at.
///
/// It is deliberately an equality and not a superset: an edge that *disappears* also invalidates
/// the ruling written about it. Duplicate edges collapse — `suppressions` points at `users` twice,
/// created-by and lifted-by — because a constraint's name is not something a ruling turns on.
#[sqlx::test]
async fn the_foreign_key_graph_is_the_one_the_adr_rules_on(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let edges: Vec<String> = sqlx::query_scalar(
        "select distinct confrelid::regclass::text || ' -> ' || conrelid::regclass::text \
                || ' (' || confdeltype::text || ')' \
         from pg_constraint \
         where contype = 'f' and connamespace = 'public'::regnamespace order by 1",
    )
    .fetch_all(&pool)
    .await
    .expect("read pg_constraint");

    assert_eq!(
        edges,
        [
            "api_keys -> selection_snapshots (n)",
            "campaigns -> jobs (c)",
            "campaigns -> messages (a)",
            "contacts -> consent_log (c)",
            "contacts -> contact_tags (c)",
            "contacts -> jobs (c)",
            "contacts -> list_memberships (c)",
            "forms -> form_lists (c)",
            "imports -> import_errors (c)",
            "lists -> consent_log (n)",
            "lists -> form_lists (c)",
            "lists -> imports (n)",
            "lists -> jobs (c)",
            "lists -> list_memberships (c)",
            "lists -> suppressions (n)",
            "relays -> relay_buckets (c)",
            "selection_snapshots -> selection_snapshot_members (c)",
            "tags -> contact_tags (c)",
            "users -> api_keys (n)",
            "users -> auth_challenges (c)",
            "users -> imports (n)",
            "users -> invites (n)",
            "users -> memberships (c)",
            "users -> passkeys (c)",
            "users -> recovery_codes (c)",
            "users -> segments (n)",
            "users -> selection_snapshots (n)",
            "users -> sessions (c)",
            "users -> suppressions (n)",
            "workspaces -> api_keys (c)",
            "workspaces -> brand_kits (c)",
            "workspaces -> campaigns (c)",
            "workspaces -> contacts (c)",
            "workspaces -> custom_fields (c)",
            "workspaces -> forms (c)",
            "workspaces -> imports (c)",
            "workspaces -> invites (c)",
            "workspaces -> jobs (c)",
            "workspaces -> lists (c)",
            "workspaces -> memberships (c)",
            "workspaces -> relays (c)",
            "workspaces -> segments (c)",
            "workspaces -> selection_snapshots (c)",
            "workspaces -> sending_domains (c)",
            "workspaces -> suppressions (c)",
            "workspaces -> tags (c)",
            "workspaces -> templates (c)",
        ],
        "the foreign key graph has moved. ADR 0005's enumeration is over exactly these edges, so \
         it is now out of date: re-run the query in its \"The method\" section and rule on what \
         changed, rather than editing this list to match"
    );
}

/// **Order A: the deletion's statement lands first, and the first debit raises `23503`.** The
/// debit writes a `relay_buckets` row and then asks for the `relays` row through
/// `relay_buckets_relay_fk` — child first, parent last, which is the inversion shape every class
/// in the ADR has. The deletion is holding that parent row, so the debit queues behind it; when it
/// commits, the referential check finds the parent gone.
///
/// **Say what this does and does not show, because the obvious reading of it is wrong.** It is
/// tempting to explain the `23503` by "the row the debit holds is uncommitted and so invisible to
/// the cascade". That is not what rules out a cycle — a cycle needs two waiting transactions, and
/// the debit is a terminal node that asks for nothing after its one statement — and the mechanism
/// is *not* in this
/// test's causal chain either: `delete from relays` locks the parent and runs its cascade scan inside one
/// statement, and that statement completed before the debit inserted anything. The cascade found
/// nothing because nothing was there yet, not because it could not see it. The invisibility
/// argument only ever matters for the interleaving neither test can stage — and that is exactly
/// why it belongs in the ADR's prose and not in a comment claiming this test measured it.
///
/// `a_first_debit_then_a_relay_deletion_both_commit` is the other arrival order, and it produces
/// no error at all. Neither order is the whole answer; the pair is.
///
/// The `settle` is what stops this passing vacuously — without it the deletion could commit before
/// the debit ever looked, and the `23503` would be a plain sequence rather than an interleaving.
#[sqlx::test]
async fn a_relay_deletion_and_a_first_debit_do_not_deadlock(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (ws, relay) = relay_fixture(&pool, false).await;
    let mut probe = pool.acquire().await.expect("a probe connection");

    let mut deleting = pool.begin().await.unwrap();
    sqlx::query("delete from relays where id = $1")
        .bind(relay)
        .execute(&mut *deleting)
        .await
        .expect("delete the relay");

    let debiting = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i32>(DEBIT)
                .bind(relay)
                .bind(ws)
                .bind(60i32)
                .bind(10i32)
                .fetch_optional(&pool)
                .await
        }
    });
    settle(&mut probe, 1).await;

    deleting.commit().await.expect("commit the deletion");
    let err = debiting
        .await
        .expect("the debit")
        .expect_err("the relay is gone, so its bucket row cannot be written");
    assert_eq!(
        sqlstate(&err),
        "23503",
        "the cascade ran before this row existed, so the debit's own check finds the parent \
         gone: a foreign key violation, never a deadlock: {err}"
    );
    assert_eq!(
        err.as_database_error().and_then(|e| e.constraint()),
        Some("relay_buckets_relay_fk"),
        "the violation names the relay reference, not some other constraint: {err}"
    );
}

/// **Order B: the debit's statement lands first, and nothing fails.** The debit holds its new
/// bucket row *and* the `for key share` its insert arm took on the relay, so the deletion blocks
/// on the **parent** row — not on the child, which is the distinction the whole ruling turns on.
/// When the debit commits, the deletion takes the relay and its cascade, now looking at a
/// committed child row, takes the bucket with it. Two transactions, both commit, no error of any
/// kind: neither the `40P01` of a cycle nor the `23503` of Order A.
///
/// **This is the order the sibling test does not reach, and it is the one that shows the insert
/// arm cannot be half of a cycle.** A cycle needs each side to hold something the other wants. The
/// debit holds the parent lock and, having finished its only statement, wants nothing further — a
/// terminal node in the wait-for graph. The deletion waits, and that is all it can do.
///
/// The deletion's wait is as long as the debit's transaction stays open, which here is however
/// long the test holds it, so no duration is asserted. What is asserted is that it waits at all:
/// `settle` panics if it never blocks, which is what it would do if the insert arm stopped taking
/// the parent lock, and the final count is what fails if the cascade stops seeing the child.
#[sqlx::test]
async fn a_first_debit_then_a_relay_deletion_both_commit(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (ws, relay) = relay_fixture(&pool, false).await;
    let mut probe = pool.acquire().await.expect("a probe connection");

    let mut debiting = pool.begin().await.unwrap();
    let left: i32 = sqlx::query_scalar(DEBIT)
        .bind(relay)
        .bind(ws)
        .bind(60i32)
        .bind(10i32)
        .fetch_one(&mut *debiting)
        .await
        .expect("the debit");
    assert_eq!(
        left, 50,
        "the premise: the INSERT arm ran, so this transaction holds the relay `for key share`"
    );

    let deleting = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("delete from relays where id = $1")
                .bind(relay)
                .execute(&pool)
                .await
        }
    });
    settle(&mut probe, 1).await;

    debiting.commit().await.expect("commit the debit");
    let deleted = deleting
        .await
        .expect("the deletion")
        .expect("neither 40P01 nor anything else: this order produces no error");
    assert_eq!(deleted.rows_affected(), 1, "the relay really did go");

    let buckets: i64 = sqlx::query_scalar("select count(*) from relay_buckets")
        .fetch_one(&pool)
        .await
        .expect("count the buckets");
    assert_eq!(
        buckets, 0,
        "the cascade re-scanned after the wait and took the now-committed bucket row"
    );
}

/// **Every debit after the first takes no lock on its relay at all.** This is the measured fact
/// behind the ruling, and the one the ADR would be wrong without: the update arm writes `tokens`
/// and `refilled_at`, so Postgres fires no referential trigger and an exclusive holder of the
/// `relays` row does not stand in its way.
///
/// The control is the half that keeps this honest. A wedge that silently failed to take the row
/// would let the debit through for the wrong reason and the test would still go green, so the same
/// `for key share` the **insert** arm takes is raced against the same wedge first and must block.
/// Break the ruling — make the update arm write `workspace_id` or `relay_id` — and the debit
/// blocks behind the wedge until the timeout names it.
#[sqlx::test]
async fn a_debit_of_an_existing_bucket_takes_no_lock_on_its_relay(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (ws, relay) = relay_fixture(&pool, true).await;
    let mut probe = pool.acquire().await.expect("a probe connection");

    // Stands for every exclusive taker of a relay row: the delete, and a rename, whose `name` is
    // part of `relays_workspace_name_key` and so takes `for update`, not `for no key update`.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from relays where id = $1 for update")
        .bind(relay)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the relay");

    let control = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("select 1 from relays where id = $1 for key share")
                .bind(relay)
                .fetch_all(&pool)
                .await
        }
    });
    settle(&mut probe, 1).await;

    let left = tokio::time::timeout(
        Duration::from_secs(10),
        sqlx::query_scalar::<_, i32>(DEBIT)
            .bind(relay)
            .bind(ws)
            .bind(60i32)
            .bind(10i32)
            .fetch_one(&pool),
    )
    .await
    .expect(
        "the debit blocked behind an exclusive lock on its relay row: the update arm DOES take \
         the parent, so `relays` IS a lock class and ADR 0005's ruling on it is wrong",
    )
    .expect("the debit");
    assert!(
        left < 40,
        "the ten the bucket held, plus a second's refill per second elapsed, less the ten this \
         tick took — so a small number. 50 would mean the INSERT arm ran: a different statement \
         taking a different lock, and not the one under test here. Got {left}"
    );

    held.commit().await.expect("release the wedge");
    control
        .await
        .expect("the control")
        .expect("the lock the insert arm would have taken");
}

/// **A drain takes no lock on its relay either** (M2c R6). `bucket::DRAIN` is an update of
/// `tokens` and `refilled_at` alone, the same short-circuit as the debit's update arm, so an
/// exclusive holder of the `relays` row does not stand in its way. Make it write a key column and
/// it blocks behind the wedge until the timeout names it.
#[sqlx::test]
async fn a_drain_takes_no_lock_on_its_relay(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (_, relay) = relay_fixture(&pool, true).await;

    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from relays where id = $1 for update")
        .bind(relay)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the relay");

    tokio::time::timeout(
        Duration::from_secs(10),
        nm::relay::bucket::drain(&pool, relay, Duration::from_secs(60)),
    )
    .await
    .expect("the drain blocked behind an exclusive lock on its relay row: `relays` IS a lock class")
    .expect("the drain");
    held.commit().await.expect("release the wedge");

    let (tokens, shut): (i32, bool) = sqlx::query_as(
        "select tokens, refilled_at > now() + interval '55 seconds' from relay_buckets
          where relay_id = $1",
    )
    .bind(relay)
    .fetch_one(&pool)
    .await
    .expect("the bucket");
    assert_eq!(
        (tokens, shut),
        (0, true),
        "the drain did not empty and shut the bucket"
    );
}

/// **A relay deletion waits for a debit in flight; it does not deadlock with one.** This is the
/// `40P01` half of the ADR's split, and it is unreachable here: the debit holds a bucket row the
/// cascade *can* see, which is exactly the condition that closes the cycle everywhere else, and
/// still nothing cycles — because the holder is asking for nothing back.
///
/// What this pins beyond the ruling: that the cascade really does wait on a committed child row.
/// Weaken `relay_buckets_relay_fk` to `no action`, or drop it, and the deletion stops blocking and
/// `settle` says so, rather than the test passing on a race it never ran.
#[sqlx::test]
async fn a_relay_deletion_waits_for_a_debit_rather_than_deadlocking(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let (ws, relay) = relay_fixture(&pool, true).await;
    let mut probe = pool.acquire().await.expect("a probe connection");

    let mut debiting = pool.begin().await.unwrap();
    let left: i32 = sqlx::query_scalar(DEBIT)
        .bind(relay)
        .bind(ws)
        .bind(60i32)
        .bind(10i32)
        .fetch_one(&mut *debiting)
        .await
        .expect("the debit");
    assert!(
        left < 40,
        "the premise: the UPDATE arm ran and took its tokens. 50 would be the insert arm. \
         Got {left}"
    );

    let deleting = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("delete from relays where id = $1")
                .bind(relay)
                .execute(&pool)
                .await
        }
    });
    settle(&mut probe, 1).await;

    debiting.commit().await.expect("commit the debit");
    deleting
        .await
        .expect("the deletion")
        .expect("a cycle here would abort one side with 40P01");

    let buckets: i64 = sqlx::query_scalar("select count(*) from relay_buckets")
        .fetch_one(&pool)
        .await
        .expect("count the buckets");
    assert_eq!(buckets, 0, "the bucket went with its relay");
}

// ---- M2b: `templates` and `brand_kits`, re-enumerated -----------------------------------------
//
// Migration `0032` added two tables and, against `pg_constraint`, **two edges and no new parent**:
// `workspaces -> templates (c)` and `workspaces -> brand_kits (c)`. Neither table is referenced by
// anything, so neither is a parent and the inversion the enumeration is over has nowhere to form;
// ADR 0005 rules both "not a parent at all", as it does `sending_domains`.
//
// That ruling rests on one measurable thing, which is what the test below holds: **a write to
// either table takes `for key share` on its one `workspaces` row, on its insert arm only, and
// nothing anywhere else.** `workspaces` is the parent of seventeen tables and nothing takes it
// exclusively, so a key share on it can wait for nobody.
//
// **R7 wrote the writers.** When this was first put here there were none in `src/` and the test
// was held on the SQL shape the handlers would have, exactly as the relay tests above were before
// `take` was written. `template::routes::{create,update,delete}_template` and
// `brand::routes::save_brand_kit` now exist, each is one locking statement, and each has a row in
// ADR 0005's M2b path table. `preview_template` writes no row at all.
//
// What this test still CANNOT see is a handler that grows a SECOND statement taking another lock —
// a `select ... for update` on the template row before the update, say, or one transaction that
// writes `custom_fields` and `templates` together. `update_template` deliberately reads its row
// with a plain `select` for that reason. A handler that needs a second lock changes the ruling and
// not just the code.

/// **A template or brand-kit write waits on its workspace only when it inserts.** Two wedges on the
/// same `workspaces` row, and the difference between them is the whole test:
///
/// * `for no key update` is what an `UPDATE` of any non-key column of `workspaces` takes — the only
///   exclusive-ish lock the code takes on that row, since `src/` contains no `delete from
///   workspaces`. A template insert and a brand-kit insert must pass straight through it, because
///   `for key share` is compatible with it.
/// * `for update` is what a delete of the workspace, or an update of a key column, would take. It
///   is the **control**: the same insert is raced against it first and must block, otherwise a
///   wedge that silently failed to take the row would let every assertion below go green for the
///   wrong reason. Under that same wedge an `UPDATE` of a template, a `DELETE` of a template and
///   the update arm of a brand-kit upsert must all complete, because none of them writes a foreign
///   key column and so Postgres fires no referential trigger on `workspaces`.
///
/// If `workspaces` ever gains an exclusive taker, or a handler starts rewriting `workspace_id`, the
/// first half or the second half of this goes red respectively, naming which.
#[sqlx::test]
async fn a_template_or_brand_kit_write_takes_at_most_a_key_share_on_its_workspace(pool: PgPool) {
    let _gate = GATE.acquire().await.expect("the concurrency gate");
    let pool = own_pool(&pool).await;
    let mut probe = pool.acquire().await.expect("a probe connection");
    let ws = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, 'acme', 'acme')")
        .bind(ws)
        .execute(&pool)
        .await
        .expect("workspace");
    let (kept, doomed) = (Uuid::now_v7(), Uuid::now_v7());
    for id in [kept, doomed] {
        sqlx::query(
            "insert into templates (id, workspace_id, name, kind, source) \
             values ($1, $2, 't', 'html', 'x')",
        )
        .bind(id)
        .bind(ws)
        .execute(&pool)
        .await
        .expect("template");
    }

    const INSERT_TEMPLATE: &str = "insert into templates (id, workspace_id, name, kind, source) \
                                   values ($1, $2, 'new', 'html', 'x')";
    const UPSERT_KIT: &str = "insert into brand_kits (workspace_id, logo_url) values ($1, $2) \
                              on conflict (workspace_id) do update \
                              set logo_url = excluded.logo_url, updated_at = now()";
    async fn finishes<T>(what: &str, fut: impl std::future::Future<Output = sqlx::Result<T>>) {
        tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .unwrap_or_else(|_| panic!("{what} blocked behind a lock on its workspace row"))
            .unwrap_or_else(|e| panic!("{what} failed: {e}"));
    }

    // What every non-key `UPDATE workspaces` takes.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from workspaces where id = $1 for no key update")
        .bind(ws)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the workspace, non-exclusively");
    finishes(
        "a template insert, whose foreign key takes `for key share`, which `for no key update` \
         permits: it",
        sqlx::query(INSERT_TEMPLATE)
            .bind(Uuid::now_v7())
            .bind(ws)
            .execute(&pool),
    )
    .await;
    finishes(
        "the INSERT arm of a brand-kit upsert: it",
        sqlx::query(UPSERT_KIT)
            .bind(ws)
            .bind("https://example.com/a.png")
            .execute(&pool),
    )
    .await;
    held.commit().await.expect("release the first wedge");

    // The control, and then the claim: exclusive on the parent.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("select 1 from workspaces where id = $1 for update")
        .bind(ws)
        .fetch_all(&mut *held)
        .await
        .expect("wedge the workspace, exclusively");
    let control = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query(INSERT_TEMPLATE)
                .bind(Uuid::now_v7())
                .bind(ws)
                .execute(&pool)
                .await
        }
    });
    settle(&mut probe, 1).await;

    finishes(
        "a template UPDATE, which writes no foreign key column: it",
        sqlx::query("update templates set name = 'renamed', updated_at = now() where id = $1")
            .bind(kept)
            .execute(&pool),
    )
    .await;
    finishes(
        "a template DELETE, which is the child side of the foreign key: it",
        sqlx::query("delete from templates where id = $1")
            .bind(doomed)
            .execute(&pool),
    )
    .await;
    finishes(
        "the UPDATE arm of a brand-kit upsert: it",
        sqlx::query(UPSERT_KIT)
            .bind(ws)
            .bind("https://example.com/b.png")
            .execute(&pool),
    )
    .await;

    held.commit().await.expect("release the second wedge");
    control
        .await
        .expect("the control")
        .expect("the insert the exclusive wedge was holding back");
}

// ---- Migration bytes: the issues/40 guard ------------------------------------------------------

/// **No file under `migrations/` may contain a CR byte.** `sqlx` records a migration's checksum
/// over its bytes, so a copy applied from a CRLF working tree records a checksum that the LF blob
/// git stores can never match again — without a single character of SQL differing. That broke the
/// dev database once (`issues/40`) and recurred inside M2b when a scripted edit of `0032` left CRLF
/// behind. `.gitattributes` (`* text=auto eol=lf`) normalises on commit, which is exactly why
/// nothing noticed: the committed blob was right and the working tree was not.
///
/// Plain read of the directory, no database. It lives in this file because ADR 0005's migrations
/// are what a careless byte breaks, and because this binary already runs in CI.
#[test]
fn no_migration_contains_a_cr_byte() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut crlf = vec![];
    for entry in std::fs::read_dir(&dir).expect("read the migrations directory") {
        let path = entry.expect("a directory entry").path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read a migration");
        if let Some(at) = bytes.iter().position(|&b| b == b'\r') {
            let line = bytes[..at].iter().filter(|&&b| b == b'\n').count() + 1;
            crlf.push(format!("{} (first CR on line {line})", path.display()));
        }
    }
    assert!(
        crlf.is_empty(),
        "CR byte in a migration — its checksum would differ from the LF form git stores, and a \
         database migrated from this file could never be migrated again (issues/40). Strip it \
         with `sed -i 's/\\r$//' <file>` BEFORE running `sqlx migrate run`; if it was already \
         applied, the database must be recreated, never `_sqlx_migrations` edited: {}",
        crlf.join(", ")
    );
}

// ---- The one statement that would invalidate the whole ADR -------------------------------------

/// Whether `text` contains a statement that deletes `workspaces` rows — or truncates the table,
/// which cascades the same way.
///
/// **What it reads is source text, not string values**, which is the thing the first two versions
/// of this got wrong: a Rust string literal arrives with its escapes still in it (`\"`, `\n`,
/// `\t`, `\r`, a `\` before a line break), a raw string or a `.sql` file arrives without them, and
/// both must match.
///
/// **What it tolerates:** case, any run of whitespace including newlines inside the statement,
/// those escapes, an optional `only` / `table` keyword, an optional `public.` qualifier, quoted
/// identifiers (bare or `\"`), the statement split across adjacent string literals
/// (`"delete from " "workspaces"`), and `truncate` naming it anywhere in a list, with `only`
/// before a name and `*` after it. A whole-line `//`, `///` or `//!` comment is skipped: it
/// executes nothing, and a sentence about truncation must not fail the build for everyone.
///
/// **What it does not catch, so nobody reads a green run as more than it is:** a table name that
/// is not spelled out in `src/` (`format!("delete from {table}")`, `concat!(…)`, a name read from
/// a config), a statement in a file outside `src/` that the binary loads at run time, a delete
/// that reaches `workspaces` only through another route (`drop table`, a foreign key from a new
/// parent's `on delete cascade`, which the foreign key graph test above is for), an SQL comment
/// between tokens (`delete /* c */ from workspaces`, `delete from -- c` over a line break), and a
/// spaced qualifier (`"public" . "workspaces"`). The last two are left alone on purpose: nobody
/// writes them, and chasing them would make this fragile guesswork. **It errs the other way
/// too:** a `/* … */` block, a comment trailing code on the same line, and a string or message
/// that merely *says* `delete from workspaces` all still trip it, because telling them from code
/// takes a Rust lexer. The failure message prints the line and the text that matched for that
/// reason.
fn deletes_workspaces(text: &str) -> bool {
    workspace_statement_at(&flatten(&without_line_comments(text))).is_some()
}

/// `text` with every whole-line `//` comment (`//`, `///`, `//!`) blanked. A line comment executes
/// nothing, so a statement inside one is harmless, and prose about truncation is ordinary English
/// that must not fail the build. The line count is kept so a line number found later is the
/// file's own. Only whole lines are cut: a trailing comment after code, and a `/* … */` block,
/// are still read (see "does not catch" above).
fn without_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lower-cased, unescaped, de-quoted and whitespace-collapsed, so one statement is one run of
/// single-spaced words.
fn flatten(text: &str) -> String {
    // Order matters: a continuation is a backslash before a REAL newline, so it goes first; then
    // the escapes a normal string spells with a backslash and a letter; then `\"`, which is a
    // quote that has to disappear along with the bare ones.
    let flat = text
        .to_lowercase()
        .replace("\\\r\n", " ")
        .replace("\\\n", " ")
        .replace("\\n", " ")
        .replace("\\r", " ")
        .replace("\\t", " ")
        .replace("\\\"", "")
        .replace('"', "");
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where in `flat` (the output of `flatten`) a statement against `workspaces` starts.
fn workspace_statement_at(flat: &str) -> Option<usize> {
    // One table name, ignoring `only` and a `public.` qualifier; true if it is `workspaces`.
    let is_workspaces = |name: &str| {
        let name = name.trim();
        let name = name.strip_prefix("only ").unwrap_or(name).trim_start();
        let name = name.strip_prefix("public.").unwrap_or(name);
        name.strip_prefix("workspaces")
            .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    };
    for (at, lead) in flat.match_indices("delete from ") {
        if is_workspaces(&flat[at + lead.len()..]) {
            return Some(at);
        }
    }
    for (at, lead) in flat.match_indices("truncate ") {
        let rest = &flat[at + lead.len()..];
        let rest = rest.strip_prefix("table ").unwrap_or(rest);
        // `truncate a, b, c`: every item is checked, and the list ends at the first item that has
        // anything after its name (`cascade`, `restart identity`, a `;`), because past that point
        // it is no longer the statement's table list. `only` before a name and `*` after it are
        // part of the name, not of what follows it.
        for item in rest.split(',') {
            if is_workspaces(item) {
                return Some(at);
            }
            let bare = item.trim();
            let bare = bare.strip_prefix("only ").unwrap_or(bare);
            let bare = bare.strip_suffix('*').unwrap_or(bare).trim_end();
            if bare.contains(' ') {
                break;
            }
        }
    }
    None
}

/// The line (1-based) and the flattened text of the first statement against `workspaces` in
/// `text`, for a failure message that shows the reader what matched rather than only which file.
fn locate_workspace_statement(text: &str) -> Option<(usize, String)> {
    let text = without_line_comments(text);
    let lines: Vec<&str> = text.lines().collect();
    for (i, first) in lines.iter().enumerate() {
        if first.trim().is_empty() {
            continue;
        }
        // A statement can run over several lines, so look at a window; it counts for this line
        // only if the statement STARTS on it, which keeps the answer at the first line of it.
        let flat = flatten(&lines[i..(i + 8).min(lines.len())].join("\n"));
        if let Some(at) = workspace_statement_at(&flat)
            && at < flatten(first).len()
        {
            return Some((i + 1, flat[at..].chars().take(70).collect()));
        }
    }
    None
}

/// **The matcher has to see its own target, in every shape a human will type it.** A grep test that
/// has never matched is not evidence, so this holds the positives and the near-misses that must
/// not match.
#[test]
fn the_workspace_deletion_matcher_sees_what_it_is_for() {
    for yes in [
        "delete from workspaces where id = $1",
        "DELETE   FROM   Workspaces",
        "delete\n    from\n    workspaces",
        "\"delete from \\\n         workspaces where id = $1\"",
        "\"delete from \\\r\n         workspaces where id = $1\"",
        "delete from public.workspaces",
        "delete from \"public\".\"workspaces\"",
        "delete from only workspaces",
        "\"delete from \" \"workspaces\"",
        "with gone as (delete from workspaces returning id) select * from gone",
        "truncate table workspaces cascade",
        "TRUNCATE workspaces",
        "truncate table lists, workspaces",
        "truncate table lists , only public.workspaces cascade",
        "truncate table \"lists\", \"workspaces\"",
        "truncate table only lists, only workspaces cascade",
        "truncate table lists *, workspaces *",
        "// a comment above does not hide the statement below\ndelete from workspaces",
    ] {
        assert!(deletes_workspaces(yes), "should have matched: {yes:?}");
    }
    // **Source form.** The matcher reads `.rs` files, so what it sees is the text of a Rust string
    // literal — backslash escapes still unprocessed — and a sample written as the already-unescaped
    // value is a sample of something it never meets. These are raw strings so that every
    // backslash below is exactly the byte that appears in a source file. The block above is
    // SQL text, as in a `.sql` file or a raw string; this one is what a normal string looks like.
    for yes in [
        r#"sqlx::query("delete from \"public\".\"workspaces\" where id = $1")"#,
        r#"sqlx::query("delete from \"workspaces\"")"#,
        r#""delete\nfrom\tworkspaces where id = $1""#,
        r#""delete from\r\n  workspaces""#,
        r#""truncate table lists, workspaces""#,
        r#""truncate table \"lists\", \"workspaces\" cascade""#,
    ] {
        assert!(deletes_workspaces(yes), "should have matched: {yes}");
    }
    for no in [
        "delete from workspaces_archive",
        "delete from memberships where workspace_id = $1",
        "delete from templates where id = $1",
        "select * from workspaces",
        "update workspaces set name = $2",
        "insert into workspaces (id) values ($1)",
        "truncate table lists",
        "truncate table lists, templates",
        "truncate table lists, workspaces_archive",
        "truncate table lists restart identity; select * from workspaces",
        // A line comment executes nothing, and prose about truncation is ordinary English.
        "// We never truncate lists, workspaces or members: the cascade is too wide.",
        "    /// Truncate workspaces names to 64 chars.",
        "//! This module must never delete from workspaces.",
        r#""delete from \"templates\"\nwhere workspace_id = $1""#,
    ] {
        assert!(!deletes_workspaces(no), "should not have matched: {no:?}");
    }
}

/// **Nothing in `src/` deletes a workspace.** `workspaces` is the parent of seventeen tables, every
/// one `on delete cascade`, so one `delete from workspaces` is every cascade in this ADR firing at
/// once; and ADR 0005's "no exclusive taker" ruling for `workspaces` — which is why none of those
/// seventeen is a lock class — rests on this being true. Until this test it rested on a grep
/// somebody ran once.
///
/// If this fails, do not delete the offending line to go green and do not edit the test. Re-open
/// ADR 0005's `workspaces` ruling: a delete makes it an exclusive taker, so every table in its
/// fan-out is a class the same day and each path writing one of them has to be placed in the
/// order.
#[test]
fn nothing_in_src_deletes_a_workspace() {
    fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, hits);
            } else if let Ok(bytes) = std::fs::read(&path)
                && deletes_workspaces(&String::from_utf8_lossy(&bytes))
            {
                let (line, excerpt) = locate_workspace_statement(&String::from_utf8_lossy(&bytes))
                    .unwrap_or((0, "(could not locate the line)".to_owned()));
                hits.push(format!("{}:{line}: `{excerpt}`", path.display()));
            }
        }
    }
    let mut hits = vec![];
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut hits,
    );
    assert!(
        hits.is_empty(),
        "text that reads as a delete or truncate of `workspaces` appears at (file:line: the text \
         that matched, lower-cased and flattened):\n  {}\n\
         If that is a comment, a message or a name, reword it: a trailing `//` comment, a `/* */` \
         block and a string are all still read, and only whole `//` lines are skipped. If it is a \
         real statement, `workspaces` has an exclusive taker and ADR 0005's \"no exclusive \
         taker\" ruling is void for all seventeen of its children: re-open that ruling rather \
         than editing this test",
        hits.join("\n  ")
    );
}
