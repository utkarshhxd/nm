// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use nm::{crypto::suppression_hmac, suppressions::is_suppressed, users::normalize_email};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn client(pool: &PgPool, email: &str, ws: Option<(Uuid, Role)>, admin: bool) -> TestApp {
    let id = create_user(pool, email, PW, admin).await;
    if let Some((ws, role)) = ws {
        add_member(pool, ws, id, role).await;
    }
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in member of each role.
struct Ws {
    id: Uuid,
    viewer: TestApp,
    editor: TestApp,
    admin: TestApp,
}

async fn workspace(pool: &PgPool, slug: &str) -> Ws {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let id = create_workspace(pool, owner, slug).await;
    let member = |role: Role| {
        let email = format!("{role:?}-{slug}@example.com").to_lowercase();
        async move { client(pool, &email, Some((id, role)), false).await }
    };
    Ws {
        id,
        viewer: member(Role::Viewer).await,
        editor: member(Role::Editor).await,
        admin: member(Role::Admin).await,
    }
}

fn path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/suppressions")
}

fn lift_path(ws: Uuid, id: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/suppressions/{id}/lift")
}

const ADMIN: &str = "/api/v1/admin/suppressions";

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

async fn add_address(app: &TestApp, url: &str, email: &str, reason: &str) -> Uuid {
    let r = app
        .post(url)
        .json(&json!({"scope": "address", "email": email, "reason": reason}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn add_domain(app: &TestApp, url: &str, domain: &str) -> Uuid {
    let r = app
        .post(url)
        .json(&json!({"scope": "domain", "domain": domain, "reason": "manual"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn blocked(app: &TestApp, ws: Uuid, email: &str) -> bool {
    is_suppressed(&app.pool, &app.state.secrets, ws, email)
        .await
        .unwrap()
}

#[sqlx::test]
async fn hmac_is_stable_across_unicode_forms(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    // "é" as one code point (NFC) and as "e" plus a combining accent (NFD).
    let (nfc, nfd) = ("Jos\u{e9}@Ex.test", "Jose\u{301}@EX.test");
    assert_ne!(nfc.to_lowercase(), nfd.to_lowercase());
    let secrets = &w.editor.state.secrets;
    assert_eq!(
        suppression_hmac(secrets, &normalize_email(nfc).unwrap()),
        suppression_hmac(secrets, &normalize_email(nfd).unwrap()),
    );

    // And end to end: the row made from one form blocks the other.
    add_address(&w.editor, &path(w.id), nfc, "manual").await;
    assert!(blocked(&w.editor, w.id, nfd).await);
    assert!(blocked(&w.editor, w.id, "JOSE\u{301}@ex.test").await);
}

#[sqlx::test]
async fn address_scope_blocks_that_address(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let other = workspace(&pool, "other").await;
    add_address(&w.editor, &path(w.id), "Bad@Ex.test", "complained").await;
    assert!(blocked(&w.editor, w.id, "bad@ex.test").await);
    assert!(blocked(&w.editor, w.id, "  BAD@ex.test ").await);
    assert!(!blocked(&w.editor, w.id, "good@ex.test").await);
    // A workspace block is that workspace's own.
    assert!(!blocked(&w.editor, other.id, "bad@ex.test").await);
}

#[sqlx::test]
async fn domain_scope_blocks_every_address_on_it(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    add_domain(&w.editor, &path(w.id), "Spam.Test").await;
    assert!(blocked(&w.editor, w.id, "a@spam.test").await);
    assert!(blocked(&w.editor, w.id, "b@spam.test").await);
    // The exact domain only: neither a subdomain nor a longer name is the same domain.
    assert!(!blocked(&w.editor, w.id, "a@mail.spam.test").await);
    assert!(!blocked(&w.editor, w.id, "a@notspam.test").await);
    // Stored as normalised, and a domain row holds no email and no hash.
    let (domain, email, hmac): (String, Option<String>, Option<Vec<u8>>) =
        sqlx::query_as("select domain, email, email_hmac from suppressions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((domain.as_str(), email, hmac), ("spam.test", None, None));
}

#[sqlx::test]
async fn instance_scope_blocks_in_every_workspace(pool: PgPool) {
    let a = workspace(&pool, "acme").await;
    let b = workspace(&pool, "other").await;
    let root = client(&pool, "root@example.com", None, true).await;
    add_address(&root, ADMIN, "bad@ex.test", "complained").await;
    add_domain(&root, ADMIN, "spam.test").await;
    for ws in [a.id, b.id] {
        assert!(blocked(&root, ws, "bad@ex.test").await);
        assert!(blocked(&root, ws, "anyone@spam.test").await);
        assert!(!blocked(&root, ws, "good@ex.test").await);
    }
    // The workspace's own list does not carry instance rows.
    let page: Value = a.viewer.get(&path(a.id)).await.json();
    assert_eq!(page["data"].as_array().unwrap().len(), 0);
}

#[sqlx::test]
async fn lifting_unblocks_and_keeps_the_row(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let id = add_address(&w.editor, &path(w.id), "a@ex.test", "bounced").await;
    assert!(blocked(&w.editor, w.id, "a@ex.test").await);

    let r = w.admin.post(&lift_path(w.id, id)).await;
    r.assert_status_ok();
    assert!(r.json::<Value>()["lifted_at"].is_string());
    assert!(!blocked(&w.editor, w.id, "a@ex.test").await);

    // Kept, not deleted: the history is the point.
    let (lifted, by): (bool, Option<Uuid>) = sqlx::query_as(
        "select lifted_at is not null, lifted_by_user_id from suppressions where id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(lifted && by.is_some());

    // Hidden from the default list, shown with `include_lifted`.
    let live: Value = w.viewer.get(&path(w.id)).await.json();
    assert_eq!(live["data"].as_array().unwrap().len(), 0);
    let all: Value = w
        .viewer
        .get(&format!("{}?include_lifted=true", path(w.id)))
        .await
        .json();
    assert_eq!(all["data"].as_array().unwrap().len(), 1);

    // Lifting a second time says it is already lifted.
    let r = w.admin.post(&lift_path(w.id, id)).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        problem_type(&r),
        "urn:nm:problem:suppression-already-lifted"
    );
}

#[sqlx::test]
async fn suppressing_again_after_a_lift_is_allowed(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let first = add_address(&w.editor, &path(w.id), "a@ex.test", "manual").await;

    // While live, the same address is a conflict (whatever the reason).
    let r = w
        .editor
        .post(&path(w.id))
        .json(&json!({"scope": "address", "email": "A@ex.test", "reason": "complained"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:suppression-exists");

    w.admin
        .post(&lift_path(w.id, first))
        .await
        .assert_status_ok();
    let second = add_address(&w.editor, &path(w.id), "a@ex.test", "complained").await;
    assert_ne!(first, second);
    assert!(blocked(&w.editor, w.id, "a@ex.test").await);
    let rows: i64 = sqlx::query_scalar("select count(*) from suppressions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);
}

#[sqlx::test]
async fn editor_can_add_but_not_lift(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let id = add_address(&w.editor, &path(w.id), "a@ex.test", "manual").await;

    let r = w.editor.post(&lift_path(w.id, id)).await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    // The refusal changed nothing: still blocked, still live.
    assert!(blocked(&w.editor, w.id, "a@ex.test").await);
    let lifted: bool = sqlx::query_scalar("select lifted_at is not null from suppressions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!lifted);
}

/// Every write route refuses a viewer, so a dropped role check cannot pass the suite.
#[sqlx::test]
async fn a_viewer_is_refused_on_every_write_route_and_can_read(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let id = add_address(&w.editor, &path(w.id), "a@ex.test", "manual").await;

    let r = w
        .viewer
        .post(&path(w.id))
        .json(&json!({"scope": "address", "email": "b@ex.test", "reason": "manual"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    let r = w.viewer.post(&lift_path(w.id, id)).await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    let rows: i64 = sqlx::query_scalar("select count(*) from suppressions where lifted_at is null")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    // But reading is a viewer's.
    let page: Value = w.viewer.get(&path(w.id)).await.json();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
}

#[sqlx::test]
async fn lift_is_audited_with_the_original_reason(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let id = add_address(
        &w.editor,
        &path(w.id),
        "secret-person@ex.test",
        "complained",
    )
    .await;
    w.admin.post(&lift_path(w.id, id)).await.assert_status_ok();

    let (target_type, target_id, metadata): (String, String, Value) = sqlx::query_as(
        "select target_type, target_id, metadata from audit_log where action = 'suppression.lifted'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(target_type, "suppression");
    assert_eq!(target_id, id.to_string());
    assert_eq!(metadata["reason"], "complained");
    assert_eq!(metadata["scope"], "address");
    // The audit log outlives an erasure, so it never carries the address.
    let all: Vec<String> = sqlx::query_scalar("select metadata::text from audit_log")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(all.iter().all(|m| !m.contains("secret-person")));
    let created: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'suppression.created'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(created, 1);
}

#[sqlx::test]
async fn domain_row_with_an_email_is_rejected(pool: PgPool) {
    let id = || Uuid::now_v7();
    let attempts: [(&str, Result<_, _>); 4] = [
        (
            "domain with an email",
            sqlx::query(
                "insert into suppressions (id, scope, domain, email, reason) \
                 values ($1, 'domain', 'ex.test', 'a@ex.test', 'manual')",
            )
            .bind(id())
            .execute(&pool)
            .await,
        ),
        (
            "domain with a hash",
            sqlx::query(
                "insert into suppressions (id, scope, domain, email_hmac, reason) \
                 values ($1, 'domain', 'ex.test', '\\x00', 'manual')",
            )
            .bind(id())
            .execute(&pool)
            .await,
        ),
        (
            "address with a domain",
            sqlx::query(
                "insert into suppressions (id, scope, domain, email, email_hmac, reason) \
                 values ($1, 'address', 'ex.test', 'a@ex.test', '\\x00', 'manual')",
            )
            .bind(id())
            .execute(&pool)
            .await,
        ),
        (
            "address with no hash",
            sqlx::query(
                "insert into suppressions (id, scope, email, reason) \
                 values ($1, 'address', 'a@ex.test', 'manual')",
            )
            .bind(id())
            .execute(&pool)
            .await,
        ),
    ];
    for (what, result) in attempts {
        let err = result.expect_err(what);
        assert!(
            err.as_database_error()
                .is_some_and(|d| d.is_check_violation()),
            "{what}: {err}"
        );
    }
    // An erasure tombstone keeps no plaintext, whatever else it says.
    let err = sqlx::query(
        "insert into suppressions (id, scope, email, email_hmac, reason) \
         values ($1, 'address', 'a@ex.test', '\\x00', 'gdpr_erasure')",
    )
    .bind(id())
    .execute(&pool)
    .await
    .expect_err("tombstone with plaintext");
    assert!(err.as_database_error().unwrap().is_check_violation());
    let rows: i64 = sqlx::query_scalar("select count(*) from suppressions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[sqlx::test]
async fn cross_workspace_suppression_is_404(pool: PgPool) {
    let a = workspace(&pool, "acme").await;
    let b = workspace(&pool, "other").await;
    let id = add_address(&b.editor, &path(b.id), "a@ex.test", "manual").await;

    // A member of `acme` is no member of `other`: every route on its path is a 404.
    a.admin.get(&path(b.id)).await.assert_status_not_found();
    a.admin
        .post(&path(b.id))
        .json(&json!({"scope": "address", "email": "x@ex.test", "reason": "manual"}))
        .await
        .assert_status_not_found();
    a.admin
        .post(&lift_path(b.id, id))
        .await
        .assert_status_not_found();
    // And through its own workspace's path, another workspace's row is not found either.
    a.admin
        .post(&lift_path(a.id, id))
        .await
        .assert_status_not_found();
    assert!(blocked(&a.editor, b.id, "a@ex.test").await);
    assert!(!blocked(&a.editor, a.id, "a@ex.test").await);
}

#[sqlx::test]
async fn instance_routes_take_an_instance_admin_only(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let root = client(&pool, "root@example.com", None, true).await;
    let id = add_address(&root, ADMIN, "a@ex.test", "manual").await;
    let lift = format!("{ADMIN}/{id}/lift");
    let body = json!({"scope": "address", "email": "b@ex.test", "reason": "manual"});

    // A workspace owner-level role is not instance admin.
    for app in [&w.viewer, &w.editor, &w.admin] {
        for r in [
            app.get(ADMIN).await,
            app.post(ADMIN).json(&body).await,
            app.post(&lift).await,
        ] {
            r.assert_status(StatusCode::FORBIDDEN);
            assert_eq!(problem_type(&r), "urn:nm:problem:instance-admin-required");
        }
    }
    let rows: i64 = sqlx::query_scalar("select count(*) from suppressions where lifted_at is null")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    // The instance list shows it; a workspace row is not on it, and cannot be lifted there.
    let ws_row = add_address(&w.editor, &path(w.id), "c@ex.test", "manual").await;
    let page: Value = root.get(ADMIN).await.json();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["workspace_id"], Value::Null);
    root.post(&format!("{ADMIN}/{ws_row}/lift"))
        .await
        .assert_status_not_found();
    // And an instance row is not liftable through a workspace's route.
    w.admin
        .post(&lift_path(w.id, id))
        .await
        .assert_status_not_found();

    root.post(&lift).await.assert_status_ok();
    assert!(!blocked(&root, w.id, "a@ex.test").await);
    let metadata: Value =
        sqlx::query_scalar("select metadata from audit_log where action = 'suppression.lifted'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(metadata["reason"], "manual");
}

#[sqlx::test]
async fn an_erasure_tombstone_blocks_by_hash_and_cannot_be_lifted(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let hmac = suppression_hmac(&w.editor.state.secrets, "gone@ex.test");
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email_hmac, reason) \
         values ($1, $2, 'address', $3, 'gdpr_erasure')",
    )
    .bind(id)
    .bind(w.id)
    .bind(hmac.as_slice())
    .execute(&pool)
    .await
    .unwrap();
    // No plaintext anywhere in the row, and still blocked: this is why the hash exists.
    assert!(blocked(&w.editor, w.id, "Gone@EX.test").await);

    // No role lifts it, and the API cannot make one.
    let r = w.admin.post(&lift_path(w.id, id)).await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:erasure-not-liftable");
    let r = w
        .admin
        .post(&path(w.id))
        .json(&json!({"scope": "address", "email": "x@ex.test", "reason": "gdpr_erasure"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(blocked(&w.editor, w.id, "gone@ex.test").await);

    // The list shows the tombstone, with no address to show.
    let page: Value = w.viewer.get(&path(w.id)).await.json();
    assert_eq!(page["data"][0]["reason"], "gdpr_erasure");
    assert_eq!(page["data"][0]["email"], Value::Null);
}

#[sqlx::test]
async fn an_address_that_cannot_be_normalised_counts_as_suppressed(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    // Fail closed: a lookup that cannot tell who it is asking about must not say "mail them".
    assert!(blocked(&w.editor, w.id, "not an address").await);
    assert!(blocked(&w.editor, w.id, "").await);
}

#[sqlx::test]
async fn no_response_carries_the_hash(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let root = client(&pool, "root@example.com", None, true).await;
    let r = w
        .editor
        .post(&path(w.id))
        .json(&json!({"scope": "address", "email": "a@ex.test", "reason": "manual", "note": "x"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let created = r.text();
    add_address(&root, ADMIN, "b@ex.test", "manual").await;
    let listed = w.viewer.get(&path(w.id)).await.text();
    let instance = root.get(ADMIN).await.text();
    for body in [created, listed, instance] {
        assert!(!body.contains("hmac"), "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        let row = v.get("data").map_or(&v, |d| &d[0]);
        assert!(row.get("email_hmac").is_none());
    }
}

#[sqlx::test]
async fn create_validates_its_input(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    for body in [
        json!({"scope": "address", "reason": "manual"}),
        json!({"scope": "address", "email": "nope", "reason": "manual"}),
        json!({"scope": "address", "email": "a@ex.test", "domain": "ex.test", "reason": "manual"}),
        json!({"scope": "domain", "reason": "manual"}),
        json!({"scope": "domain", "domain": "a@ex.test", "reason": "manual"}),
        json!({"scope": "domain", "domain": "nodot", "reason": "manual"}),
        json!({"scope": "domain", "domain": "ex.test", "email": "a@ex.test", "reason": "manual"}),
        json!({"scope": "address", "email": "a@ex.test", "reason": "manual", "note": "a\u{0}b"}),
        json!({"scope": "address", "email": "a@ex.test", "reason": "manual", "colour": "red"}),
        json!({"scope": "address", "email": "a@ex.test", "reason": "because"}),
    ] {
        w.editor
            .post(&path(w.id))
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let rows: i64 = sqlx::query_scalar("select count(*) from suppressions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// **The consent log could say that somebody opted out of everything and could never say that it
/// ended.** `public::unsubscribe::one_click` writes `suppressed` with a null `list_id` when a
/// subscriber opts out of everything; the identical fact set through this API wrote an `audit_log`
/// row and no consent row, and ending it wrote nothing in either log — the public routes write no
/// audit rows by design, and this route wrote no consent row. Both directions are recorded now,
/// `Resubscribed` with a null `list_id` being spec §7.1's word for the end of one.
///
/// Three negatives in the same fixture, because each is a reason the row must NOT be written and
/// each would otherwise be a silent over-reach: a reason that is not a workspace-wide opt-out, an
/// address that is nobody's contact here, and a domain-scope block, which is about no one person.
#[sqlx::test]
async fn an_operator_setting_or_ending_a_workspace_wide_opt_out_writes_a_consent_row(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let contact: Uuid = sqlx::query_scalar(
        "insert into contacts (id, workspace_id, email) values ($1, $2, 'a@ex.test') returning id",
    )
    .bind(Uuid::now_v7())
    .bind(w.id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let rows = |action: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "select count(*) from consent_log where contact_id = $1 and list_id is null \
                 and action = $2 and source = 'operator'",
            )
            .bind(contact)
            .bind(action)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    let id = add_address(&w.editor, &path(w.id), "a@ex.test", "unsubscribed_all").await;
    assert_eq!(rows("suppressed").await, 1, "the opt-out was set");
    w.admin.post(&lift_path(w.id, id)).await.assert_status_ok();
    assert_eq!(rows("resubscribed").await, 1, "and the log says it ended");

    // Not every suppression is a consent decision about one person in one workspace.
    let bounced = add_address(&w.editor, &path(w.id), "a@ex.test", "bounced").await;
    w.admin
        .post(&lift_path(w.id, bounced))
        .await
        .assert_status_ok();
    add_address(&w.editor, &path(w.id), "nobody@ex.test", "unsubscribed_all").await;
    w.editor
        .post(&path(w.id))
        .json(&json!({"scope": "domain", "domain": "ex.test", "reason": "unsubscribed_all"}))
        .await
        .assert_status(StatusCode::CREATED);
    let total: i64 = sqlx::query_scalar("select count(*) from consent_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        total, 2,
        "a bounce, an address with no contact and a domain block write no consent row"
    );
}
