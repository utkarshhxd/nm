// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use axum_extra::extract::cookie::{Cookie, SameSite};
use common::{PW, TestApp, create_user};
use nm::{
    crypto::tokens::sha256,
    error::ApiError,
    http::request_meta::RequestMeta,
    users::{self, normalize_email},
};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn login_request(app: &TestApp, email: &str, password: &str) -> axum_test::TestResponse {
    app.server
        .post("/api/v1/auth/login")
        .json(&json!({"email": email, "password": password}))
        .await
}

#[sqlx::test]
async fn login_sets_hardened_cookie(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    let r = login_request(&app, "ops@example.com", PW).await;
    r.assert_status_ok();
    r.assert_json(&json!({"status":"ok"}));
    let c = r.cookie("nm_session");
    assert_eq!(c.http_only(), Some(true));
    assert_eq!(c.same_site(), Some(SameSite::Lax));
    assert_eq!(c.path(), Some("/"));
    // public_url is http://localhost, the one place Secure is dropped.
    assert_eq!(c.secure(), None);
    assert_eq!(c.max_age().map(|d| d.whole_days()), Some(30));
}

#[sqlx::test]
async fn cookie_is_secure_unless_public_url_is_local_http(pool: PgPool) {
    create_user(&pool, "ops@example.com", PW, false).await;
    for (url, secure) in [
        ("https://nm.example.com", Some(true)),
        ("http://nm.example.com", Some(true)),
        ("http://localhost.evil.com", Some(true)),
        ("http://127.0.0.1:8080", None),
        ("http://localhost", None),
    ] {
        let app = TestApp::with_config(pool.clone(), |c| c.public_url = url.parse().unwrap()).await;
        let r = login_request(&app, "ops@example.com", PW).await;
        r.assert_status_ok();
        assert_eq!(r.cookie("nm_session").secure(), secure, "{url}");
    }
}

#[sqlx::test]
async fn session_cookie_is_stored_only_as_a_hash(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    let r = login_request(&app, "ops@example.com", PW).await;
    let value = r.cookie("nm_session").value().to_owned();
    let stored: Vec<u8> = sqlx::query_scalar("select id_hash from sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, sha256(value.as_bytes()).to_vec());
}

#[sqlx::test]
async fn unknown_email_and_wrong_password_are_indistinguishable(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    let wrong = login_request(&app, "ops@example.com", "not the password").await;
    let unknown = login_request(&app, "nobody@example.com", "not the password").await;
    wrong.assert_status(StatusCode::UNAUTHORIZED);
    unknown.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.json::<Value>(), unknown.json::<Value>());
    assert_eq!(
        wrong.json::<Value>()["type"],
        "urn:nm:problem:invalid-credentials"
    );
    assert_eq!(
        wrong.json::<Value>()["detail"],
        "Email or password is incorrect"
    );
}

/// The over-length check runs before the user lookup, so a known and an unknown email
/// must answer a 1025-byte password identically (and so must a NUL or malformed email).
#[sqlx::test]
async fn over_length_password_and_bad_emails_answer_like_any_failure(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    let long = "x".repeat(1025);
    let known = login_request(&app, "ops@example.com", &long).await;
    let unknown = login_request(&app, "nobody@example.com", &long).await;
    let nul = login_request(&app, "a\u{0}b@example.com", "not the password").await;
    let malformed = login_request(&app, "not-an-email", "not the password").await;
    for r in [&known, &unknown, &nul, &malformed] {
        r.assert_status(StatusCode::UNAUTHORIZED);
        assert_eq!(r.json::<Value>(), known.json::<Value>());
    }
    // A NUL must never reach audit metadata, where Postgres would reject it.
    let failed: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'auth.login.failed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(failed, 4);
}

#[sqlx::test]
async fn email_case_and_spaces_are_normalised(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, " Ops@Example.COM ", PW, false).await;
    let stored: String = sqlx::query_scalar("select email from users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, "ops@example.com");
    login_request(&app, "OPS@example.com", PW)
        .await
        .assert_status_ok();
    let again = users::create_user(&pool, "ops@example.com", PW, None, false).await;
    assert!(matches!(again, Err(ApiError::Conflict("email-taken"))));
}

#[test]
fn normalize_email_rules() {
    assert_eq!(normalize_email("  A@B.co ").unwrap(), "a@b.co");
    for bad in [
        "",
        "a@b",
        "@b.co",
        "a@@b.co",
        "a@b@c.co",
        "a b@c.co",
        "a\u{0}@b.co",
        "nope",
    ] {
        assert!(normalize_email(bad).is_err(), "{bad:?}");
    }
    assert!(normalize_email(&format!("{}@b.co", "a".repeat(250))).is_err());
}

#[sqlx::test]
async fn me_requires_session(pool: PgPool) {
    let r = TestApp::new(pool).await.server.get("/api/v1/me").await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:unauthenticated");
}

#[sqlx::test]
async fn me_returns_profile_and_csrf(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let id = create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;
    let me = app.get("/api/v1/me").await.json::<Value>();
    assert_eq!(me["id"], id.to_string());
    assert_eq!(me["email"], "ops@example.com");
    assert_eq!(me["is_instance_admin"], false);
    assert_eq!(me["totp_enabled"], false);
    assert_eq!(me["passkey_count"], 0);
    assert_eq!(me["workspaces"], json!([]));
    assert!(!me["csrf_token"].as_str().unwrap().is_empty());
    assert!(me.get("password_hash").is_none());
}

#[sqlx::test]
async fn logout_ends_session(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;
    app.post("/api/v1/auth/logout")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let sessions: i64 = sqlx::query_scalar("select count(*) from sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

#[sqlx::test]
async fn second_login_replaces_first_session(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let id = create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;
    app.login("ops@example.com", PW).await;
    let n: i64 = sqlx::query_scalar("select count(*) from sessions where user_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn password_change_revokes_other_sessions(pool: PgPool) {
    let mut app1 = TestApp::new(pool.clone()).await;
    let mut app2 = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app1.login("ops@example.com", PW).await;
    app2.login("ops@example.com", PW).await;
    let new_pw = "a brand new passphrase";
    let wrong = app1
        .post("/api/v1/me/password")
        .json(&json!({"current_password": "not the password", "new_password": new_pw}))
        .await;
    wrong.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong.json::<Value>()["type"],
        "urn:nm:problem:invalid-credentials"
    );
    app1.post("/api/v1/me/password")
        .json(&json!({"current_password": PW, "new_password": new_pw}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app2.get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    app1.get("/api/v1/me").await.assert_status_ok();
    // The new password works and the old one no longer does.
    let fresh = TestApp::new(pool.clone()).await;
    login_request(&fresh, "ops@example.com", PW)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    login_request(&fresh, "ops@example.com", new_pw)
        .await
        .assert_status_ok();
    let audited: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'auth.password.changed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audited, 1);
}

#[sqlx::test]
async fn password_change_rejects_bad_input_without_touching_sessions(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;
    let short = app
        .post("/api/v1/me/password")
        .json(&json!({"current_password": PW, "new_password": "short"}))
        .await;
    short.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(short.json::<Value>()["errors"][0]["field"], "new_password");
    let long = app
        .post("/api/v1/me/password")
        .json(&json!({"current_password": "x".repeat(1025), "new_password": "a brand new passphrase"}))
        .await;
    long.assert_status(StatusCode::UNAUTHORIZED);
    app.get("/api/v1/me").await.assert_status_ok();
}

#[sqlx::test]
async fn idle_session_expires(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;
    sqlx::query("update sessions set last_seen_at = now() - interval '8 days'")
        .execute(&pool)
        .await
        .unwrap();
    app.get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn absolute_expiry_ends_an_active_session(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;
    sqlx::query("update sessions set expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    app.get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn pending_mfa_session_is_never_accepted(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let id = create_user(&pool, "ops@example.com", PW, false).await;
    let pending = nm::auth::sessions::create(&pool, id, true, &RequestMeta::default())
        .await
        .unwrap();
    app.server
        .add_cookie(Cookie::new("nm_session", pending.cookie_value));
    app.get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    app.post("/api/v1/auth/logout")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // A pending session lasts 10 minutes, not 30 days.
    let secs: f64 = sqlx::query_scalar(
        "select extract(epoch from expires_at - created_at)::float8 from sessions",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((599.0..=601.0).contains(&secs), "{secs}");
}

#[sqlx::test]
async fn last_seen_is_written_at_most_once_a_minute(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;
    let age = || async {
        sqlx::query_scalar::<_, f64>(
            "select extract(epoch from now() - last_seen_at)::float8 from sessions",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    sqlx::query("update sessions set last_seen_at = now() - interval '30 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    app.get("/api/v1/me").await.assert_status_ok();
    assert!(age().await >= 30.0, "fresh enough: no write");
    sqlx::query("update sessions set last_seen_at = now() - interval '2 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    app.get("/api/v1/me").await.assert_status_ok();
    assert!(age().await < 5.0, "stale: refreshed");
}

#[sqlx::test]
async fn logins_are_audited_without_secrets(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    login_request(&app, "ops@example.com", "not the password")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    login_request(&app, "ops@example.com", PW)
        .await
        .assert_status_ok();
    let rows: Vec<(String, Value)> =
        sqlx::query_as("select action, metadata from audit_log order by id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let actions: Vec<_> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(actions, ["auth.login.failed", "auth.login.succeeded"]);
    for (_, metadata) in &rows {
        let text = metadata.to_string();
        assert!(!text.contains(PW) && !text.contains("not the password"));
    }
}
