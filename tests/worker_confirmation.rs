// SPDX-License-Identifier: AGPL-3.0-only

//! The `confirmation_email` handler, driven end to end: a claimed row, a real sending domain, a
//! real relay row, and an in-process SMTP sink standing in for the relay.
//!
//! **Three of these tests assert that nothing was sent, and the outcome alone cannot say that.**
//! A job that was refused before the connection and a job that was sent and then recorded as a
//! failure both end with `failed_at` set and a reason on the row. So each of them asserts
//! `sink.connections() == 0` as well: the sink counts a connection the moment it accepts one,
//! before a byte is read, so a handler that connects and thinks better of it is still caught.
//!
//! **Nothing here asserts an elapsed time as an equality.** The one quantity that moves —
//! `run_after` under `an_unknown_outcome_is_never_retried` — is read before the handler runs and
//! compared with itself afterwards, which is exact without being a clock reading.

mod common;

use std::{sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use common::smtp_sink::{AfterData, Script, Sink};
use nm::{
    config::Config,
    crypto::{secrets::Secrets, suppression_hmac},
    public::token::{self, Purpose, Token},
    relay::transport::{Outcome, Transports},
    worker::{
        self, Claimed,
        confirmation::{Ctx, handle},
    },
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// The instance's own origin. Deliberately **not** the sending domain, **not** the relay's host
/// and **not** the recipient's domain, so an unsubscribe URL derived from any of those would
/// read differently.
const PUBLIC_URL: &str = "https://mail.example.test";
const SENDING_DOMAIN: &str = "brand.test";
const ADDRESS: &str = "reader@dest.test";

const KEY_PEM: &str = include_str!("fixtures/dkim/signing-2048-pkcs8.pem");

/// `https` because `mail::build` refuses anything else as an unsubscribe URL, and this product
/// only ever sends one of its own.
fn config() -> Config {
    let mut c = common::test_config("postgres://unused");
    c.public_url = PUBLIC_URL.parse().expect("the public URL");
    c
}

/// The DER a PEM carries, which is the form `sending_domains` stores.
fn der(pem: &str) -> Vec<u8> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .concat();
    STANDARD.decode(body).expect("the fixture key's base64")
}

/// Everything one confirmation needs: a workspace with a list, a contact, one enabled relay
/// pointing at `sink`, and one sending domain whose checks all pass.
struct World {
    pool: PgPool,
    ctx: Ctx,
    secrets: Arc<Secrets>,
    ws: Uuid,
    list: Uuid,
    contact: Uuid,
    sink: Sink,
}

impl World {
    async fn new(pool: PgPool, script: Script) -> World {
        let sink = Sink::start(script).await;
        let (ws, list, contact) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

        sqlx::query("insert into workspaces (id, slug, name) values ($1, 'acme', 'Acme')")
            .bind(ws)
            .execute(&pool)
            .await
            .expect("workspace");
        sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, 'News')")
            .bind(list)
            .bind(ws)
            .execute(&pool)
            .await
            .expect("list");
        sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
            .bind(contact)
            .bind(ws)
            .bind(ADDRESS)
            .execute(&pool)
            .await
            .expect("contact");
        // `pending`, which is what a double opt-in signup writes before the link is clicked.
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status)
             values ($1, $2, $3, 'pending')",
        )
        .bind(ws)
        .bind(contact)
        .bind(list)
        .execute(&pool)
        .await
        .expect("membership");
        sqlx::query(
            "insert into relays (id, workspace_id, name, host, port, security,
                                 rate_limit_per_minute)
             values ($1, $2, 'primary', $3, $4, 'starttls', 600)",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(sink.addr.ip().to_string())
        .bind(i32::from(sink.addr.port()))
        .execute(&pool)
        .await
        .expect("relay");
        sqlx::query(
            "insert into sending_domains (id, workspace_id, domain, signing, dkim_selector, checks)
             values ($1, $2, $3, 'relay', 'nm', $4)",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(SENDING_DOMAIN)
        .bind(all_checks("pass"))
        .execute(&pool)
        .await
        .expect("sending domain");

        let secrets = Arc::new(Secrets::new(&config().secret_key));
        let ctx = Ctx {
            db: pool.clone(),
            secrets: Arc::clone(&secrets),
            config: Arc::new(config()),
            transports: Arc::new(Transports::plaintext_for_tests()),
        };
        World {
            pool,
            ctx,
            secrets,
            ws,
            list,
            contact,
            sink,
        }
    }

    /// A confirm token for this world's contact and list, as `public::forms` issues one.
    fn confirm_token(&self) -> String {
        token::issue(
            &self.secrets,
            &Token {
                workspace_id: self.ws,
                contact_id: self.contact,
                purpose: Purpose::Confirm,
                list_id: Some(self.list),
                issued_at: Utc::now().timestamp(),
                nonce: Uuid::now_v7().to_string(),
                kid: None,
                ref_id: None,
            },
        )
    }

    /// Queues the payload an M1d form would queue. `age_days` ages `created_at`, which is what
    /// the staleness rule reads.
    async fn enqueue(&self, contact: Uuid, payload: Value, age_days: f64) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "insert into jobs (id, workspace_id, kind, contact_id, list_id, payload, created_at)
             values ($1, $2, 'confirmation_email', $3, $4, $5,
                     now() - ($6::double precision * interval '1 day'))",
        )
        .bind(id)
        .bind(self.ws)
        .bind(contact)
        .bind(self.list)
        .bind(payload)
        .bind(age_days)
        .execute(&self.pool)
        .await
        .expect("job");
        id
    }

    /// The ordinary case: one fresh, well-formed job.
    async fn enqueue_one(&self) -> Uuid {
        self.enqueue(
            self.contact,
            json!({ "email": ADDRESS, "token": self.confirm_token() }),
            0.0,
        )
        .await
    }

    async fn claim(&self) -> Vec<Claimed> {
        worker::claim(&self.pool, "confirmation_email", worker::BATCH, 120)
            .await
            .expect("claim")
    }

    /// Claims and handles exactly one row, which is what every test but the batch one wants.
    async fn tick_one(&self) -> Outcome {
        let claimed = self.claim().await;
        assert_eq!(claimed.len(), 1, "one job was queued");
        handle(&self.ctx, &claimed[0], None).await
    }

    async fn row(&self, id: Uuid) -> JobRow {
        sqlx::query_as::<_, JobRow>(
            "select failed_at, completed_at, last_error, run_after from jobs where id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .expect("the job row")
    }

    async fn set_checks(&self, status: &str) {
        self.set_checks_json(all_checks(status)).await;
    }

    async fn set_checks_json(&self, checks: Value) {
        sqlx::query("update sending_domains set checks = $1 where workspace_id = $2")
            .bind(checks)
            .bind(self.ws)
            .execute(&self.pool)
            .await
            .expect("checks");
    }

    /// A second relay, tried after or before this world's sink depending on `failover_order`.
    async fn add_relay(&self, name: &str, addr: std::net::SocketAddr, failover_order: i32) {
        sqlx::query(
            "insert into relays (id, workspace_id, name, host, port, security,
                                 rate_limit_per_minute, failover_order)
             values ($1, $2, $3, $4, $5, 'starttls', 600, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(self.ws)
        .bind(name)
        .bind(addr.ip().to_string())
        .bind(i32::from(addr.port()))
        .bind(failover_order)
        .execute(&self.pool)
        .await
        .expect("the second relay");
    }

    async fn acknowledge_unverified(&self) {
        sqlx::query(
            "update sending_domains set send_unverified_acknowledged_at = now()
              where workspace_id = $1",
        )
        .bind(self.ws)
        .execute(&self.pool)
        .await
        .expect("acknowledgement");
    }

    /// The one message the sink read, as text.
    fn only_message(&self) -> String {
        let messages = self.sink.messages();
        assert_eq!(messages.len(), 1, "exactly one message reached the sink");
        String::from_utf8(messages[0].clone()).expect("the message is ASCII")
    }
}

#[derive(sqlx::FromRow)]
struct JobRow {
    failed_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    run_after: DateTime<Utc>,
}

/// All four DNS checks at one status. `spf` carries a reason when it fails, so a refusal that
/// names it has something to name.
fn all_checks(status: &str) -> Value {
    let one = |name: &str| {
        if status == "fail" {
            json!({ "status": "fail", "reason": format!("no {name} record was published") })
        } else {
            json!({ "status": status })
        }
    };
    json!({
        "spf": one("SPF"),
        "dkim": one("DKIM"),
        "dmarc": one("DMARC"),
        "return_path": one("Return-Path"),
    })
}

/// The body with quoted-printable soft line breaks joined up.
///
/// lettre encodes the text part and wraps it at 76 columns, so a token-bearing URL is split
/// across lines with a trailing `=`. The first version of the confirm-link assertion checked only
/// the `https://…/c/` prefix and passed for that reason — the whole link was never in these bytes
/// at all. A recipient's mail client joins them; this joins them the same way, so the assertion
/// is about the link a person would click.
fn body(wire: &str) -> String {
    wire.split_once("\r\n\r\n")
        .map_or_else(|| wire.to_owned(), |(_, body)| body.replace("=\r\n", ""))
}

/// The value of the first header called `name`, read off the wire bytes.
///
/// Folded continuation lines are joined first: a DKIM signature is long enough that lettre folds
/// it, and its `h=` list lands on the second line. A reader that took only the first line would
/// have let `signed_headers_covers_both_unsubscribe_headers` pass on a signature covering
/// nothing.
fn header(wire: &str, name: &str) -> Option<String> {
    let head = wire.split("\r\n\r\n").next().unwrap_or(wire);
    let head = head.replace("\r\n ", " ").replace("\r\n\t", " ");
    head.split("\r\n").find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
    })
}

// ---------------------------------------------------------------------------------------------
// Invariant 2: a suppressed or erased address is never mailed.

/// A standing block is read at send time, not only at signup. The sink would answer `250` to
/// anything, so the only thing standing between this address and a message is the handler.
#[sqlx::test]
async fn a_suppressed_address_is_never_mailed(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason)
         values ($1, $2, 'address', $3, $4, 'complained')",
    )
    .bind(Uuid::now_v7())
    .bind(w.ws)
    .bind(ADDRESS)
    .bind(suppression_hmac(&w.secrets, ADDRESS).as_slice())
    .execute(&w.pool)
    .await
    .expect("suppression");

    let id = w.enqueue_one().await;
    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0, "a suppressed address was dialled");
    let row = w.row(id).await;
    assert!(row.failed_at.is_some(), "the job was not failed");
    assert!(row.completed_at.is_none());
    let reason = row.last_error.expect("a reason");
    assert!(
        reason.to_ascii_lowercase().contains("suppress"),
        "the reason does not name the suppression: {reason}"
    );
}

/// **The test the view could not provide.** A GDPR erasure tombstone keeps the HMAC of the
/// address and destroys the plaintext, so `mailable_list_members` — which matches on plaintext —
/// still returns the member. `blocking_suppression` matches on the hash and does not.
///
/// The membership is moved to `subscribed` for one reason only: that is what the view requires
/// before it returns a row at all, and the assertion below has to be able to fail. It is not what
/// a pending double opt-in looks like.
#[sqlx::test]
async fn an_erased_contact_is_never_mailed(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query("update list_memberships set status = 'subscribed' where contact_id = $1")
        .bind(w.contact)
        .execute(&w.pool)
        .await
        .expect("subscribe");

    let mut conn = w.pool.acquire().await.expect("a connection");
    nm::suppressions::erase_address(&mut conn, &w.secrets, w.ws, ADDRESS)
        .await
        .expect("the erasure tombstone");
    drop(conn);

    // The whole point, asserted rather than assumed: the view is blind to this block. A refactor
    // that sends through `mailable_list_members` again fails here, not in production.
    let visible: i64 = sqlx::query_scalar(
        "select count(*) from mailable_list_members where contact_id = $1 and list_id = $2",
    )
    .bind(w.contact)
    .bind(w.list)
    .fetch_one(&w.pool)
    .await
    .expect("the view");
    assert_eq!(
        visible, 1,
        "mailable_list_members no longer hides the tombstone, so this test proves nothing"
    );

    let id = w.enqueue_one().await;
    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0, "an erased address was dialled");
    let row = w.row(id).await;
    assert!(row.failed_at.is_some(), "the job was not failed");
    let reason = row.last_error.expect("a reason");
    assert!(
        reason.to_ascii_lowercase().contains("suppress"),
        "the reason does not name the suppression: {reason}"
    );
    // The reason is stored and read by an operator; it must not re-publish what erasure destroyed.
    assert!(!reason.contains(ADDRESS), "the reason names the address");
}

// ---------------------------------------------------------------------------------------------
// The stale row.

/// `Purpose::Confirm` is good for 14 days, nothing re-issues the token, and rows are kept
/// indefinitely. Mailing a 15-day-old row sends someone to a `link-expired` page, which is worse
/// than not sending.
#[sqlx::test]
async fn a_job_older_than_the_confirm_ttl_fails_and_is_not_sent(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let stale = w
        .enqueue(
            w.contact,
            json!({ "email": ADDRESS, "token": w.confirm_token() }),
            15.0,
        )
        .await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0, "a dead link was mailed");
    let row = w.row(stale).await;
    assert!(row.failed_at.is_some(), "the stale job was not failed");
    // Against the TTL itself, not a number spelled out here: a second statement of the window is
    // exactly what the handler refuses to carry.
    let ttl = Purpose::Confirm.ttl_secs().expect("a confirm link expires");
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or_default()
            .contains(&ttl.to_string()),
        "the reason does not name the window it passed: {:?}",
        row.last_error
    );
}

/// The other half, so the rule is a window and not a blanket refusal: a row inside the TTL is
/// sent. Thirteen days, not thirteen minutes — a boundary read off the same constant.
#[sqlx::test]
async fn a_job_inside_the_confirm_ttl_is_sent(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let id = w
        .enqueue(
            w.contact,
            json!({ "email": ADDRESS, "token": w.confirm_token() }),
            13.0,
        )
        .await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Sent { .. }), "{outcome:?}");
    assert!(w.row(id).await.completed_at.is_some());
}

// ---------------------------------------------------------------------------------------------
// Invariant 3: the unsubscribe link is ours.

/// **Invariant 3.** `mail::build` enforces only that the URL is `https`; whose host it names is
/// decided here. The assertions are written so that an unsubscribe URL taken from anywhere else
/// fails them:
///
/// * the origin is the instance's `public_url` — not the sending domain, not the relay's host,
///   not the recipient's domain, all three of which are different strings in this test;
/// * the token parses, under this instance's key, as an **unsubscribe** token for this exact
///   workspace and contact. A relay's own opaque link cannot satisfy that, and neither can the
///   confirm token the job payload carries.
#[sqlx::test]
async fn a_sent_message_carries_our_unsubscribe_link(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    // Minted here, so the assertions below can name the exact token that was queued rather than
    // settle for the shape of a link.
    let queued_token = w.confirm_token();
    let id = w
        .enqueue(
            w.contact,
            json!({ "email": ADDRESS, "token": queued_token }),
            0.0,
        )
        .await;

    let outcome = w.tick_one().await;
    assert!(matches!(outcome, Outcome::Sent { .. }), "{outcome:?}");
    assert!(w.row(id).await.completed_at.is_some());

    let wire = w.only_message();
    // The message goes from the sending domain to the address it was queued for. Obvious, and
    // unpinned until a mutation swapped the From and the suite stayed green.
    assert_eq!(
        header(&wire, "From").as_deref(),
        Some(format!("no-reply@{SENDING_DOMAIN}").as_str()),
        "{wire}"
    );
    assert_eq!(header(&wire, "To").as_deref(), Some(ADDRESS), "{wire}");
    assert_eq!(
        header(&wire, "List-Unsubscribe-Post").as_deref(),
        Some("List-Unsubscribe=One-Click"),
        "{wire}"
    );
    let raw = header(&wire, "List-Unsubscribe").expect("a List-Unsubscribe header");
    let url = raw
        .strip_prefix('<')
        .and_then(|u| u.strip_suffix('>'))
        .unwrap_or(&raw);
    let url: url::Url = url.parse().expect("the unsubscribe URL");

    let ours: url::Url = PUBLIC_URL.parse().unwrap();
    assert_eq!(url.origin(), ours.origin(), "the link is not on our origin");
    assert!(
        !raw.contains(SENDING_DOMAIN) && !raw.contains("dest.test") && !raw.contains("127.0.0.1"),
        "the link borrows a host from somewhere other than public_url: {raw}"
    );

    let raw_token = url
        .path()
        .strip_prefix("/u/")
        .expect("an /u/{{token}} path");
    let parsed = token::parse(&w.secrets, raw_token, Purpose::Unsubscribe)
        .expect("our own unsubscribe token");
    assert_eq!(parsed.workspace_id, w.ws);
    assert_eq!(parsed.contact_id, w.contact);
    assert_eq!(parsed.list_id, None, "an unsubscribe token names no list");

    // And the confirm link the message exists to deliver is on our origin and carries **this
    // job's** token. A prefix assertion alone passes while the handler mails somebody else's
    // link, or a fresh one it minted itself — and a fresh one would be a second credential for a
    // row the resend throttle counts as one.
    assert!(
        body(&wire).contains(&format!("{PUBLIC_URL}/c/{queued_token}")),
        "the confirmation link is not the queued one: {wire}"
    );
    // The unsubscribe link is in the body too, for a reader whose client shows no header.
    assert!(
        body(&wire).contains(url.as_str()),
        "the body's unsubscribe link differs from the header's: {wire}"
    );
    let confirmed = token::parse(&w.secrets, &queued_token, Purpose::Confirm)
        .expect("the queued token is one of ours");
    assert_eq!(confirmed.workspace_id, w.ws);
    assert_eq!(confirmed.contact_id, w.contact);
    assert_eq!(confirmed.list_id, Some(w.list));
    assert!(
        !wire.to_ascii_lowercase().contains("mailto:"),
        "a mailto: unsubscribe was offered: {wire}"
    );
}

/// An app-signed domain signs what it submits, and the signature covers both unsubscribe headers
/// — RFC 8058 requires that, and it is the half of invariant 3 the relay-signed path cannot show.
#[sqlx::test]
async fn an_app_signed_domain_signs_what_it_submits(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let secrets = Secrets::new(&config().secret_key);
    sqlx::query(
        "update sending_domains set signing = 'app', dkim_private_key_encrypted = $1
          where workspace_id = $2",
    )
    .bind(secrets.encrypt(&der(KEY_PEM)))
    .bind(w.ws)
    .execute(&w.pool)
    .await
    .expect("the signing key");

    w.enqueue_one().await;
    let outcome = w.tick_one().await;
    assert!(matches!(outcome, Outcome::Sent { .. }), "{outcome:?}");

    let wire = w.only_message();
    let sig = header(&wire, "DKIM-Signature").expect("a DKIM-Signature header");
    assert!(sig.contains(&format!("d={SENDING_DOMAIN}")), "{sig}");
    assert!(sig.contains("s=nm"), "{sig}");
    let lower = sig.to_ascii_lowercase();
    assert!(lower.contains("list-unsubscribe"), "{sig}");
    assert!(lower.contains("list-unsubscribe-post"), "{sig}");
}

// ---------------------------------------------------------------------------------------------
// The unverified-domain gate.

/// The settings screen tells an operator that a send will not happen until an admin ticks the
/// box. This is what makes that sentence true.
#[sqlx::test]
async fn an_unverified_domain_is_not_sent_from_without_an_acknowledgement(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    w.set_checks("fail").await;
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(
        w.sink.connections(),
        0,
        "an unverified domain was sent from"
    );
    let reason = w.row(id).await.last_error.expect("a reason");
    for named in ["spf", "dkim", "dmarc"] {
        assert!(
            reason.to_ascii_lowercase().contains(named),
            "the reason does not name the {named} check: {reason}"
        );
    }
}

/// The other half. The tick-box is only worth having if ticking it is what changes the answer —
/// nothing else about this world differs from the test above.
#[sqlx::test]
async fn an_acknowledged_unverified_domain_is_sent_from(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    w.set_checks("fail").await;
    w.acknowledge_unverified().await;
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Sent { .. }), "{outcome:?}");
    assert_eq!(w.sink.connections(), 1);
    assert!(w.row(id).await.completed_at.is_some());
}

/// **Each of the four checks is read, and each one alone closes the gate.** Setting all four
/// together — which every other gate test does — leaves the gate free to stop considering any one
/// of them with nothing going red: deleting `Return-Path` from the list passed the whole suite.
/// So each check is failed on its own here, and the refusal must name that one and **not** the
/// three that are passing.
#[sqlx::test]
async fn every_dns_check_closes_the_gate_on_its_own(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    for (field, named) in [
        ("spf", "SPF"),
        ("dkim", "DKIM"),
        ("dmarc", "DMARC"),
        ("return_path", "Return-Path"),
    ] {
        let mut checks = all_checks("pass");
        checks[field] = json!({ "status": "fail", "reason": "not published" });
        w.set_checks_json(checks).await;
        let id = w.enqueue_one().await;

        let outcome = w.tick_one().await;

        assert!(
            matches!(outcome, Outcome::Failed(_)),
            "a failing {named} did not close the gate: {outcome:?}"
        );
        let reason = w.row(id).await.last_error.expect("a reason");
        assert!(
            reason.contains(named),
            "the refusal does not name the {named} check: {reason}"
        );
        // And names only it: a refusal that listed all four whatever failed would pass the line
        // above while telling an operator nothing about which record to publish.
        for other in ["SPF", "DKIM", "DMARC", "Return-Path"] {
            assert!(
                other == named || !reason.contains(other),
                "a failing {named} was reported as a failing {other}: {reason}"
            );
        }
    }
    assert_eq!(w.sink.connections(), 0, "an unverified domain was dialled");
}

/// A check that has never run is not a passing check. `not_checked` is what a domain starts with,
/// so reading "not failing" as "fine" would let every brand-new domain through the gate.
#[sqlx::test]
async fn a_domain_whose_checks_have_never_run_is_treated_as_unverified(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    w.set_checks("not_checked").await;
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0);
    assert!(w.row(id).await.failed_at.is_some());
}

/// **A domain with no custom return-path domain sends on the other three checks.** The
/// Return-Path is the relay's own unless one is configured (spec §9), so there is no record of
/// ours to look up and the checker writes `not_checked` — the same word it writes for a check
/// nobody has run yet. Reading those two facts as one made the reference deployment permanently
/// unverified, and `Failed` destroys a confirmation rather than retrying it.
#[sqlx::test]
async fn a_domain_with_no_return_path_configured_sends_on_the_other_three(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    // Exactly what `dns::check` writes for the domain `World` inserts, which has no
    // `return_path_domain`: three answers, and no lookup for the fourth.
    let mut checks = all_checks("pass");
    checks["return_path"] = json!({ "status": "not_checked" });
    w.set_checks_json(checks).await;
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Sent { .. }), "{outcome:?}");
    assert_eq!(w.sink.connections(), 1);
    let row = w.row(id).await;
    assert!(row.completed_at.is_some());
    assert_eq!(row.failed_at, None, "{:?}", row.last_error);
}

/// The other half, which is what stops the one above from becoming "Return-Path never matters": a
/// domain that *does* carry a return-path domain is still held to its verdict.
#[sqlx::test]
async fn a_configured_return_path_that_fails_still_closes_the_gate(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query("update sending_domains set return_path_domain = $1 where workspace_id = $2")
        .bind("bounce.example.net")
        .bind(w.ws)
        .execute(&w.pool)
        .await
        .expect("a return-path domain");
    let mut checks = all_checks("pass");
    checks["return_path"] = json!({
        "status": "fail",
        "reason": "the return-path domain is neither the sending domain nor a subdomain of it",
    });
    w.set_checks_json(checks).await;
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0);
    let reason = w.row(id).await.last_error.expect("a reason");
    assert!(reason.contains("Return-Path"), "{reason}");
}

// ---------------------------------------------------------------------------------------------
// Invariants 4 and 5: what the relay's answer is recorded as.

/// **Invariant 4.** The relay read the whole message and went away without answering, so it may
/// have queued it. A second copy of a confirmation email is indistinguishable from the product
/// being broken, so the row is given up on rather than offered again.
#[sqlx::test]
async fn an_unknown_outcome_is_never_retried(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Drop)).await;
    let id = w.enqueue_one().await;

    let claimed = w.claim().await;
    assert_eq!(claimed.len(), 1);
    // Read after the claim, which advances `run_after` by the lease: this is the value the
    // handler must leave alone. Comparing it with itself is exact and reads no clock.
    let leased_until = w.row(id).await.run_after;

    let outcome = handle(&w.ctx, &claimed[0], None).await;
    assert!(matches!(outcome, Outcome::Unknown(_)), "{outcome:?}");

    let row = w.row(id).await;
    assert!(row.failed_at.is_some(), "an unknown outcome was not failed");
    assert!(row.completed_at.is_none());
    assert_eq!(
        row.run_after, leased_until,
        "an unknown outcome was rescheduled"
    );
    assert!(
        w.claim().await.is_empty(),
        "an unknown outcome was offered again"
    );
    // It did reach the relay: this is the ambiguous case, not the refused one.
    assert_eq!(w.sink.connections(), 1);
}

/// **The other half of invariant 4, and the one the single-relay test cannot reach.** An
/// `Unknown` means the message may already be queued at the first relay. Offering it to the
/// second is the same double send arriving by failover instead of by retry, and with one relay
/// configured nothing stops `submit` walking on.
///
/// The second sink must see **no connection at all** — not "no message": a handler that dialled
/// the second relay and then thought better of it has still decided the wrong thing.
#[sqlx::test]
async fn an_unknown_outcome_does_not_fail_over_to_the_next_relay(pool: PgPool) {
    // The world's own relay is `failover_order` 0 and drops after DATA.
    let w = World::new(pool, Script::Smtp(AfterData::Drop)).await;
    let second = Sink::start(Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    w.add_relay("standby", second.addr, 1).await;

    let id = w.enqueue_one().await;
    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Unknown(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 1, "the first relay was not tried");
    assert_eq!(
        second.connections(),
        0,
        "an ambiguous outcome was offered to a second relay: the message may now be queued twice"
    );
    assert!(w.row(id).await.failed_at.is_some());
}

/// And the mirror, so the rule above is "`Unknown` stops" and not "nothing ever fails over": a
/// `5xx` is equally final at the next relay, which would refuse it for the same reason.
#[sqlx::test]
async fn a_permanent_refusal_does_not_fail_over_to_the_next_relay(pool: PgPool) {
    let w = World::new(
        pool,
        Script::Smtp(AfterData::Reply("550 5.1.1 no such user")),
    )
    .await;
    let second = Sink::start(Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    w.add_relay("standby", second.addr, 1).await;

    let id = w.enqueue_one().await;
    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 1);
    assert_eq!(
        second.connections(),
        0,
        "a 5xx was offered to a second relay that would refuse it too"
    );
    assert!(w.row(id).await.failed_at.is_some());
}

/// **Invariant 5.** OCI accepts mail to a suppressed address rather than rejecting it, answering
/// `2xx` with a suppression notice. Recording that as a delivery tells the operator that someone
/// received mail who did not.
#[sqlx::test]
async fn a_suppression_reply_is_recorded_as_a_failure(pool: PgPool) {
    let w = World::new(
        pool,
        Script::Smtp(AfterData::Reply(
            "250 2.0.0 Ok: Suppression for user reader",
        )),
    )
    .await;
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(
        !matches!(outcome, Outcome::Sent { .. }),
        "a suppression notice was recorded as a delivery: {outcome:?}"
    );
    let row = w.row(id).await;
    assert!(row.completed_at.is_none(), "the job was marked delivered");
    assert!(row.failed_at.is_some());
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("Suppression for user"),
        "the relay's own words are not on the row: {:?}",
        row.last_error
    );
}

/// A 4xx is the relay asking for the message later, not refusing it. The row keeps no `failed_at`
/// and comes back round with a backoff.
#[sqlx::test]
async fn a_transient_reply_is_deferred_and_offered_again(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("451 4.3.0 later"))).await;
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Transient(_)), "{outcome:?}");
    let row = w.row(id).await;
    assert!(row.failed_at.is_none(), "a 4xx gave up on the message");
    assert!(row.completed_at.is_none());
    // The backoff's first step is 15–30 s and the claim's lease is 120 s, so a deferred row is
    // scheduled sooner than its lease would have offered it. A bound, not an equality: the
    // backoff is jittered.
    assert!(
        row.run_after < Utc::now() + chrono::Duration::seconds(120),
        "the backoff did not move the row ahead of its lease"
    );
    assert!(row.last_error.is_some(), "no reason was left on the row");
}

// ---------------------------------------------------------------------------------------------
// One bad row does not poison the tick.

/// **A payload that does not parse fails that row and nothing else.** Three jobs are claimed
/// together; the middle one carries `{"email": 42}`. A handler that panicked, or that gave up on
/// the batch, would leave the other two unsent — and they are the two that would be hardest to
/// explain, because nothing is wrong with them.
#[sqlx::test]
async fn a_malformed_payload_fails_that_row_only(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let mut ids = Vec::new();
    for (i, payload) in [
        json!({ "email": ADDRESS, "token": w.confirm_token() }),
        json!({ "email": 42 }),
        json!({ "email": ADDRESS, "token": w.confirm_token() }),
    ]
    .into_iter()
    .enumerate()
    {
        // Each job needs a contact of its own: `jobs_confirmation_email_shape` requires one, and
        // the address is the same on all three so the recipient is not what tells them apart.
        let contact = if i == 0 {
            w.contact
        } else {
            let id = Uuid::now_v7();
            sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
                .bind(id)
                .bind(w.ws)
                .bind(format!("reader+{i}@dest.test"))
                .execute(&w.pool)
                .await
                .expect("contact");
            id
        };
        ids.push(w.enqueue(contact, payload, 0.0).await);
    }

    let claimed = w.claim().await;
    assert_eq!(claimed.len(), 3, "all three were claimed in one tick");
    for job in &claimed {
        handle(&w.ctx, job, None).await;
    }

    let bad = w.row(ids[1]).await;
    assert!(bad.failed_at.is_some(), "the malformed row was not failed");
    assert!(
        bad.last_error.is_some(),
        "the malformed row carries no reason"
    );
    for good in [ids[0], ids[2]] {
        let row = w.row(good).await;
        assert!(
            row.completed_at.is_some(),
            "a well-formed row did not go out beside a malformed one"
        );
        assert!(row.failed_at.is_none());
    }
    // Messages, not connections: lettre parks and reuses one pooled connection across the two.
    assert_eq!(
        w.sink.messages().len(),
        2,
        "the malformed row was submitted, or a good one was not"
    );
    assert!(
        w.claim().await.is_empty(),
        "the tick left a row claimable, so a second tick would run it"
    );
}

/// An address this product cannot read is given up on, not retried.
///
/// `blocking_suppression` reports one as an `Internal` error, and every database error here is
/// `Transient` — rightly, because a dropped connection says nothing about the message. But an
/// address that does not normalise today will not normalise on the fifth attempt, so without a
/// check of its own this row burns `max_attempts` over four hours on something that cannot
/// improve. It also must not be *sent*: the fail-closed reading is refuse, never guess.
#[sqlx::test]
async fn an_address_that_cannot_be_normalised_gives_up_rather_than_retrying(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let id = w
        .enqueue(
            w.contact,
            json!({ "email": "no-at-sign", "token": w.confirm_token() }),
            0.0,
        )
        .await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0, "an unreadable address was mailed");
    let row = w.row(id).await;
    assert!(
        row.failed_at.is_some(),
        "an address that will never be readable was left to be retried"
    );
    assert!(row.last_error.is_some());
}

// ---------------------------------------------------------------------------------------------
// Configuration that is missing rather than wrong.

/// **The pair, which is the point: the same shape of operator mistake gets opposite verdicts, and
/// each one has to be the right verdict for its own reason.**
///
/// A workspace with no *sending domain* has no from-address, no `Message-ID` domain and nothing
/// to sign with. No retry produces one, so the row gives up now and says why, rather than burning
/// five attempts over four hours on a configuration nobody is about to change by accident.
#[sqlx::test]
async fn a_workspace_with_no_sending_domain_gives_up(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query("delete from sending_domains where workspace_id = $1")
        .bind(w.ws)
        .execute(&w.pool)
        .await
        .expect("remove the sending domain");
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0);
    let row = w.row(id).await;
    assert!(row.failed_at.is_some(), "the row was left to be retried");
    let reason = row.last_error.expect("a reason");
    assert!(
        reason.contains("sending domain"),
        "the reason does not say what is missing: {reason}"
    );
}

/// A workspace with no *relay*, by contrast, is plausibly mid-edit: a relay is disabled and
/// re-enabled in the course of ordinary maintenance, and a confirmation that waited a minute for
/// that is better than one destroyed by it. The row must still be there when the operator is done.
#[sqlx::test]
async fn a_workspace_with_no_enabled_relay_defers_rather_than_gives_up(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query("update relays set enabled = false where workspace_id = $1")
        .bind(w.ws)
        .execute(&w.pool)
        .await
        .expect("disable");
    let id = w.enqueue_one().await;

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Transient(_)), "{outcome:?}");
    assert_eq!(w.sink.connections(), 0);
    let row = w.row(id).await;
    assert!(row.failed_at.is_none(), "a missing relay destroyed the row");
    assert!(row.completed_at.is_none());
}

/// **And deferring is not waiting for ever.** `Transient` is only defensible if the row
/// eventually gives up; otherwise a workspace that genuinely has no relay accumulates rows that
/// are retried hourly and never answer "why did this confirmation never arrive?".
///
/// `max_attempts` is set to 1, so the claim's own increment takes `attempts` to its limit and
/// `defer` fails the row instead of rescheduling it. That is `worker::defer`'s rule, exercised
/// through this handler rather than assumed from it.
#[sqlx::test]
async fn a_deferred_row_gives_up_once_it_runs_out_of_attempts(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query("update relays set enabled = false where workspace_id = $1")
        .bind(w.ws)
        .execute(&w.pool)
        .await
        .expect("disable");
    let id = w.enqueue_one().await;
    sqlx::query("update jobs set max_attempts = 1 where id = $1")
        .bind(id)
        .execute(&w.pool)
        .await
        .expect("one attempt only");

    let outcome = w.tick_one().await;

    assert!(matches!(outcome, Outcome::Transient(_)), "{outcome:?}");
    let row = w.row(id).await;
    assert!(
        row.failed_at.is_some(),
        "the last attempt deferred instead of giving up, so nothing will ever write failed_at"
    );
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("relay"),
        "the row does not say why it never went: {:?}",
        row.last_error
    );
    assert!(w.claim().await.is_empty());
}

/// Failover is what `failover_order` is for. The first relay is an address with nothing
/// listening, which the transport classes transient; the second is the sink. One message, and it
/// goes out on the relay that works.
#[sqlx::test]
async fn a_relay_that_cannot_be_reached_falls_through_to_the_next(pool: PgPool) {
    let w = World::new(pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let dead = Sink::unused_addr().await;
    sqlx::query("update relays set failover_order = 1 where workspace_id = $1")
        .bind(w.ws)
        .execute(&w.pool)
        .await
        .expect("demote the sink");
    sqlx::query(
        "insert into relays (id, workspace_id, name, host, port, security,
                             rate_limit_per_minute, failover_order)
         values ($1, $2, 'dead', $3, $4, 'starttls', 600, 0)",
    )
    .bind(Uuid::now_v7())
    .bind(w.ws)
    .bind(dead.ip().to_string())
    .bind(i32::from(dead.port()))
    .execute(&w.pool)
    .await
    .expect("the unreachable relay");

    let id = w.enqueue_one().await;
    let outcome = tokio::time::timeout(Duration::from_secs(30), w.tick_one())
        .await
        .expect("the handler finished");

    assert!(matches!(outcome, Outcome::Sent { .. }), "{outcome:?}");
    assert!(w.row(id).await.completed_at.is_some());
    assert_eq!(w.sink.connections(), 1, "the working relay saw one message");
}
