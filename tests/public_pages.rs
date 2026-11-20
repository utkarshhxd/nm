// SPDX-License-Identifier: AGPL-3.0-only

//! The public, token-authenticated pages: `GET /c/{token}` and the shell every one of them is
//! rendered into. `tests/public_tokens.rs` covers `issue`/`parse` on their own; this file is
//! about what a browser gets.

mod common;

use std::{net::IpAddr, path::Path};

use axum::http::StatusCode;
use chrono::Utc;
use common::{PW, TestApp, create_user, create_workspace};
use nm::{
    http::rate_limit::PUBLIC_PER_MINUTE,
    public::token::{Purpose, Token, issue},
};
use sqlx::PgPool;
use uuid::Uuid;

const DAY: i64 = 24 * 60 * 60;

/// A workspace with one contact holding one `pending` membership on one list: the state a
/// confirmation link is issued against.
struct Pending {
    app: TestApp,
    workspace: Uuid,
    contact: Uuid,
    list: Uuid,
    owner_email: String,
}

async fn pending(pool: &PgPool, slug: &str) -> Pending {
    let owner_email = format!("owner-{slug}@example.com");
    let owner = create_user(pool, &owner_email, PW, false).await;
    let workspace = create_workspace(pool, owner, slug).await;
    let contact = Uuid::now_v7();
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
        .bind(contact)
        .bind(workspace)
        .bind(format!("subscriber-{slug}@ex.test"))
        .execute(pool)
        .await
        .unwrap();
    let list = Uuid::now_v7();
    sqlx::query(
        "insert into lists (id, workspace_id, name, double_optin) values ($1, $2, $3, true)",
    )
    .bind(list)
    .bind(workspace)
    .bind("news")
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into list_memberships (workspace_id, contact_id, list_id, status) \
         values ($1, $2, $3, 'pending')",
    )
    .bind(workspace)
    .bind(contact)
    .bind(list)
    .execute(pool)
    .await
    .unwrap();
    Pending {
        app: TestApp::new(pool.clone()).await,
        workspace,
        contact,
        list,
        owner_email,
    }
}

impl Pending {
    /// A link for this membership, `age_days` old, with a nonce the caller can assert on.
    fn link(&self, nonce: &str, age_days: i64) -> String {
        self.token(Purpose::Confirm, Some(self.list), nonce, age_days)
    }

    fn token(&self, purpose: Purpose, list_id: Option<Uuid>, nonce: &str, age_days: i64) -> String {
        issue(
            &self.app.state.secrets,
            &Token {
                workspace_id: self.workspace,
                contact_id: self.contact,
                purpose,
                list_id,
                issued_at: Utc::now().timestamp() - age_days * DAY,
                nonce: nonce.to_owned(),
            },
        )
    }

    async fn status(&self) -> Option<String> {
        sqlx::query_scalar("select status from list_memberships where contact_id = $1")
            .bind(self.contact)
            .fetch_optional(&self.app.pool)
            .await
            .unwrap()
    }
}

/// `(action, source, ip, token_jti, source_ref)` of every consent row, oldest first.
type Evidence = (
    String,
    String,
    Option<IpAddr>,
    Option<String>,
    Option<String>,
);

async fn consent_rows(pool: &PgPool) -> Vec<Evidence> {
    sqlx::query_as("select action, source, ip, token_jti, source_ref from consent_log order by id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn confirming_a_pending_membership_subscribes_it(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let r = p.app.get(&format!("/c/{}", p.link("n1", 0))).await;
    r.assert_status_ok();
    assert!(r.text().contains("subscribed"), "{}", r.text());

    assert_eq!(p.status().await.as_deref(), Some("subscribed"));
    let at: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("select subscribed_at from list_memberships where contact_id = $1")
            .bind(p.contact)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(at.is_some(), "subscribed_at is stamped");

    // The subscriber's own request, so their IP is the evidence — the one place in the product
    // where the request's address belongs to the person whose consent it is.
    let rows = consent_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "confirmed");
    assert_eq!(rows[0].1, "form");
    assert_eq!(rows[0].2, Some("127.0.0.1".parse().unwrap()));
}

#[sqlx::test]
async fn confirming_twice_confirms_once_and_says_already_confirmed(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let link = p.link("n1", 0);
    p.app.get(&format!("/c/{link}")).await.assert_status_ok();

    let again = p.app.get(&format!("/c/{link}")).await;
    again.assert_status_ok();
    assert!(
        again.text().contains("Already confirmed"),
        "{}",
        again.text()
    );
    // Invariant 5: one consent row, not two. The second visit changed nothing, so it recorded
    // nothing — a second row would read as a second act of consent that never happened.
    assert_eq!(consent_rows(&pool).await.len(), 1);
    assert_eq!(p.status().await.as_deref(), Some("subscribed"));
}

#[sqlx::test]
async fn the_confirmation_row_carries_the_token_nonce_as_its_jti(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let nonce = Uuid::now_v7().to_string();
    p.app
        .get(&format!("/c/{}", p.link(&nonce, 0)))
        .await
        .assert_status_ok();

    let rows = consent_rows(&pool).await;
    assert_eq!(rows[0].3, Some(nonce), "which link was followed");
    // And not the operator's id: nobody signed in made this change.
    assert_eq!(rows[0].4, None);
}

#[sqlx::test]
async fn an_expired_confirm_token_offers_to_resend_without_saying_whether_the_address_exists(
    pool: PgPool,
) {
    let p = pending(&pool, "acme").await;
    let real = p.app.get(&format!("/c/{}", p.link("n1", 15))).await;
    real.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(real.text().contains("expired"), "{}", real.text());
    assert!(real.text().contains("Sign up again"), "{}", real.text());

    // The same link age for a contact that does not exist in any workspace. The two answers are
    // byte-identical, so the page discloses nothing about the address (invariant 6) — it is
    // reached before any database access at all.
    let stranger = issue(
        &p.app.state.secrets,
        &Token {
            workspace_id: Uuid::now_v7(),
            contact_id: Uuid::now_v7(),
            purpose: Purpose::Confirm,
            list_id: Some(Uuid::now_v7()),
            issued_at: Utc::now().timestamp() - 15 * DAY,
            nonce: "n2".into(),
        },
    );
    let unknown = p.app.get(&format!("/c/{stranger}")).await;
    assert_eq!(unknown.status_code(), real.status_code());
    assert_eq!(unknown.text(), real.text());

    // And nothing happened to the membership the expired link named.
    assert_eq!(p.status().await.as_deref(), Some("pending"));
    assert_eq!(consent_rows(&pool).await.len(), 0);
}

#[sqlx::test]
async fn a_confirm_token_for_a_deleted_contact_says_the_link_is_no_longer_valid(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let link = p.link("n1", 0);
    sqlx::query("delete from contacts where id = $1")
        .bind(p.contact)
        .execute(&pool)
        .await
        .unwrap();

    let r = p.app.get(&format!("/c/{link}")).await;
    // Review Focus 2: the token still verifies, the foreign key is gone. A sentence, not a 500.
    assert_eq!(r.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.text().contains("no longer valid"), "{}", r.text());
}

#[sqlx::test]
async fn an_unsubscribe_token_is_refused_by_the_confirm_route(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    // Genuinely signed by this instance, and never expires. The purpose is the only thing
    // standing between it and a confirmation that could be replayed for ever.
    let other = p.token(Purpose::Unsubscribe, None, "n1", 400);
    let r = p.app.get(&format!("/c/{other}")).await;
    assert_eq!(r.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(p.status().await.as_deref(), Some("pending"));
    assert_eq!(consent_rows(&pool).await.len(), 0);
}

#[sqlx::test]
async fn a_session_cookie_on_a_public_route_changes_nothing(pool: PgPool) {
    let mut p = pending(&pool, "acme").await;
    // Review Focus 1: the operator opens their own confirmation link in the browser they are
    // signed in with. `TestServer` keeps cookies, so the session really is sent.
    let operator = p.owner_email.clone();
    p.app.login(&operator, PW).await;
    assert!(p.app.csrf.is_some(), "the premise: there is a session");

    let r = p.app.get(&format!("/c/{}", p.link("n1", 0))).await;
    r.assert_status_ok();
    assert_eq!(p.status().await.as_deref(), Some("subscribed"));

    // The token decided everything. Had the session been read, this row would carry the operator
    // — `Source::Operator` and their user id in `source_ref` — instead of the subscriber's act.
    let rows = consent_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "form");
    assert_eq!(rows[0].4, None, "no operator id");
    assert_eq!(
        rows[0].3,
        Some("n1".to_owned()),
        "the link, not the session"
    );
}

/// The real proof behind the test above, which cannot fail unless the bug is written. A handler
/// that never names the session cookie cannot read it.
#[test]
fn no_handler_under_src_public_names_cookiejar() {
    fn walk(dir: &Path, seen: &mut Vec<String>, offenders: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, seen, offenders);
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let name = p.to_string_lossy().replace('\\', "/");
            let name = name.rsplit_once("/src/").unwrap().1.to_owned();
            let source = std::fs::read_to_string(&p).unwrap();
            // Two needles, for the two ways a handler could reach the session. `cookiejar`
            // matched case-insensitively catches the extractor however it is spelled; `COOKIE`
            // matched case-**sensitively** catches `header::COOKIE` and a raw header read, while
            // leaving prose about cookies — like the sentence above — free to be written.
            if source.to_lowercase().contains("cookiejar") || source.contains("COOKIE") {
                offenders.push(name.clone());
            }
            seen.push(name);
        }
    }
    let mut seen = vec![];
    let mut offenders = vec![];
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/public"),
        &mut seen,
        &mut offenders,
    );
    // Without this the assertion below is true of an empty directory.
    assert!(seen.len() >= 4, "the walk found only {seen:?}");
    assert_eq!(
        offenders,
        Vec::<String>::new(),
        "a public handler names the session cookie; these routes are authenticated by their \
         token and a signed-in operator's browser must get the same answer as a stranger's"
    );
}

#[sqlx::test]
async fn every_public_page_is_no_store(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let pages = [
        ("confirmed", p.link("n1", 0)),
        ("already confirmed", p.link("n2", 0)),
        ("expired", p.link("n3", 15)),
        ("invalid", "not-a-token".to_owned()),
    ];
    for (what, link) in pages {
        let r = p.app.get(&format!("/c/{link}")).await;
        // These pages name which lists a person is on, and NM runs behind a reverse proxy that
        // would otherwise cache this 200 text/html and hand it to the next caller.
        assert_eq!(r.header("cache-control"), "no-store", "{what}");
        assert_eq!(
            r.header("content-type"),
            "text/html; charset=utf-8",
            "{what}"
        );
    }
}

#[sqlx::test]
async fn the_confirm_route_is_refused_past_the_per_ip_limit(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    // Unparseable, so every one of these is refused before a single database access: what is
    // being counted is what arrives, not what is served.
    for n in 0..PUBLIC_PER_MINUTE {
        let r = p.app.get("/c/not-a-token").await;
        assert_eq!(
            r.status_code(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "request {n} of the first {PUBLIC_PER_MINUTE}"
        );
    }
    let over = p.app.get("/c/not-a-token").await;
    assert_eq!(over.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert!(over.maybe_header("retry-after").is_some());
    assert_eq!(over.header("ratelimit-remaining"), "0");

    // A valid link from the same address is refused too, so the limit is on the route and not on
    // the shape of the token.
    let r = p.app.get(&format!("/c/{}", p.link("n1", 0))).await;
    assert_eq!(r.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(p.status().await.as_deref(), Some("pending"));
}
