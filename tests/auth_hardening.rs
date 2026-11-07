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
