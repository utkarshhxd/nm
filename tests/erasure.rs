// SPDX-License-Identifier: AGPL-3.0-only

//! GDPR erasure. The invariant: after an address is erased, no `contacts` row for it exists and
//! none can be created. Each half has its own test, because `mailable_list_members` cannot see a
//! tombstone (its `email` is null and a domain-scoped one is unwritable): the view is safe only if
//! the row it would have joined to is gone and cannot come back.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const ADDRESS: &str = "Gone.Person@Example.test";

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

struct W {
    id: Uuid,
    editor: TestApp,
    admin: TestApp,
}

async fn workspace(pool: &PgPool, slug: &str) -> W {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let id = create_workspace(pool, owner, slug).await;
    W {
        id,
        editor: client(
            pool,
            &format!("editor-{slug}@example.com"),
            id,
            Role::Editor,
        )
        .await,
        admin: client(pool, &format!("admin-{slug}@example.com"), id, Role::Admin).await,
    }
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

fn erase_path(ws: Uuid, contact: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/contacts/{contact}/erase")
}

async fn create_contact(app: &TestApp, ws: Uuid, email: &str) -> axum_test::TestResponse {
    app.post(&format!("/api/v1/workspaces/{ws}/contacts"))
        .json(&json!({"email": email}))
        .await
}

/// A contact that is subscribed to a single-opt-in list and tagged: exactly what the eligibility
/// view would mail. Returns the contact, list and tag ids.
async fn mailable_contact(w: &W, email: &str) -> (Uuid, Uuid, Uuid) {
    let c = id_of(&create_contact(&w.admin, w.id, email).await);
    let list = id_of(
        &w.admin
            .post(&format!("/api/v1/workspaces/{}/lists", w.id))
            .json(&json!({"name": "news", "double_optin": false}))
            .await,
    );
    let r = w
        .admin
        .post(&format!("/api/v1/workspaces/{}/lists/{list}/members", w.id))
        .json(&json!({"contact_id": c}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["status"], "subscribed");
    let tag = id_of(
        &w.admin
            .post(&format!("/api/v1/workspaces/{}/tags", w.id))
            .json(&json!({"name": "vip"}))
            .await,
    );
    w.admin
        .put(&format!(
            "/api/v1/workspaces/{}/contacts/{c}/tags/{tag}",
            w.id
        ))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    (c, list, tag)
}

async fn count(pool: &PgPool, sql: &'static str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn in_view(pool: &PgPool, list: Uuid) -> i64 {
    count(
        pool,
        "select count(*) from mailable_list_members where list_id = $1",
        list,
    )
    .await
}

/// Half one. The assertion that matters is the view, not `contacts`: a surviving row with a
/// subscribed membership is the person being mailed after they asked to be erased.
#[sqlx::test]
async fn erase_removes_the_contact_and_its_rows(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, list, tag) = mailable_contact(&w, ADDRESS).await;
    assert_eq!(in_view(&pool, list).await, 1, "the setup must be mailable");

    w.admin
        .post(&erase_path(w.id, c))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    assert_eq!(
        in_view(&pool, list).await,
        0,
        "erased contact is still in the view"
    );
    let contacts = "select count(*) from contacts where id = $1";
    assert_eq!(count(&pool, contacts, c).await, 0);
    let members = "select count(*) from list_memberships where contact_id = $1";
    assert_eq!(count(&pool, members, c).await, 0);
    let tags = "select count(*) from contact_tags where contact_id = $1";
    assert_eq!(count(&pool, tags, c).await, 0);
    // The tag itself is the workspace's, not the person's.
    assert_eq!(
        count(&pool, "select count(*) from tags where id = $1", tag).await,
        1
    );
    w.admin
        .get(&format!("/api/v1/workspaces/{}/contacts/{c}", w.id))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// **`jobs` was the one table an erasure did not reach.** Its only foreign key was
/// `workspace_id`, a `confirmation_email` row carries the address in clear and a working confirm
/// token, and `0025` forbids deleting rows — so an erased person's row survived, runnable, and a
/// worker written from `0023`'s contract would have mailed them. `0026` gave the table a
/// composite contact key that cascades, which is how every other table already behaved.
#[sqlx::test]
async fn erase_removes_the_contacts_queued_jobs(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (gone, list, _) = mailable_contact(&w, ADDRESS).await;
    let kept = id_of(&create_contact(&w.admin, w.id, "stays@example.test").await);
    for c in [gone, kept] {
        sqlx::query(
            "insert into jobs (id, workspace_id, kind, contact_id, list_id, payload)              values ($1, $2, 'confirmation_email', $3, $4, '{}'::jsonb)",
        )
        .bind(Uuid::now_v7())
        .bind(w.id)
        .bind(c)
        .bind(list)
        .execute(&pool)
        .await
        .unwrap();
    }

    w.admin
        .post(&erase_path(w.id, gone))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let left: Vec<Uuid> = sqlx::query_scalar("select contact_id from jobs")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        left,
        [kept],
        "the erased contact's queued email outlived them"
    );
}

#[sqlx::test]
async fn erase_writes_a_tombstone_with_no_plaintext(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, ..) = mailable_contact(&w, ADDRESS).await;
    w.admin
        .post(&erase_path(w.id, c))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    type Row = (
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
        String,
        String,
        Option<Uuid>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select email, domain, email_hmac, reason, scope, workspace_id from suppressions",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    let (email, domain, hmac, reason, scope, workspace_id) = rows.into_iter().next().unwrap();
    assert_eq!(email, None);
    assert_eq!(domain, None);
    let expected = nm::crypto::suppression_hmac(&w.admin.state.secrets, "gone.person@example.test");
    assert_eq!(hmac.as_deref(), Some(expected.as_slice()));
    assert_eq!(
        (reason.as_str(), scope.as_str()),
        ("gdpr_erasure", "address")
    );
    assert_eq!(workspace_id, Some(w.id));
}

/// Half two, the one that gets forgotten: without it the next import re-creates the person.
#[sqlx::test]
async fn recreating_an_erased_address_is_refused(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, list, _) = mailable_contact(&w, ADDRESS).await;
    w.admin
        .post(&erase_path(w.id, c))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Any spelling of the address, by anyone who may create a contact.
    for (app, email) in [
        (&w.editor, ADDRESS),
        (&w.admin, " GONE.PERSON@example.TEST "),
    ] {
        let r = create_contact(app, w.id, email).await;
        r.assert_status(StatusCode::CONFLICT);
        assert_eq!(problem_type(&r), "urn:nm:problem:address-erased");
    }
    let in_ws = "select count(*) from contacts where workspace_id = $1";
    assert_eq!(count(&pool, in_ws, w.id).await, 0);
    assert_eq!(in_view(&pool, list).await, 0);
}

#[sqlx::test]
async fn erase_is_admin_only(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, ..) = mailable_contact(&w, ADDRESS).await;
    let tombstones = "select count(*) from suppressions where workspace_id = $1";
    let contacts = "select count(*) from contacts where id = $1";

    let r = w.editor.post(&erase_path(w.id, c)).await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    assert_eq!(count(&pool, contacts, c).await, 1);
    assert_eq!(count(&pool, tombstones, w.id).await, 0);

    // Another workspace's admin sees a 404, and the contact stays.
    let other = workspace(&pool, "other").await;
    for ws in [w.id, other.id] {
        other
            .admin
            .post(&erase_path(ws, c))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    assert_eq!(count(&pool, contacts, c).await, 1);
    // A contact that does not exist is a 404 too, and writes no tombstone.
    w.admin
        .post(&erase_path(w.id, Uuid::now_v7()))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(count(&pool, tombstones, w.id).await, 0);
}

#[sqlx::test]
async fn no_route_lifts_an_erasure_tombstone(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, ..) = mailable_contact(&w, ADDRESS).await;
    w.admin
        .post(&erase_path(w.id, c))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let id: Uuid = sqlx::query_scalar("select id from suppressions where workspace_id = $1")
        .bind(w.id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let r = w
        .admin
        .post(&format!(
            "/api/v1/workspaces/{}/suppressions/{id}/lift",
            w.id
        ))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:erasure-not-liftable");
    let live = "select count(*) from suppressions where id = $1 and lifted_at is null";
    assert_eq!(count(&pool, live, id).await, 1);
}

/// The audit log outlives the contact and every workspace admin reads it: the address in it would
/// mean nothing was erased.
#[sqlx::test]
async fn erase_is_audited_without_the_address(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, ..) = mailable_contact(&w, ADDRESS).await;
    w.admin
        .post(&erase_path(w.id, c))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let (target_id, metadata): (Option<String>, Value) =
        sqlx::query_as("select target_id, metadata from audit_log where action = 'contact.erased'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(target_id, Some(c.to_string()));
    assert_eq!(metadata, json!({}));
    // The whole table as text: neither the address nor its local part nor its domain.
    let everything: String =
        sqlx::query_scalar("select coalesce(string_agg(audit_log::text, ' '), '') from audit_log")
            .fetch_one(&pool)
            .await
            .unwrap();
    let everything = everything.to_lowercase();
    assert!(everything.contains("contact.erased"));
    assert!(
        !everything.contains("gone.person"),
        "address in audit: {everything}"
    );
    assert!(
        !everything.contains("example.test"),
        "domain in audit: {everything}"
    );
}

/// A plain delete is the editor's tool for a typo: it blocks nothing.
#[sqlx::test]
async fn a_plain_delete_writes_no_tombstone_and_the_address_is_free(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, ..) = mailable_contact(&w, ADDRESS).await;
    w.editor
        .delete(&format!("/api/v1/workspaces/{}/contacts/{c}", w.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let tombstones = "select count(*) from suppressions where workspace_id = $1";
    assert_eq!(count(&pool, tombstones, w.id).await, 0);
    create_contact(&w.editor, w.id, ADDRESS)
        .await
        .assert_status(StatusCode::CREATED);
}

/// Erasing an address that already has a live block must not collide with it, and must not leave
/// the block's plaintext behind: the tombstone replaces it.
#[sqlx::test]
async fn erasing_a_blocked_address_replaces_the_block_and_keeps_no_plaintext(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let (c, ..) = mailable_contact(&w, ADDRESS).await;
    w.admin
        .post(&format!("/api/v1/workspaces/{}/suppressions", w.id))
        .json(&json!({"scope": "address", "email": ADDRESS, "reason": "complained"}))
        .await
        .assert_status(StatusCode::CREATED);
    w.admin
        .post(&erase_path(w.id, c))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let rows: Vec<(Option<String>, String)> =
        sqlx::query_as("select email, reason from suppressions where workspace_id = $1")
            .bind(w.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![(None, "gdpr_erasure".to_owned())]);
}

/// An erasure is one workspace's. The boundary is the `workspace_id = $1 or workspace_id is null`
/// clause in `is_erased`: over-reaching would let one tenant's erasure block another tenant's
/// legitimate contact.
#[sqlx::test]
async fn an_erasure_does_not_block_the_same_address_in_another_workspace(pool: PgPool) {
    let a = workspace(&pool, "acme").await;
    let b = workspace(&pool, "other").await;
    let (c, ..) = mailable_contact(&a, ADDRESS).await;
    a.admin
        .post(&erase_path(a.id, c))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    create_contact(&b.editor, b.id, ADDRESS)
        .await
        .assert_status(StatusCode::CREATED);
    // And still refused in the workspace that erased it.
    create_contact(&a.editor, a.id, ADDRESS)
        .await
        .assert_status(StatusCode::CONFLICT);
}
