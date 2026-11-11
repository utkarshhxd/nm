// SPDX-License-Identifier: AGPL-3.0-only

//! The eligibility predicate: whether an address may be emailed. Two paths, both covered: the
//! view `mailable_list_members` (the set path) and `suppressions::is_suppressed` (the
//! single-address path, the only one that sees an erasure tombstone).

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::crypto::suppression_hmac;
use nm::error::ApiError;
use nm::suppressions::{Reason, Scope, is_suppressed};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let email = format!("admin-{slug}@example.com");
    let id = create_user(pool, &email, PW, false).await;
    add_member(pool, ws, id, Role::Admin).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(&email, PW).await;
    (ws, app)
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn create_list(app: &TestApp, ws: Uuid, name: &str, double_optin: bool) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/lists"))
        .json(&json!({"name": name, "double_optin": double_optin}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn create_contact(app: &TestApp, ws: Uuid, email: &str) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/contacts"))
        .json(&json!({"email": email}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// Adds the contact and returns the status the API reports.
async fn add(app: &TestApp, ws: Uuid, list: Uuid, contact: Uuid) -> String {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/lists/{list}/members"))
        .json(&json!({"contact_id": contact}))
        .await;
    r.assert_status_ok();
    r.json::<Value>()["status"].as_str().unwrap().to_owned()
}

/// A contact on a single-opt-in list, so it is `subscribed` and its only obstacle is a suppression.
async fn subscribed(app: &TestApp, ws: Uuid, list: Uuid, email: &str) -> Uuid {
    let contact = create_contact(app, ws, email).await;
    assert_eq!(add(app, ws, list, contact).await, "subscribed");
    contact
}

/// The set path, read straight from the view.
async fn mailable(pool: &PgPool, list: Uuid) -> Vec<String> {
    sqlx::query_scalar("select email from mailable_list_members where list_id = $1 order by email")
        .bind(list)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The single-address path.
async fn blocked(app: &TestApp, ws: Uuid, email: &str) -> bool {
    is_suppressed(&app.pool, &app.state.secrets, ws, email)
        .await
        .unwrap()
}

async fn count_via_api(app: &TestApp, ws: Uuid, list: Uuid) -> u64 {
    let r = app
        .get(&format!(
            "/api/v1/workspaces/{ws}/lists/{list}/mailable-count"
        ))
        .await;
    r.assert_status_ok();
    r.json::<Value>()["count"].as_u64().unwrap()
}

/// Writes a suppression row directly, which is the only way to make an erasure tombstone: the API
/// refuses `gdpr_erasure`. `workspace` `None` is instance-wide.
async fn suppress(
    app: &TestApp,
    workspace: Option<Uuid>,
    scope: Scope,
    value: &str,
    reason: Reason,
) {
    let (email, domain, hmac) = match (scope, reason) {
        (Scope::Address, Reason::GdprErasure) => (
            None,
            None,
            Some(suppression_hmac(&app.state.secrets, value)),
        ),
        (Scope::Address, _) => (
            Some(value),
            None,
            Some(suppression_hmac(&app.state.secrets, value)),
        ),
        (Scope::Domain, _) => (None, Some(value), None),
    };
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, domain, email_hmac, reason) \
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace)
    .bind(scope)
    .bind(email)
    .bind(domain)
    .bind(hmac.as_ref().map(<[u8; 32]>::as_slice))
    .bind(reason)
    .execute(&app.pool)
    .await
    .unwrap();
}

/// Every reason. Adding a variant to `Reason` fails the exhaustive `match` in the table test below
/// until the new reason is given an expectation there; it is then added here to be exercised.
const ALL_REASONS: [Reason; 5] = [
    Reason::UnsubscribedAll,
    Reason::Bounced,
    Reason::Complained,
    Reason::Manual,
    Reason::GdprErasure,
];

#[sqlx::test]
async fn suppressed_contacts_are_never_mailable(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    subscribed(&app, ws, list, "sentinel@clean.test").await;

    let mut n = 0;
    for reason in ALL_REASONS {
        // No wildcard arm, on purpose: a new `Reason` must be classified here before this compiles.
        let (scopes, view_sees_it): (&[Scope], bool) = match reason {
            Reason::UnsubscribedAll | Reason::Bounced | Reason::Complained | Reason::Manual => {
                (&[Scope::Address, Scope::Domain], true)
            }
            // A tombstone has no plaintext, so the view cannot match it (in real life the contact
            // row is gone); only the single-address path can. The table forbids it on a domain.
            Reason::GdprErasure => (&[Scope::Address], false),
        };
        for &scope in scopes {
            for workspace_scope in [true, false] {
                n += 1;
                let email = format!("p{n}@d{n}.test");
                let case = format!("{reason:?} {scope:?} workspace_scope={workspace_scope}");
                subscribed(&app, ws, list, &email).await;
                // Control: before the block the contact is mailable, so the assertions below can
                // only be satisfied by the block itself.
                assert!(mailable(&pool, list).await.contains(&email), "{case}");
                assert!(!blocked(&app, ws, &email).await, "{case}");

                let value = match scope {
                    Scope::Address => email.clone(),
                    Scope::Domain => format!("d{n}.test"),
                };
                suppress(&app, workspace_scope.then_some(ws), scope, &value, reason).await;

                // The single-address path always agrees; it alone sees a tombstone.
                assert!(blocked(&app, ws, &email).await, "single path: {case}");
                assert_eq!(
                    !mailable(&pool, list).await.contains(&email),
                    view_sees_it,
                    "set path: {case}"
                );
            }
        }
    }
    // 4 reasons x 2 scopes x 2 workspace/instance, plus the tombstone x 2: nothing silently skipped.
    assert_eq!(n, 4 * 2 * 2 + 2);
    // And the view is not simply empty: the clean contact is still mailable.
    assert!(
        mailable(&pool, list)
            .await
            .contains(&"sentinel@clean.test".to_owned())
    );
}

#[sqlx::test]
async fn pending_membership_is_not_mailable(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", true).await;
    let c = create_contact(&app, ws, "a@ex.test").await;
    assert_eq!(add(&app, ws, list, c).await, "pending");
    assert!(mailable(&pool, list).await.is_empty());
    assert_eq!(count_via_api(&app, ws, list).await, 0);
}

#[sqlx::test]
async fn unsubscribed_membership_is_not_mailable(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    let c = subscribed(&app, ws, list, "a@ex.test").await;
    assert_eq!(mailable(&pool, list).await, ["a@ex.test"]);
    app.delete(&format!("/api/v1/workspaces/{ws}/lists/{list}/members/{c}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(mailable(&pool, list).await.is_empty());
}

#[sqlx::test]
async fn subscribed_and_unsuppressed_is_mailable(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    subscribed(&app, ws, list, "a@ex.test").await;
    assert_eq!(mailable(&pool, list).await, ["a@ex.test"]);
    assert!(!blocked(&app, ws, "a@ex.test").await);
}

#[sqlx::test]
async fn lifted_suppression_restores_mailability(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    subscribed(&app, ws, list, "a@ex.test").await;
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/suppressions"))
        .json(&json!({"scope": "address", "email": "a@ex.test", "reason": "manual"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let sid = id_of(&r);
    assert!(mailable(&pool, list).await.is_empty());
    assert!(blocked(&app, ws, "a@ex.test").await);

    app.post(&format!("/api/v1/workspaces/{ws}/suppressions/{sid}/lift"))
        .await
        .assert_status_ok();
    assert_eq!(mailable(&pool, list).await, ["a@ex.test"]);
    assert!(!blocked(&app, ws, "a@ex.test").await);
}

#[sqlx::test]
async fn a_workspace_suppression_does_not_reach_another_workspace(pool: PgPool) {
    let (ws_a, a) = workspace(&pool, "acme").await;
    let (ws_b, b) = workspace(&pool, "other").await;
    let list_a = create_list(&a, ws_a, "news", false).await;
    let list_b = create_list(&b, ws_b, "news", false).await;
    subscribed(&a, ws_a, list_a, "x@ex.test").await;
    subscribed(&b, ws_b, list_b, "x@ex.test").await;
    suppress(&a, Some(ws_a), Scope::Address, "x@ex.test", Reason::Manual).await;
    suppress(&a, Some(ws_a), Scope::Domain, "ex.test", Reason::Manual).await;
    assert!(mailable(&pool, list_a).await.is_empty());
    assert_eq!(mailable(&pool, list_b).await, ["x@ex.test"]);
}

#[sqlx::test]
async fn mailable_count_matches_the_view(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    for e in [
        "a@ok.test",
        "b@ok.test",
        "c@ok.test",
        "d@bad.test",
        "e@bad.test",
    ] {
        subscribed(&app, ws, list, e).await;
    }
    suppress(&app, Some(ws), Scope::Address, "c@ok.test", Reason::Bounced).await;
    suppress(&app, None, Scope::Domain, "bad.test", Reason::Manual).await;
    // One pending member, which the count must also leave out.
    let pending = create_list(&app, ws, "double", true).await;
    let p = create_contact(&app, ws, "p@ok.test").await;
    add(&app, ws, pending, p).await;

    let direct: i64 = sqlx::query_scalar(
        "select count(*) from mailable_list_members where workspace_id = $1 and list_id = $2",
    )
    .bind(ws)
    .bind(list)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(direct, 2);
    assert_eq!(count_via_api(&app, ws, list).await, direct as u64);
    assert_eq!(count_via_api(&app, ws, pending).await, 0);
}

#[sqlx::test]
async fn mailable_count_is_scoped_to_the_workspace(pool: PgPool) {
    let (ws_a, a) = workspace(&pool, "acme").await;
    let (ws_b, b) = workspace(&pool, "other").await;
    let list_a = create_list(&a, ws_a, "news", false).await;
    let list_b = create_list(&b, ws_b, "news", false).await;
    let url = |ws: Uuid, l: Uuid| format!("/api/v1/workspaces/{ws}/lists/{l}/mailable-count");
    // Not a member of the workspace, and a list id from another workspace: both 404.
    a.get(&url(ws_b, list_b))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    a.get(&url(ws_a, list_b))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    a.get(&url(ws_a, Uuid::now_v7()))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    a.get(&url(ws_a, list_a)).await.assert_status_ok();
}

#[sqlx::test]
async fn creating_a_contact_checks_the_single_address_path(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news", false).await;
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/suppressions"))
        .json(&json!({"scope": "address", "email": "Bad@Ex.test", "reason": "complained"}))
        .await;
    r.assert_status(StatusCode::CREATED);

    // A merely suppressed address still creates (refusing is for erasure tombstones only)...
    let contact = create_contact(&app, ws, "bad@ex.test").await;
    assert!(blocked(&app, ws, "bad@ex.test").await);
    // ...and adding it to a list still writes the membership: the relationship is real. It is
    // just not eligible.
    assert_eq!(add(&app, ws, list, contact).await, "subscribed");
    let rows: i64 =
        sqlx::query_scalar("select count(*) from list_memberships where contact_id = $1")
            .bind(contact)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
    assert!(mailable(&pool, list).await.is_empty());
    assert_eq!(count_via_api(&app, ws, list).await, 0);
    // Nothing above lifted the suppression as a side effect.
    let live: i64 = sqlx::query_scalar("select count(*) from suppressions where lifted_at is null")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 1);
}

fn eligibility_url(ws: Uuid, contact: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/contacts/{contact}/eligibility")
}

async fn eligibility(app: &TestApp, ws: Uuid, contact: Uuid) -> Value {
    let r = app.get(&eligibility_url(ws, contact)).await;
    r.assert_status_ok();
    r.json()
}

#[sqlx::test]
async fn eligibility_of_an_unblocked_contact_is_mailable(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let c = create_contact(&app, ws, "a@ex.test").await;
    assert_eq!(
        eligibility(&app, ws, c).await,
        json!({"mailable": true, "blocked_by": null})
    );
}

#[sqlx::test]
async fn eligibility_names_the_reason_and_the_level_for_every_combination(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let mut n = 0;
    for reason in ALL_REASONS {
        // A tombstone is address-scope only (the table forbids it on a domain).
        let scopes: &[Scope] = if reason == Reason::GdprErasure {
            &[Scope::Address]
        } else {
            &[Scope::Address, Scope::Domain]
        };
        for &scope in scopes {
            for workspace_scope in [true, false] {
                n += 1;
                let email = format!("p{n}@d{n}.test");
                let c = create_contact(&app, ws, &email).await;
                assert_eq!(eligibility(&app, ws, c).await["mailable"], true);
                let value = match scope {
                    Scope::Address => email.clone(),
                    Scope::Domain => format!("d{n}.test"),
                };
                suppress(&app, workspace_scope.then_some(ws), scope, &value, reason).await;
                assert_eq!(
                    eligibility(&app, ws, c).await,
                    json!({"mailable": false, "blocked_by": {
                        "reason": reason,
                        "level": if workspace_scope { "workspace" } else { "instance" },
                        "scope": scope,
                    }}),
                    "{reason:?} {scope:?} workspace_scope={workspace_scope}"
                );
            }
        }
    }
    assert_eq!(n, 4 * 2 * 2 + 2);
}

/// The case that would otherwise show a workspace admin nothing: the block is instance-wide, and
/// the workspace's own suppression list does not contain it.
#[sqlx::test]
async fn an_instance_wide_block_is_explained_though_the_workspace_list_is_empty(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let c = create_contact(&app, ws, "x@ex.test").await;
    suppress(&app, None, Scope::Address, "x@ex.test", Reason::Complained).await;
    let own = app
        .get(&format!("/api/v1/workspaces/{ws}/suppressions"))
        .await;
    own.assert_status_ok();
    assert_eq!(own.json::<Value>()["data"], json!([]));
    assert_eq!(
        eligibility(&app, ws, c).await["blocked_by"],
        json!({"reason": "complained", "level": "instance", "scope": "address"})
    );
}

#[sqlx::test]
async fn a_workspace_block_is_reported_before_an_instance_one(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let c = create_contact(&app, ws, "x@ex.test").await;
    suppress(&app, None, Scope::Domain, "ex.test", Reason::Manual).await;
    suppress(&app, Some(ws), Scope::Address, "x@ex.test", Reason::Bounced).await;
    assert_eq!(
        eligibility(&app, ws, c).await["blocked_by"],
        json!({"reason": "bounced", "level": "workspace", "scope": "address"})
    );
}

#[sqlx::test]
async fn a_lifted_block_no_longer_explains_anything(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let c = create_contact(&app, ws, "x@ex.test").await;
    suppress(&app, Some(ws), Scope::Address, "x@ex.test", Reason::Manual).await;
    sqlx::query("update suppressions set lifted_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(eligibility(&app, ws, c).await["mailable"], true);
}

#[sqlx::test]
async fn eligibility_is_scoped_to_the_workspace_and_a_viewer_may_read_it(pool: PgPool) {
    let (ws_a, a) = workspace(&pool, "acme").await;
    let (ws_b, b) = workspace(&pool, "other").await;
    let in_a = create_contact(&a, ws_a, "x@ex.test").await;
    let in_b = create_contact(&b, ws_b, "y@ex.test").await;
    // Another workspace, and another workspace's contact id in mine: both 404.
    a.get(&eligibility_url(ws_b, in_b))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    a.get(&eligibility_url(ws_a, in_b))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    a.get(&eligibility_url(ws_a, Uuid::now_v7()))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let viewer = create_user(&pool, "viewer@example.com", PW, false).await;
    add_member(&pool, ws_a, viewer, Role::Viewer).await;
    let mut v = TestApp::new(pool.clone()).await;
    v.login("viewer@example.com", PW).await;
    v.get(&eligibility_url(ws_a, in_a)).await.assert_status_ok();
}

#[sqlx::test]
async fn an_address_that_will_not_normalise_is_an_error_not_a_go_ahead(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let secrets = &app.state.secrets;
    // `is_suppressed` fails closed as `true`; `blocking_suppression` cannot name a block for it, so
    // it must not answer `None`.
    assert!(
        is_suppressed(&pool, secrets, ws, "no spaces@allowed")
            .await
            .unwrap()
    );
    let err = nm::suppressions::blocking_suppression(&pool, secrets, ws, "no spaces@allowed")
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Internal(_)));
    let ApiError::Internal(cause) = err else {
        unreachable!()
    };
    assert!(!cause.to_string().contains("spaces"), "{cause}");
}
