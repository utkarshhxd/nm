// SPDX-License-Identifier: AGPL-3.0-only

//! The public, token-authenticated pages: `GET /c/{token}`, `GET`/`POST /u/{token}`, and the
//! shell every one of them is rendered into. `tests/public_tokens.rs` covers `issue`/`parse` on
//! their own; this file is about what a browser (or a mail client) gets.

mod common;

use std::{future::IntoFuture, net::IpAddr, path::Path, time::Instant};

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

/// The body RFC 8058 §3.1 tells a mail client to POST.
const ONE_CLICK: &str = "List-Unsubscribe=One-Click";

/// A workspace with one contact holding one `pending` membership on one list: the state a
/// confirmation link is issued against.
struct Pending {
    app: TestApp,
    workspace: Uuid,
    contact: Uuid,
    list: Uuid,
    owner_email: String,
}

async fn make_contact(pool: &PgPool, workspace: Uuid, email: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
        .bind(id)
        .bind(workspace)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn make_list(
    pool: &PgPool,
    workspace: Uuid,
    name: &str,
    visibility: &str,
    double_optin: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into lists (id, workspace_id, name, double_optin, visibility)          values ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(workspace)
    .bind(name)
    .bind(double_optin)
    .bind(visibility)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn make_member(pool: &PgPool, workspace: Uuid, contact: Uuid, list: Uuid, status: &str) {
    sqlx::query(
        "insert into list_memberships (workspace_id, contact_id, list_id, status)          values ($1, $2, $3, $4)",
    )
    .bind(workspace)
    .bind(contact)
    .bind(list)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn pending(pool: &PgPool, slug: &str) -> Pending {
    let owner_email = format!("owner-{slug}@example.com");
    let owner = create_user(pool, &owner_email, PW, false).await;
    let workspace = create_workspace(pool, owner, slug).await;
    let contact = make_contact(pool, workspace, &format!("subscriber-{slug}@ex.test")).await;
    let list = make_list(pool, workspace, "news", "private", true).await;
    make_member(pool, workspace, contact, list, "pending").await;
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

    /// An unsubscribe link for this contact. No `list_id`: it covers the whole workspace.
    fn unsub(&self, nonce: &str, age_days: i64) -> String {
        self.token(Purpose::Unsubscribe, None, nonce, age_days)
    }

    /// Another list in this workspace, with a membership for this contact at `status`.
    async fn on_list(&self, name: &str, visibility: &str, status: &str) -> Uuid {
        let list = make_list(&self.app.pool, self.workspace, name, visibility, true).await;
        make_member(&self.app.pool, self.workspace, self.contact, list, status).await;
        list
    }

    /// The status of the membership on the fixture's own list.
    async fn status(&self) -> Option<String> {
        sqlx::query_scalar(
            "select status from list_memberships where contact_id = $1 and list_id = $2",
        )
        .bind(self.contact)
        .bind(self.list)
        .fetch_optional(&self.app.pool)
        .await
        .unwrap()
    }

    /// Every membership this contact has, as `(list_id, status)`, ordered by list.
    async fn statuses(&self) -> Vec<(Uuid, String)> {
        sqlx::query_as(
            "select list_id, status from list_memberships where contact_id = $1 order by list_id",
        )
        .bind(self.contact)
        .fetch_all(&self.app.pool)
        .await
        .unwrap()
    }

    /// A POST to this contact's unsubscribe link.
    async fn post_unsub(&self, link: &str, body: &str) -> axum_test::TestResponse {
        self.app.post(&format!("/u/{link}")).text(body).await
    }
}

/// `(action, source, ip, token_jti, source_ref, user_agent)` of every consent row, oldest first.
///
/// The user agent is here because it is evidence like the IP is, and because nothing noticed when
/// it was only passed and never read.
type Evidence = (
    String,
    String,
    Option<IpAddr>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// `(list_id, action, source)` of every consent row, ordered by list so a test never depends on
/// the order of rows written in one transaction (`now()` is transaction-start time, so they tie).
async fn consent_by_list(pool: &PgPool) -> Vec<(Option<Uuid>, String, String)> {
    sqlx::query_as("select list_id, action, source from consent_log order by list_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Every live suppression in the workspace, as `(reason, source, email)`.
async fn live_suppressions(
    pool: &PgPool,
    workspace: Uuid,
) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as(
        "select reason, source, email from suppressions \
          where workspace_id = $1 and lifted_at is null order by id",
    )
    .bind(workspace)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn consent_rows(pool: &PgPool) -> Vec<Evidence> {
    sqlx::query_as(
        "select action, source, ip, token_jti, source_ref, user_agent from consent_log \n         order by id",
    )
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
            // One needle — `cookie`, case-insensitively — which catches every realistic
            // spelling: `CookieJar`, `TypedHeader<Cookie>`, `header::COOKIE` and
            // `headers.get("cookie")`. An earlier version matched only `cookiejar` and `COOKIE`
            // so that prose about cookies could still be written in these files; that let the two
            // commonest spellings through, and a needle narrowed to keep a comment legal is the
            // wrong trade. **Comment lines are skipped instead**, so the prose stays and the
            // needle stays wide.
            //
            // Limit: only `//` line comments are recognised, which is all this codebase uses. A
            // `/* */` block would hide a match, and a string holding the word would be a false
            // positive — neither has happened, and both fail safe towards being noticed.
            let source = std::fs::read_to_string(&p).unwrap();
            if source
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.to_lowercase().contains("cookie"))
            {
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

/// **Every page under `/c/`, `/u/` and `/f/`, which is every public page there is.** A page added
/// under any of those prefixes must be added here, or the name of this test becomes a claim it
/// does not check. The `429` page is covered by the per-IP test below, which is the only place a
/// refusal can be produced.
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
    // `/u/`'s refusal is a page too. Its success is a 200 with no body (RFC 8058), which has
    // nothing for a cache to keep and no content type to assert.
    let bad = p.post_unsub("not-a-token", ONE_CLICK).await;
    assert_eq!(
        bad.header("cache-control"),
        "no-store",
        "unsubscribe invalid"
    );
    assert_eq!(
        bad.header("content-type"),
        "text/html; charset=utf-8",
        "unsubscribe invalid"
    );

    // The preference center is a page too, and is the one that names which lists a person is on.
    let prefs = p.app.get(&format!("/u/{}", p.unsub("n9", 0))).await;
    assert_eq!(prefs.header("cache-control"), "no-store", "preferences");
    assert_eq!(
        prefs.header("content-type"),
        "text/html; charset=utf-8",
        "preferences"
    );

    // The signup form's two pages. It names no person, but it is served from the same shell and
    // a cached copy of it would be a cached copy of an operator's live copy.
    let s = signup(&pool, "globex").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;
    for (what, path) in [
        ("the form", s.url("newsletter")),
        ("no such form", s.url("nothing")),
    ] {
        let r = s.app.get(&path).await;
        assert_eq!(r.header("cache-control"), "no-store", "{what}");
        assert_eq!(
            r.header("content-type"),
            "text/html; charset=utf-8",
            "{what}"
        );
    }
    let submitted = s.submit("newsletter", "email=new@ex.test").await;
    assert_eq!(submitted.header("cache-control"), "no-store", "submitted");

    // And the bare prefixes, which reach no handler at all: `public::owns_path` keeps them off
    // the SPA, and `security_headers::uncacheable` keeps a shared cache off them.
    for path in ["/c", "/c/", "/u", "/u/", "/f", "/f/"] {
        let r = p.app.get(path).await;
        assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(r.header("cache-control"), "no-store", "{path}");
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
    // The refusal is the answer a stranger is likeliest to see, so it is a page and not an
    // RFC 9457 document — and, like every page, one a shared cache must not keep.
    assert_eq!(over.header("content-type"), "text/html; charset=utf-8");
    assert_eq!(over.header("cache-control"), "no-store");

    // A valid link from the same address is refused too, so the limit is on the route and not on
    // the shape of the token.
    let r = p.app.get(&format!("/c/{}", p.link("n1", 0))).await;
    assert_eq!(r.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(p.status().await.as_deref(), Some("pending"));
}

// ---- Task 5: RFC 8058 one-click unsubscribe ----------------------------------------------------

/// Spec §7.2: one click, every list in the workspace — not the list the email came from. The
/// consent rows follow what actually changed, so the list that was already unsubscribed gets none.
#[sqlx::test]
async fn one_click_unsubscribes_from_every_list_in_the_workspace(pool: PgPool) {
    let p = pending(&pool, "acme").await; // `news`, pending
    let weekly = p.on_list("weekly", "public", "subscribed").await;
    let already = p.on_list("old", "private", "unsubscribed").await;

    let r = p.post_unsub(&p.unsub("n1", 0), ONE_CLICK).await;
    r.assert_status_ok();
    assert_eq!(r.text(), "", "RFC 8058 wants a 200 and nothing to read");

    for (list, status) in p.statuses().await {
        assert_eq!(status, "unsubscribed", "list {list}");
    }
    // Two list rows, not three: the membership that was already `unsubscribed` did not change, so
    // nobody consented to anything a second time. Plus the workspace-wide opt-out, which has no
    // list because it is about the address.
    let mut want = vec![
        (
            Some(p.list),
            "unsubscribed".to_owned(),
            "one_click".to_owned(),
        ),
        (
            Some(weekly),
            "unsubscribed".to_owned(),
            "one_click".to_owned(),
        ),
        (None, "suppressed".to_owned(), "one_click".to_owned()),
    ];
    want.sort();
    let mut got = consent_by_list(&pool).await;
    got.sort();
    assert_eq!(got, want);
    assert!(
        !got.iter().any(|(l, _, _)| *l == Some(already)),
        "the list that was already unsubscribed recorded nothing"
    );
    // The subscriber's own request, so the address really is theirs and is kept as evidence.
    for row in consent_rows(&pool).await {
        assert_eq!(row.2, Some("127.0.0.1".parse().unwrap()), "the IP");
        assert_eq!(row.4, None, "no operator id");
    }
}

/// `docs/adr/0001-contacts-have-no-status-column.md`: the workspace-wide opt-out is a
/// `suppressions` row, keyed by address, **because it outlives the contact row**. A sweep of
/// `list_memberships` alone leaves spec §4's second anti-goal wide open — a later import naming
/// the same address on a *different* list writes a fresh `subscribed` row, and invariant 1 only
/// protects a membership that already exists as `unsubscribed`.
#[sqlx::test]
async fn one_click_writes_the_workspace_wide_opt_out_that_outlives_the_contact(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    p.post_unsub(&p.unsub("n1", 0), ONE_CLICK)
        .await
        .assert_status_ok();

    assert_eq!(
        live_suppressions(&pool, p.workspace).await,
        [(
            "unsubscribed_all".to_owned(),
            "one_click".to_owned(),
            Some("subscriber-acme@ex.test".to_owned()),
        )],
    );
    // It is the subscriber's own act, so no user is named as its author.
    let by: Option<Uuid> =
        sqlx::query_scalar("select created_by_user_id from suppressions where workspace_id = $1")
            .bind(p.workspace)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by, None);

    // A second click adds no second row, and the opt-out survives every list being deleted —
    // which is the half `list_memberships` cannot do.
    p.post_unsub(&p.unsub("n2", 0), ONE_CLICK)
        .await
        .assert_status_ok();
    sqlx::query("delete from lists where workspace_id = $1")
        .bind(p.workspace)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(live_suppressions(&pool, p.workspace).await.len(), 1);
}

/// Spec invariant 3, measured end to end.
#[sqlx::test]
async fn one_click_completes_in_under_five_seconds(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    for n in 0..10 {
        p.on_list(&format!("list-{n}"), "public", "subscribed")
            .await;
    }
    let started = Instant::now();
    p.post_unsub(&p.unsub("n1", 0), ONE_CLICK)
        .await
        .assert_status_ok();
    let took = started.elapsed();
    assert!(took.as_secs() < 5, "one-click took {took:?}");
    assert!(p.statuses().await.iter().all(|(_, s)| s == "unsubscribed"));
}

/// Review Focus 4. The two POSTs are issued **concurrently**, which is what mail clients produce:
/// they prefetch, and some send the POST twice. Sequential would prove nothing — the second
/// `UPDATE … WHERE status <> 'unsubscribed'` would simply match nothing. Concurrently, the second
/// statement blocks on the row lock and re-reads the row the first one committed, which is the
/// only case that tests the predicate being re-evaluated.
#[sqlx::test]
async fn two_overlapping_one_click_posts_write_consent_rows_only_for_real_changes(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("weekly", "public", "subscribed").await;
    let link = p.unsub("n1", 0);
    let path = format!("/u/{link}");

    let (a, b) = tokio::join!(
        p.app.post(&path).text(ONE_CLICK).into_future(),
        p.app.post(&path).text(ONE_CLICK).into_future(),
    );
    a.assert_status_ok();
    b.assert_status_ok();

    assert!(p.statuses().await.iter().all(|(_, s)| s == "unsubscribed"));
    // One row per list that changed, plus the one workspace-wide opt-out — never one per request.
    let rows = consent_by_list(&pool).await;
    assert_eq!(
        rows.len(),
        3,
        "two requests, two memberships and one address: {rows:?}"
    );
    for list in [Some(p.list), Some(weekly), None] {
        assert_eq!(
            rows.iter().filter(|(l, _, _)| *l == list).count(),
            1,
            "{list:?} was recorded twice: {rows:?}"
        );
    }
    assert_eq!(live_suppressions(&pool, p.workspace).await.len(), 1);
}

/// Spec invariant 4. The token names its workspace, and nothing else does.
#[sqlx::test]
async fn one_click_never_touches_another_workspaces_membership(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let other_owner = create_user(&pool, "owner-other@example.com", PW, false).await;
    let other = create_workspace(&pool, other_owner, "other").await;
    // The same address in both workspaces, so only the workspace in the token can separate them.
    let their_contact = make_contact(&pool, other, "subscriber-acme@ex.test").await;
    let their_list = make_list(&pool, other, "theirs", "public", true).await;
    make_member(&pool, other, their_contact, their_list, "subscribed").await;

    p.post_unsub(&p.unsub("n1", 0), ONE_CLICK)
        .await
        .assert_status_ok();

    let theirs: String = sqlx::query_scalar(
        "select status from list_memberships where contact_id = $1 and list_id = $2",
    )
    .bind(their_contact)
    .bind(their_list)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(theirs, "subscribed", "another workspace's membership moved");
    assert_eq!(live_suppressions(&pool, other).await, []);
    let theirs_logged: i64 =
        sqlx::query_scalar("select count(*) from consent_log where workspace_id = $1")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(theirs_logged, 0);
}

/// Spec §6.2: an email sits in an inbox for years and RFC 8058 must still work. There is no TTL
/// arm for `Unsubscribe`, so this is the test that would catch one being added.
#[sqlx::test]
async fn an_unsubscribe_token_two_years_old_still_works(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    p.post_unsub(&p.unsub("n1", 2 * 365), ONE_CLICK)
        .await
        .assert_status_ok();
    assert_eq!(p.status().await.as_deref(), Some("unsubscribed"));
}

#[sqlx::test]
async fn a_one_click_consent_row_carries_the_token_nonce_as_its_jti(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let nonce = Uuid::now_v7().to_string();
    p.post_unsub(&p.unsub(&nonce, 0), ONE_CLICK)
        .await
        .assert_status_ok();

    let rows = consent_rows(&pool).await;
    assert!(!rows.is_empty(), "nothing was recorded at all");
    for row in rows {
        assert_eq!(row.3, Some(nonce.clone()), "which link was followed");
    }
}

/// **The one route that reads the limiter's verdict and does not obey it.** `check` returns an
/// advisory `over`; `http::rate_limit::MAX_KEYS` names this route as the one that may not answer
/// `429`, because an unsubscribe that does not stick is spec §4's second anti-goal and a mail
/// client's one-click button has nobody to retry it. The headers are still carried and the
/// request is still counted — what changes is only what the handler does about the verdict.
#[sqlx::test]
async fn one_click_is_not_refused_past_the_per_ip_limit(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    // Spent on another public route, which is also the proof that the budget is shared: Task 4b
    // made `PUBLIC_PER_MINUTE` one budget for all five pages.
    for _ in 0..PUBLIC_PER_MINUTE {
        p.app.get("/c/not-a-token").await;
    }
    p.app
        .get("/c/not-a-token")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);

    let r = p.post_unsub(&p.unsub("n1", 0), ONE_CLICK).await;
    r.assert_status_ok();
    assert_eq!(r.header("ratelimit-remaining"), "0", "it was counted");
    assert_eq!(p.status().await.as_deref(), Some("unsubscribed"));
    assert_eq!(live_suppressions(&pool, p.workspace).await.len(), 1);
}

/// Each public route passes its own `Purpose` to `parse`. A confirm token is genuinely signed by
/// this instance, so only the purpose can be what refuses it here.
#[sqlx::test]
async fn a_confirm_token_is_refused_by_the_unsubscribe_route(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let r = p.post_unsub(&p.link("n1", 0), ONE_CLICK).await;
    assert_eq!(r.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.text().contains("no longer valid"), "{}", r.text());
    assert_eq!(p.status().await.as_deref(), Some("pending"));
    assert_eq!(consent_rows(&pool).await.len(), 0);
    assert_eq!(live_suppressions(&pool, p.workspace).await, []);
}

/// Review Focus 2 again, for this route: the token still verifies after the contact is gone, and
/// the answer is a `200` rather than a 500. There is no address left to suppress.
#[sqlx::test]
async fn one_click_for_a_deleted_contact_does_not_fail(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let link = p.unsub("n1", 0);
    sqlx::query("delete from contacts where id = $1")
        .bind(p.contact)
        .execute(&pool)
        .await
        .unwrap();

    p.post_unsub(&link, ONE_CLICK).await.assert_status_ok();
    assert_eq!(consent_rows(&pool).await.len(), 0);
    assert_eq!(live_suppressions(&pool, p.workspace).await, []);
}

// ---- Task 6: the preference center -------------------------------------------------------------

/// Spec invariant 10, both halves and the "and nothing else": the workspace's `public` lists, plus
/// any non-public list this contact already has a membership row on, and no other list.
#[sqlx::test]
async fn the_page_shows_public_lists_and_the_private_ones_this_contact_is_on(pool: PgPool) {
    let p = pending(&pool, "acme").await; // `news`, private, pending — a list they are on
    let weekly = p.on_list("Weekly", "public", "subscribed").await;
    let offers = make_list(&pool, p.workspace, "Offers", "public", true).await;
    let secret = make_list(&pool, p.workspace, "Insiders only", "private", true).await;
    // Invariant 10's tenancy half: a *public* list of another workspace is the one that would slip
    // through a query missing its `workspace_id`, because visibility alone admits it.
    let other_owner = create_user(&pool, "owner-other@example.com", PW, false).await;
    let other = create_workspace(&pool, other_owner, "other").await;
    let theirs = make_list(&pool, other, "Their public list", "public", true).await;

    let r = p.app.get(&format!("/u/{}", p.unsub("n1", 0))).await;
    r.assert_status_ok();
    let html = r.text();
    for (what, list) in [("news", p.list), ("weekly", weekly), ("offers", offers)] {
        assert!(
            html.contains(&list.to_string()),
            "{what} is missing: {html}"
        );
    }
    assert!(
        !html.contains(&secret.to_string()),
        "a private list this contact was never on is shown: {html}"
    );
    assert!(!html.contains("Insiders only"), "{html}");
    assert!(
        !html.contains(&theirs.to_string()) && !html.contains("Their public list"),
        "another workspace's public list is shown: {html}"
    );

    // The box reflects `status = 'subscribed'`, so the confirmed list is ticked and the pending
    // one is not. A `pending` row is somebody who was asked, not somebody who agreed.
    assert!(
        html.contains(&format!("value=\"{weekly}\" checked")),
        "the subscribed list is not ticked: {html}"
    );
    assert!(
        html.contains(&format!("value=\"{}\">", p.list)),
        "the pending list is ticked: {html}"
    );
    assert!(
        html.contains(&format!("value=\"{offers}\">")),
        "a list they are not on is ticked: {html}"
    );
}

/// Review Focus 3. The page was rendered, the operator deleted the list, and the POST arrives
/// naming it. The candidate set is recomputed from the database at POST time, so the vanished id
/// is simply not among it — no error, and the rest of the form still applies.
#[sqlx::test]
async fn a_list_deleted_between_the_page_and_the_post_does_not_fail_the_post(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let doomed = p.on_list("Doomed", "public", "subscribed").await;
    let weekly = p.on_list("Weekly", "public", "unsubscribed").await;
    p.app
        .get(&format!("/u/{}", p.unsub("n1", 0)))
        .await
        .assert_status_ok();

    sqlx::query("delete from lists where id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .unwrap();

    let r = p
        .post_unsub(&p.unsub("n1", 0), &format!("list={doomed}&list={weekly}"))
        .await;
    r.assert_status_ok();
    let got: Vec<_> = p.statuses().await;
    assert!(
        got.contains(&(weekly, "subscribed".to_owned())),
        "the rest of the form did not apply: {got:?}"
    );
}

/// Review Focus 3, the other way round: an operator makes a list private while somebody holds a
/// preference-center link. They are on it, so it stays on their page — hiding it would be the
/// anti-goal's other half, a page that hides a list somebody is on.
#[sqlx::test]
async fn a_list_flipped_to_private_still_shows_for_a_contact_already_on_it(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("Weekly", "public", "subscribed").await;
    sqlx::query("update lists set visibility = 'private' where id = $1")
        .bind(weekly)
        .execute(&pool)
        .await
        .unwrap();

    let r = p.app.get(&format!("/u/{}", p.unsub("n1", 0))).await;
    r.assert_status_ok();
    assert!(r.text().contains(&weekly.to_string()), "{}", r.text());
}

#[sqlx::test]
async fn unticking_a_box_unsubscribes_and_records_preference_center_as_the_source(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("Weekly", "public", "subscribed").await;
    let keep = p.on_list("Keep", "public", "subscribed").await;

    // `keep` is ticked, `weekly` is not, and the pending `news` is not.
    p.post_unsub(&p.unsub("n1", 0), &format!("list={keep}"))
        .await
        .assert_status_ok();

    let got = p.statuses().await;
    assert!(
        got.contains(&(weekly, "unsubscribed".to_owned())),
        "{got:?}"
    );
    assert!(got.contains(&(keep, "subscribed".to_owned())), "{got:?}");
    // The pending one was unticked too, so it goes as well.
    assert!(
        got.contains(&(p.list, "unsubscribed".to_owned())),
        "{got:?}"
    );

    let rows = consent_by_list(&pool).await;
    assert_eq!(
        rows.iter().filter(|(_, a, _)| a == "unsubscribed").count(),
        2,
        "one row per list that changed, and `keep` did not: {rows:?}"
    );
    for (_, _, source) in &rows {
        assert_eq!(source, "preference_center");
    }
    // Nothing was written for the list that was already subscribed and stayed subscribed.
    assert!(!rows.iter().any(|(l, _, _)| *l == Some(keep)), "{rows:?}");
}

#[sqlx::test]
async fn ticking_a_box_for_a_list_the_contact_left_records_resubscribed(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("Weekly", "public", "unsubscribed").await;

    p.post_unsub(&p.unsub("n1", 0), &format!("list={weekly}"))
        .await
        .assert_status_ok();

    let got = p.statuses().await;
    assert!(got.contains(&(weekly, "subscribed".to_owned())), "{got:?}");
    let rows = consent_by_list(&pool).await;
    assert!(
        rows.contains(&(
            Some(weekly),
            "resubscribed".to_owned(),
            "preference_center".to_owned()
        )),
        "{rows:?}"
    );
    // `unsubscribed_at` is cleared, because this is a live membership again.
    let cleared: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
        "select unsubscribed_at from list_memberships where contact_id = $1 and list_id = $2",
    )
    .bind(p.contact)
    .bind(weekly)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cleared, None);
}

/// **The ruling that reverses this milestone's usual rule.** Everywhere else a `double_optin` list
/// produces a `pending` membership and a `requested` consent row. Here it must not: double opt-in
/// exists to prove the address belongs to the person using it, and a valid token delivered to that
/// address already is that proof. Asking again would be asking them to prove the same thing twice
/// and would leave a `pending` row for somebody who has plainly agreed.
///
/// Both halves are asserted. The membership alone would still pass with a stray `requested` row
/// also written.
#[sqlx::test]
async fn ticking_a_box_on_a_double_optin_list_subscribes_outright_and_never_writes_pending(
    pool: PgPool,
) {
    let p = pending(&pool, "acme").await;
    let fresh = make_list(&pool, p.workspace, "Digest", "public", true).await;
    let double: bool = sqlx::query_scalar("select double_optin from lists where id = $1")
        .bind(fresh)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(double, "the premise: this list is double opt-in");

    // Both the list they were never on and the one they are `pending` on.
    p.post_unsub(&p.unsub("n1", 0), &format!("list={fresh}&list={}", p.list))
        .await
        .assert_status_ok();

    let got = p.statuses().await;
    assert!(got.contains(&(fresh, "subscribed".to_owned())), "{got:?}");
    assert!(
        got.contains(&(p.list, "subscribed".to_owned())),
        "a pending membership was not promoted: {got:?}"
    );
    let rows = consent_by_list(&pool).await;
    assert!(
        !rows.iter().any(|(_, action, _)| action == "requested"),
        "a `requested` row was written for somebody who has plainly agreed: {rows:?}"
    );
    assert!(
        rows.contains(&(
            Some(fresh),
            "subscribed".to_owned(),
            "preference_center".to_owned()
        )),
        "{rows:?}"
    );
    // A `pending` row promoted by the subscriber's own token is a confirmation, which is exactly
    // what `GET /c/{token}` records for the identical transition.
    assert!(
        rows.contains(&(
            Some(p.list),
            "confirmed".to_owned(),
            "preference_center".to_owned()
        )),
        "{rows:?}"
    );
}

#[sqlx::test]
async fn a_preference_center_change_carries_the_token_nonce_as_its_jti(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("Weekly", "public", "subscribed").await;
    let nonce = Uuid::now_v7().to_string();

    p.post_unsub(&p.unsub(&nonce, 0), &format!("list={}", p.list))
        .await
        .assert_status_ok();

    let rows = consent_rows(&pool).await;
    assert!(!rows.is_empty(), "nothing was recorded at all");
    for row in &rows {
        assert_eq!(row.3, Some(nonce.clone()), "which link was followed");
        assert_eq!(row.2, Some("127.0.0.1".parse().unwrap()), "the IP");
        assert_eq!(row.4, None, "no operator id");
    }
    // The premise: this request really did change something, so the rows above are not an empty
    // set being asserted over.
    let got = p.statuses().await;
    assert!(
        got.contains(&(weekly, "unsubscribed".to_owned())),
        "{got:?}"
    );
}

/// Unlike the one-click POST, a human at a browser can read a refusal and try again, so the
/// preference center answers a tripped limit the way `GET /c/{token}` does.
#[sqlx::test]
async fn the_preference_center_is_refused_past_the_per_ip_limit(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let link = p.unsub("n1", 0);
    for _ in 0..PUBLIC_PER_MINUTE {
        p.app.get(&format!("/u/{link}")).await;
    }
    let over = p.app.get(&format!("/u/{link}")).await;
    assert_eq!(over.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert!(over.maybe_header("retry-after").is_some());
    assert_eq!(over.header("content-type"), "text/html; charset=utf-8");
    assert_eq!(over.header("cache-control"), "no-store");

    // And the form POST too — it is not the RFC 8058 one-click, which is the only answer on this
    // route that ignores the verdict.
    let post = p.post_unsub(&link, &format!("list={}", p.list)).await;
    assert_eq!(post.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        p.status().await.as_deref(),
        Some("pending"),
        "nothing moved"
    );
}

/// Adds `count` public lists to the workspace, all subscribed, in one statement.
async fn seed_lists(pool: &PgPool, workspace: Uuid, contact: Uuid, count: i32, prefix: &str) {
    sqlx::query(
        "with l as (insert into lists (id, workspace_id, name, visibility)                     select gen_random_uuid(), $1, $4 || n, 'public'                       from generate_series(1, $2) n returning id)          insert into list_memberships (workspace_id, contact_id, list_id, status)          select $1, $3, l.id, 'subscribed' from l",
    )
    .bind(workspace)
    .bind(count)
    .bind(contact)
    .bind(prefix)
    .execute(pool)
    .await
    .unwrap();
}

/// The fastest of `runs` warm requests, which damps the noise a single sample carries.
async fn fastest(app: &TestApp, url: &str, runs: u32) -> std::time::Duration {
    let mut best = std::time::Duration::MAX;
    for _ in 0..runs {
        let started = Instant::now();
        let r = app.get(url).await;
        best = best.min(started.elapsed());
        r.assert_status_ok();
    }
    best
}

/// Spec §9's 200 ms floor, **and the assertion that actually holds the thing the floor is for**.
///
/// The floor is not a benchmark: it exists to catch an N+1 across lists, which is the only way
/// this page gets slow. **It does not catch one on its own, and that was measured rather than
/// assumed.** `topics` was rewritten as a genuine query-per-list; the page still answered in about
/// 50 ms at 51 lists and about 106 ms at 501, because a round trip over a loopback socket costs
/// roughly 0.2 ms. Fifty lists is nowhere near the fixture that a 200 ms wall-clock bound would
/// separate, and the fixture that would separate it — a few thousand lists — would make this a
/// test of how fast the machine is.
///
/// So the N+1 is caught by how the page's cost **scales**, which is self-calibrating: the same
/// page, the same machine, the same moment, at ten times the list count. One query does not care
/// (measured: 2 ms at 51 lists, 6 ms at 501). A query per list cares linearly (11 ms and 106 ms),
/// and no allowance that a constant-cost page can meet covers a tenfold rise.
#[sqlx::test]
async fn the_page_renders_in_under_200ms_for_a_contact_on_50_lists(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    seed_lists(&pool, p.workspace, p.contact, 50, "List ").await;
    let url = format!("/u/{}", p.unsub("n1", 0));
    // Warm: the first request pays for the connection and the statements being prepared.
    p.app.get(&url).await.assert_status_ok();

    let at_50 = fastest(&p.app, &url, 3).await;
    assert!(at_50.as_millis() < 200, "the page took {at_50:?}");
    let rendered = p.app.get(&url).await;
    assert_eq!(
        rendered.text().matches("type=\"checkbox\"").count(),
        51,
        "the fixture must actually render 51 lists"
    );

    seed_lists(&pool, p.workspace, p.contact, 450, "More ").await;
    p.app.get(&url).await.assert_status_ok();
    let at_500 = fastest(&p.app, &url, 3).await;
    assert!(at_500.as_millis() < 200, "the page took {at_500:?} at 501");
    // **The baseline is a premise, not just a scale.** The allowance below is computed from
    // `at_50`, so a slow or loaded machine would widen it until the measured N+1 (106 ms at 501)
    // fitted underneath — the assertion's power would shrink exactly when the box is busy, which
    // is the property a self-calibrating ratio was meant to remove. 2 ms is the measured figure
    // for one query at 51 lists; 20 ms is ten times that, and a machine slower than this fails
    // here, loudly, instead of quietly passing a page that scales.
    assert!(
        at_50.as_millis() < 20,
        "the baseline is {at_50:?}; the scaling assertion below would be too loose to mean \
         anything. This is the machine being slow, not the page being wrong"
    );
    // Ten times the lists for no more than three times the time, plus a flat allowance for the
    // extra rows this page really does have to render. A query per list spends ten times as long.
    let allowed = at_50 * 3 + std::time::Duration::from_millis(25);
    assert!(
        at_500 <= allowed,
        "501 lists took {at_500:?} against {at_50:?} for 51 (allowed {allowed:?}): the page's \
         cost grows with the list count, which means a query per list"
    );
}

/// `page::esc` is the only thing between an operator-typed list name and an injected tag, and the
/// preference center is the first page that renders one. The names reach element text only, and
/// the loop at the end pins that: every quoted attribute value on the page is a list's `Uuid` or
/// the RFC 8058 body, so an operator-supplied name cannot be in an attribute context at all.
#[sqlx::test]
async fn an_operator_supplied_list_name_is_escaped_on_the_page(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let nasty = "<script>alert(1)</script> \"Bob & Co\"";
    let list = make_list(&pool, p.workspace, nasty, "public", true).await;

    let r = p.app.get(&format!("/u/{}", p.unsub("n1", 0))).await;
    r.assert_status_ok();
    let html = r.text();
    assert!(!html.contains("<script>"), "the name opened a tag: {html}");
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &quot;Bob &amp; Co&quot;"),
        "{html}"
    );
    // The contexts really are the two the doc comment claims: the id is what sits in the quoted
    // attribute, and every attribute value on the page is a `Uuid`. A name interpolated into one
    // would show up here as a `value="` that is not followed by a uuid.
    assert!(html.contains(&format!("value=\"{list}\"")), "{html}");
    for value in html.split("value=\"").skip(1) {
        let value = value.split('"').next().unwrap();
        assert!(
            value == "One-Click" || value.parse::<Uuid>().is_ok(),
            "an attribute value that is neither a list id nor the RFC 8058 body: {value:?}"
        );
    }
}

/// Finding 10 of the prior-milestone hazards, both directions. The workspace-wide opt-out was the
/// subscriber's to set through one-click, so it is theirs to clear by asking for a list again —
/// otherwise the page shows boxes that tick and change nothing a send would honour. **Only
/// `unsubscribed_all` is liftable here:** a bounce, a complaint, an operator's block and an
/// erasure tombstone are facts about the address that a checkbox has no standing to overturn.
#[sqlx::test]
async fn ticking_a_box_lifts_the_subscribers_own_opt_out_and_no_other_block(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("Weekly", "public", "subscribed").await;
    p.post_unsub(&p.unsub("n1", 0), ONE_CLICK)
        .await
        .assert_status_ok();
    assert_eq!(live_suppressions(&pool, p.workspace).await.len(), 1);

    p.post_unsub(&p.unsub("n2", 0), &format!("list={weekly}"))
        .await
        .assert_status_ok();
    assert_eq!(
        live_suppressions(&pool, p.workspace).await,
        [],
        "the subscriber's own opt-out survived them asking for a list again"
    );

    // A block that is not theirs to clear stays, however many boxes they tick. The hash is the
    // real one: `lift_unsubscribed_all` finds a row by `email_hmac`, so a fabricated hash would
    // make this half of the test pass whatever the `reason` filter said — which it did, until a
    // mutation check caught it.
    let hmac = nm::crypto::suppression_hmac(&p.app.state.secrets, "subscriber-acme@ex.test");
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         select $1, $2, 'address', c.email, $4, 'bounced' from contacts c where c.id = $3",
    )
    .bind(Uuid::now_v7())
    .bind(p.workspace)
    .bind(p.contact)
    .bind(hmac.as_slice())
    .execute(&pool)
    .await
    .unwrap();
    p.post_unsub(&p.unsub("n3", 0), &format!("list={weekly}"))
        .await
        .assert_status_ok();
    assert_eq!(
        live_suppressions(&pool, p.workspace)
            .await
            .iter()
            .map(|(reason, _, _)| reason.clone())
            .collect::<Vec<_>>(),
        ["bounced".to_owned()],
        "a bounce is not a subscriber's to lift"
    );
}

/// Invariant 10's teeth on the write side. The candidate set is recomputed server-side, so a list
/// the page would never have shown cannot be joined by posting its id.
#[sqlx::test]
async fn a_private_list_the_contact_is_not_on_cannot_be_joined_by_posting_its_id(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let secret = make_list(&pool, p.workspace, "Insiders", "private", true).await;

    p.post_unsub(&p.unsub("n1", 0), &format!("list={secret}"))
        .await
        .assert_status_ok();

    assert!(
        !p.statuses().await.iter().any(|(l, _)| *l == secret),
        "a private list was joined by posting its id"
    );
    assert_eq!(consent_by_list(&pool).await.len(), 1, "only `news` moved");
}

// ---- Fix round 1 -------------------------------------------------------------------------------

/// M2. The two ways to say "stop" on this page are deliberately not the same act, and the
/// difference is durable rather than cosmetic, so it is pinned here.
///
/// Unticking every box means "not these lists": the memberships go, and nothing is written that
/// outlives the contact row. Somebody who unticks four topics may well intend to come back, and a
/// `suppressions` row is permanent, survives an erase-and-re-import and covers lists that do not
/// exist yet — more than they said.
///
/// Saying "all of it" is an explicit control on the page, which posts the same RFC 8058 body a
/// mail client's button does and so takes the same path, suppression and all. The page is not
/// complete without it: spec §6.3 makes `/u/{token}` the unsubscribe confirmation as well as the
/// preference center.
#[sqlx::test]
async fn unticking_every_box_is_not_the_workspace_wide_opt_out_but_the_page_offers_one(
    pool: PgPool,
) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("Weekly", "public", "subscribed").await;

    // The explicit control is really on the page, and really is the RFC 8058 body.
    let page = p.app.get(&format!("/u/{}", p.unsub("n0", 0))).await;
    page.assert_status_ok();
    assert!(
        page.text()
            .contains("name=\"List-Unsubscribe\" value=\"One-Click\""),
        "the page offers no way to leave everything: {}",
        page.text()
    );

    // Every box unticked: an empty `list` set, which is what a browser posts for a form whose
    // checkboxes are all clear.
    p.post_unsub(&p.unsub("n1", 0), "").await.assert_status_ok();
    let got = p.statuses().await;
    assert!(got.iter().all(|(_, s)| s == "unsubscribed"), "{got:?}");
    assert_eq!(
        live_suppressions(&pool, p.workspace).await,
        [],
        "unticking the boxes wrote a permanent workspace-wide opt-out nobody asked for"
    );
    assert_eq!(
        consent_by_list(&pool)
            .await
            .iter()
            .filter(|(_, action, _)| action == "suppressed")
            .count(),
        0
    );

    // The explicit control, from the same page, on a contact who is now on nothing: it still
    // writes the opt-out, because it is about the address and not about the memberships.
    p.post_unsub(&p.unsub("n2", 0), ONE_CLICK)
        .await
        .assert_status_ok();
    assert_eq!(
        live_suppressions(&pool, p.workspace).await,
        [(
            "unsubscribed_all".to_owned(),
            "one_click".to_owned(),
            Some("subscriber-acme@ex.test".to_owned()),
        )],
    );
    assert!(
        p.statuses()
            .await
            .contains(&(weekly, "unsubscribed".into()))
    );
}

/// L7. The documented limit on `suppressions::unsubscribe_all`: `suppressions_live` is unique per
/// address per workspace **whatever the reason**, so an address already blocked keeps the block it
/// has and gets no `unsubscribed_all` row. Pinned so that the change the comment proposes — a
/// per-reason live index — cannot happen without a test noticing.
///
/// The second half is the worst case the comment does not state: when there is also no membership
/// to change, a one-click writes nothing anywhere, so the append-only record holds no evidence
/// that the person exercised their RFC 8058 right. That is the behaviour today; this records it.
#[sqlx::test]
async fn one_click_on_an_already_blocked_address_keeps_the_block_it_has(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    let weekly = p.on_list("Weekly", "public", "subscribed").await;
    let hmac = nm::crypto::suppression_hmac(&p.app.state.secrets, "subscriber-acme@ex.test");
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         select $1, $2, 'address', c.email, $4, 'bounced' from contacts c where c.id = $3",
    )
    .bind(Uuid::now_v7())
    .bind(p.workspace)
    .bind(p.contact)
    .bind(hmac.as_slice())
    .execute(&pool)
    .await
    .unwrap();

    p.post_unsub(&p.unsub("n1", 0), ONE_CLICK)
        .await
        .assert_status_ok();

    // The memberships still move, and each one that changed is still recorded.
    let got = p.statuses().await;
    assert!(got.iter().all(|(_, s)| s == "unsubscribed"), "{got:?}");
    let rows = consent_by_list(&pool).await;
    assert_eq!(
        rows.iter()
            .filter(|(l, _, _)| *l == Some(weekly) || *l == Some(p.list))
            .count(),
        2
    );
    // The block that was there is untouched, and no second live row was written.
    assert_eq!(
        live_suppressions(&pool, p.workspace).await,
        [(
            "bounced".to_owned(),
            String::new(),
            Some("subscriber-acme@ex.test".to_owned()),
        )],
    );
    // And so no `suppressed` consent row: it is written only when a row really was.
    assert!(!rows.iter().any(|(_, action, _)| action == "suppressed"));

    // The worst case: blocked already, and nothing left to change. The act leaves no trace.
    let before = consent_by_list(&pool).await.len();
    p.post_unsub(&p.unsub("n2", 0), ONE_CLICK)
        .await
        .assert_status_ok();
    assert_eq!(
        consent_by_list(&pool).await.len(),
        before,
        "a repeat on an already-blocked address writes nothing, which is the documented limit"
    );
}

/// L1. The no-`429` exemption belongs to a genuine RFC 8058 link, not to anyone who can type the
/// field name. A forged token carrying the one-click body is refused like any other request past
/// the limit, so the route cannot be made unlimitable by a string in a body.
#[sqlx::test]
async fn the_one_click_exemption_needs_a_valid_token_not_just_the_body(pool: PgPool) {
    let p = pending(&pool, "acme").await;
    for _ in 0..PUBLIC_PER_MINUTE {
        p.app.get("/c/not-a-token").await;
    }

    let forged = p.post_unsub("not-a-token", ONE_CLICK).await;
    assert_eq!(forged.status_code(), StatusCode::TOO_MANY_REQUESTS);

    // The genuine link from the same address is still honoured, which is the ruling.
    let real = p.post_unsub(&p.unsub("n1", 0), ONE_CLICK).await;
    real.assert_status_ok();
    assert_eq!(p.status().await.as_deref(), Some("unsubscribed"));
}

// ---- Task 9: the hosted signup form ------------------------------------------------------------

/// A workspace with nothing in it yet, and the slugs that reach its forms from the open internet.
struct Signup {
    app: TestApp,
    workspace: Uuid,
    slug: String,
}

async fn signup(pool: &PgPool, slug: &str) -> Signup {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let workspace = create_workspace(pool, owner, slug).await;
    Signup {
        app: TestApp::new(pool.clone()).await,
        workspace,
        slug: slug.to_owned(),
    }
}

impl Signup {
    async fn list(&self, name: &str, double_optin: bool) -> Uuid {
        make_list(&self.app.pool, self.workspace, name, "public", double_optin).await
    }

    /// A form collecting an address and a first name, naming `lists`. Written straight into the
    /// table: `tests/forms.rs` is where the authenticated API that writes one is tested, and this
    /// file is about what a stranger gets from the public page.
    async fn form(&self, slug: &str, lists: &[Uuid], redirect: Option<&str>) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "insert into forms (id, workspace_id, slug, name, fields, redirect_url, headline, \
             button_label) values ($1, $2, $3, 'Signup', $4::jsonb, $5, 'Join the list', 'Join')",
        )
        .bind(id)
        .bind(self.workspace)
        .bind(slug)
        .bind(
            serde_json::json!([
                {"target": {"kind": "email"}, "required": true, "label": "Email"},
                {"target": {"kind": "first_name"}, "required": false, "label": "First name"},
            ])
            .to_string(),
        )
        .bind(redirect)
        .execute(&self.app.pool)
        .await
        .unwrap();
        for list in lists {
            sqlx::query(
                "insert into form_lists (workspace_id, form_id, list_id) values ($1, $2, $3)",
            )
            .bind(self.workspace)
            .bind(id)
            .bind(list)
            .execute(&self.app.pool)
            .await
            .unwrap();
        }
        id
    }

    fn url(&self, form_slug: &str) -> String {
        format!("/f/{}/{form_slug}", self.slug)
    }

    /// A submission, as a browser posts one — user agent included, because it is evidence the
    /// consent row keeps.
    async fn submit(&self, form_slug: &str, body: &str) -> axum_test::TestResponse {
        self.app
            .post(&self.url(form_slug))
            .content_type("application/x-www-form-urlencoded")
            .add_header("user-agent", UA)
            .text(body)
            .await
    }

    /// Every membership in this workspace, as `(list_id, status, source)`, ordered by list.
    async fn memberships(&self) -> Vec<(Uuid, String, Option<String>)> {
        sqlx::query_as(
            "select list_id, status, source from list_memberships where workspace_id = $1 \
             order by list_id",
        )
        .bind(self.workspace)
        .fetch_all(&self.app.pool)
        .await
        .unwrap()
    }
}

/// Blocks `email` in this workspace, through the hash the lookup really uses. A fabricated
/// `email_hmac` here would match nothing, and every assertion that followed would hold for the
/// wrong reason - the failure a Task 5/6 test shipped with.
async fn block_address(s: &Signup, email: &str, reason: &str) {
    let hmac = nm::crypto::suppression_hmac(&s.app.state.secrets, email);
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason)          values ($1, $2, 'address', $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(s.workspace)
    .bind(email)
    .bind(hmac.as_slice())
    .bind(reason)
    .execute(&s.app.pool)
    .await
    .unwrap();
}

/// What a submitting browser calls itself in these tests.
const UA: &str = "nm-public-form-test";

/// Every queued job, as `(kind, payload)`, oldest first.
async fn jobs(pool: &PgPool) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as("select kind, payload from jobs order by id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn contact_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from contacts")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A response as the invariant compares them: the status, the body, and every header a handler
/// chooses.
///
/// Two are dropped. `date` is the HTTP server's wall clock. `ratelimit-remaining` counts **this
/// address's** budget down by one per request by construction, so two sequential requests can
/// never carry the same value and comparing it would only ever assert that time passed — it says
/// nothing about the address that was submitted, which is what invariant 7 is about. That the
/// `POST` route carries no `RateLimit-*` headers at all is asserted where it belongs, in
/// `a_rate_limited_submission_answers_exactly_as_a_real_one`.
fn answer(r: &axum_test::TestResponse) -> (StatusCode, String, Vec<(String, String)>) {
    let mut headers: Vec<(String, String)> = r
        .headers()
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "date" | "ratelimit-remaining"))
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    headers.sort();
    (r.status_code(), r.text(), headers)
}

#[sqlx::test]
async fn a_new_address_on_a_double_optin_list_is_pending_and_enqueues_a_job(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    let form = s.form("newsletter", &[list], None).await;

    let r = s
        .submit("newsletter", "email=new@ex.test&first_name=Ada")
        .await;
    r.assert_status_ok();
    assert!(r.text().contains("Check your email"), "{}", r.text());

    assert_eq!(
        s.memberships().await,
        [(list, "pending".to_owned(), Some("form".to_owned()))]
    );
    // Spec 7.1 as amended: a membership landing in `pending` records `requested`, not
    // `subscribed`. The subscriber's own IP and user agent are the evidence, `source_ref` is the
    // form - and `token_jti` is empty, because no link was followed to get here.
    let rows = consent_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "requested");
    assert_eq!(rows[0].1, "form");
    assert_eq!(rows[0].2, Some("127.0.0.1".parse().unwrap()));
    assert_eq!(rows[0].3, None, "no link was followed");
    assert_eq!(
        rows[0].4,
        Some(form.to_string()),
        "which form was submitted"
    );
    assert_eq!(
        rows[0].5.as_deref(),
        Some(UA),
        "the user agent is evidence beside the IP"
    );

    // The name it collected really did land on the contact, so the fields are wired and not
    // merely accepted.
    let first: String = sqlx::query_scalar("select first_name from contacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(first, "Ada");

    let queued = jobs(&pool).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "confirmation_email");
    assert_eq!(queued[0].1["list_id"], serde_json::json!(list));
    assert_eq!(queued[0].1["email"], "new@ex.test");
    assert!(
        queued[0].1["token"]
            .as_str()
            .is_some_and(|t| t.contains('.')),
        "the payload carries the issued token string: {:?}",
        queued[0].1
    );
}

#[sqlx::test]
async fn a_new_address_on_a_single_optin_list_is_subscribed_with_no_job(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", false).await;
    s.form("newsletter", &[list], None).await;

    let r = s.submit("newsletter", "email=new@ex.test").await;
    r.assert_status_ok();
    assert!(r.text().contains("You are signed up"), "{}", r.text());

    assert_eq!(
        s.memberships().await,
        [(list, "subscribed".to_owned(), Some("form".to_owned()))]
    );
    let rows = consent_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "subscribed");
    assert_eq!(rows[0].1, "form");
    // No confirmation to ask for, so no email to send.
    assert_eq!(jobs(&pool).await.len(), 0);
}

/// Spec 7.3: double opt-in follows **the list's** flag, not the form's, so one submission can
/// produce both kinds at once - and one `jobs` row per double-opt-in list, each with its own
/// token, because `parse` requires a `list_id` on exactly the confirm tokens and one token cannot
/// cover two lists.
#[sqlx::test]
async fn one_submission_to_a_form_naming_both_kinds_does_both(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let news = s.list("News", true).await;
    let digest = s.list("Digest", true).await;
    let offers = s.list("Offers", false).await;
    s.form("newsletter", &[news, digest, offers], None).await;

    s.submit("newsletter", "email=new@ex.test")
        .await
        .assert_status_ok();

    let mut got: Vec<(Uuid, String)> = s
        .memberships()
        .await
        .into_iter()
        .map(|(l, status, _)| (l, status))
        .collect();
    got.sort();
    let mut want = vec![
        (news, "pending".to_owned()),
        (digest, "pending".to_owned()),
        (offers, "subscribed".to_owned()),
    ];
    want.sort();
    assert_eq!(got, want);

    let consent = consent_by_list(&pool).await;
    assert_eq!(consent.len(), 3);
    for (list, action, source) in &consent {
        assert_eq!(source, "form");
        let want = if *list == Some(offers) {
            "subscribed"
        } else {
            "requested"
        };
        assert_eq!(action, want, "{list:?}");
    }

    // Two double-opt-in lists, two emails, two different tokens: they are two consents.
    let queued = jobs(&pool).await;
    assert_eq!(queued.len(), 2, "one per double-opt-in list, and no more");
    let mut lists: Vec<Uuid> = queued
        .iter()
        .map(|(_, p)| serde_json::from_value(p["list_id"].clone()).unwrap())
        .collect();
    lists.sort();
    let mut want = vec![news, digest];
    want.sort();
    assert_eq!(lists, want);
    assert_ne!(
        queued[0].1["token"], queued[1].1["token"],
        "a confirm token names one list, so two lists cannot share one"
    );
}

/// Invariant 7. The three cases differ in everything the database does and in nothing a caller
/// can see: same status, same bytes, same headers.
#[sqlx::test]
async fn the_response_is_identical_for_new_existing_and_suppressed(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;

    // Suppressed workspace-wide, through the hash the lookup really uses.
    let blocked = "blocked@ex.test";
    let hmac = nm::crypto::suppression_hmac(&s.app.state.secrets, blocked);
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         values ($1, $2, 'address', $3, $4, 'unsubscribed_all')",
    )
    .bind(Uuid::now_v7())
    .bind(s.workspace)
    .bind(blocked)
    .bind(hmac.as_slice())
    .execute(&pool)
    .await
    .unwrap();

    let new = s.submit("newsletter", "email=new@ex.test").await;
    // The same address again: now an existing contact with a membership already pending.
    let existing = s.submit("newsletter", "email=new@ex.test").await;
    let suppressed = s.submit("newsletter", &format!("email={blocked}")).await;

    assert_eq!(answer(&new), answer(&existing));
    assert_eq!(answer(&new), answer(&suppressed));
    // Not vacuous: the three really did do different things underneath.
    assert_eq!(
        contact_count(&pool).await,
        1,
        "only the first wrote a contact"
    );
    assert_eq!(consent_rows(&pool).await.len(), 1);
    assert_eq!(jobs(&pool).await.len(), 1);

    // **The same three, with a field besides the address that cannot be stored.** This is where
    // the oracle was: the optional fields are validated by `contacts::create`, which only runs
    // for an address that is not already a contact, so a well-formed address with a malformed
    // name used to answer the error page for a stranger, the success page for a subscriber, and
    // the success page for a suppressed address — one request, no statistics. Varying only the
    // address, as the three submissions above do, cannot see it.
    let bad_name = |address: &str| format!("email={address}&first_name=%01");
    let new_bad = s.submit("newsletter", &bad_name("other@ex.test")).await;
    let existing_bad = s.submit("newsletter", &bad_name("new@ex.test")).await;
    let suppressed_bad = s.submit("newsletter", &bad_name(blocked)).await;

    assert_eq!(answer(&new_bad), answer(&existing_bad));
    assert_eq!(answer(&new_bad), answer(&suppressed_bad));
    // And it is the refusal, not the success page: the shape of what was typed is the one thing
    // this route may answer differently, because the submitter can see it and fix it.
    assert!(
        new_bad.text().contains("Check the details"),
        "{}",
        new_bad.text()
    );
    assert_ne!(answer(&new), answer(&new_bad));
    // Nothing of the three was written: the same counts as before this block.
    assert_eq!(contact_count(&pool).await, 1);
    assert_eq!(consent_rows(&pool).await.len(), 1);
    assert_eq!(jobs(&pool).await.len(), 1);
}

/// Review Focus 5. The address is blocked workspace-wide, so nothing is written - and the second
/// half of this test is what proves the block is what did it rather than a fixture that never
/// matched: lift it and the identical submission subscribes.
#[sqlx::test]
async fn a_suppressed_address_gets_no_membership_and_no_job(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;
    let blocked = "blocked@ex.test";
    let hmac = nm::crypto::suppression_hmac(&s.app.state.secrets, blocked);
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         values ($1, $2, 'address', $3, $4, 'unsubscribed_all')",
    )
    .bind(Uuid::now_v7())
    .bind(s.workspace)
    .bind(blocked)
    .bind(hmac.as_slice())
    .execute(&pool)
    .await
    .unwrap();

    s.submit("newsletter", &format!("email={blocked}"))
        .await
        .assert_status_ok();
    assert_eq!(contact_count(&pool).await, 0, "no contact");
    assert_eq!(s.memberships().await, [], "no membership");
    assert_eq!(consent_rows(&pool).await.len(), 0, "no consent row");
    assert_eq!(jobs(&pool).await.len(), 0, "and no email queued");

    // The control. Without this the test above passes just as well with a fabricated hash that
    // matched nothing - the failure a Task 5/6 test shipped with.
    sqlx::query("delete from suppressions")
        .execute(&pool)
        .await
        .unwrap();
    s.submit("newsletter", &format!("email={blocked}"))
        .await
        .assert_status_ok();
    assert_eq!(
        s.memberships().await,
        [(list, "pending".to_owned(), Some("form".to_owned()))],
        "the suppression was what stopped the first one"
    );
}

/// Invariant 9, first half.
#[sqlx::test]
async fn a_honeypot_submission_answers_exactly_as_a_real_one(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;

    let real = s.submit("newsletter", "email=real@ex.test").await;
    let trapped = s
        .submit(
            "newsletter",
            "email=bot@ex.test&website=http%3A%2F%2Fspam.test",
        )
        .await;

    assert_eq!(answer(&real), answer(&trapped));
    // And the trap caught it: one contact, one membership, one job - the real one's.
    assert_eq!(contact_count(&pool).await, 1);
    assert_eq!(jobs(&pool).await.len(), 1);
    assert_eq!(jobs(&pool).await[0].1["email"], "real@ex.test");

    // The page really does carry the input a script would fill in, hidden from a person.
    let page = s.app.get(&s.url("newsletter")).await;
    assert!(page.text().contains("name=\"website\""), "{}", page.text());
    assert!(page.text().contains("class=\"hp\""), "{}", page.text());
}

/// Invariant 9, second half, and the one that looks exactly like a bug: the success page, not a
/// `429`. Every other limited route in this tree refuses with a status and `RateLimit-*` headers;
/// this one must not, because a distinguishable refusal tells a scraper it has been seen
/// (spec 8.9).
#[sqlx::test]
async fn a_rate_limited_submission_answers_exactly_as_a_real_one(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;

    let real = s.submit("newsletter", "email=real@ex.test").await;
    real.assert_status_ok();
    // The shared public budget, spent from anywhere: Task 4b made `PUBLIC_PER_MINUTE` one budget
    // for all five pages, and `check` counts refused requests too.
    for _ in 0..PUBLIC_PER_MINUTE {
        s.app.get("/c/not-a-token").await;
    }

    let refused = s.submit("newsletter", "email=later@ex.test").await;
    assert_eq!(refused.status_code(), StatusCode::OK, "never a 429 here");
    assert_eq!(answer(&real), answer(&refused));
    assert!(
        refused.maybe_header("retry-after").is_none()
            && refused.maybe_header("ratelimit-remaining").is_none(),
        "the RateLimit headers are the refusal's last tell and this route does not carry them"
    );
    // It was genuinely refused: the second address got nothing.
    assert_eq!(contact_count(&pool).await, 1);
    assert_eq!(jobs(&pool).await.len(), 1);
    assert_eq!(jobs(&pool).await[0].1["email"], "real@ex.test");
}

/// Spec invariant 1, for the writer that is easiest to reach: anyone may type anyone's address
/// into a public form. A membership that was unsubscribed stays unsubscribed, records nothing,
/// and - on a double-opt-in list - is not even asked again by email.
#[sqlx::test]
async fn a_form_submission_cannot_resubscribe_an_unsubscribed_contact(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let news = s.list("News", true).await;
    let offers = s.list("Offers", false).await;
    s.form("newsletter", &[news, offers], None).await;
    let contact = make_contact(&pool, s.workspace, "gone@ex.test").await;
    make_member(&pool, s.workspace, contact, news, "unsubscribed").await;
    make_member(&pool, s.workspace, contact, offers, "unsubscribed").await;

    s.submit("newsletter", "email=gone@ex.test")
        .await
        .assert_status_ok();

    let got = s.memberships().await;
    assert_eq!(got.len(), 2, "the fixture must have both memberships");
    assert!(
        got.iter().all(|(_, status, _)| status == "unsubscribed"),
        "{got:?}"
    );
    assert_eq!(consent_rows(&pool).await.len(), 0, "nothing changed");
    assert_eq!(
        jobs(&pool).await.len(),
        0,
        "and nobody who left is emailed to ask them back"
    );
}

#[sqlx::test]
async fn the_enqueued_job_carries_a_token_that_the_confirm_route_accepts(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;
    s.submit("newsletter", "email=new@ex.test")
        .await
        .assert_status_ok();

    let queued = jobs(&pool).await;
    let token = queued[0].1["token"].as_str().unwrap().to_owned();
    let r = s.app.get(&format!("/c/{token}")).await;
    r.assert_status_ok();
    assert!(r.text().contains("subscribed"), "{}", r.text());

    assert_eq!(
        s.memberships().await,
        [(list, "subscribed".to_owned(), Some("form".to_owned()))]
    );
    // The loop closes: `requested` when the form was submitted, `confirmed` when the link in the
    // queued email was followed.
    let actions: Vec<String> = consent_rows(&pool).await.into_iter().map(|r| r.0).collect();
    assert_eq!(actions, ["requested", "confirmed"]);
}

/// Spec 9: under 500 ms, excluding the job (which is a row in `jobs`, not a send).
///
/// **It also measures the gap byte-identity cannot close.** A suppressed address returns before
/// the contact, the membership, the consent row and the job a new address writes, so it answers
/// sooner, and a large enough difference is an existence oracle wearing a different coat. The gap
/// is not asserted on - a wall-clock difference between two code paths is the flakiest assertion
/// there is - it is measured and recorded, in `src/public/forms.rs` beside the branch that causes
/// it. What is asserted is that **both** paths sit inside the budget, which is what stops the
/// difference growing without anyone noticing.
#[sqlx::test]
async fn a_submission_completes_in_under_500ms_excluding_the_job(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;
    let blocked = "blocked@ex.test";
    let hmac = nm::crypto::suppression_hmac(&s.app.state.secrets, blocked);
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         values ($1, $2, 'address', $3, $4, 'unsubscribed_all')",
    )
    .bind(Uuid::now_v7())
    .bind(s.workspace)
    .bind(blocked)
    .bind(hmac.as_slice())
    .execute(&pool)
    .await
    .unwrap();
    // Warm: the first request pays for the connection and the statements being prepared.
    s.submit("newsletter", "email=warm@ex.test").await;

    let mut writes = std::time::Duration::MAX;
    let mut refusals = std::time::Duration::MAX;
    for n in 0..5 {
        let started = Instant::now();
        s.submit("newsletter", &format!("email=new{n}@ex.test"))
            .await
            .assert_status_ok();
        writes = writes.min(started.elapsed());

        let started = Instant::now();
        s.submit("newsletter", &format!("email={blocked}"))
            .await
            .assert_status_ok();
        refusals = refusals.min(started.elapsed());
    }
    println!("new address {writes:?}, suppressed address {refusals:?}");
    assert!(writes.as_millis() < 500, "a new address took {writes:?}");
    assert!(
        refusals.as_millis() < 500,
        "a suppressed address took {refusals:?}"
    );
    assert_eq!(
        jobs(&pool).await.len(),
        6,
        "five new addresses and the warm-up"
    );
}

/// `form_lists.list_id` cascades, so "at least one list" holds when an operator writes a form and
/// not for the row's lifetime (`0023_forms_and_jobs.sql`). A form with nothing left to subscribe
/// to accepts no submission - and says so the way every other refusal here says it, which is not
/// at all.
#[sqlx::test]
async fn a_form_whose_lists_were_all_deleted_accepts_no_submission(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;
    sqlx::query("delete from lists where id = $1")
        .bind(list)
        .execute(&pool)
        .await
        .unwrap();

    let r = s.submit("newsletter", "email=new@ex.test").await;
    r.assert_status_ok();
    assert_eq!(s.memberships().await, []);
    assert_eq!(consent_rows(&pool).await.len(), 0);
    assert_eq!(jobs(&pool).await.len(), 0);
    // And no contact either. A contact row with no membership and no consent row says nothing
    // about where it came from or what it agreed to, which is the wrong thing for this product
    // to be left holding.
    assert_eq!(contact_count(&pool).await, 0);
}

/// Two guessable slugs, and one answer for every way of missing. A workspace that does not exist,
/// a form that does not exist and another workspace's form must not be told apart, or the pair is
/// an oracle for which operators are customers here.
#[sqlx::test]
async fn an_unknown_form_and_another_workspaces_form_are_the_same_page(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let other = signup(&pool, "globex").await;
    let list = other.list("News", true).await;
    other.form("newsletter", &[list], None).await;

    let theirs = s.app.get("/f/acme/newsletter").await; // exists, but not in `acme`
    let no_form = s.app.get("/f/acme/nothing").await;
    let no_workspace = s.app.get("/f/nobody/newsletter").await;
    for (what, r) in [("no form", &no_form), ("no workspace", &no_workspace)] {
        assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{what}");
        assert_eq!(answer(r), answer(&theirs), "{what}");
    }

    // And the same three through `POST`, which takes its own branch and carries its own headers
    // — no `RateLimit-*`, where the `GET` above has them. A miss must not be told apart there
    // either.
    let theirs = s.submit("newsletter", "email=new@ex.test").await;
    let no_form = s
        .app
        .post("/f/acme/nothing")
        .content_type("application/x-www-form-urlencoded")
        .text("email=new@ex.test")
        .await;
    let no_workspace = s
        .app
        .post("/f/nobody/newsletter")
        .content_type("application/x-www-form-urlencoded")
        .text("email=new@ex.test")
        .await;
    for (what, r) in [("no form", &no_form), ("no workspace", &no_workspace)] {
        assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{what}");
        assert_eq!(answer(r), answer(&theirs), "{what}");
    }
    assert_eq!(contact_count(&pool).await, 0, "a miss writes nothing");

    // The premise: that form really is reachable at its own workspace's slug, so the three
    // refusals above are refusals and not a route that is broken for everyone.
    other
        .app
        .get("/f/globex/newsletter")
        .await
        .assert_status_ok();
}

/// Every value an operator typed reaches this page, and `page::esc` is the whole of what stands
/// between them and an injected tag. The labels and the button sit in element text; the input
/// names are built from the target and a validated custom-field key, never from a label.
#[sqlx::test]
async fn the_form_page_escapes_the_operators_copy(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    let id = s.form("newsletter", &[list], None).await;
    let nasty = "<script>alert(1)</script> \"Bob & Co\"";
    sqlx::query(
        "update forms set headline = $2, button_label = $2, fields = $3::jsonb where id = $1",
    )
    .bind(id)
    .bind(nasty)
    .bind(
        serde_json::json!([{"target": {"kind": "email"}, "required": true, "label": nasty}])
            .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();

    let r = s.app.get(&s.url("newsletter")).await;
    r.assert_status_ok();
    let html = r.text();
    assert!(!html.contains("<script>"), "{html}");
    assert_eq!(
        html.matches("&lt;script&gt;alert(1)&lt;/script&gt; &quot;Bob &amp; Co&quot;")
            .count(),
        4,
        "the headline twice (title and h1), the label, and the button: {html}"
    );
}

/// `redirect_url` is a **link on the success page, not a `303`**, and the reason is measured
/// rather than assumed: `security_headers` sends `form-action 'self'`, and Chromium checks the
/// redirect that follows a form POST against it and blocks the navigation
/// (`web/e2e/public-form.spec.ts` is the browser that says so). Widening the policy for every
/// page in the product to fix one route was the alternative, and was not taken.
#[sqlx::test]
async fn a_form_with_a_redirect_url_offers_it_as_a_link(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", false).await;
    s.form(
        "newsletter",
        &[list],
        Some("https://example.test/thanks?a=1&b=2"),
    )
    .await;

    let r = s.submit("newsletter", "email=new@ex.test").await;
    r.assert_status_ok();
    assert!(
        r.maybe_header("location").is_none(),
        "no redirect is issued"
    );
    // The `&` is escaped because this is an attribute value in HTML; the URL itself is the one
    // the parser serialised when an operator stored it.
    assert!(
        r.text()
            .contains("<a href=\"https://example.test/thanks?a=1&amp;b=2\">Continue</a>"),
        "{}",
        r.text()
    );
    assert_eq!(
        s.memberships().await,
        [(list, "subscribed".to_owned(), Some("form".to_owned()))]
    );
    // The page is the answer for every case, refusals included, or it would be the tell the page
    // is not allowed to be. The second submission changes nothing: the row is subscribed.
    let again = s.submit("newsletter", "email=new@ex.test").await;
    assert_eq!(answer(&r), answer(&again));
    assert_eq!(consent_rows(&pool).await.len(), 1);
}

/// A submission whose own fields do not validate is the one answer that is not the success page,
/// and it may be: it is decided by the **shape** of what was typed, which the submitter can see,
/// and never by anything the database knows about the address.
#[sqlx::test]
async fn a_submission_with_no_usable_address_asks_for_it_again(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;

    let r = s.submit("newsletter", "email=not-an-address").await;
    r.assert_status_ok();
    assert!(r.text().contains("Check the details"), "{}", r.text());
    assert!(r.text().contains("<form method=\"post\">"), "{}", r.text());
    assert_eq!(contact_count(&pool).await, 0);
    assert_eq!(s.memberships().await, []);
}

/// Re-submitting the form is the only resend this milestone has — `public::confirm`'s expired
/// page says so, and spec 8.6 requires an expired confirmation to offer one. So a submission that
/// finds the membership already `pending` enqueues another confirmation email, and the window in
/// `RESEND_AFTER_MINUTES` is what stops that being a mailer aimed by whoever can type an address.
///
/// The window is moved by backdating the queued row rather than by sleeping fifteen minutes. Both
/// halves of the throttle are exercised: the immediate repeat must be blocked (which is only true
/// if the lookup finds the fresh row at all), and the backdated one must not be (which is only
/// true if `created_at` is what decides).
#[sqlx::test]
async fn a_second_submission_resends_the_confirmation_only_after_the_window(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;

    s.submit("newsletter", "email=lost@ex.test")
        .await
        .assert_status_ok();
    assert_eq!(jobs(&pool).await.len(), 1);

    // Straight away: the page still says "check your email", and no second email is queued.
    let again = s.submit("newsletter", "email=lost@ex.test").await;
    again.assert_status_ok();
    assert_eq!(
        jobs(&pool).await.len(),
        1,
        "a repeat inside the window must not queue a second email"
    );

    // Fifteen minutes later, without waiting fifteen minutes. `created_at` is what the throttle
    // reads; the `id` bound beside it is only there to keep the lookup off a table scan.
    sqlx::query("update jobs set created_at = now() - interval '16 minutes'")
        .execute(&pool)
        .await
        .unwrap();

    s.submit("newsletter", "email=lost@ex.test")
        .await
        .assert_status_ok();
    let queued = jobs(&pool).await;
    assert_eq!(
        queued.len(),
        2,
        "the lost confirmation can be asked for again"
    );
    assert_ne!(
        queued[0].1["token"], queued[1].1["token"],
        "a resend issues a new link, not a copy of the one that went missing"
    );

    // Three submissions, two emails, and still one act of consent: the membership was asked for
    // once and nothing about it changed.
    assert_eq!(
        s.memberships().await,
        [(list, "pending".to_owned(), Some("form".to_owned()))]
    );
    let rows = consent_rows(&pool).await;
    assert_eq!(rows.len(), 1, "a resend is not a second consent");
    assert_eq!(rows[0].0, "requested");

    // And the resent link works, which is the whole point of sending it.
    let token = queued[1].1["token"].as_str().unwrap().to_owned();
    let confirmed = s.app.get(&format!("/c/{token}")).await;
    confirmed.assert_status_ok();
    assert!(
        confirmed.text().contains("subscribed"),
        "{}",
        confirmed.text()
    );
    assert_eq!(
        s.memberships().await,
        [(list, "subscribed".to_owned(), Some("form".to_owned()))]
    );
}

/// The resend follows the membership, not the submission: a list the contact has already
/// confirmed, or has left, is not emailed again by submitting the form that names it.
#[sqlx::test]
async fn a_resend_is_only_for_a_membership_still_waiting_to_be_confirmed(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let news = s.list("News", true).await;
    let digest = s.list("Digest", true).await;
    s.form("newsletter", &[news, digest], None).await;
    let contact = make_contact(&pool, s.workspace, "known@ex.test").await;
    make_member(&pool, s.workspace, contact, news, "subscribed").await;
    make_member(&pool, s.workspace, contact, digest, "unsubscribed").await;

    s.submit("newsletter", "email=known@ex.test")
        .await
        .assert_status_ok();

    assert_eq!(
        jobs(&pool).await.len(),
        0,
        "one list is confirmed and the other was left: neither is waiting on an email"
    );
    assert_eq!(consent_rows(&pool).await.len(), 0);
}

/// Confirmation is the token's job, and only the token's.
///
/// `list_memberships.status` is `pending` and the list is single opt-in — reachable by flipping
/// `double_optin` after somebody was asked, which an operator may do at any time. A submission
/// naming that address must leave the row alone: promoting it would write a `confirmed` consent
/// row, with no `token_jti`, for a person who did nothing, because a stranger typed their address
/// into a public form. That is a forged record of consent in an append-only log.
#[sqlx::test]
async fn a_submission_cannot_confirm_someone_elses_pending_membership(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", false).await; // single opt-in now
    s.form("newsletter", &[list], None).await;
    let contact = make_contact(&pool, s.workspace, "asked@ex.test").await;
    make_member(&pool, s.workspace, contact, list, "pending").await;

    let r = s.submit("newsletter", "email=asked@ex.test").await;
    r.assert_status_ok();

    assert_eq!(
        s.memberships().await,
        [(list, "pending".to_owned(), None)],
        "a stranger's submission does not confirm anybody"
    );
    assert_eq!(
        consent_rows(&pool).await.len(),
        0,
        "and writes no record of a consent that did not happen"
    );
    // What it may do is ask again, by email, to the address itself - which is the only way that
    // row can ever be settled.
    assert_eq!(jobs(&pool).await.len(), 1);
    assert_eq!(jobs(&pool).await[0].1["email"], "asked@ex.test");
}

/// A bounce is a deliverability fact, not a consent decision, and nothing a subscriber can lift.
/// Someone re-typing their address into a signup form is usually fixing exactly that, and the
/// confirmation email is itself the deliverability test — so a bounced address signs up, and
/// every other block still refuses.
#[sqlx::test]
async fn a_bounced_address_may_sign_up_again_and_a_complaint_may_not(pool: PgPool) {
    let s = signup(&pool, "acme").await;
    let list = s.list("News", true).await;
    s.form("newsletter", &[list], None).await;
    block_address(&s, "bounced@ex.test", "bounced").await;
    block_address(&s, "angry@ex.test", "complained").await;
    block_address(&s, "blocked@ex.test", "manual").await;
    block_address(&s, "gone@ex.test", "unsubscribed_all").await;

    for email in [
        "bounced@ex.test",
        "angry@ex.test",
        "blocked@ex.test",
        "gone@ex.test",
    ] {
        s.submit("newsletter", &format!("email={email}"))
            .await
            .assert_status_ok();
    }

    // Exactly one of the four got through, and it is the bounce.
    let queued = jobs(&pool).await;
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(queued[0].1["email"], "bounced@ex.test");
    assert_eq!(contact_count(&pool).await, 1);
    let rows = consent_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "requested");
}

/// The real proof behind "a public route never writes the subscriber's IP into `audit_log`",
/// which is otherwise true only by construction and would stay true only by luck.
///
/// `0020_consent_log.sql` cascades consent rows on erasure *because* they carry an IP and a user
/// agent. `audit::record` writes `meta.ip` and `meta.user_agent` unconditionally, `audit_log` has
/// no foreign keys and nothing deletes from it — so the same data written there would outlive the
/// erasure that is supposed to remove it. `contacts::create` does not audit (the authenticated
/// route above it does), so today nothing under `src/public` audits at all. A handler that never
/// names the audit module cannot write one.
///
/// Same shape as the cookie walk above, and the same two limits: only `//` line comments are
/// skipped, and a string literal holding the word would be a false positive.
#[test]
fn no_handler_under_src_public_records_an_audit_row() {
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
            if source
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.to_lowercase().contains("audit"))
            {
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
    assert!(seen.len() >= 5, "the walk found only {seen:?}");
    assert_eq!(
        offenders,
        Vec::<String>::new(),
        "a public handler names the audit log; a subscriber's IP and user agent belong in \
         `consent_log`, which an erasure cascades, and `audit_log` has no foreign keys and \
         nothing that deletes from it"
    );
}
