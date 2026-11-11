// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
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

fn tags_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/tags")
}

fn applied_path(ws: Uuid, contact: Uuid, tag: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/contacts/{contact}/tags/{tag}")
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

async fn create_tag(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    let r = app.post(&tags_path(ws)).json(&json!({"name": name})).await;
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

async fn count_applied(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from contact_tags")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn applying_a_tag_twice_is_idempotent(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let tag = create_tag(&app, ws, "vip").await;
    let contact = create_contact(&app, ws, "a@ex.test").await;
    let url = applied_path(ws, contact, tag);
    app.put(&url).await.assert_status(StatusCode::NO_CONTENT);
    app.put(&url).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(count_applied(&pool).await, 1);
    // Only the first application is an event worth auditing.
    let audited: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'tag.applied'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audited, 1);

    // Removing takes it off, and a second removal says it was not there.
    app.delete(&url).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(count_applied(&pool).await, 0);
    app.delete(&url).await.assert_status_not_found();
}

#[sqlx::test]
async fn deleting_a_tag_removes_it_from_contacts(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let tag = create_tag(&app, ws, "vip").await;
    let keep = create_tag(&app, ws, "keep").await;
    let a = create_contact(&app, ws, "a@ex.test").await;
    let b = create_contact(&app, ws, "b@ex.test").await;
    for (c, t) in [(a, tag), (b, tag), (a, keep)] {
        app.put(&applied_path(ws, c, t))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    assert_eq!(count_applied(&pool).await, 3);
    app.delete(&format!("{}/{tag}", tags_path(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // The cascade took only that tag's rows; the contacts and the other tag are untouched.
    assert_eq!(count_applied(&pool).await, 1);
    let left: Uuid = sqlx::query_scalar("select tag_id from contact_tags")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, keep);
    let contacts: i64 = sqlx::query_scalar("select count(*) from contacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(contacts, 2);
}

#[sqlx::test]
async fn duplicate_tag_name_is_conflict(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    create_tag(&app, ws, "vip").await;
    let r = app.post(&tags_path(ws)).json(&json!({"name": "vip"})).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:tag-name-taken");

    // Renaming onto an existing name is the same conflict.
    let other = create_tag(&app, ws, "other").await;
    let r = app
        .patch(&format!("{}/{other}", tags_path(ws)))
        .json(&json!({"name": "vip"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:tag-name-taken");

    // The name is unique per workspace, not globally.
    let (ws2, app2) = workspace(&pool, "globex").await;
    create_tag(&app2, ws2, "vip").await;
}

#[sqlx::test]
async fn cross_workspace_tag_apply_is_404(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "globex").await;
    let tag_a = create_tag(&app_a, ws_a, "vip").await;
    let contact_a = create_contact(&app_a, ws_a, "a@ex.test").await;
    let tag_b = create_tag(&app_b, ws_b, "vip").await;
    let contact_b = create_contact(&app_b, ws_b, "b@ex.test").await;

    // B's editor reaching into A's workspace (not a member), then A's tag or contact through
    // their own workspace (a member, but the id is not in it).
    for url in [
        applied_path(ws_a, contact_a, tag_a),
        applied_path(ws_b, contact_b, tag_a),
        applied_path(ws_b, contact_a, tag_b),
        applied_path(ws_b, contact_a, tag_a),
    ] {
        app_b.put(&url).await.assert_status_not_found();
    }
    app_b
        .patch(&format!("{}/{tag_a}", tags_path(ws_b)))
        .json(&json!({"name": "x"}))
        .await
        .assert_status_not_found();
    app_b
        .delete(&format!("{}/{tag_a}", tags_path(ws_b)))
        .await
        .assert_status_not_found();
    assert_eq!(count_applied(&pool).await, 0);
    // A's tag is untouched.
    let names = app_a.get(&tags_path(ws_a)).await.json::<Value>();
    assert_eq!(names["data"][0]["name"], "vip");
}

#[sqlx::test]
async fn cross_workspace_tag_is_rejected_by_the_database(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "globex").await;
    let contact_a = create_contact(&app_a, ws_a, "a@ex.test").await;
    let tag_a = create_tag(&app_a, ws_a, "vip").await;
    let tag_b = create_tag(&app_b, ws_b, "vip").await;

    // No handler below: the claim is that the schema refuses, not that a route checks.
    let insert = |workspace: Uuid, contact: Uuid, tag: Uuid| {
        sqlx::query(
            "insert into contact_tags (workspace_id, contact_id, tag_id) values ($1, $2, $3)",
        )
        .bind(workspace)
        .bind(contact)
        .bind(tag)
        .execute(&pool)
    };
    let refusal = |result: sqlx::Result<sqlx::postgres::PgQueryResult>| {
        let err = result.expect_err("a cross-workspace tag must not be writable");
        let db = err.as_database_error().expect("a database error");
        (
            db.kind(),
            db.code().unwrap_or_default().into_owned(),
            db.constraint().unwrap_or_default().to_owned(),
        )
    };

    // Control: the same statement succeeds within one workspace, so the refusals below are about
    // tenancy and not a typo in the insert.
    insert(ws_a, contact_a, tag_a)
        .await
        .expect("a same-workspace tag is writable");
    sqlx::query("delete from contact_tags")
        .execute(&pool)
        .await
        .unwrap();

    // A's contact with B's tag, claiming workspace A: the contact's key holds, the tag's fails.
    let (kind, code, constraint) = refusal(insert(ws_a, contact_a, tag_b).await);
    assert_eq!(kind, sqlx::error::ErrorKind::ForeignKeyViolation);
    assert_eq!(code, "23503");
    assert_eq!(constraint, "contact_tags_workspace_id_tag_id_fkey");
    // The same pair claiming workspace B: now the tag's key holds and the contact's fails.
    let (kind, code, constraint) = refusal(insert(ws_b, contact_a, tag_b).await);
    assert_eq!(kind, sqlx::error::ErrorKind::ForeignKeyViolation);
    assert_eq!(code, "23503");
    assert_eq!(constraint, "contact_tags_workspace_id_contact_id_fkey");

    assert_eq!(count_applied(&pool).await, 0);
}

#[sqlx::test]
async fn viewer_cannot_apply_a_tag(pool: PgPool) {
    let (ws, editor) = workspace(&pool, "acme").await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    let tag = create_tag(&editor, ws, "vip").await;
    let contact = create_contact(&editor, ws, "a@ex.test").await;
    let applied = create_tag(&editor, ws, "applied").await;
    editor
        .put(&applied_path(ws, contact, applied))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // A viewer may read.
    let r = viewer.get(&tags_path(ws)).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["data"].as_array().unwrap().len(), 2);

    // Every write route refuses a viewer with `insufficient-role`. Each request is one that would
    // succeed for an editor, so the 403 can only be the role.
    let forbidden = |r: axum_test::TestResponse| {
        r.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    };
    forbidden(
        viewer
            .post(&tags_path(ws))
            .json(&json!({"name": "new"}))
            .await,
    );
    forbidden(
        viewer
            .patch(&format!("{}/{tag}", tags_path(ws)))
            .json(&json!({"name": "renamed"}))
            .await,
    );
    forbidden(viewer.delete(&format!("{}/{tag}", tags_path(ws))).await);
    forbidden(viewer.put(&applied_path(ws, contact, tag)).await);
    forbidden(viewer.delete(&applied_path(ws, contact, applied)).await);

    // And none of it took effect.
    let names: Vec<String> = sqlx::query_scalar("select name from tags order by name")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(names, ["applied", "vip"]);
    assert_eq!(count_applied(&pool).await, 1);
}

#[sqlx::test]
async fn tag_names_are_validated_and_listed(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    for body in [
        json!({"name": "  "}),
        json!({"name": "a\nb"}),
        json!({"name": "ok", "color": "red"}),
        json!({"name": "ok", "status": "active"}),
    ] {
        app.post(&tags_path(ws))
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let a = create_tag(&app, ws, "  first ").await;
    let b = create_tag(&app, ws, "second").await;
    let r = app.get(&tags_path(ws)).await.json::<Value>();
    let rows = r["data"].as_array().unwrap();
    // Newest first, and a tag is a name and nothing more: no subscription state.
    assert_eq!(rows[0]["id"], b.to_string());
    assert_eq!(rows[1]["name"], "first");
    let mut keys: Vec<&String> = rows[1].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["created_at", "id", "name"]);
    let r = app
        .patch(&format!("{}/{a}", tags_path(ws)))
        .json(&json!({"name": "renamed"}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["name"], "renamed");
}

#[sqlx::test]
async fn tag_actions_are_audited_without_personal_data(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let tag = create_tag(&app, ws, "vip").await;
    let contact = create_contact(&app, ws, "pii.person@ex.test").await;
    let url = applied_path(ws, contact, tag);
    app.put(&url).await.assert_status(StatusCode::NO_CONTENT);
    app.delete(&url).await.assert_status(StatusCode::NO_CONTENT);
    app.patch(&format!("{}/{tag}", tags_path(ws)))
        .json(&json!({"name": "gold"}))
        .await
        .assert_status_ok();
    app.delete(&format!("{}/{tag}", tags_path(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let rows: Vec<(String, Option<String>, Option<String>, Value)> = sqlx::query_as(
        "select action, target_type, target_id, metadata from audit_log \
         where workspace_id = $1 and action like 'tag.%' order by created_at, id",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    let (tag_id, contact_id) = (Some(tag.to_string()), contact.to_string());
    let target = |action: &str, metadata: Value| {
        (
            action.to_owned(),
            Some("tag".to_owned()),
            tag_id.clone(),
            metadata,
        )
    };
    assert_eq!(
        rows,
        [
            target("tag.created", json!({"name": "vip"})),
            target("tag.applied", json!({"contact_id": contact_id})),
            target("tag.removed", json!({"contact_id": contact_id})),
            target("tag.updated", json!({"name": "gold"})),
            target("tag.deleted", json!({})),
        ]
    );
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
