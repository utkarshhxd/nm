// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// A signed-in client for a fresh user, who is `role` in `ws`.
async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in editor and admin of it.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let editor = client(
        pool,
        &format!("editor-{slug}@example.com"),
        ws,
        Role::Editor,
    )
    .await;
    let admin = client(pool, &format!("admin-{slug}@example.com"), ws, Role::Admin).await;
    (ws, editor, admin)
}

fn contacts_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/contacts")
}

fn fields_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/custom-fields")
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn create_field(admin: &TestApp, ws: Uuid, body: Value) -> Uuid {
    let r = admin.post(&fields_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// Creates a contact and returns its id.
async fn create_contact(app: &TestApp, ws: Uuid, body: Value) -> Uuid {
    let r = app.post(&contacts_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn get_contact(app: &TestApp, ws: Uuid, id: Uuid) -> Value {
    let r = app.get(&format!("{}/{id}", contacts_path(ws))).await;
    r.assert_status_ok();
    r.json()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

fn first_error(r: &axum_test::TestResponse) -> Value {
    r.json::<Value>()["errors"][0].clone()
}

async fn count_contacts(pool: &PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from contacts where workspace_id = $1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn archive(admin: &TestApp, ws: Uuid, field: Uuid) {
    admin
        .delete(&format!("{}/{field}", fields_path(ws)))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn create_normalises_the_address(pool: PgPool) {
    let (ws, editor, _) = workspace(&pool, "acme").await;
    let r = editor
        .post(&contacts_path(ws))
        .json(&json!({"email": " Ops@Example.COM ", "first_name": " Ada ", "timezone": "Europe/London"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let c = r.json::<Value>();
    assert_eq!(c["email"], "ops@example.com");
    assert_eq!(c["first_name"], "Ada");
    assert_eq!(c["last_name"], "");
    assert_eq!(c["attributes"], json!({}));
    assert_eq!(c["timezone"], "Europe/London");

    let (stored, domain): (String, String) =
        sqlx::query_as("select email, email_domain from contacts where id = $1")
            .bind(Uuid::parse_str(c["id"].as_str().unwrap()).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "ops@example.com");
    assert_eq!(domain, "example.com");

    // The audit row names the contact by id, never by address.
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "select action, metadata from audit_log where workspace_id = $1 and action like 'contact.%'",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, "contact.created");
    assert!(!audit[0].1.to_string().contains("example.com"));

    for bad in ["", "no-at-sign", "a@b", "a b@example.com"] {
        let r = editor
            .post(&contacts_path(ws))
            .json(&json!({"email": bad}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(first_error(&r)["field"], "email", "{bad:?}");
    }
    let r = editor
        .post(&contacts_path(ws))
        .json(&json!({"email": "a@example.com", "timezone": "Mars/Base"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error(&r)["field"], "timezone");
    assert_eq!(count_contacts(&pool, ws).await, 1);
}

#[sqlx::test]
async fn duplicate_address_is_conflict(pool: PgPool) {
    let (ws, editor, _) = workspace(&pool, "acme").await;
    create_contact(&editor, ws, json!({"email": "ops@example.com"})).await;
    // A different spelling of the same address is the same address.
    let r = editor
        .post(&contacts_path(ws))
        .json(&json!({"email": " OPS@example.com"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:email-taken");
    assert_eq!(count_contacts(&pool, ws).await, 1);
}

#[sqlx::test]
async fn same_address_in_two_workspaces_is_fine(pool: PgPool) {
    let (ws1, editor1, _) = workspace(&pool, "one").await;
    let (ws2, editor2, _) = workspace(&pool, "two").await;
    let a = create_contact(&editor1, ws1, json!({"email": "ops@example.com"})).await;
    let b = create_contact(&editor2, ws2, json!({"email": "ops@example.com"})).await;
    assert_ne!(a, b);
}

#[sqlx::test]
async fn rejects_unknown_attribute_key(pool: PgPool) {
    let (ws, editor, admin) = workspace(&pool, "acme").await;
    create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    let r = editor
        .post(&contacts_path(ws))
        .json(&json!({"email": "a@example.com", "attributes": {"plna": "pro"}}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error(&r)["field"], "attributes.plna");
    assert_eq!(count_contacts(&pool, ws).await, 0);

    // Not an object at all.
    let r = editor
        .post(&contacts_path(ws))
        .json(&json!({"email": "a@example.com", "attributes": ["plan"]}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error(&r)["field"], "attributes");
}

#[sqlx::test]
async fn rejects_wrong_type_for_kind(pool: PgPool) {
    let (ws, editor, admin) = workspace(&pool, "acme").await;
    for (key, kind) in [
        ("seats", "number"),
        ("renews", "date"),
        ("seen", "datetime"),
        ("vip", "boolean"),
    ] {
        create_field(&admin, ws, json!({"key": key, "label": key, "kind": kind})).await;
    }
    create_field(
        &admin,
        ws,
        json!({"key": "tier", "label": "Tier", "kind": "select", "options": ["a", "b"]}),
    )
    .await;
    create_field(
        &admin,
        ws,
        json!({"key": "tags", "label": "Tags", "kind": "multi_select", "options": ["a", "b"]}),
    )
    .await;
    let bad = [
        ("seats", json!("abc")),
        ("seats", json!(null)),
        ("renews", json!("tomorrow")),
        ("renews", json!("2024-02-30")),
        ("seen", json!("2024-03-01")),
        ("vip", json!("true")),
        ("tier", json!("c")),
        ("tier", json!(["a"])),
        ("tags", json!(["a", "a"])),
        ("tags", json!(["a", "c"])),
        ("tags", json!("a")),
    ];
    for (key, value) in bad {
        let r = editor
            .post(&contacts_path(ws))
            .json(&json!({"email": "a@example.com", "attributes": {key: value}}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            first_error(&r)["field"],
            format!("attributes.{key}"),
            "{key} {value}"
        );
    }
    assert_eq!(count_contacts(&pool, ws).await, 0);
}

#[sqlx::test]
async fn rejects_value_for_archived_field(pool: PgPool) {
    let (ws, editor, admin) = workspace(&pool, "acme").await;
    let plan = create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    create_field(
        &admin,
        ws,
        json!({"key": "tier", "label": "Tier", "kind": "text"}),
    )
    .await;
    let id = create_contact(
        &editor,
        ws,
        json!({"email": "a@example.com", "attributes": {"plan": "pro", "tier": "gold"}}),
    )
    .await;
    archive(&admin, ws, plan).await;

    // A new contact cannot take a value for the archived field ...
    let r = editor
        .post(&contacts_path(ws))
        .json(&json!({"email": "b@example.com", "attributes": {"plan": "free"}}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error(&r)["field"], "attributes.plan");
    assert_eq!(count_contacts(&pool, ws).await, 1);

    // ... and neither can an existing one, which keeps the value it already had.
    let r = editor
        .patch(&format!("{}/{id}", contacts_path(ws)))
        .json(&json!({"attributes": {"plan": "free", "tier": "silver"}}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error(&r)["field"], "attributes.plan");
    let c = get_contact(&editor, ws, id).await;
    assert_eq!(c["attributes"], json!({"plan": "pro", "tier": "gold"}));

    // Naming only live fields still works.
    editor
        .patch(&format!("{}/{id}", contacts_path(ws)))
        .json(&json!({"attributes": {"tier": "silver"}}))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn accepts_every_field_kind(pool: PgPool) {
    let (ws, editor, admin) = workspace(&pool, "acme").await;
    for (key, kind, options) in [
        ("bio", "text", json!([])),
        ("seats", "number", json!([])),
        ("renews", "date", json!([])),
        ("seen", "datetime", json!([])),
        ("vip", "boolean", json!([])),
        ("tier", "select", json!(["a", "b"])),
        ("tags", "multi_select", json!(["a", "b", "c"])),
    ] {
        create_field(
            &admin,
            ws,
            json!({"key": key, "label": key, "kind": kind, "options": options}),
        )
        .await;
    }
    let sent = json!({
        "bio": "Hello, world",
        "seats": 12.5,
        "renews": "2025-01-31",
        "seen": "2025-01-31T09:30:00Z",
        "vip": false,
        "tier": "b",
        "tags": ["c", "a"],
    });
    let id = create_contact(
        &editor,
        ws,
        json!({"email": "a@example.com", "attributes": sent}),
    )
    .await;
    assert_eq!(get_contact(&editor, ws, id).await["attributes"], sent);

    // Dates and datetimes are stored in one canonical spelling.
    let id = create_contact(
        &editor,
        ws,
        json!({"email": "b@example.com",
               "attributes": {"renews": "2025-1-5", "seen": "2025-01-31T11:30:00+02:00"}}),
    )
    .await;
    assert_eq!(
        get_contact(&editor, ws, id).await["attributes"],
        json!({"renews": "2025-01-05", "seen": "2025-01-31T09:30:00Z"})
    );
}

#[sqlx::test]
async fn delete_archives_and_keeps_values(pool: PgPool) {
    let (ws, editor, admin) = workspace(&pool, "acme").await;
    let plan = create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    let id = create_contact(
        &editor,
        ws,
        json!({"email": "a@example.com", "attributes": {"plan": "pro"}}),
    )
    .await;

    archive(&admin, ws, plan).await;

    // The field is archived, not gone, and the contact still holds its value.
    let archived: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("select archived_at from custom_fields where id = $1")
            .bind(plan)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(archived.is_some());
    assert_eq!(
        get_contact(&editor, ws, id).await["attributes"],
        json!({"plan": "pro"})
    );
}

#[sqlx::test]
async fn purge_strips_the_key(pool: PgPool) {
    let (ws, editor, admin) = workspace(&pool, "acme").await;
    let plan = create_field(
        &admin,
        ws,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    create_field(
        &admin,
        ws,
        json!({"key": "tier", "label": "Tier", "kind": "text"}),
    )
    .await;
    let both = create_contact(
        &editor,
        ws,
        json!({"email": "a@example.com", "attributes": {"plan": "pro", "tier": "gold"}}),
    )
    .await;
    let without = create_contact(
        &editor,
        ws,
        json!({"email": "b@example.com", "attributes": {"tier": "silver"}}),
    )
    .await;
    // Another workspace's contact with the same key is not touched.
    let (other, other_editor, other_admin) = workspace(&pool, "other").await;
    create_field(
        &other_admin,
        other,
        json!({"key": "plan", "label": "Plan", "kind": "text"}),
    )
    .await;
    let theirs = create_contact(
        &other_editor,
        other,
        json!({"email": "a@example.com", "attributes": {"plan": "theirs"}}),
    )
    .await;

    archive(&admin, ws, plan).await;
    let r = admin
        .post(&format!("{}/{plan}/purge", fields_path(ws)))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["affected"], 1);

    assert_eq!(
        get_contact(&editor, ws, both).await["attributes"],
        json!({"tier": "gold"})
    );
    assert_eq!(
        get_contact(&editor, ws, without).await["attributes"],
        json!({"tier": "silver"})
    );
    assert_eq!(
        get_contact(&other_editor, other, theirs).await["attributes"],
        json!({"plan": "theirs"})
    );
}

#[sqlx::test]
async fn update_merges_attributes(pool: PgPool) {
    let (ws, editor, admin) = workspace(&pool, "acme").await;
    for key in ["plan", "tier"] {
        create_field(
            &admin,
            ws,
            json!({"key": key, "label": key, "kind": "text"}),
        )
        .await;
    }
    let id = create_contact(
        &editor,
        ws,
        json!({"email": "a@example.com", "first_name": "Ada", "last_name": "Lovelace",
               "attributes": {"plan": "pro", "tier": "gold"}}),
    )
    .await;
    let path = format!("{}/{id}", contacts_path(ws));

    let r = editor
        .patch(&path)
        .json(&json!({"attributes": {"tier": "silver"}, "timezone": "Asia/Kolkata"}))
        .await;
    r.assert_status_ok();
    let c = r.json::<Value>();
    // The named key changed; the others, including the names, are as they were.
    assert_eq!(c["attributes"], json!({"plan": "pro", "tier": "silver"}));
    assert_eq!(c["first_name"], "Ada");
    assert_eq!(c["last_name"], "Lovelace");
    assert_eq!(c["timezone"], "Asia/Kolkata");
    assert_eq!(c, get_contact(&editor, ws, id).await);

    // The address cannot change, and an unknown field is rejected rather than ignored.
    for body in [json!({"email": "b@example.com"}), json!({"status": "x"})] {
        editor
            .patch(&path)
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // A rejected update changes nothing.
    let r = editor
        .patch(&path)
        .json(&json!({"first_name": "Grace", "attributes": {"nope": 1}}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(get_contact(&editor, ws, id).await["first_name"], "Ada");

    let actions: Vec<String> = sqlx::query_scalar(
        "select action from audit_log where workspace_id = $1 and action like 'contact.%' order by id",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(actions, ["contact.created", "contact.updated"]);
}

#[sqlx::test]
async fn delete_is_hard_and_frees_the_address(pool: PgPool) {
    let (ws, editor, _) = workspace(&pool, "acme").await;
    let id = create_contact(&editor, ws, json!({"email": "a@example.com"})).await;
    editor
        .delete(&format!("{}/{id}", contacts_path(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(count_contacts(&pool, ws).await, 0);
    editor
        .get(&format!("{}/{id}", contacts_path(ws)))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Nothing was suppressed: the address can be added again.
    create_contact(&editor, ws, json!({"email": "a@example.com"})).await;
}

#[sqlx::test]
async fn contacts_have_no_status_column(pool: PgPool) {
    // An opt-out is a suppression keyed by address, never a column a re-import can clear.
    let mut cols: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns where table_name = 'contacts'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    cols.sort();
    assert_eq!(
        cols,
        [
            "attributes",
            "created_at",
            "email",
            "email_domain",
            "first_name",
            "id",
            "last_name",
            "timezone",
            "updated_at",
            "workspace_id"
        ]
    );
}

#[sqlx::test]
async fn viewer_cannot_create(pool: PgPool) {
    let (ws, editor, _) = workspace(&pool, "acme").await;
    let id = create_contact(&editor, ws, json!({"email": "a@example.com"})).await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;

    let r = viewer
        .post(&contacts_path(ws))
        .json(&json!({"email": "b@example.com"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    let path = format!("{}/{id}", contacts_path(ws));
    viewer
        .patch(&path)
        .json(&json!({"first_name": "x"}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    viewer
        .delete(&path)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(count_contacts(&pool, ws).await, 1);

    // Reading needs only viewer.
    assert_eq!(get_contact(&viewer, ws, id).await["email"], "a@example.com");
}

#[sqlx::test]
async fn cross_workspace_get_is_404(pool: PgPool) {
    let (ws1, editor1, _) = workspace(&pool, "one").await;
    let (ws2, editor2, _) = workspace(&pool, "two").await;
    let theirs = create_contact(&editor1, ws1, json!({"email": "a@example.com"})).await;
    let nowhere = Uuid::now_v7();

    // editor2 may edit in their own workspace, so only the workspace boundary can say 404: their
    // workspace with someone else's contact id, someone else's workspace, and no such workspace.
    let attempts = [
        ("GET", format!("{}/{theirs}", contacts_path(ws2))),
        ("PATCH", format!("{}/{theirs}", contacts_path(ws2))),
        ("DELETE", format!("{}/{theirs}", contacts_path(ws2))),
        ("GET", format!("{}/{theirs}", contacts_path(ws1))),
        ("PATCH", format!("{}/{theirs}", contacts_path(ws1))),
        ("DELETE", format!("{}/{theirs}", contacts_path(ws1))),
        ("POST", contacts_path(ws1)),
        ("POST", contacts_path(nowhere)),
    ];
    for (method, path) in attempts {
        let req = match method {
            "GET" => editor2.get(&path),
            "POST" => editor2.post(&path).json(&json!({"email": "x@example.com"})),
            "PATCH" => editor2
                .patch(&path)
                .json(&json!({"first_name": "hijacked"})),
            _ => editor2.delete(&path),
        };
        let r = req.await;
        r.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(
            problem_type(&r),
            "urn:nm:problem:not-found",
            "{method} {path}"
        );
    }
    assert_eq!(count_contacts(&pool, ws1).await, 1);
    assert_eq!(count_contacts(&pool, ws2).await, 0);
    assert_eq!(get_contact(&editor1, ws1, theirs).await["first_name"], "");
}
