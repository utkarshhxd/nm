// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{crypto::tokens::sha256, workspaces::roles::Role};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// A signed-in client for a fresh user; every client has its own cookie jar.
async fn client(pool: &PgPool, email: &str) -> (TestApp, Uuid) {
    let id = create_user(pool, email, PW, false).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    (app, id)
}

/// A workspace and its signed-in owner.
async fn workspace(pool: &PgPool) -> (TestApp, Uuid) {
    let (owner, owner_id) = client(pool, "owner@example.com").await;
    (owner, create_workspace(pool, owner_id, "acme").await)
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

/// Creates an invite and returns `(invite id, raw token)`; the token is the last path segment of
/// `invite_url`.
async fn invite(app: &TestApp, ws: Uuid, email: &str, role: &str) -> (Uuid, String) {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/invites"))
        .json(&json!({"email": email, "role": role}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let body = r.json::<Value>();
    let url = body["invite_url"].as_str().unwrap();
    assert!(url.starts_with("http://localhost:8080/invite/"), "{url}");
    let token = url.rsplit('/').next().unwrap().to_owned();
    (body["id"].as_str().unwrap().parse().unwrap(), token)
}

fn accept_path(token: &str) -> String {
    format!("/api/v1/invites/{token}/accept")
}

async fn accepted_at_is_null(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar("select accepted_at is null from invites where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn memberships_of(pool: &PgPool, ws: Uuid, user: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from memberships where workspace_id = $1 and user_id = $2")
        .bind(ws)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn newcomer_accepts_with_password_and_is_signed_in(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let (_, token) = invite(&owner, ws, "New@Example.com", "editor").await;

    let anon = TestApp::new(pool.clone()).await;
    let r = anon.get(&format!("/api/v1/invites/{token}")).await;
    r.assert_status_ok();
    assert_eq!(
        r.json::<Value>(),
        json!({"workspace_name": "acme", "email": "new@example.com", "role": "editor",
               "account_exists": false})
    );

    let r = anon
        .post(&accept_path(&token))
        .json(&json!({"password": PW, "display_name": "  Newbie "}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>(), json!({"workspace_id": ws}));
    assert!(r.maybe_cookie("nm_session").is_some());

    let me = anon.get("/api/v1/me").await.json::<Value>();
    assert_eq!(me["email"], "new@example.com");
    assert_eq!(me["display_name"], "Newbie");
    assert_eq!(me["workspaces"][0]["role"], "editor");
    assert_eq!(me["workspaces"][0]["id"], json!(ws));

    let audit: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'invite.accepted' and workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[sqlx::test]
async fn newcomer_needs_a_valid_password_and_a_failure_leaves_no_trace(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let (id, token) = invite(&owner, ws, "new@example.com", "viewer").await;
    let anon = TestApp::new(pool.clone()).await;

    for body in [json!({}), json!({"password": "short"})] {
        let r = anon.post(&accept_path(&token)).json(&body).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r.json::<Value>()["errors"][0]["field"], "password");
    }
    let r = anon
        .post(&accept_path(&token))
        .json(&json!({"password": PW, "display_name": "bad\u{0}name"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>()["errors"][0]["field"], "display_name");

    assert!(accepted_at_is_null(&pool, id).await);
    let users: i64 = sqlx::query_scalar("select count(*) from users where email = $1")
        .bind("new@example.com")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 0);
    anon.post(&accept_path(&token))
        .json(&json!({"password": PW}))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn existing_user_must_sign_in_first(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let invitee = create_user(&pool, "b@example.com", PW, false).await;
    let (_, token) = invite(&owner, ws, "b@example.com", "admin").await;

    let mut b = TestApp::new(pool.clone()).await;
    let r = b.get(&format!("/api/v1/invites/{token}")).await;
    assert_eq!(r.json::<Value>()["account_exists"], true);

    let r = b.post(&accept_path(&token)).json(&json!({})).await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(problem_type(&r), "urn:nm:problem:login-required");
    assert_eq!(memberships_of(&pool, ws, invitee).await, 0);

    b.login("b@example.com", PW).await;
    let r = b.post(&accept_path(&token)).json(&json!({})).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>(), json!({"workspace_id": ws}));
    let role: String =
        sqlx::query_scalar("select role from memberships where workspace_id = $1 and user_id = $2")
            .bind(ws)
            .bind(invitee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(role, "admin");
}

#[sqlx::test]
async fn invite_opened_as_someone_else_is_refused(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    create_user(&pool, "b@example.com", PW, false).await;
    let (c, c_id) = client(&pool, "c@example.com").await;
    // One invite for an address with an account, one for an address without.
    for email in ["b@example.com", "n@example.com"] {
        let (id, token) = invite(&owner, ws, email, "editor").await;
        let r = c
            .post(&accept_path(&token))
            .json(&json!({"password": PW}))
            .await;
        r.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(problem_type(&r), "urn:nm:problem:invite-email-mismatch");
        assert!(accepted_at_is_null(&pool, id).await);
    }
    assert_eq!(memberships_of(&pool, ws, c_id).await, 0);
    let created: i64 =
        sqlx::query_scalar("select count(*) from users where email = 'n@example.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(created, 0);
}

#[sqlx::test]
async fn invites_are_single_use_and_expire(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let anon = TestApp::new(pool.clone()).await;

    let (_, token) = invite(&owner, ws, "new@example.com", "viewer").await;
    let body = json!({"password": PW});
    anon.post(&accept_path(&token))
        .json(&body)
        .await
        .assert_status_ok();
    // A fresh client: the first one is now signed in, so its next POST would need a CSRF token.
    let anon = TestApp::new(pool.clone()).await;
    anon.post(&accept_path(&token))
        .json(&body)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let (id, token) = invite(&owner, ws, "late@example.com", "viewer").await;
    sqlx::query("update invites set expires_at = now() - interval '1 second' where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    anon.get(&format!("/api/v1/invites/{token}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    anon.post(&accept_path(&token))
        .json(&body)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn unknown_expired_used_and_revoked_invites_are_indistinguishable(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let anon = TestApp::new(pool.clone()).await;
    let body = json!({"password": PW});

    let (_, used) = invite(&owner, ws, "used@example.com", "viewer").await;
    anon.post(&accept_path(&used))
        .json(&body)
        .await
        .assert_status_ok();
    let (expired_id, expired) = invite(&owner, ws, "expired@example.com", "viewer").await;
    sqlx::query("update invites set expires_at = now() - interval '1 second' where id = $1")
        .bind(expired_id)
        .execute(&pool)
        .await
        .unwrap();
    let (revoked_id, revoked) = invite(&owner, ws, "revoked@example.com", "viewer").await;
    owner
        .delete(&format!("/api/v1/workspaces/{ws}/invites/{revoked_id}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let unknown = "A".repeat(43);
    let anon = TestApp::new(pool.clone()).await;

    let mut seen = Vec::new();
    for token in [&unknown, &expired, &used, &revoked] {
        let get = anon.get(&format!("/api/v1/invites/{token}")).await;
        let post = anon.post(&accept_path(token)).json(&body).await;
        for r in [&get, &post] {
            r.assert_status(StatusCode::NOT_FOUND);
            seen.push((r.header("content-type"), r.text()));
        }
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    assert_eq!(
        problem_type(&anon.get("/api/v1/invites/nope").await),
        "urn:nm:problem:not-found"
    );
}

#[sqlx::test]
async fn concurrent_accepts_of_one_token_create_one_membership(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let (_, token) = invite(&owner, ws, "new@example.com", "editor").await;
    let (a, b) = (
        TestApp::new(pool.clone()).await,
        TestApp::new(pool.clone()).await,
    );
    let body = json!({"password": PW});
    let (ra, rb) = tokio::join!(
        a.post(&accept_path(&token)).json(&body),
        b.post(&accept_path(&token)).json(&body),
    );
    let mut statuses = [ra.status_code(), rb.status_code()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::NOT_FOUND]);
    let members: i64 = sqlx::query_scalar(
        "select count(*) from memberships m join users u on u.id = m.user_id \
         where m.workspace_id = $1 and u.email = 'new@example.com'",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(members, 1);
    let sessions: i64 = sqlx::query_scalar(
        "select count(*) from sessions s join users u on u.id = s.user_id where u.email = 'new@example.com'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sessions, 1);
}

#[sqlx::test]
async fn reinviting_replaces_the_open_invite_and_members_conflict(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let (first_id, first) = invite(&owner, ws, "new@example.com", "viewer").await;
    let (_, second) = invite(&owner, ws, "NEW@example.com", "editor").await;
    let anon = TestApp::new(pool.clone()).await;
    anon.get(&format!("/api/v1/invites/{first}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let revoked: bool =
        sqlx::query_scalar("select revoked_at is not null from invites where id = $1")
            .bind(first_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked);
    let list = owner
        .get(&format!("/api/v1/workspaces/{ws}/invites"))
        .await
        .json::<Value>();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["data"][0]["role"], "editor");

    anon.post(&accept_path(&second))
        .json(&json!({"password": PW}))
        .await
        .assert_status_ok();
    let r = owner
        .post(&format!("/api/v1/workspaces/{ws}/invites"))
        .json(&json!({"email": "new@example.com", "role": "viewer"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:already-member");
}

#[sqlx::test]
async fn admins_cannot_invite_owners(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let (admin, admin_id) = client(&pool, "admin@example.com").await;
    add_member(&pool, ws, admin_id, Role::Admin).await;
    let path = format!("/api/v1/workspaces/{ws}/invites");

    let r = admin
        .post(&path)
        .json(&json!({"email": "x@example.com", "role": "owner"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    admin
        .post(&path)
        .json(&json!({"email": "x@example.com", "role": "admin"}))
        .await
        .assert_status(StatusCode::CREATED);
    owner
        .post(&path)
        .json(&json!({"email": "y@example.com", "role": "owner"}))
        .await
        .assert_status(StatusCode::CREATED);

    // An editor manages no one, and a stranger learns nothing about the workspace.
    let (editor, editor_id) = client(&pool, "editor@example.com").await;
    add_member(&pool, ws, editor_id, Role::Editor).await;
    editor.get(&path).await.assert_status(StatusCode::FORBIDDEN);
    let (stranger, _) = client(&pool, "stranger@example.com").await;
    stranger
        .get(&path)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    stranger
        .post(&path)
        .json(&json!({"email": "z@example.com", "role": "viewer"}))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn tokens_are_never_listed_or_stored(pool: PgPool) {
    let (owner, ws) = workspace(&pool).await;
    let (id, token) = invite(&owner, ws, "new@example.com", "viewer").await;

    let list = owner.get(&format!("/api/v1/workspaces/{ws}/invites")).await;
    list.assert_status_ok();
    let text = list.text();
    assert!(!text.contains(&token) && !text.contains("invite_url") && !text.contains("token"));
    assert_eq!(list.json::<Value>()["data"][0]["id"], json!(id));

    let stored: Vec<u8> = sqlx::query_scalar("select token_hash from invites where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(stored, token.as_bytes());
    assert_eq!(stored, sha256(token.as_bytes()));

    let leaked: i64 =
        sqlx::query_scalar("select count(*) from audit_log where metadata::text like $1")
            .bind(format!("%{token}%"))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(leaked, 0);
    let created: Value = sqlx::query_scalar(
        "select metadata from audit_log where action = 'invite.created' and workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        created,
        json!({"email": "new@example.com", "role": "viewer"})
    );
}
