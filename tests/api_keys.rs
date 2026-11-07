// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{crypto::tokens::sha256, workspaces::roles::Role};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// A signed-in owner and a workspace `slug` they own.
async fn owner_of(pool: &PgPool, slug: &str) -> (TestApp, Uuid, Uuid) {
    let id = create_user(pool, "owner@example.com", PW, false).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login("owner@example.com", PW).await;
    (app, id, create_workspace(pool, id, slug).await)
}

/// Creates a key as the signed-in `owner`; returns `(key id, the key)`.
async fn make_key(owner: &TestApp, ws: Uuid, role: &str) -> (Uuid, String) {
    let r = owner
        .post(&format!("/api/v1/workspaces/{ws}/api-keys"))
        .json(&json!({"name": "ci", "role": role}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let body = r.json::<Value>();
    (
        body["id"].as_str().unwrap().parse().unwrap(),
        body["key"].as_str().unwrap().to_owned(),
    )
}

/// A client with an empty cookie jar, so only the Bearer header can authenticate it.
async fn anon(pool: &PgPool) -> TestApp {
    TestApp::new(pool.clone()).await
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

#[sqlx::test]
async fn key_reads_its_workspace(pool: PgPool) {
    let (owner, owner_id, ws) = owner_of(&pool, "acme").await;
    let (_, key) = make_key(&owner, ws, "viewer").await;
    let app = anon(&pool).await;

    let r = app
        .get(&format!("/api/v1/workspaces/{ws}"))
        .add_header("authorization", bearer(&key))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["role"], "viewer");
    app.get(&format!("/api/v1/workspaces/{ws}/members"))
        .add_header("authorization", bearer(&key))
        .await
        .assert_status_ok();

    // A second workspace the key has no part in must not appear in its listing.
    create_workspace(&pool, owner_id, "other").await;
    let list = app
        .get("/api/v1/workspaces")
        .add_header("authorization", bearer(&key))
        .await;
    list.assert_status_ok();
    let list = list.json::<Value>();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["data"][0]["id"], ws.to_string());
    assert_eq!(list["data"][0]["role"], "viewer");
}

#[sqlx::test]
async fn key_is_confined(pool: PgPool) {
    let (owner, owner_id, ws1) = owner_of(&pool, "one").await;
    let ws2 = create_workspace(&pool, owner_id, "two").await;
    let (_, key) = make_key(&owner, ws1, "admin").await;
    let app = anon(&pool).await;

    for path in [
        format!("/api/v1/workspaces/{ws2}"),
        format!("/api/v1/workspaces/{ws2}/members"),
        format!("/api/v1/workspaces/{ws2}/api-keys"),
    ] {
        app.get(&path)
            .add_header("authorization", bearer(&key))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    let r = app
        .get("/api/v1/me")
        .add_header("authorization", bearer(&key))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:user-session-required");
}

#[sqlx::test]
async fn viewer_key_cannot_write(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let (_, key) = make_key(&owner, ws, "viewer").await;
    let r = anon(&pool)
        .await
        .patch(&format!("/api/v1/workspaces/{ws}"))
        .add_header("authorization", bearer(&key))
        .json(&json!({"name": "Hacked"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
}

#[sqlx::test]
async fn bad_keys_are_401(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let (id, key) = make_key(&owner, ws, "viewer").await;
    let app = anon(&pool).await;
    let path = format!("/api/v1/workspaces/{ws}");

    // Right prefix, wrong secret.
    let (head, _) = key.rsplit_once('_').unwrap();
    let wrong_secret = format!("{head}_{}", "A".repeat(43));
    for bad in [
        "nm_bad".to_owned(),
        wrong_secret,
        String::new(),
        "x".repeat(5000),
    ] {
        let r = app
            .get(&path)
            .add_header("authorization", bearer(&bad))
            .await;
        r.assert_status(StatusCode::UNAUTHORIZED);
        // One code for every way a key can be bad, so the response is no oracle.
        assert_eq!(problem_type(&r), "urn:nm:problem:invalid-api-key");
    }
    // Not our scheme: no key was presented, so this is simply an unauthenticated request.
    let r = app
        .get(&path)
        .add_header("authorization", format!("Basic {key}"))
        .await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(problem_type(&r), "urn:nm:problem:unauthenticated");

    // Works, then is revoked, then is refused.
    app.get(&path)
        .add_header("authorization", bearer(&key))
        .await
        .assert_status_ok();
    owner
        .delete(&format!("/api/v1/workspaces/{ws}/api-keys/{id}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let r = app
        .get(&path)
        .add_header("authorization", bearer(&key))
        .await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(problem_type(&r), "urn:nm:problem:invalid-api-key");
}

/// A valid session cookie must not rescue a bad Authorization header: with a header present the
/// cookie is not consulted at all.
#[sqlx::test]
async fn bogus_authorization_never_falls_back_to_the_cookie(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let path = format!("/api/v1/workspaces/{ws}");
    owner.get(&path).await.assert_status_ok();

    // Junk, an empty credential, a bare scheme and a differently-cased scheme: all are bearer
    // attempts, so none may fall back to the cookie.
    for header in [
        "Bearer nm_bad",
        "Bearer",
        "Bearer ",
        "bearer junk",
        "BEARER x",
    ] {
        for route in [path.as_str(), "/api/v1/me"] {
            let r = owner.get(route).add_header("authorization", header).await;
            r.assert_status(StatusCode::UNAUTHORIZED);
            assert_eq!(problem_type(&r), "urn:nm:problem:invalid-api-key");
        }
    }

    // A valid key plus a valid cookie authenticates as the key, never as the user.
    let (_, key) = make_key(&owner, ws, "viewer").await;
    let r = owner
        .get(&path)
        .add_header("authorization", bearer(&key))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["role"], "viewer");
    owner
        .get("/api/v1/me")
        .add_header("authorization", bearer(&key))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn key_shown_once_and_hashed(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let (id, key) = make_key(&owner, ws, "editor").await;

    let (prefix, secret) = key.strip_prefix("nm_").unwrap().split_once('_').unwrap();
    assert_eq!(prefix.len(), 8);
    assert!(
        prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    );
    assert_eq!(secret.len(), 43);

    let list = owner
        .get(&format!("/api/v1/workspaces/{ws}/api-keys"))
        .await;
    list.assert_status_ok();
    assert!(!list.text().contains(secret), "list leaked the secret");
    let list = list.json::<Value>();
    let item = &list["data"][0];
    assert_eq!(item["id"], id.to_string());
    assert_eq!(item["prefix"], prefix);
    assert!(item.get("key").is_none() && item.get("secret_hash").is_none());

    let stored: Vec<u8> = sqlx::query_scalar("select secret_hash from api_keys where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(stored, secret.as_bytes());
    assert_eq!(stored, sha256(secret.as_bytes()));

    // Nowhere in the audit log either.
    let audit: String =
        sqlx::query_scalar("select coalesce(string_agg(metadata::text, ' '), '') from audit_log")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!audit.contains(secret));
}

#[sqlx::test]
async fn key_writes_skip_csrf_and_are_attributed(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let (id, key) = make_key(&owner, ws, "admin").await;

    let r = anon(&pool)
        .await
        .patch(&format!("/api/v1/workspaces/{ws}"))
        .add_header("authorization", bearer(&key))
        .json(&json!({"name": "Renamed"}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["name"], "Renamed");

    let actor: Option<Uuid> = sqlx::query_scalar(
        "select actor_api_key_id from audit_log where action = 'workspace.updated' and workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actor, Some(id));
}

#[sqlx::test]
async fn key_creation_rules(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let path = format!("/api/v1/workspaces/{ws}/api-keys");

    // `owner` is not a key role: a 422, not a constraint-violation 500.
    for body in [
        json!({"name": "x", "role": "owner"}),
        json!({"name": "  ", "role": "viewer"}),
    ] {
        owner
            .post(&path)
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // A key, even an admin one, cannot mint or revoke keys: key management is for humans.
    let (id, admin_key) = make_key(&owner, ws, "admin").await;
    let app = anon(&pool).await;
    let r = app
        .post(&path)
        .add_header("authorization", bearer(&admin_key))
        .json(&json!({"name": "child", "role": "viewer"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:user-session-required");
    app.delete(&format!("{path}/{id}"))
        .add_header("authorization", bearer(&admin_key))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // A signed-in editor cannot create keys.
    let editor = create_user(&pool, "editor@example.com", PW, false).await;
    add_member(&pool, ws, editor, Role::Editor).await;
    let mut ed = TestApp::new(pool.clone()).await;
    ed.login("editor@example.com", PW).await;
    let r = ed
        .post(&path)
        .json(&json!({"name": "x", "role": "viewer"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
}

#[sqlx::test]
async fn last_used_is_written_at_most_once_a_minute(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let (id, key) = make_key(&owner, ws, "viewer").await;
    let app = anon(&pool).await;
    let used = || async {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "select last_used_at from api_keys where id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(used().await.is_none());
    let path = format!("/api/v1/workspaces/{ws}");
    app.get(&path)
        .add_header("authorization", bearer(&key))
        .await
        .assert_status_ok();
    let first = used().await.expect("first use is recorded");
    app.get(&path)
        .add_header("authorization", bearer(&key))
        .await
        .assert_status_ok();
    assert_eq!(
        used().await,
        Some(first),
        "a second use within a minute is not written"
    );

    // Once the stamp is over a minute old, the next use refreshes it.
    sqlx::query("update api_keys set last_used_at = now() - interval '2 minutes' where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    app.get(&path)
        .add_header("authorization", bearer(&key))
        .await
        .assert_status_ok();
    assert!(used().await.unwrap() >= first);
}

/// With an `Authorization` header the cookie is dropped before any handler runs, so even a
/// handler that reads the jar itself sees an anonymous caller: a signed-in invitee is not
/// recognised, and the accept is refused as `login-required` instead of succeeding.
#[sqlx::test]
async fn invite_accept_ignores_the_cookie_when_authorization_is_present(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let invitee = create_user(&pool, "b@example.com", PW, false).await;
    let mut b = TestApp::new(pool.clone()).await;
    b.login("b@example.com", PW).await;

    let r = owner
        .post(&format!("/api/v1/workspaces/{ws}/invites"))
        .json(&json!({"email": "b@example.com", "role": "viewer"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let url = r.json::<Value>()["invite_url"].as_str().unwrap().to_owned();
    let accept = format!("/api/v1/invites/{}/accept", url.rsplit('/').next().unwrap());

    let r = b
        .post(&accept)
        .add_header("authorization", "Bearer nm_bad")
        .json(&json!({}))
        .await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(problem_type(&r), "urn:nm:problem:login-required");
    let members: i64 = sqlx::query_scalar(
        "select count(*) from memberships where workspace_id = $1 and user_id = $2",
    )
    .bind(ws)
    .bind(invitee)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(members, 0);

    // The same signed-in request without the header is recognised and accepted (the claim above
    // rolled back with the refusal).
    b.post(&accept).json(&json!({})).await.assert_status_ok();
}

/// A scheme that is not ours, such as `Basic` from a reverse proxy, leaves the session alone:
/// the cookie authenticates, and CSRF still applies to it.
#[sqlx::test]
async fn non_bearer_authorization_leaves_cookie_auth_working(pool: PgPool) {
    let (owner, _, ws) = owner_of(&pool, "acme").await;
    let path = format!("/api/v1/workspaces/{ws}");
    let basic = "Basic dXNlcjpwYXNz";

    let r = owner.get(&path).add_header("authorization", basic).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["role"], "owner");
    owner
        .get("/api/v1/me")
        .add_header("authorization", basic)
        .await
        .assert_status_ok();
    owner
        .patch(&path)
        .add_header("authorization", basic)
        .json(&json!({"name": "Renamed"}))
        .await
        .assert_status_ok();

    // CSRF is not skipped for it: the same write without the token is refused.
    let r = owner
        .server
        .patch(&path)
        .add_header("authorization", basic)
        .json(&json!({"name": "Again"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:csrf");
}
