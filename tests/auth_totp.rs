// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum_test::TestResponse;
use common::{PW, TestApp, create_user};
use nm::{auth::totp::code_for, users};
use serde_json::{Value, json};
use sqlx::PgPool;
use totp_rs::Secret;
use uuid::Uuid;

const EMAIL: &str = "ops@example.com";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Enrolled {
    secret: String,
    recovery: Vec<String>,
    user_id: Uuid,
}

/// Creates the user and turns TOTP on through the API, then signs out. The confirming code is the
/// previous step's, so the current step is still unspent and sign-in can use it.
async fn enrol(app: &mut TestApp) -> Enrolled {
    let user_id = create_user(&app.pool, EMAIL, PW, false).await;
    app.login(EMAIL, PW).await;
    let setup = app.post("/api/v1/me/totp/setup").await;
    setup.assert_status_ok();
    let setup = setup.json::<Value>();
    let secret = setup["secret"].as_str().unwrap().to_owned();
    assert!(
        setup["otpauth_url"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/localhost:ops%40example.com?secret=")
    );
    let confirm = app
        .post("/api/v1/me/totp/confirm")
        .json(&json!({"code": code_for(&secret, now() - 30)}))
        .await;
    confirm.assert_status_ok();
    let recovery: Vec<String> = confirm.json::<Value>()["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(recovery.len(), 10);
    logout(app).await;
    Enrolled {
        secret,
        recovery,
        user_id,
    }
}

async fn logout(app: &mut TestApp) {
    app.post("/api/v1/auth/logout")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.csrf = None;
}

async fn password_login(app: &TestApp) -> TestResponse {
    app.server
        .post("/api/v1/auth/login")
        .json(&json!({"email": EMAIL, "password": PW}))
        .await
}

async fn second_factor(app: &TestApp, body: Value) -> TestResponse {
    app.server.post("/api/v1/auth/login/totp").json(&body).await
}

/// Reads the CSRF token of the session the cookie jar now holds, as `TestApp::login` does.
async fn adopt_session(app: &mut TestApp) {
    let me = app.server.get("/api/v1/me").await;
    me.assert_status_ok();
    app.csrf = Some(
        me.json::<Value>()["csrf_token"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
}

fn assert_problem(r: &TestResponse, status: StatusCode, code: &str) {
    r.assert_status(status);
    assert_eq!(r.json::<Value>()["type"], format!("urn:nm:problem:{code}"));
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[sqlx::test]
async fn totp_makes_login_require_a_code(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    let e = enrol(&mut app).await;

    let r = password_login(&app).await;
    r.assert_status_ok();
    r.assert_json(&json!({"status":"mfa_required","methods":["totp"]}));
    // The pending session lives 10 minutes, and its cookie says so.
    assert_eq!(
        r.cookie("nm_session").max_age().map(|d| d.whole_minutes()),
        Some(10)
    );
    app.server
        .get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    let r = second_factor(&app, json!({"code": code_for(&e.secret, now())})).await;
    r.assert_status_ok();
    r.assert_json(&json!({"status":"ok"}));
    assert_eq!(
        r.cookie("nm_session").max_age().map(|d| d.whole_days()),
        Some(30)
    );
    let me = app.server.get("/api/v1/me").await;
    me.assert_status_ok();
    assert_eq!(me.json::<Value>()["totp_enabled"], true);
    // The pending session was replaced, not upgraded.
    assert_eq!(count(&app.pool, "select count(*) from sessions").await, 1);
    assert_eq!(
        count(&app.pool, "select count(*) from sessions where mfa_pending").await,
        0
    );
}

#[sqlx::test]
async fn a_code_cannot_be_replayed(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    let e = enrol(&mut app).await;
    let code = code_for(&e.secret, now());

    password_login(&app).await.assert_status_ok();
    second_factor(&app, json!({"code": code}))
        .await
        .assert_status_ok();
    adopt_session(&mut app).await;
    logout(&mut app).await;

    password_login(&app).await.assert_status_ok();
    let r = second_factor(&app, json!({"code": code})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "invalid-code");
}

#[sqlx::test]
async fn the_confirming_code_cannot_sign_in(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    let e = enrol(&mut app).await;
    password_login(&app).await.assert_status_ok();
    let r = second_factor(&app, json!({"code": code_for(&e.secret, now() - 30)})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "invalid-code");
}

#[sqlx::test]
async fn stale_code_rejected(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    let e = enrol(&mut app).await;
    password_login(&app).await.assert_status_ok();
    let r = second_factor(&app, json!({"code": code_for(&e.secret, now() - 75)})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "invalid-code");
}

#[sqlx::test]
async fn fifth_wrong_code_forces_restart(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    let e = enrol(&mut app).await;
    password_login(&app).await.assert_status_ok();
    for _ in 0..4 {
        let r = second_factor(&app, json!({"code": "000000"})).await;
        assert_problem(&r, StatusCode::UNAUTHORIZED, "invalid-code");
    }
    let r = second_factor(&app, json!({"code": "000000"})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "mfa-restart");
    assert_eq!(count(&app.pool, "select count(*) from sessions").await, 0);
    // The session is gone, so even the right code has nothing to complete.
    let r = second_factor(&app, json!({"code": code_for(&e.secret, now())})).await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    // The restart is also a failed sign-in as far as the limiter is concerned.
    assert_eq!(
        count(
            &app.pool,
            "select count(*) from login_attempts where not succeeded"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn recovery_code_works_exactly_once(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    let e = enrol(&mut app).await;

    password_login(&app).await.assert_status_ok();
    let r = second_factor(&app, json!({"recovery_code": e.recovery[0].to_uppercase()})).await;
    r.assert_status_ok();
    r.assert_json(&json!({"status":"ok"}));
    adopt_session(&mut app).await;
    logout(&mut app).await;

    password_login(&app).await.assert_status_ok();
    let r = second_factor(&app, json!({"recovery_code": e.recovery[0]})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "invalid-code");
    // Another code still works.
    second_factor(
        &app,
        json!({"recovery_code": e.recovery[1].replace('-', "")}),
    )
    .await
    .assert_status_ok();

    let audited: Vec<(String,)> = sqlx::query_as(
        "select metadata::text from audit_log where action = 'auth.recovery_code.used'",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(audited.len(), 2);
    for (metadata,) in audited {
        assert!(e.recovery.iter().all(|c| !metadata.contains(c.as_str())));
    }
}

#[sqlx::test]
async fn a_recovery_code_is_spent_by_exactly_one_of_many_concurrent_uses(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let e = enrol(&mut app).await;
    let attempts = (0..8).map(|_| {
        let pool = pool.clone();
        let code = e.recovery[3].clone();
        tokio::spawn(async move {
            nm::auth::totp::consume_recovery_code(&pool, e.user_id, &code)
                .await
                .unwrap()
        })
    });
    let mut wins = 0;
    for a in attempts {
        wins += usize::from(a.await.unwrap());
    }
    assert_eq!(wins, 1);
}

#[sqlx::test]
async fn operator_can_disable_mfa(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let e = enrol(&mut app).await;
    password_login(&app).await.assert_status_ok();
    second_factor(&app, json!({"code": code_for(&e.secret, now())}))
        .await
        .assert_status_ok();

    users::disable_mfa(&pool, EMAIL).await.unwrap();

    // Sessions are revoked, the second factor and its recovery codes are gone.
    app.server
        .get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(count(&pool, "select count(*) from recovery_codes").await, 0);
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action = 'auth.totp.disabled' and actor_user_id is null"
        )
        .await,
        1
    );
    app.login(EMAIL, PW).await;
    let me = app.get("/api/v1/me").await;
    assert_eq!(me.json::<Value>()["totp_enabled"], false);
}

#[sqlx::test]
async fn totp_secret_encrypted_at_rest(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let e = enrol(&mut app).await;
    let stored: Vec<u8> = sqlx::query_scalar("select totp_secret_enc from users")
        .fetch_one(&pool)
        .await
        .unwrap();
    let raw = Secret::try_from_base32(&e.secret).unwrap();
    let contains = |needle: &[u8]| stored.windows(needle.len()).any(|w| w == needle);
    assert_eq!(raw.as_bytes().len(), 20);
    assert!(!contains(e.secret.as_bytes()));
    assert!(!contains(raw.as_bytes()));
    // Confirming moved the pending secret, it did not copy it.
    assert_eq!(
        count(
            &pool,
            "select count(*) from users where totp_pending_secret_enc is not null"
        )
        .await,
        0
    );
}

#[sqlx::test]
async fn disabling_totp_requires_password(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let e = enrol(&mut app).await;
    password_login(&app).await.assert_status_ok();
    second_factor(&app, json!({"code": code_for(&e.secret, now())}))
        .await
        .assert_status_ok();
    adopt_session(&mut app).await;

    let r = app
        .delete("/api/v1/me/totp")
        .json(&json!({"password": "not the password"}))
        .await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "invalid-credentials");
    assert_eq!(
        count(&pool, "select count(*) from recovery_codes").await,
        10
    );

    app.delete("/api/v1/me/totp")
        .json(&json!({"password": PW}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(count(&pool, "select count(*) from recovery_codes").await, 0);
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action = 'auth.totp.disabled'"
        )
        .await,
        1
    );
    let me = app.get("/api/v1/me").await;
    assert_eq!(me.json::<Value>()["totp_enabled"], false);
}

#[sqlx::test]
async fn setup_is_refused_once_enabled_and_confirm_needs_a_right_code(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    create_user(&app.pool, EMAIL, PW, false).await;
    app.login(EMAIL, PW).await;
    app.post("/api/v1/me/totp/setup").await.assert_status_ok();

    let r = app
        .post("/api/v1/me/totp/confirm")
        .json(&json!({"code": "000000"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>()["errors"][0]["field"], "code");

    // Setting up again before confirming replaces the pending secret and is allowed.
    let setup = app.post("/api/v1/me/totp/setup").await;
    setup.assert_status_ok();
    let secret = setup.json::<Value>()["secret"].as_str().unwrap().to_owned();
    app.post("/api/v1/me/totp/confirm")
        .json(&json!({"code": code_for(&secret, now() - 30)}))
        .await
        .assert_status_ok();

    let r = app.post("/api/v1/me/totp/setup").await;
    assert_problem(&r, StatusCode::CONFLICT, "totp-already-enabled");
}

#[sqlx::test]
async fn second_factor_needs_a_live_pending_session(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let e = enrol(&mut app).await;
    let code = code_for(&e.secret, now());

    // No session at all.
    let r = second_factor(&app, json!({"code": code})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "unauthenticated");

    // An expired pending session.
    password_login(&app).await.assert_status_ok();
    sqlx::query("update sessions set expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let r = second_factor(&app, json!({"code": code})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "unauthenticated");

    // A full session is not a pending one.
    password_login(&app).await.assert_status_ok();
    second_factor(&app, json!({"code": code}))
        .await
        .assert_status_ok();
    let r = second_factor(&app, json!({"recovery_code": e.recovery[0]})).await;
    assert_problem(&r, StatusCode::UNAUTHORIZED, "unauthenticated");
}

#[sqlx::test]
async fn second_factor_needs_exactly_one_credential(pool: PgPool) {
    let mut app = TestApp::new(pool).await;
    let e = enrol(&mut app).await;
    password_login(&app).await.assert_status_ok();
    let both = json!({"code": code_for(&e.secret, now()), "recovery_code": e.recovery[0]});
    for body in [json!({}), both] {
        second_factor(&app, body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // Neither attempt spent anything.
    second_factor(&app, json!({"code": code_for(&e.secret, now())}))
        .await
        .assert_status_ok();
}
