// SPDX-License-Identifier: AGPL-3.0-only

//! The send loop (`nm::campaign::send`, spec M2c §4.7), driven end to end: a campaign launched
//! through the real launch, a real relay row, and `common::smtp_sink` standing in for the relay.
//!
//! The two outcomes the spec ranks worst are a message sent twice and a message to someone who
//! must not get it, so most of these count what the sink saw — connections and messages — and not
//! only what the rows say: a row can claim "skipped" for a message that went out anyway.

mod common;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use common::{
    PW, TestApp, add_member, create_user, create_workspace,
    smtp_sink::{AfterData, Script, Sink},
};
use nm::{
    campaign::{
        launch::{self, LAUNCH_KIND},
        send::{CampaignWorker, States},
    },
    config::Config,
    crypto::{secrets::Secrets, suppression_hmac},
    public::token::{self, Purpose, Token},
    relay::transport::Transports,
    worker::{self, Worker, confirmation::Ctx},
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const PUBLIC_URL: &str = "https://mail.example.test";
const DOMAIN: &str = "brand.test";
const POSTAL: &str = "1 Main St, Springfield";
const ONE_CLICK: &str = "List-Unsubscribe=One-Click";
const KEY_PEM: &str = include_str!("fixtures/dkim/signing-2048-pkcs8.pem");

fn config() -> Config {
    let mut c = common::test_config("postgres://unused");
    c.public_url = PUBLIC_URL.parse().expect("the public URL");
    c
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

/// A workspace that can send a campaign: a brand kit with a postal address, a passing sending
/// domain, a template, a list, and one relay pointing at `sink`.
struct Fx {
    pool: PgPool,
    app: TestApp,
    ws: Uuid,
    domain: Uuid,
    template: Uuid,
    list: Uuid,
    relay: Uuid,
    secrets: Arc<Secrets>,
    transports: Arc<Transports>,
    sink: Sink,
}

async fn fx(pool: &PgPool, script: Script) -> Fx {
    let owner = create_user(pool, "owner@example.com", PW, false).await;
    let ws = create_workspace(pool, owner, "acme").await;
    let user = create_user(pool, "admin@example.com", PW, false).await;
    add_member(pool, ws, user, Role::Admin).await;
    let mut app = TestApp::with_config(pool.clone(), |c| *c = config()).await;
    app.login("admin@example.com", PW).await;

    app.put(&format!("/api/v1/workspaces/{ws}/brand-kit"))
        .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
                      "footer": {"address": POSTAL}}))
        .await
        .assert_status_ok();
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/sending-domains"))
        .json(&json!({"domain": DOMAIN, "signing": "relay"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let domain = id_of(&r);
    sqlx::query("update sending_domains set checks = $2 where id = $1")
        .bind(domain)
        .bind(
            json!({"spf": {"status": "pass"}, "dkim": {"status": "pass"},
                     "dmarc": {"status": "pass"}, "return_path": {"status": "pass"}}),
        )
        .execute(pool)
        .await
        .expect("checks");
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/templates"))
        .json(
            &json!({"name": "welcome", "kind": "blocks", "tree": {"blocks": [
            {"type": "Text", "runs": [{"type": "Tag", "source": "{{ first_name }}"}]}]}}),
        )
        .await;
    r.assert_status(StatusCode::CREATED);
    let template = id_of(&r);
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/lists"))
        .json(&json!({"name": "news", "double_optin": false}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let list = id_of(&r);

    let sink = Sink::start(script).await;
    let relay = Uuid::now_v7();
    let f = Fx {
        pool: pool.clone(),
        app,
        ws,
        domain,
        template,
        list,
        relay,
        secrets: Arc::new(Secrets::new(&config().secret_key)),
        transports: Arc::new(Transports::plaintext_for_tests()),
        sink,
    };
    f.add_relay(relay, "primary", f.sink.addr, 0).await;
    f
}

impl Fx {
    async fn add_relay(&self, id: Uuid, name: &str, addr: std::net::SocketAddr, order: i32) {
        sqlx::query(
            "insert into relays (id, workspace_id, name, host, port, security,
                                 rate_limit_per_minute, failover_order)
             values ($1, $2, $3, $4, $5, 'starttls', 600, $6)",
        )
        .bind(id)
        .bind(self.ws)
        .bind(name)
        .bind(addr.ip().to_string())
        .bind(i32::from(addr.port()))
        .bind(order)
        .execute(&self.pool)
        .await
        .expect("relay");
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            db: self.pool.clone(),
            secrets: Arc::clone(&self.secrets),
            config: Arc::new(config()),
            transports: Arc::clone(&self.transports),
        }
    }

    /// A messages-loop worker with a state map of its own.
    fn worker(&self, concurrency: usize) -> (CampaignWorker, Arc<States>) {
        let states = Arc::new(States::default());
        (
            CampaignWorker::new(Arc::new(self.ctx()), Arc::clone(&states), concurrency),
            states,
        )
    }

    /// One tick on a freshly read state map.
    async fn tick(&self, w: &(CampaignWorker, Arc<States>)) -> usize {
        w.1.refresh(&self.pool).await.expect("refresh");
        w.0.tick().await.expect("a tick")
    }

    /// Ticks until a tick claims nothing.
    async fn send_all(&self, w: &(CampaignWorker, Arc<States>)) {
        for _ in 0..20 {
            if self.tick(w).await == 0 {
                return;
            }
        }
        panic!("a send that never ends");
    }

    /// `n` subscribed contacts on the list; returns `(id, email)` of each.
    async fn contacts(&self, n: i32, first: &str) -> Vec<(Uuid, String)> {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "insert into contacts (id, workspace_id, email, first_name)
             select gen_random_uuid(), $1, $2 || '-' || g || '@dest.test', $2
               from generate_series(1, $3) g returning id, email",
        )
        .bind(self.ws)
        .bind(first)
        .bind(n)
        .fetch_all(&self.pool)
        .await
        .expect("contacts");
        let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status)
             select $1, c, $2, 'subscribed' from unnest($3::uuid[]) as c",
        )
        .bind(self.ws)
        .bind(self.list)
        .bind(&ids)
        .execute(&self.pool)
        .await
        .expect("memberships");
        rows
    }

    /// A campaign through the API, scheduled to send now and launched by the real launch job.
    async fn launched(&self) -> Uuid {
        let r = self
            .app
            .post(&format!("/api/v1/workspaces/{}/campaigns", self.ws))
            .json(&json!({
                "name": "c", "list_ids": [self.list], "sending_domain_id": self.domain,
                "from_local_part": "news", "from_name": "Acme", "reply_to": "hello@brand.test",
                "variants": [{"subject": "Hi {{ first_name }}", "template_id": self.template,
                              "from_name": null}],
            }))
            .await;
        r.assert_status(StatusCode::CREATED);
        let id = id_of(&r);
        let job = Uuid::now_v7();
        sqlx::query("update campaigns set state = 'scheduled', launch_job_id = $2 where id = $1")
            .bind(id)
            .bind(job)
            .execute(&self.pool)
            .await
            .expect("schedule");
        sqlx::query(
            "insert into jobs (id, workspace_id, kind, campaign_id, payload, run_after)
             values ($1, $2, 'campaign_launch', $3, '{}', now())",
        )
        .bind(job)
        .bind(self.ws)
        .bind(id)
        .execute(&self.pool)
        .await
        .expect("launch job");
        let ctx = self.ctx();
        while let Some(job) = worker::claim(&self.pool, LAUNCH_KIND, 1, 120)
            .await
            .expect("claim")
            .pop()
        {
            launch::handle(&ctx, &job, Duration::from_secs(60)).await;
        }
        assert_eq!(self.state(id).await, "sending", "the launch did not go");
        id
    }

    async fn state(&self, id: Uuid) -> String {
        sqlx::query_scalar("select state from campaigns where id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn pause_reason(&self, id: Uuid) -> Option<String> {
        sqlx::query_scalar("select pause_reason from campaigns where id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// `status -> count` over the campaign's messages.
    async fn statuses(&self, id: Uuid) -> BTreeMap<String, i64> {
        sqlx::query_as::<_, (String, i64)>(
            "select status, count(*) from messages where campaign_id = $1 group by status",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
    }

    /// `email -> (message id, status, attempts)`.
    async fn messages(&self, id: Uuid) -> BTreeMap<String, (Uuid, String, i32)> {
        sqlx::query_as::<_, (String, Uuid, String, i32)>(
            "select k.email, m.id, m.status, m.attempts from messages m
               join contacts k on k.id = m.contact_id where m.campaign_id = $1",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(e, id, s, a)| (e, (id, s, a)))
        .collect()
    }

    async fn suppress(&self, email: &str) {
        sqlx::query(
            "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason)
             values ($1, $2, 'address', $3, $4, 'complained')",
        )
        .bind(Uuid::now_v7())
        .bind(self.ws)
        .bind(email)
        .bind(suppression_hmac(&self.secrets, email).as_slice())
        .execute(&self.pool)
        .await
        .expect("suppression");
    }

    /// Every message the sink read, as text.
    fn wires(&self) -> Vec<String> {
        self.sink
            .messages()
            .into_iter()
            .map(|m| String::from_utf8(m).expect("the message is ASCII"))
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Reading what was sent.

/// The value of the first header called `name`, folded lines joined.
fn header(wire: &str, name: &str) -> Option<String> {
    let head = wire.split("\r\n\r\n").next().unwrap_or(wire);
    let head = head.replace("\r\n ", " ").replace("\r\n\t", " ");
    head.split("\r\n").find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
    })
}

/// Quoted-printable, decoded: soft breaks joined and `=XX` turned back into its byte.
fn unqp(s: &str) -> String {
    let s = s.replace("=\r\n", "");
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'=' && i + 2 < b.len())
            .then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The decoded body of the MIME part whose content type starts `mime` (`text/plain`,
/// `text/html`).
fn part(wire: &str, mime: &str) -> String {
    for chunk in wire.split("\r\n--") {
        let Some((head, body)) = chunk.split_once("\r\n\r\n") else {
            continue;
        };
        let lower = head.to_ascii_lowercase();
        if !lower.contains(&format!("content-type: {mime}")) {
            continue;
        }
        if lower.contains("content-transfer-encoding: base64") {
            let joined: String = body.split_whitespace().collect();
            return String::from_utf8_lossy(&STANDARD.decode(joined).expect("base64 part"))
                .into_owned();
        }
        if lower.contains("content-transfer-encoding: quoted-printable") {
            return unqp(body);
        }
        return body.to_owned();
    }
    panic!("no {mime} part in:\n{wire}");
}

/// The URL inside `List-Unsubscribe: <…>`.
fn unsubscribe_url(wire: &str) -> String {
    let raw = header(wire, "List-Unsubscribe").expect("a List-Unsubscribe header");
    raw.strip_prefix('<')
        .and_then(|u| u.strip_suffix('>'))
        .expect("<url>")
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// Invariants 1, 3, 4: one message each, its own link, the address, the headers.

/// **Every recipient gets one message, and it is theirs.** Twenty contacts, twenty messages, twenty
/// different `To`s. Each carries a one-click link of its own in both headers and both parts, the
/// postal address in both parts, a `Message-ID` that is its row's id on the sending domain, and the
/// campaign's `Feedback-ID`.
#[sqlx::test]
async fn every_recipient_gets_one_message_with_its_own_link_address_and_headers(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let contacts = f.contacts(20, "ada").await;
    let id = f.launched().await;
    let rows = f.messages(id).await;

    f.send_all(&f.worker(8)).await;

    let wires = f.wires();
    assert_eq!(wires.len(), 20, "one message per recipient");
    let to: BTreeSet<String> = wires.iter().map(|w| header(w, "To").expect("To")).collect();
    let want: BTreeSet<String> = contacts.iter().map(|(_, e)| e.clone()).collect();
    assert_eq!(to, want, "each recipient exactly once");
    let mut links = BTreeSet::new();
    for wire in &wires {
        let to = header(wire, "To").unwrap();
        let (message, ..) = rows[&to];
        let contact = contacts.iter().find(|(_, e)| *e == to).unwrap().0;
        let url = unsubscribe_url(wire);
        assert!(
            url.starts_with(&format!("{PUBLIC_URL}/u/")),
            "the link is not ours: {url}"
        );
        let raw = url.strip_prefix(&format!("{PUBLIC_URL}/u/")).unwrap();
        let t = token::parse(&f.secrets, raw, Purpose::Unsubscribe).expect("our token");
        assert_eq!(
            (t.workspace_id, t.contact_id, t.ref_id),
            (f.ws, contact, Some(id))
        );
        assert_eq!(t.nonce, message.to_string(), "the link names its message");
        assert!(links.insert(url.clone()), "two messages share a link");
        assert_eq!(
            header(wire, "List-Unsubscribe-Post").as_deref(),
            Some(ONE_CLICK)
        );
        assert_eq!(
            header(wire, "Message-ID"),
            Some(format!("<{}@{DOMAIN}>", message.simple()))
        );
        assert_eq!(
            header(wire, "Feedback-ID"),
            Some(format!("{id}:0:{}:nm", f.ws))
        );
        assert_eq!(
            header(wire, "From").as_deref(),
            Some(format!("Acme <news@{DOMAIN}>").as_str())
        );
        assert_eq!(header(wire, "Subject").as_deref(), Some("Hi ada"));
        let (text, html) = (part(wire, "text/plain"), part(wire, "text/html"));
        for (name, body) in [("text", &text), ("html", &html)] {
            assert!(
                body.contains(&url),
                "the {name} part lacks the header's link:\n{body}"
            );
            assert!(
                body.contains(POSTAL),
                "the {name} part lacks the postal address:\n{body}"
            );
        }
        assert!(!wire.to_ascii_lowercase().contains("mailto:"), "{wire}");
    }
    assert_eq!(
        f.statuses(id).await,
        BTreeMap::from([("sent".to_owned(), 20)])
    );
    assert_eq!(
        f.state(id).await,
        "sent",
        "every message recorded finishes it"
    );
}

/// **RFC 8058: the signature covers both unsubscribe headers.** An app-signed domain's campaign
/// message carries a DKIM signature whose `h=` names both, and `Reply-To` (spec §4.6).
#[sqlx::test]
async fn an_app_signed_campaign_message_signs_both_unsubscribe_headers(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let body: String = KEY_PEM
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(body).expect("the fixture key");
    sqlx::query(
        "update sending_domains set signing = 'app', dkim_private_key_encrypted = $1
          where id = $2",
    )
    .bind(f.secrets.encrypt(&der))
    .bind(f.domain)
    .execute(&f.pool)
    .await
    .expect("the signing key");
    f.contacts(1, "ada").await;
    f.launched().await;

    f.send_all(&f.worker(2)).await;

    let wires = f.wires();
    assert_eq!(wires.len(), 1);
    let sig = header(&wires[0], "DKIM-Signature").expect("a DKIM-Signature");
    assert!(sig.contains(&format!("d={DOMAIN}")), "{sig}");
    let h = sig
        .split(';')
        .map(str::trim)
        .find_map(|t| t.strip_prefix("h="))
        .expect("h=")
        .to_ascii_lowercase();
    let signed: BTreeSet<&str> = h.split(':').map(str::trim).collect();
    for want in [
        "list-unsubscribe",
        "list-unsubscribe-post",
        "reply-to",
        "feedback-id",
    ] {
        assert!(signed.contains(want), "h= does not cover {want}: {sig}");
    }
}

// ---------------------------------------------------------------------------------------------
// Review Focus 1: a contact unsubscribes mid-send.

/// **The link in a campaign message works, and the campaign gets the credit.** A one-click POST of
/// the exact URL the message carries suppresses the contact, and the consent row names the
/// campaign (`source_ref`) and the message whose link it was (`token_jti`): issue 43's question 7.
#[sqlx::test]
async fn the_one_click_link_in_a_campaign_message_works_and_is_attributed(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let contacts = f.contacts(1, "ada").await;
    let id = f.launched().await;
    f.send_all(&f.worker(1)).await;
    let wires = f.wires();
    assert_eq!(wires.len(), 1);
    let (message, ..) = f.messages(id).await[&contacts[0].1];

    let url = unsubscribe_url(&wires[0]);
    let path = url.strip_prefix(PUBLIC_URL).expect("our origin");
    let r = f.app.server.post(path).text(ONE_CLICK).await;
    r.assert_status_ok();

    let live: i64 = sqlx::query_scalar(
        "select count(*) from suppressions
          where workspace_id = $1 and email = $2 and lifted_at is null",
    )
    .bind(f.ws)
    .bind(&contacts[0].1)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(live, 1, "the one-click did not suppress the contact");
    let evidence: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "select source_ref, token_jti from consent_log where workspace_id = $1 and contact_id = $2",
    )
    .bind(f.ws)
    .bind(contacts[0].0)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert!(!evidence.is_empty(), "no consent row was written");
    for (source_ref, jti) in evidence {
        assert_eq!(
            source_ref,
            Some(id.to_string()),
            "not attributed to the campaign"
        );
        assert_eq!(
            jti,
            Some(message.to_string()),
            "not attributed to the message"
        );
    }
}

/// **Invariant 2.** Ada's message goes out; Bea, whose message is still queued, one-clicks a link
/// attributed to this campaign (as an earlier copy of it would carry). The next batch checks
/// eligibility once, finds Bea no longer mailable, and records her message `skipped` — with no
/// connection made for her — while Cy, untouched, is sent.
#[sqlx::test]
async fn a_one_click_mid_send_skips_the_rest_of_that_campaign(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let contacts = f.contacts(3, "p").await;
    let id = f.launched().await;
    let rows = f.messages(id).await;
    // Only the first contact is due now: the other two wait, as a later batch would.
    sqlx::query(
        "update messages set run_after = now() + interval '1 hour'
          where campaign_id = $1 and contact_id <> $2",
    )
    .bind(id)
    .bind(contacts[0].0)
    .execute(&f.pool)
    .await
    .unwrap();
    let w = f.worker(2);
    f.send_all(&w).await;
    assert_eq!(f.wires().len(), 1, "the first batch is one message");

    let (bea, bea_email) = &contacts[1];
    let link = token::issue(
        &f.secrets,
        &Token {
            workspace_id: f.ws,
            contact_id: *bea,
            purpose: Purpose::Unsubscribe,
            list_id: None,
            issued_at: Utc::now().timestamp(),
            nonce: rows[bea_email].0.to_string(),
            kid: None,
            ref_id: Some(id),
        },
    );
    f.app
        .server
        .post(&format!("/u/{link}"))
        .text(ONE_CLICK)
        .await
        .assert_status_ok();
    sqlx::query("update messages set run_after = now() where campaign_id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    f.send_all(&w).await;

    let to: BTreeSet<String> = f.wires().iter().map(|w| header(w, "To").unwrap()).collect();
    assert!(
        !to.contains(bea_email),
        "an unsubscribed contact was mailed"
    );
    assert_eq!(
        to,
        BTreeSet::from([contacts[0].1.clone(), contacts[2].1.clone()])
    );
    let after = f.messages(id).await;
    assert_eq!(after[bea_email].1, "skipped");
    assert_eq!(f.state(id).await, "sent");
}

// ---------------------------------------------------------------------------------------------
// Review Focus 4 and invariant 1: never twice.

/// **Two workers, two hundred messages, two hundred deliveries.** Both tick at once, against one
/// queue, until it is empty: every address once, no address twice.
#[sqlx::test]
async fn two_workers_send_each_message_once(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    f.contacts(200, "q").await;
    let id = f.launched().await;
    let (a, b) = (f.worker(16), f.worker(16));
    let started = Instant::now();
    while f.state(id).await == "sending" && started.elapsed() < Duration::from_secs(30) {
        a.1.refresh(&f.pool).await.unwrap();
        b.1.refresh(&f.pool).await.unwrap();
        let (x, y) = tokio::join!(a.0.tick(), b.0.tick());
        if x.unwrap() + y.unwrap() == 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    // `sent` means every message is final, so nothing is left to claim. A claim that lost its row
    // mid-session is `a_stale_claim_never_sends`'s.
    for w in [&a, &b] {
        assert_eq!(f.tick(w).await, 0, "a finished campaign still had work");
    }

    let to: Vec<String> = f.wires().iter().map(|w| header(w, "To").unwrap()).collect();
    let distinct: BTreeSet<&String> = to.iter().collect();
    assert_eq!(distinct.len(), 200, "every recipient once");
    assert_eq!(to.len(), 200, "and nobody twice");
    assert_eq!(
        f.statuses(id).await,
        BTreeMap::from([("sent".to_owned(), 200)])
    );
}

/// Waits until the sink has read `n` messages to their end.
async fn reached(sink: &Sink, n: usize, what: &str) {
    let started = Instant::now();
    while sink.delivered() < n {
        assert!(started.elapsed() < Duration::from_secs(20), "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// **Review focus 4: a claim that lost its row never sends it.** Worker A claims both messages and,
/// at concurrency one, is mid-session on the first: the relay holds its reply until the test lets
/// it go. A's claim on the second lapses meanwhile — a pool wait, a suspended VM — and worker B
/// re-claims it and has it at the relay too. Only then are the replies let go. When A reaches that
/// message its write-ahead finds the row is no longer its claim's, and A drops it without a word:
/// every address once. Ordering, not time: A cannot move past its first message until the test
/// releases it, so a test that stalls anywhere still sets the scene it describes.
#[sqlx::test]
async fn a_stale_claim_never_sends(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Held("250 2.0.0 Ok"))).await;
    f.contacts(2, "w").await;
    let id = f.launched().await;
    let a = f.worker(1);
    let a_states = Arc::clone(&a.1);
    a.1.refresh(&f.pool).await.unwrap();
    let first = tokio::spawn(async move { a.0.tick().await.map(|n| (n, a)) });
    reached(&f.sink, 1, "A sent nothing").await;

    // A's lease on the message it has not started runs out, and B takes it.
    let lapsed = sqlx::query(
        "update messages set run_after = now() where campaign_id = $1 and status = 'queued'",
    )
    .bind(id)
    .execute(&f.pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(
        lapsed, 1,
        "the premise: A holds one message it has not started"
    );
    let b = f.worker(1);
    b.1.refresh(&f.pool).await.unwrap();
    let second = tokio::spawn(async move { b.0.tick().await });
    reached(&f.sink, 2, "B did not send the lapsed message").await;
    // A's map is read again just before the release, so a slow test cannot leave A to read
    // "not sending" and stop short of the write-ahead this is about.
    a_states.refresh(&f.pool).await.unwrap();
    f.sink.release();
    assert_eq!(
        second.await.unwrap().expect("B's tick"),
        1,
        "B re-claimed the lapsed message"
    );
    let (claimed, _a) = first.await.unwrap().expect("A's tick");
    assert_eq!(claimed, 2, "the premise: A had claimed both");

    let to: Vec<String> = f.wires().iter().map(|w| header(w, "To").unwrap()).collect();
    let distinct: BTreeSet<&String> = to.iter().collect();
    assert_eq!(distinct.len(), 2, "every recipient once");
    assert_eq!(
        to.len(),
        2,
        "a stale claim sent a message another worker had sent: {to:?}"
    );
    assert_eq!(
        f.statuses(id).await,
        BTreeMap::from([("sent".to_owned(), 2)])
    );
}

/// **Invariant 6, review focus 3.** The first relay reads the whole message and drops the
/// connection: it may have queued it. Never retried, and never offered to the healthy second
/// relay — which must see no connection at all.
#[sqlx::test]
async fn an_unknown_outcome_is_never_retried_or_failed_over(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Drop)).await;
    let healthy = Sink::start(Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    f.add_relay(Uuid::now_v7(), "standby", healthy.addr, 1)
        .await;
    let contacts = f.contacts(1, "u").await;
    let id = f.launched().await;
    let w = f.worker(1);

    f.send_all(&w).await;
    f.tick(&w).await;

    assert_eq!(f.sink.messages().len(), 1, "the first relay read it once");
    assert_eq!(
        healthy.connections(),
        0,
        "an ambiguous message was offered to a second relay"
    );
    let (_, status, attempts) = f.messages(id).await[&contacts[0].1].clone();
    assert_eq!((status.as_str(), attempts), ("unknown", 1));
    assert_eq!(f.state(id).await, "sent", "unknown is final");
}

/// **Review focus 3.** Relay 1 refuses the connection: nothing was submitted, so the message goes
/// to relay 2 in the same send — after taking a token from relay 2's bucket, since the tick paid
/// relay 1 — and relay 1 is drained for its cool-down.
#[sqlx::test]
async fn a_refused_connection_fails_over_and_takes_a_token_from_the_second_relay(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    // The fixture's relay (the sink) becomes the standby; a dead address goes first.
    sqlx::query("update relays set failover_order = 1 where id = $1")
        .bind(f.relay)
        .execute(&f.pool)
        .await
        .unwrap();
    let dead = Uuid::now_v7();
    f.add_relay(dead, "dead", Sink::unused_addr().await, 0)
        .await;
    f.contacts(1, "r").await;
    let id = f.launched().await;

    f.send_all(&f.worker(1)).await;

    assert_eq!(f.sink.messages().len(), 1, "the standby delivered it");
    let (status, relay): (String, Option<Uuid>) =
        sqlx::query_as("select status, relay_id from messages where campaign_id = $1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), relay), ("sent", Some(f.relay)));
    let bucket = |relay: Uuid| {
        let pool = f.pool.clone();
        async move {
            sqlx::query_as::<_, (i32, bool)>(
                "select tokens, refilled_at > now() + interval '55 seconds'
                   from relay_buckets where relay_id = $1",
            )
            .bind(relay)
            .fetch_one(&pool)
            .await
            .expect("a bucket")
        }
    };
    assert_eq!(
        bucket(f.relay).await,
        (599, false),
        "the standby's own token was taken"
    );
    assert_eq!(bucket(dead).await, (0, true), "the dead relay was drained");
}

/// **Review focus 3: a `455` is the relay's limit, not the message's.** Released, with the attempt
/// the claim spent given back, and the relay's bucket drained for a minute so neither this worker
/// nor another offers it more meanwhile.
#[sqlx::test]
async fn a_455_releases_with_the_attempt_intact_and_drains_the_bucket(pool: PgPool) {
    let f = fx(
        &pool,
        Script::Smtp(AfterData::Reply("455 4.7.1 Server busy, try again later")),
    )
    .await;
    let contacts = f.contacts(1, "t").await;
    let id = f.launched().await;
    let w = f.worker(1);

    assert_eq!(f.tick(&w).await, 1);

    let (_, status, attempts) = f.messages(id).await[&contacts[0].1].clone();
    assert_eq!(
        (status.as_str(), attempts),
        ("queued", 0),
        "a 455 spent an attempt"
    );
    let (tokens, held): (i32, bool) = sqlx::query_as(
        "select tokens, refilled_at >= now() + interval '55 seconds'
           from relay_buckets where relay_id = $1",
    )
    .bind(f.relay)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!((tokens, held), (0, true), "the bucket was not drained");
    // Due again, but the bucket is shut: nothing is offered to the relay.
    sqlx::query("update messages set run_after = now() where campaign_id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    f.tick(&w).await;
    assert_eq!(
        f.sink.messages().len(),
        1,
        "a drained relay was offered more"
    );
    assert_eq!(f.messages(id).await[&contacts[0].1].2, 0);
}

/// **A 4xx about the message is not forgiven by a full relay after it.** Relay 1 answers `451`
/// about this message; relay 2 answers `455`, its own limit. The message was refused once on its own
/// account, so it spends the attempt (`Retry`), even though the last answer was relay-level.
#[sqlx::test]
async fn a_4xx_then_a_455_spends_the_attempt(pool: PgPool) {
    let f = fx(
        &pool,
        Script::Smtp(AfterData::Reply("451 4.3.0 try again later")),
    )
    .await;
    let full = Sink::start(Script::Smtp(AfterData::Reply("455 4.7.1 Server busy"))).await;
    f.add_relay(Uuid::now_v7(), "full", full.addr, 1).await;
    let contacts = f.contacts(1, "v").await;
    let id = f.launched().await;

    assert_eq!(f.tick(&f.worker(1)).await, 1);

    assert_eq!(full.messages().len(), 1, "the premise: relay 2 was tried");
    let (_, status, attempts) = f.messages(id).await[&contacts[0].1].clone();
    assert_eq!(
        (status.as_str(), attempts),
        ("queued", 1),
        "a message refused on its own account got its attempt back"
    );
}

// ---------------------------------------------------------------------------------------------
// Invariant 5: pause.

/// **Pause stops new submissions** (invariant 5). A slow relay, one send at a time, twenty queued.
/// While the tick is mid-batch an editor pauses, and the worker's state map is read again: from
/// that read on, nothing is started. At most the one send that had already passed its check when
/// the map was read may still reach the relay; every other message stays queued with its attempt
/// given back. Ordering, not time. That the map is read often enough for this to happen within
/// 5 s of the pause is `STATE_REFRESH` and `STALE_AFTER`, pinned at compile time in
/// `campaign::send`.
#[sqlx::test]
async fn pause_stops_new_submissions_within_five_seconds(pool: PgPool) {
    const CONCURRENCY: usize = 1;
    let f = fx(
        &pool,
        Script::Smtp(AfterData::Delayed(
            Duration::from_millis(300),
            "250 2.0.0 Ok",
        )),
    )
    .await;
    f.contacts(20, "s").await;
    let id = f.launched().await;
    let (worker, states) = f.worker(CONCURRENCY);
    states.refresh(&f.pool).await.unwrap();
    let tick = tokio::spawn(async move { worker.tick().await });
    let started = Instant::now();
    while f.sink.messages().len() < 2 {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "nothing was sent"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    f.app
        .post(&format!("/api/v1/workspaces/{}/campaigns/{id}/pause", f.ws))
        .await
        .assert_status_ok();
    states.refresh(&f.pool).await.unwrap();
    let at_refresh = f.sink.messages().len();
    assert_eq!(
        tick.await.unwrap().unwrap(),
        20,
        "the premise: one tick held them all"
    );

    let sent = f.sink.messages().len();
    assert!(
        sent <= at_refresh + CONCURRENCY,
        "{sent} reached the relay, {at_refresh} of them before the map said paused: a message \
         was started after the pause"
    );
    assert!(sent < 20, "the pause stopped nothing");
    let rows = f.messages(id).await;
    let queued: Vec<i32> = rows
        .values()
        .filter(|(_, status, _)| status == "queued")
        .map(|(_, _, attempts)| *attempts)
        .collect();
    assert_eq!(
        queued.len(),
        20 - sent,
        "what was not sent is not queued: {rows:?}"
    );
    assert!(
        queued.iter().all(|a| *a == 0),
        "a held message spent an attempt: {rows:?}"
    );
    assert_eq!(f.state(id).await, "paused");
}

// ---------------------------------------------------------------------------------------------
// Spec §4.7.10: what an operator can fix pauses the campaign rather than failing it.

/// The sending domain is deleted mid-send. Nothing is sent, the campaign is paused with a reason
/// that names the domain, and every message stays queued with its attempt given back.
#[sqlx::test]
async fn a_deleted_sending_domain_pauses_the_campaign_with_a_reason(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    f.contacts(3, "d").await;
    let id = f.launched().await;
    sqlx::query("delete from sending_domains where id = $1")
        .bind(f.domain)
        .execute(&f.pool)
        .await
        .unwrap();

    f.tick(&f.worker(2)).await;

    assert_eq!(f.sink.connections(), 0, "a message went out with no domain");
    assert_eq!(f.state(id).await, "paused");
    let reason = f.pause_reason(id).await.expect("a pause reason");
    assert!(
        reason.contains(DOMAIN) && reason.contains("deleted"),
        "{reason}"
    );
    for (_, status, attempts) in f.messages(id).await.values() {
        assert_eq!((status.as_str(), *attempts), ("queued", 0));
    }
}

/// A custom field the template names changes type mid-send. The frozen rendition no longer passes
/// `ready`, so the campaign pauses with the drift named and sends nothing. Once the template is
/// re-saved, a resume re-freezes it, and the rest goes out from the new rendition.
#[sqlx::test]
async fn field_drift_mid_send_pauses_and_a_resume_after_a_resave_refreezes(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query(
        "insert into custom_fields (id, workspace_id, key, label, kind)
         values ($1, $2, 'plan', 'Plan', 'text')",
    )
    .bind(Uuid::now_v7())
    .bind(f.ws)
    .execute(&f.pool)
    .await
    .unwrap();
    let tree = json!({"tree": {"blocks": [
        {"type": "Text", "runs": [{"type": "Tag", "source": "{{ plan }}"}]}]}});
    let template = format!("/api/v1/workspaces/{}/templates/{}", f.ws, f.template);
    f.app.patch(&template).json(&tree).await.assert_status_ok();
    f.contacts(2, "e").await;
    let id = f.launched().await;
    let frozen_at = || async {
        sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            "select frozen_at from campaigns where id = $1",
        )
        .bind(id)
        .fetch_one(&f.pool)
        .await
        .unwrap()
    };
    let first = frozen_at().await;
    sqlx::query(
        "update custom_fields set kind = 'number' where key = 'plan' and workspace_id = $1",
    )
    .bind(f.ws)
    .execute(&f.pool)
    .await
    .unwrap();
    let w = f.worker(2);

    f.tick(&w).await;

    assert_eq!(f.sink.connections(), 0, "a drifted rendition was sent");
    assert_eq!(f.state(id).await, "paused");
    let reason = f.pause_reason(id).await.expect("a pause reason");
    assert!(reason.contains("plan"), "{reason}");

    f.app.patch(&template).json(&tree).await.assert_status_ok();
    f.app
        .post(&format!(
            "/api/v1/workspaces/{}/campaigns/{id}/resume",
            f.ws
        ))
        .await
        .assert_status_ok();
    assert_ne!(frozen_at().await, first, "the resume did not re-freeze");
    f.send_all(&w).await;

    assert_eq!(f.wires().len(), 2, "the rest went out after the resume");
    assert_eq!(f.state(id).await, "sent");
}

/// **A resume that lands between two refreshes of the state map sends from the new rendition.**
/// The worker has the launch's rendition cached. The field it names changes type, the template is
/// re-saved, and the campaign is paused and resumed — which re-freezes it — all before the map is
/// read again, so the map still says "sending, frozen at launch". The next tick must read which
/// rendition is current from the database: keyed on the map's copy, it would pick the old one, find
/// it drifted, and pause the campaign the operator just resumed.
#[sqlx::test]
async fn a_resume_between_two_refreshes_uses_the_new_rendition(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    sqlx::query(
        "insert into custom_fields (id, workspace_id, key, label, kind)
         values ($1, $2, 'plan', 'Plan', 'text')",
    )
    .bind(Uuid::now_v7())
    .bind(f.ws)
    .execute(&f.pool)
    .await
    .unwrap();
    let tree = json!({"tree": {"blocks": [
        {"type": "Text", "runs": [{"type": "Tag", "source": "{{ plan }}"}]}]}});
    let template = format!("/api/v1/workspaces/{}/templates/{}", f.ws, f.template);
    f.app.patch(&template).json(&tree).await.assert_status_ok();
    let contacts = f.contacts(2, "g").await;
    let id = f.launched().await;
    // One message now, so the worker caches the launch's rendition; the other waits.
    let (pool2, other) = (f.pool.clone(), contacts[1].0);
    let later = move |hold: bool| {
        let pool = pool2.clone();
        async move {
            sqlx::query(
                "update messages
                    set run_after = case when $3 then now() + interval '1 hour' else now() end
                  where campaign_id = $1 and contact_id = $2",
            )
            .bind(id)
            .bind(other)
            .bind(hold)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    later(true).await;
    let w = f.worker(1);
    assert_eq!(f.tick(&w).await, 1);
    assert_eq!(f.wires().len(), 1);

    sqlx::query(
        "update custom_fields set kind = 'number' where key = 'plan' and workspace_id = $1",
    )
    .bind(f.ws)
    .execute(&f.pool)
    .await
    .unwrap();
    f.app.patch(&template).json(&tree).await.assert_status_ok();
    // The last read of the map: the campaign is sending, frozen at launch.
    w.1.refresh(&f.pool).await.unwrap();
    f.app
        .post(&format!("/api/v1/workspaces/{}/campaigns/{id}/pause", f.ws))
        .await
        .assert_status_ok();
    f.app
        .post(&format!(
            "/api/v1/workspaces/{}/campaigns/{id}/resume",
            f.ws
        ))
        .await
        .assert_status_ok();
    later(false).await;

    assert_eq!(
        w.0.tick().await.unwrap(),
        1,
        "the premise: the stale map still lists it"
    );

    assert_eq!(
        f.pause_reason(id).await,
        None,
        "a resumed campaign was paused again"
    );
    assert_eq!(f.wires().len(), 2, "the rest did not go out");
    assert_eq!(f.state(id).await, "sent");
}

/// **A domain that stops passing its DNS checks pauses the campaign** (spec §4.4, §4.7.10: the gate
/// is read per send batch). Nothing is sent, the reason names the domain and the check, and every
/// message stays queued with its attempt given back.
#[sqlx::test]
async fn a_domain_that_fails_the_gate_mid_send_pauses_the_campaign(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    f.contacts(3, "h").await;
    let id = f.launched().await;
    sqlx::query("update sending_domains set checks = $2 where id = $1")
        .bind(f.domain)
        .bind(
            json!({"spf": {"status": "fail", "reason": "no SPF record was published"},
                     "dkim": {"status": "pass"}, "dmarc": {"status": "pass"},
                     "return_path": {"status": "pass"}}),
        )
        .execute(&f.pool)
        .await
        .unwrap();

    f.tick(&f.worker(2)).await;

    assert_eq!(f.sink.connections(), 0, "mail left an unverified domain");
    assert_eq!(f.state(id).await, "paused");
    let reason = f.pause_reason(id).await.expect("a pause reason");
    assert!(
        reason.contains(DOMAIN) && reason.contains("SPF"),
        "{reason}"
    );
    for (_, status, attempts) in f.messages(id).await.values() {
        assert_eq!((status.as_str(), *attempts), ("queued", 0));
    }
}

// ---------------------------------------------------------------------------------------------
// Spec §4.8: test sends.

/// **A test is delivered as a test, and honours suppressions.** Two addresses: one goes out with
/// `[Test] ` on the subject, a `test:` Feedback-ID and the `/u/test` link; the other is suppressed,
/// is never dialled, and its job is failed with the reason — the API already answered `202`, so
/// the job is where the refusal has to be visible.
#[sqlx::test]
async fn a_test_send_job_is_delivered_as_a_test_and_respects_suppression(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let r = f
        .app
        .post(&format!("/api/v1/workspaces/{}/campaigns", f.ws))
        .json(&json!({
            "name": "c", "list_ids": [f.list], "sending_domain_id": f.domain,
            "from_local_part": "news", "from_name": "Acme", "reply_to": null,
            "variants": [{"subject": "Hi {{ first_name }}", "template_id": f.template,
                          "from_name": null}],
        }))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = id_of(&r);
    f.suppress("blocked@dest.test").await;
    f.app
        .post(&format!("/api/v1/workspaces/{}/campaigns/{id}/test", f.ws))
        .json(&json!({"addresses": ["tester@dest.test", "blocked@dest.test"]}))
        .await
        .assert_status(StatusCode::ACCEPTED);

    Worker::new(f.ctx(), 2).tick().await.expect("a tick");

    let wires = f.wires();
    assert_eq!(wires.len(), 1, "only the unsuppressed address was mailed");
    let wire = &wires[0];
    assert_eq!(header(wire, "To").as_deref(), Some("tester@dest.test"));
    assert!(
        header(wire, "Subject").unwrap().starts_with("[Test] "),
        "{wire}"
    );
    assert_eq!(
        header(wire, "Feedback-ID"),
        Some(format!("test:{id}:{}:nm", f.ws))
    );
    assert_eq!(unsubscribe_url(wire), format!("{PUBLIC_URL}/u/test"));
    let jobs: Vec<(String, bool, bool, Option<String>)> = sqlx::query_as(
        "select payload->>'to', completed_at is not null, failed_at is not null, last_error
           from jobs where kind = 'campaign_test' order by payload->>'to'",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    let (to, completed, failed, why) = &jobs[0];
    assert_eq!(to, "blocked@dest.test");
    assert!(!completed && *failed, "the suppressed test was not failed");
    assert!(
        why.as_deref().unwrap_or_default().contains("suppressed"),
        "no reason: {why:?}"
    );
    assert!(jobs[1].1, "the delivered test was not completed");
}

// ---------------------------------------------------------------------------------------------
// Spec §4.2 ruling 5: erasure leaves only a uuid. `messages` has no foreign key to `contacts`, so
// erasure never reaches a message row; a relay reply that quotes the recipient must not be stored
// with the address in it to begin with.

/// Fails if `text` holds `address` in any case.
#[track_caller]
fn holds_no_address(text: &str, address: &str) {
    assert!(
        !text
            .to_ascii_lowercase()
            .contains(&address.to_ascii_lowercase()),
        "the address was stored: {text}"
    );
}

/// `(status, response, error)` of the one message sent to `quoted-1@dest.test` through a relay
/// that answers `reply`.
async fn stored_reply(
    pool: &PgPool,
    reply: &'static str,
) -> (String, Option<String>, Option<String>) {
    let f = fx(pool, Script::Smtp(AfterData::Reply(reply))).await;
    f.contacts(1, "quoted").await;
    let id = f.launched().await;
    f.send_all(&f.worker(1)).await;
    assert_eq!(f.sink.delivered(), 1, "the premise: the relay read it");
    sqlx::query_as("select status, response, error from messages where campaign_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A refusal quoting the recipient in brackets and bare, in a case of its own (a relay echoes what
/// it likes): `messages.error` keeps the reply with `<recipient>` in both places.
#[sqlx::test]
async fn a_refusal_that_quotes_the_recipient_is_stored_without_the_address(pool: PgPool) {
    let (status, _, error) = stored_reply(
        &pool,
        "550 5.1.1 <Quoted-1@Dest.Test>: Recipient address rejected (quoted-1@dest.test)",
    )
    .await;
    let error = error.expect("the refusal is on the row");
    assert_eq!(status, "failed");
    holds_no_address(&error, "quoted-1@dest.test");
    assert!(
        error.contains("<recipient>: Recipient address rejected (<recipient>)"),
        "{error}"
    );
}

/// An acceptance that echoes the recipient: `messages.response` keeps it without the address.
#[sqlx::test]
async fn an_acceptance_that_quotes_the_recipient_is_stored_without_the_address(pool: PgPool) {
    let (status, response, _) =
        stored_reply(&pool, "250 2.0.0 Ok: queued for <QUOTED-1@dest.test>").await;
    let response = response.expect("the acceptance is on the row");
    assert_eq!(status, "sent");
    holds_no_address(&response, "quoted-1@dest.test");
    assert!(response.contains("queued for <recipient>"), "{response}");
}

/// A test send's refusal is kept on its job's `last_error`, which is shown: the same redaction.
#[sqlx::test]
async fn a_test_send_refusal_that_quotes_the_recipient_is_stored_without_the_address(pool: PgPool) {
    let f = fx(
        &pool,
        Script::Smtp(AfterData::Reply(
            "550 5.1.1 <Tester@Dest.Test>: Recipient address rejected (tester@dest.test)",
        )),
    )
    .await;
    let r = f
        .app
        .post(&format!("/api/v1/workspaces/{}/campaigns", f.ws))
        .json(&json!({
            "name": "c", "list_ids": [f.list], "sending_domain_id": f.domain,
            "from_local_part": "news", "from_name": "Acme", "reply_to": null,
            "variants": [{"subject": "Hi {{ first_name }}", "template_id": f.template,
                          "from_name": null}],
        }))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = id_of(&r);
    f.app
        .post(&format!("/api/v1/workspaces/{}/campaigns/{id}/test", f.ws))
        .json(&json!({"addresses": ["tester@dest.test"]}))
        .await
        .assert_status(StatusCode::ACCEPTED);

    Worker::new(f.ctx(), 2).tick().await.expect("a tick");

    assert_eq!(f.sink.delivered(), 1, "the premise: the relay read it");
    let why: Option<String> =
        sqlx::query_scalar("select last_error from jobs where kind = 'campaign_test'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let why = why.expect("the refusal is on the job");
    holds_no_address(&why, "tester@dest.test");
    assert!(
        why.contains("<recipient>: Recipient address rejected (<recipient>)"),
        "{why}"
    );
}

// ---------------------------------------------------------------------------------------------
// Spec §4.7.8: the two loops.

/// **A double opt-in email does not wait behind a campaign** (spec §4.7.8). The worker's own loops
/// run, as `worker::run` starts them, while a campaign goes out through a slow relay. The messages
/// loop is wedged from the start: the test holds the campaign's row before the loops run, so the
/// outcome flush (and with it the tick) cannot finish, and the campaign is still sending however
/// slowly the test reaches each step. A confirmation queued once the campaign is going out is
/// delivered all the same, because the jobs loop is a loop of its own and never waits on the
/// messages loop. The bound is far above the real latency (about a second): it can only be missed
/// when the two loops are sequenced.
#[sqlx::test]
async fn a_confirmation_is_not_held_behind_a_campaign(pool: PgPool) {
    let f = fx(
        &pool,
        Script::Smtp(AfterData::Delayed(
            Duration::from_millis(300),
            "250 2.0.0 Ok",
        )),
    )
    .await;
    sqlx::query("update relays set rate_limit_per_minute = 100000 where id = $1")
        .bind(f.relay)
        .execute(&f.pool)
        .await
        .unwrap();
    f.contacts(200, "k").await;
    let id = f.launched().await;

    // The wedge, before the loops start: the flush's counter update needs this row, so the
    // messages tick cannot end and the campaign cannot finish. Claims, write-aheads and sends
    // read the row without locking it and go on.
    let mut wedge = f.pool.begin().await.unwrap();
    let sent_before: i64 =
        sqlx::query_scalar("select sent from campaigns where id = $1 for update")
            .bind(id)
            .fetch_one(&mut *wedge)
            .await
            .unwrap();

    // The worker's own composition, as `worker::run` starts it.
    let (stop, mut stopped) = tokio::sync::watch::channel(());
    let loops = tokio::spawn(worker::run_loops(f.ctx(), 4, 32, async move {
        let _ = stopped.changed().await;
    }));
    reached(&f.sink, 10, "the campaign never started").await;

    let contact: Uuid = sqlx::query_scalar(
        "insert into contacts (id, workspace_id, email) values (gen_random_uuid(), $1,
                'new@dest.test') returning id",
    )
    .bind(f.ws)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let confirm = token::issue(
        &f.secrets,
        &Token {
            workspace_id: f.ws,
            contact_id: contact,
            purpose: Purpose::Confirm,
            list_id: Some(f.list),
            issued_at: Utc::now().timestamp(),
            nonce: Uuid::now_v7().to_string(),
            kid: None,
            ref_id: None,
        },
    );
    let job = Uuid::now_v7();
    sqlx::query(
        "insert into jobs (id, workspace_id, kind, contact_id, list_id, payload)
         values ($1, $2, 'confirmation_email', $3, $4, $5)",
    )
    .bind(job)
    .bind(f.ws)
    .bind(contact)
    .bind(f.list)
    .bind(json!({"email": "new@dest.test", "token": confirm}))
    .execute(&f.pool)
    .await
    .unwrap();
    let queued = Instant::now();
    let mut done = false;
    while queued.elapsed() < Duration::from_secs(30) {
        let completed: Option<DateTime<Utc>> =
            sqlx::query_scalar("select completed_at from jobs where id = $1")
                .bind(job)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        if completed.is_some() {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (state, sent_after): (String, i64) =
        sqlx::query_as("select state, sent from campaigns where id = $1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    wedge.rollback().await.unwrap();
    let _ = stop.send(());
    loops.await.unwrap();

    assert!(
        state == "sending" && sent_after == sent_before,
        "the messages loop was not held, so this proves nothing: {state}, {sent_before} -> \
         {sent_after}"
    );
    assert!(
        done,
        "the confirmation waited 30 s behind a held campaign: the loops are sequenced"
    );
}

/// **A resume finishes a campaign that has nothing left** (spec §4.1, `sending -> sent`). The last
/// messages in flight at a pause are recorded while it is paused, and the flush's finish check
/// moves only a `sending` campaign; with nothing left to flush, only the resume can finish it.
#[sqlx::test]
async fn a_resume_of_a_campaign_with_nothing_left_finishes_it(pool: PgPool) {
    let f = fx(&pool, Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    f.contacts(1, "z").await;
    let id = f.launched().await;
    f.app
        .post(&format!("/api/v1/workspaces/{}/campaigns/{id}/pause", f.ws))
        .await
        .assert_status_ok();
    // What the flush of the one message in flight at the pause leaves.
    sqlx::query("update messages set status = 'sent', sent_at = now() where campaign_id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("update campaigns set sent = 1 where id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();

    let r = f
        .app
        .post(&format!(
            "/api/v1/workspaces/{}/campaigns/{id}/resume",
            f.ws
        ))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["state"], "sent");
    assert_eq!(f.state(id).await, "sent");
}
