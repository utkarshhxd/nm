// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// A signed-in client for a fresh user, who is `role` in `ws` (or in no workspace at all).
async fn client(pool: &PgPool, email: &str, ws: Option<(Uuid, Role)>) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    if let Some((ws, role)) = ws {
        add_member(pool, ws, id, role).await;
    }
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in admin of it.
async fn workspace_with_admin(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let admin = client(
        pool,
        &format!("admin-{slug}@example.com"),
        Some((ws, Role::Admin)),
    )
    .await;
    (ws, admin)
}

fn fields_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/custom-fields")
}

/// Creates a field and returns its id.
async fn create_field(app: &TestApp, ws: Uuid, body: Value) -> Uuid {
    let r = app.post(&fields_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CREATED);
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

fn first_error(r: &axum_test::TestResponse) -> Value {
    r.json::<Value>()["errors"][0].clone()
}

async fn count_fields(pool: &PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from custom_fields where workspace_id = $1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn audit_actions(pool: &PgPool, ws: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "select action from audit_log where workspace_id = $1 and action like 'custom_field.%' \
         order by id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn create_rejects_a_bad_key(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    for bad in ["Not A Key", "", "1abc", "has-dash", &"a".repeat(64)] {
        let r = admin
            .post(&fields_path(ws))
            .json(&json!({"key": bad, "label": "L", "kind": "text"}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(first_error(&r)["field"], "key", "{bad:?}");
    }
    assert_eq!(count_fields(&pool, ws).await, 0);

    let r = admin
        .post(&fields_path(ws))
        .json(&json!({"key": "company_size", "label": " Company size ", "kind": "text"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let f = r.json::<Value>();
    assert_eq!(f["key"], "company_size");
    assert_eq!(f["label"], "Company size");
    assert_eq!(f["kind"], "text");
    assert_eq!(f["options"], json!([]));
    assert_eq!(f["archived_at"], Value::Null);
}

#[sqlx::test]
async fn create_rejects_select_without_options(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    for kind in ["select", "multi_select"] {
        for options in [json!([]), json!(["a", "a"]), json!([" "])] {
            let r = admin
                .post(&fields_path(ws))
                .json(&json!({"key": "tier", "label": "Tier", "kind": kind, "options": options}))
                .await;
            r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(first_error(&r)["field"], "options", "{kind} {options}");
        }
        // Omitting `options` is the same as an empty list.
        let r = admin
            .post(&fields_path(ws))
            .json(&json!({"key": "tier", "label": "Tier", "kind": kind}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(first_error(&r)["field"], "options");
    }
    // A text field takes no options.
    let r = admin
        .post(&fields_path(ws))
        .json(&json!({"key": "tier", "label": "Tier", "kind": "text", "options": ["a"]}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error(&r)["field"], "options");
    assert_eq!(count_fields(&pool, ws).await, 0);

    let r = admin
        .post(&fields_path(ws))
        .json(
            &json!({"key": "tier", "label": "Tier", "kind": "multi_select", "options": ["a", "b"]}),
        )
        .await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(r.json::<Value>()["options"], json!(["a", "b"]));
}

#[sqlx::test]
async fn create_is_admin_only(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    let body = json!({"key": "plan", "label": "Plan", "kind": "text"});
    for (email, role) in [
        ("editor@example.com", Role::Editor),
        ("viewer@example.com", Role::Viewer),
    ] {
        let app = client(&pool, email, Some((ws, role))).await;
        let r = app.post(&fields_path(ws)).json(&body).await;
        r.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    }
    assert_eq!(count_fields(&pool, ws).await, 0);

    // Reading needs only viewer.
    let id = create_field(&admin, ws, body).await;
    let viewer = client(&pool, "reader@example.com", Some((ws, Role::Viewer))).await;
    let r = viewer.get(&fields_path(ws)).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["data"][0]["id"], id.to_string());
}

#[sqlx::test]
async fn duplicate_key_is_conflict(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    let body = json!({"key": "plan", "label": "Plan", "kind": "text"});
    let id = create_field(&admin, ws, body.clone()).await;
    let r = admin.post(&fields_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:key-taken");

    // An archived field still holds its key: its values are still stored under it.
    admin
        .delete(&format!("{}/{id}", fields_path(ws)))
        .await
        .assert_status_ok();
    let r = admin.post(&fields_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(count_fields(&pool, ws).await, 1);

    // Keys are per workspace.
    let (other, other_admin) = workspace_with_admin(&pool, "other").await;
    create_field(&other_admin, other, body).await;
}

#[sqlx::test]
async fn delete_archives_the_field(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    let id = create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;

    let r = admin.delete(&format!("{}/{id}", fields_path(ws))).await;
    r.assert_status_ok();
    let body = r.json::<Value>();
    assert!(body["field"]["archived_at"].is_string());
    assert!(body["note"].as_str().unwrap().contains("keeps its value"));

    // The row is still there, archived, and the listing says so.
    assert_eq!(count_fields(&pool, ws).await, 1);
    let list = admin.get(&fields_path(ws)).await.json::<Value>();
    assert_eq!(list["data"][0]["id"], id.to_string());
    assert!(list["data"][0]["archived_at"].is_string());

    // Archiving again keeps the first timestamp.
    let again = admin
        .delete(&format!("{}/{id}", fields_path(ws)))
        .await
        .json::<Value>();
    assert_eq!(again["field"]["archived_at"], body["field"]["archived_at"]);

    assert_eq!(
        audit_actions(&pool, ws).await,
        [
            "custom_field.created",
            "custom_field.archived",
            "custom_field.archived"
        ]
    );

    // `delete_archives_and_keeps_values`: that after DELETE a contact's value is still readable,
    // needs a contact to hold one, so it lives in `tests/contacts.rs`.
}

#[sqlx::test]
async fn purge_refuses_unless_archived(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    let id = create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    let purge = format!("{}/{id}/purge", fields_path(ws));

    let r = admin.post(&purge).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:field-not-archived");
    assert_eq!(audit_actions(&pool, ws).await, ["custom_field.created"]);

    // Purge is admin-only, and the role check comes before anything else about the field.
    let editor = client(&pool, "editor@example.com", Some((ws, Role::Editor))).await;
    let r = editor.post(&purge).await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");

    // Purge's success path, which strips the key from every contact's `attributes` and counts
    // them, needs contacts; it is tested in `tests/contacts.rs` with the contacts table.
}

#[sqlx::test]
async fn patch_rejects_a_kind_change(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    let id = create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    let path = format!("{}/{id}", fields_path(ws));

    let r = admin.patch(&path).json(&json!({"kind": "number"})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let e = first_error(&r);
    assert_eq!(e["field"], "kind");
    assert_eq!(e["message"], "changing a field's type is not supported yet");
    // A rejected kind change takes the rest of the request with it.
    let r = admin
        .patch(&path)
        .json(&json!({"kind": "number", "label": "Renamed"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    // The key is immutable: it is not a field of the update at all.
    let r = admin.patch(&path).json(&json!({"key": "other"})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    let row: (String, String, String) =
        sqlx::query_as("select key, kind, label from custom_fields where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("plan".into(), "text".into(), "Plan".into()));

    // Restating the current kind is not a change; the label does change.
    let r = admin
        .patch(&path)
        .json(&json!({"kind": "text", "label": "Billing plan"}))
        .await;
    r.assert_status_ok();
    let f = r.json::<Value>();
    assert_eq!(f["label"], "Billing plan");
    assert_eq!(f["key"], "plan");

    // Options follow the field's kind.
    let r = admin.patch(&path).json(&json!({"options": ["a"]})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error(&r)["field"], "options");
    let select = create_field(
        &admin,
        ws,
        json!({"key": "tier", "label": "Tier", "kind": "select", "options": ["a"]}),
    )
    .await;
    let r = admin
        .patch(&format!("{}/{select}", fields_path(ws)))
        .json(&json!({"options": ["a", "b"]}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["options"], json!(["a", "b"]));
}

#[sqlx::test]
async fn list_is_cursor_paginated(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    let mut ids = vec![];
    for key in ["a", "b", "c"] {
        ids.push(
            create_field(
                &admin,
                ws,
                json!({"key": key, "label": key, "kind": "text"}),
            )
            .await,
        );
    }
    let first = admin
        .get(&format!("{}?limit=2", fields_path(ws)))
        .await
        .json::<Value>();
    let page: Vec<_> = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].clone())
        .collect();
    // Newest first.
    assert_eq!(page, [json!(ids[2].to_string()), json!(ids[1].to_string())]);
    let cursor = first["next_cursor"].as_str().expect("a second page exists");

    let second = admin
        .get(&format!("{}?limit=2&cursor={cursor}", fields_path(ws)))
        .await
        .json::<Value>();
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert_eq!(second["data"][0]["id"], ids[0].to_string());
    assert_eq!(second["next_cursor"], Value::Null);

    // Another workspace's fields never appear.
    let (other, other_admin) = workspace_with_admin(&pool, "other").await;
    create_field(
        &other_admin,
        other,
        json!({"key": "z", "label": "z", "kind": "text"}),
    )
    .await;
    let all = admin.get(&fields_path(ws)).await.json::<Value>();
    assert_eq!(all["data"].as_array().unwrap().len(), 3);
}

#[sqlx::test]
async fn cross_workspace_field_is_404(pool: PgPool) {
    let (ws1, admin1) = workspace_with_admin(&pool, "one").await;
    let (ws2, admin2) = workspace_with_admin(&pool, "two").await;
    let theirs = create_field(
        &admin1,
        ws1,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    let nowhere = Uuid::now_v7();

    // admin2 is an admin, so only the workspace boundary can answer 404 here. Their own workspace
    // with someone else's field id, and someone else's workspace, are both a 404, never a 403.
    let attempts = [
        ("PATCH", format!("{}/{theirs}", fields_path(ws2))),
        ("DELETE", format!("{}/{theirs}", fields_path(ws2))),
        ("POST", format!("{}/{theirs}/purge", fields_path(ws2))),
        ("PATCH", format!("{}/{theirs}", fields_path(ws1))),
        ("DELETE", format!("{}/{theirs}", fields_path(ws1))),
        ("POST", format!("{}/{theirs}/purge", fields_path(ws1))),
        ("GET", fields_path(ws1)),
        ("POST", fields_path(ws1)),
        ("GET", fields_path(nowhere)),
    ];
    for (method, path) in attempts {
        let req = match method {
            "GET" => admin2.get(&path),
            "POST" if path.ends_with("/purge") => admin2.post(&path),
            "POST" => admin2
                .post(&path)
                .json(&json!({"key": "x", "label": "x", "kind": "text"})),
            "PATCH" => admin2.patch(&path).json(&json!({"label": "hijacked"})),
            _ => admin2.delete(&path),
        };
        let r = req.await;
        r.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(
            problem_type(&r),
            "urn:nm:problem:not-found",
            "{method} {path}"
        );
    }

    // Nothing leaked and nothing changed.
    let row: (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("select label, archived_at from custom_fields where id = $1")
            .bind(theirs)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("Plan".into(), None));
    assert_eq!(count_fields(&pool, ws1).await, 1);
    assert_eq!(count_fields(&pool, ws2).await, 0);
}

#[sqlx::test]
async fn definitions_for_returns_live_and_archived_fields(pool: PgPool) {
    let (ws, admin) = workspace_with_admin(&pool, "acme").await;
    let (empty, _) = workspace_with_admin(&pool, "empty").await;
    let live = create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    let gone = create_field(
        &admin,
        ws,
        json!({"key": "tier", "label": "Tier", "kind": "select", "options": ["a", "b"]}),
    )
    .await;
    admin
        .delete(&format!("{}/{gone}", fields_path(ws)))
        .await
        .assert_status_ok();
    // Another workspace's field must not appear.
    let (other, other_admin) = workspace_with_admin(&pool, "other").await;
    create_field(
        &other_admin,
        other,
        json!({"key": "z", "label": "z", "kind": "text"}),
    )
    .await;

    let defs = nm::custom_fields::definitions_for(&pool, ws).await.unwrap();
    assert_eq!(defs.len(), 2);
    let by_id = |id: Uuid| {
        defs.iter()
            .find(|d| d.id == id)
            .expect("definition present")
    };
    assert_eq!(by_id(live).key, "plan");
    assert_eq!(by_id(live).archived_at, None);
    assert_eq!(by_id(gone).key, "tier");
    assert_eq!(by_id(gone).kind, nm::custom_fields::FieldKind::Select);
    assert_eq!(by_id(gone).options, ["a", "b"]);
    assert!(by_id(gone).archived_at.is_some());

    assert!(
        nm::custom_fields::definitions_for(&pool, empty)
            .await
            .unwrap()
            .is_empty()
    );
}
