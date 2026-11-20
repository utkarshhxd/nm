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

// ---- C-1/C-4 fix round 1: the two triggers that keep `list_name` honest ------------------------

use std::time::Duration;

use nm::{
    config::SecretKey,
    crypto::secrets::Secrets,
    suppressions::{lift_unsubscribed_list, unsubscribe_list},
};
use sqlx::PgConnection;

fn secrets() -> Secrets {
    Secrets::new(&SecretKey([7u8; 32]))
}

const ADA: &str = "ada@example.com";

async fn make_list(conn: &mut PgConnection, ws: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, $3)")
        .bind(id)
        .bind(ws)
        .bind(name)
        .execute(conn)
        .await
        .expect("insert list");
    id
}

/// Every live `list`-scope refusal, as `(list_id, list_name)`, oldest first.
async fn refusals(pool: &PgPool, ws: Uuid) -> Vec<(Option<Uuid>, Option<String>)> {
    sqlx::query_as(
        "select list_id, list_name from suppressions \
          where workspace_id = $1 and scope = 'list' and lifted_at is null order by id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .expect("read refusals")
}

/// **A rename that commits while somebody is unticking the box must not leave the old name
/// behind.** The refusal still carries `list_id`, so nothing looks wrong while the list lives —
/// and then deleting that list and recreating it under its current name finds no refusal of that
/// name to adopt, and the next import puts the person back on it. That is the defect the whole
/// `list` scope exists to close, and a plain `select … from lists` reintroduced it: the statement's
/// READ COMMITTED snapshot does not see the uncommitted rename, and the rename's trigger has
/// already run and will not come back for the row.
///
/// Deterministic in both directions. With `for key share` the save blocks until the rename commits
/// and then re-reads, so the stored name is necessarily the committed one; without it the save
/// does not block at all and stores the old name well before the rename lands.
#[sqlx::test]
async fn a_rename_that_commits_mid_save_does_not_leave_the_old_name_on_the_refusal(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let list = make_list(&mut conn, w.id, "news").await;
    drop(conn);

    // The operator's rename, begun and held open.
    let mut renaming = pool.begin().await.unwrap();
    sqlx::query("update lists set name = 'bulletin' where id = $1")
        .bind(list)
        .execute(&mut *renaming)
        .await
        .expect("rename the list");

    // The subscriber unticking the box, on its own connection.
    let saving = tokio::spawn({
        let pool = pool.clone();
        let ws = w.id;
        async move {
            let mut tx = pool.begin().await.unwrap();
            unsubscribe_list(&mut tx, &secrets(), ws, list, ADA)
                .await
                .expect("refuse one list");
            tx.commit().await.unwrap();
        }
    });

    // What makes the two overlap, so the check below is about a rename that landed mid-save and
    // not about one that was over before the save began. There is no assertion here: the save
    // blocks under both the fixed and the broken statement — at `for key share`, or later at the
    // foreign key's own check, which runs after the row has been built from the stale read and so
    // cannot change what it stored. The stored name is the only thing that tells them apart, which
    // is what the mutation check shows.
    tokio::time::sleep(Duration::from_millis(300)).await;

    renaming.commit().await.expect("commit the rename");
    saving.await.expect("the save");

    assert_eq!(
        refusals(&pool, w.id).await,
        [(Some(list), Some("bulletin".to_owned()))],
        "the refusal stored a name the list no longer has"
    );
}

/// **A rename cannot collide with anything, whatever else the address has refused.** No unique
/// index covering a live list refusal mentions `list_name`, so moving a snapshot is not a key
/// update and the trigger has no case to carve out. It used to: the live key was the name, so a
/// rename onto a name another live refusal by that address already held failed the operator with
/// `duplicate key value violates unique constraint "suppressions_live"`, and every attempt to
/// guard that left stale names behind that the next rename choked on.
///
/// The second refusal is made to hold the incoming name directly, because keying on the list means
/// nothing in the product produces that state any more — and the property is about the state, not
/// about one route to it.
#[sqlx::test]
async fn a_rename_cannot_collide_with_the_addresss_other_refusals(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let news = make_list(&mut conn, w.id, "news").await;
    let other = make_list(&mut conn, w.id, "other").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, news, ADA)
        .await
        .expect("refuse news");
    unsubscribe_list(&mut conn, &secrets(), w.id, other, ADA)
        .await
        .expect("refuse other");
    drop(conn);
    sqlx::query("update suppressions set list_name = 'bulletin' where list_id = $1")
        .bind(other)
        .execute(&pool)
        .await
        .expect("make one refusal stale");

    sqlx::query("update lists set name = 'bulletin' where id = $1")
        .bind(news)
        .execute(&pool)
        .await
        .expect("the operator's rename must not fail");

    // The refusal follows its list — no skipping, no stale snapshot left behind — and the other
    // refusal keeps its own `list_id`, which is the only thing any reader matches on, so neither
    // list stops being refused.
    assert_eq!(
        refusals(&pool, w.id).await,
        [
            (Some(news), Some("bulletin".to_owned())),
            (Some(other), Some("bulletin".to_owned()))
        ],
        "the renamed list's refusal must carry the name that list now has"
    );
}

/// **Important 2's sequence: a second untick after a rename must not steal the orphan that holds
/// the new name.** `unsubscribe_list` used to conflict on `list_name`, which cannot see the live
/// refusal already sitting on this list under a different name, so it conflicted with the ORPHAN
/// instead and re-pointed it. Two harms in one statement: two live refusals on one list, after
/// which every later rename of it failed permanently — and the address's refusal of the *deleted*
/// list of that name silently gone, so recreating one no longer restored it. That last part is the
/// delete-and-recreate resurrection this whole scope exists to close.
///
/// Step 4 is a second untick with the first refusal still live, which is what a re-subscribe by any
/// path that does not lift leaves behind — an operator adding the member, an import, a form
/// submission. The subscriber's own re-tick lifts, which is why this needs an operator to reach.
#[sqlx::test]
async fn a_second_untick_after_a_rename_does_not_steal_the_orphan_of_the_new_name(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let news = make_list(&mut conn, w.id, "News").await;
    let newsletter = make_list(&mut conn, w.id, "Newsletter").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, news, ADA)
        .await
        .expect("refuse News");
    unsubscribe_list(&mut conn, &secrets(), w.id, newsletter, ADA)
        .await
        .expect("refuse Newsletter");
    drop(conn);

    sqlx::query("delete from lists where id = $1")
        .bind(newsletter)
        .execute(&pool)
        .await
        .expect("delete Newsletter");
    sqlx::query("update lists set name = 'Newsletter' where id = $1")
        .bind(news)
        .execute(&pool)
        .await
        .expect("rename News");

    let mut conn = pool.acquire().await.unwrap();
    unsubscribe_list(&mut conn, &secrets(), w.id, news, ADA)
        .await
        .expect("untick it again");
    drop(conn);

    let rows = refusals(&pool, w.id).await;
    assert_eq!(
        rows.iter().filter(|(l, _)| *l == Some(news)).count(),
        1,
        "two live refusals on one list: {rows:?}"
    );
    assert!(
        rows.contains(&(None, Some("Newsletter".to_owned()))),
        "the refusal of the DELETED list of that name was taken over: {rows:?}"
    );

    // Both harms, from the other side: the list is still renamable, and recreating a list of the
    // orphan's name still restores the refusal that was waiting for it.
    sqlx::query("update lists set name = 'Digest' where id = $1")
        .bind(news)
        .execute(&pool)
        .await
        .expect("the list must still be renamable");
    let mut conn = pool.acquire().await.unwrap();
    let reborn = make_list(&mut conn, w.id, "Newsletter").await;
    drop(conn);
    assert!(
        refusals(&pool, w.id)
            .await
            .contains(&(Some(reborn), Some("Newsletter".to_owned()))),
        "a list recreated under the orphan's name must adopt the refusal waiting for it"
    );
}

/// **The re-subscribe half of the preference centre has to take the `lists` row before the refusal
/// it lifts.** The membership insert that follows takes `FOR KEY SHARE` on that row through its own
/// foreign key, so a lift that touched `suppressions` first would run the two in the opposite order
/// from a rename, which goes `lists` then `suppressions` through its trigger — and the two
/// deadlock. Reproduced against Postgres 16 before this: `deadlock detected` while locking a tuple
/// in `lists`, victim chosen by Postgres, so either a `500` on the public page or a failed rename.
///
/// The order is the requirement here, so the order is what is asserted: another session asking for
/// the same `lists` row must find it already held.
#[sqlx::test]
async fn a_re_tick_takes_the_list_row_before_the_refusal_it_lifts(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let list = make_list(&mut conn, w.id, "news").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, list, ADA)
        .await
        .expect("refuse news");
    drop(conn);

    let mut ticking = pool.begin().await.unwrap();
    lift_unsubscribed_list(&mut ticking, &secrets(), w.id, list, ADA)
        .await
        .expect("the subscriber comes back");

    // `for update` conflicts with the `for key share` the lift must be holding, and touches
    // nothing else, so the refusal row this transaction also holds cannot be what blocks it.
    let mut probe = pool.begin().await.unwrap();
    sqlx::query("set local lock_timeout = '250ms'")
        .execute(&mut *probe)
        .await
        .unwrap();
    let err = sqlx::query("select 1 from lists where id = $1 for update")
        .bind(list)
        .fetch_all(&mut *probe)
        .await
        .expect_err("the lift must already hold the list row");
    assert_eq!(
        err.as_database_error()
            .and_then(|e| e.code())
            .as_deref()
            .map(ToOwned::to_owned),
        Some("55P03".to_owned()),
        "expected the probe to time out waiting for the list row, got {err}"
    );
}

/// **Two renames over an orphaned refusal, which is where this used to leave debris that the next
/// rename choked on.** The first rename moves the refusal onto the name the orphan already holds;
/// if the orphan were then adopted onto the same list, the address would have two live refusals on
/// one list, which is what `suppressions_live_list` forbids and what the adoption's `not exists`
/// is there to keep satisfiable. The second rename is the one that used to fail.
#[sqlx::test]
async fn renaming_a_list_twice_over_an_orphaned_refusal_does_not_fail(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    // A refusal of a list called `bulletin`, whose list is then deleted: an orphan.
    let gone = make_list(&mut conn, w.id, "bulletin").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, gone, ADA)
        .await
        .expect("refuse bulletin");
    let news = make_list(&mut conn, w.id, "news").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, news, ADA)
        .await
        .expect("refuse news");
    drop(conn);
    sqlx::query("delete from lists where id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("delete the list");

    for name in ["bulletin", "digest"] {
        sqlx::query("update lists set name = $2 where id = $1")
            .bind(news)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("the operator's rename to {name} must not fail: {e}"));
    }

    let rows = refusals(&pool, w.id).await;
    assert_eq!(
        rows.iter().filter(|(l, _)| *l == Some(news)).count(),
        1,
        "one live refusal per address per list, or the next rename collides: {rows:?}"
    );
    assert!(
        rows.contains(&(None, Some("bulletin".to_owned()))),
        "and the orphan is still waiting for a list of its own name: {rows:?}"
    );
}

/// **A rename never restates the past.** `0014` keeps a lifted row rather than deleting it so the
/// blocklist can say what was once blocked, and for a list-scope row that is *which named list*
/// somebody refused and later rejoined. Renaming the list afterwards must not rewrite it.
#[sqlx::test]
async fn a_rename_does_not_rewrite_a_refusal_the_subscriber_already_lifted(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let list = make_list(&mut conn, w.id, "news").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, list, ADA)
        .await
        .expect("refuse news");
    lift_unsubscribed_list(&mut conn, &secrets(), w.id, list, ADA)
        .await
        .expect("the subscriber comes back");
    drop(conn);

    sqlx::query("update lists set name = 'bulletin' where id = $1")
        .bind(list)
        .execute(&pool)
        .await
        .expect("rename the list");

    let history: Vec<(String,)> = sqlx::query_as(
        "select list_name from suppressions where workspace_id = $1 and lifted_at is not null",
    )
    .bind(w.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        history,
        [("news".to_owned(),)],
        "the lifted row says which named list was refused, and a later rename is not that history"
    );
}

/// **Two orphans of one name for one address are allowed, and exactly one of them is adopted.**
/// They arise honestly: an address holding both a refusal of a dead list called X and a refusal of
/// a live list later renamed to X orphans into two rows named X when that list is deleted too.
/// Making that a unique violation would fail the operator's deletion from inside
/// `on delete set null`; adopting both would put two refusals on one list and violate
/// `suppressions_live_list` from inside the creation trigger. Neither may happen.
#[sqlx::test]
async fn a_list_born_over_two_orphans_of_its_name_adopts_one_of_them(pool: PgPool) {
    let w = workspace(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let gone = make_list(&mut conn, w.id, "bulletin").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, gone, ADA)
        .await
        .expect("refuse bulletin");
    let news = make_list(&mut conn, w.id, "news").await;
    unsubscribe_list(&mut conn, &secrets(), w.id, news, ADA)
        .await
        .expect("refuse news");
    drop(conn);
    sqlx::query("delete from lists where id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("delete bulletin");
    // The pair: a live refusal of `news`, now also called bulletin, beside the orphan of the dead
    // bulletin. The adoption declines here, which is what leaves two refusals naming one thing.
    sqlx::query("update lists set name = 'bulletin' where id = $1")
        .bind(news)
        .execute(&pool)
        .await
        .expect("rename news to bulletin");
    sqlx::query("delete from lists where id = $1")
        .bind(news)
        .execute(&pool)
        .await
        .expect("deleting the list must not fail over two orphans of one name");

    let orphans = refusals(&pool, w.id).await;
    assert_eq!(
        orphans.len(),
        2,
        "the premise: two live orphans of one name for one address: {orphans:?}"
    );
    assert!(
        orphans
            .iter()
            .all(|(l, n)| l.is_none() && n.as_deref() == Some("bulletin")),
        "{orphans:?}"
    );

    let mut conn = pool.acquire().await.unwrap();
    let reborn = make_list(&mut conn, w.id, "bulletin").await;
    drop(conn);

    let rows = refusals(&pool, w.id).await;
    assert_eq!(
        rows.iter().filter(|(l, _)| *l == Some(reborn)).count(),
        1,
        "exactly one orphan is adopted, or the list could not have been created: {rows:?}"
    );
    assert_eq!(rows.len(), 2, "and the other keeps waiting: {rows:?}");
}
