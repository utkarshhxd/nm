// SPDX-License-Identifier: AGPL-3.0-only

//! A `SoftPasskey` plays the authenticator and browser. It cannot do a discoverable sign-in on its
//! own (it only signs for credentials named in `allowCredentials` and sends no user handle), so
//! `assertion` does the two things a real browser does for a discoverable credential: names the
//! chosen credential and supplies its user handle.

mod common;

use axum::http::StatusCode;
use axum_test::TestResponse;
use common::{PW, TestApp, create_user};
use serde_json::{Value, json};
use sqlx::PgPool;
use url::Url;
use uuid::Uuid;
use webauthn_authenticator_rs::{
    AuthenticatorBackend, WebauthnAuthenticator, softpasskey::SoftPasskey,
};
use webauthn_rs::prelude::{
    CreationChallengeResponse, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse,
};

const EMAIL: &str = "ops@example.com";
const ORIGIN: &str = "http://localhost:8080";

fn soft() -> SoftPasskey {
    // `true`: the soft token claims user verification, which the server requires.
    SoftPasskey::new(true)
}

fn url(s: &str) -> Url {
    s.parse().unwrap()
}

/// A registered passkey as the test needs it later: the credential id and the owner's user handle.
struct Registered {
    /// The credential id as it appears in JSON.
    raw_id: Value,
    user_id: Uuid,
}

async fn register_start(app: &TestApp, name: &str) -> TestResponse {
    app.post("/api/v1/me/passkeys/register/start")
        .json(&json!({ "name": name }))
        .await
}

async fn register_finish(
    app: &TestApp,
    challenge_id: &Value,
    credential: &RegisterPublicKeyCredential,
) -> TestResponse {
    app.post("/api/v1/me/passkeys/register/finish")
        .json(&json!({ "challenge_id": challenge_id, "credential": credential }))
        .await
}

/// Registers a passkey through the API with the real origin; returns the 201 response's body.
async fn register(
    app: &TestApp,
    soft: &mut SoftPasskey,
    user_id: Uuid,
    name: &str,
) -> (Registered, Value) {
    let start = register_start(app, name).await;
    start.assert_status_ok();
    let start = start.json::<Value>();
    // `options` is the inner `publicKey` object, so wrap it back the way a browser API takes it.
    let ccr: CreationChallengeResponse =
        serde_json::from_value(json!({ "publicKey": start["options"] })).expect("creation options");
    let credential = soft
        .do_registration(url(ORIGIN), ccr)
        .expect("soft registration");
    let finish = register_finish(app, &start["challenge_id"], &credential).await;
    finish.assert_status(StatusCode::CREATED);
    (
        Registered {
            raw_id: serde_json::to_value(&credential).unwrap()["rawId"].clone(),
            user_id,
        },
        finish.json::<Value>(),
    )
}

async fn login_start(app: &TestApp) -> (Value, Value) {
    let start = app.server.post("/api/v1/auth/passkey/login/start").await;
    start.assert_status_ok();
    let start = start.json::<Value>();
    // Conditional UI needs no credential list; the browser lets the user pick one.
    assert_eq!(start["options"]["allowCredentials"], json!([]));
    (start["challenge_id"].clone(), start["options"].clone())
}

/// The authenticator's answer to `options`, for `reg`'s credential, claiming `origin` and
/// carrying `user_handle`. `perform_auth` skips the client-side origin checks that `do_*` makes,
/// which is what a malicious page's own clientDataJSON looks like to the server.
fn assertion(
    soft: &mut SoftPasskey,
    origin: &str,
    mut options: Value,
    reg: &Registered,
    user_handle: Uuid,
) -> PublicKeyCredential {
    options["allowCredentials"] = json!([{ "type": "public-key", "id": reg.raw_id }]);
    let rcr: RequestChallengeResponse =
        serde_json::from_value(json!({ "publicKey": options })).expect("request options");
    let mut cred = soft
        .perform_auth(url(origin), rcr.public_key, 60_000)
        .expect("soft authentication");
    cred.response.user_handle = Some(user_handle.as_bytes().to_vec());
    cred
}

async fn login_finish(
    app: &TestApp,
    challenge_id: &Value,
    credential: &PublicKeyCredential,
) -> TestResponse {
    app.server
        .post("/api/v1/auth/passkey/login/finish")
        .json(&json!({ "challenge_id": challenge_id, "credential": credential }))
        .await
}

/// A user with a password, signed in, who then registers one passkey. Returns the soft token too.
async fn user_with_passkey(app: &mut TestApp) -> (SoftPasskey, Registered) {
    let user_id = create_user(&app.pool, EMAIL, PW, false).await;
    app.login(EMAIL, PW).await;
    let mut soft = soft();
    let (reg, _) = register(app, &mut soft, user_id, "laptop").await;
    (soft, reg)
}

async fn logout(app: &mut TestApp) {
    app.post("/api/v1/auth/logout")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.csrf = None;
}

/// Reads the CSRF token of the session the cookie jar now holds, as `TestApp::login` does.
async fn adopt_session(app: &mut TestApp) -> Value {
    let me = app.server.get("/api/v1/me").await;
    me.assert_status_ok();
    let me = me.json::<Value>();
    app.csrf = Some(me["csrf_token"].as_str().unwrap().to_owned());
    me
}

/// The `auth.login.failed` rows, as (target user, metadata).
async fn failed_logins(pool: &PgPool) -> Vec<(Option<String>, Value)> {
    sqlx::query_as(
        "select target_id, metadata from audit_log where action = 'auth.login.failed' order by id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn assert_invalid_passkey(r: &TestResponse) {
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:invalid-passkey");
}

#[sqlx::test]
async fn register_then_sign_in_without_password(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;

    // The list shows what was registered, and nothing of the credential.
    let list = app.get("/api/v1/me/passkeys").await;
    list.assert_status_ok();
    let list = list.json::<Value>();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "laptop");
    assert!(list[0]["last_used_at"].is_null());
    assert_eq!(list[0].as_object().unwrap().len(), 4);

    // A second registration offers the existing credential for exclusion.
    let again = register_start(&app, "again").await.json::<Value>();
    let excluded = again["options"]["excludeCredentials"].as_array().unwrap();
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0]["id"], reg.raw_id);

    logout(&mut app).await;
    app.server
        .get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // Twice, so the counter is seen to move on every sign-in.
    for n in 1..=2 {
        let (challenge_id, options) = login_start(&app).await;
        let cred = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
        let finish = login_finish(&app, &challenge_id, &cred).await;
        finish.assert_status_ok();
        finish.assert_json(&json!({"status":"ok"}));
        let me = adopt_session(&mut app).await;
        assert_eq!(me["passkey_count"], 1);
        assert_eq!(me["email"], EMAIL);

        let (counter, used): (i64, bool) = sqlx::query_as(
            "select (passkey->'cred'->>'counter')::bigint, last_used_at is not null from passkeys",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            counter, n,
            "signature counter must be stored after sign-in {n}"
        );
        assert!(used);
        logout(&mut app).await;
    }

    // Both sign-ins are audited, with the method and nothing of the ceremony.
    let metas: Vec<(Uuid, Value)> = sqlx::query_as(
        "select actor_user_id, metadata from audit_log          where action = 'auth.login.succeeded' and metadata->>'method' = 'passkey'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(metas.len(), 2);
    for (actor, meta) in metas {
        assert_eq!(actor, reg.user_id);
        assert_eq!(meta, json!({ "method": "passkey" }));
    }
    let added: Value =
        sqlx::query_scalar("select metadata from audit_log where action = 'auth.passkey.added'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(added["name"], "laptop");
}

#[sqlx::test]
async fn passkey_sign_in_skips_totp(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;
    sqlx::query("update users set totp_enabled_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    logout(&mut app).await;

    // The control: the password path now stops at a second factor.
    let pw = app
        .server
        .post("/api/v1/auth/login")
        .json(&json!({"email": EMAIL, "password": PW}))
        .await;
    pw.assert_json(&json!({"status":"mfa_required","methods":["totp"]}));
    // Drop the pending session this left in the jar.
    app.server.clear_cookies();

    let (challenge_id, options) = login_start(&app).await;
    let cred = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
    let finish = login_finish(&app, &challenge_id, &cred).await;
    finish.assert_status_ok();
    finish.assert_json(&json!({"status":"ok"}));
    // A full session straight away: `/me` answers.
    let me = adopt_session(&mut app).await;
    assert_eq!(me["totp_enabled"], true);
}

#[sqlx::test]
async fn challenges_are_single_use(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;
    logout(&mut app).await;

    let (challenge_id, options) = login_start(&app).await;
    let cred = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
    login_finish(&app, &challenge_id, &cred)
        .await
        .assert_status_ok();
    app.server.clear_cookies();
    // The very same, valid, signed response again: the challenge is gone, and that is not audited.
    assert_invalid_passkey(&login_finish(&app, &challenge_id, &cred).await);
    assert!(failed_logins(&pool).await.is_empty());
    app.server
        .get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // A failed finish spends the challenge too: a bad guess does not get a second try.
    let (challenge_id, options) = login_start(&app).await;
    let mut bad = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
    bad.response.signature = vec![0u8; 8];
    assert_invalid_passkey(&login_finish(&app, &challenge_id, &bad).await);
    let left: i64 =
        sqlx::query_scalar("select count(*) from auth_challenges where kind = 'passkey_login'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn expired_challenge_rejected(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;
    logout(&mut app).await;

    let (challenge_id, options) = login_start(&app).await;
    sqlx::query("update auth_challenges set expires_at = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .unwrap();
    let cred = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
    assert_invalid_passkey(&login_finish(&app, &challenge_id, &cred).await);

    // Registration challenges expire the same way.
    app.login(EMAIL, PW).await;
    let start = register_start(&app, "late").await.json::<Value>();
    sqlx::query("update auth_challenges set expires_at = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .unwrap();
    let ccr: CreationChallengeResponse =
        serde_json::from_value(json!({ "publicKey": start["options"] })).unwrap();
    let credential = soft.do_registration(url(ORIGIN), ccr).unwrap();
    assert_invalid_passkey(&register_finish(&app, &start["challenge_id"], &credential).await);
}

#[sqlx::test]
async fn removed_passkey_cannot_sign_in(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;
    let id = app.get("/api/v1/me/passkeys").await.json::<Value>()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    app.delete(&format!("/api/v1/me/passkeys/{id}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Gone: a second delete is a 404, and `/me` counts none.
    app.delete(&format!("/api/v1/me/passkeys/{id}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(adopt_session(&mut app).await["passkey_count"], 0);
    logout(&mut app).await;

    let (challenge_id, options) = login_start(&app).await;
    let cred = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
    assert_invalid_passkey(&login_finish(&app, &challenge_id, &cred).await);

    // No credential row was found, so nothing is audited: an anonymous caller cannot write rows.
    assert!(failed_logins(&pool).await.is_empty());

    let removed: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'auth.passkey.removed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(removed, 1);
}

#[sqlx::test]
async fn foreign_origin_rejected(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;

    // Registration from another origin: the authenticator would have signed it, the server must not.
    let start = register_start(&app, "phish").await.json::<Value>();
    let ccr: CreationChallengeResponse =
        serde_json::from_value(json!({ "publicKey": start["options"] })).unwrap();
    let credential = soft
        .perform_register(url("http://evil.example"), ccr.public_key, 60_000)
        .expect("soft registration");
    assert_invalid_passkey(&register_finish(&app, &start["challenge_id"], &credential).await);
    let count: i64 = sqlx::query_scalar("select count(*) from passkeys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    logout(&mut app).await;

    // Sign-in from another origin, with an otherwise perfect assertion.
    let (challenge_id, options) = login_start(&app).await;
    let cred = assertion(&mut soft, "http://evil.example", options, &reg, reg.user_id);
    assert_invalid_passkey(&login_finish(&app, &challenge_id, &cred).await);
    app.server
        .get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // The credential was real, so the failure is audited, with a reason and no credential material.
    assert_eq!(
        failed_logins(&pool).await,
        vec![(
            Some(reg.user_id.to_string()),
            json!({ "method": "passkey", "reason": "verification-failed" })
        )]
    );
}

#[sqlx::test]
async fn credential_is_bound_to_its_user(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;
    logout(&mut app).await;
    let other = create_user(&pool, "other@example.com", PW, false).await;

    // The assertion is genuine, but the user handle names someone who does not own the credential.
    let (challenge_id, options) = login_start(&app).await;
    let cred = assertion(&mut soft, ORIGIN, options, &reg, other);
    assert_invalid_passkey(&login_finish(&app, &challenge_id, &cred).await);
    app.server
        .get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn passkey_routes_need_a_user_session(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    app.server
        .get("/api/v1/me/passkeys")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    app.server
        .post("/api/v1/me/passkeys/register/start")
        .json(&json!({"name":"x"}))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn stale_signature_counter_is_rejected(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;
    logout(&mut app).await;

    // The server has seen counter 5; the authenticator now answers with 1, as a clone would.
    sqlx::query("update passkeys set passkey = jsonb_set(passkey, '{cred,counter}', '5')")
        .execute(&pool)
        .await
        .unwrap();
    let (challenge_id, options) = login_start(&app).await;
    let cred = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
    assert_invalid_passkey(&login_finish(&app, &challenge_id, &cred).await);
    app.server
        .get("/api/v1/me")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // A possible clone is exactly what an operator wants to see.
    assert_eq!(
        failed_logins(&pool).await,
        vec![(
            Some(reg.user_id.to_string()),
            json!({ "method": "passkey", "reason": "counter-not-advanced" })
        )]
    );
}

#[sqlx::test]
async fn relying_party_comes_from_public_url(pool: PgPool) {
    // The host only: no scheme, no port, no path.
    let app = TestApp::with_config(pool.clone(), |c| {
        c.public_url = "https://nm.example.com:8443/".parse().unwrap();
    })
    .await;
    let (_, options) = login_start(&app).await;
    assert_eq!(options["rpId"], "nm.example.com");

    // The registration side too, with the fixed relying-party name.
    create_user(&pool, EMAIL, PW, false).await;
    let mut app = TestApp::with_config(pool.clone(), |c| {
        c.public_url = "http://localhost:8080".parse().unwrap();
    })
    .await;
    app.login(EMAIL, PW).await;
    let start = register_start(&app, "x").await.json::<Value>();
    assert_eq!(
        start["options"]["rp"],
        json!({ "name": "NM", "id": "localhost" })
    );
}

#[sqlx::test]
async fn an_ip_public_url_disables_passkeys_without_breaking_startup(pool: PgPool) {
    create_user(&pool, EMAIL, PW, false).await;
    let mut app = TestApp::with_config(pool.clone(), |c| {
        c.public_url = "http://127.0.0.1:8080".parse().unwrap();
    })
    .await;
    app.login(EMAIL, PW).await;
    for r in [
        register_start(&app, "x").await,
        app.server.post("/api/v1/auth/passkey/login/start").await,
    ] {
        r.assert_status(StatusCode::CONFLICT);
        assert_eq!(
            r.json::<Value>()["type"],
            "urn:nm:problem:passkeys-unavailable"
        );
    }
}

#[sqlx::test]
async fn concurrent_finishes_cannot_both_spend_a_challenge(pool: PgPool) {
    let mut app = TestApp::new(pool.clone()).await;
    let (mut soft, reg) = user_with_passkey(&mut app).await;
    logout(&mut app).await;

    let (challenge_id, options) = login_start(&app).await;
    let cred = assertion(&mut soft, ORIGIN, options, &reg, reg.user_id);
    let (a, b, c, d) = tokio::join!(
        login_finish(&app, &challenge_id, &cred),
        login_finish(&app, &challenge_id, &cred),
        login_finish(&app, &challenge_id, &cred),
        login_finish(&app, &challenge_id, &cred),
    );
    let ok = [a, b, c, d]
        .iter()
        .filter(|r| r.status_code() == StatusCode::OK)
        .count();
    assert_eq!(ok, 1);
    let sessions: i64 = sqlx::query_scalar("select count(*) from sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
}

#[sqlx::test]
async fn login_start_is_capped_per_address(pool: PgPool) {
    let app = TestApp::with_config(pool.clone(), |c| c.trust_proxy_headers = true).await;
    let start = |ip: &'static str| {
        app.server
            .post("/api/v1/auth/passkey/login/start")
            .add_header("x-forwarded-for", ip)
    };
    for _ in 0..20 {
        start("203.0.113.1").await.assert_status_ok();
    }
    let over = start("203.0.113.1").await;
    over.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(over.header("retry-after"), "60");
    assert_eq!(over.json::<Value>()["type"], "urn:nm:problem:rate-limited");
    // Refused starts store nothing; another address is untouched.
    start("203.0.113.2").await.assert_status_ok();
    let stored: i64 = sqlx::query_scalar("select count(*) from auth_challenges")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 21);

    // Expired challenges stop counting.
    sqlx::query("update auth_challenges set expires_at = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .unwrap();
    start("203.0.113.1").await.assert_status_ok();

    // The cap is on its own: it shares nothing with password sign-in's attempt log.
    let attempts: i64 = sqlx::query_scalar("select count(*) from login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
}
