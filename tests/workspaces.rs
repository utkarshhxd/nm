// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// A signed-in client for a fresh user; every client has its own cookie jar.
async fn client(pool: &PgPool, email: &str, instance_admin: bool) -> (TestApp, Uuid) {
    let id = create_user(pool, email, PW, instance_admin).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    (app, id)
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

fn first_error_field(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["errors"][0]["field"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[sqlx::test]
async fn only_instance_admins_create_workspaces(pool: PgPool) {
    let (user, _) = client(&pool, "user@example.com", false).await;
    let r = user
        .post("/api/v1/workspaces")
        .json(&json!({"slug": "acme", "name": "Acme"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:instance-admin-required");

    let (admin, admin_id) = client(&pool, "admin@example.com", true).await;
    let r = admin
        .post("/api/v1/workspaces")
        .json(&json!({"slug": "acme", "name": "Acme"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let ws = r.json::<Value>();
    assert_eq!(ws["slug"], "acme");
    assert_eq!(ws["timezone"], "UTC");
    assert_eq!(ws["role"], "owner");

    let me = admin.get("/api/v1/me").await.json::<Value>();
    assert_eq!(me["workspaces"][0]["id"], ws["id"]);
    assert_eq!(me["workspaces"][0]["slug"], "acme");
    assert_eq!(me["workspaces"][0]["role"], "owner");

    let audit = sqlx::query_scalar::<_, Value>(
        "select metadata from audit_log where action = 'workspace.created' and actor_user_id = $1",
    )
    .bind(admin_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit, json!({"slug": "acme", "name": "Acme"}));
}

#[sqlx::test]
async fn workspace_input_rules(pool: PgPool) {
    let (admin, _) = client(&pool, "admin@example.com", true).await;
    let create = |body: Value| {
        let admin = &admin;
        async move { admin.post("/api/v1/workspaces").json(&body).await }
    };

    let r = create(json!({"slug": "Brand One", "name": "Brand"})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error_field(&r), "slug");
    for bad in ["-a", "a-", "", &"a".repeat(41)] {
        let r = create(json!({"slug": bad, "name": "Brand"})).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(first_error_field(&r), "slug", "slug {bad:?}");
    }
    for bad in ["", "   ", &"x".repeat(101), "nul\u{0}byte"] {
        let r = create(json!({"slug": "ok", "name": bad})).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(first_error_field(&r), "name", "name {bad:?}");
    }
    let r = create(json!({"slug": "ok", "name": "Brand", "timezone": "Mars/Base"})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error_field(&r), "timezone");

    // Boundaries: one character, forty characters, and a trimmed name.
    create(json!({"slug": "a", "name": "  Padded  ", "timezone": "Asia/Kolkata"}))
        .await
        .assert_status(StatusCode::CREATED);
    create(json!({"slug": format!("a{}b", "-".repeat(38)), "name": "x"}))
        .await
        .assert_status(StatusCode::CREATED);
    let name: String = sqlx::query_scalar("select name from workspaces where slug = 'a'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Padded");

    create(json!({"slug": "acme", "name": "Acme"}))
        .await
        .assert_status(StatusCode::CREATED);
    let r = create(json!({"slug": "acme", "name": "Acme again"})).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:slug-taken");
}

#[sqlx::test]
async fn other_workspaces_are_invisible(pool: PgPool) {
    let (a, a_id) = client(&pool, "a@example.com", true).await;
    let (_b, b_id) = client(&pool, "b@example.com", true).await;
    let ws1 = create_workspace(&pool, a_id, "one").await;
    let ws2 = create_workspace(&pool, b_id, "two").await;
    let nowhere = Uuid::now_v7();

    // Another workspace and one that does not exist answer identically: a 404, never a 403.
    for (ws, who) in [(ws2, "someone else's"), (nowhere, "a missing")] {
        let paths = [
            ("GET", format!("/api/v1/workspaces/{ws}")),
            ("PATCH", format!("/api/v1/workspaces/{ws}")),
            ("GET", format!("/api/v1/workspaces/{ws}/members")),
            ("PATCH", format!("/api/v1/workspaces/{ws}/members/{b_id}")),
            ("DELETE", format!("/api/v1/workspaces/{ws}/members/{b_id}")),
            ("GET", format!("/api/v1/workspaces/{ws}/audit-log")),
        ];
        for (method, path) in paths {
            let req = match method {
                "GET" => a.get(&path),
                "PATCH" if path.contains("/members/") => {
                    a.patch(&path).json(&json!({"role": "viewer"}))
                }
                "PATCH" => a.patch(&path).json(&json!({"name": "x"})),
                _ => a.delete(&path),
            };
            let r = req.await;
            r.assert_status(StatusCode::NOT_FOUND);
            assert_eq!(
                problem_type(&r),
                "urn:nm:problem:not-found",
                "{method} {path} on {who}"
            );
        }
    }

    let list = a.get("/api/v1/workspaces").await.json::<Value>();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["data"][0]["id"], ws1.to_string());

    // Nothing leaked and nothing changed.
    let name: String = sqlx::query_scalar("select name from workspaces where id = $1")
        .bind(ws2)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "two");
    let owners: i64 =
        sqlx::query_scalar("select count(*) from memberships where workspace_id = $1")
            .bind(ws2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(owners, 1);
}

#[sqlx::test]
async fn viewers_cannot_manage_members(pool: PgPool) {
    let (_o, owner) = client(&pool, "owner@example.com", true).await;
    let (viewer, viewer_id) = client(&pool, "viewer@example.com", false).await;
    let (_e, editor) = client(&pool, "editor@example.com", false).await;
    let ws = create_workspace(&pool, owner, "acme").await;
    add_member(&pool, ws, viewer_id, Role::Viewer).await;
    add_member(&pool, ws, editor, Role::Editor).await;

    let r = viewer
        .patch(&format!("/api/v1/workspaces/{ws}/members/{editor}"))
        .json(&json!({"role": "viewer"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");

    let r = viewer
        .delete(&format!("/api/v1/workspaces/{ws}/members/{editor}"))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");

    let r = viewer
        .patch(&format!("/api/v1/workspaces/{ws}"))
        .json(&json!({"name": "mine now"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);

    // A viewer can still read the workspace and its member list.
    viewer
        .get(&format!("/api/v1/workspaces/{ws}"))
        .await
        .assert_status_ok();
    let members = viewer
        .get(&format!("/api/v1/workspaces/{ws}/members"))
        .await
        .json::<Value>();
    assert_eq!(members["data"].as_array().unwrap().len(), 3);
}

#[sqlx::test]
async fn admins_cannot_change_owners(pool: PgPool) {
    let (_o, owner) = client(&pool, "owner@example.com", true).await;
    let (admin, admin_id) = client(&pool, "admin@example.com", false).await;
    let (_v, viewer) = client(&pool, "viewer@example.com", false).await;
    let ws = create_workspace(&pool, owner, "acme").await;
    add_member(&pool, ws, admin_id, Role::Admin).await;
    add_member(&pool, ws, viewer, Role::Viewer).await;

    let r = admin
        .patch(&format!("/api/v1/workspaces/{ws}/members/{owner}"))
        .json(&json!({"role": "admin"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");

    let r = admin
        .delete(&format!("/api/v1/workspaces/{ws}/members/{owner}"))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);

    // Nor can an admin mint an owner.
    let r = admin
        .patch(&format!("/api/v1/workspaces/{ws}/members/{viewer}"))
        .json(&json!({"role": "owner"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);

    // But an admin may manage below owner.
    admin
        .patch(&format!("/api/v1/workspaces/{ws}/members/{viewer}"))
        .json(&json!({"role": "editor"}))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn last_owner_is_protected(pool: PgPool) {
    let (o1, owner1) = client(&pool, "owner1@example.com", true).await;
    let ws = create_workspace(&pool, owner1, "acme").await;
    let me_path = format!("/api/v1/workspaces/{ws}/members/{owner1}");

    let r = o1.delete(&me_path).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:last-owner");
    let r = o1.patch(&me_path).json(&json!({"role": "admin"})).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:last-owner");

    let owner2 = create_user(&pool, "owner2@example.com", PW, false).await;
    add_member(&pool, ws, owner2, Role::Owner).await;
    o1.patch(&me_path)
        .json(&json!({"role": "admin"}))
        .await
        .assert_status_ok();
}

/// Two owners each step down at the same moment. Serialised by the owner-row lock, exactly one
/// wins and the other gets `last-owner`; without it both would see two owners and succeed.
#[sqlx::test]
async fn concurrent_demotions_leave_an_owner(pool: PgPool) {
    let (o1, owner1) = client(&pool, "owner1@example.com", true).await;
    let (o2, owner2) = client(&pool, "owner2@example.com", false).await;
    for round in 0..5 {
        let ws = create_workspace(&pool, owner1, &format!("ws{round}")).await;
        add_member(&pool, ws, owner2, Role::Owner).await;
        let demote = |app: &TestApp, id: Uuid| {
            app.patch(&format!("/api/v1/workspaces/{ws}/members/{id}"))
                .json(&json!({"role": "viewer"}))
        };
        let (r1, r2) = tokio::join!(demote(&o1, owner1), demote(&o2, owner2));
        let mut statuses = [r1.status_code().as_u16(), r2.status_code().as_u16()];
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 409], "round {round}");
        let owners: i64 = sqlx::query_scalar(
            "select count(*) from memberships where workspace_id = $1 and role = 'owner'",
        )
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(owners, 1, "round {round}");
    }
}

#[sqlx::test]
async fn members_are_listed_and_may_leave(pool: PgPool) {
    let (_o, owner) = client(&pool, "owner@example.com", true).await;
    let (viewer, viewer_id) = client(&pool, "viewer@example.com", false).await;
    let ws = create_workspace(&pool, owner, "acme").await;
    add_member(&pool, ws, viewer_id, Role::Viewer).await;

    let members = viewer
        .get(&format!("/api/v1/workspaces/{ws}/members"))
        .await
        .json::<Value>();
    let row = members["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["user_id"] == viewer_id.to_string())
        .unwrap();
    assert_eq!(row["email"], "viewer@example.com");
    assert_eq!(row["role"], "viewer");
    assert!(row["joined_at"].is_string());
    assert!(row.get("display_name").is_some());

    // A viewer may remove only themselves.
    viewer
        .delete(&format!("/api/v1/workspaces/{ws}/members/{viewer_id}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    viewer
        .get(&format!("/api/v1/workspaces/{ws}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let removed = sqlx::query_scalar::<_, Value>(
        "select metadata from audit_log where action = 'member.removed' and workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(removed, json!({"role": "viewer"}));
}

#[sqlx::test]
async fn member_changes_are_audited(pool: PgPool) {
    let (owner_app, owner) = client(&pool, "owner@example.com", true).await;
    let (viewer_app, viewer) = client(&pool, "viewer@example.com", false).await;
    let ws = create_workspace(&pool, owner, "acme").await;
    add_member(&pool, ws, viewer, Role::Viewer).await;

    owner_app
        .patch(&format!("/api/v1/workspaces/{ws}/members/{viewer}"))
        .json(&json!({"role": "editor"}))
        .await
        .assert_status_ok();

    let log = owner_app
        .get(&format!("/api/v1/workspaces/{ws}/audit-log"))
        .await
        .json::<Value>();
    let top = &log["data"][0];
    assert_eq!(top["action"], "member.role_changed");
    assert_eq!(top["metadata"], json!({"from": "viewer", "to": "editor"}));
    assert_eq!(top["actor_user_id"], owner.to_string());
    assert_eq!(top["target_type"], "user");
    assert_eq!(top["target_id"], viewer.to_string());
    assert!(top["actor_api_key_id"].is_null());
    assert!(top["created_at"].is_string());
    assert!(top.get("ip").is_some());

    let r = viewer_app
        .get(&format!("/api/v1/workspaces/{ws}/audit-log"))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
}

#[sqlx::test]
async fn audit_log_paginates_newest_first(pool: PgPool) {
    let (owner_app, owner) = client(&pool, "owner@example.com", true).await;
    let (_v, viewer) = client(&pool, "viewer@example.com", false).await;
    // Event 1: workspace.created.
    let ws = create_workspace(&pool, owner, "acme").await;
    add_member(&pool, ws, viewer, Role::Viewer).await;
    for role in ["editor", "admin"] {
        owner_app
            .patch(&format!("/api/v1/workspaces/{ws}/members/{viewer}"))
            .json(&json!({"role": role}))
            .await
            .assert_status_ok();
    }

    let path = format!("/api/v1/workspaces/{ws}/audit-log");
    let page1 = owner_app
        .get(&format!("{path}?limit=2"))
        .await
        .json::<Value>();
    let data = page1["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["metadata"]["to"], "admin");
    assert_eq!(data[1]["metadata"]["to"], "editor");
    let cursor = page1["next_cursor"].as_str().unwrap();

    let page2 = owner_app
        .get(&path)
        .add_query_params([("limit", "2"), ("cursor", cursor)])
        .await
        .json::<Value>();
    assert_eq!(page2["data"].as_array().unwrap().len(), 1);
    assert_eq!(page2["data"][0]["action"], "workspace.created");
    assert!(page2["next_cursor"].is_null());
}

/// The audit log names its actors, and keeps the entries of actors that have since gone.
///
/// The two joins behind `actor_email` and `actor_api_key_prefix` are LEFT joins for exactly the
/// second half of that: a workspace that stops naming the people who acted in it the moment one
/// of them is deleted — or, worse, stops listing what they did — fails at its one job.
#[sqlx::test]
async fn audit_log_names_its_actors(pool: PgPool) {
    let (owner_app, owner) = client(&pool, "owner@example.com", true).await;
    let (admin_app, admin) = client(&pool, "admin@example.com", false).await;
    let (_v, viewer) = client(&pool, "viewer@example.com", false).await;
    // Event 1: workspace.created, by the owner.
    let ws = create_workspace(&pool, owner, "acme").await;
    add_member(&pool, ws, admin, Role::Admin).await;
    add_member(&pool, ws, viewer, Role::Viewer).await;

    // Event 2: member.role_changed, by the admin, who is deleted below.
    admin_app
        .patch(&format!("/api/v1/workspaces/{ws}/members/{viewer}"))
        .json(&json!({"role": "editor"}))
        .await
        .assert_status_ok();

    // Event 3: api_key.created, by the admin. Event 4: workspace.updated, by that key.
    let created = admin_app
        .post(&format!("/api/v1/workspaces/{ws}/api-keys"))
        .json(&json!({"name": "bot", "role": "admin"}))
        .await
        .json::<Value>();
    let key = created["key"].as_str().unwrap().to_owned();
    let prefix = key.split('_').nth(1).unwrap().to_owned();
    let bot = TestApp::new(pool.clone()).await;
    bot.patch(&format!("/api/v1/workspaces/{ws}"))
        .add_header("authorization", format!("Bearer {key}"))
        .json(&json!({"name": "Acme Inc"}))
        .await
        .assert_status_ok();

    let path = format!("/api/v1/workspaces/{ws}/audit-log");
    let body = owner_app.get(&path).await.json::<Value>();
    let rows = body["data"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 4);
    // Newest first, and a key is named by its prefix rather than an address.
    assert_eq!(rows[0]["action"], "workspace.updated");
    assert_eq!(rows[0]["actor_api_key_prefix"], prefix);
    assert!(rows[0]["actor_email"].is_null());
    assert_eq!(rows[3]["action"], "workspace.created");
    assert_eq!(rows[3]["actor_email"], "owner@example.com");
    assert!(rows[3]["actor_api_key_prefix"].is_null());

    sqlx::query("delete from users where id = $1")
        .bind(admin)
        .execute(&pool)
        .await
        .expect("delete the admin");

    let after = owner_app.get(&path).await.json::<Value>();
    let rows = after["data"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        4,
        "a deleted actor must not take its entries with it"
    );
    let changed = rows
        .iter()
        .find(|e| e["action"] == "member.role_changed")
        .expect("the role change is still listed");
    assert!(changed["actor_email"].is_null());
    assert_eq!(changed["actor_user_id"], admin.to_string());
}

#[sqlx::test]
async fn admins_rename_workspaces_and_it_is_audited(pool: PgPool) {
    let (admin, admin_id) = client(&pool, "admin@example.com", false).await;
    let (_o, owner) = client(&pool, "owner@example.com", true).await;
    let ws = create_workspace(&pool, owner, "acme").await;
    add_member(&pool, ws, admin_id, Role::Admin).await;
    let path = format!("/api/v1/workspaces/{ws}");

    let r = admin
        .patch(&path)
        .json(&json!({"name": " Acme Inc ", "timezone": "Europe/London"}))
        .await;
    r.assert_status_ok();
    let body = r.json::<Value>();
    assert_eq!(body["name"], "Acme Inc");
    assert_eq!(body["timezone"], "Europe/London");
    assert_eq!(body["slug"], "acme");
    assert_eq!(body["role"], "admin");

    // The slug is fixed in M0, and a bad value changes nothing.
    let r = admin.patch(&path).json(&json!({"slug": "other"})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let r = admin
        .patch(&path)
        .json(&json!({"timezone": "Mars/Base"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first_error_field(&r), "timezone");

    let meta = sqlx::query_scalar::<_, Value>(
        "select metadata from audit_log where action = 'workspace.updated' and workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        meta,
        json!({"name": "Acme Inc", "timezone": "Europe/London"})
    );
}

/// The two orderings differ on purpose. `/me.workspaces` is by name, because `home.tsx` redirects
/// into its first element and a switcher a human reads must not reshuffle when a workspace is
/// added. `GET /workspaces` is `id desc`, which its cursor forces. Do not unify them.
#[sqlx::test]
async fn me_orders_workspaces_by_name_and_the_endpoint_by_id(pool: PgPool) {
    let (admin, admin_id) = client(&pool, "admin@example.com", true).await;
    // `create_workspace` names a workspace after its slug, so "alpha" is the older id.
    create_workspace(&pool, admin_id, "alpha").await;
    create_workspace(&pool, admin_id, "zulu").await;

    let me = admin.get("/api/v1/me").await.json::<Value>();
    let names: Vec<&str> = me["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alpha", "zulu"], "/me is by name");

    let page = admin.get("/api/v1/workspaces").await.json::<Value>();
    let names: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["zulu", "alpha"],
        "the paginated list is newest first"
    );
}
