// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::lists::MembershipStatus;
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in editor of it.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let editor = client(
        pool,
        &format!("editor-{slug}@example.com"),
        ws,
        Role::Editor,
    )
    .await;
    (ws, editor)
}

fn lists_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/lists")
}

fn members_path(ws: Uuid, list: Uuid) -> String {
    format!("{}/{list}/members", lists_path(ws))
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn create_list(app: &TestApp, ws: Uuid, name: &str, double_optin: bool) -> Uuid {
    let r = app
        .post(&lists_path(ws))
        .json(&json!({"name": name, "double_optin": double_optin}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn create_contact(app: &TestApp, ws: Uuid, email: &str) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/contacts"))
        .json(&json!({"email": email}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// Adds the contact and returns the status the API reports.
async fn add(app: &TestApp, ws: Uuid, list: Uuid, contact: Uuid) -> String {
    let r = app
        .post(&members_path(ws, list))
        .json(&json!({"contact_id": contact}))
        .await;
    r.assert_status_ok();
    r.json::<Value>()["status"].as_str().unwrap().to_owned()
}

async fn remove(app: &TestApp, ws: Uuid, list: Uuid, contact: Uuid) -> axum_test::TestResponse {
    app.delete(&format!("{}/{contact}", members_path(ws, list)))
        .await
}

/// `(status, subscribed_at is set, unsubscribed_at is set)` of the one membership row, read
/// straight from the table.
async fn row(pool: &PgPool, list: Uuid, contact: Uuid) -> (String, bool, bool) {
    sqlx::query_as(
        "select status, subscribed_at is not null, unsubscribed_at is not null \
         from list_memberships where list_id = $1 and contact_id = $2",
    )
    .bind(list)
    .bind(contact)
    .fetch_one(pool)
    .await
    .expect("the membership row exists")
}

async fn count_memberships(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from list_memberships")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

#[sqlx::test]
async fn single_optin_add_is_subscribed_immediately(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    let contact = create_contact(&app, ws, "a@ex.test").await;
    assert_eq!(add(&app, ws, list, contact).await, "subscribed");
    // Written as `subscribed` and stamped, not `pending` resolved later.
    assert_eq!(
        row(&pool, list, contact).await,
        ("subscribed".into(), true, false)
    );
}

#[sqlx::test]
async fn double_optin_add_is_pending(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    // Double opt-in is the default.
    let r = app
        .post(&lists_path(ws))
        .json(&json!({"name": "news"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(r.json::<Value>()["double_optin"], true);
    let list = id_of(&r);
    let contact = create_contact(&app, ws, "a@ex.test").await;
    assert_eq!(add(&app, ws, list, contact).await, "pending");
    assert_eq!(
        row(&pool, list, contact).await,
        ("pending".into(), false, false)
    );

    // Adding again does nothing to a member: here, a confirmed one is not demoted to pending.
    sqlx::query("update list_memberships set status = 'subscribed' where list_id = $1")
        .bind(list)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(add(&app, ws, list, contact).await, "subscribed");
    assert_eq!(row(&pool, list, contact).await.0, "subscribed");
}

#[sqlx::test]
async fn pending_is_never_counted_as_mailable(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", true).await;
    for email in ["a@ex.test", "b@ex.test"] {
        let c = create_contact(&app, ws, email).await;
        add(&app, ws, list, c).await;
    }
    let mailable: i64 = sqlx::query_scalar(
        "select count(*) from list_memberships where list_id = $1 and status = 'subscribed'",
    )
    .bind(list)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mailable, 0);

    let r = app.get(&members_path(ws, list)).await;
    r.assert_status_ok();
    let page: Value = r.json();
    let members = page["data"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    for m in members {
        let status: MembershipStatus = serde_json::from_value(m["status"].clone()).unwrap();
        assert_eq!(status, MembershipStatus::Pending);
        assert!(!status.is_mailable());
    }
}

#[sqlx::test]
async fn unsubscribe_sets_status_and_timestamp(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    let contact = create_contact(&app, ws, "a@ex.test").await;
    add(&app, ws, list, contact).await;

    remove(&app, ws, list, contact)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // The row is kept: that they unsubscribed is the fact worth keeping.
    assert_eq!(
        row(&pool, list, contact).await,
        ("unsubscribed".into(), true, true)
    );
    let page: Value = app.get(&members_path(ws, list)).await.json();
    assert_eq!(page["data"][0]["status"], "unsubscribed");
    assert!(page["data"][0]["unsubscribed_at"].is_string());

    // Doing it again keeps the first timestamp.
    let stamp = || async {
        sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
            "select unsubscribed_at from list_memberships where list_id = $1",
        )
        .bind(list)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let first = stamp().await;
    remove(&app, ws, list, contact)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(stamp().await, first);

    // Someone who was never on the list has nothing to unsubscribe from.
    let other = create_contact(&app, ws, "b@ex.test").await;
    remove(&app, ws, list, other)
        .await
        .assert_status_not_found();
    assert_eq!(count_memberships(&pool).await, 1);
}

#[sqlx::test]
async fn resubscribe_after_unsubscribe_is_allowed(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let single = create_list(&app, ws, "single", false).await;
    let double = create_list(&app, ws, "double", true).await;
    let contact = create_contact(&app, ws, "a@ex.test").await;
    for (list, again) in [(single, "subscribed"), (double, "pending")] {
        add(&app, ws, list, contact).await;
        remove(&app, ws, list, contact)
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(row(&pool, list, contact).await.0, "unsubscribed");
        // The same rule as a first add: `subscribed` at once on a single-opt-in list, `pending`
        // on a double-opt-in one.
        assert_eq!(add(&app, ws, list, contact).await, again);
        let (status, _, unsubscribed_at_set) = row(&pool, list, contact).await;
        assert_eq!(status, again);
        assert!(
            !unsubscribed_at_set,
            "unsubscribed_at is set only while unsubscribed"
        );
    }
}

#[sqlx::test]
async fn duplicate_list_name_is_conflict(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    create_list(&app, ws, "news", true).await;
    let r = app
        .post(&lists_path(ws))
        .json(&json!({"name": "news"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:list-name-taken");

    // Renaming onto an existing name is the same conflict.
    let other = create_list(&app, ws, "other", true).await;
    let r = app
        .patch(&format!("{}/{other}", lists_path(ws)))
        .json(&json!({"name": "news"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);

    // The name is unique per workspace, not globally.
    let (ws2, app2) = workspace(&pool, "globex").await;
    create_list(&app2, ws2, "news", true).await;
}

#[sqlx::test]
async fn cross_workspace_membership_is_rejected_by_the_database(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "globex").await;
    let contact_a = create_contact(&app_a, ws_a, "a@ex.test").await;
    let list_a = create_list(&app_a, ws_a, "news", false).await;
    let list_b = create_list(&app_b, ws_b, "news", false).await;

    // No handler below: the claim is that the schema refuses, not that a route checks.
    let insert = |workspace: Uuid, contact: Uuid, list: Uuid| {
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status) \
             values ($1, $2, $3, 'subscribed')",
        )
        .bind(workspace)
        .bind(contact)
        .bind(list)
        .execute(&pool)
    };
    // The kind and the constraint name of the database's refusal.
    let refusal = |result: sqlx::Result<sqlx::postgres::PgQueryResult>| {
        let err = result.expect_err("a cross-workspace membership must not be writable");
        let db = err.as_database_error().expect("a database error");
        (db.kind(), db.constraint().unwrap_or_default().to_owned())
    };

    // Control: the same statement succeeds when contact and list share a workspace, so the
    // refusals below are about tenancy and not a typo in the insert.
    insert(ws_a, contact_a, list_a)
        .await
        .expect("a same-workspace membership is writable");
    sqlx::query("delete from list_memberships")
        .execute(&pool)
        .await
        .unwrap();

    // A's contact on B's list, claiming workspace A: the contact's key holds, the list's fails.
    let (kind, constraint) = refusal(insert(ws_a, contact_a, list_b).await);
    assert_eq!(kind, sqlx::error::ErrorKind::ForeignKeyViolation);
    assert_eq!(constraint, "list_memberships_workspace_id_list_id_fkey");
    // The same pair claiming workspace B: now the list's key holds and the contact's fails.
    let (kind, constraint) = refusal(insert(ws_b, contact_a, list_b).await);
    assert_eq!(kind, sqlx::error::ErrorKind::ForeignKeyViolation);
    assert_eq!(constraint, "list_memberships_workspace_id_contact_id_fkey");

    assert_eq!(count_memberships(&pool).await, 0);
}

#[sqlx::test]
async fn cross_workspace_list_get_is_404(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "globex").await;
    let list_a = create_list(&app_a, ws_a, "news", false).await;
    let contact_a = create_contact(&app_a, ws_a, "a@ex.test").await;
    let list_b = create_list(&app_b, ws_b, "news", false).await;
    let one = |ws: Uuid, id: Uuid| format!("{}/{id}", lists_path(ws));

    // B's editor asking for A's list: through A's workspace (not a member) and through their own
    // (a member, but the list is not in that workspace).
    app_b
        .get(&one(ws_a, list_a))
        .await
        .assert_status_not_found();
    app_b
        .get(&one(ws_b, list_a))
        .await
        .assert_status_not_found();
    app_b
        .patch(&one(ws_b, list_a))
        .json(&json!({"name": "x"}))
        .await
        .assert_status_not_found();
    app_b
        .delete(&one(ws_b, list_a))
        .await
        .assert_status_not_found();
    app_b
        .get(&members_path(ws_b, list_a))
        .await
        .assert_status_not_found();
    // Through the API, a contact of A cannot be put on B's list either.
    app_b
        .post(&members_path(ws_b, list_b))
        .json(&json!({"contact_id": contact_a}))
        .await
        .assert_status_not_found();
    assert_eq!(count_memberships(&pool).await, 0);
    // And A's list is untouched.
    app_a.get(&one(ws_a, list_a)).await.assert_status_ok();
}

#[sqlx::test]
async fn roles_and_validation(pool: PgPool) {
    let (ws, editor) = workspace(&pool, "acme").await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    let list = create_list(&editor, ws, "news", true).await;
    let one = format!("{}/{list}", lists_path(ws));

    viewer.get(&lists_path(ws)).await.assert_status_ok();
    viewer.get(&one).await.assert_status_ok();
    viewer
        .post(&lists_path(ws))
        .json(&json!({"name": "x"}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    viewer
        .delete(&one)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Every write route refuses a viewer with `insufficient-role`. Each request is one that would
    // succeed for an editor, so the 403 can only be the role. The member is already on the list,
    // so a removal that got through would show in the row.
    let member = create_contact(&editor, ws, "m@ex.test").await;
    let outsider = create_contact(&editor, ws, "o@ex.test").await;
    add(&editor, ws, list, member).await;
    let forbidden = |r: axum_test::TestResponse| {
        r.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    };
    forbidden(viewer.patch(&one).json(&json!({"name": "renamed"})).await);
    forbidden(
        viewer
            .post(&members_path(ws, list))
            .json(&json!({"contact_id": outsider}))
            .await,
    );
    forbidden(
        viewer
            .delete(&format!("{}/{member}", members_path(ws, list)))
            .await,
    );
    let by_contact = format!("/api/v1/workspaces/{ws}/contacts/{member}/lists");
    forbidden(viewer.delete(&format!("{by_contact}/{list}")).await);
    // And none of it took effect.
    assert_eq!(editor.get(&one).await.json::<Value>()["name"], "news");
    assert_eq!(count_memberships(&pool).await, 1);
    assert_eq!(row(&pool, list, member).await.0, "pending");

    // A blank name, and an unknown field.
    for body in [json!({"name": "  "}), json!({"name": "ok", "slug": "x"})] {
        editor
            .post(&lists_path(ws))
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[sqlx::test]
async fn double_optin_can_be_switched_off_and_pending_rows_stay_pending(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", true).await;
    let waiting = create_contact(&app, ws, "a@ex.test").await;
    let newcomer = create_contact(&app, ws, "b@ex.test").await;
    add(&app, ws, list, waiting).await;
    let url = format!("{}/{list}", lists_path(ws));

    // 200, not a refusal: with no confirmation flow before M1c, a pending row could otherwise
    // only be cleared one at a time, and the setting could never be turned off.
    app.patch(&url)
        .json(&json!({"double_optin": false}))
        .await
        .assert_status_ok();
    // The row that was pending is left exactly as it was: still pending, still not mailable.
    assert_eq!(
        row(&pool, list, waiting).await,
        ("pending".into(), false, false)
    );
    // A new member of the now single-opt-in list is subscribed at once.
    assert_eq!(add(&app, ws, list, newcomer).await, "subscribed");
    // Adding the pending one again leaves it pending: only an unsubscribed row is rewritten.
    assert_eq!(add(&app, ws, list, waiting).await, "pending");
}

/// `(action, target_type, target_id, metadata)` of the list audit rows, oldest first.
async fn list_audit(
    pool: &PgPool,
    ws: Uuid,
) -> Vec<(String, Option<String>, Option<String>, Value)> {
    sqlx::query_as(
        "select action, target_type, target_id, metadata from audit_log \
         where workspace_id = $1 and action like 'list.%' order by created_at, id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn list_actions_are_audited_without_personal_data(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    let contact = create_contact(&app, ws, "pii.person@ex.test").await;
    add(&app, ws, list, contact).await;
    remove(&app, ws, list, contact).await;
    // The contact-side routes write the same rows.
    let by_contact = format!("/api/v1/workspaces/{ws}/contacts/{contact}/lists");
    app.post(&by_contact)
        .json(&json!({"list_id": list}))
        .await
        .assert_status_ok();
    app.delete(&format!("{by_contact}/{list}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.patch(&format!("{}/{list}", lists_path(ws)))
        .json(&json!({"visibility": "public", "description": "hello"}))
        .await
        .assert_status_ok();
    app.delete(&format!("{}/{list}", lists_path(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let (list_id, contact_id) = (Some(list.to_string()), contact.to_string());
    let target = |action: &str, metadata: Value| {
        (
            action.to_owned(),
            Some("list".to_owned()),
            list_id.clone(),
            metadata,
        )
    };
    assert_eq!(
        list_audit(&pool, ws).await,
        [
            target("list.created", json!({"name": "news"})),
            target(
                "list.member_added",
                json!({"contact_id": contact_id, "status": "subscribed"})
            ),
            target("list.member_removed", json!({"contact_id": contact_id})),
            target(
                "list.member_added",
                json!({"contact_id": contact_id, "status": "subscribed"})
            ),
            target("list.member_removed", json!({"contact_id": contact_id})),
            target(
                "list.updated",
                json!({"fields": ["description", "visibility"]})
            ),
            target("list.deleted", json!({})),
        ]
    );
    // The address is personal data an erasure must be able to remove; the audit log is not erased.
    let all = sqlx::query_scalar::<_, String>("select metadata::text from audit_log")
        .fetch_all(&pool)
        .await
        .unwrap()
        .join(" ");
    assert!(
        !all.contains("pii.person"),
        "an address reached the audit log"
    );
}

#[sqlx::test]
async fn a_contacts_lists_can_be_read_and_changed_from_the_contact(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    let single = create_list(&app, ws, "single", false).await;
    let double = create_list(&app, ws, "double", true).await;
    let contact = create_contact(&app, ws, "a@ex.test").await;
    let path = format!("/api/v1/workspaces/{ws}/contacts/{contact}/lists");

    assert!(
        app.get(&path).await.json::<Value>()["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for (list, status) in [(single, "subscribed"), (double, "pending")] {
        let r = app.post(&path).json(&json!({"list_id": list})).await;
        r.assert_status_ok();
        let body: Value = r.json();
        assert_eq!(body["status"], status);
        assert_eq!(body["list_id"], list.to_string());
    }
    // The same rows the list-side routes see.
    assert_eq!(row(&pool, single, contact).await.0, "subscribed");

    // Newest list first, with the name and status: what the contact page shows.
    let page: Value = app.get(&format!("{path}?limit=1")).await.json();
    assert_eq!(page["data"][0]["name"], "double");
    assert_eq!(page["data"][0]["status"], "pending");
    let cursor = page["next_cursor"].as_str().unwrap();
    let page: Value = app
        .get(&format!("{path}?limit=1&cursor={cursor}"))
        .await
        .json();
    assert_eq!(page["data"][0]["list_id"], single.to_string());
    assert!(page["next_cursor"].is_null());

    app.delete(&format!("{path}/{single}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        row(&pool, single, contact).await,
        ("unsubscribed".into(), true, true)
    );
    // Never on that list: nothing to remove.
    let other = create_list(&app, ws, "other", false).await;
    app.delete(&format!("{path}/{other}"))
        .await
        .assert_status_not_found();

    // Viewers read but do not write.
    viewer.get(&path).await.assert_status_ok();
    viewer
        .post(&path)
        .json(&json!({"list_id": other}))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Another workspace's list, and another workspace's contact, are 404.
    let (ws_b, app_b) = workspace(&pool, "globex").await;
    let list_b = create_list(&app_b, ws_b, "b", false).await;
    app.post(&path)
        .json(&json!({"list_id": list_b}))
        .await
        .assert_status_not_found();
    app_b
        .get(&format!(
            "/api/v1/workspaces/{ws_b}/contacts/{contact}/lists"
        ))
        .await
        .assert_status_not_found();
    app_b
        .post(&format!(
            "/api/v1/workspaces/{ws_b}/contacts/{contact}/lists"
        ))
        .json(&json!({"list_id": list_b}))
        .await
        .assert_status_not_found();
}

#[sqlx::test]
async fn deleting_a_list_removes_its_memberships_not_the_contacts(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    let contact = create_contact(&app, ws, "a@ex.test").await;
    add(&app, ws, list, contact).await;
    app.delete(&format!("{}/{list}", lists_path(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let contacts: i64 = sqlx::query_scalar("select count(*) from contacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((count_memberships(&pool).await, contacts), (0, 1));
}
