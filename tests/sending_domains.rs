// SPDX-License-Identifier: AGPL-3.0-only

//! The sending-domain API and the DNS checker behind it. The guarantee the API tests pin is that a
//! domain's DKIM private key reaches no response, no audit row and no error; the checker tests
//! drive the real resolver at a real (in-process) nameserver, so what is under test is the whole
//! path from the route to a verdict on a stored row, not the parsing alone. The parsing is
//! unit-tested in `src/sending_domains/dns.rs`.

mod common;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use common::{PW, TestApp, add_member, create_user, create_workspace};
use hickory_resolver::{
    config::{NameServerConfig, ResolverConfig},
    proto::{
        op::{Message, OpCode, ResponseCode},
        rr::{
            Name, RData, Record, RecordType,
            rdata::{CNAME, TXT},
        },
    },
};
use nm::{sending_domains::dns, workspaces::roles::Role};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::UdpSocket;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// A nameserver of our own.

/// What the nameserver knows: `(name, type)` to the records there. Anything else is `NXDOMAIN`.
#[derive(Clone, Default)]
struct Zone(Arc<Mutex<Records>>);

type Records = HashMap<(String, RecordType), Vec<RData>>;

impl Zone {
    /// A TXT record, in the character-strings a nameserver would hold it as: one record, split
    /// at 255 bytes, which is where DNS makes a 2048-bit DKIM key go. The checker joins them
    /// back without a separator, as SPF and DKIM both say to.
    fn txt(&self, name: &str, text: &str) -> &Zone {
        let strings = text
            .as_bytes()
            .chunks(255)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();
        self.add(name, RecordType::TXT, RData::TXT(TXT::new(strings)))
    }

    fn cname(&self, name: &str, target: &str) -> &Zone {
        let target = Name::from_ascii(format!("{target}.")).unwrap();
        self.add(name, RecordType::CNAME, RData::CNAME(CNAME(target)))
    }

    fn add(&self, name: &str, ty: RecordType, data: RData) -> &Zone {
        let mut zone = self.0.lock().unwrap();
        zone.entry((name.to_owned(), ty)).or_default().push(data);
        self
    }

    fn clear(&self, name: &str, ty: RecordType) {
        self.0.lock().unwrap().remove(&(name.to_owned(), ty));
    }
}

struct Nameserver {
    addr: SocketAddr,
    /// Every query that has arrived, answered or not.
    queries: Arc<AtomicUsize>,
    /// The server answers only while nobody holds this. A test that takes it makes the nameserver
    /// hang, with the queries it has already been sent sitting unanswered.
    hold: Arc<tokio::sync::Mutex<()>>,
}

async fn nameserver(zone: Zone) -> Nameserver {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let queries = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(tokio::sync::Mutex::new(()));
    let (served, gate, answers) = (queries.clone(), hold.clone(), zone.clone());
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
                return;
            };
            let Ok(query) = Message::from_vec(&buf[..n]) else {
                continue;
            };
            served.fetch_add(1, Ordering::SeqCst);
            drop(gate.lock().await);
            let mut reply = Message::response(query.metadata.id, OpCode::Query);
            reply.metadata.recursion_desired = true;
            reply.metadata.recursion_available = true;
            for q in &query.queries {
                reply.add_query(q.clone());
                let name = q
                    .name()
                    .to_ascii()
                    .trim_end_matches('.')
                    .to_ascii_lowercase();
                let found = answers
                    .0
                    .lock()
                    .unwrap()
                    .get(&(name, q.query_type()))
                    .cloned();
                match found {
                    Some(records) => {
                        for data in records {
                            reply.add_answer(Record::from_rdata(q.name().clone(), 60, data));
                        }
                    }
                    None => reply.metadata.response_code = ResponseCode::NXDomain,
                }
            }
            let _ = socket.send_to(&reply.to_vec().unwrap(), peer).await;
        }
    });
    Nameserver {
        addr,
        queries,
        hold,
    }
}

/// The production resolver, aimed at `ns`. The per-attempt timeout is long, so a held query is
/// never retried inside a test.
fn resolver_for(ns: &Nameserver) -> dns::Resolver {
    let mut server = NameServerConfig::udp(ns.addr.ip());
    server.connections[0].port = ns.addr.port();
    dns::resolver(
        ResolverConfig::from_name_servers(vec![server]),
        Duration::from_secs(10),
    )
    .unwrap()
}

/// The records a correctly set-up `example.com` publishes, with a stand-in key.
fn complete_zone() -> Zone {
    let zone = Zone::default();
    zone.txt("example.com", "v=spf1 include:relay.example.net -all")
        .txt(
            "nm1._domainkey.example.com",
            "v=DKIM1; k=rsa; p=QUJDREVGR0hJ",
        )
        .txt("_dmarc.example.com", "v=DMARC1; p=none")
        .cname("bounce.example.com", "relay.example.net");
    zone
}

// ---------------------------------------------------------------------------------------------
// The API.

/// The PEM an operator pastes. A real 2048-bit RSA key, because the route now hands the bytes to
/// the signer before it stores them: a stand-in would be refused. Throwaway, and the fixture
/// directory's README says so.
const KEY_PEM: &str = include_str!("fixtures/dkim/signing-2048-pkcs8.pem");
/// The same key in the other armour. The stored DER does not record which it was, so a key saved
/// either way has to come back out signable.
const KEY_PEM_PKCS1: &str = include_str!("fixtures/dkim/signing-2048-pkcs1.pem");
/// A different key, for the selector that publishes the wrong one.
const OTHER_PEM: &str = include_str!("fixtures/dkim/other-2048-pkcs8.pem");
const TOO_SMALL_PEM: &str = include_str!("fixtures/dkim/too-small-1024-pkcs1.pem");

/// The DER a PEM carries: what the column holds once the armour is off.
fn der(pem: &str) -> Vec<u8> {
    STANDARD
        .decode(
            pem.trim()
                .lines()
                .filter(|l| !l.starts_with("-----"))
                .map(str::trim)
                .collect::<String>(),
        )
        .unwrap()
}

/// The base64 `p=` the key signs with, as the DKIM record publishes it.
fn published_p(pem: &str) -> String {
    nm::mail::dkim::public_key_p(&nm::mail::dkim::key_from_pem(pem).unwrap())
}

/// Every form the key takes on its way through: the whole base64 of the DER, and one line of the
/// PEM body. That line is a slice of the same base64, so it catches an echo of the PEM a request
/// carried as well as a re-encoding of the stored bytes.
fn key_forms() -> Vec<String> {
    vec![
        STANDARD.encode(der(KEY_PEM)),
        KEY_PEM.lines().nth(2).unwrap().trim().to_owned(),
    ]
}

struct Tenant {
    id: Uuid,
    admin: TestApp,
    viewer: TestApp,
}

async fn tenant(pool: &PgPool, slug: &str) -> Tenant {
    tenant_with(pool, slug, TestApp::new).await
}

async fn tenant_with<F, Fut>(pool: &PgPool, slug: &str, make: F) -> Tenant
where
    F: Fn(PgPool) -> Fut + Copy,
    Fut: std::future::Future<Output = TestApp>,
{
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let id = create_workspace(pool, owner, slug).await;
    let member = |role: Role| {
        let email = format!("{role:?}-{slug}@example.com").to_lowercase();
        async move {
            let user = create_user(pool, &email, PW, false).await;
            add_member(pool, id, user, role).await;
            let mut app = make(pool.clone()).await;
            app.login(&email, PW).await;
            app
        }
    };
    Tenant {
        id,
        admin: member(Role::Admin).await,
        viewer: member(Role::Viewer).await,
    }
}

fn domains_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/sending-domains")
}

fn domain_path(ws: Uuid, id: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/sending-domains/{id}")
}

fn check_path(ws: Uuid, id: Uuid) -> String {
    format!("{}/check", domain_path(ws, id))
}

fn body(domain: &str, signing: &str) -> Value {
    let mut v = json!({"domain": domain, "signing": signing});
    if signing == "app" {
        v["dkim_private_key"] = json!(KEY_PEM);
    }
    v
}

async fn add_domain(t: &Tenant, v: Value) -> Uuid {
    let r = t.admin.post(&domains_path(t.id)).json(&v).await;
    r.assert_status(StatusCode::CREATED);
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn stored_key(t: &Tenant, id: Uuid) -> Option<Vec<u8>> {
    nm::sending_domains::dkim_private_key(&t.admin.pool, &t.admin.state.secrets, t.id, id)
        .await
        .unwrap()
}

fn error_fields(r: &axum_test::TestResponse) -> Vec<String> {
    r.json::<Value>()["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no errors array: {}", r.text()))
        .iter()
        .map(|e| e["field"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test]
async fn a_sending_domain_hides_its_private_key(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let created = t
        .admin
        .post(&domains_path(t.id))
        .json(&body("example.com", "app"))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id: Uuid = created.json::<Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // A keyless one beside it, so `has_dkim_key` is shown to be a fact and not a constant.
    add_domain(&t, body("relay.example.org", "relay")).await;
    // Changing it is another way a key goes through the API.
    let patched = t
        .admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"dkim_private_key": KEY_PEM, "from_name": "Acme"}))
        .await;
    patched.assert_status_ok();

    let one = t.admin.get(&domain_path(t.id, id)).await;
    let list = t.viewer.get(&domains_path(t.id)).await;
    let checked = {
        // A response from the checker is the one that carries the most data about the domain.
        let ns = nameserver(complete_zone()).await;
        let app = TestApp::with_resolver(pool.clone(), resolver_for(&ns)).await;
        let mut app = app;
        app.login("admin-acme@example.com", PW).await;
        app.post(&check_path(t.id, id)).await
    };
    // The whole serialized body, not the field one expects the key to be in.
    for (what, text) in [
        ("create", created.text()),
        ("patch", patched.text()),
        ("get", one.text()),
        ("list", list.text()),
        ("check", checked.text()),
    ] {
        for form in key_forms() {
            assert!(
                !text.contains(&form),
                "{what} response leaked the key as {form:?}: {text}"
            );
        }
    }
    let data = list.json::<Value>()["data"].clone();
    let has_key = |domain: &str| {
        let row = data
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["domain"] == domain);
        row.unwrap_or_else(|| panic!("{domain} not listed: {data}"))["has_dkim_key"].clone()
    };
    assert_eq!(has_key("example.com"), json!(true));
    assert_eq!(has_key("relay.example.org"), json!(false));
    assert_eq!(created.json::<Value>()["has_dkim_key"], json!(true));

    // An error that quotes its input is a leak too: a PEM the route refuses is still a key.
    let mut bad = body("bad.example.com", "app");
    bad["dkim_private_key"] = json!(KEY_PEM.replace("END PRIVATE KEY", "END WRONG KEY"));
    let refused = t.admin.post(&domains_path(t.id)).json(&bad).await;
    refused.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    for form in key_forms() {
        assert!(
            !refused.text().contains(&form),
            "a 422 echoed the key: {}",
            refused.text()
        );
    }

    // No audit row, in any column, and the rows exist: "none contains it" is not true of nothing.
    t.admin
        .delete(&domain_path(t.id, id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let rows: Vec<String> =
        sqlx::query_scalar("select a::text from audit_log a where action like 'sending_domain.%'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 4, "created x2, updated, deleted: {rows:?}");
    for row in &rows {
        for form in key_forms() {
            assert!(
                !row.contains(&form),
                "an audit row holds the key as {form:?}: {row}"
            );
        }
    }
    let whole: Vec<String> = sqlx::query_scalar("select a::text from audit_log a")
        .fetch_all(&pool)
        .await
        .unwrap();
    for row in &whole {
        for form in key_forms() {
            assert!(!row.contains(&form), "an audit row holds the key: {row}");
        }
    }
}

#[sqlx::test]
async fn the_audit_rows_name_the_domain_and_what_changed(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = add_domain(&t, body("example.com", "app")).await;
    t.admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"dkim_selector": "s2", "dkim_private_key": KEY_PEM}))
        .await
        .assert_status_ok();
    t.admin
        .delete(&domain_path(t.id, id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "select action, target_type, metadata::text from audit_log
         where action like 'sending_domain.%' order by id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let actions: Vec<_> = rows.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        [
            "sending_domain.created",
            "sending_domain.updated",
            "sending_domain.deleted"
        ]
    );
    for (action, target, metadata) in &rows {
        assert_eq!(target, "sending_domain", "{action}");
        assert!(
            metadata.contains("example.com"),
            "{action} names no domain: {metadata}"
        );
    }
    // The update says which keys were sent, by name; the key is one, and its value is not.
    let updated = &rows[1].2;
    assert!(
        updated.contains("dkim_selector") && updated.contains("dkim_private_key"),
        "{updated}"
    );
}

#[sqlx::test]
async fn the_key_round_trips_through_secrets_and_stays_in_its_workspace(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let keyed = add_domain(&t, body("example.com", "app")).await;
    let keyless = add_domain(&t, body("plain.example.com", "relay")).await;

    // The signer gets the DER the PEM held, and nothing for a domain with no key.
    assert_eq!(stored_key(&t, keyed).await, Some(der(KEY_PEM)));
    assert_eq!(stored_key(&t, keyless).await, None);

    // Round-tripping alone passes on a column holding the key in the clear.
    let blob: Vec<u8> =
        sqlx::query_scalar("select dkim_private_key_encrypted from sending_domains where id = $1")
            .bind(keyed)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !blob
            .windows(16)
            .any(|w| der(KEY_PEM).windows(16).any(|k| k == w)),
        "the column holds the key in the clear"
    );

    // Scoped by workspace like every read: another workspace's id finds nothing.
    let other = tenant(&pool, "globex").await;
    let err =
        nm::sending_domains::dkim_private_key(&pool, &t.admin.state.secrets, other.id, keyed).await;
    assert!(matches!(err, Err(nm::error::ApiError::NotFound)));

    // `for_domain` is the campaign's question: this workspace's domain by name, in any case, and
    // never another workspace's.
    let found = nm::sending_domains::for_domain(&pool, t.id, "EXAMPLE.com")
        .await
        .unwrap()
        .expect("found regardless of case");
    assert_eq!((found.id, found.domain.as_str()), (keyed, "example.com"));
    assert_eq!(found.signing, nm::sending_domains::Signing::App);
    assert_eq!(found.dkim_selector, "nm1");
    assert!(
        nm::sending_domains::for_domain(&pool, other.id, "example.com")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        nm::sending_domains::for_domain(&pool, t.id, "nope.example.com")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test]
async fn another_workspaces_domain_is_404(pool: PgPool) {
    let a = tenant(&pool, "acme").await;
    let b = tenant(&pool, "globex").await;
    let id = add_domain(&a, body("acme-mail.example.com", "app")).await;

    // B's admin names A's workspace, and then B's own workspace with A's domain id.
    for ws in [a.id, b.id] {
        let responses = [
            b.admin.get(&domain_path(ws, id)).await,
            b.admin
                .patch(&domain_path(ws, id))
                .json(&json!({"from_name": "Mine now"}))
                .await,
            b.admin.delete(&domain_path(ws, id)).await,
            b.admin.post(&check_path(ws, id)).await,
        ];
        for r in responses {
            r.assert_status(StatusCode::NOT_FOUND);
            let text = r.text();
            for named in ["acme-mail", "example.com", &id.to_string()] {
                assert!(!text.contains(named), "a 404 body names {named}: {text}");
            }
        }
    }
    // Nor does it list, or create, across the boundary.
    b.admin
        .get(&domains_path(a.id))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    b.admin
        .post(&domains_path(a.id))
        .json(&body("sneak.example.com", "relay"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // And none of it did anything.
    let left: (i64, Option<String>, i64) =
        sqlx::query_as("select count(*), max(from_name), count(checked_at) from sending_domains")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, (1, None, 0));
}

#[sqlx::test]
async fn app_signing_without_a_key_is_refused_by_the_api_with_a_field_error(pool: PgPool) {
    let t = tenant(&pool, "acme").await;

    // Create, with no key at all.
    let r = t
        .admin
        .post(&domains_path(t.id))
        .json(&json!({"domain": "example.com", "signing": "app"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["dkim_private_key"]);

    // Switching a keyless domain to app, and clearing the key of an app domain: both reach the
    // table's check constraint, and neither may surface as the 500 it would be unmapped.
    let relay = add_domain(&t, body("relay.example.com", "relay")).await;
    let r = t
        .admin
        .patch(&domain_path(t.id, relay))
        .json(&json!({"signing": "app"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["dkim_private_key"]);

    let app = add_domain(&t, body("app.example.com", "app")).await;
    let r = t
        .admin
        .patch(&domain_path(t.id, app))
        .json(&json!({"dkim_private_key": null}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["dkim_private_key"]);

    // The refusals changed nothing.
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "select domain, signing, dkim_private_key_encrypted is not null from sending_domains
         order by domain",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("app.example.com".to_owned(), "app".to_owned(), true),
            ("relay.example.com".to_owned(), "relay".to_owned(), false),
        ]
    );

    // The legal moves around those: supply the key in the same request, or leave app first.
    t.admin
        .patch(&domain_path(t.id, relay))
        .json(&json!({"signing": "app", "dkim_private_key": KEY_PEM}))
        .await
        .assert_status_ok();
    t.admin
        .patch(&domain_path(t.id, app))
        .json(&json!({"signing": "relay", "dkim_private_key": null}))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn writes_take_admin_on_every_verb(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = add_domain(&t, body("example.com", "relay")).await;

    t.viewer
        .post(&domains_path(t.id))
        .json(&body("new.example.com", "relay"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    t.viewer
        .patch(&domain_path(t.id, id))
        .json(&json!({"from_name": "Viewer"}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    t.viewer
        .delete(&domain_path(t.id, id))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // The checker is a write (it stores a verdict) and an outbound call.
    t.viewer
        .post(&check_path(t.id, id))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // A viewer reads.
    t.viewer.get(&domains_path(t.id)).await.assert_status_ok();
    t.viewer
        .get(&domain_path(t.id, id))
        .await
        .assert_status_ok();

    let kept: (i64, Option<String>) =
        sqlx::query_as("select count(*), max(from_name) from sending_domains")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept, (1, None), "a refused write still wrote");
}

#[sqlx::test]
async fn a_domain_is_unique_per_workspace_whatever_its_case(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = add_domain(&t, body("Example.COM", "relay")).await;
    // Stored in the form the checker and `for_domain` look it up in.
    let shown = t.admin.get(&domain_path(t.id, id)).await.json::<Value>();
    assert_eq!(shown["domain"], json!("example.com"));

    let r = t
        .admin
        .post(&domains_path(t.id))
        .json(&body("example.com", "relay"))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert!(r.text().contains("sending-domain-taken"), "{}", r.text());

    // Another workspace may use the same name.
    let other = tenant(&pool, "globex").await;
    add_domain(&other, body("example.com", "relay")).await;
}

#[sqlx::test]
async fn bad_input_is_422_and_names_every_field_at_once(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let post = |v: Value| {
        let (url, admin) = (domains_path(t.id), &t.admin);
        async move { admin.post(&url).json(&v).await }
    };

    // Several mistakes, one response.
    let r = post(json!({
        "domain": "not a domain",
        "signing": "relay",
        "dkim_selector": "-bad-",
        "return_path_domain": "x",
        "from_name": "Line\r\nBcc: victim@example.net",
        "dkim_private_key": "not a pem",
    }))
    .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let mut fields = error_fields(&r);
    fields.sort();
    assert_eq!(
        fields,
        [
            "dkim_private_key",
            "dkim_selector",
            "domain",
            "from_name",
            "return_path_domain"
        ]
    );

    // Names that are not domains. Each is refused alone, so one passing for the wrong reason
    // (some other field being bad) cannot hide here.
    for bad in [
        "",
        "com",
        "-a.com",
        "a-.com",
        "a..com",
        "a b.com",
        "exa_mple.com",
        "例え.jp",
        "a.com.",
    ] {
        post(body(bad, "relay"))
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let long_label = format!("{}.com", "a".repeat(64));
    post(body(&long_label, "relay"))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    // The same rule on the other two names, by their own field.
    let mut v = body("ok.example.com", "relay");
    v["return_path_domain"] = json!("bad domain");
    let r = post(v).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["return_path_domain"]);

    // An unknown field, a missing one and a wrong enum are all 422, not 400 or 500.
    let mut unknown = body("a.example.com", "relay");
    unknown["dkim_privat_key"] = json!("typo");
    post(unknown)
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    post(json!({"domain": "a.example.com"}))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    post(body("a.example.com", "both"))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    // Valid ones: a selector with a dot, a return path equal to the domain, mixed case and spaces
    // around the name.
    let mut v = body("  Ok.Example.com ", "relay");
    v["dkim_selector"] = json!("Mail.2026");
    v["return_path_domain"] = json!("ok.example.com");
    v["from_name"] = json!("  Acme News ");
    let r = post(v).await;
    r.assert_status(StatusCode::CREATED);
    let shown = r.json::<Value>();
    assert_eq!(shown["domain"], json!("ok.example.com"));
    assert_eq!(shown["dkim_selector"], json!("mail.2026"));
    assert_eq!(shown["from_name"], json!("Acme News"));

    // PATCH validates too: the rule on one verb of two is the defect this suite has had before.
    let id: Uuid = shown["id"].as_str().unwrap().parse().unwrap();
    for patch in [
        json!({"dkim_selector": "bad selector"}),
        json!({"return_path_domain": "com"}),
        json!({"from_name": "  "}),
        json!({"dkim_private_key": "not a pem"}),
        // Null where there is nothing to clear to.
        json!({"signing": null}),
        json!({"dkim_selector": null}),
        // The domain is its identity: it is not editable, and says so as an unknown field.
        json!({"domain": "other.example.com"}),
    ] {
        t.admin
            .patch(&domain_path(t.id, id))
            .json(&patch)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[sqlx::test]
async fn patch_keeps_what_it_is_not_sent_and_clears_on_null(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let mut v = body("example.com", "relay");
    v["from_name"] = json!("Acme");
    v["return_path_domain"] = json!("bounce.example.com");
    v["dkim_private_key"] = json!(KEY_PEM);
    let id = add_domain(&t, v).await;
    let url = domain_path(t.id, id);

    // Nothing sent, nothing changed: the key included.
    let r = t
        .admin
        .patch(&url)
        .json(&json!({"dkim_selector": "s2"}))
        .await;
    r.assert_status_ok();
    let shown = r.json::<Value>();
    assert_eq!(shown["dkim_selector"], json!("s2"));
    assert_eq!(shown["from_name"], json!("Acme"));
    assert_eq!(shown["return_path_domain"], json!("bounce.example.com"));
    assert_eq!(shown["has_dkim_key"], json!(true));
    assert_eq!(stored_key(&t, id).await, Some(der(KEY_PEM)));

    // `null` clears the three that can be, and says so.
    let r = t
        .admin
        .patch(&url)
        .json(&json!({"from_name": null, "return_path_domain": null, "dkim_private_key": null}))
        .await;
    r.assert_status_ok();
    let shown = r.json::<Value>();
    assert_eq!(shown["from_name"], Value::Null);
    assert_eq!(shown["return_path_domain"], Value::Null);
    assert_eq!(shown["has_dkim_key"], json!(false));
    assert_eq!(stored_key(&t, id).await, None);
    let blob: Option<Vec<u8>> =
        sqlx::query_scalar("select dkim_private_key_encrypted from sending_domains where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        blob, None,
        "cleared means NULL, not an encrypted empty string"
    );

    // A new key replaces it, in the other armour: what is stored is the DER the PEM carried.
    t.admin
        .patch(&url)
        .json(&json!({"dkim_private_key": OTHER_PEM}))
        .await
        .assert_status_ok();
    assert_eq!(stored_key(&t, id).await, Some(der(OTHER_PEM)));
}

#[sqlx::test]
async fn editing_what_was_checked_resets_the_verdict(pool: PgPool) {
    let ns = nameserver(complete_zone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let mut v = body("example.com", "relay");
    v["return_path_domain"] = json!("bounce.example.com");
    let id = add_domain(&t, v).await;
    let checked = t.admin.post(&check_path(t.id, id)).await;
    checked.assert_status_ok();
    assert_ne!(checked.json::<Value>()["checked_at"], Value::Null);

    // The display name has nothing to do with DNS: the verdict stands.
    let r = t
        .admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"from_name": "Acme"}))
        .await;
    assert_eq!(r.json::<Value>()["checks"]["spf"]["status"], json!("pass"));
    assert_ne!(r.json::<Value>()["checked_at"], Value::Null);

    // An acknowledgement is of the configuration it was given for. Given now, it survives an edit
    // that has nothing to do with DNS, and is set so that the loop below has something to clear.
    let acknowledge = || async {
        let r = t
            .admin
            .patch(&domain_path(t.id, id))
            .json(&json!({"send_unverified_acknowledged": true}))
            .await;
        r.assert_status_ok();
        assert_ne!(
            r.json::<Value>()["send_unverified_acknowledged_at"],
            Value::Null
        );
    };
    acknowledge().await;
    let r = t
        .admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"from_name": "Acme Again"}))
        .await;
    assert_ne!(
        r.json::<Value>()["send_unverified_acknowledged_at"],
        Value::Null,
        "an edit that changes nothing the checker reads withdrew the acknowledgement"
    );

    // Each of the things the checker reads empties it, back to "never run", and withdraws the
    // acknowledgement with it.
    for patch in [
        json!({"dkim_selector": "s2"}),
        json!({"return_path_domain": null}),
        json!({"dkim_private_key": KEY_PEM}),
        // Whose key must be published at the selector changes with the mode.
        json!({"signing": "app"}),
    ] {
        t.admin.post(&check_path(t.id, id)).await.assert_status_ok();
        acknowledge().await;
        let r = t.admin.patch(&domain_path(t.id, id)).json(&patch).await;
        r.assert_status_ok();
        let shown = r.json::<Value>();
        assert_eq!(shown["checked_at"], Value::Null, "{patch}");
        assert_eq!(
            shown["send_unverified_acknowledged_at"],
            Value::Null,
            "{patch} left the acknowledgement of the old configuration standing"
        );
        for check in ["spf", "dkim", "dmarc", "return_path"] {
            assert_eq!(
                shown["checks"][check]["status"],
                json!("not_checked"),
                "{patch}: {check}"
            );
        }
    }
}

#[sqlx::test]
async fn an_acknowledgement_is_stamped_withdrawn_audited_and_read_by_for_domain(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = add_domain(&t, body("example.com", "relay")).await;
    let url = domain_path(t.id, id);
    let shown = t.admin.get(&url).await.json::<Value>();
    assert_eq!(shown["send_unverified_acknowledged_at"], Value::Null);

    let r = t
        .admin
        .patch(&url)
        .json(&json!({"send_unverified_acknowledged": true}))
        .await;
    r.assert_status_ok();
    let at = r.json::<Value>()["send_unverified_acknowledged_at"].clone();
    assert!(at.is_string(), "{at}");
    // A viewer sees it, and the sender's own lookup carries it: Task 10 reads it from there.
    assert_eq!(
        t.viewer.get(&url).await.json::<Value>()["send_unverified_acknowledged_at"],
        at
    );
    let found = nm::sending_domains::for_domain(&pool, t.id, "example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(found.send_unverified_acknowledged_at.is_some());

    // Withdrawn on request, and `null` is not a way to say that.
    t.admin
        .patch(&url)
        .json(&json!({"send_unverified_acknowledged": null}))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let r = t
        .admin
        .patch(&url)
        .json(&json!({"send_unverified_acknowledged": false}))
        .await;
    assert_eq!(
        r.json::<Value>()["send_unverified_acknowledged_at"],
        Value::Null
    );

    // Sent in the same request as an edit the checks depend on, it is the acknowledgement of the
    // configuration being saved, so it stands while the checks are reset.
    let r = t
        .admin
        .patch(&url)
        .json(&json!({"dkim_selector": "s3", "send_unverified_acknowledged": true}))
        .await;
    r.assert_status_ok();
    let shown = r.json::<Value>();
    assert!(
        shown["send_unverified_acknowledged_at"].is_string(),
        "{shown}"
    );
    assert_eq!(shown["checked_at"], Value::Null);

    // A viewer may not give it: it is what permits sending from a domain that is not verified.
    t.viewer
        .patch(&url)
        .json(&json!({"send_unverified_acknowledged": true}))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Audited like the other configuration changes: by key name, and with the value, which is
    // what the audit question "who accepted this, and when" needs.
    let rows: Vec<Value> = sqlx::query_scalar(
        "select metadata from audit_log where action = 'sending_domain.updated' order by id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // Three: given, withdrawn, and given with a selector change. The refused `null` and the
    // viewer's attempt wrote nothing.
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["send_unverified_acknowledged"], json!(true));
    assert_eq!(rows[0]["fields"], json!(["send_unverified_acknowledged"]));
    assert_eq!(rows[1]["send_unverified_acknowledged"], json!(false));
    assert_eq!(rows[2]["send_unverified_acknowledged"], json!(true));
    assert_eq!(
        rows[2]["fields"],
        json!(["dkim_selector", "send_unverified_acknowledged"])
    );
}

#[sqlx::test]
async fn the_view_publishes_the_public_half_of_the_stored_key_and_only_that(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let record =
        |pem: &str| nm::mail::dkim::public_key_txt(&nm::mail::dkim::key_from_pem(pem).unwrap());
    let mut v = body("example.com", "app");
    v["dkim_private_key"] = json!(KEY_PEM);
    let created = t.admin.post(&domains_path(t.id)).json(&v).await;
    created.assert_status(StatusCode::CREATED);
    let shown = created.json::<Value>();
    let id: Uuid = shown["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(shown["dkim_public_txt"], json!(record(KEY_PEM)));
    assert!(record(KEY_PEM).starts_with("v=DKIM1; k=rsa; p="));

    // A PKCS#1 key gives its own record: the stored DER carries no armour, and both forms have to
    // come back out as the key they are.
    let mut v = body("pkcs1.example.com", "app");
    v["dkim_private_key"] = json!(KEY_PEM_PKCS1);
    let r = t.admin.post(&domains_path(t.id)).json(&v).await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(
        r.json::<Value>()["dkim_public_txt"],
        json!(record(KEY_PEM_PKCS1))
    );
    assert_ne!(
        record(KEY_PEM_PKCS1),
        record(KEY_PEM),
        "the fixtures are one key"
    );

    // A keyless domain has nothing to publish.
    let keyless = add_domain(&t, body("relay.example.com", "relay")).await;
    assert_eq!(
        t.admin
            .get(&domain_path(t.id, keyless))
            .await
            .json::<Value>()["dkim_public_txt"],
        Value::Null
    );

    // Derived from what is stored now: a replaced key changes it, a cleared one removes it.
    let r = t
        .admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"dkim_private_key": OTHER_PEM}))
        .await;
    assert_eq!(
        r.json::<Value>()["dkim_public_txt"],
        json!(record(OTHER_PEM))
    );
    assert_ne!(
        record(OTHER_PEM),
        record(KEY_PEM),
        "the fixtures are one key"
    );
    let r = t
        .admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"signing": "relay", "dkim_private_key": null}))
        .await;
    assert_eq!(r.json::<Value>()["dkim_public_txt"], Value::Null);

    // Every route that returns a domain carries it, a viewer included; and none returns the
    // private key in any form while doing so.
    let list = t.viewer.get(&domains_path(t.id)).await;
    let data = list.json::<Value>()["data"].clone();
    let txts: Vec<_> = data
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["domain"].as_str().unwrap().to_owned(),
                d["dkim_public_txt"].clone(),
            )
        })
        .collect();
    assert!(
        txts.contains(&("pkcs1.example.com".to_owned(), json!(record(KEY_PEM_PKCS1)))),
        "{txts:?}"
    );
    assert!(
        txts.contains(&("relay.example.com".to_owned(), Value::Null)),
        "{txts:?}"
    );
    for form in key_forms() {
        assert!(
            !list.text().contains(&form),
            "the list leaked the private key"
        );
    }

    // Never from the client: it is not a field a request may carry.
    let mut v = body("sneaky.example.com", "relay");
    v["dkim_public_txt"] = json!("v=DKIM1; k=rsa; p=AAAA");
    t.admin
        .post(&domains_path(t.id))
        .json(&v)
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    t.admin
        .patch(&domain_path(t.id, keyless))
        .json(&json!({"dkim_public_txt": "v=DKIM1; k=rsa; p=AAAA"}))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------------------------
// The checker, through the route.

async fn checks_of(t: &Tenant, id: Uuid) -> Value {
    let r = t.admin.post(&check_path(t.id, id)).await;
    r.assert_status_ok();
    r.json::<Value>()
}

#[sqlx::test]
async fn a_check_stores_each_verdict_and_when_it_ran(pool: PgPool) {
    let ns = nameserver(complete_zone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let mut v = body("example.com", "relay");
    v["return_path_domain"] = json!("bounce.example.com");
    let id = add_domain(&t, v).await;
    // Before any check: the checker has never run, and the document says so.
    let before = t.admin.get(&domain_path(t.id, id)).await.json::<Value>();
    assert_eq!(before["checked_at"], Value::Null);
    assert_eq!(before["checks"]["spf"], json!({"status": "not_checked"}));

    let shown = checks_of(&t, id).await;
    for check in ["spf", "dkim", "dmarc", "return_path"] {
        assert_eq!(shown["checks"][check], json!({"status": "pass"}), "{check}");
    }
    assert_ne!(shown["checked_at"], Value::Null);

    // Stored, not only returned: a later read, and the column itself, show the same.
    let later = t.admin.get(&domain_path(t.id, id)).await.json::<Value>();
    assert_eq!(later["checks"], shown["checks"]);
    assert_eq!(later["checked_at"], shown["checked_at"]);
    let column: Value = sqlx::query_scalar("select checks from sending_domains where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(column, shown["checks"]);
    let at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("select checked_at from sending_domains where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(at.is_some());

    // A domain with no return path leaves that check un-run, not failed.
    let plain = add_domain(&t, body("plain.example.com", "relay")).await;
    assert_eq!(
        checks_of(&t, plain).await["checks"]["return_path"],
        json!({"status": "not_checked"})
    );
}

/// **A domain this product signs must publish the key it signs with.** Publishing somebody
/// else's leaves SPF and DMARC passing and DKIM failing at the receiver, which is the failure an
/// operator cannot see from in here.
#[sqlx::test]
async fn an_app_signed_domain_must_publish_the_key_it_signs_with(pool: PgPool) {
    let publish = |zone: &Zone, name: &str, pem: &str| {
        zone.clear(name, RecordType::TXT);
        zone.txt(name, &format!("v=DKIM1; k=rsa; p={}", published_p(pem)));
    };
    let zone = complete_zone();
    // A real DKIM record for a real key, and not this domain's.
    publish(&zone, "nm1._domainkey.example.com", OTHER_PEM);
    let ns = nameserver(zone.clone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let id = add_domain(&t, body("example.com", "app")).await;

    let shown = checks_of(&t, id).await;
    let reason = shown["checks"]["dkim"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("the wrong key passed: {shown}"));
    assert!(reason.contains("does not match"), "{reason}");
    // Neither key is named, whole or in part: this reason is stored and shown to an operator.
    for p in [published_p(KEY_PEM), published_p(OTHER_PEM)] {
        for window in p.as_bytes().windows(8) {
            let w = std::str::from_utf8(window).unwrap();
            assert!(!reason.contains(w), "the reason leaks {w:?}: {reason}");
        }
    }
    assert_eq!(
        shown["checks"]["spf"]["status"],
        json!("pass"),
        "the other checks are unaffected: {shown}"
    );

    // The key it does sign with, published: the check passes. Without this half the test above
    // would pass just as well on a checker that failed every app-signed domain — which is what
    // handing `expected_p` the whole `v=DKIM1; …` record instead of the bare `p=` value does.
    publish(&zone, "nm1._domainkey.example.com", KEY_PEM);
    assert_eq!(
        checks_of(&t, id).await["checks"]["dkim"],
        json!({"status": "pass"})
    );

    // And a key saved in the other armour. The stored DER does not record which armour it came
    // in, so this is the half that breaks if loading it back guesses.
    let mut v = body("pkcs1.example.com", "app");
    v["dkim_private_key"] = json!(KEY_PEM_PKCS1);
    let pkcs1 = add_domain(&t, v).await;
    publish(&zone, "nm1._domainkey.pkcs1.example.com", KEY_PEM_PKCS1);
    assert_eq!(
        checks_of(&t, pkcs1).await["checks"]["dkim"],
        json!({"status": "pass"})
    );

    // Hand that same domain to the relay and the comparison stops: what is published is then the
    // relay's key, which this product has no way to know, and holding it to the key still stored
    // here would fail a domain that is set up correctly.
    t.admin
        .patch(&domain_path(t.id, pkcs1))
        .json(&json!({"signing": "relay"}))
        .await
        .assert_status_ok();
    publish(&zone, "nm1._domainkey.pkcs1.example.com", OTHER_PEM);
    assert_eq!(
        checks_of(&t, pkcs1).await["checks"]["dkim"],
        json!({"status": "pass"})
    );
}

/// A key the signer cannot use is a field error while it is being saved, and never a panic in a
/// worker holding a campaign.
#[sqlx::test]
async fn a_key_the_signer_cannot_use_is_refused_when_it_is_saved(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let mut v = body("example.com", "app");
    v["dkim_private_key"] = json!(TOO_SMALL_PEM);
    let r = t.admin.post(&domains_path(t.id)).json(&v).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["dkim_private_key"]);
    // It says what is wrong with the key without quoting it.
    let text = r.text();
    assert!(text.to_lowercase().contains("small"), "{text}");
    for line in TOO_SMALL_PEM.lines().filter(|l| !l.starts_with("-----")) {
        assert!(
            line.trim().is_empty() || !text.contains(line.trim()),
            "the refusal echoed the key: {text}"
        );
    }
    // The same refusal on the other verb, and nothing was stored by either.
    let id = add_domain(&t, body("ok.example.com", "app")).await;
    let patched = t
        .admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"dkim_private_key": TOO_SMALL_PEM}))
        .await;
    patched.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&patched), ["dkim_private_key"]);
    let stored: Vec<Vec<u8>> =
        sqlx::query_scalar("select dkim_private_key_encrypted from sending_domains")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(stored.len(), 1, "a refused key was stored");
    assert_eq!(stored_key(&t, id).await, Some(der(KEY_PEM)));
}

#[sqlx::test]
async fn a_check_records_why_each_one_failed_and_a_fixed_record_passes_next_time(pool: PgPool) {
    let zone = Zone::default();
    zone.txt("example.com", "v=spf1 -all")
        .txt("example.com", "v=spf1 include:other.example.net -all")
        .txt("nm1._domainkey.example.com", "v=DKIM1; k=rsa")
        .cname("bounce.example.com", "relay.example.net");
    // No DMARC record at all, and a return-path domain that is not ours.
    let ns = nameserver(zone.clone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let mut v = body("example.com", "relay");
    v["return_path_domain"] = json!("bounce.example.net");
    let id = add_domain(&t, v).await;

    let shown = checks_of(&t, id).await;
    let failing = |check: &str| {
        let c = &shown["checks"][check];
        assert_eq!(c["status"], json!("fail"), "{check}: {c}");
        c["reason"].as_str().unwrap().to_owned()
    };
    assert!(failing("spf").contains("more than one SPF"));
    assert!(failing("dkim").contains("no p= tag"));
    assert!(failing("dmarc").contains("no DMARC record"));
    assert!(failing("return_path").contains("neither the sending domain"));

    // Fix all four at the nameserver and check again at once. A resolver that cached would answer
    // with what it saw a moment ago; this is the operator's whole workflow.
    zone.clear("example.com", RecordType::TXT);
    zone.txt("example.com", "v=spf1 include:other.example.net -all")
        .txt("_dmarc.example.com", "v=DMARC1; p=none");
    zone.clear("nm1._domainkey.example.com", RecordType::TXT);
    zone.txt("nm1._domainkey.example.com", "v=DKIM1; k=rsa; p=QUJD");
    t.admin
        .patch(&domain_path(t.id, id))
        .json(&json!({"return_path_domain": "bounce.example.com"}))
        .await
        .assert_status_ok();
    let fixed = checks_of(&t, id).await;
    for check in ["spf", "dkim", "dmarc", "return_path"] {
        assert_eq!(
            fixed["checks"][check],
            json!({"status": "pass"}),
            "{check}: {fixed}"
        );
    }
}

#[sqlx::test]
async fn a_return_path_with_no_cname_fails_the_check(pool: PgPool) {
    let zone = complete_zone();
    zone.clear("bounce.example.com", RecordType::CNAME);
    // A TXT record there is not a CNAME.
    zone.txt("bounce.example.com", "v=spf1 -all");
    let ns = nameserver(zone).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let mut v = body("example.com", "relay");
    v["return_path_domain"] = json!("bounce.example.com");
    let id = add_domain(&t, v).await;

    let shown = checks_of(&t, id).await;
    assert_eq!(shown["checks"]["return_path"]["status"], json!("fail"));
    assert!(
        shown["checks"]["return_path"]["reason"]
            .as_str()
            .unwrap()
            .contains("no CNAME"),
        "{shown}"
    );
    assert_eq!(
        shown["checks"]["spf"]["status"],
        json!("pass"),
        "the others are unaffected"
    );
}

#[sqlx::test]
async fn the_check_is_rate_limited_per_workspace(pool: PgPool) {
    // One user in two workspaces, through one app: the limiter lives in the app's state, so only
    // one app can show that the count is per workspace and not per process.
    let owner = create_user(&pool, "owner@example.com", PW, false).await;
    let (first, second) = (
        create_workspace(&pool, owner, "acme").await,
        create_workspace(&pool, owner, "globex").await,
    );
    let ns = nameserver(complete_zone()).await;
    let mut app = TestApp::with_resolver(pool.clone(), resolver_for(&ns)).await;
    app.login("owner@example.com", PW).await;
    let mut ids = Vec::new();
    for ws in [first, second] {
        let r = app
            .post(&domains_path(ws))
            .json(&body("example.com", "relay"))
            .await;
        r.assert_status(StatusCode::CREATED);
        ids.push(
            r.json::<Value>()["id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        );
    }

    let limit = nm::http::rate_limit::DNS_CHECK_PER_MINUTE;
    for n in 0..limit {
        let r = app.post(&check_path(first, ids[0])).await;
        r.assert_status_ok();
        assert!(r.headers().contains_key("ratelimit-remaining"), "check {n}");
    }
    let queried = ns.queries.load(Ordering::SeqCst);
    let refused = app.post(&check_path(first, ids[0])).await;
    refused.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert!(refused.headers().contains_key("retry-after"));
    // A refused check does no lookups.
    assert_eq!(ns.queries.load(Ordering::SeqCst), queried);

    // The same user, the other workspace: its budget is its own.
    app.post(&check_path(second, ids[1]))
        .await
        .assert_status_ok();
    // And the refusal was the limiter's: the domain itself still reads.
    app.get(&domain_path(first, ids[0]))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn a_check_holds_no_pooled_connection_while_dns_hangs(pool: PgPool) {
    let ns = nameserver(complete_zone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let id = add_domain(&t, body("example.com", "relay")).await;
    let in_use = || pool.size() as usize - pool.num_idle();
    // A released connection goes back to the pool on a spawned task, so what the setup used is
    // not idle the instant its last request returns. Measured relative to the settled count,
    // which makes the assertion about what the check adds and not about what was already there.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = in_use();

    let hang = ns.hold.clone().lock_owned().await;
    let request = async { t.admin.post(&check_path(t.id, id)).await };
    let observer = async {
        // Wait until the checker has reached the nameserver: until then the request may not have
        // got past its database reads, and a zero here would prove nothing.
        while ns.queries.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        in_use()
    };
    // The request cannot finish while the nameserver is held, so the observer decides the race;
    // the request is dropped with the select.
    let held = tokio::select! {
        _ = request => panic!("the check finished against a nameserver that was not answering"),
        held = observer => held,
    };
    drop(hang);
    assert_eq!(
        held, before,
        "the check held a pooled connection while waiting on DNS"
    );
}

#[sqlx::test]
async fn a_domain_edited_while_it_is_being_checked_is_not_given_the_old_verdict(pool: PgPool) {
    let ns = nameserver(complete_zone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let id = add_domain(&t, body("example.com", "relay")).await;

    let hang = ns.hold.clone().lock_owned().await;
    let check = async { t.admin.post(&check_path(t.id, id)).await };
    let edit = async {
        while ns.queries.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let r = t
            .admin
            .patch(&domain_path(t.id, id))
            .json(&json!({"dkim_selector": "s2"}))
            .await;
        r.assert_status_ok();
        drop(hang);
    };
    let (checked, ()) = tokio::join!(check, edit);

    // The verdict was for selector `nm1`, which is no longer the selector: it is refused rather
    // than written over the reset the edit made.
    checked.assert_status(StatusCode::CONFLICT);
    assert!(
        checked.text().contains("sending-domain-changed"),
        "{}",
        checked.text()
    );
    let shown = t.admin.get(&domain_path(t.id, id)).await.json::<Value>();
    assert_eq!(shown["checked_at"], Value::Null);
    assert_eq!(shown["checks"]["dkim"], json!({"status": "not_checked"}));
    assert_eq!(shown["dkim_selector"], json!("s2"));
}

#[sqlx::test]
async fn a_domain_deleted_while_it_is_being_checked_is_404(pool: PgPool) {
    let ns = nameserver(complete_zone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let id = add_domain(&t, body("example.com", "relay")).await;

    let hang = ns.hold.clone().lock_owned().await;
    let check = async { t.admin.post(&check_path(t.id, id)).await };
    let delete = async {
        while ns.queries.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        t.admin
            .delete(&domain_path(t.id, id))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        drop(hang);
    };
    let (checked, ()) = tokio::join!(check, delete);

    // Gone, not "changed": the two are told apart, and nothing was written back.
    checked.assert_status(StatusCode::NOT_FOUND);
    let left: i64 = sqlx::query_scalar("select count(*) from sending_domains")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn a_nameserver_that_never_answers_ends_in_failed_checks_not_a_hung_request(pool: PgPool) {
    // The route's bound is `dns::LOOKUP_TIMEOUT` (5 s); this takes that long, once.
    let ns = nameserver(complete_zone()).await;
    let t = tenant_with(&pool, "acme", |p| {
        TestApp::with_resolver(p, resolver_for(&ns))
    })
    .await;
    let id = add_domain(&t, body("example.com", "relay")).await;

    let hang = ns.hold.clone().lock_owned().await;
    let started = std::time::Instant::now();
    let shown = checks_of(&t, id).await;
    let took = started.elapsed();
    drop(hang);

    for check in ["spf", "dkim", "dmarc"] {
        assert_eq!(shown["checks"][check]["status"], json!("fail"), "{check}");
        assert!(
            shown["checks"][check]["reason"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
    }
    // Bounds, not an equality: a timeout plus whatever the scheduler adds. Under twice the
    // lookup bound is what shows the lookups overlapped; serial would be three times it.
    assert!(
        took >= dns::LOOKUP_TIMEOUT,
        "answered in {took:?}, before the lookup bound"
    );
    assert!(took < dns::LOOKUP_TIMEOUT * 2, "took {took:?}");
}
