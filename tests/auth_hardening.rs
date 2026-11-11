// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, create_user};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn login_as(app: &TestApp, email: &str, password: &str) -> axum_test::TestResponse {
    app.server
        .post("/api/v1/auth/login")
        .json(&json!({"email": email, "password": password}))
        .await
}

async fn from(app: &TestApp, ip: &str) -> axum_test::TestResponse {
    app.server
        .post("/api/v1/auth/login")
        .add_header("x-forwarded-for", ip)
        .json(&json!({"email": "ops@example.com", "password": PW}))
        .await
}

#[sqlx::test]
async fn unsafe_request_without_csrf_is_403(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;

    let r = app.server.post("/api/v1/auth/logout").await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:csrf");

    app.post("/api/v1/auth/logout")
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[sqlx::test]
async fn wrong_csrf_is_403(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    app.login("ops@example.com", PW).await;

    let r = app
        .server
        .post("/api/v1/auth/logout")
        .add_header("x-csrf-token", "nope")
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:csrf");
}

#[sqlx::test]
async fn non_json_body_is_415(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let r = app
        .server
        .post("/api/v1/auth/login")
        .content_type("text/plain")
        .text("hello")
        .await;
    r.assert_status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        r.json::<Value>()["type"],
        "urn:nm:problem:unsupported-media-type"
    );
}

#[sqlx::test]
async fn eleventh_attempt_is_429_even_with_correct_password(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    for _ in 0..10 {
        login_as(&app, "ops@example.com", "wrong password here")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    let r = login_as(&app, "ops@example.com", PW).await;
    r.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let secs: u64 = r.header("retry-after").to_str().unwrap().parse().unwrap();
    assert!((1..=900).contains(&secs), "retry-after {secs}");
}

#[sqlx::test]
async fn lockout_is_per_email(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "a@example.com", PW, false).await;
    create_user(&pool, "b@example.com", PW, false).await;
    for _ in 0..10 {
        login_as(&app, "a@example.com", "wrong password here")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    login_as(&app, "a@example.com", PW)
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    login_as(&app, "b@example.com", PW).await.assert_status_ok();
}

#[sqlx::test]
async fn ip_limit_keys_on_forwarded_ip_when_trusted(pool: PgPool) {
    let app = TestApp::with_config(pool.clone(), |c| c.trust_proxy_headers = true).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    for i in 0..100 {
        app.server
            .post("/api/v1/auth/login")
            .add_header("x-forwarded-for", "9.9.9.9")
            .json(&json!({"email": format!("nobody{i}@example.com"), "password": "wrong password here"}))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    from(&app, "9.9.9.9")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    from(&app, "8.8.8.8").await.assert_status_ok();
}

#[sqlx::test]
async fn successful_sign_ins_do_not_count_toward_the_lock(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    for _ in 0..9 {
        login_as(&app, "ops@example.com", "wrong password here")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    // Five successes would take 9 failures past the cap if they counted.
    for _ in 0..5 {
        login_as(&app, "ops@example.com", PW)
            .await
            .assert_status_ok();
    }
    // The tenth failure is still allowed to try; the one after it is locked out.
    login_as(&app, "ops@example.com", "wrong password here")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    login_as(&app, "ops@example.com", PW)
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
}

async fn row_counts(pool: &PgPool) -> (i64, i64) {
    sqlx::query_as("select (select count(*) from login_attempts), (select count(*) from audit_log)")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn locked_requests_write_nothing_even_with_an_over_length_password(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    for _ in 0..10 {
        login_as(&app, "ops@example.com", "wrong password here")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    let before = row_counts(&pool).await;
    let r = login_as(&app, "ops@example.com", &"x".repeat(1025)).await;
    r.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(row_counts(&pool).await, before);
}

#[sqlx::test]
async fn malformed_emails_have_no_email_bucket(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    for _ in 0..11 {
        login_as(&app, "not an email", "wrong password here")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    let null_email: i64 =
        sqlx::query_scalar("select count(*) from login_attempts where email is null")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(null_email, 11);
}

#[sqlx::test]
async fn ipv6_limit_keys_on_the_64(pool: PgPool) {
    let app = TestApp::with_config(pool.clone(), |c| c.trust_proxy_headers = true).await;
    create_user(&pool, "ops@example.com", PW, false).await;
    for i in 0..100 {
        app.server
            .post("/api/v1/auth/login")
            .add_header("x-forwarded-for", format!("2001:db8:1:2::{i:x}"))
            .json(&json!({"email": format!("nobody{i}@example.com"), "password": "wrong password here"}))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    // Same /64, different host bits: locked. Another /64: not.
    from(&app, "2001:db8:1:2:ffff::1")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    from(&app, "2001:db8:1:3::1").await.assert_status_ok();
}
