// SPDX-License-Identifier: AGPL-3.0-only

//! Migration 0028: the three tables the sending core reads. Raw SQL on purpose: the guarantees
//! under test belong to the database, and a handler that happens to check first proves nothing.
//!
//! From "The relay API" down, the tests drive the HTTP API instead: the guarantee there is that a
//! relay's password reaches no response, no audit row and no error, which only the whole stack can
//! show.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await
        .expect("insert workspace");
    id
}

async fn relay(pool: &PgPool, ws: Uuid, name: &str) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into relays (id, workspace_id, name, host, port, security, rate_limit_per_minute)
         values ($1, $2, $3, 'smtp.example.com', 587, 'starttls', 60)",
    )
    .bind(id)
    .bind(ws)
    .bind(name)
    .execute(pool)
    .await
    .map(|_| id)
}

fn sqlstate(err: &sqlx::Error) -> String {
    err.as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()))
        .unwrap_or_else(|| format!("not a database error: {err}"))
}

#[sqlx::test]
async fn relay_requires_a_workspace(pool: PgPool) {
    let err = relay(&pool, Uuid::now_v7(), "primary").await.unwrap_err();
    assert_eq!(sqlstate(&err), "23503", "{err}");
}

#[sqlx::test]
async fn deleting_a_workspace_takes_its_relays_and_buckets(pool: PgPool) {
    let bucket = |relay_id: Uuid, ws: Uuid| {
        sqlx::query(
            "insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 60)",
        )
        .bind(relay_id)
        .bind(ws)
        .execute(&pool)
    };
    // A second workspace, untouched by the delete. Without it the `count(*) == 0` assertions below
    // would hold just as well on a cascade that emptied the whole table.
    let (doomed, bystander) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "globex").await,
    );
    let doomed_relay = relay(&pool, doomed, "primary").await.unwrap();
    let surviving_relay = relay(&pool, bystander, "primary").await.unwrap();
    bucket(doomed_relay, doomed).await.unwrap();
    bucket(surviving_relay, bystander).await.unwrap();

    // The rows exist BEFORE the delete. Asserting only that they are gone afterwards cannot tell
    // a working cascade from an insert that never happened.
    for (table, n) in counts(&pool).await {
        assert_eq!(
            n, 2,
            "{table} should hold both workspaces' rows before the delete"
        );
    }

    sqlx::query("delete from workspaces where id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("the cascade must reach relays and relay_buckets");

    for (table, n) in counts(&pool).await {
        assert_eq!(n, 1, "{table}: the delete took the wrong number of rows");
    }
    let left: Uuid = sqlx::query_scalar("select workspace_id from relays")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        left, bystander,
        "the surviving relay belongs to the wrong workspace"
    );
}

/// Literal queries rather than a table name interpolated into one: sqlx 0.9 takes only
/// `&'static str`, and reaching for `AssertSqlSafe` to dodge that in a test would be teaching the
/// escape hatch for no gain.
async fn counts(pool: &PgPool) -> [(&'static str, i64); 2] {
    let one = |sql| sqlx::query_scalar::<_, i64>(sql).fetch_one(pool);
    [
        ("relays", one("select count(*) from relays").await.unwrap()),
        (
            "relay_buckets",
            one("select count(*) from relay_buckets").await.unwrap(),
        ),
    ]
}

#[sqlx::test]
async fn an_app_signed_domain_without_a_key_is_refused(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let insert = |domain: &'static str, signing: &'static str, key: Option<&'static [u8]>| {
        sqlx::query(
            "insert into sending_domains (id, workspace_id, domain, signing, dkim_selector,
                                          dkim_private_key_encrypted)
             values ($1, $2, $3, $4, 'nm1', $5)",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(domain)
        .bind(signing)
        .bind(key)
        .execute(&pool)
    };

    let err = insert("a.example.com", "app", None).await.unwrap_err();
    assert_eq!(sqlstate(&err), "23514", "{err}");
    // The check refuses only the keyless app-signed row: the other three shapes are legal.
    insert("b.example.com", "app", Some(b"blob")).await.unwrap();
    insert("c.example.com", "relay", None).await.unwrap();
}

#[sqlx::test]
async fn the_dns_checks_column_must_hold_an_object(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let insert = |domain: &'static str, checks: &'static str| {
        sqlx::query(
            "insert into sending_domains (id, workspace_id, domain, signing, dkim_selector, checks)
             values ($1, $2, $3, 'relay', 'nm1', $4::jsonb)",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(domain)
        .bind(checks)
        .execute(&pool)
    };

    // Every reader indexes into this by key, and jsonb takes a bare scalar or a null as happily
    // as an object.
    for bad in ["7", "\"ok\"", "null", "[]"] {
        let err = insert("a.example.com", bad).await.unwrap_err();
        assert_eq!(sqlstate(&err), "23514", "{bad} was accepted: {err}");
    }
    insert("b.example.com", "{}").await.unwrap();
    insert("c.example.com", r#"{"spf": "pass"}"#).await.unwrap();
}

#[sqlx::test]
async fn two_relays_cannot_share_a_name_in_one_workspace(pool: PgPool) {
    let (a, b) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "globex").await,
    );
    relay(&pool, a, "primary").await.unwrap();

    let err = relay(&pool, a, "primary").await.unwrap_err();
    assert_eq!(sqlstate(&err), "23505", "{err}");
    relay(&pool, b, "primary").await.unwrap();
}

#[sqlx::test]
async fn a_bucket_cannot_point_at_another_workspaces_relay(pool: PgPool) {
    let (a, b) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "globex").await,
    );
    let relay_of_a = relay(&pool, a, "primary").await.unwrap();

    let err = sqlx::query(
        "insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 60)",
    )
    .bind(relay_of_a)
    .bind(b)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "23503", "{err}");
}

#[sqlx::test]
async fn a_bucket_cannot_go_negative(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let relay_id = relay(&pool, ws, "primary").await.unwrap();
    sqlx::query("insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 1)")
        .bind(relay_id)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();

    // Spending the LAST token must be legal: the bucket's whole job is to hand out exactly its
    // limit, and Task 8 debits it down to nothing on the batch that uses the last of it. Without
    // this half the test passes on `check (tokens > 0)`, which would refuse that batch.
    sqlx::query("update relay_buckets set tokens = tokens - 1")
        .execute(&pool)
        .await
        .expect("spending the last token is legal");

    let err = sqlx::query("update relay_buckets set tokens = tokens - 1")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err), "23514", "{err}");
}

// ---------------------------------------------------------------------------------------------
// The relay API.

/// A password no other string in a response, a log or an audit row could be by accident.
const SECRET: &str = "s3kr1t-relay-pw-7f3a9c";

struct Tenant {
    id: Uuid,
    admin: TestApp,
    viewer: TestApp,
}

async fn tenant(pool: &PgPool, slug: &str) -> Tenant {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let id = create_workspace(pool, owner, slug).await;
    let member = |role: Role| {
        let email = format!("{role:?}-{slug}@example.com").to_lowercase();
        async move {
            let user = create_user(pool, &email, PW, false).await;
            add_member(pool, id, user, role).await;
            let mut app = TestApp::new(pool.clone()).await;
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

fn relays_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/relays")
}

fn relay_path(ws: Uuid, id: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/relays/{id}")
}

fn body(name: &str, password: Option<&str>) -> Value {
    json!({
        "name": name, "host": "smtp.example.com", "port": 587, "security": "starttls",
        "username": "apikey", "password": password, "rate_limit_per_minute": 60,
    })
}

async fn add_relay(t: &Tenant, name: &str, password: Option<&str>) -> Uuid {
    let r = t
        .admin
        .post(&relays_path(t.id))
        .json(&body(name, password))
        .await;
    r.assert_status(StatusCode::CREATED);
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn stored_password(t: &Tenant, id: Uuid) -> Option<String> {
    nm::relay::password(&t.admin.pool, &t.admin.state.secrets, t.id, id)
        .await
        .unwrap()
}

#[sqlx::test]
async fn a_created_relay_is_listed_without_its_password(pool: PgPool) {
    let t = tenant(&pool, "acme").await;

    let created = t
        .admin
        .post(&relays_path(t.id))
        .json(&body("primary", Some(SECRET)))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id: Uuid = created.json::<Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    add_relay(&t, "keyless", None).await;

    let one = t.admin.get(&relay_path(t.id, id)).await;
    let list = t.viewer.get(&relays_path(t.id)).await;
    // The whole serialized body, not the field one expects the password to be in: a password
    // that leaked under any other key, or in an array, is still a leak.
    for (what, text) in [
        ("create", created.text()),
        ("get", one.text()),
        ("list", list.text()),
    ] {
        assert!(!text.contains(SECRET), "{what} response leaked it: {text}");
    }

    let data = list.json::<Value>()["data"].clone();
    let has_password = |name: &str| {
        let row = data.as_array().unwrap().iter().find(|r| r["name"] == name);
        row.unwrap_or_else(|| panic!("{name} not listed: {data}"))["has_password"].clone()
    };
    assert_eq!(has_password("primary"), json!(true));
    // Without this, a handler that answers `true` for everything passes.
    assert_eq!(has_password("keyless"), json!(false));
    assert_eq!(created.json::<Value>()["has_password"], json!(true));
}

#[sqlx::test]
async fn the_password_round_trips_through_secrets(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = add_relay(&t, "primary", Some(SECRET)).await;
    let keyless = add_relay(&t, "keyless", None).await;

    assert_eq!(stored_password(&t, id).await.as_deref(), Some(SECRET));
    assert_eq!(stored_password(&t, keyless).await, None);

    // Round-tripping alone would pass on a column holding the plaintext.
    let blob: Vec<u8> = sqlx::query_scalar("select password_encrypted from relays where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        !blob.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
        "the column holds the password in the clear"
    );

    // `implicit_tls` goes in and comes out as the other variant: the one place the enum's
    // other spelling is exercised, on the wire and through sqlx.
    let mut tls = body("tls", None);
    tls["security"] = json!("implicit_tls");
    tls["port"] = json!(465);
    let r = t.admin.post(&relays_path(t.id)).json(&tls).await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(r.json::<Value>()["security"], json!("implicit_tls"));
    let relays = nm::relay::enabled_relays(&pool, t.id).await.unwrap();
    let securities: Vec<_> = relays
        .iter()
        .map(|r| (r.name.as_str(), r.security))
        .collect();
    assert!(securities.contains(&("tls", nm::relay::Security::ImplicitTls)));
    assert!(securities.contains(&("primary", nm::relay::Security::StartTls)));

    // Scoped by workspace, like every read here: another workspace's id finds nothing.
    let other = tenant(&pool, "globex").await;
    let err = nm::relay::password(&pool, &t.admin.state.secrets, other.id, id).await;
    assert!(matches!(err, Err(nm::error::ApiError::NotFound)));
}

#[sqlx::test]
async fn the_audit_row_for_a_relay_carries_no_password(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = add_relay(&t, "primary", Some(SECRET)).await;
    let changed = "an0ther-pw-5d2e81";
    t.admin
        .patch(&relay_path(t.id, id))
        .json(&json!({"password": changed, "rate_limit_per_minute": 90}))
        .await
        .assert_status_ok();
    t.admin
        .delete(&relay_path(t.id, id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let rows: Vec<(String, String)> = sqlx::query_as(
        "select action, metadata::text from audit_log where action like 'relay.%' order by id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // Three rows exist, so "none contains it" is not true of an empty set.
    let actions: Vec<_> = rows.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(actions, ["relay.created", "relay.updated", "relay.deleted"]);
    for (action, metadata) in &rows {
        for pw in [SECRET, changed] {
            assert!(!metadata.contains(pw), "{action} metadata holds it");
        }
        // It names the relay and the host, which is what an operator reads the log for.
        assert!(
            metadata.contains("primary") && metadata.contains("smtp.example.com"),
            "{action} metadata names neither relay nor host: {metadata}"
        );
    }
    // Not only `metadata`: no column of any audit row.
    let whole: Vec<String> = sqlx::query_scalar("select a::text from audit_log a")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(
        whole
            .iter()
            .all(|r| !r.contains(SECRET) && !r.contains(changed))
    );
}

#[sqlx::test]
async fn another_workspaces_relay_is_404(pool: PgPool) {
    let a = tenant(&pool, "acme").await;
    let b = tenant(&pool, "globex").await;
    let id = add_relay(&a, "primary-of-acme", Some(SECRET)).await;

    // B's admin names A's workspace, and then B's own workspace with A's relay id.
    for ws in [a.id, b.id] {
        let url = relay_path(ws, id);
        let responses = [
            b.admin.get(&url).await,
            b.admin.patch(&url).json(&json!({"enabled": false})).await,
            b.admin.delete(&url).await,
        ];
        for r in responses {
            r.assert_status(StatusCode::NOT_FOUND);
            let text = r.text();
            for named in [
                "primary-of-acme",
                "smtp.example.com",
                SECRET,
                &id.to_string(),
            ] {
                assert!(!text.contains(named), "a 404 body names {named}: {text}");
            }
        }
    }
    // Nor does it list, or create, across the boundary.
    b.admin
        .get(&relays_path(a.id))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    b.admin
        .post(&relays_path(a.id))
        .json(&body("sneak", None))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // And none of it did anything.
    let left: (i64, bool) = sqlx::query_as("select count(*), bool_and(enabled) from relays")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, (1, true));
}

#[sqlx::test]
async fn patch_without_a_password_key_keeps_the_stored_one(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = add_relay(&t, "primary", Some(SECRET)).await;
    let url = relay_path(t.id, id);

    let r = t.admin.patch(&url).json(&json!({"enabled": false})).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["has_password"], json!(true));
    assert_eq!(stored_password(&t, id).await.as_deref(), Some(SECRET));
    // The rest of the row stayed too.
    assert_eq!(r.json::<Value>()["rate_limit_per_minute"], json!(60));

    // A new value replaces it.
    t.admin
        .patch(&url)
        .json(&json!({"password": "replaced"}))
        .await
        .assert_status_ok();
    assert_eq!(stored_password(&t, id).await.as_deref(), Some("replaced"));

    // `null` clears it, and says so.
    let r = t.admin.patch(&url).json(&json!({"password": null})).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["has_password"], json!(false));
    assert_eq!(stored_password(&t, id).await, None);
    let blob: Option<Vec<u8>> =
        sqlx::query_scalar("select password_encrypted from relays where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        blob, None,
        "cleared means NULL, not an encrypted empty string"
    );
}

#[sqlx::test]
async fn enabled_relays_orders_by_failover_then_id(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let insert = |id: u128, name: &'static str, order: i32, enabled: bool| {
        sqlx::query(
            "insert into relays (id, workspace_id, name, host, port, security,
                                 rate_limit_per_minute, failover_order, enabled)
             values ($1, $2, $3, 'h', 25, 'starttls', 60, $4, $5)",
        )
        .bind(Uuid::from_u128(id))
        .bind(ws)
        .bind(name)
        .bind(order)
        .bind(enabled)
        .execute(&pool)
    };
    // Inserted so that neither insertion order nor name order is the answer: the two order-0 rows
    // go in highest id first, and the disabled one has the lowest order of all.
    insert(9, "late", 5, true).await.unwrap();
    insert(3, "tie-high", 0, true).await.unwrap();
    insert(2, "tie-low", 0, true).await.unwrap();
    insert(1, "off", -1, false).await.unwrap();

    // Another workspace's enabled relay must not appear.
    let other = workspace(&pool, "globex").await;
    sqlx::query(
        "insert into relays (id, workspace_id, name, host, port, security, rate_limit_per_minute)
         values ($1, $2, 'theirs', 'h', 25, 'starttls', 60)",
    )
    .bind(Uuid::from_u128(4))
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();

    // Index scans off, so the query cannot lean on `relays_failover` handing the rows back in
    // `id` order for free: with it in play, dropping `, id` from the `order by` changes nothing
    // here, and this would pass on a query that is not total.
    // Local to a transaction that is dropped (rolled back), so a later test on this pooled
    // connection cannot inherit the setting.
    let mut tx = pool.begin().await.unwrap();
    for setting in [
        "enable_indexscan",
        "enable_bitmapscan",
        "enable_indexonlyscan",
    ] {
        sqlx::query("select set_config($1, 'off', true)")
            .bind(setting)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let names: Vec<String> = nm::relay::enabled_relays(&mut *tx, ws)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["tie-low", "tie-high", "late"]);
}

#[sqlx::test]
async fn relay_writes_take_admin_and_bad_input_is_422(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    t.viewer
        .post(&relays_path(t.id))
        .json(&body("primary", None))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // The other two writes, against a relay that exists, and then proof they did nothing.
    let id = add_relay(&t, "kept", Some(SECRET)).await;
    t.viewer
        .patch(&relay_path(t.id, id))
        .json(&json!({"enabled": false}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    t.viewer
        .delete(&relay_path(t.id, id))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let kept: (i64, bool) =
        sqlx::query_as("select count(*), bool_and(enabled) from relays where name = 'kept'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept, (1, true));

    let (url, admin) = (relays_path(t.id), &t.admin);
    let post = |v: Value| {
        let url = url.clone();
        async move { admin.post(&url).json(&v).await }
    };
    let mut bad_port = body("a", None);
    bad_port["port"] = json!(0);
    post(bad_port)
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let mut unknown = body("b", None);
    unknown["pasword"] = json!("typo");
    post(unknown)
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    // Whitespace is not a name, a username or a password.
    for (key, blank) in [("name", "   "), ("username", "  "), ("password", " 	")] {
        let mut v = body("blank", None);
        v[key] = json!(blank);
        post(v)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        // PATCH validates too. Without this the rule is enforced on one verb of two, which is the
        // shape of the defect this test already had once for the role gate.
        let mut patched = json!({});
        patched[key] = json!(blank);
        t.admin
            .patch(&relay_path(t.id, id))
            .json(&patched)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // A credential is stored exactly as sent. Rejecting an all-whitespace password and trimming a
    // real one are one `.trim()` apart, and the second silently breaks every login against a relay
    // whose password genuinely has an edge space — a failure that looks like a wrong password.
    let padded = "  s3cret  ";
    let spaced = add_relay(&t, "padded", Some(padded)).await;
    assert_eq!(
        nm::relay::password(&t.admin.pool, &t.admin.state.secrets, t.id, spaced)
            .await
            .unwrap()
            .as_deref(),
        Some(padded),
        "the stored password was altered on the way in"
    );

    add_relay(&t, "primary", None).await;
    post(body("primary", None))
        .await
        .assert_status(StatusCode::CONFLICT);
}
