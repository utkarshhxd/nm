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

/// How long a wedged transaction is held, and how long a blocked request is given to reach its
/// block. Generous: a test that released the wedge too early would simply not interleave, and the
/// mutation checks are what show the interleaving really happens.
const SETTLE: Duration = Duration::from_millis(400);

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
    let (fx, lists) = fixture(&pool, &["first"]).await;
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
    tokio::time::sleep(SETTLE).await;

    let path = fx.delete_list_path(first);
    let deleting = tokio::spawn({
        let admin = fx.admin;
        async move { admin.delete(&path).await }
    });
    tokio::time::sleep(SETTLE).await;

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
    let (fx, lists) = fixture(&pool, &["first"]).await;
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
    tokio::time::sleep(SETTLE).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&path).await });
    tokio::time::sleep(SETTLE).await;

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
    let (fx, lists) = fixture(&pool, &["first"]).await;
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
    tokio::time::sleep(SETTLE).await;

    let renaming = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("update lists set name = 'renamed' where id = $1")
                .bind(first)
                .execute(&pool)
                .await
        }
    });
    tokio::time::sleep(SETTLE).await;

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
    let (fx, lists) = fixture(&pool, &["first"]).await;
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
    tokio::time::sleep(SETTLE).await;

    let mut deleter = TestApp::new(pool.clone()).await;
    deleter.login("owner@example.com", PW).await;
    let deleting = tokio::spawn(async move { deleter.delete(&delete_path).await });
    tokio::time::sleep(SETTLE).await;

    held.commit().await.expect("release the wedge");
    let (unsubscribed, deleted) = (
        unsubscribing.await.expect("the unsubscribe"),
        deleting.await.expect("the deletion"),
    );
    unsubscribed.assert_status(StatusCode::NO_CONTENT);
    deleted.assert_status(StatusCode::NO_CONTENT);
}
